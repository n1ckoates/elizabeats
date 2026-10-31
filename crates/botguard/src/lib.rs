//! YouTube "proof of origin" (PO) tokens and audio stream URLs, both produced
//! inside the operating system's webview (a real browser engine; YouTube
//! rejects tokens minted anywhere else).
//!
//! - [`BotguardService::mint`] mints PO tokens with bgutils-js.
//! - [`BotguardService::resolve_audio_stream`] mints the token for a video ID
//!   and runs youtubei.js (YouTube Music client) in the same page to get a
//!   direct, deciphered AAC/MP4 audio URL.
//!
//! The webview runs in a helper process (this same executable), because a
//! webview needs the main thread and its own event loop:
//!
//! ```text
//!  app process                                     helper process (same exe)
//!  ───────────                                     ─────────────────────────
//!  BotguardService ── stdin: {"id":1,"resolve":"…"} ──▶ invisible window +
//!    (spawned lazily, killed  ◀── stdout: {"id":1,"stream":{…}} ── webview on
//!     on timeout, restarted)                           youtube.com/robots.txt
//!                                                      running bgutils-js and
//!                                                      youtubei.js
//! ```
//!
//! See `protocol.rs` for the messages. [`helper_main_if_requested`] must be
//! called first thing in `main`: when the app starts itself as the helper, it
//! runs the webview and returns the exit code; otherwise it returns `None`
//! straight away.

mod client;
mod helper;
mod protocol;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

pub use helper::{HELPER_FLAG, helper_main_if_requested};

/// How long one mint request may take, including starting the helper and
/// creating a fresh minter (normally 1-3 s cold, a few ms warm).
pub const MINT_TIMEOUT: Duration = Duration::from_secs(25);
/// How long resolving one audio stream may take, including starting the
/// helper and minting the token (normally about 1-2 s cold, well under 1 s
/// warm). The page gives up on its own a little earlier.
pub const RESOLVE_TIMEOUT: Duration = Duration::from_secs(60);

/// Subdirectory of the data directory the webview may use (WebView2 needs a
/// writable folder; WKWebView and WebKitGTK run without persistent storage).
const WEBVIEW_DIR_NAME: &str = "webview-minter";

#[derive(Debug, thiserror::Error)]
pub enum BotguardError {
    /// Kept for API compatibility; no longer returned.
    #[error("BotGuard is not implemented yet")]
    Unimplemented,
    #[error("{0}")]
    Other(String),
    #[error("PO token generation timed out")]
    Timeout,
    #[error("invalid PO token identifier: {0}")]
    InvalidIdentifier(String),
    #[error("YouTube request failed: {0}")]
    Network(String),
    /// YouTube answered, but not with what we need (e.g. no integrity token).
    #[error("YouTube rejected the BotGuard attestation: {0}")]
    Rejected(String),
    #[error("BotGuard JavaScript error: {0}")]
    Js(String),
    #[error("I/O error: {0}")]
    Io(String),
    #[error("the BotGuard service is not running")]
    ServiceStopped,
    /// A user-readable problem reported by YouTube, e.g. "This video is
    /// unavailable" or "YouTube returned an unsupported audio format.".
    #[error("{0}")]
    Youtube(String),
}

impl From<std::io::Error> for BotguardError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

/// PO tokens for the requested identifiers, in request order.
#[derive(Clone, Debug)]
pub struct MintedTokens {
    pub tokens: Vec<String>,
    pub valid_until: SystemTime,
}

/// A directly downloadable audio stream (always AAC in MP4).
#[derive(Clone, Debug)]
pub struct AudioStream {
    pub url: String,
    /// e.g. `audio/mp4; codecs="mp4a.40.2"`.
    pub mime_type: String,
    pub content_length: Option<u64>,
    /// Bits per second.
    pub bitrate: Option<u64>,
    pub itag: u32,
    /// The User-Agent the stream was requested with (also in `headers`).
    pub user_agent: Option<String>,
    /// Headers to send with the download GETs. Currently just the
    /// User-Agent (the live tests show even that is optional). Download in
    /// chunks of at most 10 MB using the `&range=start-end` URL parameter,
    /// like YouTube's own player.
    pub headers: Vec<(String, String)>,
}

/// How the webview helper process is started. [`BotguardService::start`]
/// uses this executable; tests point it at a test binary or a fake.
#[derive(Clone, Debug)]
pub struct HelperOptions {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Per-request limit for minting; an overdue helper is killed and
    /// restarted.
    pub mint_timeout: Duration,
    /// The same for resolving an audio stream.
    pub resolve_timeout: Duration,
}

impl HelperOptions {
    /// The real webview helper: `program` must call
    /// [`helper_main_if_requested`] first thing in its `main`.
    pub fn webview(program: impl Into<PathBuf>, data_dir: &Path) -> Self {
        Self {
            program: program.into(),
            args: vec![
                HELPER_FLAG.into(),
                data_dir.join(WEBVIEW_DIR_NAME).into_os_string(),
            ],
            mint_timeout: MINT_TIMEOUT,
            resolve_timeout: RESOLVE_TIMEOUT,
        }
    }
}

/// Handle to the webview helper. Cheap to clone; safe to share across
/// threads. The helper process stops when the last handle is dropped.
#[derive(Clone, Debug)]
pub struct BotguardService {
    helper: Arc<client::HelperClient>,
}

impl BotguardService {
    /// Starts the service; the helper is this executable.
    ///
    /// Nothing is started or fetched until the first request (or
    /// [`BotguardService::prewarm`]).
    pub fn start(data_dir: &Path) -> Result<Self, BotguardError> {
        let program = std::env::current_exe()?;
        Self::start_with_helper(HelperOptions::webview(program, data_dir))
    }

    /// Like [`BotguardService::start`] with a custom helper process.
    pub fn start_with_helper(helper: HelperOptions) -> Result<Self, BotguardError> {
        let client = client::HelperClient::start(client::Launch {
            program: helper.program,
            args: helper.args,
            mint_timeout: helper.mint_timeout,
            resolve_timeout: helper.resolve_timeout,
        })?;
        Ok(Self {
            helper: Arc::new(client),
        })
    }

    /// Mints one PO token per identifier (video IDs or visitor data; visitor
    /// data may be URL-encoded).
    pub async fn mint(&self, identifiers: &[String]) -> Result<MintedTokens, BotguardError> {
        protocol::validate_identifiers(identifiers)?;
        let identifiers = identifiers
            .iter()
            .map(|identifier| protocol::percent_decode(identifier))
            .collect();
        let (reply, response) = tokio::sync::oneshot::channel();
        self.helper.send(client::Job::Mint {
            identifiers,
            reply: Box::new(move |result| {
                // The caller may have stopped waiting; nothing to do then.
                let _ = reply.send(result);
            }),
        })?;
        // The client always answers by the request's deadline.
        response.await.map_err(|_| BotguardError::ServiceStopped)?
    }

    /// Resolves the best AAC/MP4 audio-only stream of a YouTube video. The
    /// helper mints the PO token for it itself.
    pub async fn resolve_audio_stream(&self, video_id: &str) -> Result<AudioStream, BotguardError> {
        if !protocol::is_video_id(video_id) {
            return Err(BotguardError::InvalidIdentifier(format!("{video_id:?}")));
        }
        let (reply, response) = tokio::sync::oneshot::channel();
        self.helper.send(client::Job::Resolve {
            video_id: video_id.to_owned(),
            reply: Box::new(move |result| {
                let _ = reply.send(result);
            }),
        })?;
        response.await.map_err(|_| BotguardError::ServiceStopped)?
    }

    /// Starts the helper, its minter and its youtubei.js session in the
    /// background so the first real request is fast. Failures are only
    /// logged; the next request retries.
    pub fn prewarm(&self) {
        let sent = self.helper.send(client::Job::Mint {
            identifiers: Vec::new(),
            reply: Box::new(|result| {
                if let Err(error) = result {
                    tracing::warn!("could not prewarm the webview helper: {error}");
                }
            }),
        });
        if let Err(error) = sent {
            tracing::warn!("could not prewarm BotGuard: {error}");
        }
    }
}

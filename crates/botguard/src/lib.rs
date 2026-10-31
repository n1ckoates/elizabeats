//! YouTube audio stream URLs, resolved inside the operating system's webview
//! (a real browser engine; YouTube rejects "proof of origin" (PO) tokens
//! minted anywhere else).
//!
//! [`BotguardService::resolve_audio_stream`] has the page mint a PO token for
//! a video ID with bgutils-js and run youtubei.js (YouTube Music client) to get
//! a direct, deciphered AAC/MP4 audio URL.
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
use std::time::Duration;

pub use helper::{HELPER_FLAG, helper_main_if_requested};
pub use protocol::AudioStream;

/// How long a prewarm may take: starting the helper and creating a fresh
/// minter (normally 1-3 s).
const MINT_TIMEOUT: Duration = Duration::from_secs(25);
/// How long resolving one audio stream may take, including starting the
/// helper and minting the token (normally about 1-2 s cold, well under 1 s
/// warm). The page gives up on its own a little earlier.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(60);

/// Subdirectory of the data directory the webview may use (WebView2 needs a
/// writable folder; WKWebView and WebKitGTK run without persistent storage).
const WEBVIEW_DIR_NAME: &str = "webview-minter";

#[derive(Debug, thiserror::Error)]
pub enum BotguardError {
    #[error("{0}")]
    Other(String),
    #[error("PO token generation timed out")]
    Timeout,
    #[error("invalid PO token identifier: {0}")]
    InvalidIdentifier(String),
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

/// How the webview helper process is started. [`BotguardService::start`]
/// uses this executable; tests point it at a test binary or a fake.
#[derive(Clone, Debug)]
pub struct HelperOptions {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Per-request limit for prewarming (a mint request); an overdue helper
    /// is killed and restarted.
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
    client: Arc<client::HelperClient>,
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
    pub fn start_with_helper(options: HelperOptions) -> Result<Self, BotguardError> {
        Ok(Self {
            client: Arc::new(client::HelperClient::start(options)?),
        })
    }

    /// Resolves the best AAC/MP4 audio-only stream of a YouTube video. The
    /// helper mints the PO token for it itself.
    pub async fn resolve_audio_stream(&self, video_id: &str) -> Result<AudioStream, BotguardError> {
        if !protocol::is_video_id(video_id) {
            return Err(BotguardError::InvalidIdentifier(format!("{video_id:?}")));
        }
        let (reply, response) = tokio::sync::oneshot::channel();
        self.client.send(client::Job::Resolve {
            video_id: video_id.to_owned(),
            reply,
        })?;
        // The client always answers by the request's deadline.
        response.await.map_err(|_| BotguardError::ServiceStopped)?
    }

    /// Starts the helper, its minter and its youtubei.js session in the
    /// background so the first real request is fast. Failures are only
    /// logged; the next request retries.
    pub fn prewarm(&self) {
        if let Err(error) = self.client.send(client::Job::Prewarm) {
            tracing::warn!("could not prewarm BotGuard: {error}");
        }
    }
}

/// Starts a named background thread.
fn spawn_thread(name: &str, run: impl FnOnce() + Send + 'static) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(run)
        .map(drop)
}

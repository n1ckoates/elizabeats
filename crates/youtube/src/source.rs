//! Everything that talks to YouTube: search and link lookup (InnerTube), and
//! fetching audio into the Download Cache (youtubei.js + BotGuard).

use elizabeats_botguard::{BotguardError, BotguardService};
use elizabeats_core::Track;

use crate::cache_files::CacheFiles;
use crate::error::{YoutubeError, redact};
use crate::http_download::{self, DownloadLimits, StreamRequest};
use crate::innertube::Innertube;
use crate::{AudioFetcher, BoxFuture, FetchedAudio};

/// Name recorded in diagnostics for search and lookup requests.
pub(crate) const CLIENT_NAME: &str = "innertube";

pub(crate) struct YoutubeSource {
    innertube: Innertube,
    /// Downloads need it; without it (e.g. in tests) they fail clearly.
    botguard: Option<BotguardService>,
    http: reqwest::Client,
    limits: DownloadLimits,
}

impl YoutubeSource {
    pub(crate) fn new(botguard: Option<BotguardService>) -> Result<Self, YoutubeError> {
        Ok(Self {
            innertube: Innertube::new()?,
            botguard,
            http: http_download::build_http_client()?,
            limits: DownloadLimits::default(),
        })
    }

    /// Prepares search so the first query is as fast as later ones.
    pub(crate) async fn warm_up(&self) {
        self.innertube.warm_up().await;
    }

    /// YouTube Music song search: at most 20 Tracks.
    pub(crate) async fn search_tracks(&self, query: &str) -> Result<Vec<Track>, YoutubeError> {
        self.innertube.search_tracks(query).await
    }

    /// Metadata for one video (a pasted YouTube or YouTube Music link).
    pub(crate) async fn track_for_video(&self, video_id: &str) -> Result<Track, YoutubeError> {
        self.innertube.track_for_video(video_id).await
    }

    /// Downloads the audio. YouTube Music songs only download in full through
    /// the YouTube Music client with a PO token minted by a real browser
    /// engine, which is what youtubei.js + BotGuard (system webview) provide.
    async fn fetch_audio(
        &self,
        video_id: &str,
        files: &CacheFiles,
    ) -> Result<FetchedAudio, YoutubeError> {
        let botguard = self.botguard.as_ref().ok_or_else(|| YoutubeError::Client {
            detail: "BotGuard service is not running".to_owned(),
        })?;
        let stream =
            botguard
                .resolve_audio_stream(video_id)
                .await
                .map_err(|error| match error {
                    BotguardError::Youtube(message)
                        if message == YoutubeError::UnsupportedFormat.to_string() =>
                    {
                        YoutubeError::UnsupportedFormat
                    }
                    BotguardError::Youtube(message) => YoutubeError::Unavailable {
                        message,
                        detail: "youtubei.js playability".to_owned(),
                    },
                    other => YoutubeError::Player {
                        detail: redact(&other.to_string()),
                    },
                })?;
        if !stream
            .mime_type
            .to_ascii_lowercase()
            .starts_with("audio/mp4")
        {
            return Err(YoutubeError::UnsupportedFormat);
        }
        tracing::info!(
            "downloading {video_id}: itag {} ({:?} bytes)",
            stream.itag,
            stream.content_length
        );
        let file_name = format!("{video_id}.m4a");
        let mut part = files
            .begin(&file_name)
            .await
            .map_err(YoutubeError::cache_write)?;
        let user_agent = stream.user_agent.clone().unwrap_or_default();
        let request = StreamRequest {
            url: &stream.url,
            user_agent: &user_agent,
            content_length: stream.content_length,
        };
        http_download::download_stream(&self.http, &request, &mut part, &self.limits).await?;
        let byte_size = part.finish().await.map_err(|error| {
            if error.kind() == std::io::ErrorKind::InvalidData {
                YoutubeError::EmptyDownload
            } else {
                YoutubeError::cache_write(error)
            }
        })?;
        Ok(FetchedAudio {
            file_name,
            byte_size,
            client_type: "youtubei.js YTMUSIC".to_owned(),
        })
    }
}

impl AudioFetcher for YoutubeSource {
    fn fetch<'a>(
        &'a self,
        video_id: &'a str,
        files: &'a CacheFiles,
    ) -> BoxFuture<'a, Result<FetchedAudio, YoutubeError>> {
        Box::pin(self.fetch_audio(video_id, files))
    }
}

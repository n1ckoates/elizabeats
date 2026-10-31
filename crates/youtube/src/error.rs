//! User-facing errors.
//!
//! `Display` is short and human because it is shown in the UI. Technical
//! detail (never stream URLs or tokens) goes into [`YoutubeError::detail`],
//! which is only written to diagnostics.

/// Errors returned by [`crate::YoutubeMusic`]. Cheap to clone so one download
/// result can be shared by every caller waiting on it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum YoutubeError {
    #[error("This isn't a valid YouTube video ID.")]
    InvalidVideoId,
    #[error("YouTube Music search timed out.")]
    SearchTimedOut,
    #[error("YouTube Music search failed.")]
    SearchFailed { detail: String },
    #[error("YouTube returned incomplete track metadata.")]
    IncompleteMetadata,
    #[error("{message}")]
    Unavailable { message: String, detail: String },
    #[error("Couldn't start the YouTube client.")]
    Client { detail: String },
    #[error("YouTube couldn't provide this track.")]
    Player { detail: String },
    #[error("YouTube returned an unsupported audio format.")]
    UnsupportedFormat,
    #[error("YouTube download timed out.")]
    DownloadTimedOut,
    #[error("YouTube refused the download (HTTP 403).")]
    DownloadForbidden,
    #[error("YouTube download failed (HTTP {status}).")]
    DownloadHttp { status: u16 },
    #[error("Couldn't reach YouTube. Check the internet connection.")]
    DownloadNetwork { detail: String },
    #[error("YouTube download was incomplete.")]
    DownloadIncomplete { detail: String },
    #[error("YouTube downloaded empty audio.")]
    EmptyDownload,
    #[error("YouTube download stopped unexpectedly.")]
    DownloadInterrupted,
    #[error("Couldn't update the download cache.")]
    Cache { detail: String },
    #[error("Not enough disk space to download this track.")]
    DiskFull,
}

impl YoutubeError {
    /// Extra technical context for diagnostics (already redacted).
    fn detail(&self) -> Option<&str> {
        match self {
            Self::SearchFailed { detail }
            | Self::Unavailable { detail, .. }
            | Self::Client { detail }
            | Self::Player { detail }
            | Self::DownloadNetwork { detail }
            | Self::DownloadIncomplete { detail }
            | Self::Cache { detail } => Some(detail),
            Self::DownloadForbidden => Some(
                "HTTP 403 from the stream server; likely a missing or rejected PO token (BotGuard)",
            ),
            _ => None,
        }
    }

    /// HTTP status code, when the error came from the stream server.
    pub(crate) fn status_code(&self) -> Option<u16> {
        match self {
            Self::DownloadForbidden => Some(403),
            Self::DownloadHttp { status } => Some(*status),
            _ => None,
        }
    }

    /// The message written to diagnostics: the user message plus detail.
    pub(crate) fn diagnostic_message(&self) -> String {
        match self.detail() {
            Some(detail) if !detail.is_empty() => format!("{self} ({detail})"),
            _ => self.to_string(),
        }
    }

    /// A failed write to the Download Cache; a full disk gets its own error
    /// so the download can make room and try again.
    pub(crate) fn cache_write(error: std::io::Error) -> Self {
        match error.kind() {
            std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded => Self::DiskFull,
            _ => Self::cache(error),
        }
    }

    pub(crate) fn cache(error: impl std::fmt::Display) -> Self {
        Self::Cache {
            detail: redact(&error.to_string()),
        }
    }
}

/// A request failure's description, without its URL.
pub(crate) fn http_detail(error: reqwest::Error) -> String {
    redact(&error.without_url().to_string())
}

/// Removes anything that looks like a URL, so stream URLs (which carry
/// signatures and PO tokens) never reach diagnostics or logs.
pub(crate) fn redact(text: &str) -> String {
    text.split(' ')
        .map(|word| {
            if word.contains("://") || word.contains("googlevideo.com") {
                "<url>"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_urls() {
        assert_eq!(
            redact(
                "error sending request for url (https://rr1.googlevideo.com/videoplayback?pot=SECRET)"
            ),
            "error sending request for url <url>"
        );
    }

    #[test]
    fn diagnostic_message_includes_detail() {
        let error = YoutubeError::Cache {
            detail: "disk full".into(),
        };
        assert_eq!(
            error.diagnostic_message(),
            "Couldn't update the download cache. (disk full)"
        );
        assert_eq!(
            YoutubeError::DownloadTimedOut.diagnostic_message(),
            "YouTube download timed out."
        );
    }
}

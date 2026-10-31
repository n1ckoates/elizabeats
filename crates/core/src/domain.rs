//! Domain vocabulary shared by every crate. See `CONTEXT.md` at the repository root.
//!
//! Serialized field names match the original Electron app (camelCase JSON) so
//! existing `settings.json` files and download-cache manifests stay readable.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The origin of a Track. Each Source assigns its own Track identities.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SourceId {
    #[serde(rename = "youtube-music")]
    YoutubeMusic,
    #[serde(rename = "local")]
    Local,
}

impl SourceId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::YoutubeMusic => "youtube-music",
            Self::Local => "local",
        }
    }
}

/// A playable audio item from the local Library or YouTube Music.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Track {
    /// Source-specific identity: a YouTube video ID, or a `/`-separated path
    /// relative to the Library folder for local Tracks.
    pub id: String,
    pub source: SourceId,
    pub title: String,
    pub artist: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
    /// Remote artwork (YouTube thumbnails). Local artwork is embedded in the
    /// audio file and looked up through the Library instead of being copied here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artwork_url: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub explicit: bool,
}

impl Track {
    pub fn key(&self) -> TrackKey {
        TrackKey {
            source: self.source,
            id: self.id.clone(),
        }
    }

    /// The public YouTube Music link for YouTube Tracks.
    pub fn youtube_music_url(&self) -> Option<String> {
        (self.source == SourceId::YoutubeMusic)
            .then(|| format!("https://music.youtube.com/watch?v={}", self.id))
    }
}

/// Identifies a Track across Sources (`source:id`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TrackKey {
    pub source: SourceId,
    pub id: String,
}

impl fmt::Display for TrackKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.source.as_str(), self.id)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ItemKind {
    Track(Track),
    Break,
}

/// A Queue item: a Track or a Break, with an id unique within the Queue.
/// Serialized as `{"type":"track","id":…,"track":{…}}` / `{"type":"break","id":…}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(into = "StoredItem", from = "StoredItem")]
pub struct QueueItem {
    pub id: String,
    pub kind: ItemKind,
}

impl QueueItem {
    pub fn track(id: impl Into<String>, track: Track) -> Self {
        Self {
            id: id.into(),
            kind: ItemKind::Track(track),
        }
    }

    pub fn new_break(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind: ItemKind::Break,
        }
    }

    pub fn as_track(&self) -> Option<&Track> {
        match &self.kind {
            ItemKind::Track(track) => Some(track),
            ItemKind::Break => None,
        }
    }

    /// The Track's title, or "Break".
    pub fn title(&self) -> &str {
        self.as_track().map_or("Break", |track| &track.title)
    }
}

/// The on-disk shape of a [`QueueItem`].
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum StoredItem {
    Track { id: String, track: Track },
    Break { id: String },
}

impl From<QueueItem> for StoredItem {
    fn from(item: QueueItem) -> Self {
        match item.kind {
            ItemKind::Track(track) => Self::Track { id: item.id, track },
            ItemKind::Break => Self::Break { id: item.id },
        }
    }
}

impl From<StoredItem> for QueueItem {
    fn from(item: StoredItem) -> Self {
        match item {
            StoredItem::Track { id, track } => Self::track(id, track),
            StoredItem::Break { id } => Self::new_break(id),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackProgress {
    pub current_time: f64,
    pub duration: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlayLogReason {
    Completed,
    Skipped,
    Error,
}

/// A Track or Break whose Playing state actually began.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayLogEntry {
    pub id: String,
    pub item: QueueItem,
    /// RFC 3339 timestamp in UTC.
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<PlayLogReason>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecoveryStatus {
    Interrupted,
    Clean,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoverySnapshot {
    pub version: u32,
    pub status: RecoveryStatus,
    pub queue: Vec<QueueItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_started_at: Option<String>,
    pub updated_at: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    pub library_folder: Option<String>,
    pub theme: Theme,
    pub crossfade_duration_seconds: u32,
    #[serde(rename = "downloadCacheSizeGiB")]
    pub download_cache_size_gib: u32,
}

impl AppSettings {
    pub const CROSSFADE_OPTIONS: [u32; 8] = [0, 1, 2, 3, 5, 8, 10, 15];
    pub const MAX_CROSSFADE_SECONDS: u32 = 15;
    pub const MIN_CACHE_GIB: u32 = 1;
    pub const MAX_CACHE_GIB: u32 = 25;

    pub fn cache_bytes(&self) -> u64 {
        u64::from(self.download_cache_size_gib) * 1024 * 1024 * 1024
    }
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            library_folder: None,
            theme: Theme::System,
            crossfade_duration_seconds: 3,
            download_cache_size_gib: 5,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DownloadCacheStatus {
    pub used_bytes: u64,
    pub max_bytes: u64,
    pub pending_clear_count: usize,
}

/// A local diagnostic record. Values are redacted before they are written.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticEvent {
    pub operation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl DiagnosticEvent {
    pub fn new(operation: impl Into<String>) -> Self {
        Self {
            operation: operation.into(),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn source(mut self, source: SourceId) -> Self {
        self.source = Some(source);
        self
    }

    #[must_use]
    pub fn video_id(mut self, video_id: impl Into<String>) -> Self {
        self.video_id = Some(video_id.into());
        self
    }

    #[must_use]
    pub fn client_name(mut self, client_name: impl Into<String>) -> Self {
        self.client_name = Some(client_name.into());
        self
    }

    #[must_use]
    pub fn error(mut self, error: impl Into<String>) -> Self {
        self.error = Some(error.into());
        self
    }
}

/// Receives diagnostic events. Implementations must never block or panic.
pub type DiagnosticSink = std::sync::Arc<dyn Fn(DiagnosticEvent) + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_matches_electron_format() {
        let json = r#"{"crossfadeDurationSeconds":3,"downloadCacheSizeGiB":5,"libraryFolder":null,"theme":"system"}"#;
        let parsed: AppSettings = serde_json::from_str(json).unwrap_or_default();
        assert_eq!(parsed, AppSettings::default());
        let parsed: AppSettings = serde_json::from_str(
            r#"{"crossfadeDurationSeconds":8,"downloadCacheSizeGiB":12,"libraryFolder":"/m","theme":"dark"}"#,
        )
        .unwrap_or_default();
        assert_eq!(parsed.download_cache_size_gib, 12);
        assert_eq!(parsed.theme, Theme::Dark);
        assert_eq!(parsed.library_folder.as_deref(), Some("/m"));
    }

    #[test]
    fn stored_queue_item_uses_type_tag() {
        let item = QueueItem::new_break("b1");
        let json = serde_json::to_string(&item).unwrap_or_default();
        assert_eq!(json, r#"{"type":"break","id":"b1"}"#);
        let track = QueueItem::track(
            "t1",
            Track {
                id: "abc".into(),
                source: SourceId::Local,
                title: "t".into(),
                artist: "a".into(),
                album: None,
                duration_seconds: None,
                artwork_url: None,
                explicit: false,
            },
        );
        let json = serde_json::to_string(&track).unwrap_or_default();
        assert_eq!(
            json,
            r#"{"type":"track","id":"t1","track":{"id":"abc","source":"local","title":"t","artist":"a"}}"#
        );
        assert_eq!(serde_json::from_str::<QueueItem>(&json).ok(), Some(track));
    }

    #[test]
    fn track_key_display() {
        let track = Track {
            id: "abc".into(),
            source: SourceId::YoutubeMusic,
            title: "t".into(),
            artist: "a".into(),
            album: None,
            duration_seconds: None,
            artwork_url: None,
            explicit: false,
        };
        assert_eq!(track.key().to_string(), "youtube-music:abc");
        assert_eq!(
            track.youtube_music_url().as_deref(),
            Some("https://music.youtube.com/watch?v=abc")
        );
    }
}

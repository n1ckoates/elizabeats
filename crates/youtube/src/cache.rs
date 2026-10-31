//! Download Cache bookkeeping: the manifest format and eviction order.
//!
//! The manifest (`download-cache/manifest.json`) keeps the original app's
//! format so existing caches carry over:
//!
//! ```json
//! {"version":1,"entries":[{"videoId":"…","fileName":"….m4a","byteSize":123,
//!   "downloadedAt":"2025-01-01T00:00:00.000Z","playCount":2,"clearRequested":false}]}
//! ```
//!
//! Entries without a `fileName` only remember play counts.

use std::collections::{BTreeMap, HashSet};

use elizabeats_core::DownloadCacheStatus;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cache_files::is_valid_audio_file_name;
use crate::video_id::is_id_byte;

const MANIFEST_VERSION: u64 = 1;
/// Largest integer JavaScript can represent exactly (`Number.MAX_SAFE_INTEGER`).
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CacheEntry {
    pub video_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub byte_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downloaded_at: Option<String>,
    pub play_count: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_requested: bool,
}

impl CacheEntry {
    fn new(video_id: &str) -> Self {
        Self {
            video_id: video_id.to_owned(),
            file_name: None,
            byte_size: None,
            downloaded_at: None,
            play_count: 0,
            clear_requested: false,
        }
    }

    /// Forgets the audio file but keeps the play count.
    fn remove_audio(&mut self) {
        self.file_name = None;
        self.byte_size = None;
        self.downloaded_at = None;
        self.clear_requested = false;
    }

    /// Size counted against the limit (entries with a file and a known size).
    fn counted_bytes(&self) -> Option<u64> {
        self.file_name.as_ref().and(self.byte_size)
    }

    /// Whether a parsed manifest entry has sane values: a safe video ID, a
    /// file named after its video, a positive size and an RFC 3339 date.
    fn is_valid(&self) -> bool {
        let id = &self.video_id;
        let id_ok = !id.is_empty() && id.len() <= 64 && id.bytes().all(is_id_byte);
        let file_ok = self.file_name.as_deref().is_none_or(|name| {
            is_valid_audio_file_name(name)
                && name.rsplit_once('.').map(|(stem, _)| stem) == Some(id)
        });
        let size_ok = self
            .byte_size
            .is_none_or(|size| (1..=MAX_SAFE_INTEGER).contains(&size));
        let date_ok = self
            .downloaded_at
            .as_deref()
            .is_none_or(|date| chrono::DateTime::parse_from_rfc3339(date).is_ok());
        id_ok && file_ok && size_ok && date_ok && self.play_count <= MAX_SAFE_INTEGER
    }
}

/// In-memory Download Cache state. Entries are keyed (and written) in videoId order.
#[derive(Debug)]
pub(crate) struct CacheState {
    pub entries: BTreeMap<String, CacheEntry>,
    pub max_bytes: u64,
    pub protected: HashSet<String>,
}

impl CacheState {
    pub fn new(max_bytes: u64) -> Self {
        Self {
            entries: BTreeMap::new(),
            max_bytes,
            protected: HashSet::new(),
        }
    }

    /// The entry for `video_id`, created if missing.
    pub fn entry(&mut self, video_id: &str) -> &mut CacheEntry {
        self.entries
            .entry(video_id.to_owned())
            .or_insert_with(|| CacheEntry::new(video_id))
    }

    /// Records a complete audio file for `video_id`, downloaded just now.
    pub fn set_audio(&mut self, video_id: &str, file_name: String, byte_size: u64) {
        let entry = self.entry(video_id);
        entry.file_name = Some(file_name);
        entry.byte_size = Some(byte_size);
        entry.downloaded_at = Some(now_timestamp());
        entry.clear_requested = false;
    }

    /// Records the size found on disk, or forgets the audio when there is none.
    pub fn set_size(&mut self, video_id: &str, size: Option<u64>) {
        if let Some(entry) = self.entries.get_mut(video_id) {
            match size {
                Some(size) => entry.byte_size = Some(size),
                None => entry.remove_audio(),
            }
        }
    }

    pub fn used_bytes(&self) -> u64 {
        self.entries
            .values()
            .filter_map(CacheEntry::counted_bytes)
            .sum()
    }

    pub fn status(&self) -> DownloadCacheStatus {
        DownloadCacheStatus {
            used_bytes: self.used_bytes(),
            max_bytes: self.max_bytes,
            pending_clear_count: self
                .entries
                .values()
                .filter(|entry| entry.counted_bytes().is_some() && entry.clear_requested)
                .count(),
        }
    }

    pub fn cached_video_ids(&self) -> Vec<String> {
        self.entries
            .values()
            .filter(|entry| entry.file_name.is_some())
            .map(|entry| entry.video_id.clone())
            .collect()
    }

    /// The manifest JSON, newline-terminated like the original app's.
    pub fn manifest_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        #[derive(Serialize)]
        struct Manifest<'a> {
            version: u64,
            entries: Vec<&'a CacheEntry>,
        }
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            entries: self.entries.values().collect(),
        };
        let mut json = serde_json::to_vec(&manifest)?;
        json.push(b'\n');
        Ok(json)
    }

    /// Entries that may be evicted, in eviction order: clear-requested first,
    /// then lowest play count, oldest download, and videoId. Protected and
    /// in-flight videos are never candidates.
    pub fn eviction_candidates(&self, in_flight: &HashSet<String>) -> Vec<CacheEntry> {
        let mut candidates: Vec<CacheEntry> = self
            .entries
            .values()
            .filter(|entry| {
                entry.counted_bytes().is_some()
                    && !self.protected.contains(&entry.video_id)
                    && !in_flight.contains(&entry.video_id)
            })
            .cloned()
            .collect();
        candidates.sort_by(|left, right| {
            right
                .clear_requested
                .cmp(&left.clear_requested)
                .then(left.play_count.cmp(&right.play_count))
                .then_with(|| left.downloaded_at.cmp(&right.downloaded_at))
                .then_with(|| left.video_id.cmp(&right.video_id))
        });
        candidates
    }
}

/// Parses a manifest value. Anything other than a version-1 manifest yields
/// no entries; individual invalid entries are skipped.
pub(crate) fn parse_manifest(value: &Value) -> BTreeMap<String, CacheEntry> {
    #[derive(Deserialize)]
    struct Manifest {
        version: u64,
        entries: Vec<Value>,
    }
    let entries = match Manifest::deserialize(value) {
        Ok(manifest) if manifest.version == MANIFEST_VERSION => manifest.entries,
        _ => return BTreeMap::new(),
    };
    entries
        .iter()
        .filter_map(|item| {
            let entry = CacheEntry::deserialize(item)
                .ok()
                .filter(CacheEntry::is_valid);
            if entry.is_none() {
                tracing::warn!("skipping invalid download cache manifest entry");
            }
            entry
        })
        .map(|entry| (entry.video_id.clone(), entry))
        .collect()
}

/// Timestamp in the same format as JavaScript's `Date.toISOString()`.
fn now_timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn entry(id: &str, play_count: u64, downloaded_at: &str, clear: bool) -> CacheEntry {
        CacheEntry {
            video_id: id.to_owned(),
            file_name: Some(format!("{id}.m4a")),
            byte_size: Some(100),
            downloaded_at: Some(downloaded_at.to_owned()),
            play_count,
            clear_requested: clear,
        }
    }

    #[test]
    fn parses_valid_manifest_from_electron() {
        let value = json!({
            "entries": [
                {"videoId": "aaaaaaaaaaa", "fileName": "aaaaaaaaaaa.webm", "byteSize": 10,
                 "downloadedAt": "2025-01-02T03:04:05.678Z", "playCount": 3, "clearRequested": false},
                {"videoId": "bbbbbbbbbbb", "playCount": 1},
            ],
            "version": 1
        });
        let entries = parse_manifest(&value);
        assert_eq!(entries.len(), 2);
        let a = &entries["aaaaaaaaaaa"];
        assert_eq!(a.file_name.as_deref(), Some("aaaaaaaaaaa.webm"));
        assert_eq!(a.byte_size, Some(10));
        assert_eq!(a.play_count, 3);
        assert_eq!(entries["bbbbbbbbbbb"].file_name, None);
    }

    #[test]
    fn rejects_malformed_manifests_and_entries() {
        assert!(parse_manifest(&json!(null)).is_empty());
        assert!(parse_manifest(&json!([])).is_empty());
        assert!(
            parse_manifest(&json!({"version": 2, "entries": [{"videoId": "a", "playCount": 0}]}))
                .is_empty()
        );
        assert!(parse_manifest(&json!({"version": "1", "entries": []})).is_empty());
        assert!(parse_manifest(&json!({"version": 1, "entries": {}})).is_empty());

        let bad_entries = json!({"version": 1, "entries": [
            "nope",
            {"playCount": 0},
            {"videoId": "../etc", "playCount": 0},
            {"videoId": "aaaaaaaaaaa"},
            {"videoId": "aaaaaaaaaaa", "playCount": -1},
            {"videoId": "aaaaaaaaaaa", "playCount": 1.5},
            {"videoId": "aaaaaaaaaaa", "playCount": 9_007_199_254_740_992_u64},
            {"videoId": "aaaaaaaaaaa", "playCount": 0, "fileName": "../x.m4a"},
            {"videoId": "aaaaaaaaaaa", "playCount": 0, "fileName": "other.m4a"},
            {"videoId": "aaaaaaaaaaa", "playCount": 0, "fileName": "aaaaaaaaaaa.mp3"},
            {"videoId": "aaaaaaaaaaa", "playCount": 0, "byteSize": 0},
            {"videoId": "aaaaaaaaaaa", "playCount": 0, "byteSize": "10"},
            {"videoId": "aaaaaaaaaaa", "playCount": 0, "downloadedAt": "yesterday"},
            {"videoId": "aaaaaaaaaaa", "playCount": 0, "clearRequested": "yes"},
            {"videoId": "ccccccccccc", "playCount": 0},
        ]});
        let entries = parse_manifest(&bad_entries);
        assert_eq!(entries.keys().collect::<Vec<_>>(), vec!["ccccccccccc"]);
    }

    #[test]
    fn manifest_round_trips_in_original_format() {
        let mut state = CacheState::new(1000);
        state.entry("bbbbbbbbbbb");
        state.entries.insert(
            "aaaaaaaaaaa".into(),
            entry("aaaaaaaaaaa", 2, "2025-01-01T00:00:00.000Z", true),
        );
        let json = state.manifest_json().unwrap();
        assert_eq!(
            String::from_utf8(json.clone()).unwrap(),
            "{\"version\":1,\"entries\":[{\"videoId\":\"aaaaaaaaaaa\",\"fileName\":\"aaaaaaaaaaa.m4a\",\"byteSize\":100,\"downloadedAt\":\"2025-01-01T00:00:00.000Z\",\"playCount\":2,\"clearRequested\":true},{\"videoId\":\"bbbbbbbbbbb\",\"playCount\":0}]}\n"
        );
        let value: Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(parse_manifest(&value), state.entries);
    }

    #[test]
    fn eviction_order_and_protection() {
        let mut state = CacheState::new(0);
        for e in [
            entry("played_often", 5, "2025-01-01T00:00:00.000Z", false),
            entry("new_unplayed", 0, "2025-03-01T00:00:00.000Z", false),
            entry("old_unplayed", 0, "2025-01-01T00:00:00.000Z", false),
            entry("old_unplayeA", 0, "2025-01-01T00:00:00.000Z", false),
            entry("cleared_fave", 9, "2025-05-01T00:00:00.000Z", true),
            entry("protected_id", 0, "2024-01-01T00:00:00.000Z", false),
            entry("downloading_", 0, "2024-01-01T00:00:00.000Z", false),
        ] {
            state.entries.insert(e.video_id.clone(), e);
        }
        state.entry("no_file_here");
        state.protected.insert("protected_id".into());
        let in_flight = HashSet::from(["downloading_".to_owned()]);

        let order: Vec<_> = state
            .eviction_candidates(&in_flight)
            .into_iter()
            .map(|e| e.video_id)
            .collect();
        assert_eq!(
            order,
            vec![
                "cleared_fave",
                "old_unplayeA",
                "old_unplayed",
                "new_unplayed",
                "played_often"
            ]
        );
        let status = state.status();
        assert_eq!(status.used_bytes, 700);
        assert_eq!(status.pending_clear_count, 1);
        assert_eq!(state.cached_video_ids().len(), 7);
    }
}

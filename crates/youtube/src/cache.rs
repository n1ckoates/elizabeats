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

use serde::Serialize;
use serde_json::Value;

use crate::cache_files::is_valid_audio_file_name;

pub(crate) const MANIFEST_VERSION: u64 = 1;
/// Largest integer JavaScript can represent exactly (`Number.MAX_SAFE_INTEGER`).
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

#[derive(Clone, Debug, PartialEq, Serialize)]
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
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub clear_requested: bool,
}

impl CacheEntry {
    pub fn new(video_id: &str) -> Self {
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
    pub fn remove_audio(&mut self) {
        self.file_name = None;
        self.byte_size = None;
        self.downloaded_at = None;
        self.clear_requested = false;
    }

    /// Size counted against the limit (entries with a file and a known size).
    fn counted_bytes(&self) -> Option<u64> {
        self.file_name.as_ref().and(self.byte_size)
    }
}

#[derive(Serialize)]
struct Manifest<'a> {
    version: u64,
    entries: Vec<&'a CacheEntry>,
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

    pub fn used_bytes(&self) -> u64 {
        self.entries
            .values()
            .filter_map(CacheEntry::counted_bytes)
            .sum()
    }

    pub fn pending_clear_count(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| entry.counted_bytes().is_some() && entry.clear_requested)
            .count()
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
                .then_with(|| {
                    let l = left.downloaded_at.as_deref().unwrap_or_default();
                    let r = right.downloaded_at.as_deref().unwrap_or_default();
                    l.cmp(r)
                })
                .then_with(|| left.video_id.cmp(&right.video_id))
        });
        candidates
    }
}

/// Parses a manifest value. Anything other than a version-1 manifest yields
/// no entries; individual invalid entries are skipped.
pub(crate) fn parse_manifest(value: &Value) -> BTreeMap<String, CacheEntry> {
    let mut entries = BTreeMap::new();
    let Some(object) = value.as_object() else {
        return entries;
    };
    if object.get("version").and_then(Value::as_u64) != Some(MANIFEST_VERSION) {
        return entries;
    }
    let Some(items) = object.get("entries").and_then(Value::as_array) else {
        return entries;
    };
    for item in items {
        match parse_entry(item) {
            Some(entry) => {
                entries.insert(entry.video_id.clone(), entry);
            }
            None => tracing::warn!("skipping invalid download cache manifest entry"),
        }
    }
    entries
}

/// Validates one manifest entry strictly. Missing optional fields are fine;
/// present fields must have the right type and a sane value.
fn parse_entry(value: &Value) -> Option<CacheEntry> {
    let object = value.as_object()?;
    let video_id = object.get("videoId")?.as_str()?;
    let id_is_valid = !video_id.is_empty()
        && video_id.len() <= 64
        && video_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if !id_is_valid {
        return None;
    }
    let play_count = object.get("playCount")?.as_u64()?;
    if play_count > MAX_SAFE_INTEGER {
        return None;
    }

    let file_name = match object.get("fileName") {
        None => None,
        Some(value) => {
            let name = value.as_str()?;
            // Files are always named after their video, so anything else is suspect.
            let stem = name.rsplit_once('.').map(|(stem, _)| stem);
            if !is_valid_audio_file_name(name) || stem != Some(video_id) {
                return None;
            }
            Some(name.to_owned())
        }
    };
    let byte_size = match object.get("byteSize") {
        None => None,
        Some(value) => {
            let size = value.as_u64()?;
            if size == 0 || size > MAX_SAFE_INTEGER {
                return None;
            }
            Some(size)
        }
    };
    let downloaded_at = match object.get("downloadedAt") {
        None => None,
        Some(value) => {
            let text = value.as_str()?;
            chrono::DateTime::parse_from_rfc3339(text).ok()?;
            Some(text.to_owned())
        }
    };
    let clear_requested = match object.get("clearRequested") {
        None => false,
        Some(value) => value.as_bool()?,
    };

    Some(CacheEntry {
        video_id: video_id.to_owned(),
        file_name,
        byte_size,
        downloaded_at,
        play_count,
        clear_requested,
    })
}

/// Timestamp in the same format as JavaScript's `Date.toISOString()`.
pub(crate) fn now_timestamp() -> String {
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
            {"videoId": "aaaaaaaaaaa", "playCount": 0, "fileName": null},
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
        state
            .entries
            .insert("bbbbbbbbbbb".into(), CacheEntry::new("bbbbbbbbbbb"));
        state.entries.insert(
            "aaaaaaaaaaa".into(),
            entry("aaaaaaaaaaa", 2, "2025-01-01T00:00:00.000Z", true),
        );
        let json = state.manifest_json().unwrap_or_default();
        let text = String::from_utf8(json.clone()).unwrap_or_default();
        assert_eq!(
            text,
            "{\"version\":1,\"entries\":[{\"videoId\":\"aaaaaaaaaaa\",\"fileName\":\"aaaaaaaaaaa.m4a\",\"byteSize\":100,\"downloadedAt\":\"2025-01-01T00:00:00.000Z\",\"playCount\":2,\"clearRequested\":true},{\"videoId\":\"bbbbbbbbbbb\",\"playCount\":0}]}\n"
        );
        let value: Value = serde_json::from_slice(&json).unwrap_or_default();
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
        state
            .entries
            .insert("no_file_here".into(), CacheEntry::new("no_file_here"));
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
        assert_eq!(state.used_bytes(), 700);
        assert_eq!(state.pending_clear_count(), 1);
        assert_eq!(state.cached_video_ids().len(), 7);
    }
}

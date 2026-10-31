//! `recovery.json`: the Queue snapshot restored after an unclean shutdown.

use std::io;
use std::path::Path;

use elizabeats_core::RecoverySnapshot;

use crate::atomic::remove_file_durably;
use crate::{StoreError, write_atomic};

pub const RECOVERY_FILE: &str = "recovery.json";
pub const RECOVERY_VERSION: u32 = 1;

const WHAT: &str = "Queue recovery";

/// `Ok(None)` when there is nothing to recover. An unreadable or invalid
/// snapshot is an error so the UI can report it rather than silently losing
/// the Queue.
pub fn read_recovery(dir: &Path) -> Result<Option<RecoverySnapshot>, StoreError> {
    let path = dir.join(RECOVERY_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(StoreError::io("read", &path, source)),
    };
    let snapshot: RecoverySnapshot =
        serde_json::from_slice(&bytes).map_err(|error| StoreError::Invalid {
            what: WHAT,
            detail: error.to_string(),
        })?;
    if snapshot.version != RECOVERY_VERSION {
        return Err(StoreError::Invalid {
            what: WHAT,
            detail: format!("unsupported version {}", snapshot.version),
        });
    }
    Ok(Some(snapshot))
}

pub fn save_recovery(dir: &Path, snapshot: &RecoverySnapshot) -> Result<(), StoreError> {
    let json =
        serde_json::to_vec(snapshot).map_err(|source| StoreError::Encode { what: WHAT, source })?;
    write_atomic(&dir.join(RECOVERY_FILE), &json)
}

pub fn discard_recovery(dir: &Path) -> Result<(), StoreError> {
    remove_file_durably(&dir.join(RECOVERY_FILE))
}

#[cfg(test)]
mod tests {
    use elizabeats_core::{RecoveryStatus, SourceId, StoredQueueItem, Track};

    use super::*;

    fn snapshot() -> RecoverySnapshot {
        RecoverySnapshot {
            version: 1,
            status: RecoveryStatus::Interrupted,
            queue: vec![
                StoredQueueItem::Track {
                    id: "q1".into(),
                    track: Track {
                        id: "Artist/Song.mp3".into(),
                        source: SourceId::Local,
                        title: "Song".into(),
                        artist: "Artist".into(),
                        album: Some("Album".into()),
                        duration_seconds: Some(181.5),
                        artwork_url: None,
                        explicit: false,
                    },
                },
                StoredQueueItem::Break { id: "q2".into() },
            ],
            session_started_at: Some("2026-10-06T20:00:00.000Z".into()),
            updated_at: "2026-10-06T20:05:00.000Z".into(),
        }
    }

    #[test]
    fn round_trip_and_discard() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_recovery(dir.path()).unwrap(), None);
        save_recovery(dir.path(), &snapshot()).unwrap();
        assert_eq!(read_recovery(dir.path()).unwrap(), Some(snapshot()));

        let mut clean = snapshot();
        clean.status = RecoveryStatus::Clean;
        save_recovery(dir.path(), &clean).unwrap();
        assert_eq!(read_recovery(dir.path()).unwrap(), Some(clean));

        discard_recovery(dir.path()).unwrap();
        assert_eq!(read_recovery(dir.path()).unwrap(), None);
        discard_recovery(dir.path()).unwrap(); // Already gone is fine.
    }

    #[test]
    fn reads_snapshots_written_by_the_electron_format() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(RECOVERY_FILE),
            r#"{"queue":[{"type":"break","id":"b"}],"sessionStartedAt":"s","status":"clean","updatedAt":"u","version":1}"#,
        )
        .unwrap();
        let read = read_recovery(dir.path()).unwrap().unwrap();
        assert_eq!(read.queue, vec![StoredQueueItem::Break { id: "b".into() }]);
    }

    #[test]
    fn invalid_snapshots_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RECOVERY_FILE);
        for invalid in [
            "",
            "{trunc",
            "null",
            r#"{"version":1,"status":"clean","queue":"nope","updatedAt":"u"}"#,
            r#"{"version":2,"status":"clean","queue":[],"updatedAt":"u"}"#,
        ] {
            std::fs::write(&path, invalid).unwrap();
            let error = read_recovery(dir.path()).unwrap_err();
            assert!(
                error
                    .to_string()
                    .starts_with("Queue recovery could not be read"),
                "{invalid}: {error}"
            );
        }
    }
}

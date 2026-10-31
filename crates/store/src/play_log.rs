//! `play-log.jsonl`: playback history.
//!
//! Each line is one complete [`PlayLogEntry`] as JSON. Entries are updated by
//! appending a new line with the same `id`; the last line for an id wins.
//! Appending (instead of rewriting) means a crash can at worst tear the line
//! being written, which readers skip.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use elizabeats_core::PlayLogEntry;

use crate::atomic::{remove_file_durably, sync_dir};
use crate::{StoreError, write_atomic};

pub const PLAY_LOG_FILE: &str = "play-log.jsonl";

/// Rewrite the file once this many lines have been replaced by later ones.
const COMPACT_AFTER_SUPERSEDED_LINES: usize = 256;

/// Serialises Play Log file access within the app.
static PLAY_LOG_LOCK: Mutex<()> = Mutex::new(());

/// The Play Log as read from disk.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlayLog {
    /// Newest `startedAt` first.
    pub entries: Vec<PlayLogEntry>,
    /// Lines that could not be understood (for example a line torn by a crash).
    pub skipped_lines: usize,
    /// Lines replaced by a later line with the same id.
    pub superseded_lines: usize,
}

/// The Play Log entries, newest first. Compacts the file when worthwhile.
pub fn read_play_log(dir: &Path) -> Result<Vec<PlayLogEntry>, StoreError> {
    load_play_log(dir).map(|log| log.entries)
}

/// Like [`read_play_log`], with counts of skipped and superseded lines.
pub fn load_play_log(dir: &Path) -> Result<PlayLog, StoreError> {
    let _guard = PLAY_LOG_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let path = dir.join(PLAY_LOG_FILE);
    let bytes = read_file(&path)?;
    let parsed = parse(&bytes);
    let log = parsed.to_play_log();
    if parsed.superseded_lines >= COMPACT_AFTER_SUPERSEDED_LINES {
        // Best effort: the entries were read successfully either way.
        if let Err(error) = write_atomic(&path, &parsed.compacted()) {
            tracing::warn!("Could not compact the play log: {error}");
        }
    }
    Ok(log)
}

/// Appends (or updates) entries and waits until they are on disk.
pub fn append_play_log(dir: &Path, entries: &[PlayLogEntry]) -> Result<(), StoreError> {
    if entries.is_empty() {
        return Ok(());
    }
    let mut lines = Vec::new();
    for entry in entries {
        serde_json::to_writer(&mut lines, entry).map_err(|source| StoreError::Encode {
            what: "the play log",
            source,
        })?;
        lines.push(b'\n');
    }

    let _guard = PLAY_LOG_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let path = dir.join(PLAY_LOG_FILE);
    std::fs::create_dir_all(dir).map_err(|source| StoreError::io("create", dir, source))?;
    let io_error = |source| StoreError::io("write", &path, source);
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(&path)
        .map_err(io_error)?;
    let length = file.metadata().map_err(io_error)?.len();

    // If a crash tore the previous line, start on a fresh line so this
    // write is not glued onto the broken one.
    let mut buffer = Vec::with_capacity(lines.len() + 1);
    if length > 0 && !ends_with_newline(&mut file).map_err(io_error)? {
        buffer.push(b'\n');
    }
    buffer.extend_from_slice(&lines);
    file.write_all(&buffer).map_err(io_error)?;
    file.sync_all().map_err(io_error)?;
    if length == 0 {
        // The file may be new; make its directory entry durable too.
        sync_dir(dir).map_err(|source| StoreError::io("sync", dir, source))?;
    }
    Ok(())
}

/// Deletes the whole Play Log.
pub fn clear_play_log(dir: &Path) -> Result<(), StoreError> {
    let _guard = PLAY_LOG_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    remove_file_durably(&dir.join(PLAY_LOG_FILE))
}

/// Rewrites the file with one line per entry. Lines that could not be parsed
/// are kept as they are (they may hold data from a newer app version), except
/// an unfinished final line torn by a crash.
pub fn compact_play_log(dir: &Path) -> Result<(), StoreError> {
    let _guard = PLAY_LOG_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let path = dir.join(PLAY_LOG_FILE);
    if !path.exists() {
        return Ok(());
    }
    let bytes = read_file(&path)?;
    let parsed = parse(&bytes);
    write_atomic(&path, &parsed.compacted())
}

fn read_file(path: &Path) -> Result<Vec<u8>, StoreError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(source) => Err(StoreError::io("read", path, source)),
    }
}

fn ends_with_newline(file: &mut std::fs::File) -> io::Result<bool> {
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    Ok(last[0] == b'\n')
}

struct Parsed<'a> {
    /// Latest version of each entry, in order of first appearance.
    entries: Vec<PlayLogEntry>,
    /// Complete lines that did not parse, kept verbatim by compaction.
    invalid_lines: Vec<&'a [u8]>,
    skipped_lines: usize,
    superseded_lines: usize,
}

fn parse(bytes: &[u8]) -> Parsed<'_> {
    let mut parsed = Parsed {
        entries: Vec::new(),
        invalid_lines: Vec::new(),
        skipped_lines: 0,
        superseded_lines: 0,
    };
    let mut positions: HashMap<String, usize> = HashMap::new();
    let complete = bytes.ends_with(b"\n");
    let lines: Vec<&[u8]> = bytes.split(|byte| *byte == b'\n').collect();
    let line_count = lines.len();
    for (index, line) in lines.into_iter().enumerate() {
        let is_last = index + 1 == line_count;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<PlayLogEntry>(line) {
            Ok(entry) => match positions.get(&entry.id) {
                Some(&position) => {
                    parsed.superseded_lines += 1;
                    parsed.entries[position] = entry;
                }
                None => {
                    positions.insert(entry.id.clone(), parsed.entries.len());
                    parsed.entries.push(entry);
                }
            },
            Err(_) => {
                parsed.skipped_lines += 1;
                // An unterminated final line is a torn write: drop it.
                if !(is_last && !complete) {
                    parsed.invalid_lines.push(line);
                }
            }
        }
    }
    parsed
}

impl Parsed<'_> {
    fn to_play_log(&self) -> PlayLog {
        let mut entries = self.entries.clone();
        // Newest first; ties by id, matching the IndexedDB order.
        entries.sort_by(|a, b| {
            b.started_at
                .cmp(&a.started_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        PlayLog {
            entries,
            skipped_lines: self.skipped_lines,
            superseded_lines: self.superseded_lines,
        }
    }

    fn compacted(&self) -> Vec<u8> {
        let mut entries: Vec<&PlayLogEntry> = self.entries.iter().collect();
        entries.sort_by(|a, b| {
            a.started_at
                .cmp(&b.started_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        let mut bytes = Vec::new();
        for line in &self.invalid_lines {
            bytes.extend_from_slice(line);
            bytes.push(b'\n');
        }
        for entry in entries {
            // Serialising a parsed entry cannot realistically fail; if it
            // somehow does, keep going rather than lose the other entries.
            if serde_json::to_writer(&mut bytes, entry).is_ok() {
                bytes.push(b'\n');
            }
        }
        bytes
    }
}

#[cfg(test)]
mod tests {
    use elizabeats_core::{PlayLogReason, StoredQueueItem};

    use super::*;

    fn entry(id: &str, started_at: &str, reason: Option<PlayLogReason>) -> PlayLogEntry {
        PlayLogEntry {
            id: id.into(),
            item: StoredQueueItem::Break {
                id: format!("item-{id}"),
            },
            started_at: started_at.into(),
            ended_at: reason.map(|_| format!("{started_at}+end")),
            reason,
        }
    }

    fn log_path(dir: &Path) -> std::path::PathBuf {
        dir.join(PLAY_LOG_FILE)
    }

    #[test]
    fn missing_file_is_an_empty_log() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_play_log(dir.path()).unwrap(), Vec::new());
    }

    #[test]
    fn later_lines_supersede_earlier_ones_and_newest_comes_first() {
        let dir = tempfile::tempdir().unwrap();
        append_play_log(
            dir.path(),
            &[
                entry("a", "2026-01-01T10:00:00.000Z", None),
                entry("b", "2026-01-01T10:03:00.000Z", None),
            ],
        )
        .unwrap();
        append_play_log(
            dir.path(),
            &[entry(
                "a",
                "2026-01-01T10:00:00.000Z",
                Some(PlayLogReason::Completed),
            )],
        )
        .unwrap();
        append_play_log(dir.path(), &[]).unwrap();

        let log = load_play_log(dir.path()).unwrap();
        assert_eq!(
            log.entries,
            vec![
                entry("b", "2026-01-01T10:03:00.000Z", None),
                entry(
                    "a",
                    "2026-01-01T10:00:00.000Z",
                    Some(PlayLogReason::Completed)
                ),
            ]
        );
        assert_eq!(log.superseded_lines, 1);
        assert_eq!(log.skipped_lines, 0);
    }

    #[test]
    fn equal_start_times_are_ordered_by_id() {
        let dir = tempfile::tempdir().unwrap();
        append_play_log(dir.path(), &[entry("z", "t", None), entry("m", "t", None)]).unwrap();
        let ids: Vec<String> = read_play_log(dir.path())
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(ids, ["m", "z"]);
    }

    #[test]
    fn torn_last_line_is_skipped_and_next_append_starts_a_new_line() {
        let dir = tempfile::tempdir().unwrap();
        append_play_log(dir.path(), &[entry("a", "1", None)]).unwrap();
        // Simulate a crash halfway through writing a line.
        let mut file = OpenOptions::new()
            .append(true)
            .open(log_path(dir.path()))
            .unwrap();
        file.write_all(br#"{"id":"b","item":{"type":"bre"#).unwrap();
        drop(file);

        let log = load_play_log(dir.path()).unwrap();
        assert_eq!(log.entries, vec![entry("a", "1", None)]);
        assert_eq!(log.skipped_lines, 1);

        append_play_log(dir.path(), &[entry("c", "2", None)]).unwrap();
        let log = load_play_log(dir.path()).unwrap();
        assert_eq!(
            log.entries,
            vec![entry("c", "2", None), entry("a", "1", None)]
        );
        assert_eq!(log.skipped_lines, 1);
    }

    #[test]
    fn invalid_lines_in_the_middle_are_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let good = serde_json::to_string(&entry("a", "1", None)).unwrap();
        std::fs::write(
            log_path(dir.path()),
            format!("garbage\n\n{good}\n{{\"id\":5}}\n   \n"),
        )
        .unwrap();
        let log = load_play_log(dir.path()).unwrap();
        assert_eq!(log.entries, vec![entry("a", "1", None)]);
        assert_eq!(log.skipped_lines, 2);
    }

    #[test]
    fn final_line_without_newline_still_counts_when_complete() {
        let dir = tempfile::tempdir().unwrap();
        let good = serde_json::to_string(&entry("a", "1", None)).unwrap();
        std::fs::write(log_path(dir.path()), good).unwrap();
        assert_eq!(
            read_play_log(dir.path()).unwrap(),
            vec![entry("a", "1", None)]
        );
        append_play_log(dir.path(), &[entry("b", "2", None)]).unwrap();
        assert_eq!(read_play_log(dir.path()).unwrap().len(), 2);
    }

    #[test]
    fn clear_removes_everything() {
        let dir = tempfile::tempdir().unwrap();
        append_play_log(dir.path(), &[entry("a", "1", None)]).unwrap();
        clear_play_log(dir.path()).unwrap();
        assert!(read_play_log(dir.path()).unwrap().is_empty());
        clear_play_log(dir.path()).unwrap();
        append_play_log(dir.path(), &[entry("b", "2", None)]).unwrap();
        assert_eq!(
            read_play_log(dir.path()).unwrap(),
            vec![entry("b", "2", None)]
        );
    }

    #[test]
    fn compaction_rewrites_superseded_lines_and_keeps_unknown_lines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(log_path(dir.path()), "from-the-future\n").unwrap();
        for round in 0..COMPACT_AFTER_SUPERSEDED_LINES {
            let reason = (round % 2 == 1).then_some(PlayLogReason::Skipped);
            append_play_log(
                dir.path(),
                &[entry("a", "1", reason), entry("b", "2", None)],
            )
            .unwrap();
        }
        let path = log_path(dir.path());
        // Torn final line, which compaction drops.
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"id\":").unwrap();
        drop(file);

        let before = std::fs::read_to_string(&path).unwrap().lines().count();
        let log = load_play_log(dir.path()).unwrap();
        assert!(log.superseded_lines >= COMPACT_AFTER_SUPERSEDED_LINES);
        let expected = vec![
            entry("b", "2", None),
            entry("a", "1", Some(PlayLogReason::Skipped)),
        ];
        assert_eq!(log.entries, expected);

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(before > 500);
        assert_eq!(after.lines().count(), 3, "{after}");
        assert!(after.starts_with("from-the-future\n"));
        assert!(after.ends_with('\n'));

        let log = load_play_log(dir.path()).unwrap();
        assert_eq!(log.entries, expected);
        assert_eq!(log.superseded_lines, 0);
        assert_eq!(log.skipped_lines, 1);
    }

    #[test]
    fn explicit_compaction_of_a_missing_file_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        compact_play_log(dir.path()).unwrap();
        assert!(!log_path(dir.path()).exists());
    }

    #[test]
    fn appends_from_many_threads_are_not_interleaved() {
        let dir = tempfile::tempdir().unwrap();
        std::thread::scope(|scope| {
            for thread in 0..8 {
                let dir = dir.path();
                scope.spawn(move || {
                    for index in 0..20 {
                        append_play_log(dir, &[entry(&format!("{thread}-{index}"), "t", None)])
                            .unwrap();
                    }
                });
            }
        });
        let log = load_play_log(dir.path()).unwrap();
        assert_eq!(log.entries.len(), 160);
        assert_eq!(log.skipped_lines, 0);
    }
}

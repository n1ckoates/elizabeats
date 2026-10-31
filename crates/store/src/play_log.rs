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
use std::sync::{Mutex, MutexGuard, PoisonError};

use elizabeats_core::PlayLogEntry;

use crate::StoreError;
use crate::files::{read_optional, remove_file, sync_dir, write_atomic};

const FILE: &str = "play-log.jsonl";

/// Rewrite the file once this many lines have been replaced by later ones.
const COMPACT_AFTER_SUPERSEDED_LINES: usize = 256;

/// Serialises Play Log file access within the app.
fn lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The Play Log entries, newest `startedAt` first (ties by id). Compacts the
/// file when enough lines have been superseded.
pub fn read_play_log(dir: &Path) -> Result<Vec<PlayLogEntry>, StoreError> {
    let _guard = lock();
    let bytes = read_optional(dir, FILE)?.unwrap_or_default();
    let parsed = parse(&bytes);
    if parsed.superseded_lines >= COMPACT_AFTER_SUPERSEDED_LINES {
        // Best effort: the entries were read successfully either way.
        if let Err(error) = write_atomic(dir, FILE, &parsed.compacted()) {
            tracing::warn!("Could not compact the play log: {error}");
        }
    }
    let mut entries = parsed.entries;
    entries.sort_by(|a, b| {
        b.started_at
            .cmp(&a.started_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(entries)
}

/// Appends (or updates) `entry` and waits until it is on disk.
pub fn append_play_log(dir: &Path, entry: &PlayLogEntry) -> Result<(), StoreError> {
    let mut line = serde_json::to_vec(entry)?;
    line.push(b'\n');

    let _guard = lock();
    std::fs::create_dir_all(dir).map_err(|source| StoreError::io("create", dir, source))?;
    let path = dir.join(FILE);
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
    if length > 0 && !ends_with_newline(&mut file).map_err(io_error)? {
        line.insert(0, b'\n');
    }
    file.write_all(&line).map_err(io_error)?;
    file.sync_all().map_err(io_error)?;
    if length == 0 {
        // The file may be new; make its directory entry durable too.
        sync_dir(dir)?;
    }
    Ok(())
}

/// Deletes the whole Play Log.
pub fn clear_play_log(dir: &Path) -> Result<(), StoreError> {
    let _guard = lock();
    remove_file(dir, FILE)
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
    /// Complete lines that did not parse. Compaction keeps them verbatim
    /// (they may hold data from a newer app version).
    invalid_lines: Vec<&'a [u8]>,
    /// Lines replaced by a later line with the same id.
    superseded_lines: usize,
}

fn parse(bytes: &[u8]) -> Parsed<'_> {
    let mut parsed = Parsed {
        entries: Vec::new(),
        invalid_lines: Vec::new(),
        superseded_lines: 0,
    };
    let mut positions: HashMap<String, usize> = HashMap::new();
    let mut lines = bytes.split(|byte| *byte == b'\n').peekable();
    while let Some(line) = lines.next() {
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
            // An unterminated final line is a torn write: drop it.
            Err(_) if lines.peek().is_none() => {}
            Err(_) => parsed.invalid_lines.push(line),
        }
    }
    parsed
}

impl Parsed<'_> {
    /// One line per entry, oldest first, after any lines that did not parse.
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
    use elizabeats_core::{PlayLogReason, QueueItem};

    use super::*;

    fn entry(id: &str, started_at: &str, reason: Option<PlayLogReason>) -> PlayLogEntry {
        PlayLogEntry {
            id: id.into(),
            item: QueueItem::new_break(format!("item-{id}")),
            started_at: started_at.into(),
            ended_at: reason.map(|_| format!("{started_at}+end")),
            reason,
        }
    }

    fn append_raw(dir: &Path, bytes: &[u8]) {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(FILE))
            .unwrap();
        file.write_all(bytes).unwrap();
    }

    fn ids(dir: &Path) -> Vec<String> {
        read_play_log(dir)
            .unwrap()
            .into_iter()
            .map(|entry| entry.id)
            .collect()
    }

    #[test]
    fn missing_file_is_an_empty_log() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_play_log(dir.path()).unwrap(), Vec::new());
    }

    #[test]
    fn later_lines_supersede_earlier_ones_and_newest_comes_first() {
        let dir = tempfile::tempdir().unwrap();
        let finished = entry(
            "a",
            "2026-01-01T10:00:00.000Z",
            Some(PlayLogReason::Completed),
        );
        append_play_log(dir.path(), &entry("a", "2026-01-01T10:00:00.000Z", None)).unwrap();
        append_play_log(dir.path(), &entry("b", "2026-01-01T10:03:00.000Z", None)).unwrap();
        append_play_log(dir.path(), &finished).unwrap();

        assert_eq!(
            read_play_log(dir.path()).unwrap(),
            vec![entry("b", "2026-01-01T10:03:00.000Z", None), finished]
        );
        let bytes = std::fs::read(dir.path().join(FILE)).unwrap();
        assert_eq!(parse(&bytes).superseded_lines, 1);
    }

    #[test]
    fn equal_start_times_are_ordered_by_id() {
        let dir = tempfile::tempdir().unwrap();
        append_play_log(dir.path(), &entry("z", "t", None)).unwrap();
        append_play_log(dir.path(), &entry("m", "t", None)).unwrap();
        assert_eq!(ids(dir.path()), ["m", "z"]);
    }

    #[test]
    fn torn_last_line_is_skipped_and_next_append_starts_a_new_line() {
        let dir = tempfile::tempdir().unwrap();
        append_play_log(dir.path(), &entry("a", "1", None)).unwrap();
        // Simulate a crash halfway through writing a line.
        append_raw(dir.path(), br#"{"id":"b","item":{"type":"bre"#);
        assert_eq!(ids(dir.path()), ["a"]);

        append_play_log(dir.path(), &entry("c", "2", None)).unwrap();
        assert_eq!(ids(dir.path()), ["c", "a"]);
    }

    #[test]
    fn invalid_lines_are_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let good = serde_json::to_string(&entry("a", "1", None)).unwrap();
        append_raw(
            dir.path(),
            format!("garbage\n\n{good}\n{{\"id\":5}}\n   \n").as_bytes(),
        );
        assert_eq!(ids(dir.path()), ["a"]);
    }

    #[test]
    fn final_line_without_newline_still_counts_when_complete() {
        let dir = tempfile::tempdir().unwrap();
        let good = serde_json::to_string(&entry("a", "1", None)).unwrap();
        append_raw(dir.path(), good.as_bytes());
        assert_eq!(ids(dir.path()), ["a"]);
        append_play_log(dir.path(), &entry("b", "2", None)).unwrap();
        assert_eq!(ids(dir.path()), ["b", "a"]);
    }

    #[test]
    fn clear_removes_everything() {
        let dir = tempfile::tempdir().unwrap();
        append_play_log(dir.path(), &entry("a", "1", None)).unwrap();
        clear_play_log(dir.path()).unwrap();
        assert!(ids(dir.path()).is_empty());
        clear_play_log(dir.path()).unwrap();
        append_play_log(dir.path(), &entry("b", "2", None)).unwrap();
        assert_eq!(ids(dir.path()), ["b"]);
    }

    #[test]
    fn compaction_rewrites_superseded_lines_and_keeps_unknown_lines() {
        let dir = tempfile::tempdir().unwrap();
        append_raw(dir.path(), b"from-the-future\n");
        for round in 0..COMPACT_AFTER_SUPERSEDED_LINES {
            let reason = (round % 2 == 1).then_some(PlayLogReason::Skipped);
            append_play_log(dir.path(), &entry("a", "1", reason)).unwrap();
            append_play_log(dir.path(), &entry("b", "2", None)).unwrap();
        }
        // Torn final line, which compaction drops.
        append_raw(dir.path(), b"{\"id\":");

        let path = dir.path().join(FILE);
        assert!(std::fs::read_to_string(&path).unwrap().lines().count() > 500);
        let expected = vec![
            entry("b", "2", None),
            entry("a", "1", Some(PlayLogReason::Skipped)),
        ];
        assert_eq!(read_play_log(dir.path()).unwrap(), expected);

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after.lines().count(), 3, "{after}");
        assert!(after.starts_with("from-the-future\n"));
        assert!(after.ends_with('\n'));
        assert_eq!(read_play_log(dir.path()).unwrap(), expected);
    }

    #[test]
    fn appends_from_many_threads_are_not_interleaved() {
        let dir = tempfile::tempdir().unwrap();
        std::thread::scope(|scope| {
            for thread in 0..8 {
                let dir = dir.path();
                scope.spawn(move || {
                    for index in 0..20 {
                        append_play_log(dir, &entry(&format!("{thread}-{index}"), "t", None))
                            .unwrap();
                    }
                });
            }
        });
        let bytes = std::fs::read(dir.path().join(FILE)).unwrap();
        let parsed = parse(&bytes);
        assert_eq!(parsed.entries.len(), 160);
        assert!(parsed.invalid_lines.is_empty());
    }
}

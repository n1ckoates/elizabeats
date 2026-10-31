//! Crash-safe file helpers. Every file lives directly in the data folder.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::StoreError;

/// Marks temporary files so leftovers from a crash can be recognised.
const TEMP_MARKER: &str = ".tmp-";

/// The contents of `dir/name`, or `None` when it does not exist.
pub(crate) fn read_optional(dir: &Path, name: &str) -> Result<Option<Vec<u8>>, StoreError> {
    let path = dir.join(name);
    match fs::read(&path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(StoreError::io("read", &path, source)),
    }
}

/// Replaces `dir/name` with `bytes` atomically: the data is written and synced
/// to a temporary file in `dir`, which is then renamed over the real file.
/// Readers (and a crash at any moment) see either the old or the new file.
pub(crate) fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), StoreError> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    fs::create_dir_all(dir).map_err(|source| StoreError::io("create", dir, source))?;
    let path = dir.join(name);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = dir.join(format!(
        ".{name}{TEMP_MARKER}{}-{count}",
        std::process::id()
    ));
    let result = write_and_sync(&temp, bytes)
        .map_err(|source| StoreError::io("write", &temp, source))
        .and_then(|()| {
            fs::rename(&temp, &path).map_err(|source| StoreError::io("replace", &path, source))
        });
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result?;
    sync_dir(dir)
}

/// Removes `dir/name`, treating "already gone" as success.
pub(crate) fn remove_file(dir: &Path, name: &str) -> Result<(), StoreError> {
    let path = dir.join(name);
    match fs::remove_file(&path) {
        Ok(()) => sync_dir(dir),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(StoreError::io("delete", &path, source)),
    }
}

/// Deletes temporary files left behind by a crash mid-write. Call once at
/// startup, before anything else writes to `dir`.
pub fn remove_stale_temp_files(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let is_file = entry.file_type().is_ok_and(|kind| kind.is_file());
        if is_file && name.starts_with('.') && name.contains(TEMP_MARKER) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn write_and_sync(temp: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(temp)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Makes a rename or file creation in `dir` durable. (Windows cannot open
/// folders as files; NTFS journals renames itself.)
pub(crate) fn sync_dir(dir: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    fs::File::open(dir)
        .and_then(|folder| folder.sync_all())
        .map_err(|source| StoreError::io("sync", dir, source))?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_contents_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        write_atomic(dir.path(), "file.json", b"one").unwrap();
        write_atomic(dir.path(), "file.json", b"two").unwrap();
        assert_eq!(fs::read(dir.path().join("file.json")).unwrap(), b"two");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn crash_leftovers_do_not_affect_the_real_file_and_are_cleaned_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        write_atomic(dir.path(), "settings.json", b"good").unwrap();
        // What a crash between "create temp" and "rename" leaves behind.
        let leftover = dir.path().join(".settings.json.tmp-999-0");
        fs::write(&leftover, b"half-writ").unwrap();
        let unrelated = dir.path().join("play-log.jsonl");
        fs::write(&unrelated, b"keep").unwrap();

        write_atomic(dir.path(), "settings.json", b"better").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"better");

        remove_stale_temp_files(dir.path());
        assert!(!leftover.exists());
        assert!(unrelated.exists());
        assert!(path.exists());
    }

    #[test]
    fn creates_a_missing_folder() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("a/b");
        write_atomic(&folder, "c.json", b"x").unwrap();
        assert_eq!(read_optional(&folder, "c.json").unwrap().unwrap(), b"x");
    }

    #[test]
    fn failed_write_keeps_the_old_file() {
        let dir = tempfile::tempdir().unwrap();
        // Renaming a file over a non-empty folder fails.
        let path = dir.path().join("target");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("inside"), b"x").unwrap();
        let error = write_atomic(dir.path(), "target", b"new").unwrap_err();
        assert!(
            error.to_string().starts_with("Could not replace"),
            "{error}"
        );
        assert!(path.join("inside").exists());
        // The temporary file was removed.
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn missing_files_read_as_none_and_remove_quietly() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_optional(dir.path(), "nope").unwrap().is_none());
        remove_file(dir.path(), "nope").unwrap();
    }
}

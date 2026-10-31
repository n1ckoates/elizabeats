//! Crash-safe file replacement.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::StoreError;

/// Marks temporary files so leftovers from a crash can be recognised.
const TEMP_MARKER: &str = ".tmp-";

/// Replaces `path` with `bytes` atomically: the data is written and synced to
/// a temporary file in the same folder, which is then renamed over `path`.
/// Readers (and a crash at any moment) see either the old or the new file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let dir = parent_dir(path);
    fs::create_dir_all(dir).map_err(|source| StoreError::io("create", dir, source))?;
    let temp = temp_path(path);
    let result = write_and_sync(&temp, bytes)
        .map_err(|source| StoreError::io("write", &temp, source))
        .and_then(|()| {
            fs::rename(&temp, path).map_err(|source| StoreError::io("replace", path, source))
        });
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result?;
    sync_dir(dir).map_err(|source| StoreError::io("sync", dir, source))
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

pub(crate) fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

fn temp_path(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    parent_dir(path).join(format!(
        ".{name}{TEMP_MARKER}{}-{count}",
        std::process::id()
    ))
}

/// Makes a rename or file creation in `dir` durable.
#[cfg(unix)]
pub(crate) fn sync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

/// Windows cannot open folders as files; NTFS journals renames itself.
#[cfg(not(unix))]
pub(crate) fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Removes `path`, treating "already gone" as success.
pub(crate) fn remove_file_durably(path: &Path) -> Result<(), StoreError> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(StoreError::io("delete", path, source)),
    }
    let dir = parent_dir(path);
    sync_dir(dir).map_err(|source| StoreError::io("sync", dir, source))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_contents_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.json");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn crash_leftovers_do_not_affect_the_real_file_and_are_cleaned_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        write_atomic(&path, b"good").unwrap();
        // What a crash between "create temp" and "rename" leaves behind.
        let leftover = dir.path().join(".settings.json.tmp-999-0");
        fs::write(&leftover, b"half-writ").unwrap();
        let unrelated = dir.path().join("play-log.jsonl");
        fs::write(&unrelated, b"keep").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"good");
        write_atomic(&path, b"better").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"better");

        remove_stale_temp_files(dir.path());
        assert!(!leftover.exists());
        assert!(unrelated.exists());
        assert!(path.exists());
    }

    #[test]
    fn creates_missing_parent_folders() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/c.json");
        write_atomic(&path, b"x").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"x");
    }

    #[test]
    fn failed_write_keeps_the_old_file() {
        let dir = tempfile::tempdir().unwrap();
        // Renaming a file over a non-empty folder fails.
        let path = dir.path().join("target");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("inside"), b"x").unwrap();
        let error = write_atomic(&path, b"new").unwrap_err();
        assert!(
            error.to_string().starts_with("Could not replace"),
            "{error}"
        );
        assert!(path.join("inside").exists());
        // The temporary file was removed.
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}

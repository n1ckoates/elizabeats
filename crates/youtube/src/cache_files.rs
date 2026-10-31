//! Files inside `download-cache/`: bounded paths and atomic writes.
//!
//! Every audio filename is validated before it is joined to the directory, so
//! a malformed manifest can never make us read or delete files elsewhere.
//! Audio is written to `<name>.<uuid>.part` and renamed into place only once
//! it is complete and flushed to disk.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use tokio::io::AsyncWriteExt;

pub(crate) const MANIFEST_FILE: &str = "manifest.json";
const PART_SUFFIX: &str = ".part";
const TEMP_SUFFIX: &str = ".tmp";

/// Audio extensions the original app could write. Only `m4a` is produced now.
const AUDIO_EXTENSIONS: [&str; 3] = ["m4a", "ogg", "webm"];

#[derive(Clone, Debug)]
pub(crate) struct CacheFiles {
    dir: PathBuf,
}

/// True for `<letters, digits, _ or ->.<m4a|ogg|webm>`.
pub(crate) fn is_valid_audio_file_name(name: &str) -> bool {
    let Some((stem, extension)) = name.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty()
        && stem
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && AUDIO_EXTENSIONS.contains(&extension)
}

impl CacheFiles {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// The full path of a cached audio file. Fails for unsafe names.
    pub(crate) fn path(&self, file_name: &str) -> io::Result<PathBuf> {
        if is_valid_audio_file_name(file_name) {
            Ok(self.dir.join(file_name))
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid cache filename",
            ))
        }
    }

    pub(crate) async fn ensure_dir(&self) -> io::Result<()> {
        tokio::fs::create_dir_all(&self.dir).await
    }

    /// Size of a cached file, or `None` if it is missing or not a file.
    pub(crate) async fn size(&self, file_name: &str) -> Option<u64> {
        let path = self.path(file_name).ok()?;
        let meta = tokio::fs::metadata(path).await.ok()?;
        meta.is_file().then_some(meta.len())
    }

    /// Deletes a cached file; a file that is already gone is not an error.
    pub(crate) async fn remove(&self, file_name: &str) -> io::Result<()> {
        match tokio::fs::remove_file(self.path(file_name)?).await {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    /// Starts writing `file_name` through a uniquely named `.part` file.
    pub(crate) async fn begin(&self, file_name: &str) -> io::Result<PartFile> {
        let destination = self.path(file_name)?;
        self.ensure_dir().await?;
        let temporary = self
            .dir
            .join(format!("{file_name}.{}{PART_SUFFIX}", uuid::Uuid::new_v4()));
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .await?;
        Ok(PartFile {
            file: Some(file),
            temporary,
            destination,
            written: 0,
        })
    }

    /// Reads and parses `manifest.json`; `None` if missing or not JSON.
    pub(crate) async fn read_manifest(&self) -> Option<serde_json::Value> {
        let bytes = tokio::fs::read(self.dir.join(MANIFEST_FILE)).await.ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Replaces `manifest.json` atomically (temp file, fsync, rename).
    pub(crate) async fn write_manifest(&self, contents: &[u8]) -> io::Result<()> {
        self.ensure_dir().await?;
        let temporary = self.dir.join(format!(
            "{MANIFEST_FILE}.{}{TEMP_SUFFIX}",
            uuid::Uuid::new_v4()
        ));
        let result = async {
            let mut file = tokio::fs::File::create(&temporary).await?;
            file.write_all(contents).await?;
            file.sync_all().await?;
            drop(file);
            tokio::fs::rename(&temporary, self.dir.join(MANIFEST_FILE)).await?;
            sync_dir(&self.dir).await;
            Ok(())
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&temporary).await;
        }
        result
    }

    /// Deletes leftover `.part` and manifest temp files from an earlier run.
    /// Part files belonging to `active_video_ids` are kept.
    pub(crate) async fn remove_stale_temp_files(&self, active_video_ids: &HashSet<String>) {
        let Ok(mut dir) = tokio::fs::read_dir(&self.dir).await else {
            return;
        };
        while let Ok(Some(item)) = dir.next_entry().await {
            let name = item.file_name().to_string_lossy().into_owned();
            let is_part = name.ends_with(PART_SUFFIX);
            let is_manifest_temp =
                name.starts_with(&format!("{MANIFEST_FILE}.")) && name.ends_with(TEMP_SUFFIX);
            let owner = name.split('.').next().unwrap_or_default();
            let stale = (is_part && !active_video_ids.contains(owner)) || is_manifest_temp;
            if stale && let Err(error) = tokio::fs::remove_file(item.path()).await {
                tracing::warn!("could not remove stale cache file {name}: {error}");
            }
        }
    }

    /// Audio files (with valid names) currently on disk, with their sizes.
    pub(crate) async fn list_audio_files(&self) -> Vec<(String, u64)> {
        let mut files = Vec::new();
        let Ok(mut dir) = tokio::fs::read_dir(&self.dir).await else {
            return files;
        };
        while let Ok(Some(item)) = dir.next_entry().await {
            let name = item.file_name().to_string_lossy().into_owned();
            if !is_valid_audio_file_name(&name) {
                continue;
            }
            if let Ok(meta) = item.metadata().await
                && meta.is_file()
            {
                files.push((name, meta.len()));
            }
        }
        files
    }
}

/// An in-progress audio file. Dropping it without [`PartFile::finish`]
/// deletes the temporary file, so failed or cancelled downloads leave nothing behind.
pub(crate) struct PartFile {
    file: Option<tokio::fs::File>,
    temporary: PathBuf,
    destination: PathBuf,
    written: u64,
}

impl PartFile {
    pub(crate) async fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        let file = self.file.as_mut().ok_or_else(closed)?;
        file.write_all(bytes).await?;
        self.written += bytes.len() as u64;
        Ok(())
    }

    pub(crate) fn written(&self) -> u64 {
        self.written
    }

    /// Flushes to disk and renames into place. Returns the final size.
    pub(crate) async fn finish(mut self) -> io::Result<u64> {
        let mut file = self.file.take().ok_or_else(closed)?;
        file.flush().await?;
        file.sync_all().await?;
        let size = file.metadata().await?.len();
        drop(file);
        if size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "downloaded audio is empty",
            ));
        }
        tokio::fs::rename(&self.temporary, &self.destination).await?;
        // The rename succeeded, so there is nothing left for Drop to clean up.
        self.temporary = PathBuf::new();
        if let Some(dir) = self.destination.parent() {
            sync_dir(dir).await;
        }
        Ok(size)
    }
}

impl Drop for PartFile {
    fn drop(&mut self) {
        // Close first so removal also works on platforms that lock open files.
        drop(self.file.take());
        if !self.temporary.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.temporary);
        }
    }
}

fn closed() -> io::Error {
    io::Error::other("cache file already closed")
}

/// Makes a rename durable on Unix by syncing the directory. Best effort.
async fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    {
        let dir = dir.to_owned();
        let _ = tokio::task::spawn_blocking(move || std::fs::File::open(dir)?.sync_all()).await;
    }
    #[cfg(not(unix))]
    let _ = dir;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_file_names() {
        assert!(is_valid_audio_file_name("FGBhQbmPwH8.m4a"));
        assert!(is_valid_audio_file_name("a_b-c.webm"));
        assert!(is_valid_audio_file_name("x.ogg"));
        for bad in [
            "",
            ".m4a",
            "abc",
            "abc.mp3",
            "../abc.m4a",
            "a/b.m4a",
            "a\\b.m4a",
            "abc.m4a.part",
            "manifest.json",
            "a b.m4a",
        ] {
            assert!(!is_valid_audio_file_name(bad), "{bad}");
        }
        let files = CacheFiles::new(PathBuf::from("/cache"));
        assert!(files.path("../../etc/passwd").is_err());
        assert_eq!(
            files.path("abc.m4a").ok(),
            Some(PathBuf::from("/cache/abc.m4a"))
        );
    }

    #[tokio::test]
    async fn part_file_finishes_atomically_or_cleans_up() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let files = CacheFiles::new(temp.path().join("download-cache"));

        let mut part = files.begin("abc.m4a").await?;
        part.write_all(b"hello").await?;
        assert!(
            files.size("abc.m4a").await.is_none(),
            "not visible before finish"
        );
        assert_eq!(part.finish().await?, 5);
        assert_eq!(files.size("abc.m4a").await, Some(5));

        let mut part = files.begin("def.m4a").await?;
        part.write_all(b"partial").await?;
        drop(part);
        let empty = files.begin("ghi.m4a").await?;
        assert!(empty.finish().await.is_err(), "empty files are rejected");

        let names: Vec<_> = std::fs::read_dir(temp.path().join("download-cache"))?
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["abc.m4a".to_owned()]);
        Ok(())
    }

    #[tokio::test]
    async fn manifest_write_is_atomic_and_stale_files_are_removed() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let dir = temp.path().to_owned();
        let files = CacheFiles::new(dir.clone());
        files
            .write_manifest(b"{\"version\":1,\"entries\":[]}\n")
            .await?;
        assert!(files.read_manifest().await.is_some());

        std::fs::write(dir.join("aaaaaaaaaaa.m4a.1234.part"), b"x")?;
        std::fs::write(dir.join("bbbbbbbbbbb.m4a.5678.part"), b"x")?;
        std::fs::write(dir.join("manifest.json.99.tmp"), b"x")?;
        std::fs::write(dir.join("ccccccccccc.m4a"), b"audio")?;
        let active = HashSet::from(["bbbbbbbbbbb".to_owned()]);
        files.remove_stale_temp_files(&active).await;

        let mut names: Vec<_> = std::fs::read_dir(&dir)?
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "bbbbbbbbbbb.m4a.5678.part",
                "ccccccccccc.m4a",
                "manifest.json"
            ]
        );
        assert_eq!(
            files.list_audio_files().await,
            vec![("ccccccccccc.m4a".to_owned(), 5)]
        );
        Ok(())
    }
}

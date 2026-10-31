//! Everything Elizabeats keeps on disk in its app data folder:
//!
//! - `settings.json` — user Settings ([`load_settings`], [`save_settings`])
//! - `recovery.json` — the Queue snapshot used after an unclean shutdown
//!   ([`read_recovery`], [`save_recovery`], [`discard_recovery`])
//! - `play-log.jsonl` — append-only playback history ([`read_play_log`],
//!   [`append_play_log`], [`clear_play_log`])
//! - `diagnostics.jsonl` — redacted local diagnostics ([`DiagnosticsStore`])
//!
//! Whole-file writes are atomic: a crash leaves either the old or the new
//! file, never a truncated one.

// Tests may panic on failure; production code may not.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod diagnostics;
mod files;
mod play_log;
mod recovery;
mod settings;

use std::io;
use std::path::{Path, PathBuf};

pub use diagnostics::DiagnosticsStore;
pub use files::remove_stale_temp_files;
pub use play_log::{append_play_log, clear_play_log, read_play_log};
pub use recovery::{discard_recovery, read_recovery, save_recovery};
pub use settings::{load_settings, save_settings};

/// A storage failure, worded so it can be shown to the DJ as-is.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("Could not {action} {}: {source}", path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The file's contents are not understood; callers say which file.
    #[error("{0}")]
    Invalid(String),
    #[error("Could not encode JSON: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("Could not find the app data folder.")]
    NoDataDir,
}

impl StoreError {
    pub(crate) fn io(action: &'static str, path: &Path, source: io::Error) -> Self {
        Self::Io {
            action,
            path: path.to_path_buf(),
            source,
        }
    }
}

/// The per-user data folder, created if missing. On macOS:
/// `~/Library/Application Support/Elizabeats`.
pub fn data_dir() -> Result<PathBuf, StoreError> {
    let dir = dirs::data_dir()
        .ok_or(StoreError::NoDataDir)?
        .join("Elizabeats");
    std::fs::create_dir_all(&dir).map_err(|source| StoreError::io("create", &dir, source))?;
    Ok(dir)
}

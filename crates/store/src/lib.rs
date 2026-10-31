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

mod atomic;
mod diagnostics;
mod error;
mod play_log;
mod recovery;
mod settings;

use std::path::PathBuf;

pub use atomic::{remove_stale_temp_files, write_atomic};
pub use diagnostics::{DiagnosticsStore, MAX_DIAGNOSTICS_BYTES, MAX_ROTATED_FILES, redact};
pub use error::StoreError;
pub use play_log::{
    PLAY_LOG_FILE, PlayLog, append_play_log, clear_play_log, compact_play_log, load_play_log,
    read_play_log,
};
pub use recovery::{
    RECOVERY_FILE, RECOVERY_VERSION, discard_recovery, read_recovery, save_recovery,
};
pub use settings::{SETTINGS_FILE, load_settings, normalize_settings, save_settings};

pub const DIAGNOSTICS_FILE: &str = "diagnostics.jsonl";

const APP_FOLDER: &str = "Elizabeats";

/// The per-user data folder, created if missing. On macOS:
/// `~/Library/Application Support/Elizabeats`.
pub fn data_dir() -> Result<PathBuf, StoreError> {
    let base = dirs::data_dir().ok_or(StoreError::NoDataDir)?;
    let dir = base.join(APP_FOLDER);
    std::fs::create_dir_all(&dir).map_err(|source| StoreError::io("create", &dir, source))?;
    Ok(dir)
}

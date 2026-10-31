use std::io;
use std::path::{Path, PathBuf};

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
    #[error("{what} could not be read: {detail}")]
    Invalid { what: &'static str, detail: String },
    #[error("Could not save {what}: {source}")]
    Encode {
        what: &'static str,
        #[source]
        source: serde_json::Error,
    },
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

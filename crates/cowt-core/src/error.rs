//! Error types for the co-worktree core engine.

use std::path::PathBuf;

/// Errors produced by manifest scanning, diffing and merging.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("path escapes the base directory boundary: {0}")]
    BoundaryEscape(PathBuf),

    #[error("manifest is corrupted: {0}")]
    CorruptManifest(String),

    #[error("unsupported manifest format: {0}")]
    UnsupportedFormat(String),

    #[error("merge has {0} conflict(s); target environment was not modified")]
    Conflicts(usize),

    #[error("serialization error: {0}")]
    Serde(String),
}

impl Error {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

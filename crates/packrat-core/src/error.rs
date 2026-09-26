//! Error types for disc access and parsing.

use std::path::PathBuf;

/// Anything that can go wrong while opening or reading a disc.
#[derive(Debug, thiserror::Error)]
pub enum DiscError {
    #[error("no VIDEO_TS directory found under {0}")]
    NoVideoTs(String),

    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("not a DVD-Video IFO file ({0}: magic {1:?})")]
    NotAnIfo(String, [u8; 12]),

    #[error("malformed IFO {path}: {message}")]
    Ifo { path: PathBuf, message: String },

    #[error("remux failed: {0}")]
    Remux(String),
}

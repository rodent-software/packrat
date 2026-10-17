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

    /// Blu-ray structure or title could not be read.
    #[error("Blu-ray error: {0}")]
    BluRay(String),

    /// A Blu-ray is AACS-encrypted and no usable key was found.
    #[error("AACS decryption unavailable: {0}")]
    AacsKey(String),

    /// libdvdcss is needed but could not be loaded; the string explains why
    /// and how to supply it.
    #[error("CSS decryption unavailable: {0}")]
    Dvdcss(String),

    /// The drive is not ready to answer yet — spinning up, seeking, or busy
    /// with another command. That is a delay rather than damage, so the read
    /// paths wait it out instead of writing the sector off as unreadable.
    #[error("drive not ready: {0}")]
    NotReady(String),

    /// The caller asked the remux to stop; any half-written output is removed.
    #[error("remux cancelled")]
    Cancelled,
}

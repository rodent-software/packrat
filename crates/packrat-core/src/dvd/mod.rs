//! Packrat-owned DVD-Video structure, filesystem, and VOB demuxing.
//!
//! This module is adapted from the MIT-licensed `oxideav-dvd` implementation
//! (Copyright (c) 2026 Karpelès Lab Inc.; see `LICENSE`) and lives in-tree so
//! packrat can fix parser bugs without waiting on an upstream release.

pub mod ac3;
pub mod disc;
pub mod dts;
pub mod error;
pub mod ifo;
pub mod iso9660;
pub mod lpcm;
pub mod mpeg;
pub mod nav;
pub mod spu;
pub mod udf;
pub mod uops;
pub mod vob;

pub use ac3::{Ac3AudioCodingMode, Ac3BitstreamMode, Ac3Header, Ac3SampleRate, AC3_SYNC_WORD};
pub use disc::{DvdDisc, DvdFile, DvdFileKind};
pub use dts::{DtsAudioMode, DtsBitRate, DtsFrameType, DtsHeader, DtsSampleRate, DTS_SYNC_WORD};
pub use error::{Error, Result};
pub use ifo::*;
pub use iso9660::*;
pub use lpcm::*;
pub use mpeg::*;
pub use udf::{UdfFile, UdfVolume};
pub use vob::*;

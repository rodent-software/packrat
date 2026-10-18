#![allow(dead_code)]
//! Packrat-owned Matroska reader/writer support.
//!
//! Adapted from the MIT-licensed `oxideav-mkv` implementation and kept in-tree
//! so muxing fixes do not depend on an upstream release. The upstream license
//! is kept alongside this module.

pub mod avc;
pub mod codec_id;
pub mod demux;
pub mod ebml;
pub mod ids;
pub mod mux;
pub mod schema;
pub mod webm;

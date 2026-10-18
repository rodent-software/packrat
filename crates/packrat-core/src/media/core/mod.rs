#![allow(dead_code, unused_imports)]
//! Shared media value types and container traits.
//!
//! These are the packrat-owned equivalents of the small `oxideav-core` surface
//! the muxer, demuxer, and remux code use. Adapted from the MIT-licensed
//! `oxideav-core` crate; its license is kept alongside this module.

pub mod error;
pub mod format;
pub mod limits;
pub mod metadata;
pub mod options;
pub mod packet;
pub mod picture;
pub mod rational;
pub mod stream;
pub mod time;

pub use error::{Error, Result};
pub use format::{ChannelLayout, ChannelPosition, MediaType, PixelFormat, SampleFormat};
pub use limits::DecoderLimits;
pub use metadata::{Attachment, Chapter};
pub use options::CodecOptions;
pub use packet::{Packet, PacketFlags};
pub use picture::{AttachedPicture, PictureType};
pub use rational::Rational;
pub use stream::{
    CodecId, CodecParameters, CodecResolver, CodecTag, NullCodecResolver, ProbeContext, StreamInfo,
};
pub use time::{TimeBase, Timestamp};

use std::io::{Read, Seek, Write};

/// Seekable input used by demuxers and title readers.
pub trait ReadSeek: Read + Seek + Send {}
impl<T: Read + Seek + Send> ReadSeek for T {}

/// Seekable output used by the Matroska writer.
pub trait WriteSeek: Write + Seek + Send {}
impl<T: Write + Seek + Send> WriteSeek for T {}

/// Container muxer surface used by packrat.
pub trait Muxer: Send {
    fn format_name(&self) -> &str;
    fn write_header(&mut self) -> Result<()>;
    fn write_packet(&mut self, packet: &Packet) -> Result<()>;
    fn write_trailer(&mut self) -> Result<()>;
}

/// Container demuxer surface used by packrat.
pub trait Demuxer: Send {
    fn format_name(&self) -> &str;
    fn streams(&self) -> &[StreamInfo];
    fn next_packet(&mut self) -> Result<Packet>;
    fn set_active_streams(&mut self, _indices: &[u32]) {}
    fn seek_to(&mut self, _stream_index: u32, _pts: i64) -> Result<i64> {
        Err(Error::unsupported("this demuxer does not support seeking"))
    }
    fn metadata(&self) -> &[(String, String)] {
        &[]
    }
    fn duration_micros(&self) -> Option<i64> {
        None
    }
    fn attached_pictures(&self) -> &[AttachedPicture] {
        &[]
    }
    fn chapters(&self) -> &[Chapter] {
        &[]
    }
    fn attachments(&self) -> &[Attachment] {
        &[]
    }
}

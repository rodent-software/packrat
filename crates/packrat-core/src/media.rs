//! Internal media plumbing boundary.
//!
//! The ripping modules talk to MPEG-TS demuxing, Matroska muxing, and the
//! packet/track types through this module. Today the implementations are the
//! `oxideav` crates; keeping the `use` statements here means each one can be
//! replaced without touching the disc or title code.
//!
//! This is deliberately a narrow wrapper, not a second media framework. The
//! wrappers expose only the calls packrat makes today.

pub(crate) use oxideav_core::packet::PacketFlags;
pub(crate) use oxideav_core::{
    CodecId, CodecParameters, Error as MediaError, Packet, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_core::Muxer;

mod ts;

pub(crate) use ts::TsDemuxer;

/// Matroska writer used by both the DVD and Blu-ray paths.
pub(crate) struct MatroskaMuxer {
    inner: crate::mkv::mux::MkvMuxer,
}

impl MatroskaMuxer {
    pub(crate) fn new(
        output: Box<dyn WriteSeek>,
        streams: &[StreamInfo],
    ) -> Result<Self, MediaError> {
        Ok(Self {
            inner: crate::mkv::mux::MkvMuxer::new_matroska(output, streams)?,
        })
    }

    pub(crate) fn with_duration_finalization(&mut self) -> Result<(), MediaError> {
        self.inner.with_duration_finalization()?;
        Ok(())
    }

    pub(crate) fn set_video_aspect_ratio(
        &mut self,
        stream_index: usize,
        numerator: u64,
        denominator: u64,
    ) -> Result<(), MediaError> {
        self.inner
            .set_video_geometry(
                stream_index,
                crate::mkv::mux::MkvVideoGeometry::aspect_ratio(numerator, denominator),
            )
            .map(|_| ())
            .map_err(Into::into)
    }

    pub(crate) fn add_chapter(
        &mut self,
        start_ns: u64,
        end_ns: Option<u64>,
        title: impl Into<String>,
    ) -> Result<(), MediaError> {
        self.inner.add_chapter(start_ns, end_ns, title)
    }

    pub(crate) fn write_header(&mut self) -> Result<(), MediaError> {
        self.inner.write_header()
    }

    pub(crate) fn write_packet(&mut self, packet: &Packet) -> Result<(), MediaError> {
        self.inner.write_packet(packet)
    }

    pub(crate) fn write_trailer(&mut self) -> Result<(), MediaError> {
        self.inner.write_trailer()
    }
}

/// Build Matroska's AVCDecoderConfigurationRecord from Annex-B SPS/PPS.
pub(crate) fn avc_config_record(data: &[u8]) -> Vec<u8> {
    crate::mkv::avc::annexb_to_avcc(data).config_record
}

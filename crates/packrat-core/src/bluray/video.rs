//! Blu-ray video codec framing and Matroska codec-private construction.
//!
//! H.264 and HEVC are Annex-B NAL streams, MPEG-2 is an elementary stream of
//! start-code units, and VC-1 is a byte stream framed by sequence/entry/frame
//! start codes. This module builds the Matroska configuration records for
//! HEVC and VC-1 and assembles the non-NAL codecs into Matroska blocks.

use crate::media::core::{Packet, PacketFlags, TimeBase};

use super::{find_start_code, MuxWriter};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VideoCodec {
    H264,
    Hevc,
    Vc1,
    Mpeg2,
}

impl VideoCodec {
    pub(super) fn from_id(id: &str) -> Option<Self> {
        match id {
            "h264" => Some(Self::H264),
            "hevc" | "h265" => Some(Self::Hevc),
            "vc1" => Some(Self::Vc1),
            "mpeg2video" => Some(Self::Mpeg2),
            _ => None,
        }
    }

    pub(super) fn needs_config(self) -> bool {
        !matches!(self, Self::Mpeg2)
    }

    pub(super) fn has_parameter_set(self, data: &[u8]) -> bool {
        match self {
            Self::H264 => super::contains_nal_type(data, 7),
            Self::Hevc => contains_nal_type_range(data, 33, 33),
            Self::Vc1 => contains_start_code(data, 0x0F),
            Self::Mpeg2 => false,
        }
    }

    pub(super) fn is_keyframe(self, data: &[u8]) -> bool {
        match self {
            Self::H264 => super::contains_nal_type(data, 5),
            Self::Hevc => contains_nal_type_range(data, 16, 23),
            Self::Vc1 => contains_start_code(data, 0x0D),
            Self::Mpeg2 => mpeg2_has_i_picture(data),
        }
    }

    pub(super) fn config_record(self, data: &[u8], width: u32, height: u32) -> Vec<u8> {
        match self {
            Self::H264 => super::avc_config_record(data),
            Self::Hevc => hevc_config_record(data),
            Self::Vc1 => vc1_codec_private(data, width, height),
            Self::Mpeg2 => Vec::new(),
        }
    }
}

pub(super) enum VideoAssembler {
    H264(super::AuAssembler),
    Hevc(super::AuAssembler),
    Mpeg2(ByteAssembler),
    Vc1(ByteAssembler),
}

impl VideoAssembler {
    pub(super) fn new(codec: VideoCodec, video: Option<u32>, title_start: u64) -> Self {
        match codec {
            VideoCodec::H264 => Self::H264(super::AuAssembler::new(
                video,
                title_start,
                super::NalKind::H264,
            )),
            VideoCodec::Hevc => Self::Hevc(super::AuAssembler::new(
                video,
                title_start,
                super::NalKind::Hevc,
            )),
            VideoCodec::Mpeg2 => {
                Self::Mpeg2(ByteAssembler::new(video, title_start, ByteFraming::Mpeg2))
            }
            VideoCodec::Vc1 => Self::Vc1(ByteAssembler::new(video, title_start, ByteFraming::Vc1)),
        }
    }

    pub(super) fn feed(
        &mut self,
        writer: &mut MuxWriter<'_>,
        packet: Packet,
    ) -> Result<(), crate::error::DiscError> {
        match self {
            Self::H264(a) | Self::Hevc(a) => a.feed(writer, packet),
            Self::Mpeg2(a) | Self::Vc1(a) => a.feed(writer, packet),
        }
    }

    pub(super) fn finish(
        &mut self,
        writer: &mut MuxWriter<'_>,
    ) -> Result<(), crate::error::DiscError> {
        match self {
            Self::H264(a) | Self::Hevc(a) => a.finish(writer),
            Self::Mpeg2(a) | Self::Vc1(a) => a.finish(writer),
        }
    }
}

#[derive(Clone, Copy)]
enum ByteFraming {
    Mpeg2,
    Vc1,
}

pub(super) struct ByteAssembler {
    video: Option<u32>,
    title_start: u64,
    framing: ByteFraming,
    carry: Vec<u8>,
    carry_pts: Option<i64>,
    carry_dts: Option<i64>,
    unit: Vec<u8>,
    pts: Option<i64>,
    dts: Option<i64>,
    started: bool,
}

impl ByteAssembler {
    fn new(video: Option<u32>, title_start: u64, framing: ByteFraming) -> Self {
        Self {
            video,
            title_start,
            framing,
            carry: Vec::new(),
            carry_pts: None,
            carry_dts: None,
            unit: Vec::new(),
            pts: None,
            dts: None,
            started: video.is_none(),
        }
    }

    fn feed(
        &mut self,
        writer: &mut MuxWriter<'_>,
        packet: Packet,
    ) -> Result<(), crate::error::DiscError> {
        self.carry.extend_from_slice(&packet.data);
        match self.framing {
            ByteFraming::Mpeg2 => {
                let starts = start_code_offsets(&self.carry, 0x00);
                if starts.len() >= 2 {
                    let end = starts[1];
                    let unit = self.carry[..end].to_vec();
                    self.carry.drain(..end);
                    let pts = self.carry_pts.take();
                    let dts = self.carry_dts.take();
                    self.carry_pts = packet.pts;
                    self.carry_dts = packet.dts;
                    self.flush_unit(writer, unit, pts, dts)?;
                }
            }
            ByteFraming::Vc1 => {
                let starts = start_code_offsets(&self.carry, 0x0D);
                if starts.len() >= 2 {
                    let first = starts[0];
                    let end = starts[1];
                    let unit = self.carry[first..end].to_vec();
                    self.carry.drain(..end);
                    let pts = self.carry_pts.take();
                    let dts = self.carry_dts.take();
                    self.carry_pts = packet.pts;
                    self.carry_dts = packet.dts;
                    self.flush_unit(writer, unit, pts, dts)?;
                } else if let Some(first) = starts.first() {
                    if *first > 0 {
                        self.carry.drain(..*first);
                    }
                }
            }
        }
        Ok(())
    }

    fn flush_unit(
        &mut self,
        writer: &mut MuxWriter<'_>,
        unit: Vec<u8>,
        pts: Option<i64>,
        dts: Option<i64>,
    ) -> Result<(), crate::error::DiscError> {
        if unit.is_empty() {
            return Ok(());
        }
        if !self.started {
            let is_i = match self.framing {
                ByteFraming::Mpeg2 => mpeg2_has_i_picture(&unit),
                ByteFraming::Vc1 => true,
            };
            if !(pts.is_some_and(|pts| pts >= self.title_start as i64) && is_i) {
                return Ok(());
            }
            self.started = true;
        }
        self.unit = unit;
        self.pts = pts;
        self.dts = dts;
        self.flush(writer)
    }

    fn flush(&mut self, writer: &mut MuxWriter<'_>) -> Result<(), crate::error::DiscError> {
        if self.unit.is_empty() {
            return Ok(());
        }
        let keyframe = match self.framing {
            ByteFraming::Mpeg2 => mpeg2_has_i_picture(&self.unit),
            ByteFraming::Vc1 => true,
        };
        let data = std::mem::take(&mut self.unit);
        writer.write(Packet {
            stream_index: self.video.unwrap_or(0),
            time_base: TimeBase::new(1, 90_000),
            pts: self.pts.take(),
            dts: self.dts.take(),
            duration: None,
            flags: PacketFlags {
                keyframe,
                ..Default::default()
            },
            data,
        })
    }

    fn finish(&mut self, writer: &mut MuxWriter<'_>) -> Result<(), crate::error::DiscError> {
        if self.carry.is_empty() {
            return self.flush(writer);
        }
        let start = match self.framing {
            ByteFraming::Mpeg2 => start_code_offsets(&self.carry, 0x00)
                .first()
                .copied()
                .unwrap_or(0),
            ByteFraming::Vc1 => start_code_offsets(&self.carry, 0x0D)
                .first()
                .copied()
                .unwrap_or(0),
        };
        let unit = self.carry[start..].to_vec();
        self.carry.clear();
        let pts = self.carry_pts.take();
        let dts = self.carry_dts.take();
        self.flush_unit(writer, unit, pts, dts)
    }
}

fn contains_start_code(data: &[u8], code: u8) -> bool {
    data.windows(4).any(|w| w == [0, 0, 1, code])
}

fn start_code_offsets(data: &[u8], code: u8) -> Vec<usize> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Some(rel) = find_start_code(&data[pos..]) {
        let start = pos + rel;
        if data.get(start + 3) == Some(&code) {
            out.push(start);
        }
        pos = start + 3;
    }
    out
}

pub(super) fn mpeg2_has_i_picture(data: &[u8]) -> bool {
    let mut pos = 0usize;
    while let Some(rel) = find_start_code(&data[pos..]) {
        let start = pos + rel;
        if data.get(start + 3) == Some(&0x00) && start + 5 < data.len() {
            return (data[start + 5] >> 3) & 0x07 == 1;
        }
        pos = start + 3;
    }
    false
}

fn split_annex_b(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Some(rel) = find_start_code(&data[pos..]) {
        let start = pos + rel + 3;
        let next = find_start_code(&data[start..])
            .map(|offset| start + offset)
            .unwrap_or(data.len());
        let mut end = next;
        while end > start && data[end - 1] == 0 {
            end -= 1;
        }
        if end > start {
            out.push(&data[start..end]);
        }
        pos = next;
    }
    out
}

/// Build an HEVCDecoderConfigurationRecord (hvcC) from one Annex-B AU.
pub(super) fn hevc_config_record(data: &[u8]) -> Vec<u8> {
    let nals = split_annex_b(data);
    let vps: Vec<&[u8]> = nals
        .iter()
        .copied()
        .filter(|n| !n.is_empty() && hevc_nal_type(n[0]) == 32)
        .collect();
    let sps: Vec<&[u8]> = nals
        .iter()
        .copied()
        .filter(|n| !n.is_empty() && hevc_nal_type(n[0]) == 33)
        .collect();
    let pps: Vec<&[u8]> = nals
        .iter()
        .copied()
        .filter(|n| !n.is_empty() && hevc_nal_type(n[0]) == 34)
        .collect();
    if sps.is_empty() {
        return Vec::new();
    }
    let profile = parse_hevc_sps(sps[0]).unwrap_or_default();
    let mut out = Vec::new();
    out.push(1);
    out.push((profile.profile_space << 6) | (profile.tier << 5) | profile.profile_idc);
    out.extend_from_slice(&profile.compatibility.to_be_bytes());
    let constraint = profile.constraint.to_be_bytes();
    out.extend_from_slice(&constraint[2..8]);
    out.push(profile.level);
    out.extend_from_slice(&(0xF000u16 | (profile.min_spatial_segmentation & 0x0FFF)).to_be_bytes());
    out.push(0xFC | (profile.parallelism & 0x03));
    out.push(0xFC | (profile.chroma_format & 0x03));
    out.push(0xF8 | (profile.bit_depth_luma & 0x07));
    out.push(0xF8 | (profile.bit_depth_chroma & 0x07));
    out.extend_from_slice(&0u16.to_be_bytes());
    out.push(((profile.temporal_layers & 0x07) << 3) | ((profile.temporal_nested & 1) << 2) | 3);
    let arrays = [
        (32u8, vps.as_slice()),
        (33, sps.as_slice()),
        (34, pps.as_slice()),
    ];
    let present = arrays.iter().filter(|(_, n)| !n.is_empty()).count() as u8;
    out.push(present);
    for (kind, list) in arrays {
        if list.is_empty() {
            continue;
        }
        out.push(0x80 | kind);
        out.extend_from_slice(&(list.len() as u16).to_be_bytes());
        for nal in list {
            out.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            out.extend_from_slice(nal);
        }
    }
    out
}

#[derive(Default)]
struct HevcProfile {
    profile_space: u8,
    tier: u8,
    profile_idc: u8,
    compatibility: u32,
    constraint: u64,
    level: u8,
    min_spatial_segmentation: u16,
    parallelism: u8,
    chroma_format: u8,
    bit_depth_luma: u8,
    bit_depth_chroma: u8,
    temporal_layers: u8,
    temporal_nested: u8,
}

fn parse_hevc_sps(nal: &[u8]) -> Option<HevcProfile> {
    if nal.len() < 3 {
        return None;
    }
    let rbsp = unescape_rbsp(&nal[2..]);
    let mut bits = BitReader::new(&rbsp);
    bits.skip(4);
    let max_sub_layers = bits.u(3)? as u8;
    let temporal_nested = bits.u(1)? as u8;
    let profile_space = bits.u(2)? as u8;
    let tier = bits.u(1)? as u8;
    let profile_idc = bits.u(5)? as u8;
    let compatibility = bits.u(32)? as u32;
    let constraint = bits.u(48)?;
    let level = bits.u(8)? as u8;
    for _ in 0..max_sub_layers {
        bits.skip(2);
    }
    let _sps_id = bits.ue()?;
    let chroma_format = bits.ue()? as u8;
    if chroma_format == 3 {
        bits.skip(1);
    }
    let _width = bits.ue()?;
    let _height = bits.ue()?;
    if bits.u(1)? != 0 {
        for _ in 0..4 {
            bits.ue()?;
        }
    }
    let bit_depth_luma = bits.ue()? as u8;
    let bit_depth_chroma = bits.ue()? as u8;
    Some(HevcProfile {
        profile_space,
        tier,
        profile_idc,
        compatibility,
        constraint,
        level,
        min_spatial_segmentation: 0,
        parallelism: 0,
        chroma_format,
        bit_depth_luma,
        bit_depth_chroma,
        temporal_layers: max_sub_layers.saturating_add(1),
        temporal_nested,
    })
}

fn unescape_rbsp(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0usize;
    for &byte in data {
        if zeros >= 2 && byte == 0x03 {
            zeros = 0;
            continue;
        }
        out.push(byte);
        if byte == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
    }
    out
}

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn u(&mut self, count: usize) -> Option<u64> {
        let mut value = 0u64;
        for _ in 0..count {
            let byte = *self.data.get(self.pos / 8)?;
            value = (value << 1) | u64::from((byte >> (7 - (self.pos % 8))) & 1);
            self.pos += 1;
        }
        Some(value)
    }

    fn skip(&mut self, count: usize) {
        self.pos += count;
    }

    fn ue(&mut self) -> Option<u64> {
        let mut zeros = 0;
        while self.u(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let mut value = 1u64;
        for _ in 0..zeros {
            value = (value << 1) | self.u(1)?;
        }
        Some(value - 1)
    }
}

/// Build a Matroska `V_MS/VFW/FOURCC` CodecPrivate for VC-1: a
/// BITMAPINFOHEADER with `WVC1` plus the sequence/entry-point headers.
pub(super) fn vc1_codec_private(data: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(b"WVC1");
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for nal in split_annex_b(data) {
        if !nal.is_empty() && (nal[0] == 0x0F || nal[0] == 0x0E) {
            out.extend_from_slice(&[0, 0, 1]);
            out.extend_from_slice(nal);
        }
    }
    out
}

fn hevc_nal_type(byte: u8) -> u8 {
    (byte >> 1) & 0x3F
}

fn contains_nal_type_range(data: &[u8], low: u8, high: u8) -> bool {
    let mut pos = 0usize;
    while let Some(rel) = find_start_code(&data[pos..]) {
        let start = pos + rel + 3;
        if start < data.len() {
            let kind = hevc_nal_type(data[start]);
            if (low..=high).contains(&kind) {
                return true;
            }
        }
        pos = start;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mpeg2_picture_framing_and_i_detection() {
        // Sequence header, then I-picture (temporal_reference 0, type 1),
        // then P-picture (type 2).
        let mut data = vec![0, 0, 1, 0xB3, 0x2D, 0x01, 0xE0];
        let mut i = vec![0, 0, 1, 0x00, 0x00, 0x08, 0xAA];
        i.extend_from_slice(&[0x00, 0x00, 0x01, 0x01, 0xBB]);
        data.extend_from_slice(&i);
        assert!(mpeg2_has_i_picture(&data));
        let mut p = vec![0, 0, 1, 0x00, 0x00, 0x10, 0xBB];
        p.extend_from_slice(&[0x00, 0x00, 0x01, 0x01, 0xCC]);
        assert!(!mpeg2_has_i_picture(&p));
    }

    #[test]
    fn hevc_profile_and_parameter_sets_produce_hvcc() {
        // libx265, 320x240 black, Main profile / Level 2.
        let vps = hex("40010c01ffff0408000003009fa800000300003cba0240");
        let sps =
            hex("4201010408000003009fa800000300003ca00a080f165ba924caf0168080000003008000000c84");
        let pps = hex("4401c172b06240");
        let mut au = Vec::new();
        for nal in [&vps, &sps, &pps] {
            au.extend_from_slice(&[0, 0, 0, 1]);
            au.extend_from_slice(nal);
        }
        let hvcc = hevc_config_record(&au);
        assert_eq!(hvcc[0], 1);
        assert_eq!(hvcc[1], 4, "Main Intra profile");
        assert_eq!(hvcc[12], 60, "HEVC level 2");
        assert_eq!(hvcc[22], 3, "VPS + SPS + PPS arrays");
        let mut pos = 23usize;
        for expected in [32u8, 33, 34] {
            assert_eq!(hvcc[pos] & 0x3F, expected);
            let count = u16::from_be_bytes([hvcc[pos + 1], hvcc[pos + 2]]) as usize;
            assert!(count >= 1);
            pos += 3;
            for _ in 0..count {
                let len = u16::from_be_bytes([hvcc[pos], hvcc[pos + 1]]) as usize;
                pos += 2 + len;
            }
        }
        assert_eq!(pos, hvcc.len());
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn vc1_private_has_bitmapinfoheader_and_sequence() {
        let data = [0, 0, 1, 0x0F, 0x01, 0x02, 0, 0, 1, 0x0D, 0xAA];
        let cp = vc1_codec_private(&data, 1920, 1080);
        assert_eq!(&cp[0..4], &40u32.to_le_bytes());
        assert_eq!(&cp[16..20], b"WVC1");
        assert_eq!(&cp[40..], &[0, 0, 1, 0x0F, 0x01, 0x02]);
    }
}

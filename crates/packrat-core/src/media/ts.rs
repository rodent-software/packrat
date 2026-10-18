//! Packrat-owned MPEG-TS demuxer for Blu-ray M2TS payloads.
//!
//! The reader has already stripped BDAV TP_extra headers, so this consumes
//! standard 188-byte transport packets. It scans PAT/PMT, reassembles PES per
//! PID, and emits one packet per PES with 90 kHz timestamps.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read};

use crate::media::core::{
    CodecId, CodecParameters, Error as MediaError, Packet, ReadSeek, StreamInfo, TimeBase,
};

use super::PacketFlags;

const TS_LEN: usize = 188;
const TS_SYNC: u8 = 0x47;
const MAX_PES_BYTES: usize = 32 * 1024 * 1024;

pub(crate) struct TsDemuxer {
    reader: Box<dyn ReadSeek>,
    streams: Vec<StreamInfo>,
    pes: HashMap<u16, PesBuffer>,
    pending: VecDeque<Packet>,
    pmt_pid: Option<u16>,
    eof: bool,
}

struct PesBuffer {
    stream_index: u32,
    bytes: Vec<u8>,
    random_access: bool,
}

impl TsDemuxer {
    pub(crate) fn open(mut reader: Box<dyn ReadSeek>) -> Result<Self, MediaError> {
        let mut streams = Vec::new();
        let mut pes = HashMap::new();
        let mut pmt_pid = None;
        let mut pat_seen = false;
        let mut pmt_seen = false;

        let mut packet = [0u8; TS_LEN];
        while !(pat_seen && pmt_seen) {
            if !read_packet(&mut reader, &mut packet)? {
                return Err(MediaError::invalid(
                    "MPEG-TS ended before PAT/PMT were found",
                ));
            }
            let Some(ts) = TsPacket::parse(&packet) else {
                continue;
            };
            if ts.pid == 0 && ts.payload_unit_start {
                if let Some(program_pid) = parse_pat(ts.payload) {
                    pmt_pid = Some(program_pid);
                    pat_seen = true;
                }
            }
            if Some(ts.pid) == pmt_pid && ts.payload_unit_start {
                let declarations = parse_pmt(ts.payload)?;
                if std::env::var_os("PACKRAT_TS_DEBUG").is_some() {
                    eprintln!(
                        "packrat: PMT pid {:#x}: {:?}",
                        ts.pid,
                        declarations
                            .iter()
                            .map(|d| (d.pid, d.stream_type))
                            .collect::<Vec<_>>()
                    );
                }
                for declaration in declarations {
                    let Some((codec_id, media_type)) = codec_for(declaration.stream_type) else {
                        continue;
                    };
                    let index = streams.len() as u32;
                    let params = match media_type {
                        MediaKind::Video => CodecParameters::video(CodecId::new(codec_id)),
                        MediaKind::Audio => CodecParameters::audio(CodecId::new(codec_id)),
                        MediaKind::Subtitle => CodecParameters::subtitle(CodecId::new(codec_id)),
                    };
                    streams.push(StreamInfo {
                        index,
                        time_base: TimeBase::new(1, 90_000),
                        duration: None,
                        start_time: None,
                        params,
                    });
                    pes.insert(
                        declaration.pid,
                        PesBuffer {
                            stream_index: index,
                            bytes: Vec::new(),
                            random_access: false,
                        },
                    );
                }
                pmt_seen = true;
            }
        }
        if streams.is_empty() {
            return Err(MediaError::invalid(
                "MPEG-TS PMT advertised no streams packrat recognises",
            ));
        }
        Ok(Self {
            reader,
            streams,
            pes,
            pending: VecDeque::new(),
            pmt_pid,
            eof: false,
        })
    }

    pub(crate) fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    /// Next PES packet, or `None` at end of stream.
    pub(crate) fn next_packet(&mut self) -> Result<Option<Packet>, MediaError> {
        loop {
            if let Some(packet) = self.pending.pop_front() {
                return Ok(Some(packet));
            }
            if self.eof {
                self.flush_all();
                if let Some(packet) = self.pending.pop_front() {
                    return Ok(Some(packet));
                }
                return Ok(None);
            }
            let mut packet = [0u8; TS_LEN];
            if !read_packet(&mut self.reader, &mut packet)? {
                self.eof = true;
                continue;
            }
            self.feed(&packet)?;
        }
    }

    fn feed(&mut self, bytes: &[u8]) -> Result<(), MediaError> {
        let Some(ts) = TsPacket::parse(bytes) else {
            return Ok(());
        };
        if Some(ts.pid) == self.pmt_pid || ts.pid == 0 {
            return Ok(());
        }
        let Some(buffer) = self.pes.get_mut(&ts.pid) else {
            return Ok(());
        };
        if ts.payload_unit_start {
            if !buffer.bytes.is_empty() {
                let old = std::mem::take(&mut buffer.bytes);
                let random_access = buffer.random_access;
                let index = buffer.stream_index;
                self.pending
                    .push_back(parse_pes(index, &old, random_access)?);
            }
            buffer.random_access = ts.random_access;
        }
        if !ts.payload.is_empty() {
            if buffer.bytes.len() + ts.payload.len() > MAX_PES_BYTES {
                buffer.bytes.clear();
                return Err(MediaError::invalid("MPEG-TS PES exceeds size limit"));
            }
            buffer.bytes.extend_from_slice(ts.payload);
        }
        Ok(())
    }

    fn flush_all(&mut self) {
        let buffers: Vec<(u16, PesBuffer)> = self.pes.drain().collect();
        for (pid, buffer) in buffers {
            if buffer.bytes.is_empty() {
                continue;
            }
            if let Ok(packet) = parse_pes(buffer.stream_index, &buffer.bytes, buffer.random_access)
            {
                self.pending.push_back(packet);
            }
            self.pes.insert(
                pid,
                PesBuffer {
                    stream_index: buffer.stream_index,
                    bytes: Vec::new(),
                    random_access: false,
                },
            );
        }
    }
}

fn read_packet(reader: &mut Box<dyn ReadSeek>, out: &mut [u8; TS_LEN]) -> Result<bool, MediaError> {
    let mut filled = 0;
    while filled < TS_LEN {
        match reader.read(&mut out[filled..]) {
            Ok(0) => return Ok(false),
            Ok(n) => filled += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(MediaError::Io(error)),
        }
    }
    // Resynchronise on a damaged/short boundary rather than feeding garbage
    // downstream. Blu-ray clip boundaries can leave a partial packet.
    if out[0] != TS_SYNC {
        for _ in 0..TS_LEN * 4 {
            if reader.read(&mut out[1..]).map_err(MediaError::Io)? == 0 {
                return Ok(false);
            }
            if out[1] == TS_SYNC {
                out.copy_within(1.., 0);
                if reader
                    .read(&mut out[TS_LEN - 1..])
                    .map_err(MediaError::Io)?
                    == 0
                {
                    return Ok(false);
                }
                break;
            }
            out.copy_within(1.., 0);
        }
    }
    Ok(true)
}

struct TsPacket<'a> {
    pid: u16,
    payload_unit_start: bool,
    random_access: bool,
    payload: &'a [u8],
}

impl<'a> TsPacket<'a> {
    fn parse(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < 4 || bytes[0] != TS_SYNC {
            return None;
        }
        let pid = (u16::from(bytes[1] & 0x1F) << 8) | u16::from(bytes[2]);
        let payload_unit_start = bytes[1] & 0x40 != 0;
        let adaptation = (bytes[3] >> 4) & 0b10 != 0;
        let payload = (bytes[3] >> 4) & 1 != 0;
        let mut offset = 4;
        let mut random_access = false;
        if adaptation {
            let length = usize::from(*bytes.get(4)?);
            random_access = bytes.get(5).is_some_and(|flags| flags & 0x40 != 0);
            offset = 5 + length;
        }
        let payload = if payload && offset <= bytes.len() {
            &bytes[offset..]
        } else {
            &[]
        };
        Some(Self {
            pid,
            payload_unit_start,
            random_access,
            payload,
        })
    }
}

struct Declaration {
    pid: u16,
    stream_type: u8,
}

fn section<'a>(payload: &'a [u8]) -> Option<&'a [u8]> {
    let pointer = usize::from(*payload.first()?);
    let start = 1 + pointer;
    let section = payload.get(start..)?;
    if section.len() < 3 {
        return None;
    }
    let length = (usize::from(section[1] & 0x0F) << 8) | usize::from(section[2]);
    section.get(..3 + length)
}

fn parse_pat(payload: &[u8]) -> Option<u16> {
    let section = section(payload)?;
    if section[0] != 0x00 {
        return None;
    }
    let end = section.len().checked_sub(4)?;
    let mut offset = 8;
    while offset + 4 <= end {
        let program = u16::from_be_bytes([section[offset], section[offset + 1]]);
        let pid = (u16::from(section[offset + 2] & 0x1F) << 8) | u16::from(section[offset + 3]);
        if program != 0 {
            return Some(pid);
        }
        offset += 4;
    }
    None
}

fn parse_pmt(payload: &[u8]) -> Result<Vec<Declaration>, MediaError> {
    let section = section(payload).ok_or_else(|| MediaError::invalid("truncated PMT section"))?;
    if std::env::var_os("PACKRAT_TS_DEBUG").is_some() {
        eprintln!("packrat: PMT section: {:02x?}", section);
    }
    if section[0] != 0x02 {
        return Err(MediaError::invalid("PMT table id is not 0x02"));
    }
    if section.len() < 12 {
        return Err(MediaError::invalid("PMT shorter than its header"));
    }
    let program_info_len = usize::from(u16::from_be_bytes([section[10], section[11]]) & 0x0FFF);
    let end = section
        .len()
        .checked_sub(4)
        .ok_or_else(|| MediaError::invalid("PMT shorter than its CRC"))?;
    let mut offset = 12 + program_info_len;
    let mut out = Vec::new();
    while offset + 5 <= end {
        let stream_type = section[offset];
        let pid = (u16::from(section[offset + 1] & 0x1F) << 8) | u16::from(section[offset + 2]);
        let info_len =
            usize::from(u16::from_be_bytes([section[offset + 3], section[offset + 4]]) & 0x0FFF);
        out.push(Declaration { pid, stream_type });
        offset += 5 + info_len;
    }
    Ok(out)
}

enum MediaKind {
    Video,
    Audio,
    Subtitle,
}

fn codec_for(stream_type: u8) -> Option<(&'static str, MediaKind)> {
    match stream_type {
        0x02 => Some(("mpeg2video", MediaKind::Video)),
        0x1B => Some(("h264", MediaKind::Video)),
        0x24 => Some(("h265", MediaKind::Video)),
        0xEA => Some(("vc1", MediaKind::Video)),
        0x80 => Some(("pcm_s16be", MediaKind::Audio)),
        0x81 => Some(("ac3", MediaKind::Audio)),
        0x82 | 0x86 => Some(("dts", MediaKind::Audio)),
        0x83 => Some(("truehd", MediaKind::Audio)),
        0x84 | 0x87 => Some(("eac3", MediaKind::Audio)),
        0x90 => Some(("hdmv_pgs_subtitle", MediaKind::Subtitle)),
        0x92 => Some(("hdmv_text_subtitle", MediaKind::Subtitle)),
        _ => None,
    }
}

fn parse_pes(stream_index: u32, bytes: &[u8], random_access: bool) -> Result<Packet, MediaError> {
    if bytes.len() < 6 || bytes[0..3] != [0, 0, 1] {
        return Err(MediaError::invalid("PES start code missing"));
    }
    let stream_id = bytes[3];
    let declared = u16::from_be_bytes([bytes[4], bytes[5]]) as usize;
    let bytes = if declared == 0 {
        bytes
    } else {
        let end = (6 + declared).min(bytes.len());
        &bytes[..end]
    };
    let (pts, dts, payload_start) = if stream_id == 0xBC
        || stream_id == 0xBE
        || stream_id == 0xBF
        || stream_id == 0xF0
        || stream_id == 0xF1
        || stream_id == 0xF2
        || stream_id == 0xF8
        || stream_id == 0xFF
    {
        (None, None, 6)
    } else {
        if bytes.len() < 9 {
            return Err(MediaError::invalid("PES optional header is truncated"));
        }
        let flags = bytes[7];
        let header_len = usize::from(bytes[8]);
        let start = 9usize
            .checked_add(header_len)
            .ok_or_else(|| MediaError::invalid("PES header length overflow"))?;
        if start > bytes.len() {
            return Err(MediaError::invalid("PES optional header overruns packet"));
        }
        let pts = match (flags >> 6) & 3 {
            2 | 3 => decode_timestamp(
                bytes
                    .get(9..14)
                    .ok_or_else(|| MediaError::invalid("PES PTS is truncated"))?,
            )?,
            _ => None,
        };
        let dts = match (flags >> 6) & 3 {
            3 => decode_timestamp(
                bytes
                    .get(14..19)
                    .ok_or_else(|| MediaError::invalid("PES DTS is truncated"))?,
            )?,
            _ => None,
        };
        (pts, dts, start)
    };
    let data = bytes[payload_start..].to_vec();
    Ok(Packet {
        stream_index,
        time_base: TimeBase::new(1, 90_000),
        pts: pts.map(|value| value as i64),
        dts: dts.map(|value| value as i64),
        duration: None,
        flags: PacketFlags {
            keyframe: random_access,
            ..Default::default()
        },
        data,
    })
}

fn decode_timestamp(bytes: &[u8]) -> Result<Option<u64>, MediaError> {
    if bytes.len() < 5 {
        return Ok(None);
    }
    let value = (u64::from(bytes[0] & 0x0E) << 29)
        | (u64::from(bytes[1]) << 22)
        | (u64::from(bytes[2] & 0xFE) << 14)
        | (u64::from(bytes[3]) << 7)
        | (u64::from(bytes[4]) >> 1);
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn ts_packet(pid: u16, pusi: bool, payload: &[u8]) -> [u8; TS_LEN] {
        let mut packet = [0xFFu8; TS_LEN];
        packet[0] = TS_SYNC;
        packet[1] = ((pid >> 8) as u8 & 0x1F) | if pusi { 0x40 } else { 0 };
        packet[2] = pid as u8;
        packet[3] = 0x10;
        packet[4..4 + payload.len()].copy_from_slice(payload);
        packet
    }

    fn pat(program: u16, pmt_pid: u16) -> Vec<u8> {
        let mut section = vec![0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00];
        section.extend_from_slice(&program.to_be_bytes());
        section.extend_from_slice(&(0xE000 | pmt_pid).to_be_bytes());
        section.extend_from_slice(&[0, 0, 0, 0]);
        let mut payload = vec![0];
        payload.extend(section);
        payload
    }

    fn pmt(pid: u16, stream_type: u8) -> Vec<u8> {
        let mut section = vec![0x02, 0xB0, 0x12, 0x00, 0x01, 0xC1, 0x00, 0x00];
        section.extend_from_slice(&(0xE000 | pid).to_be_bytes());
        section.extend_from_slice(&0xF000u16.to_be_bytes());
        section.push(stream_type);
        section.extend_from_slice(&(0xE000 | pid).to_be_bytes());
        section.extend_from_slice(&0xF000u16.to_be_bytes());
        section.extend_from_slice(&[0, 0, 0, 0]);
        let mut payload = vec![0];
        payload.extend(section);
        payload
    }

    #[test]
    fn finds_pat_pmt_and_pes() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&ts_packet(0, true, &pat(1, 0x100)));
        bytes.extend_from_slice(&ts_packet(0x100, true, &pmt(0x101, 0x1B)));
        let pes = [
            0, 0, 1, 0xE0, 0, 10, 0x80, 0x80, 5, 0x21, 0, 1, 0, 1, 0xAA, 0xBB,
        ];
        let mut payload = pes.to_vec();
        payload.resize(184, 0xFF);
        bytes.extend_from_slice(&ts_packet(0x101, true, &payload));
        let mut demux = TsDemuxer::open(Box::new(Cursor::new(bytes))).unwrap();
        assert_eq!(demux.streams().len(), 1);
        let packet = demux.next_packet().unwrap().unwrap();
        assert_eq!(packet.data, [0xAA, 0xBB]);
        assert!(packet.pts.is_some());
        assert!(demux.next_packet().unwrap().is_none());
    }

    #[test]
    fn rejects_non_ts_input() {
        let bytes = vec![0u8; TS_LEN];
        assert!(TsDemuxer::open(Box::new(Cursor::new(bytes))).is_err());
    }
}

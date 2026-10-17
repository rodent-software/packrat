//! Minimal packrat-owned Blu-ray playlist parser.
//!
//! Only the fields packrat needs are decoded: PlayItems, their IN/OUT times
//! and clip names, the primary video format, primary audio/PG languages, and
//! chapter marks. Unknown bytes are skipped by their length envelopes, so
//! authoring tools can add fields without breaking a rip.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseError {}

type Result<T> = std::result::Result<T, ParseError>;

fn malformed(message: impl Into<String>) -> ParseError {
    ParseError(message.into())
}

#[derive(Debug, Clone)]
pub struct Playlist {
    pub play_items: Vec<PlayItem>,
    pub marks: Vec<Mark>,
}

#[derive(Debug, Clone)]
pub struct PlayItem {
    pub clip_id: String,
    pub in_time_ticks: u32,
    pub out_time_ticks: u32,
    pub angles: u8,
    pub streams: StreamTable,
}

#[derive(Debug, Clone, Default)]
pub struct StreamTable {
    pub primary_video: Vec<VideoStream>,
    pub primary_audio: Vec<AudioStream>,
    pub pg_subtitles: Vec<SubtitleStream>,
}

#[derive(Debug, Clone)]
pub struct VideoStream {
    pub video_format: u8,
}

#[derive(Debug, Clone)]
pub struct AudioStream {
    pub pid: u16,
    pub language: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SubtitleStream {
    pub pid: u16,
    pub language: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct Mark {
    pub kind: u8,
    pub ref_play_item_id: u16,
    pub mark_time_ticks: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChapterSpan {
    pub index: usize,
    pub start_pts_90k: u64,
    pub end_pts_90k: u64,
}

impl PlayItem {
    pub fn duration_90k(&self) -> u64 {
        u64::from(self.out_time_ticks.saturating_sub(self.in_time_ticks)) * 2
    }
}

impl Playlist {
    pub fn duration_90k(&self) -> u64 {
        self.play_items.iter().map(PlayItem::duration_90k).sum()
    }

    /// Title-relative start PTS of the first PlayItem.
    pub fn start_pts_90k(&self) -> u64 {
        self.play_items
            .first()
            .map(|item| u64::from(item.in_time_ticks) * 2)
            .unwrap_or(0)
    }

    /// Primary video format nibble from the first PlayItem that declares one.
    pub fn primary_video_format(&self) -> Option<u8> {
        self.play_items
            .iter()
            .find_map(|item| item.streams.primary_video.first())
            .map(|stream| stream.video_format)
    }

    /// Audio languages in STN class order, de-duplicated by PID.
    pub fn audio_languages(&self) -> Vec<Option<String>> {
        collected_languages(self, |item| &item.streams.primary_audio)
    }

    /// PG subtitle languages in STN class order, de-duplicated by PID.
    pub fn pg_languages(&self) -> Vec<Option<String>> {
        collected_languages(self, |item| &item.streams.pg_subtitles)
    }

    /// Chapter spans on the title timeline. Entry marks are clip-local 45 kHz
    /// timestamps; this lifts them onto the concatenated title timeline.
    pub fn chapter_spans(&self) -> Vec<ChapterSpan> {
        let mut item_start = Vec::with_capacity(self.play_items.len());
        let mut total = 0u64;
        for item in &self.play_items {
            item_start.push(total);
            total += item.duration_90k();
        }

        let mut starts = Vec::new();
        for mark in &self.marks {
            if mark.kind != 1 {
                continue;
            }
            let Some(item) = self.play_items.get(usize::from(mark.ref_play_item_id)) else {
                continue;
            };
            let mark_90k = u64::from(mark.mark_time_ticks) * 2;
            let in_90k = u64::from(item.in_time_ticks) * 2;
            if mark_90k < in_90k {
                continue;
            }
            starts.push(item_start[usize::from(mark.ref_play_item_id)] + (mark_90k - in_90k));
        }
        starts.sort_unstable();
        starts.dedup();

        let mut out = Vec::with_capacity(starts.len());
        for (index, start) in starts.iter().enumerate() {
            let end = starts.get(index + 1).copied().unwrap_or(total).max(*start);
            out.push(ChapterSpan {
                index,
                start_pts_90k: *start,
                end_pts_90k: end,
            });
        }
        out
    }
}

fn collected_languages<T, F>(playlist: &Playlist, streams: F) -> Vec<Option<String>>
where
    F: Fn(&PlayItem) -> &[T],
    T: LanguageStream,
{
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for item in &playlist.play_items {
        for stream in streams(item) {
            if stream.pid() == 0 || !seen.insert(stream.pid()) {
                continue;
            }
            out.push(stream.language().map(str::to_owned));
        }
    }
    out
}

trait LanguageStream {
    fn pid(&self) -> u16;
    fn language(&self) -> Option<&str>;
}

impl LanguageStream for AudioStream {
    fn pid(&self) -> u16 {
        self.pid
    }

    fn language(&self) -> Option<&str> {
        self.language.as_deref()
    }
}

impl LanguageStream for SubtitleStream {
    fn pid(&self) -> u16 {
        self.pid
    }

    fn language(&self) -> Option<&str> {
        self.language.as_deref()
    }
}

pub fn parse(buf: &[u8]) -> Result<Playlist> {
    if buf.len() < 40 || &buf[..4] != b"MPLS" {
        return Err(malformed("not an MPLS playlist"));
    }
    let playlist_start = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]) as usize;
    let marks_start = u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]) as usize;

    let mut r = Reader::new(buf);
    r.seek(playlist_start)?;
    let playlist_len = r.read_u32()? as usize;
    let body_start = r.pos;
    let body_end = body_start
        .checked_add(playlist_len)
        .ok_or_else(|| malformed("playlist length overflow"))?;
    if body_end > buf.len() {
        return Err(malformed("playlist body overruns file"));
    }
    r.skip(2)?;
    let item_count = r.read_u16()? as usize;
    let _sub_path_count = r.read_u16()? as usize;

    let mut play_items = Vec::with_capacity(item_count);
    for _ in 0..item_count {
        play_items.push(parse_play_item(&mut r)?);
    }
    r.seek(body_end)?;

    let mut marks = Vec::new();
    if marks_start > 0 && marks_start < buf.len() {
        r.seek(marks_start)?;
        let _len = r.read_u32()?;
        let count = r.read_u16()? as usize;
        for _ in 0..count {
            r.skip(1)?;
            marks.push(Mark {
                kind: r.read_u8()?,
                ref_play_item_id: r.read_u16()?,
                mark_time_ticks: r.read_u32()?,
            });
            r.skip(6)?;
        }
    }

    Ok(Playlist { play_items, marks })
}

fn parse_play_item(r: &mut Reader<'_>) -> Result<PlayItem> {
    let len = r.read_u16()? as usize;
    let body_end = r
        .pos
        .checked_add(len)
        .ok_or_else(|| malformed("PlayItem length overflow"))?;
    let clip_id = std::str::from_utf8(r.slice(5)?)
        .map_err(|_| malformed("PlayItem clip name is not ASCII"))?
        .to_string();
    r.skip(4)?; // clip codec identifier
    let flags = r.read_u16()?;
    r.skip(1)?; // STC id
    let in_time_ticks = r.read_u32()?;
    let out_time_ticks = r.read_u32()?;
    r.skip(8)?; // UO mask
    r.skip(1)?; // random-access flag
    r.skip(1)?; // still mode
    r.skip(2)?; // still time

    let multi_angle = (flags >> 4) & 1 != 0;
    let angles = if multi_angle {
        let count = r.read_u8()?;
        r.skip(1)?;
        for _ in 1..count {
            r.skip(11)?;
        }
        count
    } else {
        1
    };

    let stn_len = r.read_u16()? as usize;
    let stn_start = r.pos;
    let stn_end = stn_start
        .checked_add(stn_len)
        .ok_or_else(|| malformed("STN_table length overflow"))?;
    let streams = if stn_len >= 14 {
        r.skip(2)?;
        let video_count = r.read_u8()?;
        let audio_count = r.read_u8()?;
        let pg_count = r.read_u8()?;
        r.skip(4)?;
        r.skip(5)?;
        let mut streams = StreamTable::default();
        for _ in 0..video_count {
            streams.primary_video.push(parse_video_stream(r)?);
        }
        for _ in 0..audio_count {
            streams.primary_audio.push(parse_audio_stream(r)?);
        }
        for _ in 0..pg_count {
            streams.pg_subtitles.push(parse_pg_stream(r)?);
        }
        streams
    } else {
        StreamTable::default()
    };
    r.seek(stn_end)?;
    r.seek(body_end)?;

    Ok(PlayItem {
        clip_id,
        in_time_ticks,
        out_time_ticks,
        angles,
        streams,
    })
}

fn parse_video_stream(r: &mut Reader<'_>) -> Result<VideoStream> {
    let pid = parse_stream_entry(r)?;
    let len = r.read_u8()? as usize;
    let start = r.pos;
    let end = start + len;
    if end > r.buf.len() {
        return Err(malformed("video attributes overrun"));
    }
    r.skip(1)?; // coding type
    let format = if r.pos < end { r.read_u8()? >> 4 } else { 0 };
    r.seek(end)?;
    let _ = pid;
    Ok(VideoStream {
        video_format: format,
    })
}

fn parse_audio_stream(r: &mut Reader<'_>) -> Result<AudioStream> {
    let pid = parse_stream_entry(r)?;
    let len = r.read_u8()? as usize;
    let start = r.pos;
    let end = start + len;
    if end > r.buf.len() {
        return Err(malformed("audio attributes overrun"));
    }
    r.skip(2)?; // coding type + audio format/sample rate
    let language = if r.pos + 3 <= end {
        decode_language(r.slice(3)?)
    } else {
        None
    };
    r.seek(end)?;
    Ok(AudioStream { pid, language })
}

fn parse_pg_stream(r: &mut Reader<'_>) -> Result<SubtitleStream> {
    let pid = parse_stream_entry(r)?;
    let len = r.read_u8()? as usize;
    let start = r.pos;
    let end = start + len;
    if end > r.buf.len() {
        return Err(malformed("PG attributes overrun"));
    }
    r.skip(1)?; // coding type
    let language = if r.pos + 3 <= end {
        decode_language(r.slice(3)?)
    } else {
        None
    };
    r.seek(end)?;
    Ok(SubtitleStream { pid, language })
}

fn parse_stream_entry(r: &mut Reader<'_>) -> Result<u16> {
    let len = r.read_u8()? as usize;
    let start = r.pos;
    let end = start + len;
    if end > r.buf.len() {
        return Err(malformed("stream entry overruns file"));
    }
    let pid = if len >= 3 && r.read_u8()? == 1 {
        r.read_u16()?
    } else {
        0
    };
    r.seek(end)?;
    Ok(pid)
}

fn decode_language(raw: &[u8]) -> Option<String> {
    if raw == [0, 0, 0] || !raw.iter().all(u8::is_ascii_graphic) {
        return None;
    }
    Some(
        raw.iter()
            .map(|byte| char::from(*byte).to_ascii_lowercase())
            .collect(),
    )
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn seek(&mut self, pos: usize) -> Result<()> {
        if pos > self.buf.len() {
            return Err(malformed("seek past end of playlist"));
        }
        self.pos = pos;
        Ok(())
    }

    fn skip(&mut self, len: usize) -> Result<()> {
        self.seek(self.pos.saturating_add(len))
    }

    fn slice(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or_else(|| malformed("playlist slice overflow"))?;
        let out = self
            .buf
            .get(self.pos..end)
            .ok_or_else(|| malformed("playlist slice past end"))?;
        self.pos = end;
        Ok(out)
    }

    fn read_u8(&mut self) -> Result<u8> {
        Ok(self.slice(1)?[0])
    }

    fn read_u16(&mut self) -> Result<u16> {
        let b = self.slice(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn read_u32(&mut self) -> Result<u32> {
        let b = self.slice(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playlist(items: &[(&str, u32, u32, u64)], marks: &[(u8, u16, u32)]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"MPLS");
        out.extend_from_slice(b"0200");
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&[0; 20]);
        out.extend_from_slice(&14u32.to_be_bytes());
        out.extend_from_slice(&[0; 14]);

        let playlist_start = out.len() as u32;
        out.extend_from_slice(&[0; 4]);
        let body_start = out.len();
        out.extend_from_slice(&[0; 2]);
        out.extend_from_slice(&(items.len() as u16).to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        for (clip, in_time, out_time, _duration) in items {
            let mut body = Vec::new();
            body.extend_from_slice(clip.as_bytes());
            body.extend_from_slice(b"M2TS");
            body.extend_from_slice(&0u16.to_be_bytes());
            body.push(0);
            body.extend_from_slice(&in_time.to_be_bytes());
            body.extend_from_slice(&out_time.to_be_bytes());
            body.extend_from_slice(&[0; 8]);
            body.extend_from_slice(&[0; 4]);
            body.extend_from_slice(&0u16.to_be_bytes());
            body.extend_from_slice(&0u16.to_be_bytes());
            out.extend_from_slice(&(body.len() as u16).to_be_bytes());
            out.extend_from_slice(&body);
        }
        let body_len = (out.len() - body_start) as u32;
        out[playlist_start as usize..playlist_start as usize + 4]
            .copy_from_slice(&body_len.to_be_bytes());

        let marks_start = out.len() as u32;
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&(marks.len() as u16).to_be_bytes());
        for (kind, ref_id, time) in marks {
            out.push(0);
            out.push(*kind);
            out.extend_from_slice(&ref_id.to_be_bytes());
            out.extend_from_slice(&time.to_be_bytes());
            out.extend_from_slice(&[0; 6]);
        }
        let marks_len = (out.len() - marks_start as usize - 4) as u32;
        out[marks_start as usize..marks_start as usize + 4]
            .copy_from_slice(&marks_len.to_be_bytes());
        out[8..12].copy_from_slice(&playlist_start.to_be_bytes());
        out[12..16].copy_from_slice(&marks_start.to_be_bytes());
        out
    }

    #[test]
    fn parses_play_items_and_chapters() {
        let bytes = playlist(
            &[("00001", 0, 90_000, 0), ("00002", 0, 45_000, 0)],
            &[(1, 0, 0), (1, 1, 0)],
        );
        let parsed = parse(&bytes).expect("parse");
        assert_eq!(parsed.play_items.len(), 2);
        assert_eq!(parsed.duration_90k(), 270_000);
        assert_eq!(parsed.primary_video_format(), None);
        let chapters = parsed.chapter_spans();
        assert_eq!(chapters.len(), 2);
        assert_eq!(chapters[0].start_pts_90k, 0);
        assert_eq!(chapters[1].start_pts_90k, 180_000);
        assert_eq!(chapters[1].end_pts_90k, 270_000);
    }

    #[test]
    fn rejects_non_mpls_input() {
        assert!(parse(b"not an mpls").is_err());
    }
}

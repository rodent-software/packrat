//! Blu-ray structure and title remuxing.
//!
//! Blu-ray metadata (`BDMV/`, playlists, clip info) is not encrypted; the
//! `.m2ts` streams are. The metadata is read through `oxideav-bluray`, the
//! streams are decrypted through the user-supplied `KEYDB.cfg`, and the clean
//! 188-byte MPEG-TS that comes out is demuxed and written to Matroska.

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::media::{
    avc_config_record, MatroskaMuxer, Packet, PacketFlags, StreamInfo, TimeBase, TsDemuxer,
    WriteSeek,
};
use oxideav_bluray::bdmv::mpls::PlayListMpls;
use oxideav_bluray::{Disc, TitleInfo, TitleKind, TitleSource, TrackKind, VideoFormat};

use crate::disc::{DiscModel, Title};
use crate::error::DiscError;
use crate::remux::{
    partial_path, replace_file, PartialFile, RemuxPhase, RemuxProgress, RemuxReport,
};
use crate::source::DiscSource;

/// Read a mounted Blu-ray's playlists into the same model the DVD path uses.
///
/// The index only names a few entry points; the real content is the playlist
/// directory. Enumerating it gives packrat the main feature and every extra,
/// including playlists a BD-J menu reaches without naming them in `index.bdmv`.
pub fn read_disc(root: &Path) -> Result<DiscModel, DiscError> {
    let volume_id = root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let playlist_dir = root.join("BDMV").join("PLAYLIST");
    let entries = std::fs::read_dir(&playlist_dir).map_err(|source| DiscError::Io {
        path: playlist_dir.clone(),
        source,
    })?;

    let mut candidates: Vec<(String, Title)> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| DiscError::Io {
            path: playlist_dir.clone(),
            source,
        })?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("mpls") {
            continue;
        }
        let Some(number) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.parse::<u16>().ok())
        else {
            continue;
        };
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(playlist) = PlayListMpls::parse(&bytes) else {
            continue;
        };
        let duration_ticks = playlist.duration_90k();
        if duration_ticks == 0 {
            continue;
        }
        let spans = playlist.chapters_with_duration();
        let duration = Some(Duration::from_secs_f64(duration_ticks as f64 / 90_000.0));
        let chapter_durations: Vec<Duration> = if spans.is_empty() {
            duration.into_iter().collect()
        } else {
            spans
                .iter()
                .map(|span| Duration::from_secs_f64(span.duration_90k() as f64 / 90_000.0))
                .collect()
        };
        let chapters = u16::try_from(chapter_durations.len().max(1)).unwrap_or(u16::MAX);

        // Commercial discs alias the same feature under several playlist ids
        // (menu language variants, anti-rip copies). Deduplicate on the clip
        // sequence, not the duration: two different episodes can be the same
        // length and still be different content.
        let signature = playlist
            .play_list
            .play_items
            .iter()
            .map(|item| {
                format!(
                    "{}:{}:{}",
                    item.clip_information_file_name, item.in_time_ticks, item.out_time_ticks
                )
            })
            .collect::<Vec<_>>()
            .join("|");
        if signature.is_empty() {
            continue;
        }

        candidates.push((
            signature,
            Title {
                // The playlist id is the stable identity of a Blu-ray title; the
                // DVD fields stay zero because they describe VOB title sets.
                number,
                vts: 0,
                vts_ttn: 0,
                angles: playlist
                    .play_list
                    .play_items
                    .iter()
                    .map(|item| item.num_angles())
                    .min()
                    .unwrap_or(1),
                chapters,
                duration,
                chapter_durations,
            },
        ));
    }

    // Pick the lowest playlist id when several aliases carry the same clips, so
    // a rescan does not swap the main feature's identity with directory order.
    candidates.sort_by_key(|(_, title)| title.number);
    let mut seen = HashSet::new();
    let titles: Vec<Title> = candidates
        .into_iter()
        .filter(|(signature, _)| seen.insert(signature.clone()))
        .map(|(_, title)| title)
        .collect();

    if titles.is_empty() {
        return Err(DiscError::BluRay(
            "no playable HDMV playlists found on this Blu-ray".into(),
        ));
    }

    Ok(DiscModel {
        volume_id,
        provider_id: String::new(),
        vts_count: 0,
        titles,
    })
}

/// Remux one Blu-ray title to `out`.
#[allow(clippy::too_many_arguments)]
pub fn remux_title(
    source: &DiscSource,
    title: &Title,
    first: u16,
    last: u16,
    out: &Path,
    keydb: Option<&Path>,
    progress: &mut dyn FnMut(RemuxProgress),
    cancel: Option<&AtomicBool>,
) -> Result<RemuxReport, DiscError> {
    // The first pass keeps chapter-accurate selection for the DVD path. A Blu-ray
    // episode that is a chapter range inside one playlist needs a seek/stop
    // implementation; taking the whole title and truncating it would be worse.
    if first != 1 || last != title.chapters {
        return Err(DiscError::Remux(
            "Blu-ray chapter-range rips are not supported yet".into(),
        ));
    }

    let root = source.root();
    let disc = mount(root)?;
    let info = disc
        .titles()
        .iter()
        .find(|info| info.playlist_id == title.number && info.kind == TitleKind::Hdmv)
        .cloned()
        .unwrap_or_else(|| TitleInfo {
            id: title.number,
            kind: TitleKind::Hdmv,
            playlist_id: title.number,
            duration_ticks: title
                .duration
                .map(|duration| (duration.as_secs_f64() * 90_000.0) as u64)
                .unwrap_or(0),
            languages: Vec::new(),
        });

    let decryptor = if root.join("AACS").is_dir() && !title_contents_are_clear(root, title.number) {
        match oxideav_bluray::aacs_adapter::try_resolve_aacs_with_keydb(root, keydb) {
            Ok(Some(decryptor)) => Some(decryptor),
            Ok(None) => {
                return Err(DiscError::AacsKey(
                    "no KEYDB.cfg entry for this Blu-ray; import a KEYDB with its VUK \
                     (or a device key and KCD) in settings"
                        .to_string(),
                ))
            }
            Err(error) => return Err(DiscError::AacsKey(error.to_string())),
        }
    } else {
        None
    };

    let mut source_reader = disc
        .open_title(&info, decryptor)
        .map_err(|error| DiscError::BluRay(format!("opening title: {error}")))?;
    // `TitleSource` opens at the first clip's file start, which can carry
    // material before the PlayItem IN point; seek to title PTS 0 so the remux
    // begins at the title's own keyframe. The clip may also continue past the
    // PlayItem OUT point; the muxer still sees the title's leading frames at
    // the right timestamps, which is what matters for a correct start.
    let start_byte = source_reader
        .seek_to(0)
        .map_err(|error| DiscError::BluRay(format!("seeking to title start: {error}")))?;
    let bytes_total = source_reader
        .output_total()
        .saturating_sub(start_byte)
        .max(1);
    let read_bytes = Arc::new(AtomicU64::new(0));
    let reader = TitleReader {
        inner: source_reader,
        read_bytes: Arc::clone(&read_bytes),
    };

    let mut demux = TsDemuxer::open(Box::new(reader))
        .map_err(|error| DiscError::Remux(format!("Blu-ray MPEG-TS demux: {error}")))?;
    let mut streams = demux.streams().to_vec();
    let video = streams.iter().position(|stream| {
        matches!(
            stream.params.codec_id.as_str(),
            "h264" | "hevc" | "vc1" | "mpeg2video"
        )
    });

    if let Some(index) = video {
        let codec = streams[index].params.codec_id.as_str().to_string();
        if codec != "h264" {
            return Err(DiscError::Remux(format!(
                "{codec} Blu-ray video is not supported yet (H.264 only)"
            )));
        }
        let (width, height) = playlist_dimensions(root, title.number).ok_or_else(|| {
            DiscError::Remux("Blu-ray playlist carries no primary video format".into())
        })?;
        streams[index].params.width = Some(width);
        streams[index].params.height = Some(height);
    }
    apply_track_metadata(&disc, &info, &mut streams);

    // Buffer up to the first access unit that carries both the IDR slice and
    // the SPS/PPS. Blu-ray often puts a short black/logo leader before the
    // feature; starting on that first IDR keeps the movie's own timestamps
    // and gives Matroska a valid CodecPrivate.
    let title_start = playlist_start_pts(root, title.number);
    let mut pending = Vec::new();
    let mut parameter_sets: Option<Vec<u8>> = None;
    let mut have_avcc = video.is_none();
    while pending.len() < 4000 && !have_avcc {
        match demux.next_packet() {
            Ok(Some(packet)) => {
                if Some(packet.stream_index) == video.map(|index| index as u32) {
                    if contains_nal_type(&packet.data, 7) {
                        let config = avc_config_record(&packet.data);
                        if !config.is_empty() {
                            parameter_sets = Some(config);
                        }
                    }
                    if packet.pts.is_some_and(|pts| pts >= title_start as i64)
                        && contains_nal_type(&packet.data, 5)
                    {
                        if let Some(config) = parameter_sets.take() {
                            streams[video.unwrap()].params.extradata = config;
                            have_avcc = true;
                        }
                    }
                }
                pending.push(packet);
            }
            Ok(None) => break,
            Err(error) => {
                return Err(DiscError::Remux(format!(
                    "reading Blu-ray parameter sets: {error}"
                )))
            }
        }
    }
    if video.is_some() && !have_avcc {
        return Err(DiscError::Remux(
            "could not find the H.264 SPS/PPS in the Blu-ray stream".into(),
        ));
    }
    let base_pts = Some(title_start as i64);

    let spans = disc.chapter_spans(&info);
    let temp = partial_path(out);
    if let Some(parent) = temp.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|source| DiscError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
    }
    let mut guard = PartialFile::new(temp.clone());
    let output = File::create(&temp).map_err(|source| DiscError::Io {
        path: temp.clone(),
        source,
    })?;
    let writer: Box<dyn WriteSeek> = Box::new(BufWriter::new(output));
    let mut muxer = MatroskaMuxer::new(writer, &streams)
        .map_err(|error| DiscError::Remux(format!("MKV mux init: {error}")))?;
    muxer
        .with_duration_finalization()
        .map_err(|error| DiscError::Remux(format!("MKV duration finalization: {error}")))?;

    for (index, span) in spans.iter().enumerate() {
        let start = ticks_to_ns(span.start_pts_90k);
        let end = ticks_to_ns(span.end_pts_90k).max(start);
        muxer
            .add_chapter(start, Some(end), format!("Chapter {}", index + 1))
            .map_err(|error| DiscError::Remux(format!("adding chapter: {error}")))?;
    }
    muxer
        .write_header()
        .map_err(|error| DiscError::Remux(format!("writing MKV header: {error}")))?;

    let mut report = RemuxReport {
        chapters: u16::try_from(spans.len()).unwrap_or(u16::MAX),
        ..Default::default()
    };
    {
        let mut writer = MuxWriter {
            muxer: &mut muxer,
            base_pts,
            report: &mut report,
            progress,
            read_bytes: &read_bytes,
            bytes_total,
        };
        let mut video_assembler = AuAssembler::new(video.map(|index| index as u32), title_start);
        let mut audio_assembler = AudioAssembler::new(&streams);

        for packet in pending {
            if Some(packet.stream_index) == video.map(|index| index as u32) {
                video_assembler.feed(&mut writer, packet)?;
            } else {
                audio_assembler.feed(&mut writer, packet)?;
            }
        }

        loop {
            if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
                return Err(DiscError::Cancelled);
            }
            match demux.next_packet() {
                Ok(Some(packet)) => {
                    if Some(packet.stream_index) == video.map(|index| index as u32) {
                        video_assembler.feed(&mut writer, packet)?;
                    } else {
                        audio_assembler.feed(&mut writer, packet)?;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    return Err(DiscError::Remux(format!("reading Blu-ray title: {error}")))
                }
            }
        }
        video_assembler.finish(&mut writer)?;
        audio_assembler.finish(&mut writer)?;
    }

    muxer
        .write_trailer()
        .map_err(|error| DiscError::Remux(format!("writing MKV trailer: {error}")))?;
    drop(muxer);
    replace_file(&temp, out)?;
    guard.defuse();
    Ok(report)
}

fn mount(root: &Path) -> Result<Disc, DiscError> {
    Disc::mount(root).map_err(|error| DiscError::BluRay(format!("reading structure: {error}")))
}

/// Whether the stream files are already clear despite an `AACS/` directory.
///
/// A MakeMKV decrypted backup keeps the AACS tree but replaces the ciphertext;
/// treating that as encrypted would demand a key for a disc that no longer
/// needs one. Sample a few Aligned Units: in clear BD-AV every source packet
/// starts with the 0x47 TS sync byte after its 4-byte TP_extra header.
fn contents_are_clear(root: &Path) -> bool {
    let stream_dir = root.join("BDMV").join("STREAM");
    let Ok(entries) = std::fs::read_dir(stream_dir) else {
        return false;
    };
    let mut paths: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    let mut sampled = 0usize;
    for path in paths {
        if path.extension().and_then(|ext| ext.to_str()) != Some("m2ts") {
            continue;
        }
        if !clip_is_clear(&path) {
            return false;
        }
        sampled += 1;
        if sampled >= 8 {
            break;
        }
    }
    sampled > 0
}

/// Whether the clips a title actually plays are clear. Checking every stream
/// file can be fooled by clear menus, and checking only the first can be fooled
/// by a clear leader attached to encrypted content, so walk the title's own
/// PlayItems.
fn title_contents_are_clear(root: &Path, playlist_id: u16) -> bool {
    let path = root
        .join("BDMV")
        .join("PLAYLIST")
        .join(format!("{playlist_id:05}.mpls"));
    let Ok(bytes) = std::fs::read(path) else {
        return contents_are_clear(root);
    };
    let Ok(playlist) = PlayListMpls::parse(&bytes) else {
        return contents_are_clear(root);
    };
    let mut sampled = 0usize;
    for item in &playlist.play_list.play_items {
        let clip = root
            .join("BDMV")
            .join("STREAM")
            .join(format!("{}.m2ts", item.clip_information_file_name));
        if !clip_is_clear(&clip) {
            return false;
        }
        sampled += 1;
        if sampled >= 8 {
            break;
        }
    }
    sampled > 0 && sampled == playlist.play_list.play_items.len().min(8)
}

fn clip_is_clear(path: &Path) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut sample = vec![0u8; 8 * 192];
    let Ok(read) = file.read(&mut sample) else {
        return false;
    };
    read >= 192
        && sample[..read]
            .chunks_exact(192)
            .all(|packet| packet.get(4) == Some(&0x47))
}

/// `TitleSource` is byte-addressable but its `SeekFrom::End` walks the whole
/// title. The TS demuxer asks for the end once, only to probe duration, so hide
/// the end from it and keep the streaming path cheap.
struct TitleReader {
    inner: TitleSource,
    read_bytes: Arc<AtomicU64>,
}

impl Read for TitleReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.read_bytes.fetch_add(read as u64, Ordering::Relaxed);
        Ok(read)
    }
}

impl Seek for TitleReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match pos {
            SeekFrom::End(0) => Ok(0),
            other => self.inner.seek(other),
        }
    }
}

struct MuxWriter<'a> {
    muxer: &'a mut MatroskaMuxer,
    base_pts: Option<i64>,
    report: &'a mut RemuxReport,
    progress: &'a mut dyn FnMut(RemuxProgress),
    read_bytes: &'a AtomicU64,
    bytes_total: u64,
}

impl MuxWriter<'_> {
    fn write(&mut self, mut packet: Packet) -> Result<(), DiscError> {
        if let Some(base) = self.base_pts {
            if let Some(pts) = &mut packet.pts {
                *pts = (*pts - base).max(0);
            }
            if let Some(dts) = &mut packet.dts {
                *dts = (*dts - base).max(0);
            }
        }
        self.muxer
            .write_packet(&packet)
            .map_err(|error| DiscError::Remux(format!("writing MKV packet: {error}")))?;
        self.report.packets += 1;
        self.report.payload_bytes += packet.data.len() as u64;
        (self.progress)(RemuxProgress {
            phase: RemuxPhase::Muxing,
            bytes_done: self.read_bytes.load(Ordering::Relaxed),
            bytes_total: self.bytes_total,
            written_bytes: self.report.payload_bytes,
        });
        Ok(())
    }
}

/// Assemble H.264 PES payloads into complete access units.
///
/// A PES packet is not required to end at an access-unit or slice boundary.
/// Repacking each PES on its own feeds the decoder truncated slices, which
/// shows up as macroblock/reference errors. This assembler carries bytes
/// across PES boundaries and flushes on the next AUD.
struct AuAssembler {
    video: Option<u32>,
    title_start: u64,
    carry: Vec<u8>,
    au: Vec<u8>,
    pts: Option<i64>,
    dts: Option<i64>,
    started: bool,
}

impl AuAssembler {
    fn new(video: Option<u32>, title_start: u64) -> Self {
        Self {
            video,
            title_start,
            carry: Vec::new(),
            au: Vec::new(),
            pts: None,
            dts: None,
            started: video.is_none(),
        }
    }

    fn feed(&mut self, writer: &mut MuxWriter<'_>, packet: Packet) -> Result<(), DiscError> {
        self.carry.extend_from_slice(&packet.data);
        let starts = start_codes(&self.carry);
        if starts.len() < 2 {
            return Ok(());
        }
        for pair in starts.windows(2) {
            let (start, next) = (pair[0], pair[1]);
            let nal = self.carry[start..next].to_vec();
            let prefix = start_code_len(&nal);
            if nal.len() <= prefix {
                continue;
            }
            if nal[prefix] & 0x1F == 9 && !self.au.is_empty() {
                self.flush(writer)?;
            }
            if self.pts.is_none() {
                self.pts = packet.pts;
                self.dts = packet.dts;
            }
            self.au.extend_from_slice(&nal);
        }
        let last = *starts.last().unwrap();
        self.carry.drain(..last);
        Ok(())
    }

    fn finish(&mut self, writer: &mut MuxWriter<'_>) -> Result<(), DiscError> {
        if !self.carry.is_empty() {
            let from = find_start_code(&self.carry).unwrap_or(self.carry.len());
            let tail = self.carry[from..].to_vec();
            self.au.extend_from_slice(&tail);
            self.carry.clear();
        }
        self.flush(writer)
    }

    fn flush(&mut self, writer: &mut MuxWriter<'_>) -> Result<(), DiscError> {
        if self.au.is_empty() {
            return Ok(());
        }
        let data = packetize_all(&self.au);
        if !self.started {
            if self.pts.is_some_and(|pts| pts >= self.title_start as i64)
                && contains_nal_type(&self.au, 9)
                && contains_nal_type(&self.au, 5)
            {
                self.started = true;
            } else {
                self.au.clear();
                self.pts = None;
                self.dts = None;
                return Ok(());
            }
        }
        if !data.is_empty() {
            let keyframe = contains_nal_type(&self.au, 5);
            writer.write(Packet {
                stream_index: self.video.unwrap_or(0),
                time_base: TimeBase::new(1, 90_000),
                pts: self.pts,
                dts: self.dts,
                duration: None,
                flags: PacketFlags {
                    keyframe,
                    ..Default::default()
                },
                data,
            })?;
        }
        self.au.clear();
        self.pts = None;
        self.dts = None;
        Ok(())
    }
}

/// Assemble the two Blu-ray audio streams whose PES packets are not
/// self-contained Matroska blocks.
///
/// TrueHD shares its PID with a Dolby Digital compatibility core. The AC-3
/// frames are a different codec and must be dropped from the TrueHD track.
/// DTS-HD MA splits its core and extension across consecutive PES packets with
/// the same PTS; writing those separately makes the decoder complain about an
/// extension without a core, so they are grouped back into one block.
struct AudioAssembler {
    truehd: Option<u32>,
    dts: Option<u32>,
    truehd_carry: Vec<u8>,
    truehd_pts: Option<i64>,
    truehd_dts: Option<i64>,
    dts_group: Option<(Option<i64>, Vec<u8>)>,
}

impl AudioAssembler {
    fn new(streams: &[StreamInfo]) -> Self {
        let truehd = streams
            .iter()
            .position(|stream| stream.params.codec_id.as_str() == "truehd")
            .map(|index| index as u32);
        let dts = streams
            .iter()
            .position(|stream| stream.params.codec_id.as_str() == "dts")
            .map(|index| index as u32);
        Self {
            truehd,
            dts,
            truehd_carry: Vec::new(),
            truehd_pts: None,
            truehd_dts: None,
            dts_group: None,
        }
    }

    fn feed(&mut self, writer: &mut MuxWriter<'_>, packet: Packet) -> Result<(), DiscError> {
        if Some(packet.stream_index) == self.dts {
            // A DTS-HD MA access unit is a core frame followed by one or
            // more extension frames. Keep the core and its extensions in
            // one Matroska block; a core frame starts `7F FE 80 01`, while
            // the extension frames start `64 58 20 25`.
            let is_core = packet.data.starts_with(&[0x7F, 0xFE, 0x80, 0x01]);
            if is_core && self.dts_group.is_some() {
                self.flush_dts(writer)?;
            }
            let group = self
                .dts_group
                .get_or_insert_with(|| (packet.pts, Vec::new()));
            group.1.extend_from_slice(&packet.data);
            return Ok(());
        }
        if Some(packet.stream_index) == self.truehd {
            // Dolby Digital compatibility core interleaved into the TrueHD PID.
            // A real TrueHD major frame that happens to start with 0B 77 still
            // carries the F8 72 6F BA sync four bytes in.
            if packet.data.starts_with(&[0x0B, 0x77])
                && !packet
                    .data
                    .get(4..8)
                    .is_some_and(|sync| sync == [0xF8, 0x72, 0x6F, 0xBA])
            {
                return Ok(());
            }
            if self.truehd_carry.is_empty() {
                self.truehd_pts = packet.pts;
                self.truehd_dts = packet.dts;
            }
            self.truehd_carry.extend_from_slice(&packet.data);
            return self.flush_truehd(writer);
        }
        writer.write(packet)
    }

    fn finish(&mut self, writer: &mut MuxWriter<'_>) -> Result<(), DiscError> {
        self.flush_truehd(writer)?;
        self.flush_dts(writer)
    }

    fn flush_truehd(&mut self, writer: &mut MuxWriter<'_>) -> Result<(), DiscError> {
        loop {
            if self.truehd_carry.len() < 2 {
                return Ok(());
            }
            let size = (u16::from_be_bytes([self.truehd_carry[0], self.truehd_carry[1]]) & 0x0FFF)
                as usize
                * 2;
            if !(4..=8190).contains(&size) {
                let Some(sync) = find_truehd_sync(&self.truehd_carry) else {
                    self.truehd_carry.clear();
                    return Ok(());
                };
                let header = sync.saturating_sub(4);
                // A valid TrueHD frame has its sync four bytes in; a sync at
                // offset zero is malformed. Drop one byte so this loop always
                // makes progress.
                self.truehd_carry.drain(..header.max(1));
                continue;
            }
            if self.truehd_carry.len() < size {
                return Ok(());
            }
            let frame: Vec<u8> = self.truehd_carry.drain(..size).collect();
            writer.write(Packet {
                stream_index: self.truehd.unwrap_or(0),
                time_base: TimeBase::new(1, 90_000),
                pts: self.truehd_pts,
                dts: self.truehd_dts,
                duration: None,
                flags: PacketFlags::default(),
                data: frame,
            })?;
            // TrueHD carries 40 samples per frame at 48 kHz: 75 ticks.
            if let Some(pts) = &mut self.truehd_pts {
                *pts += 75;
            }
            if let Some(dts) = &mut self.truehd_dts {
                *dts += 75;
            }
        }
    }

    fn flush_dts(&mut self, writer: &mut MuxWriter<'_>) -> Result<(), DiscError> {
        let Some((pts, data)) = self.dts_group.take() else {
            return Ok(());
        };
        if data.is_empty() {
            return Ok(());
        }
        writer.write(Packet {
            stream_index: self.dts.unwrap_or(0),
            time_base: TimeBase::new(1, 90_000),
            pts,
            dts: None,
            duration: None,
            flags: PacketFlags::default(),
            data,
        })
    }
}

fn find_truehd_sync(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|window| window == [0xF8, 0x72, 0x6F, 0xBA])
}

/// Packetise every NAL in an Annex-B access unit, including SPS/PPS.
///
/// Parameter sets can change mid-title (this disc's feature switches from its
/// black leader's SPS/PPS to the movie's at the first real frame). Matroska has
/// no way to update CodecPrivate mid-track, so the in-band copies must stay.
fn packetize_all(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut pos = 0usize;
    while let Some(relative) = find_start_code(&data[pos..]) {
        let prefix = pos + relative;
        let start = prefix + 3;
        let next = find_start_code(&data[start..])
            .map(|offset| start + offset)
            .unwrap_or(data.len());
        let mut end = next;
        while end > start && data[end - 1] == 0 {
            end -= 1;
        }
        if end > start {
            out.extend_from_slice(&((end - start) as u32).to_be_bytes());
            out.extend_from_slice(&data[start..end]);
        }
        pos = next;
    }
    out
}

fn find_start_code(bytes: &[u8]) -> Option<usize> {
    let mut index = 0;
    while index + 2 < bytes.len() {
        if bytes[index] == 0 && bytes[index + 1] == 0 && bytes[index + 2] == 1 {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn start_codes(data: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut index = 0;
    while index + 3 <= data.len() {
        if data[index..].starts_with(&[0, 0, 1]) {
            out.push(index);
            index += 3;
        } else {
            index += 1;
        }
    }
    out
}

fn start_code_len(nal: &[u8]) -> usize {
    if nal.starts_with(&[0, 0, 0, 1]) {
        4
    } else {
        3
    }
}

fn contains_nal_type(data: &[u8], nal_type: u8) -> bool {
    let mut pos = 0usize;
    while let Some(relative) = find_start_code(&data[pos..]) {
        let start = pos + relative + 3;
        if start < data.len() && data[start] & 0x1F == nal_type {
            return true;
        }
        pos = start;
    }
    false
}

/// Metadata from the playlist: video geometry and per-track languages. The
/// MPEG-TS PMT does not carry the Blu-ray language table, and the MKV muxer
/// would otherwise default every track to `eng`.
fn apply_track_metadata(disc: &Disc, info: &TitleInfo, streams: &mut [StreamInfo]) {
    let catalogue = disc.title_streams(info);
    let mut audio = catalogue
        .tracks
        .iter()
        .filter(|track| track.kind == TrackKind::PrimaryAudio);
    let mut subtitles = catalogue
        .tracks
        .iter()
        .filter(|track| track.kind == TrackKind::PgSubtitle);

    for stream in streams.iter_mut() {
        match stream.params.codec_id.as_str() {
            "h264" | "hevc" | "vc1" | "mpeg2video" => {}
            "hdmv_pgs_subtitle" => {
                if let Some(track) = subtitles.next() {
                    stream.params.language = track.language.clone();
                }
            }
            _ => {
                if let Some(track) = audio.next() {
                    stream.params.language = track.language.clone();
                }
            }
        }
    }
}

fn playlist_start_pts(root: &Path, playlist_id: u16) -> u64 {
    let path = root
        .join("BDMV")
        .join("PLAYLIST")
        .join(format!("{playlist_id:05}.mpls"));
    let Ok(bytes) = std::fs::read(path) else {
        return 0;
    };
    let Ok(playlist) = PlayListMpls::parse(&bytes) else {
        return 0;
    };
    playlist
        .play_list
        .play_items
        .first()
        .map(|item| u64::from(item.in_time_ticks) * 2)
        .unwrap_or(0)
}

fn playlist_dimensions(root: &Path, playlist_id: u16) -> Option<(u32, u32)> {
    let path = root
        .join("BDMV")
        .join("PLAYLIST")
        .join(format!("{playlist_id:05}.mpls"));
    let bytes = std::fs::read(path).ok()?;
    let playlist = PlayListMpls::parse(&bytes).ok()?;
    let video = playlist
        .play_list
        .play_items
        .iter()
        .find_map(|item| item.stn_table.primary_video.first())?;
    let format = video.video_format_kind();
    let height = format.vertical_lines()?;
    let width = match format {
        VideoFormat::Video480i | VideoFormat::Video480p => 720,
        VideoFormat::Video576i | VideoFormat::Video576p => 720,
        VideoFormat::Video720p => 1280,
        VideoFormat::Video1080i | VideoFormat::Video1080p => 1920,
        VideoFormat::Video2160p => 3840,
        VideoFormat::Other(_) => return None,
    };
    Some((width, u32::from(height)))
}

fn ticks_to_ns(ticks: u64) -> u64 {
    ticks.saturating_mul(100_000) / 9
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let root =
            std::env::temp_dir().join(format!("packrat-bluray-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("BDMV/STREAM")).expect("create stream dir");
        std::fs::create_dir_all(root.join("AACS")).expect("create aacs dir");
        root
    }

    #[test]
    fn clear_stream_files_are_recognised_despite_an_aacs_directory() {
        let root = temp_root("clear");
        let mut sample = vec![0u8; 8 * 192];
        for packet in sample.chunks_exact_mut(192) {
            packet[4] = 0x47;
        }
        std::fs::write(root.join("BDMV/STREAM/00000.m2ts"), &sample).expect("write stream");
        assert!(contents_are_clear(&root));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn encrypted_stream_files_are_not_mistaken_for_clear_ones() {
        let root = temp_root("encrypted");
        std::fs::write(root.join("BDMV/STREAM/00000.m2ts"), vec![0u8; 8 * 192])
            .expect("write stream");
        assert!(!contents_are_clear(&root));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn packetize_keeps_parameter_sets_in_band() {
        // AUD, SPS, PPS, IDR. Matroska's CodecPrivate can only be written
        // once, so a mid-title parameter-set change must survive in the
        // packet data.
        let au = [
            0x00, 0x00, 0x00, 0x01, 0x09, 0x10, 0x00, 0x00, 0x00, 0x01, 0x27, 0x64, 0x00, 0x29,
            0x00, 0x00, 0x01, 0x28, 0x01, 0x00, 0x00, 0x01, 0x25, 0x88, 0x84,
        ];
        let packetized = packetize_all(&au);
        let mut cursor = 0usize;
        let mut types = Vec::new();
        while cursor + 4 <= packetized.len() {
            let len = u32::from_be_bytes([
                packetized[cursor],
                packetized[cursor + 1],
                packetized[cursor + 2],
                packetized[cursor + 3],
            ]) as usize;
            cursor += 4;
            assert!(cursor + len <= packetized.len());
            types.push(packetized[cursor] & 0x1F);
            cursor += len;
        }
        assert_eq!(types, vec![9, 7, 8, 5]);
        assert_eq!(cursor, packetized.len());
    }

    #[test]
    fn nal_type_scan_finds_parameter_sets_and_idr() {
        let au = [
            0x00, 0x00, 0x00, 0x01, 0x09, 0x10, 0x00, 0x00, 0x01, 0x27, 0x64, 0x00, 0x00, 0x01,
            0x28, 0x01, 0x00, 0x00, 0x01, 0x25, 0x88,
        ];
        assert!(contains_nal_type(&au, 7));
        assert!(contains_nal_type(&au, 8));
        assert!(contains_nal_type(&au, 5));
        assert!(!contains_nal_type(&au, 1));
    }

    #[test]
    fn chapter_ticks_convert_to_nanoseconds() {
        assert_eq!(ticks_to_ns(0), 0);
        assert_eq!(ticks_to_ns(90_000), 1_000_000_000);
        assert_eq!(ticks_to_ns(45_000), 500_000_000);
    }
}

//! Lossless remux of a DVD title's VOB chain into a Matroska (`.mkv`) file.
//!
//! A mounted disc gives us the title's VOBs as ordinary files rather than one
//! flat image, so we present `VTS_xx_1.VOB`…`VTS_xx_9.VOB` as a single virtual
//! sector-addressable reader and stream their MPEG-PS packs through the
//! `oxideav-dvd` PES parser into `oxideav-mkv`'s muxer. Video and audio are
//! copied bit-for-bit — there is no re-encode.

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use oxideav_core::packet::PacketFlags;
use oxideav_core::{CodecId, CodecParameters, Muxer, Packet, StreamInfo, TimeBase, WriteSeek};
use oxideav_dvd::vob::{
    looks_like_nav_pack, DvdSubstream, NavPack, PackHeader, PesPacket, AUDIO_SUBSTREAM_HEADER_LEN,
    SC_PADDING_STREAM, SC_PRIVATE_STREAM_1, SC_PRIVATE_STREAM_2, SC_SYSTEM_HEADER,
};
use oxideav_dvd::{
    Ac3Header, AspectRatioCode, AudioAttributes, AudioCodingMode, AudioStreamControl,
    AudioSubstreamHeader, DtsHeader, DvdChapter, DvdTitle, PictureCodingExtension, PictureHeader,
    PictureStructure, SequenceHeader, SubpictureStreamControl, VtsIfo,
};
use oxideav_mkv::mux::{MkvMuxer, MkvVideoGeometry};

use crate::disc::Title;
use crate::error::DiscError;
use crate::source::DiscSource;

/// DVD logical sector size.
const SECTOR: usize = 2048;
/// Sectors fetched from the drive in one request while walking a chain.
///
/// A drive answers a command in roughly a millisecond whether it asks for one
/// sector or a hundred, so a request per 2 KiB sector spends nearly all its
/// time on command turnaround and pins a rip near 2 MB/s whatever the disc's
/// rated speed. A window per request amortises that away and lets the media
/// rate set the pace.
///
/// 128 sectors (256 KiB) leaves command turnaround a few percent of the time
/// budget rather than the fifth or so that 64 KiB still spends on it, and stays
/// well inside the 1 MiB the drive accepts in one request. Bigger windows trade
/// against damage: a window the drive refuses costs one failed request and then
/// its sectors are read one at a time, so an over-large window would widen that
/// fallback for no further gain.
const READ_WINDOW_SECTORS: u64 = 128;
/// Times to retry a VOB sector the drive failed to read before giving up.
///
/// Optical drives return transient `EIO` for marginal sectors — a speck of
/// dust, a light scratch, or a spot just past what the media holds — and a
/// fresh attempt usually reads the same sector fine. A handful of retries
/// recovers those without spending the drive's whole error-recovery timeout on
/// a sector that is never coming back; the kernel's own retries have already
/// been exhausted by the time a read reaches us.
const SECTOR_READ_ATTEMPTS: u32 = 3;
/// How many times a "not ready" answer is waited out before giving up.
///
/// The device is opened `O_NONBLOCK` so probing a drive cannot make the kernel
/// close its tray, and that flag also applies to the reads. On Linux an optical
/// drive answers `EAGAIN` while it spins up or seeks, which is a delay rather
/// than a bad sector. With the backoff below, this many attempts wait about
/// five seconds before treating the disc as damaged.
const NOT_READY_ATTEMPTS: u32 = 100;
/// How long to wait after the first "not ready" answer.
const NOT_READY_DELAY_MIN: std::time::Duration = std::time::Duration::from_millis(2);
/// The longest a single "not ready" wait grows to.
const NOT_READY_DELAY_MAX: std::time::Duration = std::time::Duration::from_millis(50);
/// How many times the wait doubles before it stops growing.
const NOT_READY_BACKOFF_STEPS: u32 = 5;
/// Unreadable packs tolerated in one cell before the rip gives up.
///
/// A pack that will not parse is usually a single odd spot on the disc, and
/// skipping it costs a few frames. A run of them means the sectors are not the
/// ones the IFO described, which skipping cannot fix, so past this many the
/// error is the honest answer.
const MALFORMED_IN_CELL_MAX: u64 = 64;
/// Consecutive unreadable sectors before a damaged run is skipped over.
///
/// Retrying every sector of a long scratch would spend the drive's error
/// recovery on each one, so after two failures in a row the walk skips ahead
/// without probing. An isolated bad spot still costs a single probe.
const SKIP_CONSECUTIVE_FAILURES: u32 = 2;
/// First, then maximum, sectors skipped when crossing a damaged run.
///
/// The step starts small so little is lost if the damage is short, and grows
/// (then resets once a readable sector is found) so a long run is crossed in a
/// few probes rather than one per sector.
const SKIP_MIN: u64 = 4;
const SKIP_MAX: u64 = 128;
/// PES timestamps are in 90 kHz units.
const PES_TIME_BASE: TimeBase = TimeBase::new(1, 90_000);
/// Length of the DVD LPCM audio-pack header (including the substream byte).
const LPCM_HEADER_LEN: usize = 7;

/// What a remux produced.
#[derive(Debug, Clone, Copy, Default)]
pub struct RemuxReport {
    /// Elementary-stream packets written.
    pub packets: u64,
    /// Payload bytes written (excluding container overhead).
    pub payload_bytes: u64,
    /// Number of chapters written.
    pub chapters: u16,
    /// Sectors missing from the output: the drive could not read them, or they
    /// were skipped while crossing a damaged run. Non-zero means the rip is
    /// complete but has glitches there.
    pub unreadable_sectors: u64,
}

/// Which pass a remux is currently running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemuxPhase {
    /// Reading the title to discover its elementary streams (nothing written).
    ///
    /// This pass stops as soon as it has confirmed the streams the IFO
    /// declares, so it usually reads only the start of the title; it runs to
    /// the end only when the IFO cannot be trusted to list them.
    Probing,
    /// Writing packets to the output.
    Muxing,
}

/// A progress snapshot emitted while a remux runs.
///
/// `bytes_done`/`bytes_total` span the probe pass and muxing, so a throughput
/// or time estimate is available from the very first sector. How much of the
/// title the probe reads is only known once it finishes, so `bytes_total` is
/// the two-pass estimate while probing and is then revised down to the work the
/// rip will really do. `written_bytes` separates the disc read side from the
/// output write side, so a caller can tell which one is the bottleneck.
#[derive(Debug, Clone, Copy)]
pub struct RemuxProgress {
    /// The pass currently running.
    pub phase: RemuxPhase,
    /// VOB bytes read so far, across both passes.
    pub bytes_done: u64,
    /// VOB bytes that will be read, across both passes. Settled once muxing
    /// begins.
    pub bytes_total: u64,
    /// Bytes handed to the output writer so far.
    pub written_bytes: u64,
}

/// Remux a whole title to `out`.
pub fn remux_title(
    source: &DiscSource,
    vts: &VtsIfo,
    title: &Title,
    out: &Path,
) -> Result<RemuxReport, DiscError> {
    remux_chapters(source, vts, title, 1, title.chapters, out)
}

/// Remux chapters `first..=last` (1-based) of a title to `out`, reading the
/// title's VOBs from a mounted folder.
pub fn remux_chapters(
    source: &DiscSource,
    vts: &VtsIfo,
    title: &Title,
    first: u16,
    last: u16,
    out: &Path,
) -> Result<RemuxReport, DiscError> {
    let video_ts = source
        .video_ts()
        .ok_or_else(|| DiscError::NoVideoTs(source.root().display().to_string()))?;
    let mut reader = VtsChainReader::open(video_ts, title.vts)?;
    remux_chapters_with_reader(&mut reader, vts, title, first, last, out)
}

/// Remux from whichever medium the source represents: the mounted folder or
/// the raw device (decrypted through the user-supplied libdvdcss when present).
pub fn remux_chain(
    source: &DiscSource,
    vts: &VtsIfo,
    title: &Title,
    first: u16,
    last: u16,
    out: &Path,
) -> Result<RemuxReport, DiscError> {
    remux_chain_with_progress(source, vts, title, first, last, out, &mut |_| {})
}

/// Like [`remux_chain`], but reports [`RemuxProgress`] as it runs.
pub fn remux_chain_with_progress(
    source: &DiscSource,
    vts: &VtsIfo,
    title: &Title,
    first: u16,
    last: u16,
    out: &Path,
    progress: &mut dyn FnMut(RemuxProgress),
) -> Result<RemuxReport, DiscError> {
    remux_chain_impl(source, vts, title, first, last, out, progress, None)
}

/// Like [`remux_chain_with_progress`], but stops as soon as `cancel` is set.
///
/// The flag is checked per VOB sector, so cancellation lands within a disk
/// read. Any half-written `.partial` output is removed before
/// [`DiscError::Cancelled`] is returned, so an aborted rip leaves no file for
/// a media server to index.
#[allow(clippy::too_many_arguments)]
pub fn remux_chain_with_progress_and_cancel(
    source: &DiscSource,
    vts: &VtsIfo,
    title: &Title,
    first: u16,
    last: u16,
    out: &Path,
    progress: &mut dyn FnMut(RemuxProgress),
    cancel: &AtomicBool,
) -> Result<RemuxReport, DiscError> {
    remux_chain_impl(source, vts, title, first, last, out, progress, Some(cancel))
}

/// Build the right reader for the source and run the remux, passing the
/// optional cancellation flag straight through.
#[allow(clippy::too_many_arguments)]
fn remux_chain_impl(
    source: &DiscSource,
    vts: &VtsIfo,
    title: &Title,
    first: u16,
    last: u16,
    out: &Path,
    progress: &mut dyn FnMut(RemuxProgress),
    cancel: Option<&AtomicBool>,
) -> Result<RemuxReport, DiscError> {
    match source {
        DiscSource::BluRay { .. } => Err(DiscError::Remux(
            "Blu-ray titles use the Blu-ray remux path".into(),
        )),
        DiscSource::Folder { .. } => {
            let video_ts = source
                .video_ts()
                .ok_or_else(|| DiscError::NoVideoTs(source.root().display().to_string()))?;
            let mut reader = VtsChainReader::open(video_ts, title.vts)?;
            remux_chapters_with_reader_progress_cancel(
                &mut reader,
                vts,
                title,
                first,
                last,
                out,
                progress,
                cancel,
            )
        }
        DiscSource::Device { device, .. } => {
            // The raw device is only needed to decrypt CSS discs. Its
            // filesystem can still be unreadable — copy-protection schemes and
            // damage corrupt the ISO 9660 / UDF directory records a ripper uses
            // to find the VOBs — while the mounted folder reads perfectly well.
            // Fall back to the mount rather than failing the rip outright; an
            // encrypted disc still fails here, because its VOB sectors cannot
            // be read through the mount either.
            match crate::device::DeviceChainReader::open(device, title.vts) {
                Ok(mut reader) => remux_chapters_with_reader_progress_cancel(
                    &mut reader,
                    vts,
                    title,
                    first,
                    last,
                    out,
                    progress,
                    cancel,
                ),
                Err(device_error) => {
                    // Without a mount there is nothing to fall back to, and the
                    // device failure names the filesystem that could not be read.
                    let Some(video_ts) = source.video_ts() else {
                        return Err(device_error);
                    };
                    let mut reader = match VtsChainReader::open(video_ts, title.vts) {
                        Ok(reader) => reader,
                        Err(_) => return Err(device_error),
                    };
                    remux_chapters_with_reader_progress_cancel(
                        &mut reader,
                        vts,
                        title,
                        first,
                        last,
                        out,
                        progress,
                        cancel,
                    )
                }
            }
        }
    }
}

/// True when a cancellation has been requested.
fn cancelled(cancel: Option<&AtomicBool>) -> bool {
    cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed))
}

/// Wraps the output writer and keeps a running total of the bytes handed to
/// it, so progress can report a write rate alongside the disc read rate. The
/// count lives behind an `Arc<AtomicU64>` because the writer must stay `Send`.
struct CountingWriter<W> {
    inner: W,
    written: Arc<AtomicU64>,
}

impl<W> CountingWriter<W> {
    fn new(inner: W, written: Arc<AtomicU64>) -> Self {
        Self { inner, written }
    }
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<W: Seek> Seek for CountingWriter<W> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

/// Like [`remux_chapters`], but over any `Read + Seek` positioned within the
/// title's VOB chain — which is how the raw-device (libdvdcss) path feeds in
/// decrypted, re-addressed sectors.
pub fn remux_chapters_with_reader<R: Read + Seek>(
    reader: &mut R,
    vts: &VtsIfo,
    title: &Title,
    first: u16,
    last: u16,
    out: &Path,
) -> Result<RemuxReport, DiscError> {
    remux_chapters_with_reader_progress(reader, vts, title, first, last, out, &mut |_| {})
}

/// Like [`remux_chapters_with_reader`], but reports [`RemuxProgress`] as it
/// runs, with byte counts that span both the probe and muxing passes.
pub fn remux_chapters_with_reader_progress<R: Read + Seek>(
    reader: &mut R,
    vts: &VtsIfo,
    title: &Title,
    first: u16,
    last: u16,
    out: &Path,
    progress: &mut dyn FnMut(RemuxProgress),
) -> Result<RemuxReport, DiscError> {
    remux_chapters_with_reader_progress_cancel(reader, vts, title, first, last, out, progress, None)
}

/// The cancel-aware implementation behind the public remux entry points.
///
/// The probe pass stops as soon as it has confirmed the stream list the IFO
/// declares, which turns the usual two reads of the title into one. A title
/// that turns out to carry a stream its IFO does not list is re-read the long
/// way rather than ripped short, so the shortcut can only cost reads, never
/// streams.
#[allow(clippy::too_many_arguments)]
fn remux_chapters_with_reader_progress_cancel<R: Read + Seek>(
    reader: &mut R,
    vts: &VtsIfo,
    title: &Title,
    first: u16,
    last: u16,
    out: &Path,
    progress: &mut dyn FnMut(RemuxProgress),
    cancel: Option<&AtomicBool>,
) -> Result<RemuxReport, DiscError> {
    let mut probe_incomplete = false;
    match remux_once(
        reader,
        vts,
        title,
        first,
        last,
        out,
        &mut *progress,
        true,
        &mut probe_incomplete,
        cancel,
    ) {
        Err(_) if probe_incomplete => {
            eprintln!(
                "packrat: this title carries a stream its IFO does not declare; \
                 re-reading it in full"
            );
            let mut unused = false;
            remux_once(
                reader,
                vts,
                title,
                first,
                last,
                out,
                &mut *progress,
                false,
                &mut unused,
                cancel,
            )
        }
        result => result,
    }
}

/// One attempt at a remux.
///
/// `optimistic_probe` allows the probe pass to stop once the IFO's stream list
/// is confirmed. `probe_incomplete` is set when that shortcut turned out to be
/// wrong, telling the caller a full re-read is worth trying.
#[allow(clippy::too_many_arguments)]
fn remux_once<R: Read + Seek>(
    reader: &mut R,
    vts: &VtsIfo,
    title: &Title,
    first: u16,
    last: u16,
    out: &Path,
    progress: &mut dyn FnMut(RemuxProgress),
    optimistic_probe: bool,
    probe_incomplete: &mut bool,
    cancel: Option<&AtomicBool>,
) -> Result<RemuxReport, DiscError> {
    if cancelled(cancel) {
        return Err(DiscError::Cancelled);
    }
    let dvd_title = vts
        .titles
        .iter()
        .find(|t| t.number == title.vts_ttn)
        .ok_or_else(|| {
            DiscError::Remux(format!(
                "title set {} has no title {}",
                title.vts, title.vts_ttn
            ))
        })?;

    let chapter_count = title.chapter_durations.len().min(dvd_title.chapters.len()) as u16;
    if first < 1 || last < first || last > chapter_count {
        return Err(DiscError::Remux(format!(
            "chapter range {first}-{last} outside 1-{chapter_count}"
        )));
    }
    let selected = chapters_in_range(dvd_title, first, last);

    // One pass over the selected chapters, plus however much of it the probe
    // pass has to read to learn the streams — known only once it finishes, and
    // corrected below.
    let planned_bytes = count_sectors(vts, &selected).saturating_mul(SECTOR as u64);
    let mut bytes_total = planned_bytes.saturating_mul(2);
    let mut read_bytes = 0u64;
    // Sectors the drive could not read during muxing: those are the ones the
    // written file is actually missing. The probe pass discards its own count
    // so a sector that reads on the second try is not reported as lost.
    let mut unreadable = 0u64;
    // Output bytes are counted by the writer; nothing is written until muxing.
    let written_bytes = Arc::new(AtomicU64::new(0));

    // The streams the IFO says the selected chapters carry. Reading the whole
    // title just to learn this costs a second pass over the disc, which is half
    // the rip; the PGC's stream-control table already states it, so the probe
    // only has to run until it has seen all of them — usually the first seconds
    // of the title. Anything that makes that list untrustworthy falls back to
    // reading the title out, so the shortcut can only cost reads.
    let declared = declared_tracks(vts, &selected);
    let mut trustworthy = optimistic_probe && !declared.is_empty();
    let stop = AtomicBool::new(false);

    // Pass 1: discover the elementary streams, because Matroska's `Tracks`
    // element must be written before the first packet. The video sequence
    // header is parsed here too so the track carries the real geometry and
    // display aspect ratio rather than a placeholder.
    let mut tracks: Vec<Track> = Vec::new();
    let mut video_size: Option<(u32, u32)> = None;
    let mut video_dar: Option<(u64, u64)> = None;
    {
        let mut on_sectors = |sectors: u64| {
            read_bytes = read_bytes.saturating_add(sectors.saturating_mul(SECTOR as u64));
            progress(RemuxProgress {
                phase: RemuxPhase::Probing,
                bytes_done: read_bytes,
                bytes_total,
                written_bytes: 0,
            });
        };
        // The probe pass must tolerate the same bad spots as muxing, but its
        // count is not the one reported.
        let mut probe_unreadable = 0u64;
        for_each_pes(
            reader,
            vts,
            &selected,
            &mut |pes| {
                if let Some(track) = Track::from_pes(&pes) {
                    if !declared.is_empty() && !declared.contains(&track) {
                        // The title carries a stream the IFO does not list, so
                        // its list cannot be used to decide when the stream set
                        // is complete. Keep probing to the end.
                        trustworthy = false;
                    }
                    if !tracks.contains(&track) {
                        tracks.push(track);
                    }
                    if matches!(track, Track::Video) && video_size.is_none() {
                        if let Some(idx) = find_start_code(pes.payload, 0, SEQUENCE_HEADER) {
                            if let Ok(header) = SequenceHeader::parse(&pes.payload[idx..]) {
                                video_size = Some((
                                    u32::from(header.horizontal_size),
                                    u32::from(header.vertical_size),
                                ));
                                video_dar = display_aspect_ratio(header.aspect_ratio);
                            }
                        }
                    }
                }
                if probe_confirmed(trustworthy, &declared, &tracks) {
                    // Everything the IFO promised has now been seen and nothing
                    // it did not promise has turned up, so the stream set is
                    // settled and the rest of the title does not have to be read
                    // twice.
                    stop.store(true, Ordering::Relaxed);
                }
                Ok(())
            },
            &mut on_sectors,
            &mut probe_unreadable,
            WalkFlags {
                stop: Some(&stop),
                cancel,
            },
        )?;
    }
    if cancelled(cancel) {
        return Err(DiscError::Cancelled);
    }
    if tracks.is_empty() {
        return Err(DiscError::Remux("title has no playable streams".into()));
    }
    tracks.sort();

    // The probe only reads as far as it had to, so the real amount of work is
    // known now. Correcting the total here keeps the reported fraction from
    // running past its end.
    bytes_total = read_bytes.saturating_add(planned_bytes);
    progress(RemuxProgress {
        phase: RemuxPhase::Probing,
        bytes_done: read_bytes,
        bytes_total,
        written_bytes: 0,
    });

    // Which way the probe went is the difference between reading the title once
    // and reading it twice, and it turns on what the disc declares, so say it.
    let probed = read_bytes / SECTOR as u64;
    let planned = planned_bytes / SECTOR as u64;
    if read_bytes < planned_bytes {
        eprintln!(
            "packrat: IFO's stream list confirmed after {probed} of {planned} sectors; \
             reading the title once"
        );
    } else {
        eprintln!(
            "packrat: IFO's stream list was not enough to settle the streams; \
             reading all {planned} sectors before muxing"
        );
    }

    let video_index = tracks.iter().position(|t| matches!(t, Track::Video));
    let audio_streams = &vts.mat.title_attributes.audio_streams;
    let mut stream_infos: Vec<StreamInfo> = tracks
        .iter()
        .enumerate()
        .map(|(i, track)| {
            let mut params = codec_parameters(*track);
            if let Some(channels) = audio_channels(audio_streams, track) {
                params.channels = Some(channels);
            }
            StreamInfo {
                index: i as u32,
                time_base: PES_TIME_BASE,
                duration: None,
                start_time: None,
                params,
            }
        })
        .collect();
    if let (Some(index), Some((width, height))) = (video_index, video_size) {
        stream_infos[index].params.width = Some(width);
        stream_infos[index].params.height = Some(height);
    }

    // Write to a sibling `.partial` file and rename only once the trailer is
    // in place, so a crash never leaves a half-written file that a media
    // server might index.
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|source| DiscError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
    }
    let temp = partial_path(out);
    let mut guard = PartialFile::new(temp.clone());

    let out_file = File::create(&temp).map_err(|source| DiscError::Io {
        path: temp.clone(),
        source,
    })?;
    let writer: Box<dyn WriteSeek> = Box::new(CountingWriter::new(
        BufWriter::new(out_file),
        Arc::clone(&written_bytes),
    ));
    let mut muxer = MkvMuxer::new_matroska(writer, &stream_infos)
        .map_err(|e| DiscError::Remux(format!("mux init: {e}")))?;
    muxer
        .with_duration_finalization()
        .map_err(|e| DiscError::Remux(format!("duration finalization: {e}")))?;
    // DVD is anamorphic: keep the disc's display aspect ratio so a 720x480
    // 4:3 title does not play back stretched as 3:2.
    if let (Some(index), Some((num, den))) = (video_index, video_dar) {
        muxer
            .set_video_geometry(index, MkvVideoGeometry::aspect_ratio(num, den))
            .map_err(|e| DiscError::Remux(format!("set video aspect ratio: {e}")))?;
    }

    // Chapter timeline for the selected range, rebased to zero.
    let mut cursor_ns = 0u64;
    for chapter_number in first..=last {
        let index = usize::from(chapter_number - 1);
        let duration = title.chapter_durations[index];
        let end_ns = cursor_ns + duration.as_nanos() as u64;
        muxer
            .add_chapter(cursor_ns, Some(end_ns), format!("Chapter {chapter_number}"))
            .map_err(|e| DiscError::Remux(format!("add chapter: {e}")))?;
        cursor_ns = end_ns;
    }

    muxer
        .write_header()
        .map_err(|e| DiscError::Remux(format!("write header: {e}")))?;

    // Pass 2: reassemble each elementary-stream frame and mux it. Matroska
    // requires one Block per frame, but a DVD video picture spans several PES
    // packets and an AC-3 / DTS frame can span PES packets too, so the
    // assemblers buffer fragments until a complete unit is available.
    let mut anchor_pts: Option<u64> = None;
    let mut anchor_dts: Option<u64> = None;
    let mut report = RemuxReport {
        chapters: last - first + 1,
        ..Default::default()
    };

    let mut assemblers: Vec<Assembler> = tracks.iter().map(|t| Assembler::for_track(*t)).collect();
    let mut units: Vec<Unit> = Vec::new();

    let mut on_sectors = |sectors: u64| {
        read_bytes = read_bytes.saturating_add(sectors.saturating_mul(SECTOR as u64));
        progress(RemuxProgress {
            phase: RemuxPhase::Muxing,
            bytes_done: read_bytes,
            bytes_total,
            written_bytes: written_bytes.load(Ordering::Relaxed),
        });
    };

    for_each_pes(
        reader,
        vts,
        &selected,
        &mut |pes| {
            let Some(track) = Track::from_pes(&pes) else {
                return Ok(());
            };
            let stream_index = tracks.iter().position(|t| *t == track).ok_or_else(|| {
                // Reached only when the probe was cut short on the IFO's
                // word: the title carries a stream that list left out. Say
                // so, and let the caller read the title out properly.
                if optimistic_probe {
                    *probe_incomplete = true;
                }
                DiscError::Remux("stream missing from probe pass".into())
            })? as u32;

            // AC-3 / DTS PES packets carry a substream header whose
            // FirstAccUnit field points at the frame the PES timestamp
            // describes. Read it before stripping so the timestamp lands on
            // the right frame; when it is absent the timestamp belongs to a
            // frame carried over from an earlier packet and is dropped.
            let mut pts_offset = None;
            let mut suppress_pts = false;
            if matches!(track, Track::Ac3(_) | Track::Dts(_)) {
                if let Ok(header) = AudioSubstreamHeader::parse(pes.payload) {
                    match header.access_unit_offset() {
                        Some(offset) => pts_offset = Some(offset - AUDIO_SUBSTREAM_HEADER_LEN),
                        None => suppress_pts = true,
                    }
                }
            }
            let pts = if suppress_pts { None } else { pes.pts };
            let dts = if suppress_pts { None } else { pes.dts };

            let mut data = pes.payload.to_vec();
            // private_stream_1 payloads begin with a DVD substream header: a
            // single selector byte for subpictures, but a full 4-byte header
            // (selector + FrmCnt + FirstAccUnit) for AC-3 / DTS / LPCM. LPCM
            // adds a further audio-pack header. Strip them so the muxer sees
            // the clean elementary stream.
            if pes.stream_id == SC_PRIVATE_STREAM_1 && !data.is_empty() {
                let strip = match track {
                    Track::Ac3(_) | Track::Dts(_) => AUDIO_SUBSTREAM_HEADER_LEN,
                    Track::Lpcm(_) => LPCM_HEADER_LEN,
                    Track::Subpicture(_) => 1,
                    Track::Video => 0,
                };
                data.drain(..strip.min(data.len()));
            }

            units.clear();
            assemblers[stream_index as usize].push(&data, pts, dts, pts_offset, &mut units);
            write_units(
                &mut muxer,
                stream_index,
                &mut units,
                &mut anchor_pts,
                &mut anchor_dts,
                &mut report,
            )?;
            Ok(())
        },
        &mut on_sectors,
        &mut unreadable,
        WalkFlags { stop: None, cancel },
    )?;

    // Flush the final frame each assembler is still holding.
    for (stream_index, assembler) in assemblers.iter_mut().enumerate() {
        units.clear();
        assembler.finish(&mut units);
        write_units(
            &mut muxer,
            stream_index as u32,
            &mut units,
            &mut anchor_pts,
            &mut anchor_dts,
            &mut report,
        )?;
    }

    muxer
        .write_trailer()
        .map_err(|e| DiscError::Remux(format!("write trailer: {e}")))?;
    drop(muxer);

    replace_file(&temp, out)?;
    guard.defuse();

    report.unreadable_sectors = unreadable;
    Ok(report)
}

/// `out` with `.partial` appended.
pub(crate) fn partial_path(out: &Path) -> PathBuf {
    let mut os = out.as_os_str().to_owned();
    os.push(".partial");
    PathBuf::from(os)
}

/// Rename over an existing file, working around Windows' refusal to do so.
pub(crate) fn replace_file(from: &Path, to: &Path) -> Result<(), DiscError> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(source) => {
            if to.exists() {
                std::fs::remove_file(to).map_err(|source| DiscError::Io {
                    path: to.to_path_buf(),
                    source,
                })?;
                std::fs::rename(from, to).map_err(|source| DiscError::Io {
                    path: to.to_path_buf(),
                    source,
                })
            } else {
                Err(DiscError::Io {
                    path: to.to_path_buf(),
                    source,
                })
            }
        }
    }
}

/// Deletes a half-written file unless defused.
pub(crate) struct PartialFile {
    path: PathBuf,
    armed: bool,
}

impl PartialFile {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    pub(crate) fn defuse(&mut self) {
        self.armed = false;
    }
}

impl Drop for PartialFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Collect the chapters (1-based `first..=last`) with their numbers.
fn chapters_in_range(title: &DvdTitle, first: u16, last: u16) -> Vec<(u16, DvdChapter)> {
    title
        .chapters
        .iter()
        .enumerate()
        .map(|(i, ch)| ((i + 1) as u16, ch.clone()))
        .filter(|(number, _)| *number >= first && *number <= last)
        .collect()
}

/// The elementary streams the IFO declares for the selected chapters.
///
/// A PGC's stream-control table (`PGC_AST_CTL` / `PGC_SPST_CTL`) states which
/// streams the title carries, which is what lets the probe pass stop as soon as
/// it has seen them instead of reading the whole title to find out. The VTS's
/// audio attributes give each physical stream's coding mode, so a declared
/// stream can be named exactly as the probe names the ones it finds.
///
/// An empty result means the IFO cannot be used as the standard for this title
/// and the caller must read it out. That is what a coding mode this code cannot
/// name gets, because guessing would put the wrong codec on a track.
fn declared_tracks(vts: &VtsIfo, chapters: &[(u16, DvdChapter)]) -> Vec<Track> {
    let mut tracks = Vec::new();
    let audio_streams = &vts.mat.title_attributes.audio_streams;

    for (_, chapter) in chapters {
        let Some(pgc) = vts.pgcs.get(usize::from(chapter.pgcn).wrapping_sub(1)) else {
            return Vec::new();
        };
        if !declared_from_controls(
            &pgc.audio_stream_control,
            &pgc.subpicture_stream_control,
            audio_streams,
            &mut tracks,
        ) {
            return Vec::new();
        }
    }

    // A DVD-Video title carries video, and the probe names every video stream
    // `Track::Video`, so it is declared once.
    tracks.push(Track::Video);
    tracks.sort();
    tracks.dedup();
    tracks
}

/// Add the streams one PGC declares, naming each as [`Track::from_pes`] would.
///
/// `false` means a stream could not be named, so the declaration cannot be used
/// as the standard the probe is held to.
fn declared_from_controls(
    audio: &[AudioStreamControl],
    subpictures: &[SubpictureStreamControl],
    audio_streams: &[AudioAttributes],
    out: &mut Vec<Track>,
) -> bool {
    for control in audio.iter().filter(|control| control.available) {
        // The control table maps a logical stream onto the physical substream a
        // demuxer sees, and the VTS's audio attributes are indexed by that
        // physical number.
        let index = control.stream_number;
        match audio_streams.get(usize::from(index)).map(|a| a.coding_mode) {
            Some(AudioCodingMode::Ac3) => out.push(Track::Ac3(index)),
            Some(AudioCodingMode::Dts) => out.push(Track::Dts(index)),
            Some(AudioCodingMode::Lpcm) => out.push(Track::Lpcm(index)),
            _ => return false,
        }
    }

    for control in subpictures.iter().filter(|control| control.available) {
        // A logical sub-picture stream resolves to a different physical
        // substream per display mode, and the VOB carries whichever the player
        // asks for, so every mode's substream is declared. Modes commonly share
        // one substream, which is one track rather than four.
        let mut modes = vec![
            control.stream_4x3,
            control.stream_wide,
            control.stream_letterbox,
            control.stream_pan_scan,
        ];
        modes.sort_unstable();
        modes.dedup();
        for substream in modes {
            out.push(Track::Subpicture(substream));
        }
    }

    true
}

/// Elementary stream carried by a DVD title.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Track {
    Video,
    Ac3(u8),
    Dts(u8),
    Lpcm(u8),
    Subpicture(u8),
}

impl Track {
    fn codec_id(self) -> CodecId {
        match self {
            Track::Video => CodecId::new("mpeg2video"),
            Track::Ac3(_) => CodecId::new("ac3"),
            Track::Dts(_) => CodecId::new("dts"),
            Track::Lpcm(_) => CodecId::new("pcm_s16be"),
            Track::Subpicture(_) => CodecId::new("dvd_subtitle"),
        }
    }

    fn from_pes(pes: &PesPacket<'_>) -> Option<Self> {
        match pes.stream_id {
            0xE0..=0xEF => Some(Track::Video),
            SC_PRIVATE_STREAM_1 => pes.dvd_substream().map(|s| match s {
                DvdSubstream::Ac3(_) => Track::Ac3(s.track()),
                DvdSubstream::Dts(_) => Track::Dts(s.track()),
                DvdSubstream::Lpcm(_) => Track::Lpcm(s.track()),
                DvdSubstream::Subpicture(_) => Track::Subpicture(s.track()),
            }),
            _ => None,
        }
    }
}

/// MPEG-2 picture start code (`00 00 01 00`) — the first byte of a new access
/// unit (picture) in the elementary stream.
const PICTURE_START: &[u8; 4] = b"\x00\x00\x01\x00";
/// MPEG-2 sequence-header start code.
const SEQUENCE_HEADER: &[u8; 4] = b"\x00\x00\x01\xB3";
/// MPEG-2 group-of-pictures header start code.
const GOP_HEADER: &[u8; 4] = b"\x00\x00\x01\xB8";
/// MPEG-2 extension start code (the low nibble of the next byte selects which).
const EXTENSION_START: &[u8; 4] = b"\x00\x00\x01\xB5";

/// A complete elementary-stream unit ready for the Matroska muxer. Matroska's
/// `SimpleBlock` holds exactly one codec frame, so fragments are reassembled
/// before they reach the muxer.
struct Unit {
    data: Vec<u8>,
    pts: Option<u64>,
    dts: Option<u64>,
    /// Frame duration in the PES 90 kHz time base, when it can be derived.
    duration: Option<i64>,
    keyframe: bool,
}

/// Per-track reassembler that turns the PES fragments of one elementary
/// stream into whole frames.
enum Assembler {
    Video(VideoAssembler),
    Audio(AudioFrameAssembler),
    /// LPCM and subpictures are already frame-delimited per PES packet.
    PerPes,
}

impl Assembler {
    fn for_track(track: Track) -> Self {
        match track {
            Track::Video => Self::Video(VideoAssembler::default()),
            Track::Ac3(_) | Track::Dts(_) => Self::Audio(AudioFrameAssembler::default()),
            Track::Lpcm(_) | Track::Subpicture(_) => Self::PerPes,
        }
    }

    /// Feed one PES payload (already stripped of its DVD substream header).
    /// `pts_offset` is the byte offset within `data` of the frame the PES
    /// timestamp refers to (audio only).
    fn push(
        &mut self,
        data: &[u8],
        pts: Option<u64>,
        dts: Option<u64>,
        pts_offset: Option<usize>,
        out: &mut Vec<Unit>,
    ) {
        match self {
            Self::Video(assembler) => assembler.push(data, pts, dts, out),
            Self::Audio(assembler) => assembler.push(data, pts, dts, pts_offset, out),
            Self::PerPes => out.push(Unit {
                data: data.to_vec(),
                pts,
                dts,
                duration: None,
                keyframe: true,
            }),
        }
    }

    /// Flush the trailing partial unit, if any.
    fn finish(&mut self, out: &mut Vec<Unit>) {
        match self {
            Self::Video(assembler) => assembler.finish(out),
            Self::Audio(_) | Self::PerPes => {}
        }
    }
}

/// Buffers MPEG-2 video PES payloads and emits one [`Unit`] per picture.
///
/// A DVD video PES packet carries at most one pack (~2 KB), so a picture is
/// split across several PES packets. Splitting those fragments into separate
/// Matroska blocks breaks decoders (each block is treated as a frame, so
/// pictures arrive truncated); this reassembles them at picture boundaries and
/// carries the picture's PTS/DTS and duration forward.
#[derive(Default)]
struct VideoAssembler {
    buf: Vec<u8>,
    /// Bytes of `buf` already scanned for a picture start code.
    scanned: usize,
    /// Whether the pending unit already contains its picture start code.
    has_picture: bool,
    pts: Option<u64>,
    /// Coded frame rate (`num`/`den` fps) from the latest sequence header.
    frame_rate: Option<(u64, u64)>,
    /// Access units of the GOP being reassembled. A GOP is buffered so display
    /// timestamps can be reconstructed in display order, which differs from
    /// the decode order the packets are stored in.
    gop: Vec<GopPicture>,
    /// Display clock (90 kHz) left at the end of the previous GOP.
    running_clock: Option<u64>,
}

/// One reassembled video picture, awaiting its GOP's display-order timestamps.
struct GopPicture {
    data: Vec<u8>,
    /// 10-bit `temporal_reference` — the picture's display order in the GOP.
    temporal_reference: u16,
    /// Presentation duration in 90 kHz ticks.
    duration: i64,
    /// Source PTS, when the PES packet carried one (normally the GOP's first
    /// picture only).
    pts: Option<u64>,
    keyframe: bool,
    /// Whether this unit carries a sequence or GOP header, i.e. starts a GOP.
    is_gop_start: bool,
}

impl VideoAssembler {
    fn push(&mut self, data: &[u8], pts: Option<u64>, _dts: Option<u64>, out: &mut Vec<Unit>) {
        self.buf.extend_from_slice(data);
        // Rescan a few bytes back so a start code split across PES packets is
        // still found.
        let mut scan = self.scanned.saturating_sub(3);
        let mut next_pts = pts;
        loop {
            let Some(pos) = find_next_start_code(&self.buf, scan) else {
                break;
            };
            let code = self.buf[pos + 3];
            // A sequence header, GOP header, or picture start begins a new
            // access unit. Sequence/GOP headers belong to the *following*
            // picture, so the unit is cut at that header rather than at the
            // picture start code it precedes.
            if self.has_picture && matches!(code, 0x00 | 0xB3 | 0xB8) {
                let picture = self.take_picture(pos);
                self.accumulate(picture, out);
                self.has_picture = false;
                self.pts = None;
                scan = 0;
                continue;
            }
            // Take the picture's timestamp from the PES packet that carries
            // its picture start code.
            if code == 0x00 && !self.has_picture {
                self.has_picture = true;
                self.pts = next_pts.take();
            }
            scan = pos + 4;
        }
        self.scanned = self.buf.len();
    }

    fn finish(&mut self, out: &mut Vec<Unit>) {
        if !self.buf.is_empty() {
            let end = self.buf.len();
            let picture = self.take_picture(end);
            self.accumulate(picture, out);
        }
        self.flush_gop(out);
    }

    /// Build the picture holding `buf[..end]`, refresh the frame rate from any
    /// sequence header it contains, then drop those bytes.
    fn take_picture(&mut self, end: usize) -> GopPicture {
        let data = self.buf[..end].to_vec();
        if let Some(idx) = find_start_code(&data, 0, SEQUENCE_HEADER) {
            if let Ok(header) = SequenceHeader::parse(&data[idx..]) {
                if let Some((num, den)) = header.frame_rate.as_ratio() {
                    self.frame_rate = Some((u64::from(num), u64::from(den)));
                }
            }
        }
        let is_gop_start = find_start_code(&data, 0, SEQUENCE_HEADER).is_some()
            || find_start_code(&data, 0, GOP_HEADER).is_some();
        let temporal_reference = find_start_code(&data, 0, PICTURE_START)
            .and_then(|idx| PictureHeader::parse(&data[idx..]).ok())
            .map(|header| header.temporal_reference)
            .unwrap_or(0);
        let keyframe = is_intra_picture(&data);
        let duration = picture_duration_ticks(&data, self.frame_rate).unwrap_or(0);
        self.buf.drain(..end);
        GopPicture {
            data,
            temporal_reference,
            duration,
            pts: self.pts,
            keyframe,
            is_gop_start,
        }
    }

    /// Add a picture to the pending GOP, flushing the previous GOP when a new
    /// one starts.
    fn accumulate(&mut self, picture: GopPicture, out: &mut Vec<Unit>) {
        if picture.is_gop_start && !self.gop.is_empty() {
            self.flush_gop(out);
        }
        self.gop.push(picture);
    }

    /// Assign each picture the presentation time of its display-order
    /// position, then emit the GOP in the decode order the pictures arrived
    /// in.
    fn flush_gop(&mut self, out: &mut Vec<Unit>) {
        if self.gop.is_empty() {
            return;
        }
        // The GOP's first picture carries the source PTS; a GOP without one
        // continues the clock left by the previous GOP.
        let anchor = self.gop.iter().find_map(|picture| picture.pts);
        let mut display_start = anchor.or(self.running_clock);
        // Open GOPs are common on DVD: the anchored (I) picture can have a
        // non-zero `temporal_reference`, and the B-pictures that display
        // before it belong to this GOP's timeline but precede its PTS.
        if let (Some(anchor_pts), Some(anchor_picture)) = (
            anchor,
            self.gop.iter().find(|picture| picture.pts.is_some()),
        ) {
            let anchor_tr = anchor_picture.temporal_reference;
            let preceding: i64 = self
                .gop
                .iter()
                .filter(|picture| picture.temporal_reference < anchor_tr)
                .map(|picture| picture.duration.max(0))
                .sum();
            display_start = Some(anchor_pts.saturating_sub(preceding as u64));
        }
        let clock = display_start;
        let mut next_clock = clock;
        let mut assigned = vec![None; self.gop.len()];
        if let Some(mut cursor) = clock {
            let mut order: Vec<usize> = (0..self.gop.len()).collect();
            order.sort_by_key(|&i| self.gop[i].temporal_reference);
            for &i in &order {
                assigned[i] = Some(cursor);
                cursor = cursor.saturating_add(self.gop[i].duration.max(0) as u64);
            }
            next_clock = Some(cursor);
        }
        for (i, picture) in self.gop.drain(..).enumerate() {
            out.push(Unit {
                data: picture.data,
                pts: assigned[i],
                // Matroska stores presentation time only; the decoder
                // reorders B-pictures from the bitstream.
                dts: None,
                duration: Some(picture.duration),
                keyframe: picture.keyframe,
            });
        }
        self.running_clock = next_clock;
    }
}

/// Buffers DVD audio payloads and emits one [`Unit`] per AC-3 / DTS frame.
///
/// AC-3 and DTS frames straddle PES-packet boundaries, so muxing each PES
/// payload as its own block would hand the decoder partial frames. The frame
/// size is read from the sync header, and a partial tail is carried into the
/// next packet.
#[derive(Default)]
struct AudioFrameAssembler {
    buf: Vec<u8>,
    /// Absolute elementary-stream offset of `buf[0]`, used to route a PES
    /// timestamp to the frame it actually refers to.
    base_abs: u64,
    /// A pending `(absolute_offset, pts, dts)` waiting for its frame.
    pending: Option<(u64, Option<u64>, Option<u64>)>,
}

impl AudioFrameAssembler {
    fn push(
        &mut self,
        data: &[u8],
        pts: Option<u64>,
        dts: Option<u64>,
        pts_offset: Option<usize>,
        out: &mut Vec<Unit>,
    ) {
        let append_abs = self.base_abs + self.buf.len() as u64;
        self.buf.extend_from_slice(data);
        if let Some(offset) = pts_offset {
            self.pending = Some((append_abs + offset as u64, pts, dts));
        }
        loop {
            let len = self.buf.len();
            if len < 2 {
                break;
            }
            let frame_abs = self.base_abs;
            if self.buf[0] == 0x0B && self.buf[1] == 0x77 {
                if len < 7 {
                    break;
                }
                let header = match Ac3Header::parse(&self.buf) {
                    Ok(header) => header,
                    Err(_) => {
                        self.resync();
                        continue;
                    }
                };
                let Some(size) = header.frame_size_bytes() else {
                    break;
                };
                let size = size as usize;
                if len < size {
                    break;
                }
                // AC-3 sync frames carry 1536 samples.
                let duration = header
                    .sample_rate_hz()
                    .map(|hz| 1_536 * 90_000 / i64::from(hz));
                let frame: Vec<u8> = self.buf.drain(..size).collect();
                self.base_abs += size as u64;
                let (pts, dts) = self.take_pending(frame_abs);
                out.push(Unit {
                    data: frame,
                    pts,
                    dts,
                    duration,
                    keyframe: true,
                });
            } else if len >= 4 && self.buf[0..4] == [0x7F, 0xFE, 0x80, 0x01] {
                let header = match DtsHeader::parse(&self.buf) {
                    Ok(header) => header,
                    Err(_) => {
                        self.resync();
                        continue;
                    }
                };
                let size = header.frame_size_bytes() as usize;
                if size == 0 {
                    self.resync();
                    continue;
                }
                if len < size {
                    break;
                }
                let duration = header
                    .sample_rate_hz()
                    .map(|hz| i64::from(header.sample_count()) * 90_000 / i64::from(hz));
                let frame: Vec<u8> = self.buf.drain(..size).collect();
                self.base_abs += size as u64;
                let (pts, dts) = self.take_pending(frame_abs);
                out.push(Unit {
                    data: frame,
                    pts,
                    dts,
                    duration,
                    keyframe: true,
                });
            } else {
                // Not at a sync word: skip to the next AC-3 / DTS sync.
                self.resync();
                if self.buf.len() == len {
                    break;
                }
            }
        }
    }

    /// Timestamps for the frame starting at `frame_abs`, if a PES timestamp
    /// was aimed at it.
    fn take_pending(&mut self, frame_abs: u64) -> (Option<u64>, Option<u64>) {
        if let Some((target, pts, dts)) = self.pending {
            if target == frame_abs {
                self.pending = None;
                return (pts, dts);
            }
            // The target frame was consumed by a resync: drop the stale value.
            if target < frame_abs {
                self.pending = None;
            }
        }
        (None, None)
    }

    /// Advance to the next AC-3 or DTS sync word, keeping a short tail so a
    /// sync split across pushes is not lost.
    fn resync(&mut self) {
        let mut pos = 1;
        while pos + 2 <= self.buf.len() {
            let ac3 = self.buf[pos] == 0x0B && self.buf[pos + 1] == 0x77;
            let dts =
                pos + 4 <= self.buf.len() && self.buf[pos..pos + 4] == [0x7F, 0xFE, 0x80, 0x01];
            if ac3 || dts {
                self.buf.drain(..pos);
                self.base_abs += pos as u64;
                return;
            }
            pos += 1;
        }
        let keep = self.buf.len().saturating_sub(3);
        self.buf.drain(..keep);
        self.base_abs += keep as u64;
    }
}

/// Write every buffered [`Unit`] to the muxer, rebasing timestamps against the
/// first value seen per stream.
fn write_units(
    muxer: &mut MkvMuxer,
    stream_index: u32,
    units: &mut Vec<Unit>,
    anchor_pts: &mut Option<u64>,
    anchor_dts: &mut Option<u64>,
    report: &mut RemuxReport,
) -> Result<(), DiscError> {
    // Anchor on the earliest presentation time of the first batch so that an
    // open first GOP (whose I-picture carries the PTS but displays after its
    // B-pictures) does not push those B-pictures negative.
    if anchor_pts.is_none() {
        if let Some(first) = units.iter().filter_map(|unit| unit.pts).min() {
            *anchor_pts = Some(first);
        }
    }
    for unit in units.drain(..) {
        let rebase = |value: u64, anchor: &mut Option<u64>| {
            let base = *anchor.get_or_insert(value);
            value.saturating_sub(base) as i64
        };
        let pts = unit.pts.map(|p| rebase(p, anchor_pts));
        let dts = unit.dts.map(|d| rebase(d, anchor_dts));
        let mut flags = PacketFlags::default();
        flags.keyframe = unit.keyframe;
        let packet = Packet {
            stream_index,
            time_base: PES_TIME_BASE,
            pts,
            dts,
            duration: unit.duration,
            flags,
            data: unit.data,
        };
        report.packets += 1;
        report.payload_bytes += packet.data.len() as u64;
        muxer
            .write_packet(&packet)
            .map_err(|e| DiscError::Remux(format!("write packet: {e}")))?;
    }
    Ok(())
}

/// Display aspect ratio implied by an MPEG-2 sequence header's aspect-ratio
/// code. DVD only authors 4:3 and 16:9; the square/forbidden/reserved codes
/// leave the geometry unset so the muxer's pixel-derived default applies.
fn display_aspect_ratio(code: AspectRatioCode) -> Option<(u64, u64)> {
    match code {
        AspectRatioCode::Ratio4x3 => Some((4, 3)),
        AspectRatioCode::Ratio16x9 => Some((16, 9)),
        AspectRatioCode::Ratio221x1 => Some((221, 100)),
        AspectRatioCode::Forbidden | AspectRatioCode::Square | AspectRatioCode::Reserved(_) => None,
    }
}

/// First occurrence of `code` at or after `from`.
fn find_start_code(buf: &[u8], from: usize, code: &[u8; 4]) -> Option<usize> {
    let rest = buf.get(from..)?;
    if rest.len() < 4 {
        return None;
    }
    rest.windows(4)
        .position(|window| window == code)
        .map(|offset| from + offset)
}

/// First MPEG-2 start code (`00 00 01 xx`) at or after `from`, of any kind.
fn find_next_start_code(buf: &[u8], from: usize) -> Option<usize> {
    let rest = buf.get(from..)?;
    if rest.len() < 4 {
        return None;
    }
    rest.windows(4)
        .position(|window| window[0] == 0 && window[1] == 0 && window[2] == 1)
        .map(|offset| from + offset)
}

/// Whether the access unit begins with an intra-coded (I) picture.
fn is_intra_picture(data: &[u8]) -> bool {
    find_start_code(data, 0, PICTURE_START)
        .and_then(|idx| PictureHeader::parse(&data[idx..]).ok())
        .map(|header| header.coding_type.is_intra())
        .unwrap_or(false)
}

/// Offset of the picture-coding extension (`00 00 01 B5`, ext-id `1000`).
fn find_picture_coding_extension(data: &[u8]) -> Option<usize> {
    let mut from = 0;
    while let Some(idx) = find_start_code(data, from, EXTENSION_START) {
        if data.get(idx + 4).map(|byte| byte >> 4) == Some(0b1000) {
            return Some(idx);
        }
        from = idx + 4;
    }
    None
}

/// Duration of one picture in 90 kHz ticks, using the `repeat_first_field`
/// 3:2-pulldown flag so telecined DVD video keeps its true frame cadence.
fn picture_duration_ticks(data: &[u8], frame_rate: Option<(u64, u64)>) -> Option<i64> {
    let (num, den) = frame_rate.unwrap_or((30_000, 1_001));
    if num == 0 {
        return None;
    }
    let fields = match find_picture_coding_extension(data)
        .and_then(|idx| PictureCodingExtension::parse(&data[idx..]).ok())
    {
        Some(ext) => match ext.picture_structure {
            PictureStructure::TopField | PictureStructure::BottomField => 1,
            _ => 2 + u64::from(ext.repeat_first_field),
        },
        None => 2,
    };
    // One field is `1 / (2 * fps)` seconds, i.e. `45000 * den / num` ticks.
    Some((fields * 45_000 * den / num) as i64)
}

fn codec_parameters(track: Track) -> CodecParameters {
    match track {
        Track::Video => {
            let mut params = CodecParameters::video(track.codec_id());
            // DVD-Video mainstream default. The real geometry is in the MPEG-2
            // sequence header; we don't decode, so NTSC 720x480 is a safe
            // placeholder that players correct from the stream.
            params.width = Some(720);
            params.height = Some(480);
            params
        }
        Track::Ac3(_) | Track::Dts(_) | Track::Lpcm(_) => {
            let mut params = CodecParameters::audio(track.codec_id());
            params.sample_rate = Some(48_000);
            // Fallback only; the real count is filled in from the title set's
            // IFO stream attributes, which matters for 5.1 discs where a stereo
            // label can make a player skip a proper downmix.
            params.channels = Some(2);
            params
        }
        Track::Subpicture(_) => CodecParameters::subtitle(track.codec_id()),
    }
}

/// Channel count for an audio track from the title set's IFO stream
/// attributes. DVD audio streams are numbered in the same order as their VOB
/// substreams, so the local track index selects the attribute slot. `None`
/// means the IFO did not carry the attributes and the caller should keep its
/// default.
fn audio_channels(audio_streams: &[AudioAttributes], track: &Track) -> Option<u16> {
    let index = match track {
        Track::Ac3(index) | Track::Dts(index) | Track::Lpcm(index) => usize::from(*index),
        Track::Video | Track::Subpicture(_) => return None,
    };
    audio_streams
        .get(index)
        .map(|attributes| u16::from(attributes.channel_count))
        .filter(|channels| *channels > 0)
}

/// How a walk should end, beyond reaching its last sector.
///
/// The two flags are named apart rather than passed as two bare options because
/// they sit next to each other in every call: transposing them would silently
/// turn a clean stop into an abandoned rip, or the reverse.
#[derive(Clone, Copy, Default)]
struct WalkFlags<'a> {
    /// The caller has everything it needs, so the rest of the walk is not worth
    /// reading. Unlike cancelling, stopping leaves a usable result.
    stop: Option<&'a AtomicBool>,
    /// The caller has abandoned the work.
    cancel: Option<&'a AtomicBool>,
}

/// Whether the caller has asked a walk to stop early.
///
/// Distinct from cancellation: stopping means the result so far is the one that
/// was wanted, so the walk ends cleanly. Cancelling abandons the work.
fn stopped(stop: Option<&AtomicBool>) -> bool {
    stop.is_some_and(|stop| stop.load(Ordering::Relaxed))
}

/// Whether the probe has seen every stream the IFO declared, while still
/// trusting the IFO at all.
///
/// Once this holds nothing later in the title can change the stream set, so the
/// rest of it does not have to be read a second time. Neither half may be
/// dropped: a set that is merely *so far* complete stops early on a disc whose
/// streams are all at the front, and one that ignores trust stops early on a
/// disc that has already contradicted its own IFO.
fn probe_confirmed(trustworthy: bool, declared: &[Track], discovered: &[Track]) -> bool {
    trustworthy
        && declared
            .iter()
            .all(|declared| discovered.contains(declared))
}

/// Call `f` for every PES packet across the selected chapters. `on_sectors` is
/// invoked with the number of sectors just read, so callers can track progress.
/// Unreadable sectors are skipped and counted in `unreadable`.
/// Returns [`DiscError::Cancelled`] as soon as `cancel` is set.
fn for_each_pes<R: Read + Seek>(
    reader: &mut R,
    vts: &VtsIfo,
    chapters: &[(u16, DvdChapter)],
    f: &mut dyn FnMut(PesPacket<'_>) -> Result<(), DiscError>,
    on_sectors: &mut dyn FnMut(u64),
    unreadable: &mut u64,
    flags: WalkFlags<'_>,
) -> Result<(), DiscError> {
    for (number, chapter) in chapters {
        if cancelled(flags.cancel) {
            return Err(DiscError::Cancelled);
        }
        if stopped(flags.stop) {
            return Ok(());
        }
        let pgc = vts
            .pgcs
            .get(usize::from(chapter.pgcn).wrapping_sub(1))
            .ok_or_else(|| DiscError::Remux(format!("chapter {number} PGCN out of range")))?;
        for cell_no in chapter.start_cell..=chapter.end_cell {
            let cell = pgc
                .cells
                .get(usize::from(cell_no).wrapping_sub(1))
                .ok_or_else(|| DiscError::Remux(format!("cell {cell_no} out of range")))?;
            walk_sectors(
                reader,
                cell.first_vobu_start_sector,
                cell.last_vobu_end_sector,
                f,
                on_sectors,
                unreadable,
                flags,
            )?;
        }
    }
    Ok(())
}

/// Sectors spanned by the selected chapters, used as the denominator for
/// progress. Cells are counted exactly as [`for_each_pes`] walks them.
fn count_sectors(vts: &VtsIfo, chapters: &[(u16, DvdChapter)]) -> u64 {
    let mut total = 0u64;
    for (_, chapter) in chapters {
        let Some(pgc) = vts.pgcs.get(usize::from(chapter.pgcn).wrapping_sub(1)) else {
            continue;
        };
        for cell_no in chapter.start_cell..=chapter.end_cell {
            if let Some(cell) = pgc.cells.get(usize::from(cell_no).wrapping_sub(1)) {
                if cell.last_vobu_end_sector >= cell.first_vobu_start_sector {
                    total +=
                        u64::from(cell.last_vobu_end_sector - cell.first_vobu_start_sector) + 1;
                }
            }
        }
    }
    total
}

/// How long to wait after the `n`th consecutive "not ready" answer.
///
/// Short at first, so a drive that is ready almost immediately is not held back
/// by a sleep far longer than the wait it was covering, and backing off so a
/// drive that is genuinely spinning up is not hammered with commands while it
/// works. A flat wait cannot be both: at a rate low enough to be negligible per
/// occurrence, a rip that hits the answer repeatedly loses more to sleeping
/// than to the drive.
fn not_ready_delay(attempts: u32) -> std::time::Duration {
    NOT_READY_DELAY_MIN
        .saturating_mul(1u32 << attempts.min(NOT_READY_BACKOFF_STEPS))
        .min(NOT_READY_DELAY_MAX)
}

/// Read one chain-relative sector, retrying the drive's transient read errors.
///
/// Each attempt re-seeks first: a failed read can leave the previous reader
/// (device or mount) at an undefined offset, and a fresh seek is what coaxes a
/// marginal sector out of the drive. Cancellation is honoured between
/// attempts, so a stubborn sector cannot make the rip ignore the stop key.
///
/// `Ok(true)` means the sector was read, `Ok(false)` that the drive refused it
/// even after retries (the caller skips it rather than failing the rip), and
/// `Err` that the reader could not seek or the caller cancelled.
fn read_sector_with_retries<R: Read + Seek>(
    reader: &mut R,
    sector: u64,
    buffer: &mut [u8],
    cancel: Option<&AtomicBool>,
) -> Result<bool, DiscError> {
    let mut failures = 0;
    let mut not_ready = 0;

    loop {
        if cancelled(cancel) {
            return Err(DiscError::Cancelled);
        }
        reader
            .seek(SeekFrom::Start(sector * SECTOR as u64))
            .map_err(|e| DiscError::Remux(format!("seek sector {sector}: {e}")))?;

        meter::attempt(buffer.len());
        match reader.read_exact(buffer) {
            Ok(()) => return Ok(true),
            // A drive that is not ready yet is a delay, not damage. Waiting it
            // out is what stops a slow spin-up from being recorded as a run of
            // unreadable sectors.
            Err(e) if e.kind() == io::ErrorKind::WouldBlock && not_ready < NOT_READY_ATTEMPTS => {
                not_ready += 1;
                meter::not_ready();
                report_read_failure(sector, "not ready", &e);
                std::thread::sleep(not_ready_delay(not_ready));
            }
            Err(e) => {
                failures += 1;
                meter::failed();
                report_read_failure(sector, "failed", &e);
                if failures >= SECTOR_READ_ATTEMPTS {
                    return Ok(false);
                }
            }
        }
    }
}

/// Read `count` consecutive chain-relative sectors in one request.
///
/// `Ok(false)` means the drive refused the span, and the caller falls back to
/// reading its sectors one at a time so the retry and skip policy judges each
/// on its own. "Not ready" is waited out in here, because that is the drive
/// asking for time rather than reporting damage.
fn read_window<R: Read + Seek>(
    reader: &mut R,
    first: u64,
    count: usize,
    buffer: &mut [u8],
    cancel: Option<&AtomicBool>,
) -> Result<bool, DiscError> {
    debug_assert_eq!(buffer.len(), count * SECTOR);
    let mut not_ready = 0;

    loop {
        if cancelled(cancel) {
            return Err(DiscError::Cancelled);
        }
        reader
            .seek(SeekFrom::Start(first * SECTOR as u64))
            .map_err(|e| DiscError::Remux(format!("seek sector {first}: {e}")))?;

        meter::attempt(buffer.len());
        match reader.read_exact(buffer) {
            Ok(()) => return Ok(true),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock && not_ready < NOT_READY_ATTEMPTS => {
                not_ready += 1;
                meter::not_ready();
                std::thread::sleep(not_ready_delay(not_ready));
            }
            // A refused window is routine: the caller reads these sectors
            // individually and reports the failure from there.
            Err(_) => return Ok(false),
        }
    }
}

/// Counters for the requests a rip makes of the drive.
///
/// The number that matters is the mean request size. A drive answers one
/// command in roughly a millisecond whatever its size, so a request per 2 KiB
/// sector pins a rip near 2 MB/s however fast the media is, while a windowed
/// read lets the media rate through. Reporting it says which of those is
/// happening, from the rip itself rather than by inference.
mod meter {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// How much reading accumulates between reports.
    const INTERVAL: Duration = Duration::from_secs(5);

    #[derive(Default)]
    struct Totals {
        /// When the window being measured started; `None` before the first
        /// request.
        started: Option<Instant>,
        requests: u64,
        bytes: u64,
        not_ready: u64,
        failures: u64,
    }

    static TOTALS: Mutex<Option<Totals>> = Mutex::new(None);

    fn with<R>(f: impl FnOnce(&mut Totals) -> R) -> R {
        let mut guard = TOTALS.lock().unwrap_or_else(|e| e.into_inner());
        f(guard.get_or_insert_with(Totals::default))
    }

    /// Record one request handed to the drive, reporting the rate once enough
    /// has accumulated.
    pub(super) fn attempt(bytes: usize) {
        let line = with(|t| {
            let started = *t.started.get_or_insert_with(Instant::now);
            t.requests += 1;
            t.bytes += bytes as u64;

            let elapsed = started.elapsed();
            if elapsed < INTERVAL {
                return None;
            }
            let line = summary(
                t.requests,
                t.bytes,
                t.not_ready,
                t.failures,
                elapsed.as_secs_f64(),
            );
            *t = Totals {
                started: Some(Instant::now()),
                ..Totals::default()
            };
            Some(line)
        });
        if let Some(line) = line {
            eprintln!("{line}");
        }
    }

    /// The line describing what has been read so far.
    ///
    /// Split out from the recording so the arithmetic — in particular the mean
    /// request size that says whether windowing is working — can be checked
    /// without waiting for a report to fall due.
    pub(super) fn summary(
        requests: u64,
        bytes: u64,
        not_ready: u64,
        failures: u64,
        seconds: f64,
    ) -> String {
        let mib = bytes as f64 / (1024.0 * 1024.0);
        format!(
            "packrat: read {mib:.1} MiB in {seconds:.1} s ({:.1} MiB/s) over {requests} requests, \
             mean {} B/request, {not_ready} not-ready waits, {failures} read failures",
            mib / seconds,
            bytes / requests.max(1),
        )
    }

    /// Record a "not ready" answer that had to be waited out.
    pub(super) fn not_ready() {
        with(|t| t.not_ready += 1);
    }

    /// Record a read the drive refused.
    pub(super) fn failed() {
        with(|t| t.failures += 1);
    }
}

/// Report a read failure on stderr, a bounded number of times.
///
/// A rip that crawls and a badly scratched disc look identical in the counters,
/// so say what the drive actually answered. Bounded because a damaged run can
/// produce thousands of these.
fn report_read_failure(sector: u64, what: &str, error: &io::Error) {
    use std::sync::atomic::{AtomicU64, Ordering};

    const FIRST: u64 = 10;
    const EVERY: u64 = 200;
    static SEEN: AtomicU64 = AtomicU64::new(0);

    let seen = SEEN.fetch_add(1, Ordering::Relaxed);
    if seen < FIRST {
        eprintln!("packrat: sector {sector} {what}: {error}");
    } else if seen % EVERY == 0 {
        eprintln!("packrat: {seen} read failures so far; latest sector {sector} {what}: {error}");
    }
}

/// Walk a contiguous chain-relative sector range, calling `f` for each PES.
///
/// Sectors the drive cannot read are skipped and counted in `unreadable`, so a
/// few bad spots on an otherwise sound disc still yield a complete file. A long
/// damaged run is crossed by skipping ahead, which keeps a badly scratched disc
/// from spending minutes of drive error-recovery on every sector of the run.
///
/// Healthy stretches are read a [`READ_WINDOW_SECTORS`]-sector window at a
/// time. Asking the drive for one 2 KiB sector per request runs the rip at the
/// drive's command rate rather than its media rate, which is what holds a DVD
/// near 2 MB/s. A window the drive refuses is dropped and its sectors are read
/// one at a time instead, so the retry and skip policy below still judges an
/// individual sector rather than condemning a whole window.
fn walk_sectors<R: Read + Seek>(
    reader: &mut R,
    first: u32,
    last: u32,
    f: &mut dyn FnMut(PesPacket<'_>) -> Result<(), DiscError>,
    on_sectors: &mut dyn FnMut(u64),
    unreadable: &mut u64,
    flags: WalkFlags<'_>,
) -> Result<(), DiscError> {
    if last < first {
        return Ok(());
    }
    let count = u64::from(last - first) + 1;
    let mut buffer = vec![0u8; SECTOR];
    let mut window = vec![0u8; READ_WINDOW_SECTORS as usize * SECTOR];
    // Chain-relative first sector the window holds. A `window_len` of zero
    // means it holds nothing and the next sector has to be fetched.
    let mut window_first = 0u64;
    let mut window_len = 0u64;
    let mut consecutive_failures = 0u32;
    let mut malformed = 0u64;
    let mut skip_remaining = 0u64;
    let mut skip_step = SKIP_MIN;

    for offset in 0..count {
        if cancelled(flags.cancel) {
            return Err(DiscError::Cancelled);
        }
        if stopped(flags.stop) {
            return Ok(());
        }
        let sector = u64::from(first) + offset;

        if skip_remaining > 0 {
            // Inside a run already known to be damaged: advance without asking
            // the drive about this sector.
            skip_remaining -= 1;
            *unreadable += 1;
            on_sectors(1);
            continue;
        }

        // Only read ahead while the walk is healthy; once the drive starts
        // refusing sectors every one goes through `read_sector_with_retries`.
        let mut in_window =
            window_len > 0 && sector >= window_first && sector < window_first + window_len;
        if !in_window && consecutive_failures == 0 {
            let want = (count - offset).min(READ_WINDOW_SECTORS) as usize;
            if read_window(
                reader,
                sector,
                want,
                &mut window[..want * SECTOR],
                flags.cancel,
            )? {
                window_first = sector;
                window_len = want as u64;
                in_window = true;
            } else {
                window_len = 0;
            }
        }

        let read = if in_window {
            let at = (sector - window_first) as usize * SECTOR;
            buffer.copy_from_slice(&window[at..at + SECTOR]);
            true
        } else {
            read_sector_with_retries(reader, sector, &mut buffer, flags.cancel)?
        };
        on_sectors(1);
        if !read {
            *unreadable += 1;
            consecutive_failures += 1;
            if consecutive_failures >= SKIP_CONSECUTIVE_FAILURES {
                skip_remaining = skip_step;
                skip_step = (skip_step * 2).min(SKIP_MAX);
            }
            continue;
        }
        consecutive_failures = 0;
        skip_step = SKIP_MIN;

        if let Err(error) = for_each_pack_pes(&buffer, f) {
            // One pack that will not parse costs a few frames, not the film, so
            // it is skipped and counted the way an unreadable sector is. A run
            // of them is not a bad pack but the wrong sectors entirely, which no
            // amount of skipping fixes, so past a limit the error stands.
            malformed += 1;
            *unreadable += 1;
            report_malformed(sector, &buffer, &error);
            if malformed > MALFORMED_IN_CELL_MAX {
                return Err(DiscError::Remux(format!(
                    "sector {sector} is not a readable MPEG pack, and neither were the \
                     {malformed} before it: {error}"
                )));
            }
        }
    }

    Ok(())
}

/// Hand every PES packet of one pack to `f`.
///
/// A pack is one sector and a DVD holds whole PES packets in each, so a pack
/// can be read on its own. A nav pack carries no elementary stream; anything
/// else that cannot be read as MPEG-2 programme stream is handed back to the
/// caller as an error rather than guessed at.
fn for_each_pack_pes(
    sector: &[u8],
    f: &mut dyn FnMut(PesPacket<'_>) -> Result<(), DiscError>,
) -> Result<(), DiscError> {
    if looks_like_nav_pack(sector) {
        // Navigation pack — no elementary stream.
        let _ = NavPack::parse(sector);
        return Ok(());
    }

    let pack =
        PackHeader::parse(sector).map_err(|e| DiscError::Remux(format!("pack header: {e}")))?;
    let mut cursor = PackHeader::SIZE + pack.stuffing_bytes as usize;

    while cursor + 6 <= sector.len() {
        if sector[cursor..cursor + 3] != [0x00, 0x00, 0x01] {
            break;
        }
        let stream_id = sector[cursor + 3];
        if matches!(
            stream_id,
            SC_SYSTEM_HEADER | SC_PADDING_STREAM | SC_PRIVATE_STREAM_2
        ) {
            let len = ((sector[cursor + 4] as usize) << 8) | sector[cursor + 5] as usize;
            cursor += 6 + len;
            continue;
        }

        let wire_size = {
            let pes = PesPacket::parse(&sector[cursor..])
                .map_err(|e| DiscError::Remux(format!("PES: {e}")))?;
            let wire_size = pes.wire_size;
            f(pes)?;
            wire_size
        };
        cursor += wire_size;
    }

    Ok(())
}

/// Report a pack that could not be read, a bounded number of times.
///
/// The bytes matter more than the count: they say whether this is a mangled
/// pack or something else entirely, which is the difference between a disc
/// authored oddly and the wrong sectors being read.
fn report_malformed(sector: u64, bytes: &[u8], error: &DiscError) {
    use std::sync::atomic::{AtomicU64, Ordering};

    const FIRST: u64 = 10;
    const EVERY: u64 = 200;
    static SEEN: AtomicU64 = AtomicU64::new(0);

    let seen = SEEN.fetch_add(1, Ordering::Relaxed);
    if seen >= FIRST && seen % EVERY != 0 {
        return;
    }
    let head: Vec<String> = bytes.iter().take(8).map(|b| format!("{b:02x}")).collect();
    eprintln!(
        "packrat: sector {sector} is not a readable pack ({error}); first bytes {}",
        head.join(" ")
    );
}

/// Presents `VTS_xx_1.VOB`…`VTS_xx_9.VOB` as one contiguous byte stream.
struct VtsChainReader {
    /// `(file, start_offset_in_chain, length)`.
    files: Vec<(File, u64, u64)>,
    pos: u64,
    total: u64,
    index: usize,
}

impl VtsChainReader {
    fn open(video_ts: &Path, vts_number: u8) -> Result<Self, DiscError> {
        let mut files = Vec::new();
        let mut start = 0u64;
        for vob in 1..=9u8 {
            let path = video_ts.join(format!("VTS_{vts_number:02}_{vob}.VOB"));
            if !path.is_file() {
                break;
            }
            let file = File::open(&path).map_err(|source| DiscError::Io {
                path: path.clone(),
                source,
            })?;
            let len = file
                .metadata()
                .map_err(|source| DiscError::Io {
                    path: path.clone(),
                    source,
                })?
                .len();
            files.push((file, start, len));
            start += len;
        }

        if files.is_empty() {
            return Err(DiscError::Remux(format!(
                "title set {vts_number} has no VTS_{vts_number:02}_*.VOB files"
            )));
        }

        Ok(Self {
            files,
            pos: 0,
            total: start,
            index: 0,
        })
    }
}

impl Read for VtsChainReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.total || buf.is_empty() {
            return Ok(0);
        }
        // Locate the file containing `pos`. Cells are not always traversed in
        // ascending order, so this must work for backward seeks too; with at
        // most nine VOBs a linear scan is fine.
        self.index = self
            .files
            .iter()
            .position(|(_, start, len)| self.pos < start + len)
            .unwrap_or(self.files.len() - 1);

        let (start, len) = {
            let entry = &self.files[self.index];
            (entry.1, entry.2)
        };
        let file_offset = self.pos - start;
        let remaining = (len - file_offset) as usize;
        let want = buf.len().min(remaining);

        let read = {
            let file = &mut self.files[self.index].0;
            file.seek(SeekFrom::Start(file_offset))?;
            file.read(&mut buf[..want])?
        };
        self.pos += read as u64;
        Ok(read)
    }
}

impl Seek for VtsChainReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::Current(delta) => self.pos as i64 + delta,
            SeekFrom::End(delta) => self.total as i64 + delta,
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before start of VOB chain",
            ));
        }
        self.pos = target as u64;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sequence header + sequence extension, NTSC 29.97 (`frame_rate_code` 4).
    fn sequence_header() -> Vec<u8> {
        let mut bytes = b"\x00\x00\x01\xB3\x2d\x01\xe0\x24\x16\xf3\x23\x82".to_vec();
        bytes.extend_from_slice(b"\x00\x00\x01\xB5\x14\x82\x00\x01\x00\x00");
        bytes
    }

    /// A minimal access unit: picture header (I-picture, `temporal_reference`),
    /// a picture-coding extension, and one slice.
    fn picture(temporal_reference: u16, repeat_first_field: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x00\x00\x01\x00");
        let b0 = (temporal_reference >> 2) as u8;
        let b1 = (((temporal_reference & 0x3) as u8) << 6) | (1 << 3);
        bytes.extend_from_slice(&[b0, b1, 0, 0]);
        let mut extension = vec![0x00, 0x00, 0x01, 0xB5, 0x80, 0x00, 0x00, 0x00, 0x00];
        if repeat_first_field {
            // `repeat_first_field` is bit 1 of body byte 3 (buffer index 7).
            extension[7] |= 0x02;
        }
        bytes.extend_from_slice(&extension);
        bytes.extend_from_slice(b"\x00\x00\x01\x01\xaa\xbb");
        bytes
    }

    #[test]
    fn picture_duration_uses_repeat_first_field() {
        assert_eq!(picture_duration_ticks(&picture(0, false), None), Some(3003));
        assert_eq!(picture_duration_ticks(&picture(0, true), None), Some(4504));
    }

    #[test]
    fn display_aspect_ratio_maps_dvd_codes() {
        assert_eq!(
            display_aspect_ratio(AspectRatioCode::Ratio4x3),
            Some((4, 3))
        );
        assert_eq!(
            display_aspect_ratio(AspectRatioCode::Ratio16x9),
            Some((16, 9))
        );
        assert_eq!(display_aspect_ratio(AspectRatioCode::Square), None);
        assert_eq!(display_aspect_ratio(AspectRatioCode::Forbidden), None);
    }

    #[test]
    fn counting_writer_tracks_output_bytes() {
        let written = Arc::new(AtomicU64::new(0));
        let mut writer = CountingWriter::new(Vec::new(), Arc::clone(&written));

        writer.write_all(b"hello").unwrap();
        writer.write_all(&[0u8; 10]).unwrap();

        assert_eq!(written.load(Ordering::Relaxed), 15);
    }

    #[test]
    fn audio_channel_count_comes_from_the_ifo() {
        // Byte 1 bits 2..0 hold `channels - 1`, so 5 means 5.1 (6 channels).
        let mut six = [0u8; 8];
        six[1] = 5;
        let streams = [
            AudioAttributes::parse(&six),
            AudioAttributes::parse(&[0u8; 8]),
        ];

        assert_eq!(audio_channels(&streams, &Track::Ac3(0)), Some(6));
        assert_eq!(audio_channels(&streams, &Track::Dts(1)), Some(1));
        // No attribute slot for this stream: the caller keeps its default.
        assert_eq!(audio_channels(&streams, &Track::Ac3(7)), None);
        assert_eq!(audio_channels(&streams, &Track::Video), None);
    }

    #[test]
    fn a_set_cancel_flag_stops_walking_sectors() {
        let cancel = AtomicBool::new(true);
        let mut reader = std::io::Cursor::new(Vec::<u8>::new());
        let mut pes = |_pes: PesPacket<'_>| -> Result<(), DiscError> { Ok(()) };
        let mut sectors = |_sectors: u64| {};
        let mut unreadable = 0u64;

        let result = walk_sectors(
            &mut reader,
            0,
            10,
            &mut pes,
            &mut sectors,
            &mut unreadable,
            WalkFlags {
                stop: None,
                cancel: Some(&cancel),
            },
        );

        assert!(matches!(result, Err(DiscError::Cancelled)));
    }

    #[test]
    fn walking_sectors_without_a_cancel_flag_is_not_cancelled() {
        let cancel = AtomicBool::new(false);
        let mut reader = std::io::Cursor::new(Vec::<u8>::new());
        let mut pes = |_pes: PesPacket<'_>| -> Result<(), DiscError> { Ok(()) };
        let mut sectors = |_sectors: u64| {};
        let mut unreadable = 0u64;

        // Nothing is cancelled, so the empty cursor's unreadable sector is
        // skipped and counted instead of aborting.
        let result = walk_sectors(
            &mut reader,
            0,
            0,
            &mut pes,
            &mut sectors,
            &mut unreadable,
            WalkFlags {
                stop: None,
                cancel: Some(&cancel),
            },
        );

        assert!(result.is_ok());
        assert_eq!(unreadable, 1);
    }

    /// A reader that fails a fixed number of times before serving its data, so
    /// the retry path can be exercised without a real marginal disc.
    struct FlakyReader {
        data: Vec<u8>,
        remaining_failures: u32,
        pos: u64,
    }

    impl std::io::Read for FlakyReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.remaining_failures > 0 {
                self.remaining_failures -= 1;
                return Err(std::io::Error::other("medium error"));
            }
            let start = self.pos as usize;
            if start >= self.data.len() {
                return Ok(0);
            }
            let n = buf.len().min(self.data.len() - start);
            buf[..n].copy_from_slice(&self.data[start..start + n]);
            self.pos += n as u64;
            Ok(n)
        }
    }

    impl std::io::Seek for FlakyReader {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            let target = match pos {
                SeekFrom::Start(p) => p as i64,
                SeekFrom::Current(delta) => self.pos as i64 + delta,
                SeekFrom::End(delta) => self.data.len() as i64 + delta,
            };
            self.pos = target.max(0) as u64;
            Ok(self.pos)
        }
    }

    /// The "not ready" wait has to start short and back off: a flat wait either
    /// costs far more than the delay it covers, or hammers a drive that is
    /// still spinning up.
    #[test]
    fn the_not_ready_wait_starts_short_and_backs_off() {
        use std::time::Duration;

        assert_eq!(not_ready_delay(1), Duration::from_millis(4));
        assert_eq!(not_ready_delay(2), Duration::from_millis(8));
        // It stops growing at the cap rather than running away.
        assert_eq!(not_ready_delay(5), NOT_READY_DELAY_MAX);
        assert_eq!(not_ready_delay(100), NOT_READY_DELAY_MAX);
        assert!(
            not_ready_delay(1) < not_ready_delay(3),
            "the wait grows with consecutive answers"
        );
    }

    /// A sector the drive fails once or twice must still be read: the retry
    /// re-seeks and tries again rather than failing the whole rip.
    #[test]
    fn a_transient_read_error_is_retried_in_place() {
        let mut data = vec![0u8; 2 * SECTOR];
        data[SECTOR..SECTOR + 4].copy_from_slice(b"\x00\x00\x01\xba");
        let mut reader = FlakyReader {
            data,
            remaining_failures: SECTOR_READ_ATTEMPTS - 1,
            pos: 0,
        };
        let mut buffer = vec![0u8; SECTOR];

        let read = read_sector_with_retries(&mut reader, 1, &mut buffer, None)
            .expect("recovers after retries");

        assert!(read);
        assert_eq!(&buffer[..4], b"\x00\x00\x01\xba");
    }

    /// A sector that never reads is reported as skipped, not as a fatal error,
    /// so one bad spot cannot abort the whole rip.
    #[test]
    fn a_persistent_read_error_is_skipped() {
        let mut reader = FlakyReader {
            data: vec![0u8; SECTOR],
            remaining_failures: SECTOR_READ_ATTEMPTS,
            pos: 0,
        };
        let mut buffer = vec![0u8; SECTOR];

        let read = read_sector_with_retries(&mut reader, 42, &mut buffer, None)
            .expect("a failed read is not fatal");

        assert!(!read, "the sector is reported unreadable");
    }

    /// Fails every read and counts the attempts, so skipping can be told apart
    /// from probing each sector of a damaged run.
    struct CountingFailures {
        attempts: u32,
        pos: u64,
    }

    impl std::io::Read for CountingFailures {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            self.attempts += 1;
            Err(std::io::Error::other("medium error"))
        }
    }

    impl std::io::Seek for CountingFailures {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            let target = match pos {
                SeekFrom::Start(p) => p as i64,
                SeekFrom::Current(delta) => self.pos as i64 + delta,
                SeekFrom::End(delta) => delta,
            };
            self.pos = target.max(0) as u64;
            Ok(self.pos)
        }
    }

    /// A long damaged run must be crossed by skipping, not by spending the
    /// drive's error-recovery timeout on all 300 sectors.
    #[test]
    fn a_damaged_run_is_crossed_by_skipping() {
        let mut reader = CountingFailures {
            attempts: 0,
            pos: 0,
        };
        let mut pes = |_pes: PesPacket<'_>| -> Result<(), DiscError> { Ok(()) };
        let mut sectors = |_sectors: u64| {};
        let mut unreadable = 0u64;

        walk_sectors(
            &mut reader,
            0,
            299,
            &mut pes,
            &mut sectors,
            &mut unreadable,
            WalkFlags::default(),
        )
        .expect("a damaged run does not abort the walk");

        assert_eq!(unreadable, 300, "every sector of the run is accounted for");
        // Probing all 300 sectors would be 900 read attempts; skipping crosses
        // them in a few dozen.
        assert!(
            reader.attempts < 100,
            "walk probed {} times; expected skipping",
            reader.attempts
        );
    }

    #[test]
    fn video_assembler_reassembles_fragmented_pictures() {
        let mut stream = sequence_header();
        stream.extend_from_slice(&picture(0, true));
        stream.extend_from_slice(&picture(1, false));

        let mut assembler = VideoAssembler::default();
        let mut units = Vec::new();
        // One byte per "PES packet" forces start codes to straddle boundaries.
        for byte in &stream {
            assembler.push(&[*byte], None, None, &mut units);
        }
        assembler.finish(&mut units);

        assert_eq!(units.len(), 2, "one block per picture");
        assert_eq!(&units[0].data[0..4], b"\x00\x00\x01\xB3");
        assert_eq!(&units[1].data[0..4], b"\x00\x00\x01\x00");
    }

    #[test]
    fn video_assembler_orders_open_gop_timestamps() {
        let mut assembler = VideoAssembler::default();
        let mut units = Vec::new();

        // Open GOP: the I-picture is decoded first (and carries the PTS) but
        // has temporal_reference 2; B-pictures 0 and 1 display before it.
        assembler.push(&sequence_header(), None, None, &mut units);
        assembler.push(&picture(2, false), Some(90_000), None, &mut units);
        assembler.push(&picture(0, false), None, None, &mut units);
        assembler.push(&picture(1, false), None, None, &mut units);
        assembler.finish(&mut units);

        assert_eq!(units.len(), 3);
        // Decode order is I(tr=2), B(tr=0), B(tr=1); display order is 0, 1, 2.
        assert_eq!(units[0].pts, Some(90_000));
        assert_eq!(units[1].pts, Some(90_000 - 2 * 3_003));
        assert_eq!(units[2].pts, Some(90_000 - 3_003));
    }

    #[test]
    fn audio_assembler_reassembles_frames() {
        // Real AC-3 sync frame header: 48 kHz, 448 kbps => 1792 bytes/frame.
        let mut frame = vec![0u8; 1792];
        frame[0..8].copy_from_slice(&[0x0B, 0x77, 0x40, 0x2F, 0x1E, 0x30, 0xE3, 0xFF]);

        let mut assembler = AudioFrameAssembler::default();
        let mut units = Vec::new();
        assembler.push(&frame[..1000], Some(48_000), None, Some(0), &mut units);
        assert!(units.is_empty(), "a partial frame must be buffered");
        assembler.push(&frame[1000..], None, None, None, &mut units);

        assert_eq!(units.len(), 1);
        assert_eq!(units[0].data, frame);
        assert_eq!(units[0].pts, Some(48_000));
        assert_eq!(units[0].duration, Some(2880));
    }

    /// A sector the walk recognises as a navigation pack, so it routes nowhere
    /// and parsing it cannot fail. Lets a walk be exercised without fabricating
    /// a whole elementary stream.
    fn nav_pack() -> Vec<u8> {
        let mut sector = vec![0u8; SECTOR];
        sector[0..4].copy_from_slice(&[0x00, 0x00, 0x01, 0xBA]);
        sector[0x0E..0x12].copy_from_slice(&[0x00, 0x00, 0x01, 0xBB]);
        sector[0x26..0x2A].copy_from_slice(&[0x00, 0x00, 0x01, 0xBF]);
        sector[0x400..0x404].copy_from_slice(&[0x00, 0x00, 0x01, 0xBF]);
        sector[0x406] = 0x01;
        sector
    }

    /// A pack whose header is well formed but whose payload is video
    /// elementary stream rather than the PES packet a pack should start with.
    /// This is the shape of the `byte-6 top bits != 10` failure: a real
    /// `00 00 01` followed by a picture start code, which is not a PES header.
    fn malformed_pack() -> Vec<u8> {
        let mut sector = vec![0u8; SECTOR];
        // A valid MPEG-2 pack header: start code, marker bits, a non-zero
        // mux_rate, no stuffing.
        sector[0..4].copy_from_slice(&[0x00, 0x00, 0x01, 0xBA]);
        sector[4] = 0x44;
        sector[6] = 0x04;
        sector[8] = 0x04;
        sector[9] = 0x01;
        sector[11] = 0x01;
        sector[12] = 0x03;
        // Then a picture start code where a PES header belongs.
        sector[14..18].copy_from_slice(&[0x00, 0x00, 0x01, 0x00]);
        sector
    }

    /// The pack reader says what it found rather than guessing: a nav pack
    /// holds no packets, and a payload that is not a PES header is an error.
    #[test]
    fn the_pack_reader_reports_what_it_found() {
        let mut packets = 0;
        for_each_pack_pes(&nav_pack(), &mut |_pes: PesPacket<'_>| {
            packets += 1;
            Ok(())
        })
        .expect("a nav pack");
        assert_eq!(packets, 0);

        let error = for_each_pack_pes(&malformed_pack(), &mut |_pes: PesPacket<'_>| Ok(()))
            .expect_err("a pack that does not start with a PES packet");
        assert!(error.to_string().contains("MPEG-1 framing"), "{error}");
    }

    /// A reader that serves `pack` for every sector it is asked for.
    fn repeating(pack: &[u8], sectors: usize) -> FlakyReader {
        let mut data = Vec::with_capacity(pack.len() * sectors);
        for _ in 0..sectors {
            data.extend_from_slice(pack);
        }
        FlakyReader {
            data,
            remaining_failures: 0,
            pos: 0,
        }
    }

    /// A pack that will not parse costs a few frames, not the whole film, so it
    /// is skipped and counted the way an unreadable sector is.
    #[test]
    fn a_pack_that_will_not_parse_is_skipped() {
        let mut reader = repeating(&malformed_pack(), 10);
        let mut pes = |_pes: PesPacket<'_>| -> Result<(), DiscError> { Ok(()) };
        let mut walked = 0u64;
        let mut sectors = |n: u64| walked += n;
        let mut unreadable = 0u64;

        walk_sectors(
            &mut reader,
            0,
            9,
            &mut pes,
            &mut sectors,
            &mut unreadable,
            WalkFlags::default(),
        )
        .expect("one bad pack is not fatal");

        assert_eq!(walked, 10, "a skipped pack is still walked past");
        assert_eq!(unreadable, 10, "and counted as missing from the output");
    }

    /// A whole cell of packs that will not parse is not a bad pack but the
    /// wrong sectors, and reporting success would be a lie.
    #[test]
    fn a_cell_of_packs_that_will_not_parse_is_an_error() {
        let mut reader = repeating(&malformed_pack(), 200);
        let mut pes = |_pes: PesPacket<'_>| -> Result<(), DiscError> { Ok(()) };
        let mut sectors = |_n: u64| {};
        let mut unreadable = 0u64;

        let result = walk_sectors(
            &mut reader,
            0,
            199,
            &mut pes,
            &mut sectors,
            &mut unreadable,
            WalkFlags::default(),
        );

        let error = result.expect_err("past the limit the error stands");
        assert!(
            error.to_string().contains("not a readable MPEG pack"),
            "{error}"
        );
    }

    /// Serves navigation packs and records the size of every request it is
    /// given, so the number of device requests a walk makes can be counted.
    struct RequestCounter {
        data: Vec<u8>,
        pos: u64,
        requests: Vec<usize>,
    }

    impl RequestCounter {
        fn new(sectors: usize) -> Self {
            let pack = nav_pack();
            let mut data = Vec::with_capacity(sectors * SECTOR);
            for _ in 0..sectors {
                data.extend_from_slice(&pack);
            }
            Self {
                data,
                pos: 0,
                requests: Vec::new(),
            }
        }
    }

    impl std::io::Read for RequestCounter {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.requests.push(buf.len());
            let start = self.pos as usize;
            if start >= self.data.len() {
                return Ok(0);
            }
            let n = buf.len().min(self.data.len() - start);
            buf[..n].copy_from_slice(&self.data[start..start + n]);
            self.pos += n as u64;
            Ok(n)
        }
    }

    impl std::io::Seek for RequestCounter {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            let target = match pos {
                SeekFrom::Start(p) => p as i64,
                SeekFrom::Current(delta) => self.pos as i64 + delta,
                SeekFrom::End(delta) => self.data.len() as i64 + delta,
            };
            self.pos = target.max(0) as u64;
            Ok(self.pos)
        }
    }

    /// A healthy walk asks the drive for a window at a time, not a sector at a
    /// time. The request count is what decides whether a rip runs at the
    /// drive's command rate or at its media rate, so this is the regression
    /// that matters most.
    #[test]
    fn a_healthy_walk_reads_a_window_per_request() {
        let window = READ_WINDOW_SECTORS as usize;
        // Two whole windows and a part, so the last request is a short one.
        let sectors = window * 2 + 5;
        let mut reader = RequestCounter::new(sectors);
        let mut pes = |_pes: PesPacket<'_>| -> Result<(), DiscError> { Ok(()) };
        let mut sectors_cb = |_sectors: u64| {};
        let mut unreadable = 0u64;

        walk_sectors(
            &mut reader,
            0,
            sectors as u32 - 1,
            &mut pes,
            &mut sectors_cb,
            &mut unreadable,
            WalkFlags::default(),
        )
        .expect("walk");

        assert_eq!(unreadable, 0);
        // One request per window, not one per sector.
        assert_eq!(
            reader.requests,
            vec![window * SECTOR, window * SECTOR, 5 * SECTOR],
        );
    }

    /// A window the drive refuses must not cost the whole span: its sectors are
    /// read one at a time, so the retry and skip policy still judges each on
    /// its own and only the bad one is written off.
    #[test]
    fn a_refused_window_falls_back_to_reading_sectors_one_at_a_time() {
        /// Refuses anything larger than a sector, as a drive with damage in
        /// every window would, and fails one nominated sector.
        struct WindowRefusing {
            pack: Vec<u8>,
            pos: u64,
            bad_sector: u64,
        }

        impl std::io::Read for WindowRefusing {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if buf.len() > SECTOR || self.pos / SECTOR as u64 == self.bad_sector {
                    return Err(std::io::Error::other("medium error"));
                }
                let n = buf.len().min(self.pack.len());
                buf[..n].copy_from_slice(&self.pack[..n]);
                self.pos += n as u64;
                Ok(n)
            }
        }

        impl std::io::Seek for WindowRefusing {
            fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
                self.pos = match pos {
                    SeekFrom::Start(p) => p,
                    SeekFrom::Current(delta) => (self.pos as i64 + delta).max(0) as u64,
                    SeekFrom::End(_) => 0,
                };
                Ok(self.pos)
            }
        }

        let mut reader = WindowRefusing {
            pack: nav_pack(),
            pos: 0,
            bad_sector: 5,
        };
        let mut pes = |_pes: PesPacket<'_>| -> Result<(), DiscError> { Ok(()) };
        let mut walked = 0u64;
        let mut sectors = |n: u64| walked += n;
        let mut unreadable = 0u64;

        walk_sectors(
            &mut reader,
            0,
            9,
            &mut pes,
            &mut sectors,
            &mut unreadable,
            WalkFlags::default(),
        )
        .expect("walk");

        // Every sector is accounted for, and only the bad one is written off.
        assert_eq!(walked, 10);
        assert_eq!(unreadable, 1);
    }

    /// The meter is what tells a windowed read apart from a sector-at-a-time
    /// one, so its arithmetic has to be right: a mean request size that is
    /// quietly wrong would send anyone reading it after the wrong cause.
    #[test]
    fn the_read_meter_reports_the_mean_request_size_and_rate() {
        let line = meter::summary(32, 32 * 64 * 1024, 0, 0, 4.0);

        // 2 MiB over 32 requests in 4 s is 64 KiB per request at 0.5 MiB/s.
        assert!(line.contains("2.0 MiB in 4.0 s (0.5 MiB/s)"), "{line}");
        assert!(
            line.contains("over 32 requests, mean 65536 B/request"),
            "{line}"
        );
    }

    /// An audio-attribute field carrying `coding_mode`, the 3-bit selector in
    /// byte 0 bits 7..5 (0 = AC-3, 4 = LPCM, 6 = DTS).
    fn attribute(coding_mode: u8) -> AudioAttributes {
        AudioAttributes::parse(&[coding_mode << 5, 0, 0, 0, 0, 0, 0, 0])
    }

    /// Every coding mode the remuxer can name has to come out of the IFO as the
    /// same track the probe would name, or the two sets can never be compared.
    #[test]
    fn declared_audio_streams_are_named_as_the_probe_names_them() {
        let mut controls = [AudioStreamControl {
            available: false,
            stream_number: 0,
        }; 8];
        controls[0] = AudioStreamControl {
            available: true,
            stream_number: 0,
        };
        // Stream 1 is unavailable even though the title's slot maps it to 2.
        controls[1] = AudioStreamControl {
            available: false,
            stream_number: 2,
        };
        controls[2] = AudioStreamControl {
            available: true,
            stream_number: 2,
        };
        let attributes = [attribute(0), attribute(4), attribute(6)];
        let mut out = Vec::new();

        assert!(declared_from_controls(
            &controls,
            &[],
            &attributes,
            &mut out
        ));
        assert_eq!(out, vec![Track::Ac3(0), Track::Dts(2)]);
    }

    /// A sub-picture stream resolves to a different physical substream per
    /// display mode and the VOB carries whichever the player asks for, so every
    /// mode's substream is declared.
    #[test]
    fn declared_subpicture_streams_cover_every_display_mode() {
        let mut controls = [SubpictureStreamControl {
            available: false,
            stream_4x3: 0,
            stream_wide: 0,
            stream_letterbox: 0,
            stream_pan_scan: 0,
        }; 32];
        controls[0] = SubpictureStreamControl {
            available: true,
            stream_4x3: 1,
            stream_wide: 1,
            stream_letterbox: 2,
            stream_pan_scan: 2,
        };
        let mut out = Vec::new();

        assert!(declared_from_controls(&[], &controls, &[], &mut out));
        assert_eq!(out, vec![Track::Subpicture(1), Track::Subpicture(2)]);
    }

    /// A coding mode this remuxer cannot name must make the IFO unusable as a
    /// standard, rather than put a guessed codec on a track.
    #[test]
    fn an_unnamable_coding_mode_makes_the_declaration_unusable() {
        let controls = [AudioStreamControl {
            available: true,
            stream_number: 1,
        }; 8];
        // Byte 0 bits 7..5 = 1 is MPEG-1 audio, which this remuxer drops.
        let attributes = [attribute(0), attribute(1)];
        let mut out = Vec::new();

        assert!(!declared_from_controls(
            &controls,
            &[],
            &attributes,
            &mut out
        ));
    }

    /// The probe may only stop on a set that is both complete against the IFO
    /// and still trusted: either half alone stops early on a disc that deserves
    /// a full read.
    #[test]
    fn the_probe_stops_only_on_a_confirmed_and_trustworthy_stream_set() {
        let declared = [Track::Video, Track::Ac3(0)];
        let seen = [Track::Ac3(0), Track::Video];

        assert!(probe_confirmed(true, &declared, &seen));
        // A declared stream is still missing, so keep reading.
        assert!(!probe_confirmed(true, &declared, &[Track::Video]));
        // The title carried an undeclared stream, so the IFO cannot be trusted
        // to say when the set is complete.
        assert!(!probe_confirmed(false, &declared, &seen));
    }

    /// Stopping is not cancelling: the walk ends where it is told to and leaves
    /// a usable result, which is what lets the probe stop short and still mux.
    #[test]
    fn a_stop_flag_ends_the_walk_without_an_error() {
        let stop = AtomicBool::new(false);
        let mut reader = RequestCounter::new(100);
        let mut pes = |_pes: PesPacket<'_>| -> Result<(), DiscError> { Ok(()) };
        let mut walked = 0u64;
        let mut sectors = |n: u64| {
            walked += n;
            if walked == 5 {
                stop.store(true, Ordering::Relaxed);
            }
        };
        let mut unreadable = 0u64;

        walk_sectors(
            &mut reader,
            0,
            99,
            &mut pes,
            &mut sectors,
            &mut unreadable,
            WalkFlags {
                stop: Some(&stop),
                cancel: None,
            },
        )
        .expect("a stopped walk is not an error");

        assert_eq!(walked, 5, "the walk ends where it was told to");
    }

    /// A drive answering "not ready" is a delay, not damage: it must be waited
    /// out rather than recorded as an unreadable sector. This is the answer a
    /// drive gives while it spins up.
    #[test]
    fn a_not_ready_drive_is_waited_out_rather_than_written_off() {
        /// Fills any request with navigation packs, after answering
        /// `WouldBlock` a fixed number of times.
        struct NotReady {
            pos: u64,
            remaining: u32,
        }

        impl std::io::Read for NotReady {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.remaining > 0 {
                    self.remaining -= 1;
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "drive not ready",
                    ));
                }
                let pack = nav_pack();
                for chunk in buf.chunks_mut(SECTOR) {
                    chunk.copy_from_slice(&pack[..chunk.len()]);
                }
                self.pos += buf.len() as u64;
                Ok(buf.len())
            }
        }

        impl std::io::Seek for NotReady {
            fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
                self.pos = match pos {
                    SeekFrom::Start(p) => p,
                    SeekFrom::Current(delta) => (self.pos as i64 + delta).max(0) as u64,
                    SeekFrom::End(_) => 0,
                };
                Ok(self.pos)
            }
        }

        let mut reader = NotReady {
            pos: 0,
            remaining: 2,
        };
        let mut pes = |_pes: PesPacket<'_>| -> Result<(), DiscError> { Ok(()) };
        let mut walked = 0u64;
        let mut sectors = |n: u64| walked += n;
        let mut unreadable = 0u64;

        walk_sectors(
            &mut reader,
            0,
            1,
            &mut pes,
            &mut sectors,
            &mut unreadable,
            WalkFlags::default(),
        )
        .expect("walk");

        assert_eq!(walked, 2);
        assert_eq!(unreadable, 0, "a not-ready drive is not damaged media");
    }
}

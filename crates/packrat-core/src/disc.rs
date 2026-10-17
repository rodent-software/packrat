//! Reading a disc's title/chapter structure out of its `VIDEO_TS` IFO files.
//!
//! The IFO bytes can come from a mounted `VIDEO_TS` directory, or — when the
//! desktop never mounted the disc — straight off the raw device, whose ISO
//! 9660/UDF directory is not encrypted. The `oxideav-dvd` IFO parsers are handed
//! the individual file byte buffers either way.

use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use oxideav_dvd::udf::UdfVolume;
use oxideav_dvd::{DvdDisc, DvdFile, DvdFileKind, TtSrpt, VmgIfo, VtsIfo};

use crate::error::DiscError;
use crate::source::DiscSource;

/// DVD logical sector size.
const SECTOR: usize = 2048;

/// How long to wait for a drive that is not ready, mirroring the remux path.
const NOT_READY_ATTEMPTS: u32 = 100;
const NOT_READY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

/// Times to retry a structure the drive refused to read.
///
/// The same reasoning as the VOB path: an optical drive returns `EIO` for
/// sectors it will answer for a moment later, most of all on the first read
/// after a disc goes in. Waiting out only "not ready" and giving up on the
/// first `EIO` is how a scan comes to need pressing two or three times.
const READ_ATTEMPTS: u32 = 5;
const READ_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(120);

/// The largest structure file a real disc carries.
///
/// Only `IFO`/`.BUP` files are read through [`read_extent`], and those are
/// hundreds of kilobytes at worst. The figure exists because a file's size comes
/// off the disc's own filesystem — the very thing a damaged or protected disc
/// misreads — and an unchecked one is an allocation, and then a read, of
/// whatever those bytes happen to say. Sixteen megabytes is far above any real
/// `IFO` and far below anything that would hurt the machine.
const MAX_STRUCTURE_BYTES: u64 = 16 * 1024 * 1024;

/// Everything we know about a disc before matching it to metadata.
#[derive(Debug, Clone)]
pub struct DiscModel {
    /// Volume label as seen on the mounted filesystem (folders do not carry
    /// the UDF volume id, so this is the mount/directory name).
    pub volume_id: String,
    /// VMGI provider id (authoring house), if present.
    pub provider_id: String,
    /// Number of video title sets (VTS).
    pub vts_count: u16,
    /// Disc-global titles, in TT_SRPT order (1-based `number`).
    pub titles: Vec<Title>,
}

/// One disc-global title.
#[derive(Debug, Clone)]
pub struct Title {
    /// Disc-global title number (1-based) — what players and HandBrake show.
    pub number: u16,
    /// Video title set this title lives in.
    pub vts: u8,
    /// Title number within its VTS.
    pub vts_ttn: u8,
    /// Number of camera angles.
    pub angles: u8,
    /// Number of chapters/parts-of-title.
    pub chapters: u16,
    /// Total PGC playback time, when the VTS could be parsed.
    pub duration: Option<Duration>,
    /// Per-chapter playback time, in chapter order.
    pub chapter_durations: Vec<Duration>,
}

impl Title {
    /// Sum of the per-chapter durations (may differ from `duration` by a few
    /// frames because the PGC header rounds).
    pub fn summed_chapter_duration(&self) -> Duration {
        self.chapter_durations
            .iter()
            .copied()
            .fold(Duration::ZERO, |a, b| a + b)
    }
}

impl DiscModel {
    /// Sum of every title's duration that we could read.
    pub fn total_duration(&self) -> Duration {
        self.titles
            .iter()
            .filter_map(|t| t.duration)
            .fold(Duration::ZERO, |acc, d| acc + d)
    }

    /// Content titles — long enough that they are plausibly video, not menus.
    pub fn content_titles(&self, minimum: Duration) -> Vec<&Title> {
        self.titles
            .iter()
            .filter(|t| t.duration.map(|d| d >= minimum).unwrap_or(false))
            .collect()
    }
}

/// Read the title structure of a disc.
///
/// A mounted disc is read through its `VIDEO_TS` directory. A device with no
/// mount is read through the device's own filesystem, so no automounter is
/// needed. When both are available the mount is tried first: it is the plain
/// file read, and some discs with unusual copy protection enumerate better
/// through it.
pub fn read_disc(source: &DiscSource) -> Result<DiscModel, DiscError> {
    if let DiscSource::BluRay { root, .. } = source {
        return crate::bluray::read_disc(root);
    }

    let mounted = source
        .video_ts()
        .map(|video_ts| read_disc_from_dir(source, video_ts));

    match mounted {
        Some(Ok(model)) => Ok(model),
        Some(Err(mounted_error)) => match source.device() {
            // The mount was tried first and failed, so the device is the
            // fallback — and the reason the device gives is the one worth
            // reporting. Swallowing it used to leave a stale mount point
            // looking like the cause of everything.
            Some(device) => read_disc_from_device(device).map_err(|device_error| {
                DiscError::Remux(format!(
                    "{mounted_error}; reading the device instead: {device_error}"
                ))
            }),
            None => Err(mounted_error),
        },
        None => match source.device() {
            Some(device) => read_disc_from_device(device),
            None => Err(DiscError::NoVideoTs(source.root().display().to_string())),
        },
    }
}

/// Read the structure through a `VIDEO_TS` directory.
fn read_disc_from_dir(source: &DiscSource, video_ts: &Path) -> Result<DiscModel, DiscError> {
    let main_buf = read_ifo_or_backup(video_ts, "VIDEO_TS")
        .ok_or_else(|| DiscError::NoVideoTs(source.root().display().to_string()))??;
    let main_path = video_ts.join("VIDEO_TS.IFO");

    let volume_id = source
        .root()
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    assemble(&main_buf, &main_path, volume_id, |n| {
        read_ifo_or_backup(video_ts, &format!("VTS_{n:02}_0"))
    })
}

/// Read `VIDEO_TS.IFO` or `VTS_xx_0.IFO`, falling back to its `.BUP` copy.
///
/// The disc carries a backup of every `IFO` precisely so that one unreadable
/// spot cannot cost the disc's structure, and the protection that damages the
/// sectors a ripper looks at tends to leave the backup readable. Without this,
/// a single bad sector in a 46-sector file hides every title on the disc.
fn read_ifo_or_backup(video_ts: &Path, stem: &str) -> Option<Result<Vec<u8>, DiscError>> {
    let ifo = video_ts.join(format!("{stem}.IFO"));
    if !ifo.is_file() {
        return None;
    }
    match read_file(&ifo) {
        Ok(bytes) => Some(Ok(bytes)),
        Err(first) => {
            let bup = video_ts.join(format!("{stem}.BUP"));
            if !bup.is_file() {
                return Some(Err(first));
            }
            eprintln!(
                "packrat: {} could not be read ({}); using {}",
                ifo.display(),
                match &first {
                    DiscError::Io { source, .. } => source.to_string(),
                    other => other.to_string(),
                },
                bup.file_name().unwrap_or_default().to_string_lossy()
            );
            Some(read_file(&bup).map_err(|_| first))
        }
    }
}

/// Read the structure straight off the raw device's filesystem.
fn read_disc_from_device(device: &Path) -> Result<DiscModel, DiscError> {
    let mut reader = open_device(device)?;
    let disc = read_dvd_structure(&mut reader, device)?;
    let base = filesystem_base_sector(&mut reader);

    let main_buf = read_extent_or_backup(
        &mut reader,
        device,
        &disc,
        DvdFileKind::Vmgi,
        DvdFileKind::VmgiBup,
        base,
    )
    .ok_or_else(|| DiscError::NoVideoTs(device.display().to_string()))??;

    // The device knows the real UDF volume id, which beats the mount's name.
    assemble(&main_buf, device, disc.volume_id.clone(), |n| {
        read_extent_or_backup(
            &mut reader,
            device,
            &disc,
            DvdFileKind::Vtsi(n),
            DvdFileKind::VtsiBup(n),
            base,
        )
    })
}

/// Read an `IFO` extent, falling back to its `.BUP` copy.
///
/// See [`read_ifo_or_backup`]: the backup is there so that one unreadable spot
/// cannot cost the disc's structure, and it is often placed somewhere the
/// protection did not damage. `VTS_01_0.IFO` failing while `VTS_01_0.BUP` reads
/// is exactly the case this exists for.
fn read_extent_or_backup<R: Read + Seek>(
    reader: &mut R,
    device: &Path,
    disc: &DvdDisc,
    primary: DvdFileKind,
    backup: DvdFileKind,
    base: u64,
) -> Option<Result<Vec<u8>, DiscError>> {
    let find = |kind: DvdFileKind| disc.video_ts_files.iter().find(|file| file.kind == kind);

    let file = find(primary)?;
    Some(match read_extent(reader, device, file, base) {
        Ok(bytes) => Ok(bytes),
        Err(first) => {
            // Reported once, because falling back is a real event: the disc is
            // damaged or protected where a ripper looks.
            eprintln!(
                "packrat: {} could not be read ({first}); using its backup",
                file.name
            );
            match find(backup) {
                Some(backup_file) => {
                    read_extent(reader, device, backup_file, base).map_err(|_| first)
                }
                None => Err(first),
            }
        }
    })
}

/// The absolute sector a disc's file entries are addressed from.
///
/// A UDF file entry carries an *in-partition* block number, so the partition's
/// start sector has to be added to reach the disc. ISO 9660 — the fallback for
/// authoring tools that omit the AVDP — addresses from the start of the disc,
/// so the base is zero.
fn filesystem_base_sector<R: Read + Seek>(reader: &mut R) -> u64 {
    match UdfVolume::open(&mut *reader) {
        Ok(udf) => udf.partition_start_sector,
        Err(_) => 0,
    }
}

/// Parse the disc's file table, mapping a failure to a readable error.
fn read_dvd_structure<R: Read + Seek>(reader: &mut R, device: &Path) -> Result<DvdDisc, DiscError> {
    DvdDisc::from_reader(reader).map_err(|e| DiscError::Ifo {
        path: device.to_path_buf(),
        message: format!("reading the disc filesystem: {e}"),
    })
}

/// Read one file's bytes out of a disc image or device.
///
/// The device is opened `O_NONBLOCK` so probing it cannot close the tray, and
/// that flag applies to these reads too: Linux answers `EAGAIN` while an
/// optical drive spins up or seeks. That is a delay rather than a failure, so
/// it is waited out instead of failing the disc. Each retry re-seeks, because a
/// partial `read_exact` leaves the reader mid-buffer.
fn read_extent<R: Read + Seek>(
    reader: &mut R,
    device: &Path,
    file: &DvdFile,
    base: u64,
) -> Result<Vec<u8>, DiscError> {
    let io = |source| DiscError::Io {
        path: device.to_path_buf(),
        source,
    };
    let start = (base + u64::from(file.lba)) * SECTOR as u64;
    let sector = start / SECTOR as u64;

    // Checked before anything is allocated or asked of the drive. A garbled
    // entry costs the `.BUP` fallback, which is cheap; unchecked, it costs an
    // allocation and a read of whatever number the damaged bytes held.
    if !extent_is_plausible(reader, start, file.size) {
        eprintln!(
            "packrat: {} is listed at sector {sector} as {} bytes, which this disc \
             cannot hold; not reading it",
            file.name, file.size
        );
        return Err(io(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the file table gives {} bytes for {}", file.size, file.name),
        )));
    }

    let mut buf = vec![0u8; usize::try_from(file.size).unwrap_or(0)];
    let mut not_ready = 0u32;
    let mut failures = 0u32;
    let started = Instant::now();

    loop {
        reader.seek(SeekFrom::Start(start)).map_err(io)?;
        match reader.read_exact(&mut buf) {
            Ok(()) => return Ok(buf),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock && not_ready < NOT_READY_ATTEMPTS => {
                not_ready += 1;
                if not_ready == 1 {
                    eprintln!(
                        "packrat: sector {sector} ({}) is not ready yet; waiting for the drive",
                        file.name
                    );
                }
                std::thread::sleep(NOT_READY_DELAY);
            }
            // Ask again before giving up: the drive may be settling, and this
            // is the file table rather than the content, so it is small.
            Err(e) if failures < READ_ATTEMPTS => {
                failures += 1;
                // Every attempt, with the time it has taken so far: each one is
                // a read the kernel may be waiting the drive's full timeout for,
                // so a stall here has to say it is still going rather than go
                // quiet for minutes.
                eprintln!(
                    "packrat: sector {sector} ({}) did not read ({e}); attempt {failures} of \
                     {READ_ATTEMPTS}, {:.1}s so far",
                    file.name,
                    started.elapsed().as_secs_f32()
                );
                std::thread::sleep(READ_RETRY_DELAY);
            }
            Err(source) => {
                eprintln!(
                    "packrat: sector {sector} ({}) would not read after {READ_ATTEMPTS} \
                     attempts ({:.1}s, {source})",
                    file.name,
                    started.elapsed().as_secs_f32()
                );
                return Err(io(source));
            }
        }
    }
}

/// Whether a directory entry describes a file this disc could actually hold.
///
/// Position and size come from the disc's own filesystem, so a disc that
/// misreads its directory misreads these. A file cannot be longer than the disc
/// it sits on, nor a structure file longer than any real `IFO`, so anything
/// outside that is a misread rather than a file — and the caller falls back to
/// the `.BUP` exactly as it does for a read that failed.
fn extent_is_plausible<R: Seek>(reader: &mut R, start: u64, size: u64) -> bool {
    if size > MAX_STRUCTURE_BYTES {
        return false;
    }
    let Ok(device_end) = reader.seek(SeekFrom::End(0)) else {
        // A drive that will not say how big it is decides nothing here; the
        // read itself is the check then.
        return true;
    };
    // Zero is a device that is not reporting a size, not an empty disc: the
    // filesystem has already been read off it by this point.
    device_end == 0 || start.saturating_add(size) <= device_end
}

/// Open a raw optical device for reading, without letting the kernel close its
/// tray.
fn open_device(device: &Path) -> Result<fs::File, DiscError> {
    crate::drives::open_device(device).map_err(|source| DiscError::Io {
        path: device.to_path_buf(),
        source,
    })
}

/// Turn a `VIDEO_TS.IFO` buffer plus a way to fetch each `VTS_xx_0.IFO` into a
/// [`DiscModel`]. Shared by the mounted and device-backed readers.
fn assemble(
    main_buf: &[u8],
    label: &Path,
    volume_id: String,
    mut vts_bytes: impl FnMut(u8) -> Option<Result<Vec<u8>, DiscError>>,
) -> Result<DiscModel, DiscError> {
    let ifo_error = |message: String| DiscError::Ifo {
        path: label.to_path_buf(),
        message,
    };

    let vmg = VmgIfo::parse(main_buf).map_err(|e| ifo_error(e.to_string()))?;

    // TT_SRPT lives at the sector the VMGI_MAT points to.
    let tt_offset = vmg.tt_srpt_sector as usize * SECTOR;
    let tt_buf = main_buf.get(tt_offset..).ok_or_else(|| {
        ifo_error(format!(
            "TT_SRPT sector {} is past end of file",
            vmg.tt_srpt_sector
        ))
    })?;
    let tt = TtSrpt::parse(tt_buf).map_err(|e| ifo_error(format!("TT_SRPT: {e}")))?;

    // Parse each VTS once, so titles can carry chapter detail.
    let mut vts: Vec<Option<VtsIfo>> = vec![None; usize::from(vmg.number_of_title_sets) + 1];
    for n in 1..=vmg.number_of_title_sets {
        let Ok(nu8) = u8::try_from(n) else {
            continue;
        };
        if let Some(bytes) = vts_bytes(nu8) {
            let buf = bytes?;
            let parsed =
                VtsIfo::parse(&buf, nu8).map_err(|e| ifo_error(format!("VTS_{nu8:02}: {e}")))?;
            vts[usize::from(nu8)] = Some(parsed);
        }
    }

    let titles = tt
        .entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let parsed = vts
                .get(usize::from(entry.vts_number))
                .and_then(|o| o.as_ref());
            let (duration, chapter_durations) = match parsed {
                Some(v) => title_times(v, entry.vts_title_number),
                None => (None, Vec::new()),
            };
            Title {
                number: (i + 1) as u16,
                vts: entry.vts_number,
                vts_ttn: entry.vts_title_number,
                angles: entry.angle_count,
                chapters: entry.chapter_count,
                duration,
                chapter_durations,
            }
        })
        .collect();

    Ok(DiscModel {
        volume_id,
        provider_id: vmg.provider_id,
        vts_count: vmg.number_of_title_sets,
        titles,
    })
}

/// Total and per-chapter playback times for a title in a VTS.
///
/// The title total comes from the PGC header (what players and HandBrake
/// report); the per-chapter times are summed from the cells each chapter
/// covers, because chapters have no playback-time field of their own.
fn title_times(vts: &VtsIfo, vts_ttn: u8) -> (Option<Duration>, Vec<Duration>) {
    let Some(title) = vts.titles.iter().find(|t| t.number == vts_ttn) else {
        return (None, Vec::new());
    };

    let mut chapters = Vec::with_capacity(title.chapters.len());
    let mut header_total: Option<u32> = None;
    let mut summed_total = 0u32;

    for (index, chapter) in title.chapters.iter().enumerate() {
        let mut seconds = 0u32;
        if let Some(pgc) = vts.pgcs.get(usize::from(chapter.pgcn).wrapping_sub(1)) {
            if index == 0 {
                header_total = Some(pgc.playback_time.total_seconds());
            }
            let lo = usize::from(chapter.start_cell).saturating_sub(1);
            let hi = usize::from(chapter.end_cell);
            if let Some(cells) = pgc.cells.get(lo..hi) {
                for cell in cells {
                    seconds += cell.playback_time.total_seconds();
                }
            }
        }
        summed_total += seconds;
        chapters.push(Duration::from_secs(u64::from(seconds)));
    }

    let total = header_total.unwrap_or(summed_total);
    (Some(Duration::from_secs(u64::from(total))), chapters)
}

fn read_file(path: &PathBuf) -> Result<Vec<u8>, DiscError> {
    fs::read(path).map_err(|source| DiscError::Io {
        path: path.clone(),
        source,
    })
}

/// Parse one title set's `VTS_xx_0.IFO` (needed for its PGC/cell layout when
/// remuxing).
pub fn read_vts(source: &DiscSource, vts_number: u8) -> Result<VtsIfo, DiscError> {
    if matches!(source, DiscSource::BluRay { .. }) {
        return Err(DiscError::BluRay(
            "Blu-ray titles do not have DVD title sets".into(),
        ));
    }
    let buf = read_vts_bytes(source, vts_number)?;
    VtsIfo::parse(&buf, vts_number).map_err(|e| DiscError::Ifo {
        path: vts_label(source, vts_number),
        message: e.to_string(),
    })
}

/// The bytes of one `VTS_xx_0.IFO`, from the mount when there is one and the
/// device otherwise.
fn read_vts_bytes(source: &DiscSource, vts_number: u8) -> Result<Vec<u8>, DiscError> {
    if let Some(video_ts) = source.video_ts() {
        let stem = format!("VTS_{vts_number:02}_0");
        // The folder gets the same IFO-then-`.BUP` fallback as the raw device:
        // a mounted disc reports a protected sector exactly the same way.
        let mounted = read_ifo_or_backup(video_ts, &stem).unwrap_or_else(|| {
            Err(DiscError::Io {
                path: video_ts.join(format!("{stem}.IFO")),
                source: io::Error::new(io::ErrorKind::NotFound, "no such file"),
            })
        });
        match mounted {
            Ok(buf) => return Ok(buf),
            // A folder that cannot supply the file is not fatal when the device
            // can read the disc's own filesystem.
            Err(mounted_error) => {
                let Some(device) = source.device() else {
                    return Err(mounted_error);
                };
                return read_vts_from_device(device, vts_number).map_err(|_| mounted_error);
            }
        }
    }

    let device = source
        .device()
        .ok_or_else(|| DiscError::NoVideoTs(source.root().display().to_string()))?;
    read_vts_from_device(device, vts_number)
}

fn read_vts_from_device(device: &Path, vts_number: u8) -> Result<Vec<u8>, DiscError> {
    let mut reader = open_device(device)?;
    let disc = read_dvd_structure(&mut reader, device)?;
    let base = filesystem_base_sector(&mut reader);
    // Remuxing needs this title set's PGC/cell layout, so it must survive the
    // same protection as the disc table: the `.BUP` holds the same bytes.
    match read_extent_or_backup(
        &mut reader,
        device,
        &disc,
        DvdFileKind::Vtsi(vts_number),
        DvdFileKind::VtsiBup(vts_number),
        base,
    ) {
        Some(bytes) => bytes,
        None => Err(DiscError::NoVideoTs(device.display().to_string())),
    }
}

fn vts_label(source: &DiscSource, vts_number: u8) -> PathBuf {
    let name = format!("VTS_{vts_number:02}_0.IFO");
    match source.video_ts() {
        Some(video_ts) => video_ts.join(name),
        None => source.root().join(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One sector of `byte`, addressed by the file it belongs to.
    const IFO_LBA: u32 = 100;
    const BUP_LBA: u32 = 200;
    const IFO_BYTE: u8 = 0x11;
    const BUP_BYTE: u8 = 0x22;

    fn file(kind: DvdFileKind, name: &str, lba: u32) -> DvdFile {
        DvdFile {
            kind,
            name: name.to_string(),
            lba,
            size: SECTOR as u64,
            title_set: 1,
            vob_index: 0,
        }
    }

    /// A disc table plus an image of `VTS_01_0.IFO` and its backup.
    fn disc_and_image() -> (DvdDisc, Vec<u8>) {
        let mut image = vec![0u8; 300 * SECTOR];
        image[IFO_LBA as usize * SECTOR..(IFO_LBA as usize + 1) * SECTOR].fill(IFO_BYTE);
        image[BUP_LBA as usize * SECTOR..(BUP_LBA as usize + 1) * SECTOR].fill(BUP_BYTE);
        let disc = DvdDisc {
            volume_id: "TEST".to_string(),
            title_set_count: 1,
            video_ts_files: vec![
                file(DvdFileKind::Vtsi(1), "VTS_01_0.IFO", IFO_LBA),
                file(DvdFileKind::VtsiBup(1), "VTS_01_0.BUP", BUP_LBA),
            ],
            audio_ts_files: Vec::new(),
        };
        (disc, image)
    }

    /// An image whose `damaged` byte range fails every read with `EIO`, the way
    /// a drive answers for a protected sector.
    struct Damaged {
        bytes: Vec<u8>,
        pos: u64,
        damaged: std::ops::Range<u64>,
    }

    impl Damaged {
        fn new(bytes: Vec<u8>, damaged: std::ops::Range<u64>) -> Self {
            Self {
                bytes,
                pos: 0,
                damaged,
            }
        }
    }

    impl Read for Damaged {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let start = self.pos;
            if start < self.damaged.end && start + buf.len() as u64 > self.damaged.start {
                return Err(io::Error::from_raw_os_error(5));
            }
            let from = usize::try_from(start).unwrap_or(usize::MAX);
            let available = self.bytes.len().saturating_sub(from);
            let n = available.min(buf.len());
            buf[..n].copy_from_slice(&self.bytes[from..from + n]);
            self.pos += n as u64;
            Ok(n)
        }
    }

    impl Seek for Damaged {
        fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
            let pos = match from {
                SeekFrom::Start(n) => n as i64,
                SeekFrom::End(n) => self.bytes.len() as i64 + n,
                SeekFrom::Current(n) => self.pos as i64 + n,
            };
            if pos < 0 {
                return Err(io::Error::from_raw_os_error(22));
            }
            self.pos = pos as u64;
            Ok(self.pos)
        }
    }

    fn read_vts_extent(reader: &mut Damaged, disc: &DvdDisc) -> Option<Result<Vec<u8>, DiscError>> {
        read_extent_or_backup(
            reader,
            Path::new("/dev/sr-test"),
            disc,
            DvdFileKind::Vtsi(1),
            DvdFileKind::VtsiBup(1),
            0,
        )
    }

    /// The disc that started this: `VTS_01_0.IFO` unreadable, its backup fine.
    #[test]
    fn a_damaged_ifo_is_read_from_its_backup() {
        let (disc, image) = disc_and_image();
        let damaged = (IFO_LBA as u64 * SECTOR as u64)..((IFO_LBA as u64 + 1) * SECTOR as u64);
        let mut reader = Damaged::new(image, damaged);

        let bytes = read_vts_extent(&mut reader, &disc)
            .expect("the file is in the table")
            .expect("the backup reads");

        assert_eq!(bytes.len(), SECTOR);
        assert!(
            bytes.iter().all(|byte| *byte == BUP_BYTE),
            "the backup's bytes, not the damaged IFO's"
        );
    }

    #[test]
    fn a_readable_ifo_is_preferred_to_its_backup() {
        let (disc, image) = disc_and_image();
        let mut reader = Damaged::new(image, 0..0);

        let bytes = read_vts_extent(&mut reader, &disc)
            .expect("the file is in the table")
            .expect("the IFO reads");

        assert!(bytes.iter().all(|byte| *byte == IFO_BYTE));
    }

    /// Both copies unreadable is the one case worth failing on, and the error
    /// has to describe the primary, not the backup that was tried after it.
    #[test]
    fn damage_to_both_copies_reports_the_primary_failure() {
        let (disc, image) = disc_and_image();
        let damaged = (IFO_LBA as u64 * SECTOR as u64)..((BUP_LBA as u64 + 1) * SECTOR as u64);
        let mut reader = Damaged::new(image, damaged);

        let error = read_vts_extent(&mut reader, &disc)
            .expect("the file is in the table")
            .expect_err("neither copy reads");

        match error {
            DiscError::Io { path, source } => {
                assert_eq!(path, Path::new("/dev/sr-test"));
                assert_eq!(source.raw_os_error(), Some(5));
            }
            other => panic!("expected an I/O error, got {other}"),
        }
    }

    /// The size in a directory entry comes off the disc, and a damaged disc is
    /// exactly where that reads wrong. It must not become an allocation: the
    /// backup is read instead, and the drive is never asked for the extent.
    #[test]
    fn an_impossible_size_never_becomes_an_allocation() {
        let (mut disc, image) = disc_and_image();
        disc.video_ts_files[0].size = u64::MAX / 2;
        // Nothing is damaged, so an attempt to read the real extent would
        // succeed and return the IFO's bytes.
        let mut reader = Damaged::new(image, 0..0);

        let bytes = read_vts_extent(&mut reader, &disc)
            .expect("the file is in the table")
            .expect("the backup reads");

        assert!(bytes.iter().all(|byte| *byte == BUP_BYTE));
    }

    /// An entry addressed past the end of the disc is the other shape of a
    /// misread, and gets the same treatment.
    #[test]
    fn an_extent_past_the_end_of_the_disc_falls_back() {
        let (mut disc, image) = disc_and_image();
        // A plausible size, at a sector the image does not contain.
        disc.video_ts_files[0].lba = 100_000;
        let mut reader = Damaged::new(image, 0..0);

        let bytes = read_vts_extent(&mut reader, &disc)
            .expect("the file is in the table")
            .expect("the backup reads");

        assert!(bytes.iter().all(|byte| *byte == BUP_BYTE));
    }
}

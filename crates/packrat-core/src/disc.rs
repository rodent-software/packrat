//! Reading a disc's title/chapter structure out of its `VIDEO_TS` IFO files.
//!
//! This works on a *mounted folder* (a `VIDEO_TS` directory) rather than a
//! whole-disc image, because that is how a disc already appears to us. The
//! `oxideav-dvd` IFO parsers are handed the individual file byte buffers.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use oxideav_dvd::{TtSrpt, VmgIfo, VtsIfo};

use crate::error::DiscError;
use crate::source::DiscSource;

/// DVD logical sector size.
const SECTOR: usize = 2048;

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

/// Read the title structure of a folder-backed disc.
pub fn read_disc(source: &DiscSource) -> Result<DiscModel, DiscError> {
    let main_path = source
        .main_ifo()
        .ok_or_else(|| DiscError::NoVideoTs(source.root().display().to_string()))?;
    let main_buf = read_file(&main_path)?;

    let vmg = VmgIfo::parse(&main_buf).map_err(|e| DiscError::Ifo {
        path: main_path.clone(),
        message: e.to_string(),
    })?;

    // TT_SRPT lives at the sector the VMGI_MAT points to.
    let tt_offset = vmg.tt_srpt_sector as usize * SECTOR;
    let tt_buf = main_buf.get(tt_offset..).ok_or_else(|| DiscError::Ifo {
        path: main_path.clone(),
        message: format!("TT_SRPT sector {} is past end of file", vmg.tt_srpt_sector),
    })?;
    let tt = TtSrpt::parse(tt_buf).map_err(|e| DiscError::Ifo {
        path: main_path.clone(),
        message: format!("TT_SRPT: {e}"),
    })?;

    // Parse each VTS once, so titles can carry chapter detail.
    let mut vts: Vec<Option<VtsIfo>> = vec![None; usize::from(vmg.number_of_title_sets) + 1];
    for n in 1..=vmg.number_of_title_sets {
        let Ok(nu8) = u8::try_from(n) else {
            continue;
        };
        let path = source.video_ts().join(format!("VTS_{nu8:02}_0.IFO"));
        if !path.is_file() {
            continue;
        }
        let buf = read_file(&path)?;
        let parsed = VtsIfo::parse(&buf, nu8).map_err(|e| DiscError::Ifo {
            path: path.clone(),
            message: e.to_string(),
        })?;
        vts[usize::from(nu8)] = Some(parsed);
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
        volume_id: source
            .root()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
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
    let path = source.video_ts().join(format!("VTS_{vts_number:02}_0.IFO"));
    let buf = read_file(&path)?;
    VtsIfo::parse(&buf, vts_number).map_err(|e| DiscError::Ifo {
        path,
        message: e.to_string(),
    })
}

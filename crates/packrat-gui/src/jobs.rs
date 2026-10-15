//! The per-file model behind the plan and progress screens.
//!
//! The file list itself comes from `packrat-engine`'s planner, so the GUI and
//! any other frontend agree on what a disc produces. This adds the two things
//! only a screen cares about: the playback duration (for a time estimate) and
//! where each file has got to.

use std::path::{Path, PathBuf};
use std::time::Duration;

use packrat_core::{Classification, DiscKind, DiscModel, RemuxReport, Title};
use packrat_engine::{plan_jobs, ExtrasMode, MovieNaming, Naming, PlanJob, PlanOptions};

/// Where a queued file is in the rip.
#[derive(Debug, Default)]
pub enum JobStatus {
    #[default]
    Pending,
    Running,
    Done(RemuxReport),
    Failed(String),
    Cancelled,
}

/// One planned output file.
#[derive(Debug)]
pub struct Job {
    pub title: u16,
    pub first: u16,
    pub last: u16,
    pub path: PathBuf,
    /// Playback duration of the chapter range, used to estimate time left.
    pub duration: Duration,
    pub enabled: bool,
    pub status: JobStatus,
}

impl Job {
    fn from_plan(disc: &DiscModel, plan: PlanJob) -> Self {
        let duration = disc
            .titles
            .iter()
            .find(|t| t.number == plan.title)
            .map(|t| chapter_span_duration(t, plan.first, plan.last))
            .unwrap_or_default();
        Self {
            title: plan.title,
            first: plan.first,
            last: plan.last,
            path: plan.path,
            duration,
            enabled: plan.enabled,
            status: JobStatus::Pending,
        }
    }
}

/// Sum of the per-chapter durations over the 1-based inclusive range.
fn chapter_span_duration(title: &Title, first: u16, last: u16) -> Duration {
    title
        .chapter_durations
        .iter()
        .skip(usize::from(first.saturating_sub(1)))
        .take(usize::from(last.saturating_sub(first).saturating_add(1)))
        .copied()
        .sum()
}

/// Build the job list for a disc.
///
/// A movie's extras are always listed so they can be ticked individually, while
/// a TV disc's appear only when the extras toggle is on — the same rule the
/// command line used.
#[allow(clippy::too_many_arguments)]
pub fn assemble(
    disc: &DiscModel,
    classification: &Classification,
    naming: Option<&Naming>,
    movie_naming: Option<&MovieNaming>,
    movie_dir: Option<&Path>,
    out_dir: &Path,
    include_extras: bool,
) -> Vec<Job> {
    let extras = match (classification.kind, include_extras) {
        (DiscKind::Movie, true) => ExtrasMode::ListEnabled,
        (DiscKind::Movie, false) => ExtrasMode::ListDisabled,
        (_, true) => ExtrasMode::ListEnabled,
        (_, false) => ExtrasMode::Skip,
    };
    let options = PlanOptions {
        extras,
        ..PlanOptions::default()
    };

    plan_jobs(
        disc,
        classification,
        naming,
        movie_naming,
        movie_dir,
        out_dir,
        &options,
    )
    .into_iter()
    .map(|plan| Job::from_plan(disc, plan))
    .collect()
}

/// Playback duration still to be ripped: jobs left selected that have not
/// started. Unselected jobs never enter the queue, so counting them would
/// overstate the remaining time.
pub fn remaining_duration(jobs: &[Job]) -> Duration {
    jobs.iter()
        .filter(|job| job.enabled && matches!(job.status, JobStatus::Pending))
        .map(|job| job.duration)
        .sum()
}

/// Human-readable byte size, e.g. `4.6 GB`.
pub fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Format a playback duration as `h:mm:ss`, or `-` when unknown.
pub fn fmt_duration(d: Option<Duration>) -> String {
    match d {
        Some(d) => {
            let secs = d.as_secs();
            format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
        }
        None => "-".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_byte_sizes() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1_500), "1.5 KB");
        assert_eq!(fmt_bytes(100_000_000), "100 MB");
    }

    #[test]
    fn remaining_duration_skips_unselected_and_finished_jobs() {
        let disc = DiscModel {
            volume_id: "TEST".into(),
            provider_id: String::new(),
            vts_count: 1,
            titles: vec![Title {
                number: 1,
                vts: 1,
                vts_ttn: 1,
                angles: 1,
                chapters: 2,
                duration: Some(Duration::from_secs(120)),
                chapter_durations: vec![Duration::from_secs(60), Duration::from_secs(60)],
            }],
        };
        let plan = PlanJob {
            title: 1,
            first: 1,
            last: 2,
            path: PathBuf::from("a.mkv"),
            kind: packrat_engine::JobKind::Episode,
            enabled: true,
        };
        let mut job = Job::from_plan(&disc, plan);
        assert_eq!(job.duration, Duration::from_secs(120));
        assert_eq!(
            remaining_duration(std::slice::from_ref(&job)),
            Duration::from_secs(120)
        );

        job.enabled = false;
        assert_eq!(
            remaining_duration(std::slice::from_ref(&job)),
            Duration::ZERO
        );
    }
}

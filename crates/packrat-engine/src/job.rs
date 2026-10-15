//! Turning a disc and its resolved metadata into the list of files to write.
//!
//! This is the single planner behind both the CLI's `split` and the
//! interactive guide's job list, so naming decisions cannot drift between
//! frontends.

use std::path::{Path, PathBuf};

use packrat_core::{
    display_name, movie_extra_file_in, movie_extras, movie_file_in, parse_label, preferred_titles,
    primary_feature, split_title, Classification, DiscKind, DiscModel, Title, EXTRA_MIN,
    MIN_CONTENT,
};

use crate::naming::{MovieNaming, Naming};

/// What a planned output is. The file name alone does not say whether a short
/// title was chosen as a deliberate extra or is just a small episode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    /// The main feature of a movie disc.
    Feature,
    /// A TV episode, or a range of episodes delivered as one file.
    Episode,
    /// A bonus title: a trailer, featurette or second feature.
    Extra,
}

/// Whether bonus titles go in the plan, and whether they start selected.
///
/// The CLI omits extras entirely unless asked for them; the guide lists a
/// movie's extras so they can be ticked individually, which is why this is a
/// three-way choice rather than a flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExtrasMode {
    /// Leave bonus titles out of the plan.
    #[default]
    Skip,
    /// List bonus titles, unselected.
    ListDisabled,
    /// List bonus titles, selected.
    ListEnabled,
}

/// How much of a disc to plan.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlanOptions {
    /// Only this title number, even a short one.
    pub only_title: Option<u16>,
    /// Only this episode of each selected title, 1-based.
    pub only_episode: Option<usize>,
    /// Include every content-length title, not just the preferred ones.
    pub all_titles: bool,
    pub extras: ExtrasMode,
}

/// One planned output file.
#[derive(Debug, Clone)]
pub struct PlanJob {
    pub title: u16,
    pub first: u16,
    pub last: u16,
    pub path: PathBuf,
    pub kind: JobKind,
    /// Whether this job starts selected. Extras follow [`PlanOptions::extras`].
    pub enabled: bool,
}

/// Plan the files a disc should produce.
#[allow(clippy::too_many_arguments)]
pub fn plan_jobs(
    disc: &DiscModel,
    classification: &Classification,
    naming: Option<&Naming>,
    movie_naming: Option<&MovieNaming>,
    movie_dir: Option<&Path>,
    out_dir: &Path,
    options: &PlanOptions,
) -> Vec<PlanJob> {
    // An explicitly named title is always honoured, even a short extra, so a
    // movie disc only takes the movie path when no title was named.
    let is_movie = classification.kind == DiscKind::Movie && options.only_title.is_none();
    if is_movie {
        return plan_movie(disc, movie_naming, movie_dir, out_dir, options);
    }
    plan_tv(disc, naming, out_dir, options)
}

/// The movie half: the main feature and, per [`ExtrasMode`], its bonus titles.
fn plan_movie(
    disc: &DiscModel,
    movie_naming: Option<&MovieNaming>,
    movie_dir: Option<&Path>,
    out_dir: &Path,
    options: &PlanOptions,
) -> Vec<PlanJob> {
    let mut jobs = Vec::new();
    let label = parse_label(&disc.volume_id);

    // The main feature, or the longest content title when the disc's only
    // title is a short film and so under the feature threshold.
    if let Some(feature) = primary_feature(disc) {
        let path = match movie_naming {
            Some(naming) => naming.feature_path(),
            None => match movie_dir {
                Some(dir) => movie_file_in(dir, &label.title, label.year),
                None => out_dir.join(format!("{}.mkv", label.title)),
            },
        };
        jobs.push(PlanJob {
            title: feature.number,
            first: 1,
            last: feature.chapters,
            path,
            kind: JobKind::Feature,
            enabled: true,
        });
    }

    // Every other title is listed so the user can pick; extras start selected
    // only when the extras toggle is on. This is one selection surface, not a
    // reveal-then-choose two-step.
    if options.extras != ExtrasMode::Skip {
        let enabled = options.extras == ExtrasMode::ListEnabled;
        for title in movie_extras(disc) {
            let description = format!("Title {:02}", title.number);
            let path = match movie_naming {
                Some(naming) => naming.extra_path(&description),
                None => match movie_dir {
                    Some(dir) => movie_extra_file_in(dir, &label.title, label.year, &description),
                    None => out_dir.join(format!(
                        "{} - {}.mkv",
                        display_name(&label.title, label.year),
                        description
                    )),
                },
            };
            jobs.push(PlanJob {
                title: title.number,
                first: 1,
                last: title.chapters,
                path,
                kind: JobKind::Extra,
                enabled,
            });
        }
    }

    jobs
}

/// The TV half: per-episode files, or one file per title when the disc delivers
/// an episode range in a single title.
fn plan_tv(
    disc: &DiscModel,
    naming: Option<&Naming>,
    out_dir: &Path,
    options: &PlanOptions,
) -> Vec<PlanJob> {
    let mut jobs = Vec::new();
    let preferred = preferred_titles(disc);

    for title in disc.titles.iter().filter(|t| match options.only_title {
        // An explicitly named title is always honoured, even a short extra.
        Some(n) => t.number == n,
        None if options.all_titles => is_content(t),
        None => preferred.contains(&t.number),
    }) {
        let segments = split_title(title);
        if segments.len() >= 2 {
            for (i, segment) in segments.iter().enumerate() {
                if options.only_episode.is_some_and(|want| want != i + 1) {
                    continue;
                }
                let path = naming
                    .and_then(|n| n.path_for(title.number, i))
                    .unwrap_or_else(|| {
                        out_dir.join(format!("title{:02}-E{:02}.mkv", title.number, i + 1))
                    });
                jobs.push(PlanJob {
                    title: title.number,
                    first: segment.start_chapter,
                    last: segment.end_chapter,
                    path,
                    kind: JobKind::Episode,
                    enabled: true,
                });
            }
        } else if options.only_episode.is_none() {
            let path = naming
                .and_then(|n| n.whole_path(title.number))
                .unwrap_or_else(|| out_dir.join(format!("title{:02}.mkv", title.number)));
            jobs.push(PlanJob {
                title: title.number,
                first: 1,
                last: title.chapters,
                path,
                kind: JobKind::Episode,
                enabled: true,
            });
        }
    }

    if options.extras != ExtrasMode::Skip {
        let enabled = options.extras == ExtrasMode::ListEnabled;
        for title in disc
            .titles
            .iter()
            .filter(|t| is_extra(t) && options.only_title.map_or(true, |n| n == t.number))
        {
            let description = format!("Title {:02}", title.number);
            let path = naming
                .map(|n| n.extra_path(&description))
                .unwrap_or_else(|| out_dir.join(format!("title{:02}.mkv", title.number)));
            jobs.push(PlanJob {
                title: title.number,
                first: 1,
                last: title.chapters,
                path,
                kind: JobKind::Extra,
                enabled,
            });
        }
    }

    jobs
}

/// Whether a title is long enough to be content rather than an extra.
pub fn is_content(title: &Title) -> bool {
    title.duration.map(|d| d >= MIN_CONTENT).unwrap_or(false)
}

/// Whether a title is a plausible bonus feature.
pub fn is_extra(title: &Title) -> bool {
    title
        .duration
        .map(|d| d >= EXTRA_MIN && d < MIN_CONTENT)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn title(number: u16, chapters: u16, chapter_secs: u64) -> Title {
        Title {
            number,
            vts: 1,
            vts_ttn: number as u8,
            angles: 1,
            chapters,
            duration: Some(Duration::from_secs(chapter_secs * u64::from(chapters))),
            chapter_durations: vec![Duration::from_secs(chapter_secs); chapters as usize],
        }
    }

    fn disc(titles: Vec<Title>) -> DiscModel {
        DiscModel {
            volume_id: "TEST_DISC".into(),
            provider_id: String::new(),
            vts_count: 1,
            titles,
        }
    }

    fn classification(kind: DiscKind) -> Classification {
        Classification {
            kind,
            confidence: 90,
            reasons: Vec::new(),
        }
    }

    fn plan(
        disc: &DiscModel,
        disc_kind: DiscKind,
        movie_dir: Option<&Path>,
        extras: ExtrasMode,
    ) -> Vec<PlanJob> {
        let options = PlanOptions {
            extras,
            ..PlanOptions::default()
        };
        plan_jobs(
            disc,
            &classification(disc_kind),
            None,
            None,
            movie_dir,
            Path::new("."),
            &options,
        )
    }

    #[test]
    fn movie_plan_names_the_feature() {
        let d = disc(vec![title(1, 10, 600)]); // 100 minutes
        let jobs = plan(
            &d,
            DiscKind::Movie,
            Some(Path::new("/lib")),
            ExtrasMode::Skip,
        );
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].title, 1);
        assert_eq!(jobs[0].first, 1);
        assert_eq!(jobs[0].last, 10);
        assert_eq!(jobs[0].kind, JobKind::Feature);
        assert!(jobs[0].path.starts_with("/lib"));
        assert_eq!(jobs[0].path.extension().unwrap(), "mkv");
    }

    /// A short film is under the feature-length threshold, so the movie path
    /// has to plan it from the longest content title. Otherwise a disc whose
    /// only title is a 65-minute film plans no feature at all, and the film
    /// turns up in its own extras list.
    #[test]
    fn a_short_film_is_planned_as_the_feature() {
        // 65 minutes, plus a 10-minute making-of.
        let d = disc(vec![title(1, 13, 300), title(2, 1, 600)]);

        let jobs = plan(
            &d,
            DiscKind::Movie,
            Some(Path::new("/lib")),
            ExtrasMode::ListDisabled,
        );

        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].title, 1, "the short film is the feature");
        assert_eq!(jobs[0].kind, JobKind::Feature);
        assert!(jobs[0].enabled);
        assert_eq!(jobs[1].title, 2, "the making-of is the extra");
        assert_eq!(jobs[1].kind, JobKind::Extra);
        assert!(!jobs[1].enabled, "extras start unselected");
    }

    #[test]
    fn movie_extras_are_listed_and_selected_by_the_toggle() {
        // A 100-minute feature plus a 200-second featurette.
        let d = disc(vec![title(1, 10, 600), title(2, 1, 200)]);

        let off = plan(
            &d,
            DiscKind::Movie,
            Some(Path::new("/lib")),
            ExtrasMode::ListDisabled,
        );
        // Both are listed, but only the feature starts selected.
        assert_eq!(off.len(), 2);
        assert!(off[0].enabled);
        let extra = off.iter().find(|j| j.title == 2).expect("extra");
        assert!(!extra.enabled);
        assert_eq!(extra.kind, JobKind::Extra);
        assert!(extra.path.starts_with("/lib"));
        assert!(extra.path.to_string_lossy().contains("Other"));

        let on = plan(
            &d,
            DiscKind::Movie,
            Some(Path::new("/lib")),
            ExtrasMode::ListEnabled,
        );
        assert!(on.iter().find(|j| j.title == 2).expect("extra").enabled);
    }

    #[test]
    fn a_skipped_movie_plan_omits_extras_entirely() {
        let d = disc(vec![title(1, 10, 600), title(2, 1, 200)]);
        let jobs = plan(
            &d,
            DiscKind::Movie,
            Some(Path::new("/lib")),
            ExtrasMode::Skip,
        );
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].kind, JobKind::Feature);
    }

    #[test]
    fn extras_are_added_only_when_requested() {
        // Three ~22 minute episodes plus a 200 second featurette.
        let d = disc(vec![
            title(1, 1, 22 * 60),
            title(2, 1, 22 * 60),
            title(3, 1, 22 * 60),
            title(4, 1, 200),
        ]);
        let without = plan(&d, DiscKind::TvSeries, None, ExtrasMode::Skip);
        let with = plan(&d, DiscKind::TvSeries, None, ExtrasMode::ListEnabled);
        assert!(!without.is_empty());
        assert_eq!(with.len(), without.len() + 1);
        assert!(with.iter().any(|j| j.title == 4));
        assert!(!without.iter().any(|j| j.title == 4));
    }
}

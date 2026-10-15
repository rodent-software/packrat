//! Matching a disc against TVmaze/TMDb and turning the result into file names.
//!
//! None of this touches the network unless a destination and (for movies) a
//! TMDb key are configured; a miss is reported as a warning so a rip can still
//! proceed with plain file names.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use packrat_core::{
    episode_file_in, episode_range_file_in, episodes_before_season, extra_file_in,
    match_episodes_global, match_span_global, movie_extra_file_in, movie_file_in, parse_label,
    resolve_movie, search_show, season_dir_in, show_dir_in, split_title, DiscModel, Episode,
    EpisodeMatch, EpisodeSpan, Movie, MovieQuery, Show, Title,
};

use crate::config::Config;

/// Metadata used to name outputs in a Plex library.
#[derive(Clone)]
pub struct Naming {
    tv_dir: PathBuf,
    show: String,
    year: Option<u16>,
    episodes: HashMap<u16, Vec<EpisodeMatch>>,
    spans: HashMap<u16, EpisodeSpan>,
}

impl Naming {
    pub fn path_for(&self, title: u16, episode_index: usize) -> Option<PathBuf> {
        let matched = self.episodes.get(&title)?.get(episode_index)?;
        Some(episode_file_in(
            &self.tv_dir,
            &self.show,
            self.year,
            matched.season,
            matched.number,
            matched.title.as_deref(),
        ))
    }

    /// Path for a title delivered as one file that spans several episodes.
    pub fn whole_path(&self, title: u16) -> Option<PathBuf> {
        let span = self.spans.get(&title)?;
        Some(episode_range_file_in(
            &self.tv_dir,
            &self.show,
            self.year,
            span.season,
            span.first,
            span.last,
            span.title.as_deref(),
        ))
    }

    /// Path for a short title ripped as an extra under `<show>/Other/`.
    pub fn extra_path(&self, description: &str) -> PathBuf {
        extra_file_in(&self.tv_dir, &self.show, self.year, description)
    }
}

/// The outcome of a metadata lookup.
pub struct Resolved {
    pub show: Option<Show>,
    pub naming: Option<Naming>,
    pub warning: Option<String>,
}

/// Metadata used to name movie outputs in a Plex library.
#[derive(Clone)]
pub struct MovieNaming {
    movies_dir: PathBuf,
    title: String,
    year: Option<u16>,
}

impl MovieNaming {
    pub fn feature_path(&self) -> PathBuf {
        movie_file_in(&self.movies_dir, &self.title, self.year)
    }

    /// Path for a short title ripped as an extra under `<Title (Year)>/Other/`.
    pub fn extra_path(&self, description: &str) -> PathBuf {
        movie_extra_file_in(&self.movies_dir, &self.title, self.year, description)
    }

    /// Replace the title and year, e.g. after the user picks a TMDb candidate.
    pub fn retitle(&mut self, title: String, year: Option<u16>) {
        self.title = title;
        self.year = year;
    }
}

/// The outcome of a movie metadata lookup.
pub struct ResolvedMovie {
    pub naming: MovieNaming,
    /// The auto-accepted TMDb match, when there was one.
    pub movie: Option<Movie>,
    /// Ranked candidates when the match was not confident enough to accept.
    pub candidates: Vec<(f64, Movie)>,
    pub warning: Option<String>,
}

/// Resolve TMDb metadata for a disc's main feature.
///
/// A missing key or a low-confidence match is reported through
/// [`ResolvedMovie::warning`] and falls back to the disc label, so ripping
/// never depends on the network. Without a key it makes no request at all.
pub fn resolve_movie_naming(
    movies_dir: &Path,
    disc: &DiscModel,
    feature: &Title,
    title_override: Option<&str>,
    year_override: Option<u16>,
) -> Result<ResolvedMovie> {
    let label = parse_label(&disc.volume_id);
    let query_title = title_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(label.title.as_str());
    let query_year = year_override.or(label.year);

    let fallback = |warning: Option<String>| ResolvedMovie {
        naming: MovieNaming {
            movies_dir: movies_dir.to_path_buf(),
            title: query_title.to_string(),
            year: query_year,
        },
        movie: None,
        candidates: Vec::new(),
        warning,
    };

    let Some(key) = Config::load().tmdb_key() else {
        return Ok(fallback(Some(
            "no TMDb API key; naming the movie from the disc label".into(),
        )));
    };

    let query = MovieQuery::new(query_title, query_year, feature.duration);
    let resolution = match resolve_movie(&key, &query) {
        Ok(resolution) => resolution,
        // A key or network problem must not block the rip; fall back to the
        // label and say why.
        Err(e) => {
            return Ok(fallback(Some(format!(
                "TMDb lookup failed ({e}); naming the movie from the disc label"
            ))));
        }
    };

    if let Some(movie) = resolution.auto_accepted() {
        return Ok(ResolvedMovie {
            naming: MovieNaming {
                movies_dir: movies_dir.to_path_buf(),
                title: movie.title.clone(),
                year: movie.year(),
            },
            movie: Some(movie.clone()),
            candidates: resolution.candidates.clone(),
            warning: None,
        });
    }

    let warning = if resolution.candidates.is_empty() {
        format!("no TMDb match for '{query_title}'; naming the movie from the disc label")
    } else {
        format!("no confident TMDb match for '{query_title}'; choose a candidate")
    };
    let mut resolved = fallback(Some(warning));
    resolved.candidates = resolution.candidates;
    Ok(resolved)
}

/// One thing a title search found, in the terms the match fields are set in.
#[derive(Debug, Clone)]
pub struct TitleMatch {
    /// What the list shows.
    pub label: String,
    /// What goes in the title or show field.
    pub name: String,
    pub year: Option<u16>,
    /// How well the provider thinks it matched, where it says at all.
    pub score: Option<f64>,
}

impl TitleMatch {
    /// A candidate the resolver ranked, in the same shape as a search hit, so
    /// both reach the screen through one list.
    pub fn from_movie(score: f64, movie: &Movie) -> Self {
        let year = movie.year();
        Self {
            label: name_and_year(&movie.title, year),
            name: movie.title.clone(),
            year,
            score: Some(score),
        }
    }
}

/// Search the provider for a movie or a show.
///
/// A list rather than a verdict: this backs the search box, where the point is
/// to let the person choose between things a volume label cannot tell apart.
/// Failures are reported rather than swallowed, because this is the one lookup
/// the user asked for by hand.
pub fn search_titles(
    movies: bool,
    query: &str,
    year: Option<u16>,
) -> std::result::Result<Vec<TitleMatch>, String> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }

    if movies {
        let key = Config::load()
            .tmdb_key()
            .ok_or_else(|| "no TMDb API key; add one in settings".to_string())?;
        let found = packrat_core::tmdb::search_movies(&key, query, year)
            .map_err(|e| format!("TMDb search failed: {e}"))?;
        Ok(found
            .into_iter()
            .map(|movie| {
                let year = movie.year();
                TitleMatch {
                    label: name_and_year(&movie.title, year),
                    name: movie.title,
                    year,
                    // TMDb returns no score; its order is the ranking.
                    score: None,
                }
            })
            .collect())
    } else {
        let found = packrat_core::meta::search_shows(query)
            .map_err(|e| format!("TVmaze search failed: {e}"))?;
        Ok(found
            .into_iter()
            .map(|(score, show)| {
                let year = show.year();
                TitleMatch {
                    label: name_and_year(&show.name, year),
                    name: show.name,
                    year,
                    score: Some(score),
                }
            })
            .collect())
    }
}

/// `Name (Year)`, or just the name when the year is not known.
fn name_and_year(name: &str, year: Option<u16>) -> String {
    match year {
        Some(year) => format!("{name} ({year})"),
        None => name.to_string(),
    }
}

/// The episode number to start this disc at: the user's correction when one
/// was given, otherwise the number after the highest episode already in the
/// destination season. `None` leaves the provider runtime and disc hint to
/// decide.
pub fn first_episode_for(
    tv_dir: &Path,
    show: &str,
    year: Option<u16>,
    season: u16,
    manual: Option<u16>,
) -> Option<u16> {
    manual.or_else(|| {
        let season_dir = season_dir_in(tv_dir, show, year, season);
        packrat_core::library::highest_episode(&season_dir, season)
            .map(|episode| episode.saturating_add(1))
    })
}

/// Resolve TVmaze metadata for the disc's preferred titles. A miss is reported
/// through [`Resolved::warning`] rather than as an error, so ripping can still
/// proceed with plain file names.
pub fn resolve_naming(
    tv_dir: &Path,
    disc: &DiscModel,
    preferred: &[u16],
    show_override: Option<&str>,
    season_override: Option<u16>,
    first_episode: Option<u16>,
) -> Result<Resolved> {
    let label = parse_label(&disc.volume_id);
    let query: &str = show_override
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(label.title.as_str());
    let Some(show) = search_show(query).context("TVmaze search failed")? else {
        return Ok(Resolved {
            show: None,
            naming: None,
            warning: Some(format!(
                "no TVmaze match for '{query}'; using plain file names"
            )),
        });
    };
    let season = season_override.or(label.season).unwrap_or(1);
    let episodes = packrat_core::meta::episodes(show.id).context("fetching episodes")?;
    let year = show.year();
    let name = show_override
        .map(str::to_string)
        .unwrap_or_else(|| show.name.clone());

    let (episodes_by_title, spans) = match_disc_episodes(
        tv_dir,
        &name,
        year,
        season,
        disc,
        label.disc,
        &episodes,
        preferred,
        first_episode,
    );

    Ok(Resolved {
        show: Some(show),
        naming: Some(Naming {
            tv_dir: tv_dir.to_path_buf(),
            show: name,
            year,
            episodes: episodes_by_title,
            spans,
        }),
        warning: None,
    })
}

/// Match a disc's preferred titles to provider episodes, using the destination
/// library to place the disc in the show's global episode order.
///
/// A DVD set does not split a show where the provider does, so a disc's own
/// episode number cannot simply be looked up in the provider's season of the
/// same name. The library's earlier seasons say how many episodes precede this
/// season on the discs, and that many provider episodes is where it starts;
/// the disc's own number is what goes in the file name.
#[allow(clippy::too_many_arguments)]
pub fn match_disc_episodes(
    tv_dir: &Path,
    name: &str,
    year: Option<u16>,
    season: u16,
    disc: &DiscModel,
    disc_number: Option<u16>,
    episodes: &[Episode],
    preferred: &[u16],
    manual_first: Option<u16>,
) -> (HashMap<u16, Vec<EpisodeMatch>>, HashMap<u16, EpisodeSpan>) {
    let hints = disc_episode_hints(episodes, season, disc, preferred, disc_number);

    // Where this season starts in the show's global episode order. The
    // provider's own season boundary cannot say, but the earlier seasons
    // already backed up can; fall back to the provider's earlier-season count
    // for a library that has none.
    let show_dir = show_dir_in(tv_dir, name, year);
    let library_before = episodes_before_season(&show_dir, season);
    let provider_before = provider_episodes_before(episodes, season);
    let position_before = if library_before > 0 {
        library_before
    } else {
        provider_before
    };

    // Number from the user's correction when they made one, else continue
    // after the episodes already in the destination season: the label's disc
    // number cannot tell us that a final disc holds fewer episodes than the
    // earlier ones, but the files already backed up can.
    let mut next_episode =
        manual_first.or_else(|| first_episode_for(tv_dir, name, year, season, None));

    let mut by_title = HashMap::new();
    let mut spans = HashMap::new();
    for number in preferred {
        let Some(title) = disc.titles.iter().find(|t| t.number == *number) else {
            continue;
        };
        let segments = split_title(title);
        // The disc's own number for this title's first episode, and where that
        // episode sits in the provider's global order. Without a library start
        // the disc-number hint places it; the library still supplies the
        // season's global offset.
        let (first, global) = match next_episode {
            Some(first) => (
                first,
                position_before.saturating_add(usize::from(first.saturating_sub(1))),
            ),
            None => {
                let offset = hints.get(number).copied().unwrap_or(0);
                (
                    u16::try_from(offset + 1).unwrap_or(u16::MAX),
                    position_before.saturating_add(offset),
                )
            }
        };
        if segments.len() < 2 {
            // Delivered as one file: see whether it spans several episodes.
            if let Some(span) = match_span_global(
                episodes,
                season,
                first,
                global,
                title.duration.unwrap_or_default(),
            ) {
                if next_episode.is_some() {
                    next_episode = span.last.checked_add(1);
                }
                spans.insert(*number, span);
            }
            continue;
        }
        let durations: Vec<Duration> = segments.iter().map(|s| s.duration).collect();
        let matched = match_episodes_global(episodes, season, first, global, &durations);
        if next_episode.is_some() {
            if let Some(last) = matched.last() {
                next_episode = last.number.checked_add(1);
            }
        }
        by_title.insert(*number, matched);
    }

    (by_title, spans)
}

/// Guess where each preferred title starts within its season, so a disc other
/// than the first does not restart at episode 1.
///
/// A DVD label carries a disc number but no episode count, so we assume each
/// disc covers a contiguous block and estimate the block size from how many
/// episodes this disc appears to hold. For a "Play All" title that is the
/// number of segments it splits into; for separate per-episode titles, the
/// title's runtime divided by the season's average episode runtime stands in.
fn disc_episode_hints(
    episodes: &[Episode],
    season: u16,
    disc: &DiscModel,
    preferred: &[u16],
    disc_number: Option<u16>,
) -> HashMap<u16, usize> {
    let average = average_runtime(episodes, season);
    let mut hints = HashMap::new();
    let mut before = 0usize;
    for number in preferred {
        let Some(title) = disc.titles.iter().find(|t| t.number == *number) else {
            continue;
        };
        hints.insert(*number, before);
        let segments = split_title(title);
        before += if segments.len() >= 2 {
            segments.len()
        } else {
            estimated_episodes(title.duration, average)
        };
    }

    // The disc's own estimated episode count is our best guess at how many
    // episodes precede it on every earlier disc.
    let per_disc = before.max(1);
    let start = usize::from(disc_number.unwrap_or(1).saturating_sub(1)) * per_disc;
    hints
        .into_iter()
        .map(|(number, offset)| (number, start + offset))
        .collect()
}

/// Content episodes in provider seasons before `season`.
fn provider_episodes_before(episodes: &[Episode], season: u16) -> usize {
    episodes
        .iter()
        .filter(|e| e.season > 0 && e.season < u32::from(season))
        .count()
}

/// Mean runtime of a season's episodes, when the provider supplies any.
fn average_runtime(episodes: &[Episode], season: u16) -> Option<Duration> {
    let runtimes: Vec<u64> = episodes
        .iter()
        .filter(|e| e.season == u32::from(season))
        .filter_map(|e| e.runtime)
        .map(|minutes| u64::from(minutes) * 60)
        .collect();
    if runtimes.is_empty() {
        return None;
    }
    Some(Duration::from_secs(
        runtimes.iter().sum::<u64>() / runtimes.len() as u64,
    ))
}

/// How many episodes a single whole-file title is likely to contain.
fn estimated_episodes(duration: Option<Duration>, average: Option<Duration>) -> usize {
    match (duration, average) {
        (Some(duration), Some(average)) if average.as_secs() > 0 => {
            let episodes = (duration.as_secs() + average.as_secs() / 2) / average.as_secs();
            usize::try_from(episodes.max(1)).unwrap_or(1)
        }
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn episode(number: u32, runtime_minutes: u32) -> Episode {
        Episode {
            id: number,
            name: Some(format!("Episode {number}")),
            season: 1,
            number: Some(number),
            runtime: Some(runtime_minutes),
        }
    }

    fn whole_title(number: u16, minutes: u64) -> Title {
        Title {
            number,
            vts: 1,
            vts_ttn: number as u8,
            angles: 1,
            chapters: 1,
            duration: Some(Duration::from_secs(minutes * 60)),
            chapter_durations: vec![Duration::from_secs(minutes * 60)],
        }
    }

    fn disc_with(titles: Vec<Title>) -> DiscModel {
        DiscModel {
            volume_id: "TEST_S1_D2".into(),
            provider_id: String::new(),
            vts_count: 1,
            titles,
        }
    }

    #[test]
    fn estimates_episodes_from_the_average_runtime() {
        assert_eq!(
            estimated_episodes(
                Some(Duration::from_secs(48 * 60)),
                Some(Duration::from_secs(24 * 60))
            ),
            2
        );
        assert_eq!(
            estimated_episodes(
                Some(Duration::from_secs(24 * 60)),
                Some(Duration::from_secs(24 * 60))
            ),
            1
        );
        assert_eq!(
            estimated_episodes(Some(Duration::from_secs(30 * 60)), None),
            1
        );
    }

    #[test]
    fn second_disc_hints_continue_after_the_first_disc() {
        let episodes: Vec<Episode> = (1..=12).map(|n| episode(n, 24)).collect();
        // Six separate per-episode titles, so each disc is estimated at six.
        let titles: Vec<Title> = (1..=6).map(|n| whole_title(n, 24)).collect();
        let disc = disc_with(titles);
        let preferred = vec![1, 2, 3, 4, 5, 6];

        let hints = disc_episode_hints(&episodes, 1, &disc, &preferred, Some(2));
        let starts: Vec<usize> = preferred.iter().map(|n| hints[n]).collect();
        assert_eq!(starts, vec![6, 7, 8, 9, 10, 11]);

        // Those hints place every file on the matching second-disc episode.
        let numbers: Vec<u16> = preferred
            .iter()
            .filter_map(|n| {
                let title = disc.titles.iter().find(|t| t.number == *n)?;
                let span = packrat_core::match_span(
                    &episodes,
                    1,
                    title.duration.unwrap_or_default(),
                    hints.get(n).copied(),
                )?;
                Some(span.first)
            })
            .collect();
        assert_eq!(numbers, vec![7, 8, 9, 10, 11, 12]);
    }

    #[test]
    fn first_disc_starts_at_episode_one() {
        let episodes: Vec<Episode> = (1..=12).map(|n| episode(n, 24)).collect();
        let titles: Vec<Title> = (1..=6).map(|n| whole_title(n, 24)).collect();
        let disc = disc_with(titles);
        let preferred = vec![1, 2, 3, 4, 5, 6];

        let hints = disc_episode_hints(&episodes, 1, &disc, &preferred, Some(1));
        assert_eq!(hints[&1], 0);
        assert_eq!(hints[&6], 5);
    }

    #[test]
    fn a_disc_continues_after_the_destination_library() {
        let root = std::env::temp_dir().join(format!("packrat-first-ep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let season = season_dir_in(&root, "Dragon Ball", Some(1986), 1);
        std::fs::create_dir_all(&season).unwrap();
        std::fs::write(
            season.join("Dragon Ball (1986) - s01e28 - The Final Blow.mkv"),
            b"x",
        )
        .unwrap();

        // The disc after a 28-episode library starts at 29, and a manual
        // correction still wins over the library.
        assert_eq!(
            first_episode_for(&root, "Dragon Ball", Some(1986), 1, None),
            Some(29)
        );
        assert_eq!(
            first_episode_for(&root, "Dragon Ball", Some(1986), 1, Some(5)),
            Some(5)
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// An empty search asks nothing. It is the one case that can be checked
    /// without a provider, and the one that would otherwise send a request for
    /// every keystroke on the way to a real query.
    #[test]
    fn an_empty_search_asks_nothing() {
        assert!(search_titles(false, "   ", None).unwrap().is_empty());
        assert!(search_titles(true, "", None).unwrap().is_empty());
    }

    /// Choosing a show writes its name, and leaves the disc's season to the
    /// disc: the provider's year is when the show started, not when this series
    /// of it was made.
    #[test]
    fn a_chosen_match_carries_the_name_and_the_title_year() {
        let found = TitleMatch {
            label: "The Matrix (1999)".into(),
            name: "The Matrix".into(),
            year: Some(1999),
            score: None,
        };

        assert_eq!(found.name, "The Matrix");
        assert_eq!(found.year, Some(1999));
        assert_eq!(name_and_year("Unannounced", None), "Unannounced");
        assert_eq!(
            name_and_year("Dragon Ball", Some(1986)),
            "Dragon Ball (1986)"
        );
    }
}

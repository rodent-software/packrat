//! Metadata lookup and mapping ripped segments onto real episodes.
//!
//! TVmaze is the default source because it is free and needs no API key. A
//! TMDb client (which does need a key) can be layered on later for movies and
//! for shows TVmaze does not carry.

use std::time::Duration;

use serde::Deserialize;

const TVMAZE: &str = "https://api.tvmaze.com";

/// Runtime slack, per disc segment, that still counts as a tie when a disc
/// start hint is available. TVmaze frequently reports the same (or nearly the
/// same) runtime for every episode, so without slack the runtime score can
/// nudge a later disc back to episode 1.
const HINT_TIE_SECONDS_PER_SEGMENT: u64 = 3 * 60;
/// Relative runtime slack for whole-file span matching.
const SPAN_TIE_MARGIN: f64 = 0.05;

/// Anything that can go wrong talking to a metadata provider.
#[derive(Debug, thiserror::Error)]
pub enum MetaError {
    #[error("network error: {0}")]
    Http(String),
    #[error("could not parse metadata response: {0}")]
    Json(String),
}

/// A show as returned by TVmaze.
#[derive(Debug, Clone, Deserialize)]
pub struct Show {
    pub id: u32,
    pub name: String,
    #[serde(default)]
    pub premiered: Option<String>,
}

impl Show {
    /// First four digits of the premiere date, as a year.
    pub fn year(&self) -> Option<u16> {
        self.premiered
            .as_deref()
            .and_then(|p| p.get(0..4))
            .and_then(|y| y.parse().ok())
    }
}

/// One episode as returned by TVmaze.
#[derive(Debug, Clone, Deserialize)]
pub struct Episode {
    #[serde(default)]
    pub id: u32,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub season: u32,
    #[serde(default)]
    pub number: Option<u32>,
    /// Nominal runtime in minutes.
    #[serde(default)]
    pub runtime: Option<u32>,
}

/// A disc segment mapped to a real episode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpisodeMatch {
    pub season: u16,
    pub number: u16,
    pub title: Option<String>,
    pub runtime: Option<Duration>,
    /// 0-based index of the disc segment this maps to.
    pub segment: usize,
}

/// Look up a single show by name. `Ok(None)` means no match.
pub fn search_show(title: &str) -> Result<Option<Show>, MetaError> {
    let url = format!("{TVMAZE}/singlesearch/shows?q={}", encode(title));
    match ureq::get(&url).call() {
        Ok(response) => response
            .into_body()
            .read_json::<Show>()
            .map(Some)
            .map_err(|e| MetaError::Json(e.to_string())),
        Err(ureq::Error::StatusCode(404)) => Ok(None),
        Err(e) => Err(MetaError::Http(e.to_string())),
    }
}

/// Every episode of a show, across all seasons.
pub fn episodes(show_id: u32) -> Result<Vec<Episode>, MetaError> {
    let url = format!("{TVMAZE}/shows/{show_id}/episodes");
    let response = ureq::get(&url)
        .call()
        .map_err(|e| MetaError::Http(e.to_string()))?;
    response
        .into_body()
        .read_json::<Vec<Episode>>()
        .map_err(|e| MetaError::Json(e.to_string()))
}

/// Map `segments` (already in disc order) onto the best-matching window of
/// `episodes` in `season`.
///
/// Runtimes choose the window, so a disc 2 whose episodes start at number 8
/// lands on the right offset. `start_hint` is a 0-based index into the season
/// guessed from the disc number; it is used directly when no runtimes are
/// available, and otherwise settles windows whose runtimes match within
/// `HINT_TIE_SECONDS_PER_SEGMENT`, so a later disc does not restart at
/// episode 1.
pub fn match_episodes(
    episodes: &[Episode],
    season: u16,
    segments: &[Duration],
    start_hint: Option<usize>,
) -> Vec<EpisodeMatch> {
    let mut season_episodes: Vec<&Episode> = episodes
        .iter()
        .filter(|e| e.season == u32::from(season))
        .collect();
    season_episodes.sort_by_key(|e| e.number.unwrap_or(0));

    if season_episodes.is_empty() || segments.is_empty() {
        return Vec::new();
    }

    let runtime_of = |episode: &Episode| {
        episode
            .runtime
            .map(|m| Duration::from_secs(u64::from(m) * 60))
    };

    let max_start = season_episodes.len().saturating_sub(segments.len());
    let hint = start_hint.map(|h| h.min(max_start));

    let scores: Vec<Option<u64>> = (0..=max_start)
        .map(|start| {
            let mut score = 0u64;
            let mut comparable = false;
            for (i, segment) in segments.iter().enumerate() {
                if let Some(runtime) = runtime_of(season_episodes[start + i]) {
                    comparable = true;
                    score += runtime.as_secs().abs_diff(segment.as_secs());
                }
            }
            comparable.then_some(score)
        })
        .collect();
    let best_score = scores.iter().flatten().min().copied();

    let offset = match (best_score, hint) {
        // No runtimes anywhere: trust the disc hint, else season start.
        (None, None) => 0,
        (None, Some(hint)) => hint,
        // Without a hint, keep the historical runtime choice (earliest best).
        (Some(min_score), None) => scores
            .iter()
            .position(|score| *score == Some(min_score))
            .unwrap_or(0),
        // With a hint, windows within the margin are treated as tied and the
        // one nearest the disc hint wins. A runtime that clearly disagrees
        // (beyond the margin) still overrides the hint.
        (Some(min_score), Some(hint)) => {
            let margin = HINT_TIE_SECONDS_PER_SEGMENT.saturating_mul(segments.len() as u64);
            let affordable = min_score.saturating_add(margin);
            scores
                .iter()
                .enumerate()
                .filter(|(_, score)| score.is_some_and(|score| score <= affordable))
                .map(|(start, _)| start)
                .min_by_key(|start| start.abs_diff(hint))
                .unwrap_or(hint)
        }
    };

    segments
        .iter()
        .enumerate()
        .filter_map(|(i, _segment)| {
            let episode = season_episodes.get(offset + i)?;
            Some(EpisodeMatch {
                season,
                number: u16::try_from(episode.number.unwrap_or((offset + i + 1) as u32)).ok()?,
                title: episode.name.clone(),
                runtime: runtime_of(episode),
                segment: i,
            })
        })
        .collect()
}

/// Map `segments` onto a run of episodes that starts at the explicit episode
/// number `first`.
///
/// This backs a user's manual correction of a disc whose place in the season
/// the label got wrong (a final disc with fewer episodes than the earlier
/// ones, say). Numbers increase by one per segment. A run that leaves the
/// provider's season — a DVD set does not split a show the way TVmaze does —
/// keeps its own numbers but takes titles from the next provider season, so
/// the episodes are not left unlabelled; only a number past the provider's
/// last episode has no title.
pub fn match_episodes_from(
    episodes: &[Episode],
    season: u16,
    first: u16,
    segments: &[Duration],
) -> Vec<EpisodeMatch> {
    let runtime_of = |episode: &Episode| {
        episode
            .runtime
            .map(|m| Duration::from_secs(u64::from(m) * 60))
    };

    // Provider order, so a number the season does not reach continues into the
    // seasons after it.
    let mut ordered: Vec<&Episode> = episodes.iter().collect();
    ordered.sort_by_key(|e| (e.season, e.number.unwrap_or(0)));
    let season_start = ordered.iter().position(|e| e.season == u32::from(season));

    segments
        .iter()
        .enumerate()
        .filter_map(|(i, _segment)| {
            let number = first.checked_add(u16::try_from(i).ok()?)?;
            let episode = episodes
                .iter()
                .find(|e| e.season == u32::from(season) && e.number == Some(u32::from(number)))
                .or_else(|| {
                    let start = season_start?;
                    ordered
                        .get(start + usize::from(number.saturating_sub(1)))
                        .copied()
                });
            Some(EpisodeMatch {
                season,
                number,
                title: episode.and_then(|e| e.name.clone()),
                runtime: episode.and_then(runtime_of),
                segment: i,
            })
        })
        .collect()
}

/// A single file that spans several consecutive episodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpisodeSpan {
    pub season: u16,
    pub first: u16,
    pub last: u16,
    /// Episode title, only when the span is a single episode.
    pub title: Option<String>,
}

/// Best span of consecutive episodes whose combined runtime matches `segment`.
///
/// Used when a disc delivers several episodes in one file, so it can be named
/// `sXXeYY-eZZ` the way Plex expects. `start_hint` is a 0-based season index
/// guessed from the disc number and broken the same way as
/// [`match_episodes`].
pub fn match_span(
    episodes: &[Episode],
    season: u16,
    segment: Duration,
    start_hint: Option<usize>,
) -> Option<EpisodeSpan> {
    let mut season_episodes: Vec<&Episode> = episodes
        .iter()
        .filter(|e| e.season == u32::from(season))
        .collect();
    season_episodes.sort_by_key(|e| e.number.unwrap_or(0));
    if season_episodes.is_empty() {
        return None;
    }

    let target = segment.as_secs_f64().max(1.0);
    // (error, start, end-exclusive) for every candidate span.
    let mut candidates: Vec<(f64, usize, usize)> = Vec::new();

    for start in 0..season_episodes.len() {
        let mut sum = 0.0f64;
        for (end, episode) in season_episodes.iter().enumerate().skip(start) {
            let Some(minutes) = episode.runtime else {
                continue;
            };
            sum += f64::from(minutes) * 60.0;
            if sum > target * 1.5 {
                break;
            }
            let error = ((sum - target) / target).abs();
            candidates.push((error, start, end + 1));
        }
    }

    let best_error = candidates
        .iter()
        .map(|(error, _, _)| *error)
        .fold(f64::INFINITY, f64::min);
    if best_error > 0.15 {
        return None;
    }

    let (_, start, end) = match start_hint {
        Some(hint) => {
            // Runtimes within the tie margin are equally plausible; choose the
            // span that starts nearest the disc hint.
            let affordable = best_error + SPAN_TIE_MARGIN;
            candidates
                .iter()
                .filter(|(error, _, _)| *error <= affordable)
                .min_by(|a, b| {
                    a.1.abs_diff(hint)
                        .cmp(&b.1.abs_diff(hint))
                        .then(a.0.total_cmp(&b.0))
                })
                .copied()?
        }
        None => candidates
            .into_iter()
            .min_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)))?,
    };

    if end <= start {
        return None;
    }

    let first = u16::try_from(season_episodes[start].number.unwrap_or(start as u32 + 1)).ok()?;
    let last = u16::try_from(season_episodes[end - 1].number.unwrap_or(end as u32)).ok()?;
    let title = (end == start + 1)
        .then(|| season_episodes[start].name.clone())
        .flatten();

    Some(EpisodeSpan {
        season,
        first,
        last,
        title,
    })
}

/// Anchor a multi-episode file at the explicit episode number `first`.
///
/// The provider's runtimes still decide how many episodes the file spans; the
/// user's correction only says where that run begins. Without runtimes the
/// file is treated as a single episode, so it is at least named as the episode
/// the user identified instead of falling back to a generic file name.
pub fn match_span_from(
    episodes: &[Episode],
    season: u16,
    first: u16,
    segment: Duration,
) -> Option<EpisodeSpan> {
    let span = match_span(episodes, season, segment, None);
    let count = span
        .map(|s| s.last.saturating_sub(s.first).saturating_add(1))
        .unwrap_or(1);
    let last = first.checked_add(count.saturating_sub(1))?;
    let title = (count == 1)
        .then(|| {
            episodes
                .iter()
                .find(|e| e.season == u32::from(season) && e.number == Some(u32::from(first)))
                .and_then(|e| e.name.clone())
        })
        .flatten();

    Some(EpisodeSpan {
        season,
        first,
        last,
        title,
    })
}

/// Minimal percent-encoding for a query string.
fn encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push_str("%20"),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn episode(number: u32, runtime: u32) -> Episode {
        Episode {
            id: number,
            name: Some(format!("Episode {number}")),
            season: 1,
            number: Some(number),
            runtime: Some(runtime),
        }
    }

    #[test]
    fn picks_the_window_by_runtime() {
        let runtimes = [24u32, 25, 23, 26, 24, 25, 24, 23, 25, 24, 26, 23, 24];
        let episodes: Vec<Episode> = runtimes
            .iter()
            .enumerate()
            .map(|(i, r)| episode(i as u32 + 1, *r))
            .collect();

        // Disc carrying episodes 8..=13.
        let segments: Vec<Duration> = runtimes[7..13]
            .iter()
            .map(|m| Duration::from_secs(u64::from(*m) * 60))
            .collect();

        let matched = match_episodes(&episodes, 1, &segments, None);
        let numbers: Vec<u16> = matched.iter().map(|m| m.number).collect();
        assert_eq!(numbers, vec![8, 9, 10, 11, 12, 13]);
    }

    #[test]
    fn disc_hint_wins_when_every_episode_has_the_same_runtime() {
        // A 12-episode season where TVmaze lists one runtime for every
        // episode: the runtime score is identical for every window, so only
        // the disc-derived hint can place disc 2 at episodes 7..=12.
        let episodes: Vec<Episode> = (1..=12).map(|n| episode(n, 24)).collect();
        let segments: Vec<Duration> = (0..6).map(|_| Duration::from_secs(24 * 60)).collect();

        let matched = match_episodes(&episodes, 1, &segments, Some(6));
        let numbers: Vec<u16> = matched.iter().map(|m| m.number).collect();
        assert_eq!(numbers, vec![7, 8, 9, 10, 11, 12]);
    }

    #[test]
    fn disc_hint_replaces_the_start_of_season_fallback_without_runtimes() {
        let episodes: Vec<Episode> = (1..=12)
            .map(|n| Episode {
                runtime: None,
                ..episode(n, 0)
            })
            .collect();
        let segments: Vec<Duration> = (0..6).map(|_| Duration::from_secs(1440)).collect();

        let matched = match_episodes(&episodes, 1, &segments, Some(6));
        let numbers: Vec<u16> = matched.iter().map(|m| m.number).collect();
        assert_eq!(numbers, vec![7, 8, 9, 10, 11, 12]);
    }

    #[test]
    fn runtime_still_beats_a_wrong_hint() {
        // The hint wrongly says this disc starts at episode 1, but the
        // distinctive 40/41-minute runtimes place it at episodes 7..=8, which
        // must win.
        let mut runtimes = [24u32; 8];
        runtimes[6] = 40;
        runtimes[7] = 41;
        let episodes: Vec<Episode> = runtimes
            .iter()
            .enumerate()
            .map(|(i, r)| episode(i as u32 + 1, *r))
            .collect();
        let segments = [Duration::from_secs(40 * 60), Duration::from_secs(41 * 60)];

        let matched = match_episodes(&episodes, 1, &segments, Some(0));
        let numbers: Vec<u16> = matched.iter().map(|m| m.number).collect();
        assert_eq!(numbers, vec![7, 8]);
    }

    #[test]
    fn disc_hint_wins_over_near_equal_runtimes() {
        // Episode runtimes differ by only a minute, so the wrong window (off by
        // one disc) scores marginally better; within the tie margin the disc
        // hint must still win.
        let runtimes = [24u32, 25, 24, 25, 24, 25, 23, 24, 23, 24, 23, 24];
        let episodes: Vec<Episode> = runtimes
            .iter()
            .enumerate()
            .map(|(i, r)| episode(i as u32 + 1, *r))
            .collect();
        let segments: Vec<Duration> = [24u32, 25, 24, 25, 24, 25]
            .iter()
            .map(|m| Duration::from_secs(u64::from(*m) * 60))
            .collect();

        // Window 0 matches exactly, but this is disc 2, so it starts at 6.
        let matched = match_episodes(&episodes, 1, &segments, Some(6));
        let numbers: Vec<u16> = matched.iter().map(|m| m.number).collect();
        assert_eq!(numbers, vec![7, 8, 9, 10, 11, 12]);
    }

    #[test]
    fn falls_back_to_season_start_without_runtimes() {
        let episodes: Vec<Episode> = (1..=5)
            .map(|n| Episode {
                runtime: None,
                ..episode(n, 0)
            })
            .collect();
        let segments: Vec<Duration> = (0..3).map(|_| Duration::from_secs(1440)).collect();

        let matched = match_episodes(&episodes, 1, &segments, None);
        let numbers: Vec<u16> = matched.iter().map(|m| m.number).collect();
        assert_eq!(numbers, vec![1, 2, 3]);
    }

    #[test]
    fn other_seasons_are_ignored() {
        let mut episodes: Vec<Episode> = (1..=3).map(|n| episode(n, 24)).collect();
        episodes.push(Episode {
            season: 2,
            ..episode(1, 24)
        });
        let matched = match_episodes(&episodes, 1, &[Duration::from_secs(1440)], None);
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].number, 1);
    }

    #[test]
    fn matches_a_two_episode_span() {
        let episodes: Vec<Episode> = (1..=6).map(|n| episode(n, 24)).collect();
        // A 48-minute file is two 24-minute episodes.
        let span = match_span(&episodes, 1, Duration::from_secs(48 * 60), None).expect("span");
        assert_eq!((span.first, span.last), (1, 2));
        assert_eq!(span.title, None);
    }

    #[test]
    fn matches_a_single_episode_span_with_title() {
        let episodes: Vec<Episode> = (1..=6).map(|n| episode(n, 24)).collect();
        let span = match_span(&episodes, 1, Duration::from_secs(24 * 60), None).expect("span");
        assert_eq!((span.first, span.last), (1, 1));
        assert_eq!(span.title.as_deref(), Some("Episode 1"));
    }

    #[test]
    fn span_uses_the_disc_hint_when_runtimes_tie() {
        // Every episode is 24 minutes, so every single-episode span scores
        // equally; the hint places disc 2's first file at episode 7.
        let episodes: Vec<Episode> = (1..=12).map(|n| episode(n, 24)).collect();
        let span = match_span(&episodes, 1, Duration::from_secs(24 * 60), Some(6)).expect("span");
        assert_eq!((span.first, span.last), (7, 7));
    }

    #[test]
    fn span_hint_covers_a_multi_episode_file() {
        let episodes: Vec<Episode> = (1..=12).map(|n| episode(n, 24)).collect();
        let span = match_span(&episodes, 1, Duration::from_secs(48 * 60), Some(6)).expect("span");
        assert_eq!((span.first, span.last), (7, 8));
    }

    #[test]
    fn span_hint_wins_over_near_equal_runtimes() {
        // Episode 1 matches the file's 24 minutes exactly, but episode 7 is
        // only a minute off; within the tie margin the hint still wins.
        let mut episodes: Vec<Episode> = (1..=12).map(|n| episode(n, 24)).collect();
        episodes[6].runtime = Some(25);

        let span = match_span(&episodes, 1, Duration::from_secs(24 * 60), Some(6)).expect("span");
        assert_eq!((span.first, span.last), (7, 7));
    }

    #[test]
    fn no_span_when_runtimes_are_missing() {
        let episodes: Vec<Episode> = (1..=3)
            .map(|n| Episode {
                runtime: None,
                ..episode(n, 0)
            })
            .collect();
        assert!(match_span(&episodes, 1, Duration::from_secs(48 * 60), None).is_none());
    }

    #[test]
    fn explicit_start_overrides_the_provider_window() {
        // A last disc the label places at episodes 29-31 even though the
        // provider's season stops at 28.
        let episodes: Vec<Episode> = (1..=6).map(|n| episode(n, 24)).collect();
        let segments: Vec<Duration> = (0..3).map(|_| Duration::from_secs(24 * 60)).collect();

        let matched = match_episodes_from(&episodes, 1, 29, &segments);
        let numbers: Vec<u16> = matched.iter().map(|m| m.number).collect();
        assert_eq!(numbers, vec![29, 30, 31]);
        // The provider has no episodes at those numbers, so there are no titles.
        assert!(matched.iter().all(|m| m.title.is_none()));
    }

    /// A run that leaves the provider's season continues into the next one:
    /// a DVD set whose episode count differs from the provider's still gets
    /// titles for the overflow instead of leaving them blank.
    #[test]
    fn explicit_start_continues_into_the_next_season() {
        let mut episodes: Vec<Episode> = (1..=3).map(|n| episode(n, 24)).collect();
        episodes.extend((1..=2).map(|n| Episode {
            season: 2,
            ..episode(n, 24)
        }));
        let segments: Vec<Duration> = (0..5).map(|_| Duration::from_secs(24 * 60)).collect();

        let matched = match_episodes_from(&episodes, 1, 2, &segments);

        let labels: Vec<(u16, Option<&str>)> = matched
            .iter()
            .map(|m| (m.number, m.title.as_deref()))
            .collect();
        assert_eq!(
            labels,
            vec![
                (2, Some("Episode 2")),
                (3, Some("Episode 3")),
                // Season 1 stopped at 3, so these take season 2's episodes.
                (4, Some("Episode 1")),
                (5, Some("Episode 2")),
                // Past the provider's last episode there is nothing to name.
                (6, None),
            ]
        );
    }

    #[test]
    fn explicit_start_keeps_provider_titles_when_they_line_up() {
        let episodes: Vec<Episode> = (1..=12).map(|n| episode(n, 24)).collect();
        let segments = [Duration::from_secs(24 * 60), Duration::from_secs(24 * 60)];

        let matched = match_episodes_from(&episodes, 1, 7, &segments);
        assert_eq!(matched[0].number, 7);
        assert_eq!(matched[0].title.as_deref(), Some("Episode 7"));
        assert_eq!(matched[1].number, 8);
    }

    #[test]
    fn explicit_span_anchors_at_the_requested_episode() {
        let episodes: Vec<Episode> = (1..=6).map(|n| episode(n, 24)).collect();
        let span = match_span_from(&episodes, 1, 29, Duration::from_secs(48 * 60)).expect("span");
        assert_eq!((span.first, span.last), (29, 30));
        assert_eq!(span.title, None);
    }

    #[test]
    fn explicit_span_defaults_to_one_episode_without_runtimes() {
        let episodes: Vec<Episode> = (1..=6)
            .map(|n| Episode {
                runtime: None,
                ..episode(n, 0)
            })
            .collect();
        let span = match_span_from(&episodes, 1, 4, Duration::from_secs(48 * 60)).expect("span");
        assert_eq!((span.first, span.last), (4, 4));
    }
}

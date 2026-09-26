//! Metadata lookup and mapping ripped segments onto real episodes.
//!
//! TVmaze is the default source because it is free and needs no API key. A
//! TMDb client (which does need a key) can be layered on later for movies and
//! for shows TVmaze does not carry.

use std::time::Duration;

use serde::Deserialize;

const TVMAZE: &str = "https://api.tvmaze.com";

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
/// lands on the right offset; if no runtimes are available we fall back to
/// start-of-season order.
pub fn match_episodes(
    episodes: &[Episode],
    season: u16,
    segments: &[Duration],
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

    let offset = if segments.len() >= season_episodes.len() {
        0
    } else {
        let mut best = (0usize, u64::MAX);
        for start in 0..=(season_episodes.len() - segments.len()) {
            let mut score = 0u64;
            let mut comparable = false;
            for (i, segment) in segments.iter().enumerate() {
                if let Some(runtime) = runtime_of(season_episodes[start + i]) {
                    comparable = true;
                    score += runtime.as_secs().abs_diff(segment.as_secs());
                }
            }
            if comparable && score < best.1 {
                best = (start, score);
            }
        }
        best.0
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
/// `sXXeYY-eZZ` the way Plex expects.
pub fn match_span(episodes: &[Episode], season: u16, segment: Duration) -> Option<EpisodeSpan> {
    let mut season_episodes: Vec<&Episode> = episodes
        .iter()
        .filter(|e| e.season == u32::from(season))
        .collect();
    season_episodes.sort_by_key(|e| e.number.unwrap_or(0));
    if season_episodes.is_empty() {
        return None;
    }

    let target = segment.as_secs_f64().max(1.0);
    let mut best: Option<(f64, usize, usize)> = None; // (error, start, end-exclusive)

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
            if best.map_or(true, |(best_error, _, _)| error < best_error) {
                best = Some((error, start, end + 1));
            }
        }
    }

    let (error, start, end) = best?;
    if error > 0.15 || end <= start {
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

        let matched = match_episodes(&episodes, 1, &segments);
        let numbers: Vec<u16> = matched.iter().map(|m| m.number).collect();
        assert_eq!(numbers, vec![8, 9, 10, 11, 12, 13]);
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

        let matched = match_episodes(&episodes, 1, &segments);
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
        let matched = match_episodes(&episodes, 1, &[Duration::from_secs(1440)]);
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].number, 1);
    }

    #[test]
    fn matches_a_two_episode_span() {
        let episodes: Vec<Episode> = (1..=6).map(|n| episode(n, 24)).collect();
        // A 48-minute file is two 24-minute episodes.
        let span = match_span(&episodes, 1, Duration::from_secs(48 * 60)).expect("span");
        assert_eq!((span.first, span.last), (1, 2));
        assert_eq!(span.title, None);
    }

    #[test]
    fn matches_a_single_episode_span_with_title() {
        let episodes: Vec<Episode> = (1..=6).map(|n| episode(n, 24)).collect();
        let span = match_span(&episodes, 1, Duration::from_secs(24 * 60)).expect("span");
        assert_eq!((span.first, span.last), (1, 1));
        assert_eq!(span.title.as_deref(), Some("Episode 1"));
    }

    #[test]
    fn no_span_when_runtimes_are_missing() {
        let episodes: Vec<Episode> = (1..=3)
            .map(|n| Episode {
                runtime: None,
                ..episode(n, 0)
            })
            .collect();
        assert!(match_span(&episodes, 1, Duration::from_secs(48 * 60)).is_none());
    }
}

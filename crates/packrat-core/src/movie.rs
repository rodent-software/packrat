//! Matching a disc's feature to a real movie.
//!
//! Ranking is pure so it can be unit-tested without a network. TMDb search
//! results carry no runtime, so [`resolve`] fills the runtime in for the
//! shortlist with details calls and re-ranks; only the shortlist costs extra
//! requests.

use std::time::Duration;

use strsim::normalized_levenshtein;

use crate::meta::MetaError;
use crate::tmdb::{self, Movie};

/// Best score that is trusted without asking the user to choose.
pub const AUTO_ACCEPT: f64 = 0.75;
/// How many search hits get a details call and appear in the prompt list.
pub const CANDIDATES: usize = 5;

/// What we know about the disc's feature before matching.
#[derive(Debug, Clone)]
pub struct MovieQuery<'a> {
    pub title: &'a str,
    pub year: Option<u16>,
    pub runtime: Option<Duration>,
}

impl<'a> MovieQuery<'a> {
    pub fn new(title: &'a str, year: Option<u16>, runtime: Option<Duration>) -> Self {
        Self {
            title,
            year,
            runtime,
        }
    }
}

/// Ranked candidates, best first, each with its 0..=1 score.
#[derive(Debug, Clone, Default)]
pub struct MovieResolution {
    pub candidates: Vec<(f64, Movie)>,
}

impl MovieResolution {
    pub fn best(&self) -> Option<&Movie> {
        self.candidates.first().map(|(_, movie)| movie)
    }

    /// The best candidate when it is trusted without a prompt.
    pub fn auto_accepted(&self) -> Option<&Movie> {
        self.candidates
            .first()
            .filter(|(score, _)| *score >= AUTO_ACCEPT)
            .map(|(_, movie)| movie)
    }

    /// True when there is a plausible match the user should choose between.
    pub fn needs_prompt(&self) -> bool {
        self.best().is_some() && self.auto_accepted().is_none()
    }
}

/// Score and order candidates without touching the network.
///
/// Returns `(score, index-into-candidates)` best first; the index is stable so
/// callers can fill in runtimes and re-rank.
pub fn rank(candidates: &[Movie], query: &MovieQuery) -> Vec<(f64, usize)> {
    let mut scored: Vec<(f64, usize)> = candidates
        .iter()
        .enumerate()
        .map(|(index, movie)| (score(movie, query), index))
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored
}

/// Resolve the disc's feature against TMDb.
pub fn resolve(key: &str, query: &MovieQuery) -> Result<MovieResolution, MetaError> {
    let mut candidates = tmdb::search_movies(key, query.title, query.year)?;
    if candidates.is_empty() {
        return Ok(MovieResolution::default());
    }

    let mut ranked = rank(&candidates, query);
    ranked.truncate(CANDIDATES);

    // Search results have no runtime; fetch it for the shortlist so runtime
    // can separate same-title remakes and cuts.
    if query.runtime.is_some() {
        for (_, index) in &ranked {
            if candidates[*index].runtime.is_none() {
                if let Ok(details) = tmdb::movie_details(key, candidates[*index].id) {
                    candidates[*index].runtime = details.runtime;
                }
            }
        }
        ranked = rank(&candidates, query);
        ranked.truncate(CANDIDATES);
    }

    let candidates = ranked
        .into_iter()
        .map(|(score, index)| (score, candidates[index].clone()))
        .collect();
    Ok(MovieResolution { candidates })
}

/// Weighted blend of title, year and runtime agreement.
fn score(movie: &Movie, query: &MovieQuery) -> f64 {
    let title = title_score(query.title, movie);
    let year = year_score(query.year, movie.year());
    let runtime = runtime_score(query.runtime, movie.runtime());
    let popularity = movie.popularity.unwrap_or(0.0).clamp(0.0, 100.0) / 100.0;
    0.60 * title + 0.20 * year + 0.18 * runtime + 0.02 * popularity
}

/// Best normalized similarity against the title and its original title.
fn title_score(query: &str, movie: &Movie) -> f64 {
    let query = normalize(query);
    let mut best = normalized_levenshtein(&query, &normalize(&movie.title));
    if let Some(original) = &movie.original_title {
        best = best.max(normalized_levenshtein(&query, &normalize(original)));
    }
    best
}

fn year_score(query: Option<u16>, candidate: Option<u16>) -> f64 {
    match (query, candidate) {
        (Some(a), Some(b)) if a == b => 1.0,
        (Some(a), Some(b)) if a.abs_diff(b) == 1 => 0.7,
        (Some(a), Some(b)) if a.abs_diff(b) <= 3 => 0.3,
        (Some(_), Some(_)) => 0.0,
        // No year on either side is no evidence either way.
        _ => 0.5,
    }
}

fn runtime_score(query: Option<Duration>, candidate: Option<Duration>) -> f64 {
    let (Some(query), Some(candidate)) = (query, candidate) else {
        return 0.5;
    };
    let diff = query.as_secs().abs_diff(candidate.as_secs());
    if diff <= 3 * 60 {
        1.0
    } else if diff <= 8 * 60 {
        0.7
    } else if diff * 100 <= query.as_secs().max(1) * 15 {
        0.4
    } else {
        0.0
    }
}

/// Fold case, articles and punctuation so `The Matrix` matches `Matrix, The`.
fn normalize(input: &str) -> String {
    let folded = input
        .to_lowercase()
        .replace('&', " and ")
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>();
    let mut words: Vec<&str> = folded.split_whitespace().collect();
    if words.len() > 1 && matches!(words[0], "the" | "a" | "an") {
        words.remove(0);
    }
    words.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn movie(id: u32, title: &str, year: u16, runtime: u32) -> Movie {
        Movie {
            id,
            title: title.into(),
            original_title: None,
            release_date: Some(format!("{year}-01-01")),
            runtime: Some(runtime),
            popularity: None,
        }
    }

    fn query<'a>(title: &'a str, year: Option<u16>, runtime: Option<u32>) -> MovieQuery<'a> {
        MovieQuery::new(
            title,
            year,
            runtime.map(|m| Duration::from_secs(u64::from(m) * 60)),
        )
    }

    #[test]
    fn exact_title_and_year_auto_accepts() {
        let candidates = vec![movie(603, "The Matrix", 1999, 136)];
        let ranked = rank(&candidates, &query("The Matrix", Some(1999), Some(136)));
        assert_eq!(ranked[0].1, 0);
        assert!(ranked[0].0 >= AUTO_ACCEPT, "score {}", ranked[0].0);

        let resolution = MovieResolution {
            candidates: ranked
                .into_iter()
                .map(|(s, i)| (s, candidates[i].clone()))
                .collect(),
        };
        assert!(resolution.auto_accepted().is_some());
        assert!(!resolution.needs_prompt());
    }

    #[test]
    fn remake_is_disambiguated_by_year() {
        let candidates = vec![
            movie(1, "The Thing", 1982, 109),
            movie(2, "The Thing", 2011, 103),
        ];
        let ranked = rank(&candidates, &query("The Thing", Some(2011), Some(103)));
        assert_eq!(ranked[0].1, 1, "the 2011 remake should win");
        assert!(ranked[0].0 > ranked[1].0);
    }

    #[test]
    fn runtime_breaks_a_same_title_and_year_tie() {
        // Same title and year, different cuts; the disc's runtime picks one.
        let candidates = vec![
            movie(1, "Blade Runner", 1982, 117),
            movie(2, "Blade Runner", 1982, 130),
        ];
        let ranked = rank(&candidates, &query("Blade Runner", Some(1982), Some(130)));
        assert_eq!(ranked[0].1, 1);
    }

    #[test]
    fn normalizes_articles_and_punctuation() {
        let candidates = [movie(1, "The Matrix", 1999, 136)];
        // "Matrix" and "The Matrix" must normalize to the same string.
        assert!(title_score("Matrix", &candidates[0]) > 0.99);
        assert!(title_score("the matrix", &candidates[0]) > 0.99);
    }

    #[test]
    fn a_weak_match_needs_a_prompt() {
        let candidates = vec![movie(1, "Completely Different Film", 1974, 90)];
        let ranked = rank(&candidates, &query("The Matrix", Some(1999), Some(136)));
        let resolution = MovieResolution {
            candidates: ranked
                .into_iter()
                .map(|(s, i)| (s, candidates[i].clone()))
                .collect(),
        };
        assert!(resolution.auto_accepted().is_none());
        assert!(resolution.needs_prompt());
    }

    #[test]
    fn an_empty_resolution_is_neither_accepted_nor_prompted() {
        let resolution = MovieResolution::default();
        assert!(resolution.best().is_none());
        assert!(resolution.auto_accepted().is_none());
        assert!(!resolution.needs_prompt());
    }

    #[test]
    fn missing_year_or_runtime_is_neutral() {
        let candidates = vec![movie(1, "The Matrix", 1999, 136)];
        // Disc has no year or runtime: title alone must still auto-accept.
        let ranked = rank(&candidates, &query("The Matrix", None, None));
        assert!(ranked[0].0 >= AUTO_ACCEPT, "score {}", ranked[0].0);
    }
}

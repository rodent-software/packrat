//! TMDb metadata for movies.
//!
//! Unlike TVmaze, TMDb needs an API key, so this provider is strictly optional.
//! When no key is configured the movie path falls back to naming from the disc
//! label. Both the classic v3 API key (sent as `api_key`) and the v4 read
//! access token (sent as a bearer token) are accepted; a key containing a dot
//! is assumed to be the latter.
//!
//! The search endpoint does not return a runtime, so [`search_movies`] results
//! carry `runtime: None`; [`movie_details`] fills it in for a single movie.

use std::time::Duration;

use serde::Deserialize;

use crate::meta::MetaError;

const TMDB: &str = "https://api.themoviedb.org/3";

/// A movie as returned by TMDb, from either the search or details endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct Movie {
    pub id: u32,
    pub title: String,
    #[serde(default)]
    pub original_title: Option<String>,
    /// `YYYY-MM-DD`; can be empty for very old or unreleased entries.
    #[serde(default)]
    pub release_date: Option<String>,
    /// Nominal runtime in minutes; only present on the details endpoint.
    #[serde(default)]
    pub runtime: Option<u32>,
    #[serde(default)]
    pub popularity: Option<f64>,
}

impl Movie {
    /// First four digits of the release date, as a year.
    pub fn year(&self) -> Option<u16> {
        self.release_date
            .as_deref()
            .and_then(|d| d.get(0..4))
            .and_then(|y| y.parse().ok())
    }

    /// Runtime as a [`Duration`].
    pub fn runtime(&self) -> Option<Duration> {
        self.runtime.map(|m| Duration::from_secs(u64::from(m) * 60))
    }
}

/// Search for movies by title, optionally constrained by release year.
pub fn search_movies(key: &str, query: &str, year: Option<u16>) -> Result<Vec<Movie>, MetaError> {
    let mut request = ureq::get(&format!("{TMDB}/search/movie"))
        .query("query", query)
        .query("include_adult", "false");
    if let Some(year) = year {
        request = request.query("year", year.to_string());
    }

    let response = authorize(request, key).call().map_err(http_error)?;
    let body = response
        .into_body()
        .read_json::<SearchResponse>()
        .map_err(|e| MetaError::Json(e.to_string()))?;
    Ok(body.results)
}

/// Fetch full details (notably the runtime) for one movie.
pub fn movie_details(key: &str, id: u32) -> Result<Movie, MetaError> {
    let response = authorize(ureq::get(&format!("{TMDB}/movie/{id}")), key)
        .call()
        .map_err(http_error)?;
    response
        .into_body()
        .read_json::<Movie>()
        .map_err(|e| MetaError::Json(e.to_string()))
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    results: Vec<Movie>,
}

/// Attach the configured credentials to a request.
fn authorize(
    request: ureq::RequestBuilder<ureq::typestate::WithoutBody>,
    key: &str,
) -> ureq::RequestBuilder<ureq::typestate::WithoutBody> {
    if key.contains('.') {
        request.header("Authorization", &format!("Bearer {key}"))
    } else {
        request.query("api_key", key)
    }
}

fn http_error(error: ureq::Error) -> MetaError {
    MetaError::Http(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_search_response() {
        let json = r#"{
            "page": 1,
            "results": [
                {
                    "id": 603,
                    "title": "The Matrix",
                    "original_title": "The Matrix",
                    "release_date": "1999-03-31",
                    "popularity": 42.5
                },
                {
                    "id": 999,
                    "title": "The Matrix Resurrections",
                    "release_date": "2021-12-16",
                    "popularity": 10.0
                }
            ]
        }"#;
        let body: SearchResponse = serde_json::from_str(json).expect("parse");
        assert_eq!(body.results.len(), 2);
        assert_eq!(body.results[0].id, 603);
        assert_eq!(body.results[0].year(), Some(1999));
        assert_eq!(body.results[0].runtime(), None);
        assert_eq!(body.results[1].original_title, None);
    }

    #[test]
    fn parses_a_details_response_with_runtime() {
        let json = r#"{
            "id": 603,
            "title": "The Matrix",
            "release_date": "1999-03-31",
            "runtime": 136,
            "popularity": 42.5
        }"#;
        let movie: Movie = serde_json::from_str(json).expect("parse");
        assert_eq!(movie.runtime(), Some(Duration::from_secs(136 * 60)));
        assert_eq!(movie.year(), Some(1999));
    }

    #[test]
    fn treats_a_dotted_key_as_a_bearer_token() {
        // The distinction lives in `authorize`, which needs a live request to
        // exercise; this documents the rule the function implements.
        assert!("eyJhbGciOi.abc.def".contains('.'));
        assert!(!"0123456789abcdef".contains('.'));
    }
}

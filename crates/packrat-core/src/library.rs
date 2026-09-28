//! Building Plex/Jellyfin-compatible paths.
//!
//! The default helpers insert Plex's conventional category folder under a
//! library root:
//!
//! ```text
//! <root>/Movies/<Title> (<Year>)/<Title> (<Year>).mkv
//! <root>/TV Shows/<Show> (<Year>)/Season 01/<Show> (<Year>) - s01e01 - <Episode>.mkv
//! ```
//!
//! The `*_in` helpers instead take the movie or TV directory itself — the
//! folder that already holds movie or show folders, such as an existing Plex
//! library pointed at `/mnt/media/movies` and `/mnt/media/tv` — and insert no
//! category component.

use std::path::{Path, PathBuf};

/// Characters that are illegal or unhelpful in file names on at least one of
/// Linux/macOS/Windows.
const ILLEGAL: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|', '\0'];

/// Make a string safe to use as a single path component.
pub fn sanitize_component(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_control() || ILLEGAL.contains(&ch) {
            out.push('-');
        } else {
            out.push(ch);
        }
    }
    // Collapse whitespace and trim trailing dots/spaces (problematic on
    // Windows).
    let collapsed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .trim_matches(|c: char| c == '.' || c == ' ')
        .to_string()
}

/// `"Title (Year)"`, or just `"Title"` when the year is unknown.
pub fn display_name(title: &str, year: Option<u16>) -> String {
    let title = sanitize_component(title);
    match year {
        Some(year) => format!("{title} ({year})"),
        None => title,
    }
}

/// Movie folder inside `movies_dir`: `<movies_dir>/<Title (Year)>`.
///
/// Unlike [`movie_dir`] this inserts no `Movies/` component: `movies_dir` is
/// already the directory that holds movie folders (an existing Plex movie
/// library, for example).
pub fn movie_dir_in(movies_dir: &Path, title: &str, year: Option<u16>) -> PathBuf {
    movies_dir.join(display_name(title, year))
}

/// Movie file inside `movies_dir`:
/// `<movies_dir>/<Title (Year)>/<Title (Year)>.mkv`.
pub fn movie_file_in(movies_dir: &Path, title: &str, year: Option<u16>) -> PathBuf {
    let name = display_name(title, year);
    movie_dir_in(movies_dir, title, year).join(format!("{name}.mkv"))
}

/// One part of a movie delivered across several files, inside `movies_dir`:
/// `<movies_dir>/<Title (Year)>/<Title (Year)> - partN.mkv` (the form Plex and
/// Jellyfin both treat as one movie).
pub fn movie_part_file_in(movies_dir: &Path, title: &str, year: Option<u16>, part: u16) -> PathBuf {
    let name = display_name(title, year);
    movie_dir_in(movies_dir, title, year).join(format!("{name} - part{part}.mkv"))
}

/// Extra (trailer/featurette) belonging to a movie:
/// `<movies_dir>/<Title (Year)>/Other/<Description>.mkv`.
///
/// `Other/` is an extras folder Plex, Jellyfin and Emby all recognise, the
/// same choice [`extra_file_in`] makes for TV.
pub fn movie_extra_file_in(
    movies_dir: &Path,
    title: &str,
    year: Option<u16>,
    description: &str,
) -> PathBuf {
    let name = sanitize_component(description);
    movie_dir_in(movies_dir, title, year)
        .join("Other")
        .join(format!("{name}.mkv"))
}

/// Show folder inside `tv_dir`: `<tv_dir>/<Show (Year)>`.
pub fn show_dir_in(tv_dir: &Path, show: &str, year: Option<u16>) -> PathBuf {
    tv_dir.join(display_name(show, year))
}

/// Season folder inside `tv_dir`: `<tv_dir>/<Show (Year)>/Season NN`.
pub fn season_dir_in(tv_dir: &Path, show: &str, year: Option<u16>, season: u16) -> PathBuf {
    show_dir_in(tv_dir, show, year).join(format!("Season {season:02}"))
}

/// The episode number in a Plex-style file name for `season`, e.g. `28` from
/// `Dragon Ball (1986) - s01e28 - The Final Blow.mkv`.
///
/// Multi-episode names (`s01e02-e03`) yield the last episode in the span, so a
/// library's highest episode is the end of what has been backed up. Returns
/// `None` for a name that is not an episode of `season`.
pub fn episode_number_from_file_name(file_name: &str, season: u16) -> Option<u16> {
    let lower = file_name.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i + 4 < bytes.len() {
        if bytes[i] == b's'
            && bytes[i + 1].is_ascii_digit()
            && bytes[i + 2].is_ascii_digit()
            && bytes[i + 3] == b'e'
            && lower[i + 1..i + 3].parse::<u16>() == Ok(season)
        {
            let mut best = None;
            let mut j = i + 4;
            loop {
                let start = j;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j == start {
                    break;
                }
                if let Ok(number) = lower[start..j].parse::<u16>() {
                    best = Some(best.map_or(number, |b: u16| b.max(number)));
                }
                // Step over the `-e` of a multi-episode name, then keep going.
                if bytes.get(j) == Some(&b'-') && bytes.get(j + 1) == Some(&b'e') {
                    j += 2;
                } else {
                    break;
                }
            }
            if best.is_some() {
                return best;
            }
        }
        i += 1;
    }
    None
}

/// Highest episode already present in `dir` for `season`, when it holds any
/// named episodes. Used to place a later disc after the episodes backed up
/// from the earlier ones, which the label alone cannot tell us.
pub fn highest_episode(dir: &Path, season: u16) -> Option<u16> {
    let mut highest = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if let Some(number) = episode_number_from_file_name(name, season) {
            highest = Some(highest.map_or(number, |b: u16| b.max(number)));
        }
    }
    highest
}

/// Episodes already present in the show's seasons *before* `season`, summed
/// over their season folders.
///
/// This is the show's global episode position at the start of `season` as the
/// discs number it. A provider may split the show at different points, so a
/// disc's episode number cannot be looked up in the provider's season of the
/// same name; the episodes before it can still be counted, though, and the
/// display number is what a disc set already gave us.
pub fn episodes_before_season(show_dir: &Path, season: u16) -> usize {
    let Ok(entries) = std::fs::read_dir(show_dir) else {
        return 0;
    };
    let mut total = 0usize;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(number) = season_number_from_dir(name) else {
            continue;
        };
        if number < season {
            total += usize::from(highest_episode(&entry.path(), number).unwrap_or(0));
        }
    }
    total
}

/// The number in a `Season NN` folder name.
fn season_number_from_dir(name: &str) -> Option<u16> {
    name.strip_prefix("Season ")?.trim().parse().ok()
}

/// Extra (featurette/trailer/etc.) belonging to a show:
/// `<tv_dir>/<Show (Year)>/Other/<Description>.mkv`.
///
/// `Other/` is one of the extras folders Plex, Jellyfin and Emby all
/// recognise; without a way to classify an extra further it is the honest
/// choice.
pub fn extra_file_in(tv_dir: &Path, show: &str, year: Option<u16>, description: &str) -> PathBuf {
    let name = sanitize_component(description);
    show_dir_in(tv_dir, show, year)
        .join("Other")
        .join(format!("{name}.mkv"))
}

/// Episode file inside `tv_dir`, with the episode title when known:
/// `<Show (Year)> - sXXeYY - <Episode>.mkv`.
pub fn episode_file_in(
    tv_dir: &Path,
    show: &str,
    year: Option<u16>,
    season: u16,
    episode: u16,
    episode_title: Option<&str>,
) -> PathBuf {
    let show_name = display_name(show, year);
    let mut name = format!("{show_name} - s{season:02}e{episode:02}");
    if let Some(title) = episode_title {
        let title = sanitize_component(title);
        if !title.is_empty() {
            name.push_str(" - ");
            name.push_str(&title);
        }
    }
    season_dir_in(tv_dir, show, year, season).join(format!("{name}.mkv"))
}

/// Episode file spanning more than one episode, inside `tv_dir`:
/// `<Show (Year)> - sXXeYY-eZZ - <Title>.mkv` (Plex's multi-episode form).
pub fn episode_range_file_in(
    tv_dir: &Path,
    show: &str,
    year: Option<u16>,
    season: u16,
    first: u16,
    last: u16,
    title: Option<&str>,
) -> PathBuf {
    if first == last {
        return episode_file_in(tv_dir, show, year, season, first, title);
    }

    let show_name = display_name(show, year);
    let mut name = format!("{show_name} - s{season:02}e{first:02}-e{last:02}");
    if let Some(title) = title {
        let title = sanitize_component(title);
        if !title.is_empty() {
            name.push_str(" - ");
            name.push_str(&title);
        }
    }
    season_dir_in(tv_dir, show, year, season).join(format!("{name}.mkv"))
}

/// Movie folder: `<root>/Movies/<Title (Year)>`.
pub fn movie_dir(root: &Path, title: &str, year: Option<u16>) -> PathBuf {
    movie_dir_in(&root.join("Movies"), title, year)
}

/// Movie file: `<root>/Movies/<Title (Year)>/<Title (Year)>.mkv`.
pub fn movie_file(root: &Path, title: &str, year: Option<u16>) -> PathBuf {
    movie_file_in(&root.join("Movies"), title, year)
}

/// Show folder: `<root>/TV Shows/<Show (Year)>`.
pub fn show_dir(root: &Path, show: &str, year: Option<u16>) -> PathBuf {
    show_dir_in(&root.join("TV Shows"), show, year)
}

/// Season folder: `<root>/TV Shows/<Show (Year)>/Season NN`.
pub fn season_dir(root: &Path, show: &str, year: Option<u16>, season: u16) -> PathBuf {
    season_dir_in(&root.join("TV Shows"), show, year, season)
}

/// Extra (featurette/trailer/etc.) belonging to a show:
/// `<root>/TV Shows/<Show (Year)>/Other/<Description>.mkv`.
pub fn extra_file(root: &Path, show: &str, year: Option<u16>, description: &str) -> PathBuf {
    extra_file_in(&root.join("TV Shows"), show, year, description)
}

/// Episode file, with the episode title when known:
/// `<Show (Year)> - sXXeYY - <Episode>.mkv`.
pub fn episode_file(
    root: &Path,
    show: &str,
    year: Option<u16>,
    season: u16,
    episode: u16,
    episode_title: Option<&str>,
) -> PathBuf {
    episode_file_in(
        &root.join("TV Shows"),
        show,
        year,
        season,
        episode,
        episode_title,
    )
}

/// Episode file spanning more than one episode:
/// `<Show (Year)> - sXXeYY-eZZ - <Title>.mkv` (Plex's multi-episode form).
pub fn episode_range_file(
    root: &Path,
    show: &str,
    year: Option<u16>,
    season: u16,
    first: u16,
    last: u16,
    title: Option<&str>,
) -> PathBuf {
    episode_range_file_in(
        &root.join("TV Shows"),
        show,
        year,
        season,
        first,
        last,
        title,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn sanitizes_illegal_characters() {
        assert_eq!(sanitize_component("AC/DC: Live?"), "AC-DC- Live-");
        assert_eq!(sanitize_component("  trailing.  "), "trailing");
    }

    #[test]
    fn builds_movie_paths() {
        let path = movie_file(Path::new("/lib"), "The Matrix", Some(1999));
        assert_eq!(
            path,
            Path::new("/lib/Movies/The Matrix (1999)/The Matrix (1999).mkv")
        );
    }

    #[test]
    fn builds_movie_part_and_extra_paths() {
        let part = movie_part_file_in(Path::new("/movies"), "The Matrix", Some(1999), 2);
        assert_eq!(
            part,
            Path::new("/movies/The Matrix (1999)/The Matrix (1999) - part2.mkv")
        );

        let extra = movie_extra_file_in(
            Path::new("/movies"),
            "The Matrix",
            Some(1999),
            "Behind The Scenes",
        );
        assert_eq!(
            extra,
            Path::new("/movies/The Matrix (1999)/Other/Behind The Scenes.mkv")
        );
    }

    #[test]
    fn builds_episode_paths() {
        let path = episode_file(
            Path::new("/lib"),
            "Dragon Ball",
            Some(1986),
            1,
            3,
            Some("The Nimbus Cloud of Roshi"),
        );
        assert_eq!(
            path,
            Path::new(
                "/lib/TV Shows/Dragon Ball (1986)/Season 01/\
                 Dragon Ball (1986) - s01e03 - The Nimbus Cloud of Roshi.mkv"
            )
        );
    }

    #[test]
    fn builds_episode_paths_without_title() {
        let path = episode_file(Path::new("/lib"), "Firefly", None, 2, 14, None);
        assert_eq!(
            path,
            Path::new("/lib/TV Shows/Firefly/Season 02/Firefly - s02e14.mkv")
        );
    }

    #[test]
    fn builds_multi_episode_paths() {
        let path = episode_range_file(Path::new("/lib"), "Firefly", None, 1, 2, 3, None);
        assert_eq!(
            path,
            Path::new("/lib/TV Shows/Firefly/Season 01/Firefly - s01e02-e03.mkv")
        );
        // A single-episode range falls back to the plain episode name.
        let single = episode_range_file(Path::new("/lib"), "Firefly", None, 1, 2, 2, None);
        assert_eq!(
            single,
            Path::new("/lib/TV Shows/Firefly/Season 01/Firefly - s01e02.mkv")
        );
    }

    #[test]
    fn builds_paths_directly_inside_a_configured_directory() {
        // The shape of an existing Plex library: the destination is already
        // the folder that holds shows/movies, so no category folder is added.
        let episode = episode_file_in(
            Path::new("/mnt/dvd/media/tv"),
            "Dragon Ball",
            Some(1986),
            1,
            3,
            Some("The Nimbus Cloud of Roshi"),
        );
        assert_eq!(
            episode,
            Path::new(
                "/mnt/dvd/media/tv/Dragon Ball (1986)/Season 01/\
                 Dragon Ball (1986) - s01e03 - The Nimbus Cloud of Roshi.mkv"
            )
        );

        let movie = movie_file_in(Path::new("/mnt/dvd/media/movies"), "The Matrix", Some(1999));
        assert_eq!(
            movie,
            Path::new("/mnt/dvd/media/movies/The Matrix (1999)/The Matrix (1999).mkv")
        );

        let extra = extra_file_in(
            Path::new("/mnt/dvd/media/tv"),
            "Firefly",
            None,
            "Deleted Scenes",
        );
        assert_eq!(
            extra,
            Path::new("/mnt/dvd/media/tv/Firefly/Other/Deleted Scenes.mkv")
        );
    }

    #[test]
    fn parses_episode_numbers_from_plex_file_names() {
        assert_eq!(
            episode_number_from_file_name("Dragon Ball (1986) - s01e28 - The Final Blow.mkv", 1),
            Some(28)
        );
        assert_eq!(
            episode_number_from_file_name("Show - s02e03.mkv", 1),
            None,
            "another season's episode is ignored"
        );
        assert_eq!(
            episode_number_from_file_name("Show - s01e02-e03.mkv", 1),
            Some(3),
            "a multi-episode name yields the last episode"
        );
        assert_eq!(episode_number_from_file_name("not an episode.mkv", 1), None);
    }

    #[test]
    fn highest_episode_reads_the_season_folder() {
        let dir = std::env::temp_dir().join(format!("packrat-library-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Show - s01e07 - Seven.mkv"), b"x").unwrap();
        std::fs::write(dir.join("Show - s01e03 - Three.mkv"), b"x").unwrap();
        std::fs::write(dir.join("Show - s01e02-e03 - Double.mkv"), b"x").unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();

        assert_eq!(highest_episode(&dir, 1), Some(7));
        assert_eq!(highest_episode(&dir, 2), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn episodes_before_a_season_sum_the_earlier_folders() {
        let dir = std::env::temp_dir().join(format!("packrat-before-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (season, highest) in [(1u16, 28u16), (2, 5), (3, 1)] {
            let season_dir = dir.join(format!("Season {season:02}"));
            std::fs::create_dir_all(&season_dir).unwrap();
            std::fs::write(
                season_dir.join(format!("Show - s{season:02}e{highest:02}.mkv")),
                b"x",
            )
            .unwrap();
        }

        assert_eq!(episodes_before_season(&dir, 1), 0);
        assert_eq!(episodes_before_season(&dir, 2), 28);
        assert_eq!(episodes_before_season(&dir, 3), 33);
        assert_eq!(episodes_before_season(&dir, 4), 34);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

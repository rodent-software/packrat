//! Summary statistics for the library packrat has already backed up.
//!
//! Everything here comes from the filesystem layout packrat writes —
//! `<tv_dir>/<Show (Year)>/Season NN/*.mkv` and
//! `<movie_dir>/<Title (Year)>/*.mkv` — and no file contents are read. That
//! keeps a scan cheap enough to run on a worker thread whenever the guide needs
//! to refresh its header.

use std::path::Path;
use std::time::SystemTime;

/// How deep a media folder is walked. A real library nests a movie or a season
/// a couple of levels down; the cap only stops a pathological tree (or a
/// directory symlink the walk failed to notice) from stalling the scan.
const MAX_DEPTH: usize = 6;

/// Counts and sizes for the shows and movies already on disk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LibraryStats {
    /// Shows holding at least one `Season NN` folder.
    pub shows: u64,
    /// `Season NN` folders across every show.
    pub seasons: u64,
    /// Video files inside those season folders.
    pub episodes: u64,
    /// Movie folders (or loose movie files) holding at least one video.
    pub movies: u64,
    /// Bytes of video under the TV directory.
    pub tv_bytes: u64,
    /// Bytes of video under the movie directory.
    pub movie_bytes: u64,
    /// Most recent write time across every counted file.
    pub newest: Option<SystemTime>,
}

impl LibraryStats {
    /// Bytes across both destinations.
    pub fn total_bytes(&self) -> u64 {
        self.tv_bytes + self.movie_bytes
    }

    /// Whether the scan found nothing at all, so the header can stay quiet.
    pub fn is_empty(&self) -> bool {
        self.shows == 0 && self.seasons == 0 && self.episodes == 0 && self.movies == 0
    }
}

/// Walk the configured destinations and summarise what is already backed up.
///
/// A missing or unreadable directory simply contributes nothing, so the guide
/// still starts on a machine whose library is not mounted yet.
pub fn scan(tv_dir: Option<&Path>, movie_dir: Option<&Path>) -> LibraryStats {
    let mut stats = LibraryStats::default();
    if let Some(dir) = tv_dir {
        scan_tv(dir, &mut stats);
    }
    if let Some(dir) = movie_dir {
        scan_movies(dir, &mut stats);
    }
    stats
}

/// Count shows/seasons/episodes and TV bytes under `<tv_dir>`.
fn scan_tv(dir: &Path, stats: &mut LibraryStats) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        // A show counts once it contains at least one season folder.
        let seasons_before = stats.seasons;
        walk_tv(&entry.path(), false, stats, MAX_DEPTH);
        if stats.seasons > seasons_before {
            stats.shows += 1;
        }
    }
}

/// Recurse a show folder, counting videos that live in a `Season NN` folder.
fn walk_tv(path: &Path, in_season: bool, stats: &mut LibraryStats, depth: usize) {
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            let season = is_season_dir(&entry.file_name().to_string_lossy());
            if season && !in_season {
                stats.seasons += 1;
            }
            walk_tv(&entry.path(), in_season || season, stats, depth - 1);
        } else if file_type.is_file() && is_video(&entry.path()) {
            if in_season {
                stats.episodes += 1;
            }
            add_video(&entry, &mut stats.tv_bytes, &mut stats.newest);
        }
    }
}

/// Count movies and movie bytes under `<movie_dir>`.
fn scan_movies(dir: &Path, stats: &mut LibraryStats) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            let mut found = false;
            walk_movie(
                &entry.path(),
                &mut found,
                &mut stats.movie_bytes,
                &mut stats.newest,
                MAX_DEPTH,
            );
            if found {
                stats.movies += 1;
            }
        } else if file_type.is_file() && is_video(&entry.path()) {
            // A movie stored as a loose file directly in the destination.
            stats.movies += 1;
            add_video(&entry, &mut stats.movie_bytes, &mut stats.newest);
        }
    }
}

/// Recurse a movie folder, adding every video and noting whether any was found.
fn walk_movie(
    path: &Path,
    found: &mut bool,
    bytes: &mut u64,
    newest: &mut Option<SystemTime>,
    depth: usize,
) {
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            walk_movie(&entry.path(), found, bytes, newest, depth - 1);
        } else if file_type.is_file() && is_video(&entry.path()) {
            *found = true;
            add_video(&entry, bytes, newest);
        }
    }
}

/// Add one video file's size and write time to the running totals.
fn add_video(entry: &std::fs::DirEntry, bytes: &mut u64, newest: &mut Option<SystemTime>) {
    if let Ok(metadata) = entry.metadata() {
        *bytes += metadata.len();
        if let Ok(modified) = metadata.modified() {
            *newest = Some(match *newest {
                Some(previous) => previous.max(modified),
                None => modified,
            });
        }
    }
}

/// Whether a folder name is a Plex season folder such as `Season 01`.
fn is_season_dir(name: &str) -> bool {
    let Some(rest) = name
        .strip_prefix("Season ")
        .or_else(|| name.strip_prefix("season "))
    else {
        return false;
    };
    !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
}

/// Video container extensions worth counting in an existing library.
fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "mkv"
                    | "mp4"
                    | "m4v"
                    | "avi"
                    | "mov"
                    | "ts"
                    | "wmv"
                    | "webm"
                    | "mpg"
                    | "mpeg"
                    | "flv"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// A unique scratch tree for one test.
    fn tree(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("packrat-stats-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create tree");
        dir
    }

    fn write(path: &Path, bytes: usize) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, vec![0u8; bytes]).unwrap();
    }

    #[test]
    fn counts_shows_seasons_episodes_and_movies() {
        let root = tree("counts");
        let tv = root.join("tv");
        let movies = root.join("movies");

        write(&tv.join("Show (2020)/Season 01/e01.mkv"), 10);
        write(&tv.join("Show (2020)/Season 01/e02.mkv"), 20);
        write(&tv.join("Show (2020)/Season 02/e01.mkv"), 30);
        write(&tv.join("Show (2020)/Other/extra.mkv"), 40);
        // A non-video file must not inflate an episode count.
        write(&tv.join("Show (2020)/Season 01/poster.jpg"), 5);
        // A show with a season but no episodes still counts its season.
        write(&tv.join("Empty (2021)/Season 01/.keep"), 1);

        write(&movies.join("The Matrix (1999)/The Matrix (1999).mkv"), 100);
        write(&movies.join("The Matrix (1999)/Other/featurette.mkv"), 50);
        write(&movies.join("Loose.mkv"), 25);
        write(&movies.join("poster.jpg"), 5);

        let stats = scan(Some(&tv), Some(&movies));
        assert_eq!(stats.shows, 2);
        assert_eq!(stats.seasons, 3);
        assert_eq!(stats.episodes, 3);
        assert_eq!(stats.movies, 2);
        assert_eq!(stats.tv_bytes, 10 + 20 + 30 + 40);
        assert_eq!(stats.movie_bytes, 100 + 50 + 25);
        assert_eq!(stats.total_bytes(), 275);
        assert!(stats.newest.is_some());
        assert!(!stats.is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn handles_a_missing_or_empty_library() {
        let empty = scan(None, None);
        assert!(empty.is_empty());
        assert_eq!(empty.total_bytes(), 0);

        let missing = scan(Some(Path::new("/definitely/not/a/library")), None);
        assert!(missing.is_empty());
    }

    #[test]
    fn recognises_season_folders_and_video_files() {
        assert!(is_season_dir("Season 01"));
        assert!(is_season_dir("Season 12"));
        assert!(!is_season_dir("Season "));
        assert!(!is_season_dir("Specials"));
        assert!(!is_season_dir("Season One"));

        assert!(is_video(Path::new("a.mkv")));
        assert!(is_video(Path::new("a.MP4")));
        assert!(!is_video(Path::new("a.jpg")));
        assert!(!is_video(Path::new("a")));
    }
}

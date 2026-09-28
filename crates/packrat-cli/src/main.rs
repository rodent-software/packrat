//! `packrat` command-line entry point.

mod config;
mod history;
mod tui;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::config::Config;
use packrat_core::{
    alternates, classify, display_name, episode_file_in, episode_range_file_in, extra_file_in,
    feature_titles, match_episodes, match_episodes_from, match_span, match_span_from,
    movie_extra_file_in, movie_extras, movie_file_in, parse_label, preferred_titles, read_disc,
    read_vts, remux_chain, resolve_movie, search_show, split_title, DiscKind, DiscModel,
    DiscSource, Episode, EpisodeMatch, EpisodeSpan, Movie, MovieQuery, OpticalDrive, Show, Title,
    EXTRA_MIN, MIN_CONTENT,
};

#[derive(Parser)]
#[command(
    name = "packrat",
    version,
    about = "Back up DVDs into a Plex-compatible library",
    after_help = "Run with no subcommand for the interactive guide:\n  packrat\n\nOr ask about a command:\n  packrat <COMMAND> --help"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Inspect a disc and report its title/chapter structure (read-only).
    #[command(
        after_help = "Examples:\n  packrat probe /run/media/$USER/DRAGON_BALL_S1_D1\n  packrat probe /mnt/dvd/VIDEO_TS"
    )]
    Probe {
        /// A mounted disc root, or its VIDEO_TS directory.
        path: PathBuf,
    },
    /// Classify a disc and propose how it should be ripped (read-only).
    #[command(
        after_help = "Examples:\n  packrat plan /run/media/$USER/DRAGON_BALL_S1_D1\n  packrat plan /run/media/$USER/THE_MATRIX_1999"
    )]
    Plan {
        /// A mounted disc root, or its VIDEO_TS directory.
        path: PathBuf,
    },
    /// Remux one title to a lossless MKV (no re-encode).
    #[command(
        after_help = "Examples:\n  packrat rip /run/media/$USER/DRAGON_BALL_S1_D1 --title 11 --out ep1.mkv\n  packrat rip /run/media/$USER/DRAGON_BALL_S1_D1 --title 11 --chapters 1-5 --out ep1.mkv"
    )]
    Rip {
        /// A mounted disc root, or its VIDEO_TS directory.
        path: PathBuf,
        /// Title number as shown by `probe`.
        #[arg(long)]
        title: u16,
        /// Chapter range to rip, e.g. `6-10` (default: the whole title).
        #[arg(long)]
        chapters: Option<String>,
        /// Read VOB data from this raw device (e.g. `/dev/sr0`), using the
        /// positional path for IFO structure.
        #[arg(long)]
        device: Option<PathBuf>,
        /// Output `.mkv` path.
        #[arg(long)]
        out: PathBuf,
    },
    /// Split a disc into outputs: per-episode MKVs for a TV disc, or the main
    /// feature and extras for a movie disc.
    #[command(
        after_help = "Examples:\n  packrat split /run/media/$USER/DRAGON_BALL_S1_D1 --out-dir out --dry-run\n  packrat split /run/media/$USER/DRAGON_BALL_S1_D1 --out-dir out --tv-dir /mnt/dvd/media/tv\n  packrat split /run/media/$USER/DRAGON_BALL_S1_D1 --out-dir out --library /mnt/media\n  packrat split /run/media/$USER/THE_MATRIX_1999 --out-dir out --library /mnt/media --include-extras\n  packrat split /run/media/$USER/DVD_LABEL --device /dev/sr0 --out-dir out --tv-dir /mnt/dvd/media/tv"
    )]
    Split {
        /// A mounted disc root, or its VIDEO_TS directory.
        path: PathBuf,
        /// Output directory (created if missing).
        #[arg(long)]
        out_dir: PathBuf,
        /// Only this title number (default: every content title).
        #[arg(long)]
        title: Option<u16>,
        /// Only this episode of the selected title (1-based).
        #[arg(long)]
        episode: Option<usize>,
        /// Also rip titles detected as redundant alternates (default: skip).
        #[arg(long)]
        all_titles: bool,
        /// Also rip short titles (trailers, featurettes) as extras.
        #[arg(long)]
        include_extras: bool,
        /// Print the plan without remuxing anything.
        #[arg(long)]
        dry_run: bool,
        /// Write Plex-named files under this library root (adds `TV Shows/`
        /// and `Movies/`, and looks the disc up on TVmaze or TMDb). Shorthand
        /// for `--tv-dir <LIBRARY>/TV Shows --movie-dir <LIBRARY>/Movies`.
        #[arg(long)]
        library: Option<PathBuf>,
        /// Directory that holds show folders, such as an existing Plex TV
        /// library. Overrides `--library` and any saved preference.
        #[arg(long)]
        tv_dir: Option<PathBuf>,
        /// Directory that holds movie folders, such as an existing Plex movie
        /// library. Overrides `--library` and any saved preference.
        #[arg(long)]
        movie_dir: Option<PathBuf>,
        /// Read VOB data from this raw device (e.g. `/dev/sr0`), using the
        /// positional path for IFO structure.
        #[arg(long)]
        device: Option<PathBuf>,
        /// Override the show/movie name used for metadata and naming.
        #[arg(long)]
        show: Option<String>,
        /// Override the detected season number.
        #[arg(long)]
        season: Option<u16>,
        /// Number the disc's episodes from this episode number, for a disc
        /// whose place in the season the label got wrong.
        #[arg(long, alias = "start-episode")]
        first_episode: Option<u16>,
        /// Override the movie title used for metadata and naming.
        #[arg(long)]
        movie: Option<String>,
        /// Override the detected release year (movies).
        #[arg(long)]
        year: Option<u16>,
    },
    /// Match the disc against TVmaze (shows) or TMDb (movies) and show the
    /// proposed Plex layout.
    #[command(
        after_help = "Examples:\n  packrat identify /run/media/$USER/DRAGON_BALL_S1_D1 --library /mnt/media\n  packrat identify /run/media/$USER/THE_MATRIX_1999 --movie-dir /mnt/media/Movies"
    )]
    Identify {
        /// A mounted disc root, or its VIDEO_TS directory.
        path: PathBuf,
        /// Library root used when printing proposed paths (adds `TV Shows/`
        /// and `Movies/`).
        #[arg(long)]
        library: Option<PathBuf>,
        /// Directory that holds show folders. Overrides `--library` and any
        /// saved preference.
        #[arg(long)]
        tv_dir: Option<PathBuf>,
        /// Directory that holds movie folders. Overrides `--library` and any
        /// saved preference.
        #[arg(long)]
        movie_dir: Option<PathBuf>,
        /// Number the disc's episodes from this episode number, for a disc
        /// whose place in the season the label got wrong.
        #[arg(long, alias = "start-episode")]
        first_episode: Option<u16>,
    },
    /// List optical drives and any disc in them.
    #[command(after_help = "Examples:\n  packrat drives")]
    Drives,
    /// Check the environment, including CSS decryption support.
    #[command(after_help = "Examples:\n  packrat doctor")]
    Doctor,
    /// Watch for a disc and back it up when one appears.
    #[command(
        after_help = "Examples:\n  packrat watch --library /mnt/media --include-extras\n  packrat watch --once --dry-run --library /mnt/media\n  packrat watch --device /dev/sr1 --library /mnt/media"
    )]
    Watch {
        /// Library root to write into (adds `TV Shows/` and `Movies/`).
        #[arg(long)]
        library: Option<PathBuf>,
        /// Directory that holds show folders. Overrides `--library` and any
        /// saved preference.
        #[arg(long)]
        tv_dir: Option<PathBuf>,
        /// Directory that holds movie folders. Overrides `--library` and any
        /// saved preference.
        #[arg(long)]
        movie_dir: Option<PathBuf>,
        /// Only watch this optical drive, e.g. `/dev/sr1`. Defaults to the
        /// drive last used in the guide, else the first with a disc.
        #[arg(long, alias = "drive")]
        device: Option<PathBuf>,
        /// Poll interval in seconds.
        #[arg(long, default_value_t = 5)]
        interval: u64,
        /// Also rip short titles (trailers, featurettes) as extras.
        #[arg(long)]
        include_extras: bool,
        /// Process a present disc once, then exit.
        #[arg(long)]
        once: bool,
        /// Print the plan without remuxing anything.
        #[arg(long)]
        dry_run: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => tui::run(),
        Some(Command::Probe { path }) => probe(&path),
        Some(Command::Plan { path }) => plan(&path),
        Some(Command::Rip {
            path,
            title,
            chapters,
            device,
            out,
        }) => rip(&path, title, chapters.as_deref(), device.as_deref(), &out),
        Some(Command::Split {
            path,
            out_dir,
            title,
            episode,
            all_titles,
            include_extras,
            dry_run,
            library,
            tv_dir,
            movie_dir,
            device,
            show,
            season,
            first_episode,
            movie,
            year,
        }) => {
            let dest =
                resolve_destinations(library.as_deref(), tv_dir.as_deref(), movie_dir.as_deref());
            split(
                &path,
                &out_dir,
                title,
                episode,
                all_titles,
                include_extras,
                dry_run,
                &dest,
                device.as_deref(),
                show.as_deref(),
                season,
                first_episode,
                movie.as_deref(),
                year,
            )
        }
        Some(Command::Identify {
            path,
            library,
            tv_dir,
            movie_dir,
            first_episode,
        }) => {
            let dest =
                resolve_destinations(library.as_deref(), tv_dir.as_deref(), movie_dir.as_deref());
            identify(&path, &dest, first_episode)
        }
        Some(Command::Drives) => drives(),
        Some(Command::Doctor) => doctor(),
        Some(Command::Watch {
            library,
            tv_dir,
            movie_dir,
            device,
            interval,
            include_extras,
            once,
            dry_run,
        }) => {
            let dest =
                resolve_destinations(library.as_deref(), tv_dir.as_deref(), movie_dir.as_deref());
            watch(
                &dest,
                device.as_deref(),
                interval,
                include_extras,
                once,
                dry_run,
            )
        }
    }
}

/// Where Plex-named files should go, by media type.
struct Destinations {
    tv: Option<PathBuf>,
    movie: Option<PathBuf>,
}

/// Combine explicit flags, the `--library` shorthand and saved preferences.
///
/// Precedence per media type: an explicit `--tv-dir`/`--movie-dir`, then
/// `--library` (`<root>/TV Shows`, `<root>/Movies`), then the saved preference.
fn resolve_destinations(
    library: Option<&Path>,
    tv_dir: Option<&Path>,
    movie_dir: Option<&Path>,
) -> Destinations {
    let config = Config::load();
    let tv = tv_dir
        .map(Path::to_path_buf)
        .or_else(|| library.map(|root| root.join("TV Shows")))
        .or(config.tv_dir);
    let movie = movie_dir
        .map(Path::to_path_buf)
        .or_else(|| library.map(|root| root.join("Movies")))
        .or(config.movie_dir);
    Destinations { tv, movie }
}

fn open(path: &PathBuf) -> Result<(DiscSource, DiscModel)> {
    open_with_device(path, None)
}

/// Open a disc, optionally reading VOB data from a raw device while using the
/// mounted `path` for IFO structure.
fn open_with_device(path: &PathBuf, device: Option<&Path>) -> Result<(DiscSource, DiscModel)> {
    if device.is_some() {
        warn_if_no_dvdcss();
    }
    let source = match device {
        Some(dev) => DiscSource::discover_device(dev, path),
        None => DiscSource::discover(path),
    }
    .with_context(|| format!("opening disc at {}", path.display()))?;
    let disc = read_disc(&source).with_context(|| "reading disc structure")?;
    Ok((source, disc))
}

/// Warn once per process when a raw device is used but libdvdcss is missing.
fn warn_if_no_dvdcss() {
    use std::sync::OnceLock;

    static WARNED: OnceLock<()> = OnceLock::new();
    if WARNED.get().is_some() {
        return;
    }
    if packrat_core::dvdcss::status().is_available() {
        return;
    }
    WARNED.set(()).ok();
    eprintln!(
        "warning: libdvdcss was not found, so CSS-encrypted discs cannot be read \
         from the raw device. Run `packrat doctor` for how to install it."
    );
}

/// Report the environment packrat depends on.
fn doctor() -> Result<()> {
    println!("packrat  : {}", env!("CARGO_PKG_VERSION"));
    println!(
        "platform : {} ({})",
        std::env::consts::OS,
        std::env::consts::ARCH
    );

    match packrat_core::dvdcss::status() {
        packrat_core::dvdcss::DvdcssStatus::Available { path } => {
            println!("css      : available");
            println!("libdvdcss: {}", path.display());
        }
        packrat_core::dvdcss::DvdcssStatus::Unavailable { reason } => {
            println!("css      : unavailable");
            println!("reason   : {reason}");
            println!();
            println!("Rips of unencrypted discs work without libdvdcss. To read");
            println!("CSS-encrypted discs, install it for your platform or set");
            println!("PACKRAT_DVDCSS to its path. See docs/installation.md.");
        }
    }
    Ok(())
}

fn probe(path: &PathBuf) -> Result<()> {
    let (_, disc) = open(path)?;

    println!("Disc      : {}", disc.volume_id);
    if !disc.provider_id.is_empty() {
        println!("Provider  : {}", disc.provider_id);
    }
    println!("Title sets: {}", disc.vts_count);
    println!("Titles    : {}", disc.titles.len());
    println!();
    println!(
        "{:>5}  {:>4}  {:>7}  {:>7}  {:>8}  Duration",
        "Title", "VTS", "VTS_TTN", "Angles", "Chapters"
    );
    for t in &disc.titles {
        println!(
            "{:>5}  {:>4}  {:>7}  {:>7}  {:>8}  {}",
            t.number,
            t.vts,
            t.vts_ttn,
            t.angles,
            t.chapters,
            fmt_duration(t.duration)
        );
    }
    println!();
    println!("Total     : {}", fmt_duration(Some(disc.total_duration())));
    Ok(())
}

fn plan(path: &PathBuf) -> Result<()> {
    let (_, disc) = open(path)?;
    let classification = classify(&disc);

    println!("Disc      : {}", disc.volume_id);
    println!(
        "Looks like: {} ({}% sure)",
        kind_label(classification.kind),
        classification.confidence
    );
    for reason in &classification.reasons {
        println!("            - {reason}");
    }
    println!();

    if classification.kind == DiscKind::Movie {
        return plan_movie(&disc);
    }

    let mut planned = 0usize;
    let preferred = preferred_titles(&disc);
    for title in disc
        .titles
        .iter()
        .filter(|t| is_content(t) && preferred.contains(&t.number))
    {
        let segments = split_title(title);
        if segments.len() >= 2 {
            println!(
                "Title {} ({} chapters, {}) -> {} episode(s):",
                title.number,
                title.chapters,
                fmt_duration(title.duration),
                segments.len()
            );
            for (i, seg) in segments.iter().enumerate() {
                println!(
                    "    E{:<2}  chapters {:>3}-{:<3}  {}",
                    i + 1,
                    seg.start_chapter,
                    seg.end_chapter,
                    fmt_duration(Some(seg.duration))
                );
                planned += 1;
            }
        } else {
            println!(
                "Title {} ({} chapters, {}) -> single file",
                title.number,
                title.chapters,
                fmt_duration(title.duration)
            );
            planned += 1;
        }
    }

    let sets = alternates(&disc);
    if !sets.is_empty() {
        println!();
        println!("Alternates (same episodes, different credits):");
        for set in &sets {
            let dropped: Vec<String> = set.dropped.iter().map(|n| n.to_string()).collect();
            println!(
                "  keeping title {} ({} episodes); skipping {}",
                set.kept,
                set.episodes,
                dropped.join(", ")
            );
        }
    }

    println!();
    println!("Planned outputs: {planned}");
    Ok(())
}

/// The movie half of [`plan`]: the main feature, its extras and, when a TMDb
/// key is configured, the metadata match.
fn plan_movie(disc: &DiscModel) -> Result<()> {
    let label = parse_label(&disc.volume_id);
    let extras = movie_extras(disc);

    let Some(feature) = feature_titles(disc).first().copied() else {
        println!("No feature-length title found.");
        return Ok(());
    };
    println!(
        "Feature: title {} ({} chapters, {})",
        feature.number,
        feature.chapters,
        fmt_duration(feature.duration)
    );
    println!(
        "Extras : {} bonus title(s) (featurettes, trailers, second features)",
        extras.len()
    );
    println!();

    let Some(key) = Config::load().tmdb_key() else {
        println!("(set a TMDb API key in settings to match this movie)");
        println!();
        println!("Planned outputs: 1 feature + {} extra(s)", extras.len());
        return Ok(());
    };

    let query = MovieQuery::new(&label.title, label.year, feature.duration);
    match resolve_movie(&key, &query) {
        Ok(resolution) => {
            if let Some(movie) = resolution.auto_accepted() {
                println!(
                    "Matched: {} ({})  [tmdb {}]",
                    movie.title,
                    movie
                        .year()
                        .map(|y| y.to_string())
                        .unwrap_or_else(|| "?".into()),
                    movie.id
                );
            } else if resolution.candidates.is_empty() {
                println!("No TMDb match for '{}'.", label.title);
            } else {
                println!("No confident TMDb match for '{}'; candidates:", label.title);
                for (score, candidate) in &resolution.candidates {
                    println!(
                        "  {:.0}%  {} ({})",
                        score * 100.0,
                        candidate.title,
                        candidate
                            .year()
                            .map(|y| y.to_string())
                            .unwrap_or_else(|| "?".into())
                    );
                }
            }
        }
        Err(e) => println!("TMDb lookup failed: {e}"),
    }

    println!();
    println!("Planned outputs: 1 feature + {} extra(s)", extras.len());
    Ok(())
}

fn rip(
    path: &PathBuf,
    title_number: u16,
    chapters: Option<&str>,
    device: Option<&Path>,
    out: &Path,
) -> Result<()> {
    let (source, disc) = open_with_device(path, device)?;
    let title = disc
        .titles
        .iter()
        .find(|t| t.number == title_number)
        .with_context(|| format!("disc has no title {title_number}"))?
        .clone();
    let vts =
        read_vts(&source, title.vts).with_context(|| format!("reading title set {}", title.vts))?;

    let (first, last) = match chapters {
        Some(spec) => parse_range(spec).with_context(|| format!("bad --chapters '{spec}'"))?,
        None => (1, title.chapters),
    };

    println!(
        "Remuxing title {} chapters {}-{} ({} chapters total, {}) -> {}",
        title.number,
        first,
        last,
        title.chapters,
        fmt_duration(title.duration),
        out.display()
    );

    let report = remux_chain(&source, &vts, &title, first, last, out)?;

    println!(
        "Wrote {} packets ({} bytes of payload), {} chapters",
        report.packets, report.payload_bytes, report.chapters
    );
    if report.unreadable_sectors > 0 {
        eprintln!(
            "warning: {} sector(s) could not be read or were skipped; the file has gaps there",
            report.unreadable_sectors
        );
    }
    Ok(())
}

/// Parse a `first-last` chapter range.
fn parse_range(spec: &str) -> Result<(u16, u16)> {
    let (a, b) = spec
        .split_once('-')
        .with_context(|| "expected a range like 6-10")?;
    let first: u16 = a.trim().parse().context("bad start chapter")?;
    let last: u16 = b.trim().parse().context("bad end chapter")?;
    Ok((first, last))
}

/// A single planned output: which title, which chapter range, which file.
struct Job {
    title: u16,
    first: u16,
    last: u16,
    path: PathBuf,
}

/// Metadata used to name outputs in a Plex library.
#[derive(Clone)]
pub(crate) struct Naming {
    tv_dir: PathBuf,
    show: String,
    year: Option<u16>,
    episodes: HashMap<u16, Vec<EpisodeMatch>>,
    spans: HashMap<u16, EpisodeSpan>,
}

impl Naming {
    pub(crate) fn path_for(&self, title: u16, episode_index: usize) -> Option<PathBuf> {
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
    pub(crate) fn whole_path(&self, title: u16) -> Option<PathBuf> {
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
    pub(crate) fn extra_path(&self, description: &str) -> PathBuf {
        extra_file_in(&self.tv_dir, &self.show, self.year, description)
    }
}

/// The outcome of a metadata lookup.
pub(crate) struct Resolved {
    pub(crate) show: Option<Show>,
    pub(crate) naming: Option<Naming>,
    pub(crate) warning: Option<String>,
}

/// Metadata used to name movie outputs in a Plex library.
#[derive(Clone)]
pub(crate) struct MovieNaming {
    movies_dir: PathBuf,
    title: String,
    year: Option<u16>,
}

impl MovieNaming {
    pub(crate) fn feature_path(&self) -> PathBuf {
        movie_file_in(&self.movies_dir, &self.title, self.year)
    }

    /// Path for a short title ripped as an extra under `<Title (Year)>/Other/`.
    pub(crate) fn extra_path(&self, description: &str) -> PathBuf {
        movie_extra_file_in(&self.movies_dir, &self.title, self.year, description)
    }

    /// Replace the title and year, e.g. after the user picks a TMDb candidate.
    pub(crate) fn retitle(&mut self, title: String, year: Option<u16>) {
        self.title = title;
        self.year = year;
    }
}

/// The outcome of a movie metadata lookup.
pub(crate) struct ResolvedMovie {
    pub(crate) naming: MovieNaming,
    /// The auto-accepted TMDb match, when there was one.
    pub(crate) movie: Option<Movie>,
    /// Ranked candidates when the match was not confident enough to accept.
    pub(crate) candidates: Vec<(f64, Movie)>,
    pub(crate) warning: Option<String>,
}

/// Resolve TMDb metadata for a disc's main feature.
///
/// A missing key or a low-confidence match is reported through
/// [`ResolvedMovie::warning`] and falls back to the disc label, so ripping
/// never depends on the network. Without a key it makes no request at all.
pub(crate) fn resolve_movie_naming(
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

/// The episode number to start this disc at: the user's correction when one
/// was given, otherwise the number after the highest episode already in the
/// destination season. `None` leaves the provider runtime and disc hint to
/// decide.
fn first_episode_for(
    tv_dir: &Path,
    show: &str,
    year: Option<u16>,
    season: u16,
    manual: Option<u16>,
) -> Option<u16> {
    manual.or_else(|| {
        let season_dir = packrat_core::library::season_dir_in(tv_dir, show, year, season);
        packrat_core::library::highest_episode(&season_dir, season)
            .map(|episode| episode.saturating_add(1))
    })
}

/// Resolve TVmaze metadata for the disc's preferred titles. A miss is reported
/// through [`Resolved::warning`] rather than as an error, so ripping can still
/// proceed with plain file names.
pub(crate) fn resolve_naming(
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
    let hints = disc_episode_hints(&episodes, season, disc, preferred, label.disc);
    let year = show.year();
    let name = show_override
        .map(str::to_string)
        .unwrap_or_else(|| show.name.clone());

    // Number from the user's correction when they made one, else continue
    // after the episodes already in the destination season: the label's disc
    // number cannot tell us that a final disc holds fewer episodes than the
    // earlier ones, but the files already backed up can.
    let first_episode = first_episode_for(tv_dir, &name, year, season, first_episode);

    let mut by_title = HashMap::new();
    let mut spans = HashMap::new();
    // A first episode numbers the disc's preferred titles in order, each
    // continuing where the previous one stopped. Without one the provider
    // runtime and the disc hint choose the window as before.
    let mut next_episode = first_episode;
    for number in preferred {
        let Some(title) = disc.titles.iter().find(|t| t.number == *number) else {
            continue;
        };
        let hint = hints.get(number).copied();
        let segments = split_title(title);
        if segments.len() < 2 {
            // Delivered as one file: see whether it spans several episodes.
            let span = match next_episode {
                Some(first) => {
                    match_span_from(&episodes, season, first, title.duration.unwrap_or_default())
                }
                None => match_span(&episodes, season, title.duration.unwrap_or_default(), hint),
            };
            if let Some(span) = span {
                if next_episode.is_some() {
                    next_episode = span.last.checked_add(1);
                }
                spans.insert(*number, span);
            }
            continue;
        }
        let durations: Vec<Duration> = segments.iter().map(|s| s.duration).collect();
        let matched = match next_episode {
            Some(first) => match_episodes_from(&episodes, season, first, &durations),
            None => match_episodes(&episodes, season, &durations, hint),
        };
        if next_episode.is_some() {
            if let Some(last) = matched.last() {
                next_episode = last.number.checked_add(1);
            }
        }
        by_title.insert(*number, matched);
    }

    Ok(Resolved {
        show: Some(show),
        naming: Some(Naming {
            tv_dir: tv_dir.to_path_buf(),
            show: name,
            year,
            episodes: by_title,
            spans,
        }),
        warning: None,
    })
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

#[allow(clippy::too_many_arguments)]
fn split(
    path: &PathBuf,
    out_dir: &Path,
    only_title: Option<u16>,
    episode: Option<usize>,
    all_titles: bool,
    include_extras: bool,
    dry_run: bool,
    dest: &Destinations,
    device: Option<&Path>,
    show: Option<&str>,
    season: Option<u16>,
    first_episode: Option<u16>,
    movie: Option<&str>,
    year: Option<u16>,
) -> Result<()> {
    let (source, disc) = open_with_device(path, device)?;
    let preferred = preferred_titles(&disc);

    let classification = classify(&disc);
    let is_movie = classification.kind == DiscKind::Movie && only_title.is_none();
    let naming = if is_movie {
        None
    } else {
        match dest.tv.as_deref() {
            Some(tv_dir) => {
                let resolved =
                    resolve_naming(tv_dir, &disc, &preferred, show, season, first_episode)?;
                if let Some(warning) = resolved.warning {
                    eprintln!("warning: {warning}");
                }
                resolved.naming
            }
            None => None,
        }
    };

    let mut jobs: Vec<Job> = Vec::new();

    if is_movie {
        // A feature disc: rip the main feature and name it as a Plex movie.
        let label = parse_label(&disc.volume_id);
        let feature = feature_titles(&disc).first().copied();
        let extras = movie_extras(&disc);
        if !include_extras && !extras.is_empty() {
            eprintln!(
                "note: {} extra title(s) available; pass --include-extras to rip them",
                extras.len()
            );
        }

        // Metadata is only consulted when a movie directory is configured;
        // without one, files are named from the label exactly as before.
        let resolved = match (dest.movie.as_deref(), feature) {
            (Some(dir), Some(feature)) => {
                Some(resolve_movie_naming(dir, &disc, feature, movie, year)?)
            }
            _ => None,
        };
        if let Some(warning) = resolved.as_ref().and_then(|r| r.warning.as_deref()) {
            eprintln!("warning: {warning}");
        }

        if let Some(feature) = feature {
            let path = match &resolved {
                Some(r) => r.naming.feature_path(),
                None => out_dir.join(format!("{}.mkv", label.title)),
            };
            jobs.push(Job {
                title: feature.number,
                first: 1,
                last: feature.chapters,
                path,
            });
        }

        if include_extras {
            for title in &extras {
                let description = format!("Title {:02}", title.number);
                let path = match &resolved {
                    Some(r) => r.naming.extra_path(&description),
                    None => out_dir.join(format!(
                        "{} - {}.mkv",
                        display_name(&label.title, label.year),
                        description
                    )),
                };
                jobs.push(Job {
                    title: title.number,
                    first: 1,
                    last: title.chapters,
                    path,
                });
            }
        }
    } else {
        for title in disc.titles.iter().filter(|t| match only_title {
            // An explicitly named title is always honoured, even a short extra.
            Some(n) => t.number == n,
            None if all_titles => is_content(t),
            None => preferred.contains(&t.number),
        }) {
            let segments = split_title(title);
            if segments.len() >= 2 {
                for (i, segment) in segments.iter().enumerate() {
                    if let Some(want) = episode {
                        if want != i + 1 {
                            continue;
                        }
                    }
                    let path = naming
                        .as_ref()
                        .and_then(|n| n.path_for(title.number, i))
                        .unwrap_or_else(|| {
                            out_dir.join(format!("title{:02}-E{:02}.mkv", title.number, i + 1))
                        });
                    jobs.push(Job {
                        title: title.number,
                        first: segment.start_chapter,
                        last: segment.end_chapter,
                        path,
                    });
                }
            } else if episode.is_none() {
                let path = naming
                    .as_ref()
                    .and_then(|n| n.whole_path(title.number))
                    .unwrap_or_else(|| out_dir.join(format!("title{:02}.mkv", title.number)));
                jobs.push(Job {
                    title: title.number,
                    first: 1,
                    last: title.chapters,
                    path,
                });
            }
        }

        if include_extras {
            for title in disc
                .titles
                .iter()
                .filter(|t| is_extra(t) && only_title.map_or(true, |n| n == t.number))
            {
                let description = format!("Title {:02}", title.number);
                let path = naming
                    .as_ref()
                    .map(|n| n.extra_path(&description))
                    .unwrap_or_else(|| out_dir.join(format!("title{:02}.mkv", title.number)));
                jobs.push(Job {
                    title: title.number,
                    first: 1,
                    last: title.chapters,
                    path,
                });
            }
        }
    }

    if jobs.is_empty() {
        println!("Nothing to do.");
        return Ok(());
    }

    for job in &jobs {
        println!(
            "Title {:>2}  chapters {:>3}-{:<3}  ->  {}",
            job.title,
            job.first,
            job.last,
            job.path.display()
        );
    }

    if dry_run {
        println!("\n{} job(s) (dry run)", jobs.len());
        return Ok(());
    }

    for job in &jobs {
        let title = disc
            .titles
            .iter()
            .find(|t| t.number == job.title)
            .expect("job title exists")
            .clone();
        let vts = read_vts(&source, title.vts)
            .with_context(|| format!("reading title set {}", title.vts))?;
        let report = remux_chain(&source, &vts, &title, job.first, job.last, &job.path)?;
        println!(
            "Wrote {} ({} packets, {} chapters)",
            job.path.display(),
            report.packets,
            report.chapters
        );
        if report.unreadable_sectors > 0 {
            eprintln!(
                "warning: {} could not be read in full; {} sector(s) are missing",
                job.path.display(),
                report.unreadable_sectors
            );
        }
    }

    Ok(())
}

fn identify(path: &PathBuf, dest: &Destinations, first_episode: Option<u16>) -> Result<()> {
    let (_, disc) = open(path)?;
    match classify(&disc).kind {
        DiscKind::Movie => identify_movie(&disc, dest),
        _ => identify_tv(&disc, dest, first_episode),
    }
}

fn identify_tv(disc: &DiscModel, dest: &Destinations, first_episode: Option<u16>) -> Result<()> {
    let tv_dir = dest
        .tv
        .clone()
        .unwrap_or_else(|| PathBuf::from(".").join("TV Shows"));
    let label = parse_label(&disc.volume_id);

    println!("Disc label : {}", disc.volume_id);
    println!(
        "Parsed     : '{}'{}  season {}  disc {}",
        label.title,
        label.year.map(|y| format!(" ({y})")).unwrap_or_default(),
        label
            .season
            .map(|s| s.to_string())
            .unwrap_or_else(|| "?".into()),
        label
            .disc
            .map(|d| d.to_string())
            .unwrap_or_else(|| "?".into()),
    );

    let Some(show) = search_show(&label.title).context("TVmaze search failed")? else {
        println!("No TVmaze match for '{}'.", label.title);
        return Ok(());
    };
    let year = show.year();
    let season = label.season.unwrap_or(1);
    println!(
        "Matched    : {} ({})  [tvmaze {}]",
        show.name,
        year.map(|y| y.to_string()).unwrap_or_else(|| "?".into()),
        show.id
    );

    let episodes = packrat_core::meta::episodes(show.id).context("fetching episodes")?;
    let preferred = preferred_titles(disc);
    let hints = disc_episode_hints(&episodes, season, disc, &preferred, label.disc);

    let mut next_episode = first_episode_for(&tv_dir, &show.name, year, season, first_episode);
    for number in preferred {
        let Some(title) = disc.titles.iter().find(|t| t.number == number) else {
            continue;
        };
        let segments = split_title(title);
        if segments.len() < 2 {
            continue;
        }
        let durations: Vec<Duration> = segments.iter().map(|s| s.duration).collect();
        let matched = match next_episode {
            Some(first) => match_episodes_from(&episodes, season, first, &durations),
            None => match_episodes(&episodes, season, &durations, hints.get(&number).copied()),
        };
        if next_episode.is_some() {
            if let Some(last) = matched.last() {
                next_episode = last.number.checked_add(1);
            }
        }
        println!("\nTitle {} -> {} episode(s):", number, matched.len());
        for m in &matched {
            let out = episode_file_in(
                &tv_dir,
                &show.name,
                year,
                m.season,
                m.number,
                m.title.as_deref(),
            );
            println!(
                "  s{:02}e{:02}  {:<34}  {:>9}  ->  {}",
                m.season,
                m.number,
                m.title.clone().unwrap_or_default(),
                fmt_duration(m.runtime),
                out.display()
            );
        }
    }

    Ok(())
}

/// Preview how a movie disc would be named, matching TMDb when a key is set.
fn identify_movie(disc: &DiscModel, dest: &Destinations) -> Result<()> {
    let label = parse_label(&disc.volume_id);
    let movies_dir = dest
        .movie
        .clone()
        .unwrap_or_else(|| PathBuf::from(".").join("Movies"));

    println!("Disc label : {}", disc.volume_id);
    println!(
        "Parsed     : '{}'{}",
        label.title,
        label.year.map(|y| format!(" ({y})")).unwrap_or_default()
    );
    println!("Looks like : a movie");

    let Some(feature) = feature_titles(disc).first().copied() else {
        println!("No feature-length title found.");
        return Ok(());
    };
    println!(
        "Feature    : title {} ({} chapters, {})",
        feature.number,
        feature.chapters,
        fmt_duration(feature.duration)
    );

    let resolved = resolve_movie_naming(&movies_dir, disc, feature, None, None)?;
    match (&resolved.movie, &resolved.warning) {
        (Some(movie), _) => println!(
            "Matched    : {} ({})  [tmdb {}]",
            movie.title,
            movie
                .year()
                .map(|y| y.to_string())
                .unwrap_or_else(|| "?".into()),
            movie.id
        ),
        (None, Some(warning)) => {
            println!("Metadata   : {warning}");
            for (score, candidate) in &resolved.candidates {
                println!(
                    "  {:.0}%  {} ({})  [tmdb {}]",
                    score * 100.0,
                    candidate.title,
                    candidate
                        .year()
                        .map(|y| y.to_string())
                        .unwrap_or_else(|| "?".into()),
                    candidate.id
                );
            }
        }
        (None, None) => {}
    }

    println!("\nProposed   :");
    println!("  {}", resolved.naming.feature_path().display());
    for title in movie_extras(disc) {
        let description = format!("Title {:02}", title.number);
        println!(
            "  {}   ({})",
            resolved.naming.extra_path(&description).display(),
            fmt_duration(title.duration)
        );
    }

    Ok(())
}

fn is_content(title: &Title) -> bool {
    title.duration.map(|d| d >= MIN_CONTENT).unwrap_or(false)
}

fn drives() -> Result<()> {
    let found = packrat_core::drives::list();
    if found.is_empty() {
        println!("No optical drives detected.");
        return Ok(());
    }
    println!("{:<14} {:>6}  Mount", "Device", "Disc");
    for drive in found {
        println!(
            "{:<14} {:>6}  {}",
            drive.device.display(),
            if drive.has_disc { "yes" } else { "no" },
            drive
                .mount
                .as_ref()
                .map(|m| m.display().to_string())
                .unwrap_or_else(|| "-".into())
        );
    }
    Ok(())
}

/// Pick the drive for `watch` to process: the requested one, else the first
/// with a mounted disc.
fn select_watch_drive<'a>(
    drives: &'a [OpticalDrive],
    requested: Option<&Path>,
) -> Option<&'a OpticalDrive> {
    drives.iter().find(|drive| {
        let matches_requested = match requested {
            Some(requested) => drive.device.as_path() == requested,
            None => true,
        };
        drive.has_disc && drive.mount.is_some() && matches_requested
    })
}

/// Watch optical drives and back up a disc when it appears.
///
/// Reuses the same `split` pipeline, so naming, detection and extras behave
/// identically to a manual run. `device` pins the drive to watch; without it
/// the drive last used in the guide is preferred, falling back to the first
/// one with a disc.
fn watch(
    dest: &Destinations,
    device: Option<&Path>,
    interval: u64,
    include_extras: bool,
    once: bool,
    dry_run: bool,
) -> Result<()> {
    let explicit = device.map(Path::to_path_buf);
    let remembered = Config::load().last_device;

    let mut last_processed: Option<PathBuf> = None;

    loop {
        let drives = packrat_core::drives::list();

        // An explicit drive must exist; a remembered one may have been
        // unplugged, in which case we fall back to any ready drive.
        if let Some(explicit) = &explicit {
            if !drives.iter().any(|drive| &drive.device == explicit) {
                anyhow::bail!("no optical drive at {}", explicit.display());
            }
        }
        let chosen = match &explicit {
            Some(explicit) => select_watch_drive(&drives, Some(explicit)),
            None => select_watch_drive(&drives, remembered.as_deref())
                .or_else(|| select_watch_drive(&drives, None)),
        };

        match chosen {
            Some(drive) => {
                let mount = drive.mount.clone().expect("checked above");
                if last_processed.as_ref() != Some(&mount) {
                    println!(
                        "Disc detected in {} mounted at {}",
                        drive.device.display(),
                        mount.display()
                    );
                    let out_dir = dest
                        .tv
                        .as_deref()
                        .or(dest.movie.as_deref())
                        .unwrap_or_else(|| Path::new("."));
                    split(
                        &mount,
                        out_dir,
                        None,
                        None,
                        false,
                        include_extras,
                        dry_run,
                        dest,
                        Some(&drive.device),
                        None,
                        None,
                        None,
                        None,
                        None,
                    )?;
                    last_processed = Some(mount);
                } else if once {
                    println!("Disc already processed.");
                    return Ok(());
                }
            }
            None if once => {
                match &explicit {
                    Some(device) => println!("No disc in {}.", device.display()),
                    None => println!("No disc present."),
                }
                return Ok(());
            }
            None => {}
        }

        if once {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(interval.max(1)));
    }
}

fn is_extra(title: &Title) -> bool {
    title
        .duration
        .map(|d| d >= EXTRA_MIN && d < MIN_CONTENT)
        .unwrap_or(false)
}

fn kind_label(kind: DiscKind) -> &'static str {
    match kind {
        DiscKind::Movie => "a movie",
        DiscKind::TvSeries => "a TV series",
        DiscKind::Unknown => "something I cannot identify",
    }
}

fn fmt_duration(d: Option<Duration>) -> String {
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
                let span = match_span(
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
        let season = packrat_core::library::season_dir_in(&root, "Dragon Ball", Some(1986), 1);
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

    fn drive(device: &str, has_disc: bool, mount: Option<&str>) -> OpticalDrive {
        OpticalDrive {
            device: PathBuf::from(device),
            mount: mount.map(PathBuf::from),
            has_disc,
        }
    }

    #[test]
    fn watch_drive_selection_respects_the_requested_device() {
        let drives = vec![
            drive("/dev/sr0", true, Some("/mnt/a")),
            drive("/dev/sr1", true, Some("/mnt/b")),
        ];
        let chosen = select_watch_drive(&drives, Some(Path::new("/dev/sr1"))).expect("selected");
        assert_eq!(chosen.device, PathBuf::from("/dev/sr1"));
    }

    #[test]
    fn watch_drive_selection_defaults_to_the_first_ready_disc() {
        let drives = vec![
            drive("/dev/sr0", false, None),
            drive("/dev/sr1", true, Some("/mnt/b")),
        ];
        let chosen = select_watch_drive(&drives, None).expect("selected");
        assert_eq!(chosen.device, PathBuf::from("/dev/sr1"));
    }

    #[test]
    fn watch_drive_selection_waits_when_the_requested_drive_has_no_disc() {
        let drives = vec![
            drive("/dev/sr0", true, Some("/mnt/a")),
            drive("/dev/sr1", false, None),
        ];
        assert!(select_watch_drive(&drives, Some(Path::new("/dev/sr1"))).is_none());
    }
}

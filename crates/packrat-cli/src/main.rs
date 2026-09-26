//! `packrat` command-line entry point.

mod config;
mod tui;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::config::Config;
use packrat_core::{
    alternates, classify, episode_file_in, episode_range_file_in, extra_file_in, match_episodes,
    match_span, movie_file_in, parse_label, preferred_titles, read_disc, read_vts, remux_chain,
    search_show, split_title, DiscKind, DiscModel, DiscSource, EpisodeMatch, EpisodeSpan, Show,
    Title, EXTRA_MIN, MIN_CONTENT,
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
    #[command(after_help = "Examples:\n  packrat plan /run/media/$USER/DRAGON_BALL_S1_D1")]
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
    /// Split a TV disc's "Play All" titles into per-episode MKVs.
    #[command(
        after_help = "Examples:\n  packrat split /run/media/$USER/DRAGON_BALL_S1_D1 --out-dir out --dry-run\n  packrat split /run/media/$USER/DRAGON_BALL_S1_D1 --out-dir out --tv-dir /mnt/dvd/media/tv\n  packrat split /run/media/$USER/DRAGON_BALL_S1_D1 --out-dir out --library /mnt/media\n  packrat split /run/media/$USER/DVD_LABEL --device /dev/sr0 --out-dir out --tv-dir /mnt/dvd/media/tv"
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
        /// and `Movies/`, and looks the disc up on TVmaze). Shorthand for
        /// `--tv-dir <LIBRARY>/TV Shows --movie-dir <LIBRARY>/Movies`.
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
    },
    /// Match the disc against TVmaze and show the proposed Plex layout.
    #[command(
        after_help = "Examples:\n  packrat identify /run/media/$USER/DRAGON_BALL_S1_D1 --library /mnt/media"
    )]
    Identify {
        /// A mounted disc root, or its VIDEO_TS directory.
        path: PathBuf,
        /// Library root used when printing proposed paths (adds `TV Shows/`).
        #[arg(long)]
        library: Option<PathBuf>,
        /// Directory that holds show folders. Overrides `--library` and any
        /// saved preference.
        #[arg(long)]
        tv_dir: Option<PathBuf>,
    },
    /// List optical drives and any disc in them.
    #[command(after_help = "Examples:\n  packrat drives")]
    Drives,
    /// Watch for a disc and back it up when one appears.
    #[command(
        after_help = "Examples:\n  packrat watch --library /mnt/media --include-extras\n  packrat watch --once --dry-run --library /mnt/media"
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
            )
        }
        Some(Command::Identify {
            path,
            library,
            tv_dir,
        }) => {
            let dest = resolve_destinations(library.as_deref(), tv_dir.as_deref(), None);
            identify(&path, &dest)
        }
        Some(Command::Drives) => drives(),
        Some(Command::Watch {
            library,
            tv_dir,
            movie_dir,
            interval,
            include_extras,
            once,
            dry_run,
        }) => {
            let dest =
                resolve_destinations(library.as_deref(), tv_dir.as_deref(), movie_dir.as_deref());
            watch(&dest, interval, include_extras, once, dry_run)
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
    let source = match device {
        Some(dev) => DiscSource::discover_device(dev, path),
        None => DiscSource::discover(path),
    }
    .with_context(|| format!("opening disc at {}", path.display()))?;
    let disc = read_disc(&source).with_context(|| "reading disc structure")?;
    Ok((source, disc))
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

/// Resolve TVmaze metadata for the disc's preferred titles. A miss is reported
/// through [`Resolved::warning`] rather than as an error, so ripping can still
/// proceed with plain file names.
pub(crate) fn resolve_naming(
    tv_dir: &Path,
    disc: &DiscModel,
    preferred: &[u16],
    show_override: Option<&str>,
    season_override: Option<u16>,
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

    let mut by_title = HashMap::new();
    let mut spans = HashMap::new();
    for number in preferred {
        let Some(title) = disc.titles.iter().find(|t| t.number == *number) else {
            continue;
        };
        let segments = split_title(title);
        if segments.len() < 2 {
            // Delivered as one file: see whether it spans several episodes.
            if let Some(span) = match_span(&episodes, season, title.duration.unwrap_or_default()) {
                spans.insert(*number, span);
            }
            continue;
        }
        let durations: Vec<Duration> = segments.iter().map(|s| s.duration).collect();
        by_title.insert(*number, match_episodes(&episodes, season, &durations));
    }

    let year = show.year();
    let name = show_override
        .map(str::to_string)
        .unwrap_or_else(|| show.name.clone());
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
                let resolved = resolve_naming(tv_dir, &disc, &preferred, show, season)?;
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
        if let Some(film) = disc
            .content_titles(MIN_CONTENT)
            .into_iter()
            .max_by_key(|t| t.duration)
        {
            let label = parse_label(&disc.volume_id);
            let path = match dest.movie.as_deref() {
                Some(root) => movie_file_in(root, &label.title, label.year),
                None => out_dir.join(format!("{}.mkv", label.title)),
            };
            jobs.push(Job {
                title: film.number,
                first: 1,
                last: film.chapters,
                path,
            });
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
    }

    Ok(())
}

fn identify(path: &PathBuf, dest: &Destinations) -> Result<()> {
    let tv_dir = dest
        .tv
        .clone()
        .unwrap_or_else(|| PathBuf::from(".").join("TV Shows"));
    let (_, disc) = open(path)?;
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
    let preferred = preferred_titles(&disc);

    for number in preferred {
        let Some(title) = disc.titles.iter().find(|t| t.number == number) else {
            continue;
        };
        let segments = split_title(title);
        if segments.len() < 2 {
            continue;
        }
        let durations: Vec<Duration> = segments.iter().map(|s| s.duration).collect();
        let matched = match_episodes(&episodes, season, &durations);
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

/// Watch optical drives and back up a disc when it appears.
///
/// Reuses the same `split` pipeline, so naming, detection and extras behave
/// identically to a manual run.
fn watch(
    dest: &Destinations,
    interval: u64,
    include_extras: bool,
    once: bool,
    dry_run: bool,
) -> Result<()> {
    let mut last_processed: Option<PathBuf> = None;

    loop {
        let drives = packrat_core::drives::list();
        let ready = drives.iter().find(|d| d.has_disc && d.mount.is_some());

        match ready {
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
                    )?;
                    last_processed = Some(mount);
                } else if once {
                    println!("Disc already processed.");
                    return Ok(());
                }
            }
            None if once => {
                println!("No disc present.");
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

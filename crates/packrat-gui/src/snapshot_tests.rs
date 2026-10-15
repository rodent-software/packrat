//! Headless renders of each screen.
//!
//! These exist so a layout can be looked at as an image without opening the
//! window and driving it. Each test writes a PNG under `snapshots/`, which is
//! gitignored: the images are a review aid, not a committed baseline.
//!
//! Render them with:
//!
//! ```sh
//! cargo test -p packrat snapshot
//! ```
//!
//! They force the software renderer, so the result does not depend on a GPU
//! (CI runners have none) and is identical from run to run. To turn these into
//! a regression gate, stop regenerating — keep each file and compare it with
//! `matches_image` — and commit the images.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::time::Duration;
use std::time::Instant;

use iced::{Settings, Size};

use packrat_core::{Classification, DiscKind, DiscModel, LabelInfo, LibraryStats, Title};
use packrat_engine::history::History;
use packrat_engine::TitleMatch;

use crate::jobs::JobStatus;
use crate::theme;
use crate::{App, Message, MetaState, ProgressSnapshot, Stage};
use packrat_engine::{DriveState, WorkerEvent};

/// The size the window opens at.
const WINDOW: (f32, f32) = (1040.0, 780.0);

/// An app with no disc, no drives and no history, for a test to fill in.
fn blank(stage: Stage) -> App {
    let (tx, rx) = mpsc::channel();

    App {
        stage,
        tv_input: String::new(),
        movie_input: String::new(),
        tmdb_input: String::new(),
        auto_eject: false,
        config_note: None,
        css_note: None,
        config_return: Stage::Detect,
        drives: Vec::new(),
        selected_drive: None,
        manual_path: String::new(),
        detect_loading: false,
        detect_note: None,
        last_device: None,
        watched_device: None,
        drive_watch_countdown: 0,
        drive_watch_in_flight: false,
        drive_watch_failures: 0,
        loaded_media: None,
        watch_note: None,
        source: None,
        disc_paths: Vec::new(),
        disc: None,
        classification: None,
        kind_override: None,
        plan_kind: None,
        label: None,
        device: None,
        fingerprint: None,
        naming: None,
        movie_naming: None,
        meta_show: None,
        meta_movie: None,
        meta_state: MetaState::Idle,
        title_matches: Vec::new(),
        search_state: MetaState::Idle,
        meta_note: None,
        show_input: String::new(),
        season_input: String::new(),
        first_episode_input: String::new(),
        jobs: Vec::new(),
        include_extras: false,
        tx,
        rx,
        cancel: Arc::new(AtomicBool::new(false)),
        abort: Arc::new(AtomicBool::new(false)),
        rip_handle: None,
        progress: None,
        current: None,
        total: 0,
        done: 0,
        failed: 0,
        cancelled: false,
        rip_started: None,
        rip_elapsed: None,
        session_files: 0,
        session_bytes: 0,
        library: LibraryStats::default(),
        free_bytes: None,
        history: History::default(),
        ejected: false,
        eject_note: None,
        signal_at: Instant::now(),
    }
}

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

/// A TV disc that looks like a real one: a Play All title plus episodes.
fn tv_disc() -> DiscModel {
    DiscModel {
        volume_id: "DRAGON_BALL_S1_D1".into(),
        provider_id: "ARCCOS".into(),
        vts_count: 4,
        titles: vec![
            title(1, 4, 1_380),
            title(2, 1, 1_380),
            title(3, 1, 1_380),
            title(4, 1, 1_380),
            title(5, 1, 1_380),
            title(6, 2, 240),
        ],
    }
}

fn movie_disc() -> DiscModel {
    DiscModel {
        volume_id: "THE_MATRIX_1999".into(),
        provider_id: "WARNER".into(),
        vts_count: 3,
        titles: vec![title(1, 28, 293), title(2, 3, 90), title(3, 1, 145)],
    }
}

/// Point the app at the drive a fixture disc came from, so the source line and
/// the shortcuts render as they would in use.
fn with_drive(app: &mut App) {
    app.drives = vec![packrat_core::OpticalDrive {
        device: "/dev/sr0".into(),
        mount: None,
        has_disc: true,
    }];
    app.selected_drive = Some(0);
    app.device = Some("/dev/sr0".into());
    app.manual_path = "/dev/sr0".into();
}

fn tv_classification() -> Classification {
    Classification {
        kind: DiscKind::TvSeries,
        confidence: 92,
        reasons: vec![
            "several similar-length titles".into(),
            "disc number in the label".into(),
        ],
    }
}

fn movie_classification() -> Classification {
    Classification {
        kind: DiscKind::Movie,
        confidence: 88,
        reasons: vec!["one feature-length title".into()],
    }
}

fn label(title: &str, year: Option<u16>, season: Option<u16>, disc: Option<u16>) -> LabelInfo {
    LabelInfo {
        title: title.to_string(),
        year,
        season,
        disc,
        looks_like_tv: season.is_some(),
    }
}

/// The window-sized render of `app`, written to `snapshots/<name>`.
///
/// A `name` must not be a prefix of another render's name followed by `-`: the
/// images are written as `<name>-<renderer>.png` and the previous one is dropped
/// by matching that prefix, so `done` would delete `done-ejected`.
fn render(name: &str, app: &App) {
    render_at(name, app, WINDOW.0, WINDOW.1);
}

/// Render a page at full height.
///
/// The backup screen is taller than the window it opens in, so a review render
/// at the window size only ever shows its top. This one shows the whole page, so
/// what is below the fold can be looked at too.
fn render_whole(name: &str, app: &App) {
    render_at(name, app, WINDOW.0, 1500.0);
}

fn render_at(name: &str, app: &App, width: f32, height: f32) {
    // Software rendering: no GPU is needed, so this works on a CI runner, and
    // the output does not vary with the driver.
    std::env::set_var("ICED_TEST_BACKEND", "tiny-skia");

    let mut ui =
        iced_test::Simulator::with_size(Settings::default(), Size::new(width, height), app.view());

    let snapshot = ui.snapshot(&theme::theme()).expect("render the screen");
    let path = PathBuf::from("snapshots").join(name);

    // A render aid, not a gate: drop any previous image so the snapshot is
    // always taken fresh. Matched on the `name-` prefix rather than the bare
    // name, which would make `done` delete `done-ejected`.
    let written_prefix = format!("{name}-");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
        if let Ok(entries) = std::fs::read_dir(parent) {
            for entry in entries.flatten() {
                if entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&written_prefix)
                {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }

    let written = snapshot.matches_image(&path).expect("write the snapshot");
    assert!(written, "snapshot {name} did not match");

    for file in written_files(&path) {
        println!("rendered {}", file.display());
    }
}

/// The files `matches_image` writes, whatever renderer suffix it used.
fn written_files(path: &Path) -> Vec<PathBuf> {
    let Some(parent) = path.parent() else {
        return Vec::new();
    };
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let prefix = format!("{stem}-");

    std::fs::read_dir(parent)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|file| {
            file.file_name()
                .map(|n| n.to_string_lossy().starts_with(&prefix))
                .unwrap_or(false)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Screens
// ---------------------------------------------------------------------------

#[test]
fn snapshot_setup() {
    let mut app = blank(Stage::Setup);
    app.tv_input = "/mnt/media/tv".into();
    app.movie_input = "/mnt/media/movies".into();
    app.tmdb_input = "a1b2c3d4e5f6".into();
    app.library = LibraryStats {
        shows: 14,
        seasons: 38,
        episodes: 612,
        movies: 87,
        tv_bytes: 1_940_000_000_000,
        movie_bytes: 720_000_000_000,
        newest: None,
    };
    app.free_bytes = Some(7_400_000_000_000);
    app.history = History {
        discs: 143,
        files: 1_208,
        bytes: 2_600_000_000_000,
        last_backup: None,
    };
    render("setup", &app);
}

/// A drive as the scan would report it.
fn drive(device: &str, has_disc: bool) -> packrat_core::OpticalDrive {
    packrat_core::OpticalDrive {
        device: device.into(),
        mount: None,
        has_disc,
    }
}

/// The disc list holds the drives and the folders opened by hand, in that
/// order, and reports the right one as current. A closed dropdown renders only
/// its selection, so this is the only place the contents can be checked.
#[test]
fn the_disc_list_holds_drives_then_opened_folders() {
    use crate::view::disc_choice_labels;

    let mut app = blank(Stage::Detect);
    app.drives = vec![drive("/dev/sr0", true), drive("/dev/sr1", false)];
    app.disc_paths = vec!["/run/media/you/ONE".into(), "/run/media/you/TWO".into()];
    app.selected_drive = Some(0);

    let (labels, selected) = disc_choice_labels(&app);
    assert_eq!(
        labels,
        vec![
            "/dev/sr0 (disc)",
            "/dev/sr1 (empty)",
            "/run/media/you/ONE · folder",
            "/run/media/you/TWO · folder",
        ]
    );
    assert_eq!(selected.as_deref(), Some("/dev/sr0 (disc)"));

    // A folder is current only while no drive is.
    app.selected_drive = None;
    app.disc = Some(tv_disc());
    app.manual_path = "/run/media/you/TWO".into();

    let (_, selected) = disc_choice_labels(&app);
    assert_eq!(selected.as_deref(), Some("/run/media/you/TWO · folder"));
}

/// A folder opened by hand goes to the top without repeating, and the list
/// stops growing rather than collecting every path ever opened.
#[test]
fn opened_folders_are_remembered_newest_first() {
    let mut app = blank(Stage::Detect);

    for path in ["/a", "/b", "/a", "/c", "/d", "/e", "/f", "/g"] {
        app.remember_disc_path(std::path::Path::new(path));
    }

    let names: Vec<String> = app
        .disc_paths
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    assert_eq!(names, vec!["/g", "/f", "/e", "/d", "/c", "/a"]);
}

/// A disc taken out of the drive takes the plan with it: nothing read off it is
/// still true, and leaving it up would offer to back up a disc that is no
/// longer there.
#[test]
fn a_removed_disc_returns_the_screen_to_the_picker() {
    let mut app = blank(Stage::Plan);
    app.disc = Some(tv_disc());
    with_drive(&mut app);
    app.classification = Some(tv_classification());
    app.label = Some(label("Dragon Ball", Some(1986), Some(1), Some(1)));
    app.include_extras = true;
    app.rebuild_jobs();
    assert!(!app.jobs.is_empty());

    app.disc_removed();

    assert!(app.disc.is_none());
    assert!(app.source.is_none());
    assert!(app.classification.is_none());
    assert!(app.naming.is_none());
    assert!(app.label.is_none());
    assert!(app.jobs.is_empty());
    assert!(!app.include_extras);
    assert!(app.device.is_none());
    assert_eq!(app.stage, Stage::Detect);
    // The drive stays selected so the list can say which one is now empty.
    assert_eq!(app.selected_drive, Some(0));
    assert_eq!(app.manual_path, "/dev/sr0");
}

/// The watcher looks while the screen is about the disc, one look at a time.
///
/// It must not read a disc a run is using, because a run owns its disc. A
/// finished run is looked at like any other screen: without that, a disc
/// ejected at the end of one is never noticed, and the window goes on
/// describing a disc that is no longer in the drive.
#[test]
fn the_drive_watcher_only_looks_where_it_can_help() {
    let mut app = blank(Stage::Plan);
    assert!(app.should_poll_drive(), "an idle screen");

    for stage in [Stage::Detect, Stage::Done] {
        app.stage = stage;
        assert!(app.should_poll_drive(), "{stage:?}");
    }

    for stage in [Stage::Ripping, Stage::Setup] {
        app.stage = stage;
        assert!(!app.should_poll_drive(), "{stage:?}");
    }
    app.stage = Stage::Plan;

    // One question at a time: a look already out is not joined by another.
    app.drive_watch_in_flight = true;
    assert!(!app.should_poll_drive());
    app.drive_watch_in_flight = false;
    assert!(app.should_poll_drive());
}

/// Picking a drive points the window at it and reads straight away, rather than
/// waiting out a poll interval.
#[test]
fn picking_a_drive_points_the_watcher_at_it() {
    let mut app = blank(Stage::Plan);
    with_drive(&mut app);
    app.disc = None;

    app.select_drive(0);

    assert_eq!(app.watched_device.as_deref(), Some(Path::new("/dev/sr0")));
    assert_eq!(app.drive_watch_countdown, 0, "looks on the next tick");
    assert_eq!(app.drive_watch_failures, 0);
}

/// The retry after a drive will not read a disc starts short and backs off, so
/// a disc that needs a moment is picked up quickly and one that is not coming
/// back is not hammered.
#[test]
fn a_failed_read_backs_off() {
    let mut app = blank(Stage::Plan);

    app.drive_watch_failures = 1;
    assert_eq!(app.retry_ticks(), 5, "half a second for the first retry");
    app.drive_watch_failures = 2;
    assert_eq!(app.retry_ticks(), 10);
    app.drive_watch_failures = 3;
    assert_eq!(app.retry_ticks(), 20);

    // It stops growing, so a disc that never reads is retried, not hammered.
    app.drive_watch_failures = 5;
    assert_eq!(app.retry_ticks(), 50);
    app.drive_watch_failures = 1000;
    assert_eq!(app.retry_ticks(), 50);
}

/// The one table, whole: a row per title, its selection, and what will be made
/// of it — including a title nothing will be made from.
#[test]
fn snapshot_titles_table() {
    let mut app = blank(Stage::Plan);
    app.tv_input = "/mnt/media/tv".into();
    // A twenty-second title: too short to be content and below even the extras
    // floor, so the table has an unplanned row to show as well.
    let mut disc = tv_disc();
    disc.titles.push(title(7, 1, 20));
    app.disc = Some(disc);
    with_drive(&mut app);
    app.classification = Some(tv_classification());
    app.label = Some(label("Dragon Ball", Some(1986), Some(1), Some(1)));
    app.include_extras = true;
    app.rebuild_jobs();
    render_whole("titles-table", &app);
}

/// The watcher's note is its own. A look every second that cleared the disc's
/// notes would wipe any message from anywhere else within a second of it
/// appearing.
#[test]
fn the_drive_watch_keeps_its_note_to_itself() {
    let mut app = blank(Stage::Plan);
    with_drive(&mut app);
    app.watched_device = Some("/dev/sr0".into());
    app.detect_note = Some("Something worth reading.".into());

    app.on_worker(WorkerEvent::DriveWatched {
        device: "/dev/sr0".into(),
        drives: app.drives.clone(),
        state: DriveState::Unreadable {
            bytes: 1,
            message: "not ready".into(),
        },
    });

    assert!(app.watch_note.is_some(), "the wait is reported");
    assert_eq!(
        app.detect_note.as_deref(),
        Some("Something worth reading."),
        "and nothing else is disturbed"
    );

    app.on_worker(WorkerEvent::DriveWatched {
        device: "/dev/sr0".into(),
        drives: app.drives.clone(),
        state: DriveState::Present { bytes: 0 },
    });

    assert!(app.watch_note.is_none(), "the wait is over");
    assert_eq!(
        app.detect_note.as_deref(),
        Some("Something worth reading."),
        "still nothing else is disturbed"
    );
}

/// A disc swapped for another with no moment of emptiness in between is still a
/// different disc, and the screen would otherwise go on describing the one that
/// left.
#[test]
fn a_swapped_disc_is_noticed_by_its_size() {
    let mut app = blank(Stage::Plan);
    app.disc = Some(tv_disc());
    with_drive(&mut app);
    app.watched_device = Some("/dev/sr0".into());

    // First sight of it records the size.
    app.on_worker(WorkerEvent::DriveWatched {
        device: "/dev/sr0".into(),
        drives: app.drives.clone(),
        state: DriveState::Present {
            bytes: 4_700_000_000,
        },
    });
    assert_eq!(app.loaded_media, Some(4_700_000_000));
    assert!(app.disc.is_some(), "the same disc is left alone");

    // A disc of another size in the same drive.
    app.on_worker(WorkerEvent::DriveWatched {
        device: "/dev/sr0".into(),
        drives: app.drives.clone(),
        state: DriveState::Present {
            bytes: 3_100_000_000,
        },
    });

    assert!(app.disc.is_none(), "the plan for the old disc goes with it");
}

/// A window pinned to one drive still has to know about the others: the list
/// it offers is otherwise frozen at whatever it was when it pinned one, and a
/// second drive with a disc in it never shows up.
#[test]
fn a_pinned_window_still_learns_about_the_other_drives() {
    let mut app = blank(Stage::Plan);
    with_drive(&mut app);
    app.watched_device = Some("/dev/sr0".into());
    assert_eq!(app.drives.len(), 1, "the fixture knows of one drive");

    app.on_worker(WorkerEvent::DriveWatched {
        device: "/dev/sr0".into(),
        state: DriveState::Present { bytes: 0 },
        drives: vec![drive("/dev/sr0", true), drive("/dev/sr1", true)],
    });

    assert_eq!(
        app.drives.len(),
        2,
        "the second drive arrived with the answer"
    );
    assert!(app.drives[1].has_disc);
}

#[test]
fn snapshot_no_disc() {
    let mut app = blank(Stage::Detect);
    app.drives = vec![
        packrat_core::OpticalDrive {
            device: "/dev/sr0".into(),
            mount: None,
            has_disc: true,
        },
        packrat_core::OpticalDrive {
            device: "/dev/sr1".into(),
            mount: None,
            has_disc: false,
        },
    ];
    // And a folder opened by hand, so the render shows both kinds of entry.
    app.disc_paths = vec!["/run/media/you/DISC_LABEL".into()];
    render("no-disc", &app);
}

#[test]
fn snapshot_movie_disc() {
    let mut app = blank(Stage::Plan);
    app.movie_input = "/mnt/media/movies".into();
    app.show_input = "The Matrix".into();
    app.season_input = "1999".into();
    app.title_matches = vec![
        TitleMatch::from_movie(0.94, &movie("The Matrix", 1999, 1)),
        TitleMatch::from_movie(0.71, &movie("The Matrix Reloaded", 2003, 2)),
    ];
    app.meta_movie = Some(movie("The Matrix", 1999, 1));
    app.disc = Some(movie_disc());
    with_drive(&mut app);
    app.classification = Some(movie_classification());
    app.label = Some(label("The Matrix", Some(1999), None, None));
    app.include_extras = true;
    app.rebuild_jobs();

    // Pretend one file already failed, so the status column shows every state.
    if let Some(job) = app.jobs.get_mut(1) {
        job.status = JobStatus::Failed("input/output error".into());
    }
    render("movie-disc", &app);
}

#[test]
fn snapshot_tv_disc() {
    let mut app = blank(Stage::Plan);
    app.tv_input = "/mnt/media/tv".into();
    app.show_input = "Dragon Ball".into();
    app.season_input = "1".into();
    app.disc = Some(tv_disc());
    with_drive(&mut app);
    app.classification = Some(tv_classification());
    app.label = Some(label("Dragon Ball", Some(1986), Some(1), Some(1)));
    app.rebuild_jobs();
    render("tv-disc", &app);
}

/// The verdict is a guess off the durations, the chapters and the label, so it
/// has to be overridable: the override moves the disc to the other planner and
/// its output elsewhere, and choosing what was detected puts it back.
#[test]
fn the_kind_override_replans_the_disc() {
    let mut app = blank(Stage::Plan);
    app.tv_input = "/mnt/media/tv".into();
    app.movie_input = "/mnt/media/movies".into();
    app.disc = Some(tv_disc());
    with_drive(&mut app);
    app.classification = Some(tv_classification());
    app.label = Some(label("Dragon Ball", Some(1986), Some(1), Some(1)));
    app.rebuild_jobs();

    // Read as television: every episode is selected, and written under the TV
    // directory.
    assert!(!app.is_movie());
    assert!(app.jobs.iter().all(|job| job.enabled));
    assert!(
        app.jobs
            .iter()
            .all(|job| job.path.starts_with("/mnt/media/tv")),
        "TV output goes to the TV directory"
    );

    // Overruling the guess sends the disc to the movie planner. One title
    // becomes the feature and the rest become unselected extras.
    app.choose_kind(DiscKind::Movie);
    app.rebuild_jobs();

    assert!(app.is_movie());
    assert_eq!(app.kind_override, Some(DiscKind::Movie));
    assert_eq!(
        app.jobs.iter().filter(|job| job.enabled).count(),
        1,
        "only the feature is selected"
    );
    assert!(
        app.jobs
            .iter()
            .all(|job| job.path.starts_with("/mnt/media/movies")),
        "movie output goes to the movie directory"
    );

    // Choosing the kind that was detected drops the override rather than
    // pinning the disc to it.
    app.choose_kind(DiscKind::TvSeries);
    app.rebuild_jobs();

    assert_eq!(app.kind_override, None);
    assert!(!app.is_movie());
    assert!(app
        .jobs
        .iter()
        .all(|job| job.path.starts_with("/mnt/media/tv")));
}

#[test]
fn snapshot_ripping() {
    let mut app = blank(Stage::Ripping);
    app.tv_input = "/mnt/media/tv".into();
    app.disc = Some(tv_disc());
    with_drive(&mut app);
    app.classification = Some(tv_classification());
    app.label = Some(label("Dragon Ball", Some(1986), Some(1), Some(1)));
    app.rebuild_jobs();

    app.total = app.jobs.len();
    app.done = 2;
    app.session_bytes = 4_100_000_000;
    if let Some(job) = app.jobs.get_mut(0) {
        job.status = JobStatus::Done(packrat_core::RemuxReport {
            packets: 120_000,
            payload_bytes: 2_100_000_000,
            chapters: 4,
            unreadable_sectors: 0,
        });
    }
    if let Some(job) = app.jobs.get_mut(1) {
        job.status = JobStatus::Done(packrat_core::RemuxReport {
            packets: 118_000,
            payload_bytes: 2_000_000_000,
            chapters: 1,
            unreadable_sectors: 12,
        });
    }
    if let Some(job) = app.jobs.get_mut(2) {
        job.status = JobStatus::Running;
    }
    app.current = Some(2);
    app.progress = Some(ProgressSnapshot {
        phase: packrat_core::RemuxPhase::Muxing,
        bytes_done: 980_000_000,
        bytes_total: 2_400_000_000,
        elapsed: Duration::from_secs(96),
    });
    render("ripping", &app);
}

#[test]
fn snapshot_done() {
    let mut app = blank(Stage::Done);
    app.tv_input = "/mnt/media/tv".into();
    app.disc = Some(tv_disc());
    with_drive(&mut app);
    app.classification = Some(tv_classification());
    app.label = Some(label("Dragon Ball", Some(1986), Some(1), Some(1)));
    app.rebuild_jobs();

    app.total = app.jobs.len();
    app.done = app.total;
    app.failed = 1;
    app.session_bytes = 8_400_000_000;
    app.rip_elapsed = Some(Duration::from_secs(3_612));
    for (i, job) in app.jobs.iter_mut().enumerate() {
        job.status = if i == 2 {
            JobStatus::Failed("the drive reported a read error".into())
        } else {
            JobStatus::Done(packrat_core::RemuxReport {
                packets: 120_000,
                payload_bytes: 2_100_000_000,
                chapters: 2,
                unreadable_sectors: 0,
            })
        };
    }
    render("done", &app);
}

/// The same finished run once its disc has left the drive: the result stays on
/// the bar, and everything read off the disc is gone.
#[test]
fn snapshot_ejected() {
    let mut app = blank(Stage::Done);
    app.tv_input = "/mnt/media/tv".into();
    app.disc = Some(tv_disc());
    with_drive(&mut app);
    app.classification = Some(tv_classification());
    app.label = Some(label("Dragon Ball", Some(1986), Some(1), Some(1)));
    app.rebuild_jobs();

    app.total = app.jobs.len();
    app.done = app.total;
    app.session_bytes = 8_400_000_000;
    app.rip_elapsed = Some(Duration::from_secs(3_612));
    for job in &mut app.jobs {
        job.status = JobStatus::Done(packrat_core::RemuxReport {
            packets: 120_000,
            payload_bytes: 2_100_000_000,
            chapters: 2,
            unreadable_sectors: 0,
        });
    }
    app.ejected = true;
    app.disc_removed();
    // What the scan says once it has answered the refresh: the disc is out and
    // the drive is waiting for the next one.
    if let Some(drive) = app.drives.first_mut() {
        drive.has_disc = false;
    }

    render_whole("ejected", &app);
}

// ---------------------------------------------------------------------------
// The top bar's state line
// ---------------------------------------------------------------------------
/// A finished run says what it did, under the verdict: this is the whole report
/// of the run now that there is no result card.
#[test]
fn the_finished_run_reports_its_own_figures() {
    let mut app = blank(Stage::Done);
    app.disc = Some(tv_disc());
    with_drive(&mut app);
    app.rebuild_jobs();
    app.total = 6;
    app.done = 6;
    app.failed = 1;
    app.session_bytes = 8_400_000_000;
    app.rip_elapsed = Some(Duration::from_secs(3_600));
    app.ejected = true;

    let line = crate::view::top_detail(&app);

    assert!(line.contains("5 of 6 files"), "{line}");
    assert!(line.contains("written"), "{line}");
    assert!(line.contains("1 failed"), "{line}");
    assert!(line.contains("1:00:00"), "the run's duration: {line}");
    assert!(line.contains("disc ejected"), "{line}");
}

/// A run reads the plan it started with, so nothing that would rewrite the plan
/// gets through while it goes — including a keypress in a field that was focused
/// when the run began.
#[test]
fn a_run_refuses_edits_to_its_plan() {
    let mut app = blank(Stage::Plan);
    app.disc = Some(tv_disc());
    with_drive(&mut app);
    app.classification = Some(tv_classification());
    app.rebuild_jobs();
    app.season_input = "1".into();
    app.show_input = "Dragon Ball".into();

    let before: Vec<bool> = app.jobs.iter().map(|job| job.enabled).collect();
    assert!(before.iter().any(|enabled| *enabled), "a fixture to toggle");

    app.stage = Stage::Ripping;
    for message in [
        Message::ToggleJob(0),
        Message::SelectAll(false),
        Message::ToggleExtras,
        Message::KindChosen(DiscKind::Movie),
        Message::SeasonChanged("9".into()),
        Message::ShowChanged("Something else".into()),
        Message::Browse,
    ] {
        let _ = app.update(message);
    }

    assert_eq!(
        before,
        app.jobs.iter().map(|job| job.enabled).collect::<Vec<_>>(),
        "the selection was rewritten mid-run"
    );
    assert_eq!(app.season_input, "1");
    assert_eq!(app.show_input, "Dragon Ball");
    assert!(!app.include_extras);
    assert_eq!(app.stage, Stage::Ripping);
}

/// The same messages do apply when nothing is running, so the guard is a guard
/// and not a dead path.
#[test]
fn a_plan_can_be_edited_when_no_run_is_going() {
    let mut app = blank(Stage::Plan);
    app.disc = Some(tv_disc());
    with_drive(&mut app);
    app.classification = Some(tv_classification());
    app.rebuild_jobs();

    let first = app.jobs[0].enabled;
    let _ = app.update(Message::ToggleJob(0));
    let _ = app.update(Message::SeasonChanged("9".into()));

    assert_eq!(app.jobs[0].enabled, !first);
    assert_eq!(app.season_input, "9");
}

/// Backing up needs something to back up: the button in the bar is dead until a
/// file is selected.
#[test]
fn back_up_needs_a_selection() {
    let mut app = blank(Stage::Plan);
    app.disc = Some(tv_disc());
    with_drive(&mut app);
    app.classification = Some(tv_classification());
    app.rebuild_jobs();

    assert!(app.has_selection());
    let _ = app.update(Message::SelectAll(false));
    assert!(!app.has_selection());
}

/// A drive that has stopped answering has to be reported: nothing else on the
/// screen changes while a read is waiting on it, so silence is indistinguishable
/// from a freeze.
#[test]
fn a_silent_drive_is_reported_while_something_waits_on_it() {
    let mut app = blank(Stage::Ripping);
    app.disc = Some(tv_disc());
    with_drive(&mut app);
    app.rebuild_jobs();

    // Just asked, so there is nothing to report.
    assert_eq!(app.quiet_for(), None);

    app.signal_at = Instant::now() - Duration::from_secs(31);
    assert!(app.quiet_for().is_some(), "a run waiting on the drive");

    // A watcher probe is a read too, and goes quiet the same way.
    app.stage = Stage::Plan;
    app.drive_watch_in_flight = true;
    assert!(
        app.quiet_for().is_some(),
        "the watcher waiting on the drive"
    );

    // Nothing waiting on the disc: nothing to complain about.
    app.drive_watch_in_flight = false;
    assert_eq!(app.quiet_for(), None);
}

#[test]
fn the_status_line_names_the_stalled_drive() {
    let mut app = blank(Stage::Ripping);
    app.disc = Some(tv_disc());
    with_drive(&mut app);
    app.classification = Some(tv_classification());
    app.rebuild_jobs();
    app.total = app.jobs.len();
    app.signal_at = Instant::now() - Duration::from_secs(45);

    let line = crate::view::top_detail(&app);

    assert!(line.contains("has not answered"), "{line}");
    assert!(line.contains("45s"), "{line}");

    // Reading a disc says the same thing, on the same line.
    app.stage = Stage::Plan;
    app.detect_loading = true;
    let line = crate::view::top_detail(&app);
    assert!(line.contains("has not answered"), "{line}");
}

/// Ejecting after a run has to put the screen back. The disc is what the cards
/// and the plan describe, and it is no longer in the drive; the run it just
/// finished is still the run it just finished, so its figures stay on the bar.
#[test]
fn an_ejected_disc_resets_the_screen_after_a_run() {
    let mut app = blank(Stage::Done);
    app.disc = Some(tv_disc());
    with_drive(&mut app);
    app.classification = Some(tv_classification());
    app.rebuild_jobs();
    app.total = app.jobs.len();
    app.done = app.total;
    app.session_bytes = 8_400_000_000;
    app.rip_elapsed = Some(Duration::from_secs(3_600));
    app.ejected = true;

    app.disc_removed();

    assert!(app.disc.is_none(), "the disc is gone");
    assert!(app.jobs.is_empty(), "so is the plan read off it");
    assert!(app.device.is_none());
    assert!(app.label.is_none());
    assert!(app.detect_note.as_deref() == Some("The disc was ejected."));
    assert_eq!(app.stage, Stage::Done, "the finished run is still reported");
    let line = crate::view::top_detail(&app);
    assert!(line.contains("written"), "the run's figures: {line}");
}

/// The scan's idea of what is in a drive comes from `/sys/block`, which on a
/// drive the kernel is not polling keeps saying "no disc" long after one is in.
/// Eject must not be greyed out by that: the window knows which device it read
/// the disc from.
#[test]
fn a_disc_read_from_a_drive_can_be_ejected_however_the_scan_reads_it() {
    let mut app = blank(Stage::Plan);
    app.drives = vec![drive("/dev/sr0", false)];
    app.selected_drive = Some(0);
    app.device = Some("/dev/sr0".into());

    assert_eq!(
        crate::view::ejectable_device(&app),
        Some(PathBuf::from("/dev/sr0"))
    );

    // A disc opened from a folder is not in a drive, so there is nothing to do.
    app.device = None;
    assert_eq!(crate::view::ejectable_device(&app), None);
}

/// The one way on from a finished run: back to the plan, with the disc still
/// loaded to take more from. The two buttons this replaced did the same thing
/// except that one of them threw the disc away, which cost a re-read to undo.
#[test]
fn a_finished_run_goes_back_to_its_plan() {
    let mut app = blank(Stage::Done);
    app.disc = Some(tv_disc());
    with_drive(&mut app);
    app.classification = Some(tv_classification());
    app.rebuild_jobs();
    let jobs = app.jobs.len();

    let _ = app.update(Message::BackToPlan);

    assert_eq!(app.stage, Stage::Plan);
    assert!(app.disc.is_some(), "the disc is still loaded");
    assert_eq!(app.jobs.len(), jobs, "and so is the plan made from it");
    assert!(app.progress.is_none());
    assert!(app.current.is_none());
}

/// Done after an eject lands on the picker: the disc took the plan with it, so
/// there is nothing left to go back to but the next disc.
#[test]
fn done_after_an_eject_lands_on_the_picker() {
    let mut app = blank(Stage::Done);
    app.disc = Some(tv_disc());
    with_drive(&mut app);
    app.classification = Some(tv_classification());
    app.rebuild_jobs();
    app.disc_removed();

    let _ = app.update(Message::BackToPlan);

    assert_eq!(app.stage, Stage::Plan);
    assert!(app.disc.is_none());
    assert!(app.jobs.is_empty());
}

fn movie(title: &str, year: u16, id: u32) -> packrat_core::Movie {
    packrat_core::Movie {
        id,
        title: title.to_string(),
        original_title: None,
        release_date: Some(format!("{year}-03-31")),
        runtime: Some(136),
        popularity: Some(42.0),
    }
}

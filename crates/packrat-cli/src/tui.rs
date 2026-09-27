//! Full-screen interactive backup flow.
//!
//! Running `packrat` with no subcommand lands here. The TUI detects a disc,
//! shows what it thinks the disc is, lets the user correct the show/season and
//! pick which files to write, then rips with live per-file progress.
//!
//! All slow work (drive probing, TVmaze lookups, remuxing) happens on worker
//! threads that report back over a channel, so the interface never blocks.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use packrat_core::{
    classify, display_name, feature_titles, movie_extra_file_in, movie_extras, movie_file_in,
    parse_label, preferred_titles, read_disc, read_vts, remux_chain_with_progress_and_cancel,
    split_title, Classification, DiscError, DiscKind, DiscModel, DiscSource, LabelInfo,
    LibraryStats, Movie, OpticalDrive, RemuxPhase, RemuxProgress, RemuxReport, Show, Title,
    EXTRA_MIN, MIN_CONTENT,
};

use crate::config::{self, Config};
use crate::history::{self, History};
use crate::{resolve_movie_naming, resolve_naming, MovieNaming, Naming};

/// Spinner frames used while a background task runs.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// How often the header rotates to the next collection stat, in loop ticks.
/// The event loop wakes ten times a second, so this is about twelve seconds.
const STATS_ROTATE_TICKS: usize = 120;

/// Kaomoji frames for the header mouse. `RAT_IDLE` is the base pose; the
/// others blink, flick an ear, sniff, or carry a load. The tail stays put —
/// swapping multi-cell glyphs cannot interpolate smoothly. Each frame is three
/// lines; the renderer pads them to a common width so the ambiguous-width
/// glyphs cannot shift the art. The feet sit under the middle of the body, and
/// the trailing space on that row keeps every pose the same width so the mouse
/// does not jog sideways as it animates.
const RAT_IDLE: [&str; 3] = ["　C・プ", "＼(　）", "　｀｀　"];
const RAT_BLINK: [&str; 3] = ["　C－プ", "＼(　）", "　｀｀　"];
const RAT_EAR: [&str; 3] = ["　^・プ", "＼(　）", "　｀｀　"];
const RAT_SNIFF: [&str; 3] = ["　C・プ｡", "＼(　）", "　｀｀　"];
const RAT_PACK: [&str; 3] = ["　C・プ", "＼(▣ ）", "　｀｀　"];
const RAT_PACK_BLINK: [&str; 3] = ["　C－プ", "＼(▣ ）", "　｀｀　"];
const RAT_CHEESE: [&str; 3] = ["　C・プ■■", "＼(　）", "　｀｀　"];
const RAT_CHEESE_BLINK: [&str; 3] = ["　C－プ■■", "＼(　）", "　｀｀　"];
const RAT_ERROR: [&str; 3] = ["　Cｘプ", "＼(　）", "　｀｀　"];

/// Plain-ASCII fallback for terminals without the wide glyphs.
const RAT_ASCII: [[&str; 3]; 3] = [
    ["   __         ", "  <o,o)___    ", "   ^   ^ )~~~ "],
    ["   __         ", "  <o,o)___    ", "    ^ ^  )~~  "],
    ["   __         ", "  <o,-)___    ", "   ^   ^ )~   "],
];

/// Lay a frame out as equal-width lines. Padding on the right (rather than
/// right-aligning each line) keeps the mouse's parts in register, and the
/// whole mouse is tinted a soft grey.
fn rat_lines(frame: &'static [&'static str; 3]) -> Vec<Line<'static>> {
    let colour = Color::Rgb(200, 195, 205);
    let mut lines: Vec<Line> = frame
        .iter()
        .map(|row| Line::from(Span::styled(*row, Style::default().fg(colour))))
        .collect();
    let width = lines.iter().map(Line::width).max().unwrap_or(0);
    for line in &mut lines {
        let pad = width.saturating_sub(line.width());
        if pad > 0 {
            line.spans.push(Span::raw(" ".repeat(pad)));
        }
    }
    lines
}

/// Launch the interactive flow. Returns once the user quits.
pub fn run() -> Result<()> {
    // Ctrl-C sent while the terminal is in raw mode arrives as a key event,
    // but an external SIGINT/SIGTERM does not. Catch those too so a rip is
    // stopped and the terminal restored instead of the process just vanishing.
    let shutdown = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        let _ = signal_hook::flag::register(signal, Arc::clone(&shutdown));
    }

    // If we panic while the terminal is in raw mode, put it back first.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratatui::restore();
        original_hook(info);
    }));

    let mut terminal = ratatui::init();
    let mut app = App::new(shutdown);
    let result = app.event_loop(&mut terminal);
    // Last-resort cleanup if the loop bailed out with `?`: stop and join any
    // rip worker before the terminal is restored.
    app.finish_rip();
    ratatui::restore();
    result
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Config,
    Detect,
    Plan,
    Ripping,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Show,
    Season,
    /// First episode on the disc, for a TV disc the label placed wrongly.
    Episode,
    Jobs,
}

/// A setting shown on the config screen.
///
/// Destinations live here rather than on the scan screen so a new media type
/// (CDs, for example) only has to add a variant and a [`Config`] field, not a
/// new row to the scan layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigField {
    TvDir,
    MovieDir,
    TmdbKey,
    AutoEject,
}

impl ConfigField {
    /// Every setting, in tab order. This is the one list to extend when a new
    /// media type gains its own destination.
    const ALL: [ConfigField; 4] = [
        ConfigField::TvDir,
        ConfigField::MovieDir,
        ConfigField::TmdbKey,
        ConfigField::AutoEject,
    ];

    fn next(self) -> Self {
        let index = Self::ALL
            .iter()
            .position(|field| *field == self)
            .unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    fn prev(self) -> Self {
        let index = Self::ALL
            .iter()
            .position(|field| *field == self)
            .unwrap_or(0);
        Self::ALL[(index + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    /// Whether this row is a checkbox rather than a text field.
    fn is_toggle(self) -> bool {
        matches!(self, Self::AutoEject)
    }

    fn title(self) -> &'static str {
        match self {
            Self::TvDir => " TV directory ",
            Self::MovieDir => " Movie directory ",
            Self::TmdbKey => " TMDb API key ",
            Self::AutoEject => " Eject when done ",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::TvDir => "the folder that holds show folders, e.g. /mnt/media/tv",
            Self::MovieDir => "the folder that holds movie folders, e.g. /mnt/media/movies",
            Self::TmdbKey => "optional; without it movies are named from the disc label",
            Self::AutoEject => "Eject the disc automatically once a rip finishes",
        }
    }
}

/// Which control the Detect screen is editing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetectField {
    /// The manual path text box.
    Path,
    /// The list of attached optical drives.
    Drives,
}

impl Field {
    /// The next field in tab order. A movie has no episode field, so it moves
    /// straight from the year to the file list.
    fn next(self, is_movie: bool) -> Self {
        match (self, is_movie) {
            (Self::Show, _) => Self::Season,
            (Self::Season, true) => Self::Jobs,
            (Self::Season, false) => Self::Episode,
            (Self::Episode, _) => Self::Jobs,
            (Self::Jobs, _) => Self::Show,
        }
    }

    fn prev(self, is_movie: bool) -> Self {
        match (self, is_movie) {
            (Self::Show, _) => Self::Jobs,
            (Self::Jobs, true) => Self::Season,
            (Self::Jobs, false) => Self::Episode,
            (Self::Episode, _) => Self::Season,
            (Self::Season, _) => Self::Show,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MetaState {
    Idle,
    Loading,
    Ready,
}

enum JobStatus {
    Pending,
    Running,
    Done(RemuxReport),
    Failed(String),
    Cancelled,
}

/// One planned output file.
struct Job {
    title: u16,
    first: u16,
    last: u16,
    path: PathBuf,
    /// Playback duration of the chapter range, used to estimate time left.
    duration: Duration,
    enabled: bool,
    status: JobStatus,
}

impl Job {
    fn new(title: &Title, first: u16, last: u16, path: PathBuf) -> Self {
        Self {
            title: title.number,
            first,
            last,
            path,
            duration: chapter_span_duration(title, first, last),
            enabled: true,
            status: JobStatus::Pending,
        }
    }
}

/// Sum of the per-chapter durations over the 1-based inclusive range.
fn chapter_span_duration(title: &Title, first: u16, last: u16) -> Duration {
    title
        .chapter_durations
        .iter()
        .skip(usize::from(first.saturating_sub(1)))
        .take(usize::from(last.saturating_sub(first).saturating_add(1)))
        .copied()
        .sum()
}

/// Playback duration still to be ripped: jobs the user left selected that have
/// not started yet. Unselected jobs never enter the queue, so counting them
/// would overstate the remaining time.
fn remaining_queue_duration(jobs: &[Job]) -> Duration {
    jobs.iter()
        .filter(|job| job.enabled && matches!(job.status, JobStatus::Pending))
        .map(|job| job.duration)
        .sum()
}

/// A single-line editable text field with a cursor.
#[derive(Default, Clone)]
struct TextInput {
    chars: Vec<char>,
    cursor: usize,
}

impl TextInput {
    fn from(s: &str) -> Self {
        let chars: Vec<char> = s.chars().collect();
        let cursor = chars.len();
        Self { chars, cursor }
    }

    fn as_str(&self) -> String {
        self.chars.iter().collect()
    }

    fn trimmed(&self) -> String {
        self.as_str().trim().to_string()
    }

    fn insert(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn right(&mut self) {
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
    }
}

/// Events sent from worker threads back to the UI thread.
enum WorkerEvent {
    DiscReady {
        source: DiscSource,
        disc: DiscModel,
        classification: Classification,
        label: LabelInfo,
        device: Option<PathBuf>,
    },
    DiscError(String),
    Drives(Vec<OpticalDrive>),
    MetaReady {
        show: Option<Show>,
        naming: Option<Naming>,
        note: Option<String>,
    },
    MovieMetaReady {
        naming: MovieNaming,
        movie: Option<Movie>,
        candidates: Vec<(f64, Movie)>,
        note: Option<String>,
    },
    MetaError(String),
    JobStarted(usize),
    JobProgress {
        index: usize,
        phase: RemuxPhase,
        bytes_done: u64,
        bytes_total: u64,
        written_bytes: u64,
        elapsed: Duration,
    },
    JobDone {
        index: usize,
        result: Result<RemuxReport, String>,
    },
    RipDone {
        cancelled: bool,
    },
    EjectDone {
        result: Result<(), String>,
    },
    /// A refreshed collection summary from the background scan.
    LibraryStats {
        library: LibraryStats,
        free_bytes: Option<u64>,
    },
}

/// The latest progress from the file currently being ripped.
struct ProgressSnapshot {
    index: usize,
    phase: RemuxPhase,
    /// VOB bytes read from the disc so far, across both passes.
    bytes_done: u64,
    /// VOB bytes that will be read across both passes.
    bytes_total: u64,
    /// Bytes written to the output file so far.
    written_bytes: u64,
    elapsed: Duration,
    /// Disc read rate over the last progress interval, in bytes per second.
    read_rate: Option<f64>,
    /// Output write rate over the last progress interval, in bytes per second.
    write_rate: Option<f64>,
}

struct App {
    stage: Stage,
    tx: Sender<WorkerEvent>,
    rx: Receiver<WorkerEvent>,
    /// Set to stop after the file currently being written (soft cancel).
    cancel: Arc<AtomicBool>,
    /// Set to stop the file currently being written immediately (hard abort).
    abort: Arc<AtomicBool>,
    /// Set by the SIGINT/SIGTERM handler to request a graceful exit.
    shutdown: Arc<AtomicBool>,
    /// The rip worker, joined on exit so a cancelled file is cleaned up.
    rip_handle: Option<JoinHandle<()>>,
    tick: usize,

    // Disc detection
    detect_loading: bool,
    /// A detection thread is in flight; guards against overlapping scans.
    scan_in_flight: bool,
    drives: Vec<OpticalDrive>,
    /// Selected row in `drives`.
    drive_cursor: usize,
    detect_focus: DetectField,
    /// Drive last used, restored on the next launch and saved to config.
    last_device: Option<PathBuf>,
    path_input: TextInput,
    detect_note: Option<String>,
    /// Fingerprint of the disc currently loaded, so a background re-scan of
    /// the same disc does not reset the user's plan.
    loaded: Option<String>,

    // The disc under consideration
    source: Option<DiscSource>,
    disc: Option<DiscModel>,
    classification: Option<Classification>,
    label: Option<LabelInfo>,
    #[allow(dead_code)]
    device: Option<PathBuf>,

    // Settings (config screen)
    config_focus: ConfigField,
    /// Where the config screen returns to when it closes.
    config_return: Stage,
    /// First run: there was no config file, so the screen is onboarding rather
    /// than a settings visit.
    config_onboarding: bool,
    config_note: Option<String>,

    // Editable plan
    show_input: TextInput,
    season_input: TextInput,
    /// Manual first episode number; empty means let the match decide.
    first_episode_input: TextInput,
    tv_input: TextInput,
    movie_input: TextInput,
    tmdb_input: TextInput,
    /// Open the tray automatically when a rip finishes cleanly.
    auto_eject: bool,
    /// A rip or an explicit command has opened the tray; the loaded disc is
    /// gone even though its plan is still on screen.
    ejected: bool,
    /// After a successful eject, fall back to the drive picker rather than
    /// staying on the current screen.
    eject_then_picker: bool,
    eject_note: Option<String>,
    include_extras: bool,
    naming: Option<Naming>,
    /// Metadata naming for a movie disc, when one was resolved.
    movie_naming: Option<MovieNaming>,
    meta_show: Option<Show>,
    /// Auto-accepted TMDb match for a movie disc.
    meta_movie: Option<Movie>,
    /// Ranked TMDb candidates offered when no match cleared the threshold.
    movie_candidates: Vec<(f64, Movie)>,
    movie_cursor: usize,
    meta_note: Option<String>,
    meta_state: MetaState,
    jobs: Vec<Job>,
    job_cursor: usize,
    focus: Field,

    // Ripping
    total: usize,
    done: usize,
    failed: usize,
    cancelled: bool,
    current: Option<ProgressSnapshot>,
    /// Files written and payload bytes for the rip in progress, folded into
    /// `history` when it finishes.
    session_files: u64,
    session_bytes: u64,

    // Collection summary for the header
    library: LibraryStats,
    history: History,
    /// Free bytes on the destination volume, when it could be read.
    free_bytes: Option<u64>,
    /// A scan is in flight, so a second request waits rather than piling on.
    stats_in_flight: bool,

    show_help: bool,
    should_quit: bool,
}

impl App {
    fn new(shutdown: Arc<AtomicBool>) -> Self {
        let mut app = Self::initial();
        app.shutdown = shutdown;
        app.refresh_stats();
        if config::exists() {
            app.begin_detection(true);
        } else {
            // First run: set up the storage directories before scanning.
            app.start_onboarding();
        }
        app
    }

    /// Build the initial state without starting a drive probe (tests use this
    /// directly; `new` follows it with a scan).
    fn initial() -> Self {
        let (tx, rx) = mpsc::channel();
        let config = Config::load();
        let tv_input = TextInput::from(&display_path(config.tv_dir.as_deref()));
        let movie_input = TextInput::from(&display_path(config.movie_dir.as_deref()));
        let tmdb_input = TextInput::from(config.tmdb_api_key.as_deref().unwrap_or(""));
        Self {
            stage: Stage::Detect,
            tx,
            rx,
            cancel: Arc::new(AtomicBool::new(false)),
            abort: Arc::new(AtomicBool::new(false)),
            shutdown: Arc::new(AtomicBool::new(false)),
            rip_handle: None,
            tick: 0,
            detect_loading: false,
            scan_in_flight: false,
            drives: Vec::new(),
            drive_cursor: 0,
            detect_focus: DetectField::Drives,
            last_device: config.last_device.clone(),
            path_input: TextInput::default(),
            detect_note: None,
            loaded: None,
            source: None,
            disc: None,
            classification: None,
            label: None,
            device: None,
            show_input: TextInput::default(),
            season_input: TextInput::default(),
            first_episode_input: TextInput::default(),
            tv_input,
            movie_input,
            tmdb_input,
            config_focus: ConfigField::TvDir,
            config_return: Stage::Detect,
            config_onboarding: false,
            config_note: None,
            auto_eject: config.auto_eject,
            ejected: false,
            eject_then_picker: false,
            eject_note: None,
            include_extras: false,
            naming: None,
            movie_naming: None,
            meta_show: None,
            meta_movie: None,
            movie_candidates: Vec::new(),
            movie_cursor: 0,
            meta_note: None,
            meta_state: MetaState::Idle,
            jobs: Vec::new(),
            job_cursor: 0,
            focus: Field::Jobs,
            total: 0,
            done: 0,
            failed: 0,
            cancelled: false,
            current: None,
            session_files: 0,
            session_bytes: 0,
            library: LibraryStats::default(),
            history: History::load(),
            free_bytes: None,
            stats_in_flight: false,
            show_help: false,
            should_quit: false,
        }
    }

    /// Start a drive probe on a worker thread. `show_spinner` draws the
    /// scanning state for the initial probe and manual path loads; background
    /// re-scans leave the current screen visible.
    ///
    /// The last-used drive is auto-loaded when it is present, so a two-drive
    /// setup does not force a choice every run; otherwise a single ready disc
    /// loads straight away and several ready discs leave the picker up.
    fn begin_detection(&mut self, show_spinner: bool) {
        self.detect_loading = show_spinner;
        self.scan_in_flight = true;
        detect_and_load(self.tx.clone(), self.last_device.clone());
    }

    /// Refresh the drive list without loading anything, used by the picker and
    /// the "change drive" action.
    fn refresh_drives(&mut self) {
        self.scan_in_flight = true;
        self.detect_loading = false;
        list_drives(self.tx.clone());
    }

    /// Rescan the library in the background for the header summary. A request
    /// already in flight is left to finish, so a burst of changes only scans
    /// once.
    fn refresh_stats(&mut self) {
        if self.stats_in_flight {
            return;
        }
        self.stats_in_flight = true;
        scan_stats(self.tx.clone(), self.tv_path(), self.movie_path());
    }

    /// Fold the finished rip into the persisted history and refresh the
    /// collection totals so the header reflects the files just written.
    fn record_backup(&mut self) {
        let files = self.session_files;
        let bytes = self.session_bytes;
        self.session_files = 0;
        self.session_bytes = 0;
        if files > 0 {
            self.history.record(files, bytes, history::unix_now());
            if let Err(e) = self.history.save() {
                self.meta_note = Some(format!("Could not save backup history: {e:#}"));
            }
        }
        self.refresh_stats();
    }

    /// Load the disc in the highlighted drive.
    fn load_selected_drive(&mut self) {
        let Some(drive) = self.drives.get(self.drive_cursor).cloned() else {
            self.detect_note = Some("No drive selected".into());
            return;
        };
        let Some(mount) = drive.mount.clone() else {
            self.detect_note = Some(format!("{} has no mounted disc", drive.device.display()));
            return;
        };
        self.detect_note = None;
        self.detect_loading = true;
        self.scan_in_flight = true;
        load_drive(self.tx.clone(), drive.device, mount);
    }

    /// Return to the drive picker from the plan, without discarding it.
    fn change_drive(&mut self) {
        self.stage = Stage::Detect;
        self.detect_focus = DetectField::Drives;
        self.detect_note = None;
        self.refresh_drives();
    }

    // -- Settings (config screen) -----------------------------------------

    /// First run: open the settings screen before any disc scan, so the
    /// destination directories are set up front. Saving writes the config,
    /// which is what ends onboarding on the next launch.
    fn start_onboarding(&mut self) {
        self.config_return = Stage::Detect;
        self.config_onboarding = true;
        self.config_focus = ConfigField::TvDir;
        self.config_note = None;
        self.stage = Stage::Config;
    }

    /// Open the settings screen from the guide, remembering where to return.
    fn open_config(&mut self) {
        self.config_return = self.stage;
        self.config_onboarding = false;
        self.config_focus = ConfigField::TvDir;
        self.config_note = None;
        self.stage = Stage::Config;
    }

    /// Leave the settings screen and resume the prior view. Persisting is the
    /// caller's job; this only restores the view and refreshes what depends on
    /// the destinations.
    fn close_config(&mut self) {
        self.stage = self.config_return;
        match self.stage {
            Stage::Detect => self.begin_detection(self.disc.is_none()),
            Stage::Plan => self.apply_edits(),
            _ => {}
        }
    }

    /// The text field behind a config row.
    fn config_input(&self, field: ConfigField) -> &TextInput {
        match field {
            ConfigField::TvDir => &self.tv_input,
            ConfigField::MovieDir => &self.movie_input,
            ConfigField::TmdbKey => &self.tmdb_input,
            ConfigField::AutoEject => unreachable!("the eject toggle is not a text field"),
        }
    }

    fn config_input_mut(&mut self, field: ConfigField) -> &mut TextInput {
        match field {
            ConfigField::TvDir => &mut self.tv_input,
            ConfigField::MovieDir => &mut self.movie_input,
            ConfigField::TmdbKey => &mut self.tmdb_input,
            ConfigField::AutoEject => unreachable!("the eject toggle is not a text field"),
        }
    }

    /// Discard unsaved settings edits by restoring the persisted values.
    fn reload_config_inputs(&mut self) {
        let config = Config::load();
        self.tv_input = TextInput::from(&display_path(config.tv_dir.as_deref()));
        self.movie_input = TextInput::from(&display_path(config.movie_dir.as_deref()));
        self.tmdb_input = TextInput::from(config.tmdb_api_key.as_deref().unwrap_or(""));
        self.auto_eject = config.auto_eject;
    }

    // -- Tray control -----------------------------------------------------

    /// Eject `device` on a worker thread so the UI never blocks on the ioctl or
    /// the fallback command-line tool. `then_picker` returns to the drive
    /// picker on success; otherwise the current screen stays put.
    fn eject_drive(&mut self, device: PathBuf, then_picker: bool) {
        self.eject_note = None;
        self.eject_then_picker = then_picker;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = packrat_core::drives::eject(&device);
            let _ = tx.send(WorkerEvent::EjectDone { result });
        });
    }

    /// Eject the drive the loaded disc came from, if it is a real drive.
    fn eject_loaded_disc(&mut self, then_picker: bool) {
        match self.device.clone() {
            Some(device) => self.eject_drive(device, then_picker),
            None => self.meta_note = Some("No drive to eject for a path-loaded disc".into()),
        }
    }

    /// Eject the drive highlighted in the picker.
    fn eject_selected_drive(&mut self) {
        let Some(drive) = self.drives.get(self.drive_cursor) else {
            self.detect_note = Some("No drive selected".into());
            return;
        };
        if !drive.has_disc {
            self.detect_note = Some(format!("{} has no disc", drive.device.display()));
            return;
        }
        self.detect_note = None;
        self.eject_drive(drive.device.clone(), false);
    }

    /// The success screen's "eject and configure another": open the tray, then
    /// return to the picker when the eject lands.
    fn eject_and_configure(&mut self) {
        if self.ejected || self.device.is_none() {
            // Nothing to open (or it is already open): just move on.
            self.back_to_plan();
        } else {
            self.eject_loaded_disc(true);
        }
    }

    /// Whether the drive the loaded disc came from still reports ready media.
    /// A manually loaded path is not tied to a drive, so it is assumed present.
    fn loaded_drive_ready(&self, drives: &[OpticalDrive]) -> bool {
        match &self.device {
            Some(device) => drives
                .iter()
                .any(|drive| &drive.device == device && drive.has_disc && drive.mount.is_some()),
            None => true,
        }
    }

    /// Forget the loaded disc and return to the picker, e.g. after an eject, so
    /// the plan does not linger over a disc that is no longer there.
    fn clear_disc(&mut self) {
        self.source = None;
        self.disc = None;
        self.classification = None;
        self.label = None;
        self.device = None;
        self.loaded = None;
        self.naming = None;
        self.movie_naming = None;
        self.meta_show = None;
        self.meta_movie = None;
        self.movie_candidates.clear();
        self.movie_cursor = 0;
        self.meta_note = None;
        self.meta_state = MetaState::Idle;
        self.jobs.clear();
        self.job_cursor = 0;
        self.show_input = TextInput::default();
        self.season_input = TextInput::default();
        self.first_episode_input = TextInput::default();
        self.ejected = false;
        self.eject_then_picker = false;
        self.eject_note = None;
        self.detect_note = Some("Disc ejected — insert a disc or choose a drive".into());
        self.detect_focus = DetectField::Drives;
        self.stage = Stage::Detect;
    }

    /// Replace the drive list, keeping the cursor on the drive that was
    /// highlighted, then the last-used one, then the first ready disc.
    fn set_drives(&mut self, drives: Vec<OpticalDrive>) {
        let highlighted = self
            .drives
            .get(self.drive_cursor)
            .map(|drive| drive.device.clone());
        self.drive_cursor = highlighted
            .as_ref()
            .and_then(|device| drives.iter().position(|drive| &drive.device == device))
            .or_else(|| {
                self.last_device
                    .as_ref()
                    .and_then(|device| drives.iter().position(|drive| &drive.device == device))
            })
            .or_else(|| {
                drives
                    .iter()
                    .position(|drive| drive.has_disc && drive.mount.is_some())
            })
            .unwrap_or(0)
            .min(drives.len().saturating_sub(1));
        self.drives = drives;
    }

    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.should_quit {
            if self.shutdown.load(Ordering::Relaxed) {
                self.request_shutdown();
                break;
            }

            terminal.draw(|f| self.ui(f))?;

            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        self.on_key(key);
                    }
                }
            }

            let events: Vec<WorkerEvent> = self.rx.try_iter().collect();
            for ev in events {
                self.on_worker(ev);
            }

            self.poll_disc_change();

            self.tick = self.tick.wrapping_add(1);
        }
        // Stop a running rip and wait for it to remove its `.partial` output
        // before the terminal (and process) go away.
        self.finish_rip();
        Ok(())
    }

    /// Ask a running rip to stop immediately and leave the guide.
    fn request_shutdown(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.abort.store(true, Ordering::Relaxed);
        self.should_quit = true;
    }

    /// Wait for the rip worker to finish, cancelling it first if it is still
    /// running, so no half-written file is left behind.
    fn finish_rip(&mut self) {
        if self.rip_handle.is_some() {
            self.cancel.store(true, Ordering::Relaxed);
            self.abort.store(true, Ordering::Relaxed);
        }
        if let Some(handle) = self.rip_handle.take() {
            let _ = handle.join();
        }
    }

    /// Re-scan for a disc while the user is still configuring, so swapping
    /// discs updates the guide without a restart. Paused once a rip starts.
    fn poll_disc_change(&mut self) {
        if self.scan_in_flight || !matches!(self.stage, Stage::Detect | Stage::Plan) {
            return;
        }
        // The loop wakes ~10 times a second; scan every couple of seconds.
        if self.tick % 20 != 0 {
            return;
        }
        // Once the picker has drives up, only refresh the list: auto-loading on
        // a poll would yank the user away from a choice they are making.
        if self.stage == Stage::Detect && !self.drives.is_empty() {
            self.refresh_drives();
        } else {
            self.begin_detection(false);
        }
    }

    // -- Input ------------------------------------------------------------

    fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.request_shutdown();
            return;
        }

        if self.show_help {
            self.show_help = false;
            return;
        }
        if key.code == KeyCode::Char('?') && !matches!(self.stage, Stage::Detect | Stage::Config) {
            self.show_help = true;
            return;
        }

        match self.stage {
            Stage::Config => self.on_key_config(key),
            Stage::Detect => self.on_key_detect(key),
            Stage::Plan => self.on_key_plan(key),
            Stage::Ripping => match key.code {
                // Soft cancel: finish the file in progress, then stop.
                KeyCode::Char('q') | KeyCode::Esc => {
                    self.cancel.store(true, Ordering::Relaxed);
                }
                // Hard abort: stop the file in progress right now.
                KeyCode::Char('a') => {
                    self.abort.store(true, Ordering::Relaxed);
                }
                _ => {}
            },
            Stage::Done => match key.code {
                KeyCode::Char('c') => self.back_to_plan(),
                KeyCode::Char('x') => self.eject_and_configure(),
                KeyCode::Enter | KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char(' ') => {
                    self.should_quit = true;
                }
                _ => {}
            },
        }
    }

    /// The settings screen: edit a directory, save and continue, or back out.
    fn on_key_config(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Tab => self.config_focus = self.config_focus.next(),
            KeyCode::BackTab => self.config_focus = self.config_focus.prev(),
            KeyCode::Enter => match self.persist_config() {
                Ok(_) => {
                    self.close_config();
                    self.refresh_stats();
                }
                Err(e) => self.config_note = Some(format!("Could not save settings: {e}")),
            },
            KeyCode::Esc => {
                // Leaving without saving drops the edits; a first-time user has
                // nothing to restore, so this just skips onboarding.
                if !self.config_onboarding {
                    self.reload_config_inputs();
                }
                self.close_config();
            }
            _ => {
                let focus = self.config_focus;
                if focus.is_toggle() {
                    // Space (or either arrow) flips the checkbox.
                    if matches!(
                        key.code,
                        KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right
                    ) {
                        self.auto_eject = !self.auto_eject;
                    }
                } else {
                    edit_input(self.config_input_mut(focus), key);
                }
            }
        }
    }

    fn on_key_detect(&mut self, key: KeyEvent) {
        if self.detect_loading {
            if key.code == KeyCode::Esc {
                self.should_quit = true;
            }
            return;
        }
        match key.code {
            KeyCode::Tab | KeyCode::BackTab => {
                self.detect_focus = match self.detect_focus {
                    DetectField::Path => DetectField::Drives,
                    DetectField::Drives => DetectField::Path,
                };
            }
            KeyCode::Up => self.drive_cursor = self.drive_cursor.saturating_sub(1),
            KeyCode::Down => {
                if self.drive_cursor + 1 < self.drives.len() {
                    self.drive_cursor += 1;
                }
            }
            KeyCode::Enter => match self.detect_focus {
                DetectField::Path => self.load_manual_path(),
                DetectField::Drives => self.load_selected_drive(),
            },
            // Rescan the picker; while typing a path, `r` is just a character.
            KeyCode::Char('r') if self.detect_focus == DetectField::Drives => {
                self.refresh_drives();
            }
            // Open the settings screen; while typing a path, `s` is just a
            // character.
            KeyCode::Char('s') if self.detect_focus == DetectField::Drives => {
                self.open_config();
            }
            // Eject the highlighted drive; while typing a path, `x` is a
            // character.
            KeyCode::Char('x') if self.detect_focus == DetectField::Drives => {
                self.eject_selected_drive();
            }
            KeyCode::Esc => {
                // Arriving here from a plan, Esc backs out instead of quitting.
                if self.disc.is_some() {
                    self.stage = Stage::Plan;
                    self.detect_note = None;
                } else {
                    self.should_quit = true;
                }
            }
            _ => {
                if self.detect_focus == DetectField::Path {
                    edit_input(&mut self.path_input, key);
                }
            }
        }
    }

    fn on_key_plan(&mut self, key: KeyEvent) {
        let is_movie = self.is_movie();
        match key.code {
            KeyCode::Tab => self.focus = self.focus.next(is_movie),
            KeyCode::BackTab => self.focus = self.focus.prev(is_movie),
            KeyCode::Esc => {
                if self.focus == Field::Jobs {
                    self.should_quit = true;
                } else {
                    self.focus = Field::Jobs;
                }
            }
            _ => match self.focus {
                Field::Show | Field::Season | Field::Episode => {
                    if key.code == KeyCode::Enter {
                        self.apply_edits();
                    } else {
                        let input = match self.focus {
                            Field::Show => &mut self.show_input,
                            Field::Season => &mut self.season_input,
                            _ => &mut self.first_episode_input,
                        };
                        edit_input(input, key);
                    }
                }
                Field::Jobs => match key.code {
                    KeyCode::Up => self.job_cursor = self.job_cursor.saturating_sub(1),
                    KeyCode::Down => {
                        if self.job_cursor + 1 < self.jobs.len() {
                            self.job_cursor += 1;
                        }
                    }
                    KeyCode::Char('[') if !self.movie_candidates.is_empty() => {
                        self.cycle_movie_candidate(-1);
                    }
                    KeyCode::Char(']') if !self.movie_candidates.is_empty() => {
                        self.cycle_movie_candidate(1);
                    }
                    KeyCode::Char(' ') => {
                        if let Some(job) = self.jobs.get_mut(self.job_cursor) {
                            job.enabled = !job.enabled;
                        }
                    }
                    KeyCode::Char('a') => {
                        let all = !self.jobs.is_empty() && self.jobs.iter().all(|j| j.enabled);
                        for job in &mut self.jobs {
                            job.enabled = !all;
                        }
                    }
                    KeyCode::Char('e') => self.toggle_extras(),
                    KeyCode::Char('r') => self.start_rip(),
                    KeyCode::Char('s') => self.open_config(),
                    KeyCode::Char('x') => self.eject_loaded_disc(true),
                    KeyCode::Char('d') => self.change_drive(),
                    KeyCode::Char('q') => self.should_quit = true,
                    _ => {}
                },
            },
        }
    }

    // -- Actions ----------------------------------------------------------

    /// Whether the loaded disc is a movie, so it is named from TMDb and has no
    /// season or episode fields.
    fn is_movie(&self) -> bool {
        self.classification
            .as_ref()
            .is_some_and(|c| c.kind == DiscKind::Movie)
    }

    fn tv_path(&self) -> Option<PathBuf> {
        path_from(&self.tv_input)
    }

    fn movie_path(&self) -> Option<PathBuf> {
        path_from(&self.movie_input)
    }

    /// The optional TMDb key as typed on the settings screen.
    fn tmdb_key(&self) -> Option<String> {
        let key = self.tmdb_input.trimmed();
        (!key.is_empty()).then_some(key)
    }

    fn load_manual_path(&mut self) {
        let raw = self.path_input.trimmed();
        if raw.is_empty() {
            self.detect_note = Some("Enter a path to a mounted disc".into());
            return;
        }
        self.detect_note = None;
        self.detect_loading = true;
        self.scan_in_flight = true;
        let path = config::expand_tilde(&raw);
        let tx = self.tx.clone();
        std::thread::spawn(move || match load_disc(&path, None) {
            Ok((source, disc, classification, label)) => {
                let _ = tx.send(WorkerEvent::DiscReady {
                    source,
                    disc,
                    classification,
                    label,
                    device: None,
                });
            }
            Err(e) => {
                let _ = tx.send(WorkerEvent::DiscError(e));
            }
        });
    }

    /// Recompute the metadata and job list after the user edits the show and
    /// season (or movie title and year), or corrects the first episode.
    fn apply_edits(&mut self) {
        self.meta_note = None;
        let Some(disc) = self.disc.clone() else {
            return;
        };
        // For a movie disc these two inputs carry the title and release year.
        let show = self.show_input.trimmed();
        let show = if show.is_empty() { None } else { Some(show) };
        let season = self.season_input.trimmed().parse::<u16>().ok();
        let first_episode = self.first_episode_input.trimmed().parse::<u16>().ok();

        let is_movie = self.is_movie();
        if is_movie {
            self.naming = None;
            self.meta_show = None;
            self.meta_movie = None;
            self.movie_candidates.clear();
            self.movie_cursor = 0;

            let feature = feature_titles(&disc).first().copied().cloned();
            let Some((movie_dir, feature)) = self.movie_path().zip(feature) else {
                self.movie_naming = None;
                self.meta_state = MetaState::Idle;
                self.meta_note = Some("set a movie directory to name files from metadata".into());
                self.rebuild_jobs();
                return;
            };

            self.meta_state = MetaState::Loading;
            let tx = self.tx.clone();
            let title_override = show;
            std::thread::spawn(move || {
                match resolve_movie_naming(
                    &movie_dir,
                    &disc,
                    &feature,
                    title_override.as_deref(),
                    season,
                ) {
                    Ok(resolved) => {
                        let _ = tx.send(WorkerEvent::MovieMetaReady {
                            naming: resolved.naming,
                            movie: resolved.movie,
                            candidates: resolved.candidates,
                            note: resolved.warning,
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(WorkerEvent::MetaError(format!("{e:#}")));
                    }
                }
            });
            return;
        }

        match self.tv_path() {
            Some(tv_dir) => {
                self.meta_state = MetaState::Loading;
                let tx = self.tx.clone();
                let preferred = preferred_titles(&disc);
                std::thread::spawn(move || {
                    match resolve_naming(
                        &tv_dir,
                        &disc,
                        &preferred,
                        show.as_deref(),
                        season,
                        first_episode,
                    ) {
                        Ok(resolved) => {
                            let _ = tx.send(WorkerEvent::MetaReady {
                                show: resolved.show,
                                naming: resolved.naming,
                                note: resolved.warning,
                            });
                        }
                        Err(e) => {
                            let _ = tx.send(WorkerEvent::MetaError(format!("{e:#}")));
                        }
                    }
                });
            }
            None => {
                self.naming = None;
                self.meta_show = None;
                self.meta_state = MetaState::Idle;
                self.rebuild_jobs();
            }
        }
    }

    /// Persist the settings and the last-used drive. The config screen surfaces
    /// the error; other callers are free to ignore it.
    fn persist_config(&self) -> std::result::Result<bool, String> {
        let config = Config {
            tv_dir: self.tv_path(),
            movie_dir: self.movie_path(),
            tmdb_api_key: self.tmdb_key(),
            last_device: self.last_device.clone(),
            auto_eject: self.auto_eject,
        };
        config.save().map_err(|e| format!("{e:#}"))
    }

    fn rebuild_jobs(&mut self) {
        let Some(disc) = self.disc.clone() else {
            return;
        };
        let Some(classification) = self.classification.clone() else {
            return;
        };
        let tv_dir = self.tv_path();
        let movie_dir = self.movie_path();
        let out_dir = match classification.kind {
            DiscKind::Movie => movie_dir.clone(),
            _ => tv_dir.clone(),
        }
        .unwrap_or_else(|| PathBuf::from("."));
        let jobs = assemble_jobs(
            &disc,
            &classification,
            self.naming.as_ref(),
            self.movie_naming.as_ref(),
            movie_dir.as_deref(),
            &out_dir,
            self.include_extras,
        );
        self.set_jobs(jobs);
    }

    /// Move through the TMDb candidate list, rewriting the movie naming so the
    /// job list previews the chosen title.
    fn cycle_movie_candidate(&mut self, delta: isize) {
        let len = self.movie_candidates.len();
        if len == 0 {
            return;
        }
        let next = (self.movie_cursor as isize + delta).rem_euclid(len as isize) as usize;
        let chosen = self.movie_candidates[next].1.clone();
        self.movie_cursor = next;
        self.meta_movie = Some(chosen.clone());
        if let Some(naming) = self.movie_naming.as_mut() {
            naming.retitle(chosen.title.clone(), chosen.year());
        }
        self.rebuild_jobs();
    }

    /// Toggle which bonus files are selected.
    ///
    /// On a TV disc `e` reveals or hides extras, so the job list is rebuilt. On
    /// a movie disc every title is always listed, so `e` just flips the
    /// selection of all non-primary files.
    fn toggle_extras(&mut self) {
        let is_movie = self
            .classification
            .as_ref()
            .is_some_and(|c| c.kind == DiscKind::Movie);
        if !is_movie {
            self.include_extras = !self.include_extras;
            self.rebuild_jobs();
            return;
        }
        let all_on = self.jobs.len() > 1 && self.jobs.iter().skip(1).all(|job| job.enabled);
        for job in self.jobs.iter_mut().skip(1) {
            job.enabled = !all_on;
        }
        self.include_extras = !all_on;
    }

    /// Replace the job list, carrying over each job's enabled state so a
    /// metadata refresh does not undo the user's selections.
    fn set_jobs(&mut self, jobs: Vec<Job>) {
        let merged =
            jobs.into_iter()
                .map(|mut job| {
                    if let Some(old) = self.jobs.iter().find(|o| {
                        o.title == job.title && o.first == job.first && o.last == job.last
                    }) {
                        job.enabled = old.enabled;
                    }
                    job
                })
                .collect::<Vec<_>>();
        self.jobs = merged;
        if self.job_cursor >= self.jobs.len() {
            self.job_cursor = self.jobs.len().saturating_sub(1);
        }
    }

    fn start_rip(&mut self) {
        let selected: Vec<usize> = self
            .jobs
            .iter()
            .enumerate()
            .filter(|(_, j)| j.enabled)
            .map(|(i, _)| i)
            .collect();
        if selected.is_empty() {
            self.meta_note = Some("Select at least one file to rip".into());
            return;
        }
        let (Some(source), Some(disc)) = (self.source.clone(), self.disc.clone()) else {
            return;
        };

        // Reap a previous run so its thread cannot outlive a new one.
        self.finish_rip();

        for job in &mut self.jobs {
            job.status = JobStatus::Pending;
        }
        self.total = selected.len();
        self.done = 0;
        self.failed = 0;
        self.cancelled = false;
        self.current = None;
        self.session_files = 0;
        self.session_bytes = 0;
        self.ejected = false;
        self.eject_then_picker = false;
        self.eject_note = None;
        self.stage = Stage::Ripping;
        self.cancel.store(false, Ordering::Relaxed);
        self.abort.store(false, Ordering::Relaxed);

        let jobs: Vec<RipJob> = selected
            .iter()
            .map(|&index| {
                let job = &self.jobs[index];
                RipJob {
                    index,
                    title: job.title,
                    first: job.first,
                    last: job.last,
                    path: job.path.clone(),
                }
            })
            .collect();
        self.rip_handle = Some(spawn_rip(
            self.tx.clone(),
            self.cancel.clone(),
            self.abort.clone(),
            source,
            disc,
            jobs,
            self.device.clone(),
        ));
    }

    /// Return from the result screen to the configure screen, keeping the
    /// current job selections, so another disc (or another pass) can be set up
    /// without restarting.
    fn back_to_plan(&mut self) {
        self.current = None;
        self.done = 0;
        self.failed = 0;
        self.cancelled = false;
        for job in &mut self.jobs {
            job.status = JobStatus::Pending;
        }
        self.meta_note = None;
        // A disc that was ejected cannot be reconfigured, so fall back to the
        // picker for the next one.
        if self.ejected {
            self.clear_disc();
            return;
        }
        self.focus = Field::Jobs;
        self.stage = Stage::Plan;
    }

    // -- Worker events ----------------------------------------------------

    fn on_worker(&mut self, event: WorkerEvent) {
        // A scan launched just before the rip started can land mid-run; never
        // let it yank the user back to the configure screen.
        if self.stage == Stage::Ripping
            && matches!(
                &event,
                WorkerEvent::DiscReady { .. } | WorkerEvent::Drives(_) | WorkerEvent::DiscError(_)
            )
        {
            self.detect_loading = false;
            self.scan_in_flight = false;
            return;
        }
        match event {
            WorkerEvent::DiscReady {
                source,
                disc,
                classification,
                label,
                device,
            } => {
                self.detect_loading = false;
                self.scan_in_flight = false;
                let fingerprint = disc_fingerprint(&source, &disc);
                if self.stage != Stage::Detect
                    && self.loaded.as_deref() == Some(fingerprint.as_str())
                {
                    // The same disc is still mounted; keep the user's plan.
                    return;
                }
                self.loaded = Some(fingerprint);
                let is_movie = classification.kind == DiscKind::Movie;
                self.show_input = TextInput::from(&label.title);
                let secondary = if is_movie {
                    label.year.map(|y| y.to_string())
                } else {
                    label.season.map(|s| s.to_string())
                };
                self.season_input = TextInput::from(&secondary.unwrap_or_default());
                self.first_episode_input = TextInput::default();
                self.source = Some(source);
                self.disc = Some(disc);
                self.classification = Some(classification);
                self.label = Some(label);
                self.naming = None;
                self.movie_naming = None;
                self.meta_show = None;
                self.meta_movie = None;
                self.movie_candidates.clear();
                self.movie_cursor = 0;
                self.meta_note = None;
                self.ejected = false;
                self.eject_then_picker = false;
                self.eject_note = None;
                // Remember which drive this came from, so the next launch
                // (and `watch`) can prefer it.
                if let Some(device) = &device {
                    if self.last_device.as_deref() != Some(device.as_path()) {
                        self.last_device = Some(device.clone());
                        let _ = self.persist_config();
                    }
                }
                self.device = device;
                self.focus = Field::Jobs;
                self.stage = Stage::Plan;
                self.rebuild_jobs();

                // With a destination already configured, look metadata up
                // straight away so titles fill in without pressing Enter first.
                let wants_lookup = if is_movie {
                    self.movie_path().is_some()
                } else {
                    self.tv_path().is_some()
                };
                if wants_lookup {
                    self.apply_edits();
                }
            }
            WorkerEvent::Drives(drives) => {
                self.detect_loading = false;
                self.scan_in_flight = false;
                let none_found = drives.is_empty();
                // The disc was ejected while its plan was up: drop the plan and
                // fall back to the picker rather than showing stale metadata.
                let loaded_gone = self.disc.is_some() && !self.loaded_drive_ready(&drives);
                self.set_drives(drives);
                self.detect_note = None;
                // With nothing to pick, put the caret in the manual path box.
                if none_found && self.detect_focus == DetectField::Drives {
                    self.detect_focus = DetectField::Path;
                }
                if loaded_gone {
                    self.clear_disc();
                }
            }
            WorkerEvent::DiscError(e) => {
                self.detect_loading = false;
                self.scan_in_flight = false;
                self.detect_note = Some(e);
            }
            WorkerEvent::MetaReady { show, naming, note } => {
                self.meta_state = MetaState::Ready;
                self.meta_show = show;
                self.naming = naming;
                self.meta_note = note;
                self.rebuild_jobs();
            }
            WorkerEvent::MovieMetaReady {
                naming,
                movie,
                candidates,
                note,
            } => {
                self.meta_state = MetaState::Ready;
                self.movie_naming = Some(naming);
                self.meta_movie = movie;
                self.movie_candidates = candidates;
                self.movie_cursor = 0;
                self.meta_note = note;
                self.rebuild_jobs();
            }
            WorkerEvent::MetaError(e) => {
                self.meta_state = MetaState::Idle;
                self.meta_note = Some(e);
            }
            WorkerEvent::JobStarted(index) => {
                if let Some(job) = self.jobs.get_mut(index) {
                    job.status = JobStatus::Running;
                }
            }
            WorkerEvent::JobProgress {
                index,
                phase,
                bytes_done,
                bytes_total,
                written_bytes,
                elapsed,
            } => {
                // Rates are deltas against the previous sample for the same
                // file and pass, so the slow probe phase does not drag the
                // write rate down once muxing starts.
                let (read_rate, write_rate) = match &self.current {
                    Some(prev) if prev.index == index && prev.phase == phase => {
                        let dt = elapsed.saturating_sub(prev.elapsed).as_secs_f64();
                        if dt > 0.0 {
                            (
                                Some(bytes_done.saturating_sub(prev.bytes_done) as f64 / dt),
                                Some(written_bytes.saturating_sub(prev.written_bytes) as f64 / dt),
                            )
                        } else {
                            (prev.read_rate, prev.write_rate)
                        }
                    }
                    _ => (None, None),
                };
                self.current = Some(ProgressSnapshot {
                    index,
                    phase,
                    bytes_done,
                    bytes_total,
                    written_bytes,
                    elapsed,
                    read_rate,
                    write_rate,
                });
            }
            WorkerEvent::JobDone { index, result } => {
                self.current = None;
                match result {
                    Ok(report) => {
                        self.done += 1;
                        self.session_files = self.session_files.saturating_add(1);
                        self.session_bytes =
                            self.session_bytes.saturating_add(report.payload_bytes);
                        if let Some(job) = self.jobs.get_mut(index) {
                            job.status = JobStatus::Done(report);
                        }
                    }
                    Err(e) => {
                        self.failed += 1;
                        if let Some(job) = self.jobs.get_mut(index) {
                            job.status = JobStatus::Failed(e);
                        }
                    }
                }
            }
            WorkerEvent::RipDone { cancelled } => {
                if cancelled {
                    // The file in flight was cut short; mark it so the result
                    // screen does not leave a spinner where a write stopped.
                    for job in &mut self.jobs {
                        if matches!(job.status, JobStatus::Running) {
                            job.status = JobStatus::Cancelled;
                        }
                    }
                }
                self.cancelled = cancelled;
                self.stage = Stage::Done;
                // The worker has sent its last event; join so a cancelled
                // `.partial` file is gone before the result screen appears.
                // That also releases the tray lock before an auto-eject.
                self.finish_rip();
                // A cancelled run may still have written some files;
                // `record_backup` records only those that finished.
                self.record_backup();
                if self.auto_eject && self.device.is_some() && !cancelled && self.failed == 0 {
                    self.eject_loaded_disc(false);
                }
            }
            WorkerEvent::EjectDone { result } => {
                self.handle_eject_done(result);
            }
            WorkerEvent::LibraryStats {
                library,
                free_bytes,
            } => {
                self.stats_in_flight = false;
                self.library = library;
                self.free_bytes = free_bytes;
            }
        }
    }

    /// React to a finished tray command, landing wherever the request asked.
    fn handle_eject_done(&mut self, result: Result<(), String>) {
        match result {
            Ok(()) => {
                self.ejected = true;
                self.eject_note = Some("Disc ejected".into());
                if self.eject_then_picker {
                    // The disc is gone: drop the plan and return to the picker.
                    self.clear_disc();
                } else if self.stage == Stage::Detect {
                    self.refresh_drives();
                }
            }
            Err(e) => {
                let note = format!("Could not eject: {e}");
                self.eject_note = Some(note.clone());
                match self.stage {
                    Stage::Plan => self.meta_note = Some(note),
                    Stage::Detect => self.detect_note = Some(note),
                    _ => {}
                }
            }
        }
        self.eject_then_picker = false;
    }

    // -- Rendering --------------------------------------------------------

    fn ui(&self, f: &mut Frame) {
        let area = f.area();
        // Three rows for the kaomoji mouse; short terminals get the ASCII one.
        let header_height = if area.height >= 10 { 3 } else { 1 };
        let rows = Layout::vertical([
            Constraint::Length(header_height),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

        self.render_header(f, rows[0]);
        match self.stage {
            Stage::Config => self.render_config(f, rows[1]),
            Stage::Detect => self.render_detect(f, rows[1]),
            Stage::Plan => self.render_plan(f, rows[1]),
            Stage::Ripping => self.render_ripping(f, rows[1]),
            Stage::Done => self.render_done(f, rows[1]),
        }
        self.render_footer(f, rows[2]);

        if self.show_help {
            self.render_help(f, area);
        }
    }

    /// The kaomoji pose the header mouse holds for the current stage: sniffing
    /// while it looks for a disc, idling with an occasional blink or ear flick
    /// during review, hauling the pack while backing up, pleased with its
    /// cheese when finished, and cross-eyed when something goes wrong.
    fn rat_frame(&self) -> &'static [&'static str; 3] {
        match self.stage {
            Stage::Config => {
                const CYCLE: [&[&str; 3]; 4] = [&RAT_IDLE, &RAT_IDLE, &RAT_BLINK, &RAT_IDLE];
                CYCLE[(self.tick / 3) % CYCLE.len()]
            }
            Stage::Detect => {
                if self.detect_note.is_some() {
                    &RAT_ERROR
                } else if (self.tick / 5) % 2 == 0 {
                    &RAT_SNIFF
                } else {
                    &RAT_IDLE
                }
            }
            Stage::Plan => {
                const CYCLE: [&[&str; 3]; 8] = [
                    &RAT_IDLE, &RAT_IDLE, &RAT_BLINK, &RAT_IDLE, &RAT_IDLE, &RAT_IDLE, &RAT_EAR,
                    &RAT_IDLE,
                ];
                CYCLE[(self.tick / 3) % CYCLE.len()]
            }
            Stage::Ripping => {
                const CYCLE: [&[&str; 3]; 4] = [&RAT_PACK, &RAT_PACK, &RAT_PACK_BLINK, &RAT_PACK];
                CYCLE[(self.tick / 3) % CYCLE.len()]
            }
            Stage::Done => {
                if self.failed > 0 {
                    &RAT_ERROR
                } else {
                    const CYCLE: [&[&str; 3]; 4] =
                        [&RAT_CHEESE, &RAT_CHEESE, &RAT_CHEESE_BLINK, &RAT_CHEESE];
                    CYCLE[(self.tick / 3) % CYCLE.len()]
                }
            }
        }
    }

    /// Rotation-friendly collection lines for the header. Each entry is one
    /// "page"; the caller shows as many as fit and rotates through the rest.
    fn stat_pages(&self) -> Vec<Line<'static>> {
        let mut pages = Vec::new();
        let library = &self.library;
        if !library.is_empty() {
            pages.push(stat_line(
                "Library",
                format!(
                    "{} · {} · {} · {}",
                    count(library.shows, "show", "shows"),
                    count(library.seasons, "season", "seasons"),
                    count(library.episodes, "episode", "episodes"),
                    count(library.movies, "movie", "movies"),
                ),
            ));
        }
        if library.total_bytes() > 0 || self.free_bytes.is_some() {
            let mut parts = Vec::new();
            if library.total_bytes() > 0 {
                parts.push(format!("{} backed up", fmt_bytes(library.total_bytes())));
            }
            if let Some(free) = self.free_bytes {
                parts.push(format!("{} free", fmt_bytes(free)));
            }
            pages.push(stat_line("Storage", parts.join(" · ")));
        }
        if self.history.discs > 0 {
            pages.push(stat_line(
                "History",
                format!(
                    "{} · {} written",
                    count(self.history.discs, "disc", "discs"),
                    fmt_bytes(self.history.bytes),
                ),
            ));
        }
        pages
    }

    /// A rotating window over the stat pages that fits in `rows` lines. With
    /// more pages than rows the window advances every [`STATS_ROTATE_TICKS`].
    fn header_stats(&self, rows: usize) -> Vec<Line<'static>> {
        let pages = self.stat_pages();
        if pages.is_empty() || rows == 0 {
            return Vec::new();
        }
        let start = (self.tick / STATS_ROTATE_TICKS) % pages.len();
        (0..rows.min(pages.len()))
            .map(|i| pages[(start + i) % pages.len()].clone())
            .collect()
    }

    fn render_header(&self, f: &mut Frame, area: Rect) {
        let where_ = match self.stage {
            Stage::Config => "settings",
            Stage::Detect => "finding a disc",
            Stage::Plan => "review",
            Stage::Ripping => "backing up",
            Stage::Done => "finished",
        };
        let badge = Line::from(vec![
            Span::styled(
                " packrat ",
                Style::default().fg(Color::Black).bg(Color::Cyan).bold(),
            ),
            Span::styled(format!("  {where_}"), Style::default().fg(Color::DarkGray)),
        ]);

        if area.height >= 3 {
            let frame = self.rat_frame();
            let rat_width = frame
                .iter()
                .map(|row| Line::from(*row).width())
                .max()
                .unwrap_or(0) as u16;
            if area.width <= rat_width {
                f.render_widget(Paragraph::new(badge), area);
                return;
            }
            let cols =
                Layout::horizontal([Constraint::Min(1), Constraint::Length(rat_width)]).split(area);
            // Top-align the title with the first row of the three-row mouse;
            // the collection summary fills the rows beneath it.
            let mut lines = vec![badge];
            lines.extend(self.header_stats(usize::from(area.height).saturating_sub(1)));
            f.render_widget(Paragraph::new(lines), cols[0]);
            f.render_widget(
                Paragraph::new(rat_lines(frame)).alignment(Alignment::Right),
                cols[1],
            );
        } else {
            // Compact terminals get the ASCII mouse's middle row only.
            let ascii = &RAT_ASCII[(self.tick / 3) % RAT_ASCII.len()];
            let rat_width = ascii[1].len() as u16;
            if area.width <= rat_width {
                f.render_widget(Paragraph::new(badge), area);
                return;
            }
            let cols =
                Layout::horizontal([Constraint::Min(1), Constraint::Length(rat_width)]).split(area);
            f.render_widget(Paragraph::new(badge), cols[0]);
            f.render_widget(
                Paragraph::new(ascii[1])
                    .style(Style::default().fg(Color::Gray))
                    .alignment(Alignment::Right),
                cols[1],
            );
        }
    }

    fn render_footer(&self, f: &mut Frame, area: Rect) {
        let hints: Vec<(&str, &str)> = match self.stage {
            Stage::Config => vec![
                ("Tab", "field"),
                ("Enter", "save & continue"),
                (
                    "Esc",
                    if self.config_onboarding {
                        "skip"
                    } else {
                        "back"
                    },
                ),
            ],
            Stage::Detect => vec![
                ("Tab", "list/path"),
                ("↑↓", "drive"),
                ("Enter", "load"),
                ("r", "rescan"),
                ("x", "eject"),
                ("s", "settings"),
                ("Esc", "quit"),
            ],
            Stage::Plan => vec![
                ("Tab", "field"),
                ("↑↓", "select"),
                ("Space", "toggle"),
                ("e", "extras"),
                ("Enter", "apply"),
                ("r", "rip"),
                ("x", "eject"),
                ("s", "settings"),
                ("d", "change drive"),
                ("?", "help"),
                ("q", "quit"),
            ],
            Stage::Ripping => vec![("q", "stop after this file"), ("a", "abort now")],
            Stage::Done => vec![
                ("c", "configure another"),
                ("x", "eject & configure"),
                ("Enter", "quit"),
            ],
        };
        let mut spans = vec![Span::raw(" ")];
        for (i, (key, label)) in hints.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled("  ·  ", Style::default().fg(Color::DarkGray)));
            }
            spans.push(Span::styled(*key, Style::default().fg(Color::Cyan).bold()));
            spans.push(Span::raw(format!(" {label}")));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    /// The settings screen: a text field per storage directory plus any
    /// toggles. Adding a media type changes [`ConfigField`], not this layout.
    fn render_config(&self, f: &mut Frame, area: Rect) {
        let mut constraints = vec![Constraint::Length(3)];
        constraints.extend(ConfigField::ALL.iter().map(|_| Constraint::Length(3)));
        constraints.push(Constraint::Length(1));
        constraints.push(Constraint::Min(0));
        let rows = Layout::vertical(constraints).split(area);

        let intro = if self.config_onboarding {
            "Welcome! Choose where packrat writes your media (press s to change later)."
        } else {
            "Where packrat writes files, one directory per media type."
        };
        f.render_widget(
            Paragraph::new(intro)
                .block(Block::default().borders(Borders::ALL).title(" Settings "))
                .wrap(Wrap { trim: true }),
            rows[0],
        );

        for (i, &field) in ConfigField::ALL.iter().enumerate() {
            let row = rows[i + 1];
            let focused = self.config_focus == field;
            let border = if focused {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            if field.is_toggle() {
                let check = if self.auto_eject { "[x]" } else { "[ ]" };
                let line = Line::from(vec![
                    Span::styled(format!(" {check} "), Style::default().fg(Color::Cyan)),
                    Span::raw(field.hint()),
                    Span::styled("   Space to toggle", Style::default().fg(Color::DarkGray)),
                ]);
                f.render_widget(
                    Paragraph::new(line).block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(border)
                            .title(field.title()),
                    ),
                    row,
                );
            } else {
                f.render_widget(
                    self.field(
                        field.title(),
                        self.config_input(field),
                        focused,
                        field.hint(),
                    ),
                    row,
                );
                if focused {
                    set_cursor(f, row, self.config_input(field));
                }
            }
        }

        let mut footer = Vec::new();
        if let Some(path) = config::config_path() {
            footer.push(Span::styled(
                " Saved to ",
                Style::default().fg(Color::DarkGray),
            ));
            footer.push(Span::styled(
                path.display().to_string(),
                Style::default().fg(Color::DarkGray),
            ));
        }
        if let Some(note) = &self.config_note {
            footer.push(Span::styled(
                format!("   {note}"),
                Style::default().fg(Color::Red),
            ));
        }
        f.render_widget(
            Paragraph::new(Line::from(footer)).wrap(Wrap { trim: true }),
            rows[ConfigField::ALL.len() + 1],
        );
    }

    fn render_detect(&self, f: &mut Frame, area: Rect) {
        if self.detect_loading {
            let spinner = SPINNER[(self.tick / 2) % SPINNER.len()];
            let text = Paragraph::new(format!("{spinner} Scanning optical drives…")).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Finding a disc "),
            );
            f.render_widget(text, area);
            return;
        }

        let rows = Layout::vertical([Constraint::Length(3), Constraint::Min(3)]).split(area);

        let path_focused = self.detect_focus == DetectField::Path;
        let input = Paragraph::new(self.path_input.as_str()).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(if path_focused {
                    Color::Cyan
                } else {
                    Color::DarkGray
                }))
                .title(" Disc path — Tab between this box and the drive list "),
        );
        f.render_widget(input, rows[0]);
        if path_focused {
            set_cursor(f, rows[0], &self.path_input);
        }

        let items: Vec<ListItem> = self
            .drives
            .iter()
            .map(|drive| {
                let media = if drive.has_disc { "disc" } else { "no disc" };
                let label = drive.label().unwrap_or_default();
                let mount = drive
                    .mount
                    .as_ref()
                    .map(|m| m.display().to_string())
                    .unwrap_or_else(|| "—".into());
                ListItem::new(format!(
                    "{:<14}  {:<8}  {:<24}  {}",
                    drive.device.display(),
                    media,
                    label,
                    mount
                ))
            })
            .collect();

        let drives_focused = self.detect_focus == DetectField::Drives;
        let message = self.detect_note.clone().unwrap_or_else(|| {
            if self.drives.is_empty() {
                "no optical drives — type a path above".into()
            } else if drives_focused {
                "↑↓ choose · Enter load · r rescan".into()
            } else {
                "Enter loads the typed path".into()
            }
        });
        let mut state = ListState::default();
        if drives_focused && !self.drives.is_empty() {
            state.select(Some(self.drive_cursor));
        }
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if drives_focused {
                        Color::Cyan
                    } else {
                        Color::DarkGray
                    }))
                    .title(format!(" Drives ({}) — {} ", self.drives.len(), message)),
            )
            .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan).bold())
            .highlight_symbol("▶ ");
        f.render_stateful_widget(list, rows[1], &mut state);
    }

    fn render_plan(&self, f: &mut Frame, area: Rect) {
        let is_movie = self
            .classification
            .as_ref()
            .is_some_and(|c| c.kind == DiscKind::Movie);
        let show_candidates = self.movie_candidates.len() > 1;

        let mut constraints = vec![
            // Disc, looks-like, parsed and metadata lines inside a bordered block.
            Constraint::Length(6),
            Constraint::Length(3),
        ];
        if show_candidates {
            constraints.push(Constraint::Length(1));
        }
        constraints.push(Constraint::Length(1));
        constraints.push(Constraint::Min(5));
        let rows = Layout::vertical(constraints).split(area);

        self.render_disc_summary(f, rows[0]);

        // A TV disc has Show, Season and First episode; a movie has Title and
        // Year. The first-episode box is where a disc the label mis-placed is
        // corrected.
        let fields = if is_movie {
            Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                .split(rows[1])
        } else {
            Layout::horizontal([
                Constraint::Percentage(42),
                Constraint::Percentage(28),
                Constraint::Percentage(30),
            ])
            .split(rows[1])
        };
        let (first_title, first_hint) = if is_movie {
            (" Title ", "TMDb searches this name")
        } else {
            (" Show ", "TVmaze searches this name")
        };
        f.render_widget(
            self.field(
                first_title,
                &self.show_input,
                self.focus == Field::Show,
                first_hint,
            ),
            fields[0],
        );
        if self.focus == Field::Show {
            set_cursor(f, fields[0], &self.show_input);
        }
        let second_title = if is_movie { " Year " } else { " Season " };
        f.render_widget(
            self.field(
                second_title,
                &self.season_input,
                self.focus == Field::Season,
                "",
            ),
            fields[1],
        );
        if self.focus == Field::Season {
            set_cursor(f, fields[1], &self.season_input);
        }
        if !is_movie {
            f.render_widget(
                self.field(
                    " First episode ",
                    &self.first_episode_input,
                    self.focus == Field::Episode,
                    "e.g. 29",
                ),
                fields[2],
            );
            if self.focus == Field::Episode {
                set_cursor(f, fields[2], &self.first_episode_input);
            }
        }

        let mut row = 2;
        if show_candidates {
            f.render_widget(self.candidate_line(), rows[row]);
            row += 1;
        }

        let checkbox = if self.include_extras { "[x]" } else { "[ ]" };
        let extras_label = if is_movie {
            "Select extras (trailers and featurettes)"
        } else {
            "Include extras (trailers and featurettes)"
        };
        let extras = vec![
            Span::styled(format!(" {checkbox} "), Style::default().fg(Color::Cyan)),
            Span::raw(extras_label),
            Span::styled("   e to toggle", Style::default().fg(Color::DarkGray)),
        ];
        f.render_widget(Paragraph::new(Line::from(extras)), rows[row]);
        row += 1;

        let selected = self.jobs.iter().filter(|j| j.enabled).count();
        let title = format!(" Files — {selected} of {} selected ", self.jobs.len());
        self.render_job_list(f, rows[row], title, Some(self.job_cursor), true);
    }

    /// The TMDb candidate currently highlighted, with a hint for changing it.
    fn candidate_line(&self) -> Paragraph<'static> {
        let index = self.movie_cursor.min(self.movie_candidates.len() - 1);
        let (score, movie) = &self.movie_candidates[index];
        let year = movie.year().map(|y| format!(" ({y})")).unwrap_or_default();
        Paragraph::new(Line::from(vec![
            Span::styled(" Candidate ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}/{}  ", index + 1, self.movie_candidates.len()),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(movie.title.clone()),
            Span::styled(
                format!("{year}  {:.0}%", score * 100.0),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                "   [ ] to change    Enter to re-search",
                Style::default().fg(Color::DarkGray),
            ),
        ]))
    }

    /// A bordered text field, brightening when focused.
    fn field<'a>(
        &self,
        title: &'a str,
        input: &TextInput,
        focused: bool,
        hint: &'a str,
    ) -> Paragraph<'a> {
        let border = if focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let value = input.as_str();
        let line = if value.is_empty() {
            Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray)))
        } else {
            Line::from(value)
        };
        Paragraph::new(line).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border)
                .title(title),
        )
    }

    fn render_disc_summary(&self, f: &mut Frame, area: Rect) {
        let mut lines = Vec::new();
        if let Some(disc) = &self.disc {
            lines.push(Line::from(vec![
                Span::styled(" Disc    ", Style::default().fg(Color::DarkGray)),
                Span::styled(&disc.volume_id, Style::default().bold()),
            ]));
        }
        if let Some(classification) = &self.classification {
            let (label, color) = match classification.kind {
                DiscKind::Movie => ("movie", Color::Green),
                DiscKind::TvSeries => ("TV series", Color::Green),
                DiscKind::Unknown => ("unidentified", Color::Yellow),
            };
            lines.push(Line::from(vec![
                Span::styled(" Looks   ", Style::default().fg(Color::DarkGray)),
                Span::styled(label, Style::default().fg(color)),
                Span::styled(
                    format!("  ({}% sure)", classification.confidence),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
        if let Some(label) = &self.label {
            let is_movie = self
                .classification
                .as_ref()
                .is_some_and(|c| c.kind == DiscKind::Movie);
            let detail = if is_movie {
                label
                    .year
                    .map(|y| format!("  year {y}"))
                    .unwrap_or_default()
            } else {
                format!(
                    "  season {}  disc {}",
                    label
                        .season
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "?".into()),
                    label
                        .disc
                        .map(|d| d.to_string())
                        .unwrap_or_else(|| "?".into()),
                )
            };
            lines.push(Line::from(vec![
                Span::styled(" Parsed  ", Style::default().fg(Color::DarkGray)),
                Span::raw(label.title.clone()),
                Span::styled(detail, Style::default().fg(Color::DarkGray)),
            ]));
        }
        lines.push(self.metadata_line());

        f.render_widget(
            Paragraph::new(lines)
                .block(Block::default().borders(Borders::ALL).title(" Disc "))
                .wrap(Wrap { trim: true }),
            area,
        );
    }

    fn metadata_line(&self) -> Line<'static> {
        let is_movie = self
            .classification
            .as_ref()
            .is_some_and(|c| c.kind == DiscKind::Movie);
        let source = if is_movie { " TMDb   " } else { " TVmaze " };
        let idle = if is_movie {
            "set a TMDb key and movie directory, then press Enter"
        } else {
            "set a library and press Enter to match"
        };
        match self.meta_state {
            MetaState::Loading => {
                let spinner = SPINNER[(self.tick / 2) % SPINNER.len()];
                Line::from(vec![
                    Span::styled(source, Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!("{spinner} looking up…"),
                        Style::default().fg(Color::Yellow),
                    ),
                ])
            }
            MetaState::Ready => {
                let matched = if is_movie {
                    self.meta_movie.as_ref().map(|movie| {
                        (
                            movie.title.clone(),
                            movie.year().map(|y| format!("  ({y})")).unwrap_or_default(),
                        )
                    })
                } else {
                    self.meta_show.as_ref().map(|show| {
                        (
                            show.name.clone(),
                            show.year().map(|y| format!("  ({y})")).unwrap_or_default(),
                        )
                    })
                };
                match matched {
                    Some((name, year)) => Line::from(vec![
                        Span::styled(source, Style::default().fg(Color::DarkGray)),
                        Span::styled(name, Style::default().fg(Color::Green)),
                        Span::styled(year, Style::default().fg(Color::DarkGray)),
                    ]),
                    None => Line::from(vec![
                        Span::styled(source, Style::default().fg(Color::DarkGray)),
                        Span::styled(
                            self.meta_note.clone().unwrap_or_else(|| "no match".into()),
                            Style::default().fg(Color::Yellow),
                        ),
                    ]),
                }
            }
            MetaState::Idle => Line::from(vec![
                Span::styled(source, Style::default().fg(Color::DarkGray)),
                Span::styled(
                    self.meta_note.clone().unwrap_or_else(|| idle.into()),
                    Style::default().fg(Color::DarkGray),
                ),
            ]),
        }
    }

    fn render_ripping(&self, f: &mut Frame, area: Rect) {
        let rows = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(5),
        ])
        .split(area);
        let finished = self.done + self.failed;
        let metrics = self.metrics();
        let ratio = if self.total == 0 {
            0.0
        } else {
            ((finished as f64 + metrics.file_fraction) / self.total as f64).clamp(0.0, 1.0)
        };
        let note = if self.abort.load(Ordering::Relaxed) {
            "  aborting…"
        } else if self.cancel.load(Ordering::Relaxed) {
            "  cancelling…"
        } else {
            ""
        };
        let eta = metrics
            .overall_eta
            .map(|d| format!("  ·  ETA {}", fmt_eta(d)))
            .unwrap_or_default();
        let gauge = Gauge::default()
            .block(Block::default().borders(Borders::ALL).title(" Progress "))
            .gauge_style(Style::default().fg(Color::Green).bg(Color::Black))
            .ratio(ratio)
            .label(format!("{finished}/{} files{eta}{note}", self.total));
        f.render_widget(gauge, rows[0]);
        f.render_widget(Paragraph::new(self.progress_line()), rows[1]);
        // Read-only list, scrolled to the file currently being written.
        let active = self
            .jobs
            .iter()
            .position(|j| matches!(&j.status, JobStatus::Running))
            .or_else(|| {
                self.jobs
                    .iter()
                    .rposition(|j| !matches!(&j.status, JobStatus::Pending))
            });
        self.render_job_list(f, rows[2], " Files ".to_string(), active, false);
    }

    /// Throughput and time remaining for the current file and the whole queue.
    fn metrics(&self) -> Metrics {
        let remaining_queue = remaining_queue_duration(&self.jobs);
        let current_duration = self
            .current
            .as_ref()
            .and_then(|p| self.jobs.get(p.index))
            .map(|job| job.duration)
            .unwrap_or_default();
        compute_metrics(self.current.as_ref(), current_duration, remaining_queue)
    }

    fn progress_line(&self) -> Line<'static> {
        let Some(progress) = &self.current else {
            return Line::from("");
        };
        let name = self
            .jobs
            .get(progress.index)
            .and_then(|job| job.path.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let metrics = self.metrics();
        let (marker, color) = match progress.phase {
            RemuxPhase::Probing => (
                SPINNER[(self.tick / 2) % SPINNER.len()].to_string(),
                Color::Yellow,
            ),
            RemuxPhase::Muxing => ("▶".to_string(), Color::Green),
        };
        // The probe pass writes nothing, so it carries just the read rate and
        // an "analyzing" label; once muxing starts both rates are shown side
        // by side, which makes a slow side obvious without a second verb.
        let rate = |value: Option<f64>| {
            value
                .map(|mib| format!("{mib:.1}"))
                .unwrap_or_else(|| "—".into())
        };
        let read = rate(metrics.read_megabytes_per_sec);
        let write = rate(metrics.write_megabytes_per_sec);
        let eta = metrics
            .file_eta
            .map(|d| format!("ETA {}", fmt_eta(d)))
            .unwrap_or_else(|| "ETA —".into());
        let mut spans = vec![
            Span::styled(format!(" {marker} "), Style::default().fg(color)),
            Span::styled(name, Style::default().fg(Color::Cyan)),
        ];
        if progress.phase == RemuxPhase::Probing {
            spans.push(Span::styled("  analyzing", Style::default().fg(color)));
            spans.push(Span::styled(
                "  read ",
                Style::default().fg(Color::DarkGray),
            ));
            spans.push(Span::styled(read, Style::default().fg(Color::Green)));
            spans.push(Span::styled(" MB/s", Style::default().fg(Color::DarkGray)));
        } else {
            spans.push(Span::styled(
                "  read ",
                Style::default().fg(Color::DarkGray),
            ));
            spans.push(Span::styled(read, Style::default().fg(Color::Green)));
            spans.push(Span::styled(
                " · write ",
                Style::default().fg(Color::DarkGray),
            ));
            spans.push(Span::styled(write, Style::default().fg(Color::Cyan)));
            spans.push(Span::styled(" MB/s", Style::default().fg(Color::DarkGray)));
        }
        spans.push(Span::styled(
            format!("  {eta}"),
            Style::default().fg(Color::DarkGray),
        ));
        Line::from(spans)
    }

    fn render_done(&self, f: &mut Frame, area: Rect) {
        let rows = Layout::vertical([Constraint::Length(6), Constraint::Min(5)]).split(area);

        let (headline, color) = if self.cancelled {
            (
                format!("Cancelled — {} written, {} failed.", self.done, self.failed),
                Color::Yellow,
            )
        } else {
            (
                format!("Done — {} written, {} failed.", self.done, self.failed),
                if self.failed == 0 {
                    Color::Green
                } else {
                    Color::Red
                },
            )
        };
        let mut lines = vec![Line::from(Span::styled(
            headline,
            Style::default().fg(color).bold(),
        ))];
        if let Some(note) = &self.meta_note {
            lines.push(Line::from(Span::styled(
                note.clone(),
                Style::default().fg(Color::Yellow),
            )));
        }
        if let Some(note) = &self.eject_note {
            lines.push(Line::from(Span::styled(
                note.clone(),
                Style::default().fg(if self.ejected {
                    Color::Green
                } else {
                    Color::Red
                }),
            )));
        }
        lines.push(Line::from(Span::styled(
            "Press c to configure another, x to eject, or Enter to quit.",
            Style::default().fg(Color::DarkGray),
        )));
        f.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Result ")),
            rows[0],
        );

        self.render_job_list(f, rows[1], " Files ".to_string(), None, false);
    }

    /// Render the file list. `interactive` adds the selection arrow and
    /// highlight; otherwise the list is read-only (it still scrolls to `select`
    /// so the active file stays on screen).
    fn render_job_list(
        &self,
        f: &mut Frame,
        area: Rect,
        title: String,
        select: Option<usize>,
        interactive: bool,
    ) {
        let items: Vec<ListItem> = self
            .jobs
            .iter()
            .map(|job| ListItem::new(self.job_line(job)))
            .collect();
        let mut list = List::new(items).block(Block::default().borders(Borders::ALL).title(title));
        if interactive {
            list = list
                .highlight_style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("▶ ");
        }
        let mut state = ListState::default();
        state.select(
            select
                .filter(|_| !self.jobs.is_empty())
                .map(|i| i.min(self.jobs.len() - 1)),
        );
        f.render_stateful_widget(list, area, &mut state);
    }

    fn job_line(&self, job: &Job) -> Line<'static> {
        let checkbox = if job.enabled { "[x]" } else { "[ ]" };
        // Show the destination directory dimmed so the full path is visible
        // without drowning out the file name.
        let (dir, name) = match (job.path.parent(), job.path.file_name()) {
            (Some(dir), Some(name)) if !dir.as_os_str().is_empty() => (
                format!("{}/", dir.display()),
                name.to_string_lossy().into_owned(),
            ),
            _ => (String::new(), job.path.display().to_string()),
        };
        let status = match &job.status {
            JobStatus::Pending => Span::raw(""),
            JobStatus::Running => {
                let spinner = SPINNER[(self.tick / 2) % SPINNER.len()];
                Span::styled(
                    format!("{spinner} ripping"),
                    Style::default().fg(Color::Yellow),
                )
            }
            JobStatus::Done(report) => Span::styled(
                format!("✓ {} chapters", report.chapters),
                Style::default().fg(Color::Green),
            ),
            JobStatus::Failed(e) => Span::styled(format!("✗ {e}"), Style::default().fg(Color::Red)),
            JobStatus::Cancelled => Span::styled("⊘ cancelled", Style::default().fg(Color::Yellow)),
        };
        Line::from(vec![
            Span::styled(format!("{checkbox} "), Style::default().fg(Color::Cyan)),
            Span::styled(
                format!("T{:02} ch {:>3}-{:<3}  ", job.title, job.first, job.last),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(dir, Style::default().fg(Color::DarkGray)),
            Span::raw(name),
            Span::raw("  "),
            status,
        ])
    }

    fn render_help(&self, f: &mut Frame, area: Rect) {
        let popup = centered(area, 62, 20);
        f.render_widget(Clear, popup);
        let lines = vec![
            Line::from(Span::styled(
                "packrat — interactive backup",
                Style::default().fg(Color::Cyan).bold(),
            )),
            Line::raw(""),
            Line::from("Tab / Shift+Tab   move between fields or the drive list"),
            Line::from("↑ / ↓             select a drive or file"),
            Line::from("Space             include / skip a file · toggle a setting"),
            Line::from("a                 include / skip all (plan) · abort now (ripping)"),
            Line::from("e                 include extras"),
            Line::from("s                 open settings"),
            Line::from("x                 eject the disc · eject & configure (done)"),
            Line::from("d                 change drive"),
            Line::from("Enter             load a drive · apply the plan · save settings"),
            Line::from("r                 rescan drives · start backing up"),
            Line::from("q / Esc           quit (while ripping: stop after this file)"),
            Line::from("Ctrl+C            stop now and quit"),
            Line::raw(""),
            Line::from(Span::styled(
                "Press any key to close",
                Style::default().fg(Color::DarkGray),
            )),
        ];
        f.render_widget(
            Paragraph::new(lines)
                .block(Block::default().borders(Borders::ALL).title(" Help "))
                .wrap(Wrap { trim: false }),
            popup,
        );
    }
}

// ---------------------------------------------------------------------------
// Background work
// ---------------------------------------------------------------------------

struct RipJob {
    index: usize,
    title: u16,
    first: u16,
    last: u16,
    path: PathBuf,
}

/// Probe drives and, when the choice is unambiguous, load the disc. A
/// remembered drive wins; otherwise a single ready disc loads straight in and
/// several ready discs are left for the picker.
fn detect_and_load(tx: Sender<WorkerEvent>, preferred: Option<PathBuf>) {
    std::thread::spawn(move || {
        let drives = packrat_core::drives::list();
        let ready: Vec<OpticalDrive> = drives
            .iter()
            .filter(|drive| drive.has_disc && drive.mount.is_some())
            .cloned()
            .collect();

        let chosen = preferred
            .as_ref()
            .and_then(|device| ready.iter().find(|drive| &drive.device == device).cloned())
            .or_else(|| (ready.len() == 1).then(|| ready[0].clone()));

        match chosen {
            Some(drive) => load_drive_inner(tx, drive),
            None => {
                let _ = tx.send(WorkerEvent::Drives(drives));
            }
        }
    });
}

/// Load the drive the user picked from the list.
fn load_drive(tx: Sender<WorkerEvent>, device: PathBuf, mount: PathBuf) {
    std::thread::spawn(move || {
        load_drive_inner(
            tx,
            OpticalDrive {
                device,
                mount: Some(mount),
                has_disc: true,
            },
        );
    });
}

/// Refresh the drive list without loading anything.
fn list_drives(tx: Sender<WorkerEvent>) {
    std::thread::spawn(move || {
        let _ = tx.send(WorkerEvent::Drives(packrat_core::drives::list()));
    });
}

/// Summarise the configured destinations for the header, reporting the free
/// space of whichever destination exists. Runs off the UI thread because the
/// walk touches every folder in the library.
fn scan_stats(tx: Sender<WorkerEvent>, tv_dir: Option<PathBuf>, movie_dir: Option<PathBuf>) {
    std::thread::spawn(move || {
        let library = packrat_core::stats::scan(tv_dir.as_deref(), movie_dir.as_deref());
        let probe = tv_dir
            .as_deref()
            .filter(|path| path.is_dir())
            .or_else(|| movie_dir.as_deref().filter(|path| path.is_dir()))
            .map(Path::to_path_buf);
        let free_bytes = probe
            .as_deref()
            .and_then(|path| fs4::available_space(path).ok());
        let _ = tx.send(WorkerEvent::LibraryStats {
            library,
            free_bytes,
        });
    });
}

/// Read a drive's disc and report it back to the UI.
fn load_drive_inner(tx: Sender<WorkerEvent>, drive: OpticalDrive) {
    let Some(mount) = drive.mount.clone() else {
        // Unmounted media: hand the list back so the picker stays usable.
        let _ = tx.send(WorkerEvent::Drives(vec![drive]));
        return;
    };
    let device = Some(drive.device.clone());
    match load_disc(&mount, device.as_deref()) {
        Ok((source, disc, classification, label)) => {
            let _ = tx.send(WorkerEvent::DiscReady {
                source,
                disc,
                classification,
                label,
                device,
            });
        }
        Err(e) => {
            let _ = tx.send(WorkerEvent::DiscError(e));
        }
    }
}

fn load_disc(
    path: &Path,
    device: Option<&Path>,
) -> Result<(DiscSource, DiscModel, Classification, LabelInfo), String> {
    // Only read from a raw device if it can actually be opened. A detected drive
    // whose device node is missing or unreadable still has a usable mount, and
    // falling back to it beats failing the whole disc.
    let usable_device = device.filter(|dev| std::fs::File::open(dev).is_ok());
    let source = match usable_device {
        Some(dev) => DiscSource::discover_device(dev, path),
        None => DiscSource::discover(path),
    }
    .map_err(|e| e.to_string())?;
    let disc = read_disc(&source).map_err(|e| e.to_string())?;
    let classification = classify(&disc);
    let label = parse_label(&disc.volume_id);
    Ok((source, disc, classification, label))
}

/// A cheap identity for a mounted disc, used to ignore background re-scans of
/// the disc that is already loaded — resetting the plan on every poll would
/// otherwise discard the user's edits and selections.
fn disc_fingerprint(source: &DiscSource, disc: &DiscModel) -> String {
    format!(
        "{}|{}|{}|{}",
        source.root().display(),
        disc.provider_id,
        disc.titles.len(),
        disc.total_duration().as_secs()
    )
}

fn spawn_rip(
    tx: Sender<WorkerEvent>,
    cancel: Arc<AtomicBool>,
    abort: Arc<AtomicBool>,
    source: DiscSource,
    disc: DiscModel,
    jobs: Vec<RipJob>,
    device: Option<PathBuf>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        // Hold the tray shut for the whole run so an accidental eject cannot
        // interrupt a read. A failed lock is not fatal.
        let _tray = device.as_deref().map(packrat_core::drives::lock_tray);
        for job in jobs {
            // `cancel` stops between files; `abort` also stops the file that
            // is in progress.
            if cancel.load(Ordering::Relaxed) || abort.load(Ordering::Relaxed) {
                let _ = tx.send(WorkerEvent::RipDone { cancelled: true });
                return;
            }
            let _ = tx.send(WorkerEvent::JobStarted(job.index));

            let outcome = match disc.titles.iter().find(|t| t.number == job.title) {
                Some(title) => {
                    let index = job.index;
                    let progress_tx = tx.clone();
                    let started = Instant::now();
                    let mut last_sent = Instant::now() - Duration::from_millis(500);
                    // Throttle: the core reports every sector, but the UI only
                    // needs a few updates per second.
                    let mut report_progress = |p: RemuxProgress| {
                        if last_sent.elapsed() < Duration::from_millis(200) {
                            return;
                        }
                        last_sent = Instant::now();
                        let _ = progress_tx.send(WorkerEvent::JobProgress {
                            index,
                            phase: p.phase,
                            bytes_done: p.bytes_done,
                            bytes_total: p.bytes_total,
                            written_bytes: p.written_bytes,
                            elapsed: started.elapsed(),
                        });
                    };
                    read_vts(&source, title.vts).and_then(|vts| {
                        remux_chain_with_progress_and_cancel(
                            &source,
                            &vts,
                            title,
                            job.first,
                            job.last,
                            &job.path,
                            &mut report_progress,
                            &abort,
                        )
                    })
                }
                None => Err(DiscError::Remux(format!("disc has no title {}", job.title))),
            };

            // An aborted file is not a failure: the core has already removed
            // its `.partial`, so report the run as cancelled and stop.
            match outcome {
                Err(DiscError::Cancelled) => {
                    let _ = tx.send(WorkerEvent::RipDone { cancelled: true });
                    return;
                }
                outcome => {
                    let _ = tx.send(WorkerEvent::JobDone {
                        index: job.index,
                        result: outcome.map_err(|e| e.to_string()),
                    });
                }
            }

            if abort.load(Ordering::Relaxed) {
                let _ = tx.send(WorkerEvent::RipDone { cancelled: true });
                return;
            }
        }
        let _ = tx.send(WorkerEvent::RipDone { cancelled: false });
    })
}

// ---------------------------------------------------------------------------
// Plan assembly
// ---------------------------------------------------------------------------

/// Turn a disc and its resolved metadata into the list of files to write.
fn assemble_jobs(
    disc: &DiscModel,
    classification: &Classification,
    naming: Option<&Naming>,
    movie_naming: Option<&MovieNaming>,
    movie_dir: Option<&Path>,
    out_dir: &Path,
    include_extras: bool,
) -> Vec<Job> {
    let mut jobs = Vec::new();

    if classification.kind == DiscKind::Movie {
        let label = parse_label(&disc.volume_id);
        if let Some(film) = feature_titles(disc).first().copied() {
            let path = match movie_naming {
                Some(naming) => naming.feature_path(),
                None => match movie_dir {
                    Some(dir) => movie_file_in(dir, &label.title, label.year),
                    None => out_dir.join(format!("{}.mkv", label.title)),
                },
            };
            jobs.push(Job::new(film, 1, film.chapters, path));
        }

        // Every other title is listed so the user can pick; extras start
        // selected only when the extras toggle is on. This is one selection
        // surface, not a reveal-then-choose two-step.
        for title in movie_extras(disc) {
            let description = format!("Title {:02}", title.number);
            let path = match movie_naming {
                Some(naming) => naming.extra_path(&description),
                None => match movie_dir {
                    Some(dir) => movie_extra_file_in(dir, &label.title, label.year, &description),
                    None => out_dir.join(format!(
                        "{} - {}.mkv",
                        display_name(&label.title, label.year),
                        description
                    )),
                },
            };
            let mut job = Job::new(title, 1, title.chapters, path);
            job.enabled = include_extras;
            jobs.push(job);
        }
        return jobs;
    }

    let preferred = preferred_titles(disc);
    for title in disc.titles.iter().filter(|t| preferred.contains(&t.number)) {
        let segments = split_title(title);
        if segments.len() >= 2 {
            for (i, segment) in segments.iter().enumerate() {
                let path = naming
                    .and_then(|n| n.path_for(title.number, i))
                    .unwrap_or_else(|| {
                        out_dir.join(format!("title{:02}-E{:02}.mkv", title.number, i + 1))
                    });
                jobs.push(Job::new(
                    title,
                    segment.start_chapter,
                    segment.end_chapter,
                    path,
                ));
            }
        } else {
            let path = naming
                .and_then(|n| n.whole_path(title.number))
                .unwrap_or_else(|| out_dir.join(format!("title{:02}.mkv", title.number)));
            jobs.push(Job::new(title, 1, title.chapters, path));
        }
    }

    if include_extras {
        for title in disc.titles.iter().filter(|t| is_extra(t)) {
            let description = format!("Title {:02}", title.number);
            let path = naming
                .map(|n| n.extra_path(&description))
                .unwrap_or_else(|| out_dir.join(format!("title{:02}.mkv", title.number)));
            jobs.push(Job::new(title, 1, title.chapters, path));
        }
    }

    jobs
}

fn is_extra(title: &Title) -> bool {
    title
        .duration
        .map(|d| d >= EXTRA_MIN && d < MIN_CONTENT)
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn edit_input(input: &mut TextInput, key: KeyEvent) {
    match key.code {
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => input.insert(c),
        KeyCode::Backspace => input.backspace(),
        KeyCode::Delete => input.delete(),
        KeyCode::Left => input.left(),
        KeyCode::Right => input.right(),
        KeyCode::Home => input.cursor = 0,
        KeyCode::End => input.cursor = input.chars.len(),
        _ => {}
    }
}

/// Place the terminal cursor inside a bordered text field.
fn set_cursor(f: &mut Frame, area: Rect, input: &TextInput) {
    let max_x = area.right().saturating_sub(2);
    let x = (area.x + 1 + input.cursor as u16).min(max_x);
    let y = area.y + 1;
    if y < area.bottom() {
        f.set_cursor_position((x, y));
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// A destination path from a text field, or `None` when blank.
fn path_from(input: &TextInput) -> Option<PathBuf> {
    let raw = input.trimmed();
    if raw.is_empty() {
        None
    } else {
        Some(config::expand_tilde(&raw))
    }
}

/// Render an optional path into a text field.
fn display_path(path: Option<&Path>) -> String {
    path.map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// What the ripping screen derives from the latest progress snapshot.
#[derive(Default)]
struct Metrics {
    /// Fraction of the current file's playback read (0 while probing).
    file_fraction: f64,
    /// Disc read throughput for the current file.
    read_megabytes_per_sec: Option<f64>,
    /// Output write throughput for the current file.
    write_megabytes_per_sec: Option<f64>,
    /// Estimated time left for the current file.
    file_eta: Option<Duration>,
    /// Estimated time left for the whole queue.
    overall_eta: Option<Duration>,
}

/// Turn a progress snapshot into the numbers the UI shows. The current file's
/// read rate and bytes-per-playback-second are used to extrapolate the queue;
/// the instantaneous read and write rates show which side is the bottleneck.
fn compute_metrics(
    current: Option<&ProgressSnapshot>,
    current_duration: Duration,
    remaining_queue: Duration,
) -> Metrics {
    let Some(progress) = current else {
        return Metrics::default();
    };
    let elapsed = progress.elapsed.as_secs_f64();
    let file_fraction = if progress.bytes_total == 0 {
        0.0
    } else {
        (progress.bytes_done as f64 / progress.bytes_total as f64).clamp(0.0, 1.0)
    };

    // The ETA uses the cumulative read rate, which stays stable across both
    // passes; the displayed rates are the latest interval's.
    let (file_eta, overall_eta) = if elapsed > 0.0 && progress.bytes_done > 0 {
        // Bytes read per wall second.
        let rate = progress.bytes_done as f64 / elapsed;
        let remaining = progress.bytes_total.saturating_sub(progress.bytes_done) as f64;
        let file_eta = Duration::from_secs_f64(remaining / rate);
        // Bytes per playback second for this title, spanning both passes,
        // used to size the files that have not started yet.
        let bytes_per_playback_second = if current_duration.is_zero() {
            0.0
        } else {
            progress.bytes_total as f64 / current_duration.as_secs_f64()
        };
        let queue_eta = Duration::from_secs_f64(
            remaining_queue.as_secs_f64() * bytes_per_playback_second / rate,
        );
        (Some(file_eta), Some(file_eta + queue_eta))
    } else {
        (None, None)
    };

    Metrics {
        file_fraction,
        read_megabytes_per_sec: progress.read_rate.map(|rate| rate / 1_048_576.0),
        write_megabytes_per_sec: progress.write_rate.map(|rate| rate / 1_048_576.0),
        file_eta,
        overall_eta,
    }
}

/// Format a duration as `1h 02m`, `3m 05s` or `42s`.
fn fmt_eta(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs >= 3600 {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// One collection stat line, with its label dimmed so it stays in the
/// background next to the stage badge.
fn stat_line(label: &'static str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {label} "), Style::default().fg(Color::DarkGray)),
        Span::raw(value),
    ])
}

/// `1 show`, `2 shows`: the count with the right noun.
fn count(n: u64, singular: &str, plural: &str) -> String {
    format!("{n} {}", if n == 1 { singular } else { plural })
}

/// Human-readable decimal size, e.g. `18.4 GB`.
fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn title(number: u16, chapters: u16, chapter_secs: u64) -> Title {
        Title {
            number,
            vts: 1,
            vts_ttn: number as u8,
            angles: 1,
            chapters,
            duration: Some(Duration::from_secs(chapter_secs * chapters as u64)),
            chapter_durations: vec![Duration::from_secs(chapter_secs); chapters as usize],
        }
    }

    fn disc(titles: Vec<Title>) -> DiscModel {
        DiscModel {
            volume_id: "TEST_DISC".into(),
            provider_id: String::new(),
            vts_count: 1,
            titles,
        }
    }

    fn kind(kind: DiscKind) -> Classification {
        Classification {
            kind,
            confidence: 90,
            reasons: Vec::new(),
        }
    }

    #[test]
    fn movie_plan_names_the_feature() {
        let d = disc(vec![title(1, 10, 600)]); // 100 minutes
        let jobs = assemble_jobs(
            &d,
            &kind(DiscKind::Movie),
            None,
            None,
            Some(Path::new("/lib")),
            Path::new("."),
            false,
        );
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].title, 1);
        assert_eq!(jobs[0].first, 1);
        assert_eq!(jobs[0].last, 10);
        assert!(jobs[0].path.starts_with("/lib"));
        assert_eq!(jobs[0].path.extension().unwrap(), "mkv");
    }

    #[test]
    fn movie_extras_are_listed_and_selected_by_the_toggle() {
        // A 100-minute feature plus a 200-second featurette.
        let d = disc(vec![title(1, 10, 600), title(2, 1, 200)]);

        let off = assemble_jobs(
            &d,
            &kind(DiscKind::Movie),
            None,
            None,
            Some(Path::new("/lib")),
            Path::new("."),
            false,
        );
        // Both are listed, but only the feature starts selected.
        assert_eq!(off.len(), 2);
        assert!(off[0].enabled);
        let extra = off.iter().find(|j| j.title == 2).expect("extra");
        assert!(!extra.enabled);
        assert!(extra.path.starts_with("/lib"));
        assert!(extra.path.to_string_lossy().contains("Other"));

        let on = assemble_jobs(
            &d,
            &kind(DiscKind::Movie),
            None,
            None,
            Some(Path::new("/lib")),
            Path::new("."),
            true,
        );
        assert!(on.iter().find(|j| j.title == 2).expect("extra").enabled);
    }

    #[test]
    fn extras_are_added_only_when_requested() {
        // Three ~22 minute episodes plus a 200 second featurette.
        let d = disc(vec![
            title(1, 1, 22 * 60),
            title(2, 1, 22 * 60),
            title(3, 1, 22 * 60),
            title(4, 1, 200),
        ]);
        let without = assemble_jobs(
            &d,
            &kind(DiscKind::TvSeries),
            None,
            None,
            None,
            Path::new("."),
            false,
        );
        let with = assemble_jobs(
            &d,
            &kind(DiscKind::TvSeries),
            None,
            None,
            None,
            Path::new("."),
            true,
        );
        assert!(!without.is_empty());
        assert_eq!(with.len(), without.len() + 1);
        assert!(with.iter().any(|j| j.title == 4));
        assert!(!without.iter().any(|j| j.title == 4));
    }

    #[test]
    fn metrics_estimate_throughput_and_eta() {
        // 60 MiB of a 120 MiB file read in 30s, with another 120s of playback
        // queued behind it and a 120s current file.
        let snapshot = ProgressSnapshot {
            index: 0,
            phase: RemuxPhase::Muxing,
            bytes_done: 60 * 1024 * 1024,
            bytes_total: 120 * 1024 * 1024,
            written_bytes: 30 * 1024 * 1024,
            elapsed: Duration::from_secs(30),
            read_rate: Some(2.0 * 1_048_576.0),
            write_rate: Some(0.5 * 1_048_576.0),
        };
        let metrics = compute_metrics(
            Some(&snapshot),
            Duration::from_secs(120),
            Duration::from_secs(120),
        );
        assert!((metrics.file_fraction - 0.5).abs() < 0.001);
        // The displayed rates come from the latest interval.
        assert!((metrics.read_megabytes_per_sec.unwrap() - 2.0).abs() < 0.001);
        assert!((metrics.write_megabytes_per_sec.unwrap() - 0.5).abs() < 0.001);
        // 2 MiB/s, 60 MiB left -> 30s for this file.
        assert_eq!(metrics.file_eta.unwrap(), Duration::from_secs(30));
        // 1 MiB of work per playback second, 120s queued at 2 MiB/s -> 60s.
        assert_eq!(metrics.overall_eta.unwrap(), Duration::from_secs(90));
    }

    #[test]
    fn metrics_report_read_and_write_rates_separately() {
        // A fast disc with slow writes: the two rates must not be conflated.
        let snapshot = ProgressSnapshot {
            index: 0,
            phase: RemuxPhase::Muxing,
            bytes_done: 100 * 1024 * 1024,
            bytes_total: 200 * 1024 * 1024,
            written_bytes: 10 * 1024 * 1024,
            elapsed: Duration::from_secs(10),
            read_rate: Some(10.0 * 1_048_576.0),
            write_rate: Some(1.0 * 1_048_576.0),
        };
        let metrics = compute_metrics(Some(&snapshot), Duration::from_secs(120), Duration::ZERO);
        assert!((metrics.read_megabytes_per_sec.unwrap() - 10.0).abs() < 0.001);
        assert!((metrics.write_megabytes_per_sec.unwrap() - 1.0).abs() < 0.001);
    }

    #[test]
    fn progress_rates_are_deltas_between_samples() {
        let mut app = App::initial();
        app.on_worker(WorkerEvent::JobProgress {
            index: 0,
            phase: RemuxPhase::Muxing,
            bytes_done: 1024 * 1024,
            bytes_total: 100 * 1024 * 1024,
            written_bytes: 512 * 1024,
            elapsed: Duration::from_secs(1),
        });
        assert!(
            app.current.as_ref().unwrap().read_rate.is_none(),
            "the first sample has no baseline"
        );

        app.on_worker(WorkerEvent::JobProgress {
            index: 0,
            phase: RemuxPhase::Muxing,
            bytes_done: 3 * 1024 * 1024,
            bytes_total: 100 * 1024 * 1024,
            written_bytes: 1024 * 1024,
            elapsed: Duration::from_secs(2),
        });
        let snapshot = app.current.as_ref().unwrap();
        // 2 MiB read and 0.5 MiB written in one second.
        assert!((snapshot.read_rate.unwrap() - 2.0 * 1_048_576.0).abs() < 1.0);
        assert!((snapshot.write_rate.unwrap() - 0.5 * 1_048_576.0).abs() < 1.0);
    }

    #[test]
    fn ripping_screen_shows_read_and_write_rates() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = App::initial();
        app.stage = Stage::Ripping;
        app.total = 1;
        app.jobs = vec![Job::new(&title(1, 1, 60), 1, 1, PathBuf::from("ep.mkv"))];
        app.current = Some(ProgressSnapshot {
            index: 0,
            phase: RemuxPhase::Muxing,
            bytes_done: 60 * 1024 * 1024,
            bytes_total: 120 * 1024 * 1024,
            written_bytes: 30 * 1024 * 1024,
            elapsed: Duration::from_secs(30),
            read_rate: Some(2.0 * 1_048_576.0),
            write_rate: Some(0.5 * 1_048_576.0),
        });

        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| app.ui(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        // Header (3) + gauge (3) puts the progress line on row 6.
        let row: String = (0..buffer.area.width)
            .map(|x| buffer[(x, 6)].symbol())
            .collect();

        assert!(row.contains("read"), "row: {row:?}");
        assert!(row.contains("2.0"), "row: {row:?}");
        assert!(row.contains("write"), "row: {row:?}");
        assert!(row.contains("0.5"), "row: {row:?}");
        assert!(
            !row.contains("writing"),
            "the write stat needs no extra verb: {row:?}"
        );
    }

    #[test]
    fn ripping_screen_labels_the_probe_pass_without_a_write_rate() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = App::initial();
        app.stage = Stage::Ripping;
        app.total = 1;
        app.jobs = vec![Job::new(&title(1, 1, 60), 1, 1, PathBuf::from("ep.mkv"))];
        app.current = Some(ProgressSnapshot {
            index: 0,
            phase: RemuxPhase::Probing,
            bytes_done: 30 * 1024 * 1024,
            bytes_total: 120 * 1024 * 1024,
            written_bytes: 0,
            elapsed: Duration::from_secs(10),
            read_rate: Some(3.0 * 1_048_576.0),
            write_rate: Some(0.0),
        });

        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| app.ui(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let row: String = (0..buffer.area.width)
            .map(|x| buffer[(x, 6)].symbol())
            .collect();

        assert!(row.contains("analyzing"), "row: {row:?}");
        assert!(row.contains("3.0"), "row: {row:?}");
        assert!(
            !row.contains("write"),
            "no write rate while probing: {row:?}"
        );
    }

    #[test]
    fn metrics_are_empty_before_the_first_byte() {
        let snapshot = ProgressSnapshot {
            index: 0,
            phase: RemuxPhase::Probing,
            bytes_done: 0,
            bytes_total: 120 * 1024 * 1024,
            written_bytes: 0,
            elapsed: Duration::from_secs(5),
            read_rate: None,
            write_rate: None,
        };
        let metrics = compute_metrics(
            Some(&snapshot),
            Duration::from_secs(120),
            Duration::from_secs(60),
        );
        assert_eq!(metrics.file_fraction, 0.0);
        assert!(metrics.read_megabytes_per_sec.is_none());
        assert!(metrics.write_megabytes_per_sec.is_none());
        assert!(metrics.file_eta.is_none());
        assert!(metrics.overall_eta.is_none());
    }

    #[test]
    fn remaining_queue_excludes_unselected_and_finished_jobs() {
        let job = |duration_secs: u64, enabled: bool, status: JobStatus| Job {
            title: 1,
            first: 1,
            last: 1,
            path: PathBuf::new(),
            duration: Duration::from_secs(duration_secs),
            enabled,
            status,
        };
        let jobs = vec![
            job(100, true, JobStatus::Pending),  // selected, still queued
            job(200, false, JobStatus::Pending), // deselected: never queued
            job(300, true, JobStatus::Done(RemuxReport::default())),
            job(400, true, JobStatus::Running), // the current file
            job(50, true, JobStatus::Pending),
        ];
        assert_eq!(remaining_queue_duration(&jobs), Duration::from_secs(150));
    }

    #[test]
    fn done_screen_returns_to_plan() {
        let mut app = App::initial();
        app.stage = Stage::Done;
        app.done = 2;
        app.failed = 1;
        app.jobs = vec![
            Job::new(&title(1, 1, 60), 1, 1, PathBuf::from("a.mkv")),
            Job::new(&title(2, 1, 60), 1, 1, PathBuf::from("b.mkv")),
        ];
        app.jobs[0].status = JobStatus::Done(RemuxReport::default());
        app.jobs[1].enabled = false;

        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));

        assert!(matches!(app.stage, Stage::Plan));
        assert_eq!(app.done, 0);
        assert_eq!(app.failed, 0);
        assert!(!app.should_quit);
        assert!(app
            .jobs
            .iter()
            .all(|job| matches!(job.status, JobStatus::Pending)));
        assert!(!app.jobs[1].enabled, "selections are preserved");
    }

    #[test]
    fn disc_events_are_ignored_during_rip() {
        let mut app = App::initial();
        app.stage = Stage::Ripping;
        app.scan_in_flight = true;

        app.on_worker(WorkerEvent::Drives(Vec::new()));

        assert!(matches!(app.stage, Stage::Ripping));
        assert!(!app.scan_in_flight);
    }

    #[test]
    fn cancel_key_stops_after_the_current_file() {
        let mut app = App::initial();
        app.stage = Stage::Ripping;

        app.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));

        assert!(app.cancel.load(Ordering::Relaxed));
        assert!(!app.abort.load(Ordering::Relaxed), "q is a soft cancel");
        assert!(!app.should_quit);
    }

    #[test]
    fn abort_key_stops_the_current_file_immediately() {
        let mut app = App::initial();
        app.stage = Stage::Ripping;

        app.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));

        assert!(app.abort.load(Ordering::Relaxed));
        assert!(
            !app.cancel.load(Ordering::Relaxed),
            "abort does not need the soft flag"
        );
        assert!(!app.should_quit);
    }

    #[test]
    fn ctrl_c_requests_a_graceful_shutdown() {
        let mut app = App::initial();
        app.stage = Stage::Ripping;

        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert!(app.should_quit);
        assert!(app.cancel.load(Ordering::Relaxed));
        assert!(app.abort.load(Ordering::Relaxed));
    }

    #[test]
    fn shutdown_joins_the_rip_worker() {
        let mut app = App::initial();
        let stopped = Arc::new(AtomicBool::new(false));
        let abort = app.abort.clone();
        let stopped_by_worker = stopped.clone();
        app.rip_handle = Some(std::thread::spawn(move || {
            while !abort.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(1));
            }
            stopped_by_worker.store(true, Ordering::Relaxed);
        }));

        app.request_shutdown();
        app.finish_rip();

        assert!(stopped.load(Ordering::Relaxed), "worker was not joined");
        assert!(app.should_quit);
        assert!(app.rip_handle.is_none());
    }

    #[test]
    fn disc_fingerprint_changes_with_the_disc() {
        let source = DiscSource::Folder {
            root: PathBuf::from("/mnt/dvd"),
            video_ts: PathBuf::from("/mnt/dvd/VIDEO_TS"),
        };
        let mut disc = DiscModel {
            volume_id: "DISC".into(),
            provider_id: "PROVIDER".into(),
            vts_count: 1,
            titles: vec![title(1, 1, 60)],
        };
        let first = disc_fingerprint(&source, &disc);
        disc.titles.push(title(2, 1, 60));
        assert_ne!(first, disc_fingerprint(&source, &disc));
    }

    #[test]
    fn header_shows_the_kaomoji_mouse() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = App::initial();
        let mut terminal = Terminal::new(TestBackend::new(50, 16)).unwrap();
        let header = |app: &App, terminal: &mut Terminal<TestBackend>| {
            terminal.draw(|frame| app.ui(frame)).unwrap();
            let buffer = terminal.backend().buffer();
            let width = buffer.area.width;
            (0..3)
                .map(|y| {
                    (0..width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<String>>()
        };

        // The detect stage sniffs with a twitching nose.
        app.tick = 0;
        let sniff = header(&app, &mut terminal).concat();
        assert!(
            sniff.contains('C') && sniff.contains('・') && sniff.contains('プ'),
            "expected the kaomoji mouse: {sniff:?}"
        );
        assert!(sniff.contains('｡'), "expected the sniffing nose: {sniff:?}");
        assert!(sniff.contains('＼'), "expected the tail/body: {sniff:?}");

        // Half a cycle later the nose is at rest, so the mouse animates.
        app.tick = 5;
        let idle = header(&app, &mut terminal).concat();
        assert!(
            idle.contains('プ') && !idle.contains('｡'),
            "expected the idle nose: {idle:?}"
        );
        assert_ne!(sniff, idle);

        // The mouse carries the tint rather than the default style.
        let buffer = terminal.backend().buffer();
        let tinted = (0..3).any(|y| {
            (0..buffer.area.width).any(|x| buffer[(x, y)].fg == Color::Rgb(200, 195, 205))
        });
        assert!(tinted, "expected the tinted mouse");
    }

    #[test]
    fn rat_reacts_to_the_stage() {
        let mut app = App::initial();

        app.stage = Stage::Detect;
        app.tick = 0;
        assert_eq!(*app.rat_frame(), RAT_SNIFF);
        app.tick = 5;
        assert_eq!(*app.rat_frame(), RAT_IDLE);

        app.stage = Stage::Plan;
        app.tick = 0;
        assert_eq!(*app.rat_frame(), RAT_IDLE);
        app.tick = 6; // blink frame
        assert_eq!(*app.rat_frame(), RAT_BLINK);
        app.tick = 18; // ear-flick frame
        assert_eq!(*app.rat_frame(), RAT_EAR);

        app.stage = Stage::Ripping;
        app.tick = 0;
        assert_eq!(*app.rat_frame(), RAT_PACK);
        app.tick = 6; // blink frame
        assert_eq!(*app.rat_frame(), RAT_PACK_BLINK);

        app.stage = Stage::Done;
        app.tick = 0;
        assert_eq!(*app.rat_frame(), RAT_CHEESE);
        app.tick = 6;
        assert_eq!(*app.rat_frame(), RAT_CHEESE_BLINK);

        // Errors cross the eye rather than changing the nose.
        app.failed = 1;
        assert_eq!(*app.rat_frame(), RAT_ERROR);
        assert!(app.rat_frame()[0].contains('ｘ'));
        assert!(app.rat_frame()[0].contains('プ'));
    }

    #[test]
    fn header_title_is_top_aligned() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = App::initial();
        app.stage = Stage::Plan;
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|frame| app.ui(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let row: String = (0..buffer.area.width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect();

        // The title sits on the very first row, not centred against the mouse.
        assert!(row.contains("packrat"), "row 0: {row:?}");
    }

    #[test]
    fn header_falls_back_to_ascii_on_short_terminals() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = App::initial();
        let mut terminal = Terminal::new(TestBackend::new(50, 8)).unwrap();
        app.tick = 0;
        terminal.draw(|frame| app.ui(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let row: String = (0..buffer.area.width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect();
        assert!(
            row.contains("<o,o)"),
            "expected the compact ASCII mouse: {row:?}"
        );
    }

    fn drive(device: &str, has_disc: bool, mount: Option<&str>) -> OpticalDrive {
        OpticalDrive {
            device: PathBuf::from(device),
            mount: mount.map(PathBuf::from),
            has_disc,
        }
    }

    #[test]
    fn picker_prefers_the_remembered_device() {
        let mut app = App::initial();
        app.last_device = Some(PathBuf::from("/dev/sr1"));

        app.set_drives(vec![
            drive("/dev/sr0", true, Some("/mnt/a")),
            drive("/dev/sr1", true, Some("/mnt/b")),
        ]);

        assert_eq!(app.drive_cursor, 1);
    }

    #[test]
    fn picker_falls_back_to_the_first_ready_disc() {
        let mut app = App::initial();
        // Ignore whatever drive the machine that runs the tests last used.
        app.last_device = None;

        app.set_drives(vec![
            drive("/dev/sr0", false, None),
            drive("/dev/sr1", true, Some("/mnt/b")),
        ]);

        assert_eq!(app.drive_cursor, 1);
    }

    #[test]
    fn picker_keeps_the_highlighted_drive_across_refreshes() {
        let mut app = App::initial();
        app.set_drives(vec![
            drive("/dev/sr0", true, Some("/mnt/a")),
            drive("/dev/sr1", true, Some("/mnt/b")),
        ]);
        app.drive_cursor = 0;

        // A rescan that returns the same drives must not move the cursor.
        app.set_drives(vec![
            drive("/dev/sr0", true, Some("/mnt/a")),
            drive("/dev/sr1", true, Some("/mnt/b")),
        ]);

        assert_eq!(app.drive_cursor, 0);
    }

    #[test]
    fn picker_keys_move_and_switch_focus() {
        let mut app = App::initial();
        app.set_drives(vec![
            drive("/dev/sr0", true, Some("/mnt/a")),
            drive("/dev/sr1", true, Some("/mnt/b")),
        ]);
        app.drive_cursor = 0;
        app.detect_focus = DetectField::Drives;

        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.drive_cursor, 1);
        app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.drive_cursor, 0);

        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.detect_focus, DetectField::Path);
    }

    #[test]
    fn change_drive_returns_to_the_picker() {
        let mut app = App::initial();
        app.stage = Stage::Plan;
        app.disc = Some(disc(vec![title(1, 1, 60)]));

        app.change_drive();

        assert_eq!(app.stage, Stage::Detect);
        assert_eq!(app.detect_focus, DetectField::Drives);
    }

    #[test]
    fn ejecting_the_disc_clears_the_plan() {
        let mut app = App::initial();
        app.stage = Stage::Plan;
        app.disc = Some(disc(vec![title(1, 1, 60)]));
        app.device = Some(PathBuf::from("/dev/sr0"));
        app.loaded = Some("fingerprint".into());
        app.jobs = vec![Job::new(&title(1, 1, 60), 1, 1, PathBuf::from("ep.mkv"))];

        // The drive is still attached but no longer holds a disc.
        app.on_worker(WorkerEvent::Drives(vec![drive("/dev/sr0", false, None)]));

        assert_eq!(app.stage, Stage::Detect);
        assert!(app.disc.is_none());
        assert!(app.jobs.is_empty());
        assert!(app.detect_note.is_some(), "the user is told why it cleared");
    }

    #[test]
    fn a_still_loaded_disc_keeps_the_plan() {
        let mut app = App::initial();
        app.stage = Stage::Plan;
        app.disc = Some(disc(vec![title(1, 1, 60)]));
        app.device = Some(PathBuf::from("/dev/sr0"));

        app.on_worker(WorkerEvent::Drives(vec![drive(
            "/dev/sr0",
            true,
            Some("/mnt/a"),
        )]));

        assert_eq!(app.stage, Stage::Plan);
        assert!(app.disc.is_some());
    }

    #[test]
    fn a_manual_path_disc_survives_a_drive_rescan() {
        let mut app = App::initial();
        app.stage = Stage::Plan;
        app.disc = Some(disc(vec![title(1, 1, 60)]));
        app.device = None;

        // No drive is associated with a path-typed disc, so a scan that finds
        // none must not clear it.
        app.on_worker(WorkerEvent::Drives(vec![drive("/dev/sr0", false, None)]));

        assert_eq!(app.stage, Stage::Plan);
        assert!(app.disc.is_some());
    }

    #[test]
    fn detect_screen_renders_the_drive_picker() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = App::initial();
        app.detect_focus = DetectField::Drives;
        app.set_drives(vec![
            drive("/dev/sr0", true, Some("/run/media/user/DISC_A")),
            drive("/dev/sr1", false, None),
        ]);
        app.drive_cursor = 0;

        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| app.ui(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| (x, y)))
            .map(|(x, y)| buffer[(x, y)].symbol())
            .collect();

        assert!(screen.contains("/dev/sr0"), "{screen}");
        assert!(screen.contains("DISC_A"), "{screen}");
        assert!(screen.contains("no disc"), "{screen}");
        assert!(
            screen.contains("▶"),
            "the cursor highlights a row: {screen}"
        );
    }

    #[test]
    fn rat_feet_are_centred_under_the_body() {
        /// Display width of the part of `row` before its first `glyph`.
        fn width_before(row: &str, glyph: char) -> usize {
            let byte = row.find(glyph).expect("glyph is in the frame");
            Line::from(row[..byte].to_string()).width()
        }

        for frame in [
            &RAT_IDLE,
            &RAT_BLINK,
            &RAT_EAR,
            &RAT_SNIFF,
            &RAT_PACK,
            &RAT_PACK_BLINK,
            &RAT_CHEESE,
            &RAT_CHEESE_BLINK,
            &RAT_ERROR,
        ] {
            let (body, feet) = (frame[1], frame[2]);
            // The torso runs from its opening `(` through the closing `）`; its
            // width is odd, so the even-width feet can only centre to within
            // half a cell.
            let body_start = width_before(body, '(');
            let body_end = width_before(body, '）') + Line::from("）".to_string()).width();
            let feet_start = width_before(feet, '｀');
            let feet_end = feet_start + Line::from("｀｀".to_string()).width();

            let body_centre = (body_start + body_end) as f32 / 2.0;
            let feet_centre = (feet_start + feet_end) as f32 / 2.0;
            assert!(
                (body_centre - feet_centre).abs() <= 1.0,
                "the feet at {feet_start}..{feet_end} should sit under the body at \
                 {body_start}..{body_end}: {frame:?}"
            );
        }
    }

    #[test]
    fn rat_poses_share_a_width_within_each_stage() {
        // Every pose a stage can cycle through must render at the same width,
        // or the whole mouse would jump sideways as it animates.
        let frame_width = |frame: &[&str; 3]| {
            frame
                .iter()
                .map(|row| Line::from(*row).width())
                .max()
                .unwrap_or(0)
        };

        // Detect cycles the sniff and idle poses; errors can interrupt either.
        assert_eq!(frame_width(&RAT_SNIFF), frame_width(&RAT_IDLE));
        assert_eq!(frame_width(&RAT_IDLE), frame_width(&RAT_ERROR));
        // Plan blinks and flicks an ear.
        assert_eq!(frame_width(&RAT_BLINK), frame_width(&RAT_IDLE));
        assert_eq!(frame_width(&RAT_EAR), frame_width(&RAT_IDLE));
        // Ripping hauls the pack; Done holds the cheese.
        assert_eq!(frame_width(&RAT_PACK_BLINK), frame_width(&RAT_PACK));
        assert_eq!(frame_width(&RAT_CHEESE_BLINK), frame_width(&RAT_CHEESE));
    }

    /// Render the whole UI and flatten it into one string.
    fn screen(app: &mut App, width: u16, height: u16) -> String {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.ui(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| (x, y)))
            .map(|(x, y)| buffer[(x, y)].symbol())
            .collect()
    }

    #[test]
    fn config_fields_tab_in_a_cycle() {
        assert_eq!(ConfigField::TvDir.next(), ConfigField::MovieDir);
        assert_eq!(ConfigField::MovieDir.next(), ConfigField::TmdbKey);
        assert_eq!(ConfigField::TmdbKey.next(), ConfigField::AutoEject);
        assert_eq!(ConfigField::AutoEject.next(), ConfigField::TvDir);
        assert_eq!(ConfigField::TvDir.prev(), ConfigField::AutoEject);
        assert_eq!(ConfigField::AutoEject.prev(), ConfigField::TmdbKey);
        assert_eq!(ConfigField::TmdbKey.prev(), ConfigField::MovieDir);
        assert_eq!(ConfigField::MovieDir.prev(), ConfigField::TvDir);
    }

    #[test]
    fn onboarding_opens_the_config_screen_before_scanning() {
        let mut app = App::initial();
        app.start_onboarding();

        assert_eq!(app.stage, Stage::Config);
        assert!(app.config_onboarding);
        assert_eq!(app.config_return, Stage::Detect);
        assert!(
            !app.scan_in_flight,
            "onboarding should not start a drive scan"
        );
    }

    #[test]
    fn open_config_remembers_where_to_return() {
        let mut app = App::initial();
        app.stage = Stage::Plan;
        app.open_config();

        assert_eq!(app.stage, Stage::Config);
        assert_eq!(app.config_return, Stage::Plan);
        assert!(!app.config_onboarding);
    }

    #[test]
    fn closing_settings_returns_to_the_plan() {
        let mut app = App::initial();
        app.stage = Stage::Plan;
        app.open_config();

        app.close_config();

        assert_eq!(app.stage, Stage::Plan);
    }

    #[test]
    fn escaping_settings_discards_unsaved_edits() {
        let saved = display_path(Config::load().tv_dir.as_deref());
        let mut app = App::initial();
        app.stage = Stage::Plan;
        app.open_config();
        app.tv_input = TextInput::from("/tmp/unsaved");

        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert_eq!(app.stage, Stage::Plan);
        assert_eq!(app.tv_input.as_str(), saved);
    }

    #[test]
    fn settings_key_opens_the_config_screen_from_the_plan() {
        let mut app = App::initial();
        app.stage = Stage::Plan;
        app.focus = Field::Jobs;

        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));

        assert_eq!(app.stage, Stage::Config);
        assert_eq!(app.config_return, Stage::Plan);
    }

    #[test]
    fn s_types_into_the_show_field_instead_of_opening_settings() {
        let mut app = App::initial();
        app.stage = Stage::Plan;
        app.focus = Field::Show;

        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));

        assert_eq!(app.stage, Stage::Plan);
        assert_eq!(app.show_input.as_str(), "s");
    }

    #[test]
    fn config_screen_renders_each_directory_field() {
        let mut app = App::initial();
        app.start_onboarding();
        app.tv_input = TextInput::from("/mnt/media/tv");
        app.movie_input = TextInput::from("/mnt/media/movies");

        let screen = screen(&mut app, 100, 20);

        assert!(screen.contains("Settings"), "{screen}");
        assert!(screen.contains("TV directory"), "{screen}");
        assert!(screen.contains("Movie directory"), "{screen}");
        assert!(screen.contains("/mnt/media/tv"), "{screen}");
        assert!(screen.contains("/mnt/media/movies"), "{screen}");
    }

    #[test]
    fn config_screen_renders_the_auto_eject_toggle() {
        let mut app = App::initial();
        app.start_onboarding();
        app.config_focus = ConfigField::AutoEject;
        app.auto_eject = true;

        let screen = screen(&mut app, 100, 24);

        assert!(screen.contains("Eject when done"), "{screen}");
        assert!(screen.contains("[x]"), "{screen}");
        assert!(screen.contains("once a rip finishes"), "{screen}");
    }

    #[test]
    fn space_toggles_auto_eject_on_the_config_screen() {
        let mut app = App::initial();
        app.start_onboarding();
        app.config_focus = ConfigField::AutoEject;
        let before = app.auto_eject;

        app.on_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));

        assert_eq!(app.auto_eject, !before);
    }

    #[test]
    fn x_ejects_from_the_plan_without_a_drive() {
        let mut app = App::initial();
        app.stage = Stage::Plan;
        app.focus = Field::Jobs;
        app.device = None;
        app.meta_note = None;

        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));

        assert!(
            app.meta_note.is_some(),
            "the user is told there is no drive"
        );
    }

    #[test]
    fn x_types_into_the_show_field_instead_of_ejecting() {
        let mut app = App::initial();
        app.stage = Stage::Plan;
        app.focus = Field::Show;

        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));

        assert_eq!(app.stage, Stage::Plan);
        assert_eq!(app.show_input.as_str(), "x");
    }

    #[test]
    fn successful_eject_returns_to_the_picker_when_asked() {
        let mut app = App::initial();
        app.stage = Stage::Plan;
        app.disc = Some(disc(vec![title(1, 1, 60)]));
        app.device = Some(PathBuf::from("/dev/sr0"));
        app.eject_then_picker = true;

        app.handle_eject_done(Ok(()));

        assert_eq!(app.stage, Stage::Detect);
        assert!(app.disc.is_none());
    }

    #[test]
    fn successful_auto_eject_stays_on_the_result_screen() {
        let mut app = App::initial();
        app.stage = Stage::Done;
        app.disc = Some(disc(vec![title(1, 1, 60)]));
        app.device = Some(PathBuf::from("/dev/sr0"));
        app.eject_then_picker = false;

        app.handle_eject_done(Ok(()));

        assert!(app.ejected);
        assert_eq!(app.stage, Stage::Done, "auto-eject keeps the results up");
        assert_eq!(app.eject_note.as_deref(), Some("Disc ejected"));
    }

    #[test]
    fn ejected_result_screen_falls_back_to_the_picker() {
        let mut app = App::initial();
        app.stage = Stage::Done;
        app.disc = Some(disc(vec![title(1, 1, 60)]));
        app.ejected = true;

        app.back_to_plan();

        assert_eq!(app.stage, Stage::Detect);
        assert!(!app.ejected);
    }

    #[test]
    fn failed_eject_keeps_the_screen_and_reports_it() {
        let mut app = App::initial();
        app.stage = Stage::Plan;
        app.eject_then_picker = true;

        app.handle_eject_done(Err("no disc".into()));

        assert_eq!(app.stage, Stage::Plan);
        assert!(!app.ejected);
        assert!(app.eject_note.as_deref().unwrap().contains("no disc"));
        assert!(app.meta_note.as_deref().unwrap().contains("no disc"));
    }

    #[test]
    fn plan_screen_no_longer_shows_directory_fields() {
        let mut app = App::initial();
        app.stage = Stage::Plan;
        app.disc = Some(disc(vec![title(1, 1, 60)]));
        app.classification = Some(kind(DiscKind::TvSeries));

        let screen = screen(&mut app, 100, 20);

        assert!(screen.contains("Show"), "{screen}");
        assert!(!screen.contains("TV directory"), "{screen}");
        assert!(!screen.contains("Movie directory"), "{screen}");
    }

    #[test]
    fn tv_plan_tabs_through_show_season_and_episode() {
        assert_eq!(Field::Show.next(false), Field::Season);
        assert_eq!(Field::Season.next(false), Field::Episode);
        assert_eq!(Field::Episode.next(false), Field::Jobs);
        assert_eq!(Field::Jobs.next(false), Field::Show);
        assert_eq!(Field::Show.prev(false), Field::Jobs);
        assert_eq!(Field::Jobs.prev(false), Field::Episode);
        assert_eq!(Field::Episode.prev(false), Field::Season);
    }

    #[test]
    fn movie_plan_skips_the_episode_field() {
        assert_eq!(Field::Season.next(true), Field::Jobs);
        assert_eq!(Field::Jobs.prev(true), Field::Season);
    }

    #[test]
    fn first_episode_types_into_its_field() {
        let mut app = App::initial();
        app.stage = Stage::Plan;
        app.classification = Some(kind(DiscKind::TvSeries));
        app.focus = Field::Episode;

        app.on_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Char('9'), KeyModifiers::NONE));

        assert_eq!(app.first_episode_input.as_str(), "29");
    }

    #[test]
    fn plan_screen_shows_the_first_episode_field_for_tv() {
        let mut app = App::initial();
        app.stage = Stage::Plan;
        app.disc = Some(disc(vec![title(1, 1, 60)]));
        app.classification = Some(kind(DiscKind::TvSeries));

        let screen = screen(&mut app, 120, 20);

        assert!(screen.contains("First episode"), "{screen}");
        assert!(screen.contains("e.g. 29"), "{screen}");
    }

    /// Flatten a stat page into plain text for assertions.
    fn lines_text(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn formats_collection_sizes() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(999), "999 B");
        assert_eq!(fmt_bytes(1_000), "1.0 KB");
        assert_eq!(fmt_bytes(1_500_000), "1.5 MB");
        assert_eq!(fmt_bytes(18_400_000_000), "18.4 GB");
        assert_eq!(fmt_bytes(1_200_000_000_000), "1.2 TB");
    }

    #[test]
    fn counts_use_the_right_noun() {
        assert_eq!(count(1, "show", "shows"), "1 show");
        assert_eq!(count(3, "show", "shows"), "3 shows");
    }

    #[test]
    fn header_stats_are_quiet_until_there_is_a_library() {
        let mut app = App::initial();
        app.history = History::default();
        assert!(app.stat_pages().is_empty());
        assert!(app.header_stats(2).is_empty());
    }

    #[test]
    fn header_stats_rotate_through_the_pages() {
        let mut app = App::initial();
        app.library = LibraryStats {
            shows: 1,
            seasons: 1,
            episodes: 28,
            movies: 1,
            tv_bytes: 17_000_000_000,
            movie_bytes: 1_400_000_000,
            newest: None,
        };
        app.free_bytes = Some(47_000_000_000);
        app.history = History {
            discs: 3,
            files: 41,
            bytes: 18_000_000_000,
            last_backup: Some(1),
        };

        assert_eq!(app.stat_pages().len(), 3);

        // The window advances one page per rotation interval.
        app.tick = 0;
        let first = lines_text(&app.header_stats(2));
        app.tick = STATS_ROTATE_TICKS;
        let second = lines_text(&app.header_stats(2));
        assert_ne!(first, second, "the header window should advance");
        assert!(first.contains("Library"), "{first}");
        assert!(first.contains("Storage"), "{first}");
    }

    #[test]
    fn plan_header_renders_a_collection_stat() {
        let mut app = App::initial();
        app.stage = Stage::Plan;
        app.disc = Some(disc(vec![title(1, 1, 60)]));
        app.classification = Some(kind(DiscKind::TvSeries));
        app.library = LibraryStats {
            shows: 1,
            seasons: 1,
            episodes: 28,
            movies: 1,
            ..LibraryStats::default()
        };
        app.history = History::default();

        let screen = screen(&mut app, 140, 20);

        assert!(screen.contains("1 show"), "{screen}");
    }

    #[test]
    fn job_done_tallies_the_rip_for_history() {
        let mut app = App::initial();
        let d = disc(vec![title(1, 5, 60)]);
        app.jobs = vec![Job::new(&d.titles[0], 1, 5, PathBuf::from("/tmp/x.mkv"))];

        app.on_worker(WorkerEvent::JobDone {
            index: 0,
            result: Ok(RemuxReport {
                packets: 10,
                payload_bytes: 1_234,
                chapters: 5,
            }),
        });

        assert_eq!(app.done, 1);
        assert_eq!(app.session_files, 1);
        assert_eq!(app.session_bytes, 1_234);
    }
}

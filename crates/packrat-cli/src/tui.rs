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
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use packrat_core::{
    classify, movie_file_in, parse_label, preferred_titles, read_disc, read_vts,
    remux_chain_with_progress, split_title, Classification, DiscKind, DiscModel, DiscSource,
    LabelInfo, OpticalDrive, RemuxPhase, RemuxProgress, RemuxReport, Show, Title, EXTRA_MIN,
    MIN_CONTENT,
};

use crate::config::{self, Config};
use crate::{resolve_naming, Naming};

/// Spinner frames used while a background task runs.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Launch the interactive flow. Returns once the user quits.
pub fn run() -> Result<()> {
    // If we panic while the terminal is in raw mode, put it back first.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratatui::restore();
        original_hook(info);
    }));

    let mut terminal = ratatui::init();
    let mut app = App::new();
    let result = app.event_loop(&mut terminal);
    ratatui::restore();
    result
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Detect,
    Plan,
    Ripping,
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Show,
    Season,
    TvDir,
    MovieDir,
    Jobs,
}

impl Field {
    fn next(self) -> Self {
        match self {
            Self::Show => Self::Season,
            Self::Season => Self::TvDir,
            Self::TvDir => Self::MovieDir,
            Self::MovieDir => Self::Jobs,
            Self::Jobs => Self::Show,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::Show => Self::Jobs,
            Self::Season => Self::Show,
            Self::TvDir => Self::Season,
            Self::MovieDir => Self::TvDir,
            Self::Jobs => Self::MovieDir,
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
    NoDisc(Vec<OpticalDrive>),
    MetaReady {
        show: Option<Show>,
        naming: Option<Naming>,
        note: Option<String>,
    },
    MetaError(String),
    JobStarted(usize),
    JobProgress {
        index: usize,
        phase: RemuxPhase,
        bytes_done: u64,
        bytes_total: u64,
        elapsed: Duration,
    },
    JobDone {
        index: usize,
        result: Result<RemuxReport, String>,
    },
    RipDone {
        cancelled: bool,
    },
}

/// The latest progress from the file currently being ripped.
struct ProgressSnapshot {
    index: usize,
    phase: RemuxPhase,
    bytes_done: u64,
    bytes_total: u64,
    elapsed: Duration,
}

struct App {
    stage: Stage,
    tx: Sender<WorkerEvent>,
    rx: Receiver<WorkerEvent>,
    cancel: Arc<AtomicBool>,
    tick: usize,

    // Disc detection
    detect_loading: bool,
    /// A detection thread is in flight; guards against overlapping scans.
    scan_in_flight: bool,
    drives: Vec<OpticalDrive>,
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

    // Editable plan
    show_input: TextInput,
    season_input: TextInput,
    tv_input: TextInput,
    movie_input: TextInput,
    config_note: Option<String>,
    include_extras: bool,
    naming: Option<Naming>,
    meta_show: Option<Show>,
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

    show_help: bool,
    should_quit: bool,
}

impl App {
    fn new() -> Self {
        let mut app = Self::initial();
        app.begin_detection(true);
        app
    }

    /// Build the initial state without starting a drive probe (tests use this
    /// directly; `new` follows it with a scan).
    fn initial() -> Self {
        let (tx, rx) = mpsc::channel();
        let config = Config::load();
        let tv_input = TextInput::from(&display_path(config.tv_dir.as_deref()));
        let movie_input = TextInput::from(&display_path(config.movie_dir.as_deref()));
        Self {
            stage: Stage::Detect,
            tx,
            rx,
            cancel: Arc::new(AtomicBool::new(false)),
            tick: 0,
            detect_loading: false,
            scan_in_flight: false,
            drives: Vec::new(),
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
            tv_input,
            movie_input,
            config_note: None,
            include_extras: false,
            naming: None,
            meta_show: None,
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
            show_help: false,
            should_quit: false,
        }
    }

    /// Start a drive probe on a worker thread. `show_spinner` draws the
    /// scanning state for the initial probe and manual path loads; background
    /// re-scans leave the current screen visible.
    fn begin_detection(&mut self, show_spinner: bool) {
        self.detect_loading = show_spinner;
        self.scan_in_flight = true;
        detect_and_load(self.tx.clone());
    }

    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.should_quit {
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
        Ok(())
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
        self.begin_detection(false);
    }

    // -- Input ------------------------------------------------------------

    fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        if self.show_help {
            self.show_help = false;
            return;
        }
        if key.code == KeyCode::Char('?') && self.stage != Stage::Detect {
            self.show_help = true;
            return;
        }

        match self.stage {
            Stage::Detect => self.on_key_detect(key),
            Stage::Plan => self.on_key_plan(key),
            Stage::Ripping => {
                if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                    self.cancel.store(true, Ordering::Relaxed);
                }
            }
            Stage::Done => match key.code {
                KeyCode::Char('c') => self.back_to_plan(),
                KeyCode::Enter | KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char(' ') => {
                    self.should_quit = true;
                }
                _ => {}
            },
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
            KeyCode::Enter => self.load_manual_path(),
            KeyCode::Esc => self.should_quit = true,
            _ => {
                edit_input(&mut self.path_input, key);
            }
        }
    }

    fn on_key_plan(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Tab => self.focus = self.focus.next(),
            KeyCode::BackTab => self.focus = self.focus.prev(),
            KeyCode::Esc => {
                if self.focus == Field::Jobs {
                    self.should_quit = true;
                } else {
                    self.focus = Field::Jobs;
                }
            }
            _ => match self.focus {
                Field::Show | Field::Season | Field::TvDir | Field::MovieDir => {
                    if key.code == KeyCode::Enter {
                        self.apply_edits();
                    } else {
                        let input = match self.focus {
                            Field::Show => &mut self.show_input,
                            Field::Season => &mut self.season_input,
                            Field::TvDir => &mut self.tv_input,
                            _ => &mut self.movie_input,
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
                    KeyCode::Char('e') => {
                        self.include_extras = !self.include_extras;
                        self.rebuild_jobs();
                    }
                    KeyCode::Char('r') => self.start_rip(),
                    KeyCode::Char('q') => self.should_quit = true,
                    _ => {}
                },
            },
        }
    }

    // -- Actions ----------------------------------------------------------

    fn tv_path(&self) -> Option<PathBuf> {
        path_from(&self.tv_input)
    }

    fn movie_path(&self) -> Option<PathBuf> {
        path_from(&self.movie_input)
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

    /// Recompute the metadata and job list after the user edits a field, and
    /// remember the destination preferences.
    fn apply_edits(&mut self) {
        self.meta_note = None;
        self.save_preferences();
        let Some(disc) = self.disc.clone() else {
            return;
        };
        let show = self.show_input.trimmed();
        let show = if show.is_empty() { None } else { Some(show) };
        let season = self.season_input.trimmed().parse::<u16>().ok();

        // Movies are named from the disc label, so no metadata lookup is needed.
        let is_movie = self
            .classification
            .as_ref()
            .map(|c| c.kind == DiscKind::Movie)
            .unwrap_or(false);
        if is_movie {
            self.naming = None;
            self.meta_show = None;
            self.meta_state = MetaState::Idle;
            self.rebuild_jobs();
            return;
        }

        match self.tv_path() {
            Some(tv_dir) => {
                self.meta_state = MetaState::Loading;
                let tx = self.tx.clone();
                let preferred = preferred_titles(&disc);
                std::thread::spawn(move || {
                    match resolve_naming(&tv_dir, &disc, &preferred, show.as_deref(), season) {
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

    /// Persist the TV and movie destinations, reporting the result.
    fn save_preferences(&mut self) {
        let config = Config {
            tv_dir: self.tv_path(),
            movie_dir: self.movie_path(),
        };
        match config.save() {
            Ok(true) => self.config_note = Some("Preferences saved".into()),
            Ok(false) => {}
            Err(e) => self.config_note = Some(format!("Could not save preferences: {e:#}")),
        }
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
            movie_dir.as_deref(),
            &out_dir,
            self.include_extras,
        );
        self.set_jobs(jobs);
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

        for job in &mut self.jobs {
            job.status = JobStatus::Pending;
        }
        self.total = selected.len();
        self.done = 0;
        self.failed = 0;
        self.cancelled = false;
        self.current = None;
        self.stage = Stage::Ripping;
        self.cancel.store(false, Ordering::Relaxed);

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
        spawn_rip(self.tx.clone(), self.cancel.clone(), source, disc, jobs);
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
                WorkerEvent::DiscReady { .. }
                    | WorkerEvent::NoDisc(_)
                    | WorkerEvent::DiscError(_)
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
                self.show_input = TextInput::from(&label.title);
                self.season_input = TextInput::from(
                    &label
                        .season
                        .map(|s| s.to_string())
                        .unwrap_or_else(String::new),
                );
                self.source = Some(source);
                self.disc = Some(disc);
                self.classification = Some(classification);
                self.label = Some(label);
                self.device = device;
                self.focus = Field::Jobs;
                self.stage = Stage::Plan;
                self.rebuild_jobs();

                // With a TV destination already configured, look metadata up
                // straight away so episode titles fill in without pressing
                // Enter first.
                let is_tv = self
                    .classification
                    .as_ref()
                    .is_some_and(|c| c.kind != DiscKind::Movie);
                if is_tv && self.tv_path().is_some() {
                    self.apply_edits();
                }
            }
            WorkerEvent::NoDisc(drives) => {
                self.detect_loading = false;
                self.scan_in_flight = false;
                self.drives = drives;
                self.detect_note = None;
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
                elapsed,
            } => {
                self.current = Some(ProgressSnapshot {
                    index,
                    phase,
                    bytes_done,
                    bytes_total,
                    elapsed,
                });
            }
            WorkerEvent::JobDone { index, result } => {
                self.current = None;
                match result {
                    Ok(report) => {
                        self.done += 1;
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
                self.cancelled = cancelled;
                self.stage = Stage::Done;
            }
        }
    }

    // -- Rendering --------------------------------------------------------

    fn ui(&self, f: &mut Frame) {
        let area = f.area();
        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

        self.render_header(f, rows[0]);
        match self.stage {
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

    fn render_header(&self, f: &mut Frame, area: Rect) {
        let where_ = match self.stage {
            Stage::Detect => "finding a disc",
            Stage::Plan => "review",
            Stage::Ripping => "backing up",
            Stage::Done => "finished",
        };
        let line = Line::from(vec![
            Span::styled(
                " packrat ",
                Style::default().fg(Color::Black).bg(Color::Cyan).bold(),
            ),
            Span::styled(format!("  {where_}"), Style::default().fg(Color::DarkGray)),
        ]);
        f.render_widget(Paragraph::new(line), area);
    }

    fn render_footer(&self, f: &mut Frame, area: Rect) {
        let hints: &[(&str, &str)] = match self.stage {
            Stage::Detect => &[("Enter", "load path"), ("Esc", "quit")],
            Stage::Plan => &[
                ("Tab", "field"),
                ("↑↓", "select"),
                ("Space", "toggle"),
                ("e", "extras"),
                ("Enter", "apply"),
                ("r", "rip"),
                ("?", "help"),
                ("q", "quit"),
            ],
            Stage::Ripping => &[("q", "cancel after this file")],
            Stage::Done => &[("c", "configure another"), ("Enter", "quit")],
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
        let message = self.detect_note.clone().unwrap_or_else(|| {
            "No disc found — enter a path to a mounted disc or its VIDEO_TS folder".into()
        });
        let input = Paragraph::new(self.path_input.as_str()).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(" Disc path "),
        );
        f.render_widget(input, rows[0]);
        set_cursor(f, rows[0], &self.path_input);

        let items: Vec<ListItem> = self
            .drives
            .iter()
            .map(|d| {
                let disc = if d.has_disc { "disc" } else { "no disc" };
                ListItem::new(format!("{:<16} {}", d.device.display(), disc))
            })
            .collect();
        let title = format!(" Drives ({}) — {} ", self.drives.len(), message);
        f.render_widget(
            List::new(items).block(Block::default().borders(Borders::ALL).title(title)),
            rows[1],
        );
    }

    fn render_plan(&self, f: &mut Frame, area: Rect) {
        let rows = Layout::vertical([
            // Disc, looks-like, parsed and TVmaze lines inside a bordered block.
            Constraint::Length(6),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(5),
        ])
        .split(area);

        self.render_disc_summary(f, rows[0]);

        // Show + season side by side.
        let fields = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(rows[1]);
        f.render_widget(
            self.field(
                " Show ",
                &self.show_input,
                self.focus == Field::Show,
                "TVmaze searches this name",
            ),
            fields[0],
        );
        if self.focus == Field::Show {
            set_cursor(f, fields[0], &self.show_input);
        }
        f.render_widget(
            self.field(
                " Season ",
                &self.season_input,
                self.focus == Field::Season,
                "",
            ),
            fields[1],
        );
        if self.focus == Field::Season {
            set_cursor(f, fields[1], &self.season_input);
        }

        f.render_widget(
            self.field(
                " TV directory ",
                &self.tv_input,
                self.focus == Field::TvDir,
                "optional — the folder that holds show folders",
            ),
            rows[2],
        );
        if self.focus == Field::TvDir {
            set_cursor(f, rows[2], &self.tv_input);
        }
        f.render_widget(
            self.field(
                " Movie directory ",
                &self.movie_input,
                self.focus == Field::MovieDir,
                "optional — the folder that holds movie folders",
            ),
            rows[3],
        );
        if self.focus == Field::MovieDir {
            set_cursor(f, rows[3], &self.movie_input);
        }

        let checkbox = if self.include_extras { "[x]" } else { "[ ]" };
        let mut extras = vec![
            Span::styled(format!(" {checkbox} "), Style::default().fg(Color::Cyan)),
            Span::raw("Include extras (trailers and featurettes)"),
            Span::styled("   e to toggle", Style::default().fg(Color::DarkGray)),
        ];
        if let Some(note) = &self.config_note {
            extras.push(Span::styled(
                format!("   {note}"),
                Style::default().fg(Color::Green),
            ));
        }
        f.render_widget(Paragraph::new(Line::from(extras)), rows[4]);

        let selected = self.jobs.iter().filter(|j| j.enabled).count();
        let title = format!(" Files — {selected} of {} selected ", self.jobs.len());
        self.render_job_list(f, rows[5], title, Some(self.job_cursor), true);
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
            lines.push(Line::from(vec![
                Span::styled(" Parsed  ", Style::default().fg(Color::DarkGray)),
                Span::raw(label.title.clone()),
                Span::styled(
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
                    ),
                    Style::default().fg(Color::DarkGray),
                ),
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
        match self.meta_state {
            MetaState::Loading => {
                let spinner = SPINNER[(self.tick / 2) % SPINNER.len()];
                Line::from(vec![
                    Span::styled(" TVmaze  ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!("{spinner} looking up…"),
                        Style::default().fg(Color::Yellow),
                    ),
                ])
            }
            MetaState::Ready => match &self.meta_show {
                Some(show) => Line::from(vec![
                    Span::styled(" TVmaze  ", Style::default().fg(Color::DarkGray)),
                    Span::styled(show.name.clone(), Style::default().fg(Color::Green)),
                    Span::styled(
                        show.year().map(|y| format!("  ({y})")).unwrap_or_default(),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]),
                None => Line::from(vec![
                    Span::styled(" TVmaze  ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        self.meta_note.clone().unwrap_or_else(|| "no match".into()),
                        Style::default().fg(Color::Yellow),
                    ),
                ]),
            },
            MetaState::Idle => Line::from(vec![
                Span::styled(" TVmaze  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    self.meta_note
                        .clone()
                        .unwrap_or_else(|| "set a library and press Enter to match".into()),
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
        let note = if self.cancel.load(Ordering::Relaxed) {
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
        let (marker, action, color) = match progress.phase {
            RemuxPhase::Probing => (
                SPINNER[(self.tick / 2) % SPINNER.len()].to_string(),
                "analyzing",
                Color::Yellow,
            ),
            RemuxPhase::Muxing => ("▶".to_string(), "writing", Color::Green),
        };
        let speed = metrics
            .megabytes_per_sec
            .map(|v| format!("{v:.1} MB/s"))
            .unwrap_or_else(|| "— MB/s".into());
        let eta = metrics
            .file_eta
            .map(|d| format!("ETA {}", fmt_eta(d)))
            .unwrap_or_else(|| "ETA —".into());
        Line::from(vec![
            Span::styled(format!(" {marker} "), Style::default().fg(color)),
            Span::styled(name, Style::default().fg(Color::Cyan)),
            Span::styled(format!("  {action}"), Style::default().fg(color)),
            Span::styled(format!("  {speed}"), Style::default().fg(Color::Green)),
            Span::styled(format!("  {eta}"), Style::default().fg(Color::DarkGray)),
        ])
    }

    fn render_done(&self, f: &mut Frame, area: Rect) {
        let rows = Layout::vertical([Constraint::Length(5), Constraint::Min(5)]).split(area);

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
        lines.push(Line::from(Span::styled(
            "Press c to configure another disc, or Enter to quit.",
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
        let popup = centered(area, 62, 16);
        f.render_widget(Clear, popup);
        let lines = vec![
            Line::from(Span::styled(
                "packrat — interactive backup",
                Style::default().fg(Color::Cyan).bold(),
            )),
            Line::raw(""),
            Line::from("Tab / Shift+Tab   move between fields"),
            Line::from("↑ / ↓             select a file"),
            Line::from("Space             include / skip a file"),
            Line::from("a                 include / skip all"),
            Line::from("e                 include extras"),
            Line::from("Enter             apply the show, season and directories"),
            Line::from("r                 start backing up"),
            Line::from("q / Esc           quit (while ripping: stop after this file)"),
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

fn detect_and_load(tx: Sender<WorkerEvent>) {
    std::thread::spawn(move || {
        let drives = packrat_core::drives::list();
        let ready = drives
            .iter()
            .find(|d| d.has_disc && d.mount.is_some())
            .cloned();
        match ready {
            Some(drive) => {
                let mount = drive.mount.clone().expect("checked above");
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
            None => {
                let _ = tx.send(WorkerEvent::NoDisc(drives));
            }
        }
    });
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
    source: DiscSource,
    disc: DiscModel,
    jobs: Vec<RipJob>,
) {
    std::thread::spawn(move || {
        for job in jobs {
            if cancel.load(Ordering::Relaxed) {
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
                            elapsed: started.elapsed(),
                        });
                    };
                    read_vts(&source, title.vts)
                        .and_then(|vts| {
                            remux_chain_with_progress(
                                &source,
                                &vts,
                                title,
                                job.first,
                                job.last,
                                &job.path,
                                &mut report_progress,
                            )
                        })
                        .map_err(|e| e.to_string())
                }
                None => Err(format!("disc has no title {}", job.title)),
            };

            let _ = tx.send(WorkerEvent::JobDone {
                index: job.index,
                result: outcome,
            });
        }
        let _ = tx.send(WorkerEvent::RipDone { cancelled: false });
    });
}

// ---------------------------------------------------------------------------
// Plan assembly
// ---------------------------------------------------------------------------

/// Turn a disc and its resolved metadata into the list of files to write.
fn assemble_jobs(
    disc: &DiscModel,
    classification: &Classification,
    naming: Option<&Naming>,
    movie_dir: Option<&Path>,
    out_dir: &Path,
    include_extras: bool,
) -> Vec<Job> {
    let mut jobs = Vec::new();

    if classification.kind == DiscKind::Movie {
        if let Some(film) = disc
            .content_titles(MIN_CONTENT)
            .into_iter()
            .max_by_key(|t| t.duration)
        {
            let label = parse_label(&disc.volume_id);
            let path = match movie_dir {
                Some(dir) => movie_file_in(dir, &label.title, label.year),
                None => out_dir.join(format!("{}.mkv", label.title)),
            };
            jobs.push(Job::new(film, 1, film.chapters, path));
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
    /// Fraction of the current file's playback written (0 while probing).
    file_fraction: f64,
    /// Output throughput for the current file.
    megabytes_per_sec: Option<f64>,
    /// Estimated time left for the current file.
    file_eta: Option<Duration>,
    /// Estimated time left for the whole queue.
    overall_eta: Option<Duration>,
}

/// Turn a progress snapshot into the numbers the UI shows. The current file's
/// read rate and bytes-per-playback-second are used to extrapolate the queue.
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

    let (megabytes_per_sec, file_eta, overall_eta) = if elapsed > 0.0 && progress.bytes_done > 0 {
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
        (
            Some(rate / 1_048_576.0),
            Some(file_eta),
            Some(file_eta + queue_eta),
        )
    } else {
        (None, None, None)
    };

    Metrics {
        file_fraction,
        megabytes_per_sec,
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
            Path::new("."),
            false,
        );
        let with = assemble_jobs(
            &d,
            &kind(DiscKind::TvSeries),
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
            elapsed: Duration::from_secs(30),
        };
        let metrics = compute_metrics(
            Some(&snapshot),
            Duration::from_secs(120),
            Duration::from_secs(120),
        );
        assert!((metrics.file_fraction - 0.5).abs() < 0.001);
        assert!((metrics.megabytes_per_sec.unwrap() - 2.0).abs() < 0.001);
        // 2 MiB/s, 60 MiB left -> 30s for this file.
        assert_eq!(metrics.file_eta.unwrap(), Duration::from_secs(30));
        // 1 MiB of work per playback second, 120s queued at 2 MiB/s -> 60s.
        assert_eq!(metrics.overall_eta.unwrap(), Duration::from_secs(90));
    }

    #[test]
    fn metrics_are_empty_before_the_first_byte() {
        let snapshot = ProgressSnapshot {
            index: 0,
            phase: RemuxPhase::Probing,
            bytes_done: 0,
            bytes_total: 120 * 1024 * 1024,
            elapsed: Duration::from_secs(5),
        };
        let metrics = compute_metrics(
            Some(&snapshot),
            Duration::from_secs(120),
            Duration::from_secs(60),
        );
        assert_eq!(metrics.file_fraction, 0.0);
        assert!(metrics.megabytes_per_sec.is_none());
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

        app.on_worker(WorkerEvent::NoDisc(Vec::new()));

        assert!(matches!(app.stage, Stage::Ripping));
        assert!(!app.scan_in_flight);
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
}

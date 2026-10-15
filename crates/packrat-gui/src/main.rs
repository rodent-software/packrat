//! `packrat` desktop application.
//!
//! The window is a small state machine over the backup flow: settings, drive
//! detection, review of what the disc is and which files to write, the rip
//! itself, and the result. All slow work happens on the engine's worker
//! threads, which report back over a channel that [`App`] drains on a timer.

mod jobs;
#[cfg(test)]
mod snapshot_tests;
mod theme;
mod view;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use iced::{Subscription, Task};

use packrat_core::{
    preferred_titles, primary_feature, Classification, DiscKind, DiscModel, DiscSource, LabelInfo,
    LibraryStats, Movie, OpticalDrive, RemuxPhase, Show,
};
use packrat_engine::config::{self, Config};
use packrat_engine::history::{self, History};
use packrat_engine::{
    detect_and_load, disc_fingerprint, eject_drive, list_drives, load_path, resolve_movie_naming,
    resolve_naming, scan_stats, search_titles, spawn_rip, watch_drive, DriveState, MovieNaming,
    Naming, RipJob, TitleMatch, WorkerEvent,
};

use jobs::Job;

fn main() -> iced::Result {
    iced::application(App::new, App::update, App::view)
        .subscription(App::subscription)
        .title(App::title)
        .theme(App::theme)
        .window_size((1040.0, 780.0))
        .run()
}

/// How often the worker channel is drained.
const TICK: Duration = Duration::from_millis(100);
/// Ticks between looks at a drive that has nothing to report.
const WATCH_IDLE_TICKS: u32 = 10;
/// How long a read may go without an answer before the window says so.
///
/// Every attempt a stuck read makes can cost the drive's whole command timeout
/// — half a minute on the drives this was written against — and nothing in the
/// process can interrupt a read the kernel is still waiting on. Twenty seconds
/// is past any normal wait and well short of the first timeout, so the window
/// reports the stall while the user can still do something about it.
const STALL_AFTER: Duration = Duration::from_secs(20);

/// Which screen the window is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    Setup,
    Detect,
    Plan,
    Ripping,
    Done,
}

/// Whether metadata resolution is in flight.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) enum MetaState {
    #[default]
    Idle,
    Loading,
    Failed(String),
}

/// The latest progress from the file currently being ripped.
#[derive(Debug)]
pub(crate) struct ProgressSnapshot {
    pub(crate) phase: RemuxPhase,
    pub(crate) bytes_done: u64,
    pub(crate) bytes_total: u64,
    pub(crate) elapsed: Duration,
}

impl ProgressSnapshot {
    /// Disc read rate over the whole run so far, in bytes per second.
    pub(crate) fn read_rate(&self) -> Option<f64> {
        let secs = self.elapsed.as_secs_f64();
        (secs > 0.0 && self.bytes_done > 0).then(|| self.bytes_done as f64 / secs)
    }

    /// How long the file being read still needs, at the rate it has run at.
    ///
    /// Only the current file: the bytes a later one will read are not known
    /// until it is read, so a figure for the whole run would be invented.
    pub(crate) fn eta(&self) -> Option<Duration> {
        let rate = self.read_rate()?;
        if rate <= 0.0 {
            return None;
        }
        let left = self.bytes_total.saturating_sub(self.bytes_done) as f64;
        Some(Duration::from_secs_f64(left / rate))
    }
}

/// Everything the window needs.
pub(crate) struct App {
    pub(crate) stage: Stage,

    // Settings.
    pub(crate) tv_input: String,
    pub(crate) movie_input: String,
    pub(crate) tmdb_input: String,
    pub(crate) auto_eject: bool,
    pub(crate) config_note: Option<String>,
    /// Why CSS-encrypted discs cannot be read, when libdvdcss is missing.
    pub(crate) css_note: Option<String>,
    config_return: Stage,

    // Drive picker.
    pub(crate) drives: Vec<OpticalDrive>,
    pub(crate) selected_drive: Option<usize>,
    pub(crate) manual_path: String,
    pub(crate) detect_loading: bool,
    pub(crate) detect_note: Option<String>,
    last_device: Option<PathBuf>,
    /// The drive this window is looking after, if any. Distinct from
    /// [`App::device`], which is cleared when the disc leaves: the window keeps
    /// watching the drive so the next disc in it loads on its own.
    watched_device: Option<PathBuf>,
    /// Ticks until the watched drive is looked at again, whether a look is in
    /// flight, and how many reads in a row have failed.
    drive_watch_countdown: u32,
    drive_watch_in_flight: bool,
    drive_watch_failures: u32,
    /// The size of the disc that was read, so the same drive handing over a
    /// different disc can be told from the same one still being there.
    loaded_media: Option<u64>,
    /// What the watcher is waiting for, when a disc is in the drive but will not
    /// read yet. Kept apart from the disc's own notes because this one is set
    /// and cleared on a timer, and would wipe a message from anywhere else.
    watch_note: Option<String>,

    // Loaded disc.
    pub(crate) source: Option<DiscSource>,
    /// Folders opened by hand, newest first. Session-scoped: it is a shortcut
    /// back to a disc, not a setting, and a remembered path that has since gone
    /// away would only be a trap.
    pub(crate) disc_paths: Vec<PathBuf>,
    pub(crate) disc: Option<DiscModel>,
    pub(crate) classification: Option<Classification>,
    /// The user's answer to movie-or-TV, when they have overruled the guess.
    /// `None` means the disc is planned for as whatever was detected.
    pub(crate) kind_override: Option<DiscKind>,
    /// The verdict the current job list was planned for, so a rebuild can tell
    /// a refresh of the same plan from a change of plan.
    plan_kind: Option<DiscKind>,
    pub(crate) label: Option<LabelInfo>,
    pub(crate) device: Option<PathBuf>,
    fingerprint: Option<String>,

    // Metadata.
    pub(crate) naming: Option<Naming>,
    pub(crate) movie_naming: Option<MovieNaming>,
    pub(crate) meta_show: Option<Show>,
    pub(crate) meta_movie: Option<Movie>,
    pub(crate) meta_state: MetaState,
    pub(crate) meta_note: Option<String>,
    /// What the last title search found, and how it went.
    pub(crate) title_matches: Vec<TitleMatch>,
    pub(crate) search_state: MetaState,
    pub(crate) show_input: String,
    pub(crate) season_input: String,
    pub(crate) first_episode_input: String,

    // Plan.
    pub(crate) jobs: Vec<Job>,
    pub(crate) include_extras: bool,

    // Rip.
    tx: Sender<WorkerEvent>,
    rx: Receiver<WorkerEvent>,
    cancel: Arc<AtomicBool>,
    abort: Arc<AtomicBool>,
    rip_handle: Option<JoinHandle<()>>,
    pub(crate) progress: Option<ProgressSnapshot>,
    pub(crate) current: Option<usize>,
    pub(crate) total: usize,
    pub(crate) done: usize,
    pub(crate) failed: usize,
    pub(crate) cancelled: bool,
    pub(crate) rip_started: Option<Instant>,
    /// How long the last run took, frozen when it ended so the summary stops
    /// counting while it is being read.
    pub(crate) rip_elapsed: Option<Duration>,
    session_files: u64,
    session_bytes: u64,

    /// When the worker last answered, and when a read was last asked for.
    ///
    /// Together with the state, this is what tells a working drive from one that
    /// has stopped answering: everything the worker does sends an event, so a
    /// long silence while something is waiting on the disc is the drive, not us.
    signal_at: Instant,

    // Collection summary.
    pub(crate) library: LibraryStats,
    pub(crate) free_bytes: Option<u64>,
    pub(crate) history: History,

    // Eject.
    pub(crate) ejected: bool,
    pub(crate) eject_note: Option<String>,
}

/// Every interaction the window can produce.
#[derive(Debug, Clone)]
pub(crate) enum Message {
    Tick,

    TvDirChanged(String),
    MovieDirChanged(String),
    TmdbKeyChanged(String),
    AutoEjectToggled(bool),
    SaveAndClose,
    OpenSettings,
    CloseSettings,

    DriveSelected(usize),
    DiscPathSelected(PathBuf),
    Browse,
    PathChosen(Option<PathBuf>),
    Eject(PathBuf),
    KindChosen(DiscKind),
    ShowChanged(String),
    SeasonChanged(String),
    FirstEpisodeChanged(String),
    SearchTitles,
    ChooseTitleMatch(usize),
    RefreshMetadata,

    ToggleExtras,
    ToggleJob(usize),
    SelectAll(bool),
    StartRip,

    AbortRip,

    BackToPlan,
}

/// Whether a message would change the plan a run is already working from.
///
/// A run reads the plan it started with, so editing a name, a title's kind or a
/// checkbox halfway through would leave the screen describing a backup other
/// than the one being written. The controls are disabled while it runs, and this
/// is the same rule in one place for anything that still arrives — a keypress in
/// a field that was focused when the run began, say.
fn edits_the_plan(message: &Message) -> bool {
    matches!(
        message,
        Message::DriveSelected(_)
            | Message::DiscPathSelected(_)
            | Message::Browse
            | Message::Eject(_)
            | Message::KindChosen(_)
            | Message::ShowChanged(_)
            | Message::SeasonChanged(_)
            | Message::FirstEpisodeChanged(_)
            | Message::SearchTitles
            | Message::ChooseTitleMatch(_)
            | Message::RefreshMetadata
            | Message::ToggleExtras
            | Message::ToggleJob(_)
            | Message::SelectAll(_)
            | Message::StartRip
    )
}

impl App {
    fn new() -> (Self, Task<Message>) {
        let config = Config::load();
        let (tx, rx) = mpsc::channel();
        let configured = config::exists();
        let last_device = config.last_device.clone();

        // Checked once at startup: the user installs libdvdcss outside packrat.
        let css_note = match packrat_core::dvdcss::status() {
            packrat_core::dvdcss::DvdcssStatus::Available { .. } => None,
            packrat_core::dvdcss::DvdcssStatus::Unavailable { reason } => Some(format!(
                "CSS-encrypted discs cannot be read: {reason}. See the installation guide."
            )),
        };

        let mut app = App {
            stage: Stage::Setup,
            tv_input: config
                .tv_dir
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            movie_input: config
                .movie_dir
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            tmdb_input: config.tmdb_api_key.clone().unwrap_or_default(),
            auto_eject: config.auto_eject,
            config_note: None,
            css_note,
            config_return: Stage::Detect,
            drives: Vec::new(),
            selected_drive: None,
            manual_path: String::new(),
            detect_loading: false,
            detect_note: None,
            last_device,
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
            meta_note: None,
            title_matches: Vec::new(),
            search_state: MetaState::Idle,
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
            history: History::load(),
            ejected: false,
            eject_note: None,
            signal_at: Instant::now(),
        };

        // Start looking for a disc straight away; on a first run the settings
        // screen is shown over the top so the probe overlaps with the user
        // filling the form in, and the disc is ready when they continue.
        app.begin_scan();
        if !configured {
            app.stage = Stage::Setup;
            app.config_return = Stage::Detect;
        }

        (app, Task::none())
    }

    fn title(&self) -> String {
        "packrat".to_string()
    }

    fn theme(&self) -> iced::Theme {
        theme::theme()
    }

    fn subscription(&self) -> Subscription<Message> {
        iced::time::every(TICK).map(|_| Message::Tick)
    }

    // -- worker bridge ------------------------------------------------------

    /// Probe for drives and load the remembered disc if it is unambiguous.
    fn begin_scan(&mut self) {
        self.stage = Stage::Detect;
        self.detect_loading = true;
        self.detect_note = None;
        self.asked_the_worker();
        detect_and_load(self.tx.clone(), self.last_device.clone());
        self.refresh_stats();
    }

    fn refresh_stats(&mut self) {
        scan_stats(self.tx.clone(), self.tv_path(), self.movie_path());
    }

    /// Whether a run is reading the screen's disc right now.
    ///
    /// The controls that would change that disc or its plan read this, so they
    /// go grey rather than disappearing or turning into labels.
    pub(crate) fn ripping(&self) -> bool {
        self.stage == Stage::Ripping
    }

    /// Whether the plan would produce anything, which is what enables backing
    /// up.
    pub(crate) fn has_selection(&self) -> bool {
        self.jobs.iter().any(|job| job.enabled)
    }

    /// How long something has been waiting on the drive, when it has gone quiet.
    ///
    /// A read is in flight while a disc is being loaded, while the watcher is
    /// probing the drive, and while a run is going; every answer resets the
    /// clock. A figure here therefore means the drive has not answered for that
    /// long, which is what a drive in trouble looks like from here — the app
    /// cannot interrupt a read the kernel is still waiting on, so all it can do
    /// is say so rather than sit silent.
    pub(crate) fn quiet_for(&self) -> Option<Duration> {
        let waiting = self.detect_loading || self.drive_watch_in_flight || self.ripping();
        if !waiting {
            return None;
        }
        let quiet = self.signal_at.elapsed();
        (quiet >= STALL_AFTER).then_some(quiet)
    }

    /// Note that the worker has just been asked for something.
    fn asked_the_worker(&mut self) {
        self.signal_at = Instant::now();
    }

    /// Load the drive the dropdown points at, when it holds a disc.
    fn select_drive(&mut self, index: usize) {
        // A run reads the disc it started with. Loading another one now would
        // swap the screen to a different disc while the rip kept going, so the
        // picker is inert until it stops.
        if self.stage == Stage::Ripping {
            return;
        }
        let Some(drive) = self.drives.get(index).cloned() else {
            return;
        };

        // The path field is the one place the current disc is named, so a drive
        // chosen any way at all lands in it.
        self.manual_path = drive.device.display().to_string();

        // Whatever was loaded came from somewhere else now.
        if self.device.as_deref() != Some(drive.device.as_path()) {
            self.clear_disc();
        }

        self.detect_note = None;
        self.last_device = Some(drive.device.clone());
        // The watcher does the reading, and goes on doing it: a drive that will
        // not answer yet is retried rather than left for the user to poke.
        self.focus_drive(drive.device);
    }

    /// Read a disc from the path typed into the folder field.
    /// Put a hand-opened folder at the top of the disc list, without repeats.
    pub(crate) fn remember_disc_path(&mut self, path: &Path) {
        const REMEMBERED: usize = 6;

        self.disc_paths.retain(|kept| kept != path);
        self.disc_paths.insert(0, path.to_path_buf());
        self.disc_paths.truncate(REMEMBERED);
    }

    /// Look at the drive this window is watching, now and then.
    ///
    /// Nothing tells us when a disc goes into a drive, so the only way to notice
    /// is to look. The look reads `/sys/block` rather than the drive, so it is
    /// cheap enough to repeat, and it does the reading too when there is a disc
    /// this window does not have yet.
    fn poll_drive(&mut self) {
        self.drive_watch_countdown = self.drive_watch_countdown.saturating_sub(1);
        if self.drive_watch_countdown > 0 || !self.should_poll_drive() {
            return;
        }
        self.drive_watch_in_flight = true;
        self.asked_the_worker();

        match self.watched_device.clone() {
            Some(device) => {
                // A disc the window does not have yet is worth reading; one it
                // has is only worth checking is still there.
                self.drive_watch_countdown = WATCH_IDLE_TICKS;
                watch_drive(self.tx.clone(), device, self.disc.is_none());
            }
            None => {
                // Nothing to watch, so all this window can do is keep the list
                // of drives current. It must not read a disc another window may
                // be working with, which is why it only ever lists them.
                self.drive_watch_countdown = WATCH_IDLE_TICKS;
                list_drives(self.tx.clone());
            }
        }
    }

    /// Whether there is any point looking.
    ///
    /// A run in flight owns the disc, and settings are not about the disc at
    /// all. Everything else looks, a finished run included: a drive that is not
    /// watched never notices its disc leaving, so the screen would go on
    /// describing a disc that is no longer in it.
    fn should_poll_drive(&self) -> bool {
        !self.drive_watch_in_flight && self.stage != Stage::Setup && !self.ripping()
    }

    /// How long to wait before reading the watched drive again.
    ///
    /// A disc that has just gone into a drive usually needs a moment before it
    /// can be read, and the first attempts often fail. Starting short and
    /// backing off is what a person does by pressing rescan a few times, done
    /// for them.
    fn retry_ticks(&self) -> u32 {
        const FIRST: u32 = 5; // 0.5 s
        const MAX: u32 = 50; // 5 s
        FIRST
            .saturating_mul(1 << self.drive_watch_failures.saturating_sub(1).min(4))
            .min(MAX)
    }

    /// Point this window at a drive, and let the watcher do the reading.
    pub(crate) fn focus_drive(&mut self, device: PathBuf) {
        self.watched_device = Some(device);
        self.drive_watch_failures = 0;
        // Look straight away: a drive the user has just picked should not wait
        // out a poll interval to be read.
        self.drive_watch_countdown = 0;
    }

    /// The disc we were reading has gone: put the screen back to the picker.
    ///
    /// The drive stays watched, so the next disc into it loads on its own; it
    /// also stays selected, so the list and the line under it say which drive is
    /// now empty.
    ///
    /// A finished run keeps its figures on the bar. What left is the disc, and
    /// the run is reported where the run is reported; everything the disc
    /// supplied — its cards, its plan — goes with it.
    pub(crate) fn disc_removed(&mut self) {
        let finished = self.stage == Stage::Done;
        self.clear_disc();
        if finished {
            self.stage = Stage::Done;
        }
        self.detect_note = Some(
            if self.ejected {
                "The disc was ejected."
            } else {
                "The disc was removed."
            }
            .into(),
        );
        // The drive is still there; it is the disc that is not, so refresh what
        // the list says about it.
        list_drives(self.tx.clone());
    }

    /// Forget everything that came off the disc.
    ///
    /// Everything read off it is stale the moment it leaves the drive, so the
    /// plan goes with it: leaving it up would offer to back up a disc that is no
    /// longer there.
    pub(crate) fn clear_disc(&mut self) {
        self.disc = None;
        self.source = None;
        self.classification = None;
        self.kind_override = None;
        self.plan_kind = None;
        self.label = None;
        self.fingerprint = None;
        self.naming = None;
        self.movie_naming = None;
        self.meta_show = None;
        self.meta_movie = None;
        self.title_matches.clear();
        self.search_state = MetaState::Idle;
        self.jobs.clear();
        self.include_extras = false;
        self.show_input.clear();
        self.season_input.clear();
        self.first_episode_input.clear();
        self.meta_note = None;
        self.title_matches.clear();
        self.search_state = MetaState::Idle;
        self.device = None;
        self.loaded_media = None;
        self.stage = Stage::Detect;
    }

    fn load_manual_path(&mut self) {
        // Same as the drive picker: not while a run is reading one disc.
        if self.stage == Stage::Ripping {
            return;
        }
        self.watched_device = None;
        if non_empty(&self.manual_path).is_none() {
            self.detect_note = Some("Enter the path of a mounted disc.".into());
            return;
        }

        let path = config::expand_tilde(self.manual_path.trim());
        self.detect_loading = true;
        self.detect_note = None;
        self.device = None;
        self.selected_drive = None;
        self.asked_the_worker();
        load_path(self.tx.clone(), path);
    }

    pub(crate) fn tv_path(&self) -> Option<PathBuf> {
        path_from(&self.tv_input)
    }

    pub(crate) fn movie_path(&self) -> Option<PathBuf> {
        path_from(&self.movie_input)
    }

    /// The kind to plan with: the user's choice when they have made one, else
    /// what the heuristics read off the disc.
    pub(crate) fn disc_kind(&self) -> Option<DiscKind> {
        self.kind_override
            .or_else(|| self.classification.as_ref().map(|c| c.kind))
    }

    /// Record the user's movie-or-TV choice.
    ///
    /// Choosing the kind that was already detected clears the override, so the
    /// disc goes back to being described by what was read off it rather than
    /// staying pinned to a choice that agrees with it anyway.
    fn choose_kind(&mut self, kind: DiscKind) {
        let detected = self.classification.as_ref().map(|c| c.kind);
        self.kind_override = (detected != Some(kind)).then_some(kind);
    }

    /// The detected classification with the user's choice substituted in, so the
    /// planner, the output directory and the metadata lookup all follow the
    /// verdict the picker shows.
    fn effective_classification(&self) -> Option<Classification> {
        let mut classification = self.classification.clone()?;
        if let Some(kind) = self.kind_override {
            classification.kind = kind;
        }
        Some(classification)
    }

    pub(crate) fn is_movie(&self) -> bool {
        self.disc_kind() == Some(DiscKind::Movie)
    }

    /// Recompute the job list after the disc, metadata or extras toggle change.
    fn rebuild_jobs(&mut self) {
        let (Some(disc), Some(classification)) =
            (self.disc.clone(), self.effective_classification())
        else {
            return;
        };
        let tv_dir = self.tv_path();
        let movie_dir = self.movie_path();
        let out_dir = match classification.kind {
            DiscKind::Movie => movie_dir.clone(),
            _ => tv_dir.clone(),
        }
        .unwrap_or_else(|| PathBuf::from("."));

        let jobs = jobs::assemble(
            &disc,
            &classification,
            self.naming.as_ref(),
            self.movie_naming.as_ref(),
            movie_dir.as_deref(),
            &out_dir,
            self.include_extras,
        );
        // A rebuild for the same verdict is a refresh — new metadata, another
        // extras setting — so the user's ticks survive it. A change of verdict
        // is a different plan whose chapter ranges have become different
        // things, so it starts from the plan's own defaults instead of
        // inheriting selections that no longer mean the same.
        let carry_selection = self.plan_kind == Some(classification.kind);
        self.set_jobs(jobs, carry_selection);
        self.plan_kind = Some(classification.kind);
    }

    /// Replace the job list, carrying each job's selected state across so a
    /// metadata refresh does not undo the user's choices.
    fn set_jobs(&mut self, new: Vec<Job>, carry_selection: bool) {
        self.jobs = new
            .into_iter()
            .map(|mut job| {
                if carry_selection {
                    if let Some(old) = self.jobs.iter().find(|o| {
                        o.title == job.title && o.first == job.first && o.last == job.last
                    }) {
                        job.enabled = old.enabled;
                    }
                }
                job
            })
            .collect();
    }

    /// Record a chosen search result as the match fields' new contents.
    ///
    /// The year is a property of the title, so a movie takes it; a show's
    /// season is a property of the disc, and is left alone.
    fn apply_title_match(&mut self, found: &TitleMatch) {
        self.show_input = found.name.clone();
        if self.is_movie() {
            self.season_input = found.year.map(|year| year.to_string()).unwrap_or_default();
        }
        self.title_matches.clear();
        self.search_state = MetaState::Idle;
    }

    /// Re-resolve TVmaze/TMDb metadata for the current inputs.
    fn refresh_metadata(&mut self) {
        self.meta_note = None;
        let Some(disc) = self.disc.clone() else {
            return;
        };
        let show = non_empty(&self.show_input).map(str::to_string);
        let season = self.season_input.trim().parse::<u16>().ok();
        let first_episode = self.first_episode_input.trim().parse::<u16>().ok();

        if self.is_movie() {
            self.naming = None;
            self.meta_show = None;
            self.meta_movie = None;
            self.title_matches.clear();
            self.search_state = MetaState::Idle;

            let feature = primary_feature(&disc).cloned();
            let Some((movie_dir, feature)) = self.movie_path().zip(feature) else {
                self.movie_naming = None;
                self.meta_state = MetaState::Idle;
                self.meta_note =
                    Some("Set a movie directory in settings to name files from TMDb.".into());
                self.rebuild_jobs();
                return;
            };

            self.meta_state = MetaState::Loading;
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                match resolve_movie_naming(&movie_dir, &disc, &feature, show.as_deref(), season) {
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

    /// Queue the selected files and start ripping.
    fn start_rip(&mut self) {
        let selected: Vec<usize> = self
            .jobs
            .iter()
            .enumerate()
            .filter(|(_, job)| job.enabled)
            .map(|(i, _)| i)
            .collect();
        if selected.is_empty() {
            self.meta_note = Some("Select at least one file to back up.".into());
            return;
        }
        let (Some(source), Some(disc)) = (self.source.clone(), self.disc.clone()) else {
            return;
        };

        self.finish_rip();
        for job in &mut self.jobs {
            job.status = jobs::JobStatus::Pending;
        }
        self.total = selected.len();
        self.done = 0;
        self.failed = 0;
        self.cancelled = false;
        self.current = None;
        self.progress = None;
        self.session_files = 0;
        self.session_bytes = 0;
        self.ejected = false;
        self.eject_note = None;
        self.rip_started = Some(Instant::now());
        self.rip_elapsed = None;
        self.stage = Stage::Ripping;
        self.cancel.store(false, Ordering::Relaxed);
        self.abort.store(false, Ordering::Relaxed);

        let rip_jobs: Vec<RipJob> = selected
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
        self.asked_the_worker();
        self.rip_handle = Some(spawn_rip(
            self.tx.clone(),
            self.cancel.clone(),
            self.abort.clone(),
            source,
            disc,
            rip_jobs,
            self.device.clone(),
        ));
    }

    /// Stop any rip worker and wait for it, so a new run cannot race the old.
    fn finish_rip(&mut self) {
        if let Some(handle) = self.rip_handle.take() {
            self.cancel.store(true, Ordering::Relaxed);
            self.abort.store(true, Ordering::Relaxed);
            let _ = handle.join();
        }
    }

    /// Persist the settings screen.
    fn save_config(&mut self) {
        let config = Config {
            tv_dir: self.tv_path(),
            movie_dir: self.movie_path(),
            tmdb_api_key: non_empty(&self.tmdb_input).map(str::to_string),
            last_device: self.last_device.clone(),
            auto_eject: self.auto_eject,
        };
        self.config_note = Some(match config.save() {
            Ok(true) => "Settings saved.".into(),
            Ok(false) => "Settings unchanged.".into(),
            Err(e) => format!("Could not save settings: {e:#}"),
        });
    }

    // -- update -------------------------------------------------------------

    fn update(&mut self, message: Message) -> Task<Message> {
        // The one rule that keeps the screen and the run describing the same
        // disc. Checked before dispatch so no arm has to remember it.
        if self.stage == Stage::Ripping && edits_the_plan(&message) {
            return Task::none();
        }

        match message {
            Message::Tick => {
                let events: Vec<WorkerEvent> = self.rx.try_iter().collect();
                for event in events {
                    self.on_worker(event);
                }
                self.poll_drive();
            }

            Message::KindChosen(kind) => {
                let before = self.disc_kind();
                self.choose_kind(kind);
                // The kind decides which metadata service answers and how the
                // files are named, so re-resolve rather than only re-planning —
                // but not when the click left the verdict as it was.
                if self.disc_kind() != before {
                    self.refresh_metadata();
                }
            }

            Message::TvDirChanged(value) => {
                self.tv_input = value;
                self.config_note = None;
            }
            Message::MovieDirChanged(value) => {
                self.movie_input = value;
                self.config_note = None;
            }
            Message::TmdbKeyChanged(value) => {
                self.tmdb_input = value;
                self.config_note = None;
            }
            Message::AutoEjectToggled(value) => {
                self.auto_eject = value;
                self.config_note = None;
            }
            Message::SaveAndClose => {
                self.save_config();
                self.stage = self.config_return;
                self.refresh_metadata();
            }
            Message::OpenSettings => {
                if self.stage != Stage::Setup {
                    self.config_return = self.stage;
                }
                self.config_note = None;
                self.stage = Stage::Setup;
            }
            Message::CloseSettings => {
                self.stage = self.config_return;
                self.refresh_metadata();
            }

            Message::DriveSelected(index) => {
                self.selected_drive = Some(index);
                self.select_drive(index);
            }
            Message::DiscPathSelected(path) => {
                // A folder has no drive to watch, so the watch stops.
                self.watched_device = None;
                self.manual_path = path.display().to_string();
                self.load_manual_path();
            }
            Message::Browse => {
                // Reopening the picker lands where the last one left off, if that
                // was a folder: a device path is not somewhere a folder dialog
                // can open.
                let current = config::expand_tilde(self.manual_path.trim());
                let start = current.is_dir().then_some(current);
                return Task::perform(
                    async move {
                        let mut dialog =
                            rfd::AsyncFileDialog::new().set_title("Choose a disc folder");
                        if let Some(dir) = start {
                            dialog = dialog.set_directory(dir);
                        }
                        dialog
                            .pick_folder()
                            .await
                            .map(|handle| handle.path().to_path_buf())
                    },
                    Message::PathChosen,
                );
            }
            Message::PathChosen(Some(path)) => {
                self.watched_device = None;
                // A folder the user opened by hand joins the list, so getting
                // back to it is a click rather than another trip through the
                // picker.
                self.remember_disc_path(&path);
                self.manual_path = path.display().to_string();
                self.load_manual_path();
            }
            Message::PathChosen(None) => {}
            Message::Eject(device) => {
                self.eject_note = None;
                eject_drive(self.tx.clone(), device);
            }

            Message::ShowChanged(value) => self.show_input = value,
            Message::SeasonChanged(value) => self.season_input = value,
            Message::FirstEpisodeChanged(value) => self.first_episode_input = value,
            Message::SearchTitles => {
                // The field is the query: it already holds what the disc calls
                // itself, so searching is one press rather than a retype.
                let query = non_empty(&self.show_input)
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        self.label
                            .as_ref()
                            .map(|label| label.title.clone())
                            .unwrap_or_default()
                    });
                let year = self.season_input.trim().parse::<u16>().ok();
                self.title_matches.clear();
                self.search_state = MetaState::Loading;
                search_titles(self.tx.clone(), self.is_movie(), query, year);
            }
            Message::ChooseTitleMatch(index) => {
                let Some(found) = self.title_matches.get(index).cloned() else {
                    return Task::none();
                };
                self.apply_title_match(&found);
                // The fields now name something else, so resolve against it.
                self.refresh_metadata();
            }
            Message::RefreshMetadata => self.refresh_metadata(),
            Message::ToggleExtras => {
                if self.is_movie() {
                    // A movie lists every title, so the toggle only flips the
                    // selection of the non-feature files.
                    let all_on = self.jobs.len() > 1 && self.jobs.iter().skip(1).all(|j| j.enabled);
                    for job in self.jobs.iter_mut().skip(1) {
                        job.enabled = !all_on;
                    }
                    self.include_extras = !all_on;
                } else {
                    self.include_extras = !self.include_extras;
                    self.rebuild_jobs();
                }
            }
            Message::ToggleJob(i) => {
                if let Some(job) = self.jobs.get_mut(i) {
                    job.enabled = !job.enabled;
                }
            }
            Message::SelectAll(value) => {
                for job in &mut self.jobs {
                    job.enabled = value;
                }
            }
            Message::StartRip => self.start_rip(),

            Message::AbortRip => {
                self.abort.store(true, Ordering::Relaxed);
            }

            // What the "Done" button does: the result has been read, so the
            // screen goes back to the plan it came from. The disc is still
            // loaded to take more from, and the picker beside it is how the next
            // disc is chosen, so the two ways on a run used to offer are one act.
            Message::BackToPlan => {
                self.stage = Stage::Plan;
                self.progress = None;
                self.current = None;
            }
        }
        Task::none()
    }

    // -- worker events ------------------------------------------------------

    fn on_worker(&mut self, event: WorkerEvent) {
        // Only the answers that are about a disc reset the stall clock: a
        // library scan finishing or a title search coming back says nothing
        // about whether the drive is answering.
        if matches!(
            &event,
            WorkerEvent::DiscReady { .. }
                | WorkerEvent::DiscError(_)
                | WorkerEvent::Drives(_)
                | WorkerEvent::DriveWatched { .. }
                | WorkerEvent::JobStarted(_)
                | WorkerEvent::JobProgress { .. }
                | WorkerEvent::JobDone { .. }
                | WorkerEvent::RipDone { .. }
                | WorkerEvent::EjectDone { .. }
        ) {
            self.signal_at = Instant::now();
        }

        match event {
            WorkerEvent::DiscReady {
                source,
                disc,
                classification,
                label,
                device,
            } => {
                let fingerprint = disc_fingerprint(&source, &disc);
                let unchanged = self.fingerprint.as_deref() == Some(fingerprint.as_str())
                    && self.disc.is_some();
                self.fingerprint = Some(fingerprint);
                self.detect_loading = false;
                self.detect_note = None;
                self.device = device;
                if let Some(device) = &self.device {
                    self.manual_path = device.display().to_string();
                    // Reading from a drive is what makes this window the one
                    // looking after it.
                    self.focus_drive(device.clone());
                }
                self.selected_drive = self
                    .device
                    .as_ref()
                    .and_then(|d| self.drives.iter().position(|drive| &drive.device == d));

                // A rescan of the disc already loaded must not discard edits.
                if unchanged {
                    return;
                }

                self.show_input = label.title.clone();
                self.season_input = label.season.map(|s| s.to_string()).unwrap_or_default();
                self.title_matches.clear();
                self.search_state = MetaState::Idle;
                self.first_episode_input.clear();
                self.naming = None;
                self.movie_naming = None;
                self.meta_show = None;
                self.meta_movie = None;
                self.include_extras = false;
                self.meta_note = None;
                self.label = Some(label);
                self.classification = Some(classification);
                self.kind_override = None;
                self.source = Some(source);
                self.disc = Some(disc);
                // Everything is on one screen, so a disc goes straight to being
                // planned for: there is no step to press to make its metadata
                // resolve, and nothing to navigate to in order to see it.
                self.stage = Stage::Plan;
                self.detect_loading = false;
                self.rebuild_jobs();
                self.refresh_metadata();
            }

            WorkerEvent::DiscError(message) => {
                self.detect_loading = false;
                self.detect_note = Some(message);
                self.stage = Stage::Detect;
            }

            WorkerEvent::Drives(drives) => {
                // The list is also what an unpinned window asks for on a timer,
                // so this is where that look finishes.
                self.drive_watch_in_flight = false;
                // Only when it actually changed: the watcher asks about once a
                // second, and a fresh list every time would rebuild the picker
                // for nothing.
                if drives != self.drives {
                    self.drives = drives;
                }
                self.detect_loading = false;
            }

            WorkerEvent::MetaReady { show, naming, note } => {
                self.meta_state = MetaState::Idle;
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
                self.meta_state = MetaState::Idle;
                self.movie_naming = Some(naming);
                self.meta_movie = movie;
                // The resolver's ranking and a search hit are the same choice to
                // the person making it, so they share one list.
                self.title_matches = candidates
                    .iter()
                    .map(|(score, movie)| TitleMatch::from_movie(*score, movie))
                    .collect();
                self.meta_note = note;
                self.rebuild_jobs();
            }

            WorkerEvent::TitleMatches(matches) => {
                self.search_state = MetaState::Idle;
                self.title_matches = matches;
            }

            WorkerEvent::TitleSearchFailed(message) => {
                self.search_state = MetaState::Failed(message);
                self.title_matches.clear();
            }

            WorkerEvent::MetaError(message) => {
                self.meta_state = MetaState::Failed(message.clone());
                self.meta_note = Some(message);
                self.rebuild_jobs();
            }

            WorkerEvent::LibraryStats {
                library,
                free_bytes,
            } => {
                self.library = library;
                self.free_bytes = free_bytes;
            }

            WorkerEvent::JobStarted(index) => {
                if let Some(job) = self.jobs.get_mut(index) {
                    job.status = jobs::JobStatus::Running;
                }
                self.current = Some(index);
                self.progress = None;
            }

            WorkerEvent::JobProgress {
                index: _,
                phase,
                bytes_done,
                bytes_total,
                elapsed,
                ..
            } => {
                self.progress = Some(ProgressSnapshot {
                    phase,
                    bytes_done,
                    bytes_total,
                    elapsed,
                });
            }

            WorkerEvent::JobDone { index, result } => {
                self.done += 1;
                self.progress = None;
                self.current = None;
                match result {
                    Ok(report) => {
                        self.session_files += 1;
                        self.session_bytes += report.payload_bytes;
                        if let Some(job) = self.jobs.get_mut(index) {
                            job.status = jobs::JobStatus::Done(report);
                        }
                    }
                    Err(message) => {
                        self.failed += 1;
                        if let Some(job) = self.jobs.get_mut(index) {
                            job.status = jobs::JobStatus::Failed(message);
                        }
                    }
                }
            }

            WorkerEvent::RipDone { cancelled } => {
                if let Some(handle) = self.rip_handle.take() {
                    let _ = handle.join();
                }
                self.cancelled = cancelled;
                self.progress = None;
                self.current = None;
                self.rip_elapsed = self.rip_started.map(|started| started.elapsed());
                self.stage = Stage::Done;

                // An aborted file never gets a `JobDone`, so mark whatever was
                // in flight as stopped rather than leaving it "running".
                if cancelled {
                    for job in &mut self.jobs {
                        if matches!(job.status, jobs::JobStatus::Running) {
                            job.status = jobs::JobStatus::Cancelled;
                        }
                    }
                }

                if self.session_files > 0 {
                    self.history.record(
                        self.session_files,
                        self.session_bytes,
                        history::unix_now(),
                    );
                    let _ = self.history.save();
                }
                self.refresh_stats();

                // Eject automatically only after a clean, complete run.
                if !cancelled && self.failed == 0 && self.auto_eject {
                    if let Some(device) = self.device.clone() {
                        eject_drive(self.tx.clone(), device);
                    }
                }
            }

            WorkerEvent::DriveWatched {
                device,
                state,
                drives,
            } => {
                self.drive_watch_in_flight = false;
                // The list comes with every answer, so a window pinned to one
                // drive still knows about the others. Only when it changed: a
                // look a second would otherwise rebuild the picker for nothing.
                if drives != self.drives {
                    self.drives = drives;
                }
                // A stale answer about a drive this window has stopped watching
                // describes a disc that is none of its business.
                if self.watched_device.as_deref() != Some(device.as_path()) {
                    return;
                }
                match state {
                    DriveState::Empty => {
                        self.drive_watch_failures = 0;
                        self.drive_watch_countdown = WATCH_IDLE_TICKS;
                        self.watch_note = None;
                        if self.device.is_some() {
                            self.disc_removed();
                        }
                    }
                    DriveState::Unreadable { message, .. } => {
                        // Ordinary just after a disc goes in, so keep trying
                        // and say so without shouting.
                        self.drive_watch_failures += 1;
                        self.drive_watch_countdown = self.retry_ticks();
                        self.watch_note =
                            Some(format!("Reading the disc… (the drive said: {message})"));
                    }
                    DriveState::Present { bytes } => {
                        self.drive_watch_failures = 0;
                        self.drive_watch_countdown = WATCH_IDLE_TICKS;
                        self.watch_note = None;
                        if self.disc.is_some() {
                            match self.loaded_media {
                                // First sight of the disc: remember how big it
                                // is, so the next look has something to compare.
                                None => self.loaded_media = Some(bytes),
                                // The same drive, a disc of another size: it was
                                // swapped with no moment of emptiness in between
                                // for the watcher to catch, and the screen would
                                // otherwise go on describing the disc that left.
                                Some(loaded) if loaded != bytes => self.clear_disc(),
                                Some(_) => {}
                            }
                        }
                    }
                }
            }

            WorkerEvent::EjectDone { result } => {
                match result {
                    Ok(()) => {
                        self.ejected = true;
                        self.eject_note = None;
                        // The disc is out of the drive, so nothing read off it
                        // is true any more. Cleared here rather than left to the
                        // watcher, which is not looking while settings are up.
                        if self.disc.is_some() {
                            self.disc_removed();
                        }
                        self.refresh_stats();
                        // Look at the drive straight away: the next disc should
                        // load without waiting out a poll interval.
                        self.drive_watch_countdown = 0;
                    }
                    Err(message) => {
                        self.ejected = false;
                        self.eject_note = Some(message);
                    }
                }
            }
        }
    }

    fn view(&self) -> iced::Element<'_, Message> {
        view::view(self)
    }
}

/// A trimmed non-empty input, or `None`.
fn non_empty(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Turn a path input into a path, expanding a leading `~/`.
fn path_from(input: &str) -> Option<PathBuf> {
    non_empty(input).map(config::expand_tilde)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_path_inputs_are_ignored() {
        assert_eq!(path_from("   "), None);
        assert_eq!(path_from("/mnt/media"), Some(PathBuf::from("/mnt/media")));
    }

    #[test]
    fn non_empty_trims() {
        assert_eq!(non_empty("  x "), Some("x"));
        assert_eq!(non_empty("  "), None);
    }
}

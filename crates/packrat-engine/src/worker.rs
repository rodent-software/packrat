//! Background work: probing drives, loading discs, scanning the library and
//! ripping files.
//!
//! Every slow operation runs on a worker thread and reports back over an
//! `mpsc` channel as a [`WorkerEvent`], so a frontend only has to drain events
//! on its own thread. That event stream is the whole concurrency surface a
//! graphical frontend needs; nothing here assumes a terminal.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use packrat_core::{
    read_vts, remux_chain_with_progress_and_cancel, Classification, DiscError, DiscModel,
    DiscSource, LabelInfo, LibraryStats, Movie, OpticalDrive, RemuxPhase, RemuxProgress,
    RemuxReport, Show, VtsIfo,
};

use crate::disc::{load_device, load_disc};
use packrat_core::drives::Media;

use crate::naming::{MovieNaming, Naming, TitleMatch};

/// Events sent from worker threads back to the UI thread.
pub enum WorkerEvent {
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
    /// What a title search found, in the order the provider ranked it.
    TitleMatches(Vec<TitleMatch>),
    /// A title search that could not be made, or answered.
    TitleSearchFailed(String),
    /// The answer to a [`watch_drive`] look at one drive, along with the drives
    /// as they are now: a window that is watching one drive still has to know
    /// about the others.
    DriveWatched {
        device: PathBuf,
        state: DriveState,
        drives: Vec<OpticalDrive>,
    },
    /// A refreshed collection summary from the background scan.
    LibraryStats {
        library: LibraryStats,
        free_bytes: Option<u64>,
    },
}

/// One file queued for ripping.
pub struct RipJob {
    pub index: usize,
    pub title: u16,
    pub first: u16,
    pub last: u16,
    pub path: PathBuf,
}

/// Probe drives and, when the choice is unambiguous, load the disc. A
/// remembered drive wins; otherwise a single ready disc loads straight in and
/// several ready discs are left for the picker.
pub fn detect_and_load(tx: Sender<WorkerEvent>, preferred: Option<PathBuf>) {
    std::thread::spawn(move || {
        let drives = packrat_core::drives::list();
        // An unmounted disc is read through the device's own filesystem, so a
        // mount is not what makes a disc ready.
        // The remembered drive is asked directly rather than trusted to the
        // cached size: it is the drive this window is expected to use, and a
        // disc that went in since the kernel last looked reads as absent there.
        if let Some(device) = &preferred {
            if matches!(
                packrat_core::drives::media(device),
                packrat_core::drives::Media::Present { .. }
            ) {
                let drive = drives
                    .iter()
                    .find(|drive| &drive.device == device)
                    .cloned()
                    .unwrap_or_else(|| OpticalDrive {
                        device: device.clone(),
                        mount: None,
                        has_disc: true,
                    });
                if let Err(message) = load_drive_inner(tx.clone(), drive) {
                    let _ = tx.send(WorkerEvent::DiscError(message));
                }
                return;
            }
        }

        let ready: Vec<OpticalDrive> = drives
            .iter()
            .filter(|drive| drive.has_disc)
            .cloned()
            .collect();
        let chosen = (ready.len() == 1).then(|| ready[0].clone());

        match chosen {
            Some(drive) => {
                if let Err(message) = load_drive_inner(tx.clone(), drive) {
                    let _ = tx.send(WorkerEvent::DiscError(message));
                }
            }
            None => {
                let _ = tx.send(WorkerEvent::Drives(drives));
            }
        }
    });
}

/// Load the drive the user picked from the list.
pub fn load_drive(tx: Sender<WorkerEvent>, device: PathBuf, mount: Option<PathBuf>) {
    std::thread::spawn(move || {
        if let Err(message) = load_drive_inner(
            tx.clone(),
            OpticalDrive {
                device,
                mount,
                has_disc: true,
            },
        ) {
            let _ = tx.send(WorkerEvent::DiscError(message));
        }
    });
}

/// Read and classify a disc at a mounted path rather than a drive.
pub fn load_path(tx: Sender<WorkerEvent>, path: PathBuf) {
    std::thread::spawn(move || match load_disc(&path, None) {
        Ok(loaded) => {
            let _ = tx.send(WorkerEvent::DiscReady {
                source: loaded.source,
                disc: loaded.disc,
                classification: loaded.classification,
                label: loaded.label,
                device: None,
            });
        }
        Err(e) => {
            let _ = tx.send(WorkerEvent::DiscError(e));
        }
    });
}

/// Refresh the drive list without loading anything.
pub fn list_drives(tx: Sender<WorkerEvent>) {
    std::thread::spawn(move || {
        let _ = tx.send(WorkerEvent::Drives(packrat_core::drives::list()));
    });
}

/// Summarise the configured destinations for the header, reporting the free
/// space of whichever destination exists. Runs off the UI thread because the
/// walk touches every folder in the library.
pub fn scan_stats(tx: Sender<WorkerEvent>, tv_dir: Option<PathBuf>, movie_dir: Option<PathBuf>) {
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
/// Read a drive's disc and announce it, reporting why if it cannot be read.
///
/// The error comes back rather than going straight out as an event, because the
/// caller knows whether a failure is worth retrying: one attempt after a disc
/// goes in fails often, and the watcher retries.
fn load_drive_inner(tx: Sender<WorkerEvent>, drive: OpticalDrive) -> Result<(), String> {
    // The mount is a convenience, not a requirement: an unmounted disc is read
    // through the device's own filesystem.
    let loaded = load_device(&drive.device, drive.mount.as_deref())?;
    let _ = tx.send(WorkerEvent::DiscReady {
        source: loaded.source,
        disc: loaded.disc,
        classification: loaded.classification,
        label: loaded.label,
        device: Some(drive.device),
    });
    Ok(())
}

/// Eject a drive on a worker thread, reporting the result through `tx`.
///
/// The ioctl blocks, so it never runs on the UI thread.
/// Search a metadata provider for titles matching `query`.
pub fn search_titles(tx: Sender<WorkerEvent>, movies: bool, query: String, year: Option<u16>) {
    std::thread::spawn(move || {
        let event = match crate::naming::search_titles(movies, &query, year) {
            Ok(matches) => WorkerEvent::TitleMatches(matches),
            Err(message) => WorkerEvent::TitleSearchFailed(message),
        };
        let _ = tx.send(event);
    });
}

/// What looking at a drive found.
pub enum DriveState {
    /// No disc in it, or no such drive.
    Empty,
    /// A disc is in it, of `bytes` — 0 where the platform cannot say — and this
    /// window has it, or already had it.
    Present { bytes: u64 },
    /// A disc is in it that could not be read this time.
    Unreadable { bytes: u64, message: String },
}

/// Look at `device`, and read its disc when `load` asks for one.
///
/// This is the whole of what the old Rescan button did, run on a timer: a disc
/// that has just gone into a drive is often not readable for a moment while the
/// drive spins it up, so the caller retries. The presence check reads
/// `/sys/block` rather than the drive, so asking is cheap and cannot stall on a
/// disc the drive is busy with.
pub fn watch_drive(tx: Sender<WorkerEvent>, device: PathBuf, load: bool) {
    std::thread::spawn(move || {
        // Working from the drive list is how the mount is found, and sending
        // it back is how a window that is pinned to one drive still learns
        // about the others — without that, the list it offers is frozen at
        // whatever it was when the window pinned one.
        let drives = packrat_core::drives::list();

        // Asked of the drive rather than read from `/sys/block`: a disc that
        // went in since the kernel last looked reads as absent there, and a
        // watcher that trusts it never sees the disc at all.
        let media = packrat_core::drives::media(&device);
        let state = match media {
            // Only a drive that says it is empty is a reason not to try. One
            // that will not say is not the same thing, and a read answers
            // better than the status does.
            Media::Empty => DriveState::Empty,
            Media::Unavailable(message) => DriveState::Unreadable { bytes: 0, message },
            Media::Present { .. } | Media::Unsure(_) if !load => DriveState::Present {
                bytes: media.bytes(),
            },
            _ => {
                // The mount is only a convenience, so the drive is looked up
                // again for one; a drive with none is read from the device.
                let drive = drives
                    .iter()
                    .find(|drive| drive.device == device)
                    .cloned()
                    .unwrap_or(OpticalDrive {
                        device: device.clone(),
                        mount: None,
                        has_disc: true,
                    });
                match load_drive_inner(tx.clone(), drive) {
                    Ok(()) => DriveState::Present {
                        bytes: media.bytes(),
                    },
                    Err(message) => DriveState::Unreadable {
                        bytes: media.bytes(),
                        message,
                    },
                }
            }
        };
        let _ = tx.send(WorkerEvent::DriveWatched {
            device,
            state,
            drives,
        });
    });
}

pub fn eject_drive(tx: Sender<WorkerEvent>, device: PathBuf) {
    std::thread::spawn(move || {
        let result = packrat_core::drives::eject(&device);
        let _ = tx.send(WorkerEvent::EjectDone { result });
    });
}

/// Rip the queued files, reporting progress and completion through `tx`.
///
/// `cancel` stops between files; `abort` also stops the file in progress.
pub fn spawn_rip(
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

        // A title set's IFO cannot change during a run. On a raw-device source,
        // re-reading it walks the whole disc filesystem each time, and nothing
        // in that path checks the stop flag — so it is both slow and
        // uninterruptible. Read each title set once.
        let mut vts_cache: HashMap<u8, VtsIfo> = HashMap::new();

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
                    let vts = match vts_cache.get(&title.vts) {
                        Some(vts) => Ok(vts.clone()),
                        None => read_vts(&source, title.vts).inspect(|vts| {
                            vts_cache.insert(title.vts, vts.clone());
                        }),
                    };
                    vts.and_then(|vts| {
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
        // Drop the tray lock before the UI sees the result, so the picker can
        // immediately eject the disc or load another one.
        drop(_tray);
        let _ = tx.send(WorkerEvent::RipDone { cancelled: false });
    })
}

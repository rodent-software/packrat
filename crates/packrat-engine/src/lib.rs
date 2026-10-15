//! Terminal-independent orchestration for `packrat`.
//!
//! This crate owns everything between the disc I/O in `packrat-core` and a
//! frontend: user preferences, matching a disc against metadata providers,
//! turning the result into the list of files to write, and the background
//! worker threads that do the slow work.
//!
//! It deliberately has no opinion about presentation, so the graphical
//! application (and any future frontend) can share the same decisions.

pub mod config;
pub mod disc;
pub mod history;
pub mod job;
pub mod naming;
pub mod worker;

pub use config::Config;
pub use disc::{disc_fingerprint, load_device, load_disc, LoadedDisc};
pub use history::History;
pub use job::{plan_jobs, ExtrasMode, JobKind, PlanJob, PlanOptions};
pub use naming::{
    resolve_movie_naming, resolve_naming, MovieNaming, Naming, Resolved, ResolvedMovie, TitleMatch,
};
pub use worker::{
    detect_and_load, eject_drive, list_drives, load_drive, load_path, scan_stats, search_titles,
    spawn_rip, watch_drive, DriveState, RipJob, WorkerEvent,
};

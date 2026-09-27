//! Core library for `packrat`.
//!
//! This crate owns everything that is not the command-line surface: locating a
//! disc, modelling its titles, deciding what the disc is, and eventually
//! demuxing and remuxing it into a Plex-friendly MKV.

pub mod detect;
pub mod device;
pub mod disc;
pub mod drives;
#[cfg(feature = "dvdcss")]
pub mod dvdcss;
pub mod error;
pub mod identify;
pub mod library;
pub mod meta;
pub mod remux;
pub mod source;

pub use detect::{
    alternates, chapter_pattern, classify, preferred_titles, split_title, AlternateSet,
    ChapterPattern, Classification, DiscKind, Segment, EPISODE_MAX, EPISODE_MIN, EXTRA_MIN,
    MIN_CONTENT,
};
pub use disc::{read_disc, read_vts, DiscModel, Title};
pub use drives::OpticalDrive;
pub use error::DiscError;
pub use identify::{parse_label, LabelInfo};
pub use library::{
    episode_file, episode_file_in, episode_range_file, episode_range_file_in, extra_file,
    extra_file_in, movie_dir_in, movie_file, movie_file_in, sanitize_component, season_dir,
    season_dir_in, show_dir, show_dir_in,
};
pub use meta::{
    match_episodes, match_span, search_show, Episode, EpisodeMatch, EpisodeSpan, MetaError, Show,
};
pub use remux::{
    remux_chain, remux_chain_with_progress, remux_chain_with_progress_and_cancel, remux_chapters,
    remux_chapters_with_reader, remux_title, RemuxPhase, RemuxProgress, RemuxReport,
};
pub use source::DiscSource;

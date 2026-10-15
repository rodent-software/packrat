//! Opening a disc for reading.
//!
//! Everything here is frontend-agnostic: it takes paths and returns the disc
//! model, with no terminal or UI assumptions.

use std::path::Path;

use packrat_core::{
    classify, parse_label, read_disc, Classification, DiscModel, DiscSource, LabelInfo,
};

/// A disc that has been read and classified.
pub struct LoadedDisc {
    pub source: DiscSource,
    pub disc: DiscModel,
    pub classification: Classification,
    pub label: LabelInfo,
}

/// Read and classify a disc at a mounted path or folder.
///
/// Only reads from a raw device if it can actually be opened: a detected drive
/// whose device node is missing or unreadable still has a usable mount, and
/// falling back to it beats failing the whole disc.
pub fn load_disc(path: &Path, device: Option<&Path>) -> Result<LoadedDisc, String> {
    let usable_device = device.filter(|dev| packrat_core::drives::open_device(dev).is_ok());
    let source = match usable_device {
        Some(dev) => DiscSource::discover_device(dev, Some(path)),
        None => DiscSource::discover(path),
    }
    .map_err(|e| e.to_string())?;
    finish(source)
}

/// Read and classify the disc in an optical drive.
///
/// The mount is used when the desktop made one; when it did not, the disc's own
/// filesystem is read off the device, so an automounter is never required.
pub fn load_device(device: &Path, mount: Option<&Path>) -> Result<LoadedDisc, String> {
    let source = DiscSource::discover_device(device, mount).map_err(|e| e.to_string())?;
    finish(source)
}

fn finish(source: DiscSource) -> Result<LoadedDisc, String> {
    let disc = read_disc(&source).map_err(|e| e.to_string())?;
    let classification = classify(&disc);
    let label = parse_label(&disc.volume_id);
    Ok(LoadedDisc {
        source,
        disc,
        classification,
        label,
    })
}

/// A cheap identity for a mounted disc, used to ignore background re-scans of
/// the disc that is already loaded — resetting the plan on every poll would
/// otherwise discard the user's edits and selections.
pub fn disc_fingerprint(source: &DiscSource, disc: &DiscModel) -> String {
    format!(
        "{}|{}|{}|{}",
        source.root().display(),
        disc.provider_id,
        disc.titles.len(),
        disc.total_duration().as_secs()
    )
}

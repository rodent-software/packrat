//! Where the bytes of a disc come from.
//!
//! Two shapes are supported: a mounted DVD-Video folder (or an extracted copy
//! of one), and a raw optical device paired with a mount for its IFO
//! structure. The device form is what CSS decryption needs.

use std::path::{Path, PathBuf};

use crate::error::DiscError;

/// A location whose `VIDEO_TS` contents can be read.
#[derive(Debug, Clone)]
pub enum DiscSource {
    /// A mounted DVD-Video disc or an extracted folder containing `VIDEO_TS/`.
    Folder { root: PathBuf, video_ts: PathBuf },
    /// A raw optical device (`/dev/sr0`) plus a mount of the same disc, which
    /// supplies the (unencrypted) IFO files.
    Device {
        root: PathBuf,
        video_ts: PathBuf,
        device: PathBuf,
    },
}

impl DiscSource {
    /// Discover a folder-backed source from a path.
    ///
    /// Accepts either a disc root that contains `VIDEO_TS/`, or the `VIDEO_TS`
    /// directory itself.
    pub fn discover(path: impl AsRef<Path>) -> Result<Self, DiscError> {
        let path = path.as_ref();

        if path.is_dir() {
            let looks_like_video_ts = path
                .file_name()
                .map(|n| n.eq_ignore_ascii_case("VIDEO_TS"))
                .unwrap_or(false);

            if looks_like_video_ts {
                let root = path.parent().unwrap_or(path).to_path_buf();
                return Ok(Self::Folder {
                    root,
                    video_ts: path.to_path_buf(),
                });
            }

            let video_ts = path.join("VIDEO_TS");
            if video_ts.is_dir() {
                return Ok(Self::Folder {
                    root: path.to_path_buf(),
                    video_ts,
                });
            }
        }

        Err(DiscError::NoVideoTs(path.display().to_string()))
    }

    /// Build a device-backed source: `device` is read for VOB sectors (through
    /// libdvdcss when the feature is enabled) and `mount` supplies the IFO
    /// structure.
    pub fn discover_device(
        device: impl AsRef<Path>,
        mount: impl AsRef<Path>,
    ) -> Result<Self, DiscError> {
        let device = device.as_ref().to_path_buf();
        let mount = mount.as_ref();

        let (root, video_ts) = if mount
            .file_name()
            .map(|n| n.eq_ignore_ascii_case("VIDEO_TS"))
            .unwrap_or(false)
        {
            (
                mount.parent().unwrap_or(mount).to_path_buf(),
                mount.to_path_buf(),
            )
        } else {
            (mount.to_path_buf(), mount.join("VIDEO_TS"))
        };

        if !video_ts.is_dir() {
            return Err(DiscError::NoVideoTs(mount.display().to_string()));
        }

        Ok(Self::Device {
            root,
            video_ts,
            device,
        })
    }

    /// The disc's root directory.
    pub fn root(&self) -> &Path {
        match self {
            Self::Folder { root, .. } | Self::Device { root, .. } => root,
        }
    }

    /// The `VIDEO_TS` directory.
    pub fn video_ts(&self) -> &Path {
        match self {
            Self::Folder { video_ts, .. } | Self::Device { video_ts, .. } => video_ts,
        }
    }

    /// The raw device, for sources that have one.
    pub fn device(&self) -> Option<&Path> {
        match self {
            Self::Folder { .. } => None,
            Self::Device { device, .. } => Some(device),
        }
    }

    /// All `VTS_xx_0.IFO` files, sorted by title-set number.
    pub fn vts_ifos(&self) -> Result<Vec<PathBuf>, DiscError> {
        let mut out = Vec::new();
        let video_ts = self.video_ts();

        let entries = std::fs::read_dir(video_ts).map_err(|source| DiscError::Io {
            path: video_ts.to_path_buf(),
            source,
        })?;

        for entry in entries {
            let entry = entry.map_err(|source| DiscError::Io {
                path: video_ts.to_path_buf(),
                source,
            })?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let upper = name.to_ascii_uppercase();
            if upper.starts_with("VTS_")
                && upper.ends_with("_0.IFO")
                && upper.len() == "VTS_00_0.IFO".len()
            {
                out.push(entry.path());
            }
        }

        out.sort();
        Ok(out)
    }

    /// The main `VIDEO_TS.IFO`, if present.
    pub fn main_ifo(&self) -> Option<PathBuf> {
        let p = self.video_ts().join("VIDEO_TS.IFO");
        p.is_file().then_some(p)
    }
}

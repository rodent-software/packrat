//! Discovering optical drives and the discs in them.
//!
//! Linux is implemented via sysfs and `/proc/mounts`. macOS (DiskArbitration
//! / `drutil`) and Windows (`GetLogicalDrives` + `GetDriveTypeW`) are stubbed
//! for now; they return no drives rather than guessing.

use std::path::PathBuf;

/// An optical drive and, if there is one, the disc in it.
#[derive(Debug, Clone)]
pub struct OpticalDrive {
    /// Device node, e.g. `/dev/sr0` or `\\.\D:`.
    pub device: PathBuf,
    /// Where the disc is mounted, if the OS mounted it.
    pub mount: Option<PathBuf>,
    /// Whether media is present.
    pub has_disc: bool,
}

impl OpticalDrive {
    /// The mounted disc's label, taken from the mount point's directory name.
    pub fn label(&self) -> Option<String> {
        self.mount
            .as_ref()
            .and_then(|m| m.file_name())
            .map(|n| n.to_string_lossy().into_owned())
    }
}

/// All optical drives on the system.
#[cfg(target_os = "linux")]
pub fn list() -> Vec<OpticalDrive> {
    let mounts = linux_mounts();
    let mut drives = Vec::new();

    let Ok(entries) = std::fs::read_dir("/sys/block") else {
        return drives;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("sr") {
            continue;
        }
        // Only real devices, not partitions or virtual nodes.
        if !entry.path().join("device").exists() {
            continue;
        }
        let device = PathBuf::from(format!("/dev/{name}"));
        let size = std::fs::read_to_string(entry.path().join("size"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);

        drives.push(OpticalDrive {
            mount: mounts.get(&device).cloned(),
            device,
            has_disc: size > 0,
        });
    }

    drives.sort_by(|a, b| a.device.cmp(&b.device));
    drives
}

#[cfg(target_os = "linux")]
fn linux_mounts() -> std::collections::HashMap<PathBuf, PathBuf> {
    let mut map = std::collections::HashMap::new();
    let Ok(contents) = std::fs::read_to_string("/proc/mounts") else {
        return map;
    };
    for line in contents.lines() {
        let mut parts = line.split_whitespace();
        let (Some(source), Some(target)) = (parts.next(), parts.next()) else {
            continue;
        };
        if !source.starts_with("/dev/") {
            continue;
        }
        map.insert(
            PathBuf::from(unescape(source)),
            PathBuf::from(unescape(target)),
        );
    }
    map
}

/// `/proc/mounts` octal-escapes spaces, tabs, newlines and backslashes.
#[cfg(target_os = "linux")]
fn unescape(input: &str) -> String {
    input
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

#[cfg(target_os = "macos")]
pub fn list() -> Vec<OpticalDrive> {
    use std::process::Command;

    let Ok(listing) = Command::new("diskutil").arg("list").output() else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&listing.stdout);

    let mut drives = Vec::new();
    // Each whole disk starts a block like `/dev/disk4 (external, physical):`.
    for block in text.split("\n/dev/").skip(1) {
        let disk = block
            .lines()
            .next()
            .unwrap_or("")
            .trim_end_matches(':')
            .trim();
        if disk.is_empty() {
            continue;
        }
        let is_optical = block.lines().any(|line| {
            let upper = line.to_ascii_uppercase();
            upper.contains("CD-ROM") || upper.contains("DVD") || upper.contains("BLU-RAY")
        });
        if !is_optical {
            continue;
        }

        let device = PathBuf::from(format!("/dev/{disk}"));
        let (mount, has_disc) = diskutil_info(disk);
        drives.push(OpticalDrive {
            device,
            mount,
            has_disc,
        });
    }
    drives
}

/// Read a disk's mount point and media presence from `diskutil info`.
#[cfg(target_os = "macos")]
fn diskutil_info(disk: &str) -> (Option<PathBuf>, bool) {
    use std::process::Command;

    let Ok(output) = Command::new("diskutil").args(["info", disk]).output() else {
        return (None, false);
    };
    let text = String::from_utf8_lossy(&output.stdout);

    let mut mount = None;
    let mut has_disc = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Mount Point:") {
            let value = rest.trim();
            if !value.is_empty() && !value.eq_ignore_ascii_case("Not mounted") {
                mount = Some(PathBuf::from(value));
            }
        }
        if trimmed.starts_with("Optical Drive:") && trimmed.ends_with("Yes") {
            has_disc = true;
        }
        if trimmed.starts_with("Disk Size:") {
            has_disc = true;
        }
    }
    (mount, has_disc)
}

#[cfg(target_os = "windows")]
pub fn list() -> Vec<OpticalDrive> {
    use std::iter;
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetLogicalDrives() -> u32;
        fn GetDriveTypeW(root: *const u16) -> u32;
    }

    /// `DRIVE_CDROM` from `GetDriveTypeW`.
    const DRIVE_CDROM: u32 = 5;

    let mask = unsafe { GetLogicalDrives() };
    let mut drives = Vec::new();

    for index in 0..26u32 {
        if mask & (1 << index) == 0 {
            continue;
        }
        let letter = (b'A' + index as u8) as char;
        let root = format!("{letter}:\\");
        let wide: Vec<u16> = std::ffi::OsStr::new(&root)
            .encode_wide()
            .chain(iter::once(0))
            .collect();
        if unsafe { GetDriveTypeW(wide.as_ptr()) } != DRIVE_CDROM {
            continue;
        }

        let mount = PathBuf::from(&root);
        let has_disc = mount.exists();
        drives.push(OpticalDrive {
            device: PathBuf::from(format!("\\\\.\\{letter}:")),
            mount: has_disc.then_some(mount),
            has_disc,
        });
    }

    drives
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn list() -> Vec<OpticalDrive> {
    // Unsupported platform: report nothing rather than guessing.
    Vec::new()
}

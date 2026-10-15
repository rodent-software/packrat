//! Discovering optical drives and the discs in them, and working the tray.
//!
//! Linux reads drives from sysfs and `/proc/mounts`, and controls the tray
//! with the kernel CD-ROM ioctls. macOS drives are found and ejected through
//! `diskutil`; Windows uses `GetLogicalDrives`/`GetDriveTypeW` and the Win32
//! device APIs. Unsupported platforms report no drives rather than guessing.

use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// An optical drive and, if there is one, the disc in it.
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Open an optical device for control ioctls without letting the kernel close
/// an open tray.
///
/// Linux's `autoclose` (on by default) closes the tray when `/dev/sr*` is
/// opened in blocking mode, so a background probe of a drive whose tray the
/// user just opened would snap it shut. Every open of a raw device goes
/// through here with `O_NONBLOCK`; block-device reads ignore the flag.
pub fn open_device(device: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    options.open(device)
}

/// Like [`open_device`], but writable, for the occasional bridge that only
/// accepts an eject command on a read-write handle.
pub fn open_device_writable(device: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    options.open(device)
}

/// Holds an optical drive's tray closed for as long as it lives.
///
/// A rip takes one of these so a stray eject cannot interrupt the read; it is
/// released when the guard is dropped. Platforms with no software tray lock
/// yield a guard that does nothing, so a rip is never blocked waiting on a
/// lock the OS cannot provide.
pub struct TrayLock {
    #[cfg(target_os = "linux")]
    file: Option<std::fs::File>,
    #[cfg(target_os = "windows")]
    handle: Option<win::Handle>,
}

impl Drop for TrayLock {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(file) = self.file.take() {
            let _ = linux_ioctl(&file, CDROM_LOCKDOOR, 0);
        }
        #[cfg(target_os = "windows")]
        if let Some(handle) = self.handle.take() {
            win::close(handle);
        }
    }
}

/// Lock `device`'s tray closed for the guard's lifetime, where supported.
///
/// A failure to lock is not fatal: the caller simply keeps reading without a
/// lock, since refusing to rip over it would be worse.
pub fn lock_tray(device: &Path) -> TrayLock {
    #[cfg(target_os = "linux")]
    {
        let file = open_device(device)
            .ok()
            .filter(|file| linux_ioctl(file, CDROM_LOCKDOOR, 1).is_ok());
        TrayLock { file }
    }
    #[cfg(target_os = "windows")]
    {
        TrayLock {
            handle: win::lock(device),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = device;
        TrayLock {}
    }
}

/// Ask the drive to open its tray.
pub fn eject(device: &Path) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        linux_eject(device)
    }
    #[cfg(target_os = "macos")]
    {
        macos_eject(device)
    }
    #[cfg(target_os = "windows")]
    {
        win::eject(device)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = device;
        Err("ejecting is not supported on this platform".into())
    }
}

/// `CDROMEJECT` from `<linux/cdrom.h>`.
#[cfg(target_os = "linux")]
const CDROMEJECT: std::ffi::c_ulong = 0x5309;
/// `CDROM_DRIVE_STATUS` — `_IOR('c', 0x5A, int)`. Asks the drive for its own
/// answer about the tray and the media, rather than the kernel's cached idea.
///
/// Despite the `_IOR`, the answer comes back as the ioctl's *return value*, not
/// through the argument: the argument is the slot number to ask about.
#[cfg(target_os = "linux")]
const CDROM_DRIVE_STATUS: std::ffi::c_ulong = 0x8004_635A;
/// `CDSL_CURRENT` — ask about the disc that is in the drive now. A slot number,
/// which is what this ioctl's argument is, not a pointer.
#[cfg(target_os = "linux")]
const CDSL_CURRENT: std::ffi::c_ulong = u64::MAX;
/// `CDS_NO_DISC` from `CDROM_DRIVE_STATUS`.
#[cfg(target_os = "linux")]
const CDS_NO_DISC: i32 = 1;
/// `CDS_TRAY_OPEN`.
#[cfg(target_os = "linux")]
const CDS_TRAY_OPEN: i32 = 2;
/// `CDS_DRIVE_NOT_READY` — a disc is there, but the drive is not ready to be
/// read yet. This is what a drive says while it spins up, and it is not an
/// empty drive.
#[cfg(target_os = "linux")]
const CDS_DRIVE_NOT_READY: i32 = 3;
/// `CDS_DISC_OK`.
#[cfg(target_os = "linux")]
const CDS_DISC_OK: i32 = 4;
/// `CDROM_LOCKDOOR` from `<linux/cdrom.h>`.
#[cfg(target_os = "linux")]
const CDROM_LOCKDOOR: std::ffi::c_ulong = 0x5329;

#[cfg(target_os = "linux")]
extern "C" {
    fn ioctl(fd: std::ffi::c_int, request: std::ffi::c_ulong, ...) -> std::ffi::c_int;
}

#[cfg(target_os = "linux")]
fn linux_ioctl(
    file: &std::fs::File,
    request: std::ffi::c_ulong,
    arg: std::ffi::c_ulong,
) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;

    if unsafe { ioctl(file.as_raw_fd(), request, arg) } == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Eject via the kernel ioctl, falling back to the command-line tools that
/// unmount first and use the desktop's privilege service.
///
/// Every failed step is kept so the UI can show *why* a drive refused to open,
/// which matters most for USB bridges that do not implement the SCSI eject
/// command.
#[cfg(target_os = "linux")]
fn linux_eject(device: &Path) -> Result<(), String> {
    let mut reasons: Vec<String> = Vec::new();

    // Some bridges only accept the command on a read-write handle.
    let candidates = [
        ("read-only", open_device(device).map_err(|e| e.to_string())),
        (
            "read-write",
            open_device_writable(device).map_err(|e| e.to_string()),
        ),
    ];
    for (access, file) in candidates {
        match file {
            Ok(file) => {
                // Clear a door lock left behind by a previous run or another
                // application; the kernel refuses CDROMEJECT while one is set.
                let _ = linux_ioctl(&file, CDROM_LOCKDOOR, 0);
                match linux_ioctl(&file, CDROMEJECT, 0) {
                    Ok(()) => return Ok(()),
                    Err(e) => reasons.push(format!("CDROMEJECT ({access}): {e}")),
                }
            }
            Err(e) => reasons.push(format!("open {access}: {e}")),
        }
    }

    let path = device.to_string_lossy().into_owned();

    // `eject` unmounts first and then issues the same ioctl, so it succeeds
    // where a mounted volume made the direct ioctl fail.
    match std::process::Command::new("eject")
        .arg("-v")
        .arg(&path)
        .output()
    {
        Ok(output) if output.status.success() => return Ok(()),
        Ok(output) => reasons.push(format!("eject: {}", command_detail(&output))),
        Err(e) => reasons.push(format!("eject: {e}")),
    }

    // udisks has no `eject` verb; let its privilege service unmount the volume,
    // then retry the ioctl on the now-free device.
    match std::process::Command::new("udisksctl")
        .args(["unmount", "-b", &path])
        .output()
    {
        Ok(output) if output.status.success() => match open_device(device) {
            Ok(file) => match linux_ioctl(&file, CDROMEJECT, 0) {
                Ok(()) => return Ok(()),
                Err(e) => reasons.push(format!("CDROMEJECT after udisks unmount: {e}")),
            },
            Err(e) => reasons.push(format!("open after udisks unmount: {e}")),
        },
        Ok(output) => reasons.push(format!("udisksctl unmount: {}", command_detail(&output))),
        Err(e) => reasons.push(format!("udisksctl: {e}")),
    }

    // A different command path through SG_IO, which sometimes works where the
    // block ioctl is refused.
    match std::process::Command::new("sg_start")
        .args(["--eject", &path])
        .output()
    {
        Ok(output) if output.status.success() => return Ok(()),
        Ok(output) => reasons.push(format!("sg_start: {}", command_detail(&output))),
        Err(e) => reasons.push(format!("sg_start: {e}")),
    }

    Err(reasons.join("; "))
}

/// The most useful line from a failed helper's output.
#[cfg(target_os = "linux")]
fn command_detail(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = stderr.trim();
    if text.is_empty() {
        format!("exited with {}", output.status)
    } else {
        text.lines().last().unwrap_or(text).trim().to_string()
    }
}

#[cfg(target_os = "macos")]
fn macos_eject(device: &Path) -> Result<(), String> {
    let status = std::process::Command::new("diskutil")
        .arg("eject")
        .arg(device)
        .status()
        .map_err(|e| format!("could not run diskutil: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("could not eject {}", device.display()))
    }
}

#[cfg(target_os = "windows")]
mod win {
    use std::ffi::c_void;
    use std::iter;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr;

    pub type Handle = *mut c_void;

    const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const OPEN_EXISTING: u32 = 3;
    const IOCTL_STORAGE_EJECT_MEDIA: u32 = 0x0002_D808;
    const FSCTL_LOCK_VOLUME: u32 = 0x0009_0018;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            attributes: *mut c_void,
            creation: u32,
            flags: u32,
            template: Handle,
        ) -> Handle;
        fn DeviceIoControl(
            device: Handle,
            control: u32,
            in_buffer: *mut c_void,
            in_size: u32,
            out_buffer: *mut c_void,
            out_size: u32,
            returned: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
    }

    fn open(device: &Path, access: u32) -> Option<Handle> {
        let wide: Vec<u16> = device
            .as_os_str()
            .encode_wide()
            .chain(iter::once(0))
            .collect();
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null_mut(),
                OPEN_EXISTING,
                0,
                ptr::null_mut(),
            )
        };
        (handle != INVALID_HANDLE_VALUE).then_some(handle)
    }

    fn ioctl(handle: Handle, control: u32) -> bool {
        let mut returned = 0u32;
        unsafe {
            DeviceIoControl(
                handle,
                control,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                0,
                &mut returned,
                ptr::null_mut(),
            ) != 0
        }
    }

    pub fn eject(device: &Path) -> Result<(), String> {
        let handle = open(device, GENERIC_READ)
            .or_else(|| open(device, GENERIC_READ | GENERIC_WRITE))
            .ok_or_else(|| format!("opening {}", device.display()))?;
        let ejected = ioctl(handle, IOCTL_STORAGE_EJECT_MEDIA);
        close(handle);
        if ejected {
            Ok(())
        } else {
            Err(format!("ejecting {}", device.display()))
        }
    }

    pub fn lock(device: &Path) -> Option<Handle> {
        let handle = open(device, GENERIC_READ | GENERIC_WRITE)?;
        if !ioctl(handle, FSCTL_LOCK_VOLUME) {
            close(handle);
            return None;
        }
        Some(handle)
    }

    pub fn close(handle: Handle) {
        unsafe {
            CloseHandle(handle);
        }
    }
}

/// What a drive says about its media.
///
/// The distinction that matters is between a drive that says it is empty and
/// one that will not say anything, because only the first is a reason not to
/// try reading it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Media {
    /// The drive holds a disc. `bytes` is 0 where the size is not known.
    Present { bytes: u64 },
    /// The drive answered that there is no disc in it.
    Empty,
    /// The drive would not say: still spinning up, or no usable answer. A disc
    /// may well be there, so the read is the only thing that settles it.
    Unsure(String),
    /// The device could not be opened at all, so a read cannot be tried either.
    Unavailable(String),
}

impl Media {
    /// The size of the disc, where it is known.
    pub fn bytes(&self) -> u64 {
        match self {
            Self::Present { bytes } => *bytes,
            _ => 0,
        }
    }
}

/// Ask `device` what media it holds.
///
/// Asking the drive is the point rather than an overhead. Two things go wrong
/// when the kernel's cached idea is used instead:
///
/// - It is only refreshed when something opens the drive — `/sys/block/sr0/`
///   `events_poll_msecs` is -1, so it does not poll — and a disc that went in
///   since reads as absent for as long as nothing asks.
/// - A disc that is there but not yet spun up is not the same as an empty
///   drive, and only the drive can tell the two apart.
///
/// So `CDROM_DRIVE_STATUS` is asked first: it makes the drive answer for
/// itself, and it says "not ready" when that is what is happening. The size is
/// then a bonus, and the kernel's capacity is the fallback where the drive will
/// not answer at all. The open is non-blocking for the same reason every other
/// one is: it must not make the kernel close the tray.
pub fn media(device: &Path) -> Media {
    #[cfg(unix)]
    {
        let mut file = match open_device(device) {
            Ok(file) => file,
            Err(e) => {
                return Media::Unavailable(format!("cannot open {}: {e}", device.display()));
            }
        };

        // A capacity is only good news: it means a disc is there. Zero means
        // the kernel has not been told yet, not that the drive is empty.
        let bytes = file.seek(SeekFrom::End(0)).unwrap_or(0);

        #[cfg(target_os = "linux")]
        {
            use std::os::unix::io::AsRawFd;

            // The status is the return value; a negative one is an error.
            let status = unsafe { ioctl(file.as_raw_fd(), CDROM_DRIVE_STATUS, CDSL_CURRENT) };
            match status {
                CDS_DISC_OK => return Media::Present { bytes },
                CDS_DRIVE_NOT_READY => {
                    return Media::Unsure("the drive is not ready yet".into());
                }
                // Only both sources agreeing that there is nothing means there
                // is nothing: a drive that says "no disc" while the kernel has
                // a capacity for one is a drive to try reading, not to skip.
                CDS_NO_DISC | CDS_TRAY_OPEN if bytes == 0 => return Media::Empty,
                // `CDS_NO_INFO`, an error, and a disagreement all fall through.
                _ => {}
            }
        }

        if bytes > 0 {
            Media::Present { bytes }
        } else {
            Media::Unsure("the drive did not say".into())
        }
    }
    #[cfg(not(unix))]
    {
        // Windows and the rest already ask the drive when they list drives, so
        // their answer is the live one — but not a size, hence the 0.
        if list()
            .into_iter()
            .any(|drive| drive.device == device && drive.has_disc)
        {
            Media::Present { bytes: 0 }
        } else {
            Media::Empty
        }
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::io::AsRawFd;
    use std::path::Path;

    /// A drive that cannot be asked is not an empty drive, and saying so is the
    /// difference between "put a disc in" and "this drive is not answering".
    /// Needs a real drive to prove the asking; this pins the failure side.
    #[test]
    fn a_device_that_cannot_be_opened_is_not_an_empty_one() {
        let answer = media(Path::new("/dev/packrat-no-such-drive"));

        match answer {
            Media::Unavailable(reason) => assert!(
                reason.contains("packrat-no-such-drive"),
                "the reason should name the drive: {reason}"
            ),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    /// The point of [`open_device`] is that the kernel does not autoclose an
    /// open tray; it only does that for blocking opens, so the flag must be
    /// set on the file we hand out.
    #[test]
    fn open_device_sets_the_nonblocking_flag() {
        let file = open_device(Path::new("/dev/null")).expect("open /dev/null");
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0, "F_GETFL failed");
        assert_ne!(flags & libc::O_NONBLOCK, 0, "O_NONBLOCK was not set");
    }
}

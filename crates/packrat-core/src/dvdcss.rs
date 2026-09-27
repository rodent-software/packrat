//! Runtime loading of `libdvdcss` for CSS decryption on raw optical devices.
//!
//! packrat never links, bundles or ships `libdvdcss`: distribution of the
//! library is legally sensitive, so the copy is supplied by the user (usually
//! through their OS package manager) and found at runtime. When it is absent
//! the raw-device path still reads sectors directly, which is enough for
//! unencrypted discs. See `docs/installation.md` for why, and for how to
//! acquire it on each platform.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use libloading::Library;

use crate::error::DiscError;

/// `dvdcss_read` flag: decrypt the sectors we read.
const DVDCSS_READ_DECRYPT: c_int = 1 << 0;

/// Library file names to try, most specific first.
#[cfg(target_os = "windows")]
const LIBRARY_NAMES: &[&str] = &["libdvdcss-2.dll", "libdvdcss.dll", "dvdcss.dll"];
#[cfg(target_os = "macos")]
const LIBRARY_NAMES: &[&str] = &["libdvdcss.2.dylib", "libdvdcss.dylib"];
#[cfg(all(unix, not(target_os = "macos")))]
const LIBRARY_NAMES: &[&str] = &["libdvdcss.so.2", "libdvdcss.so"];

type FnOpen = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type FnClose = unsafe extern "C" fn(*mut c_void) -> c_int;
type FnSeek = unsafe extern "C" fn(*mut c_void, c_int, c_int) -> c_int;
type FnRead = unsafe extern "C" fn(*mut c_void, *mut c_void, c_int, c_int) -> c_int;
type FnError = unsafe extern "C" fn(*mut c_void) -> *const c_char;

/// The resolved libdvdcss entry points, and where they were loaded from.
struct DvdcssApi {
    open: FnOpen,
    close: FnClose,
    seek: FnSeek,
    read: FnRead,
    error: FnError,
    path: PathBuf,
}

impl DvdcssApi {
    /// Bind the entry points from a loaded library. On success the caller must
    /// keep `library` mapped for the life of the process; the function
    /// pointers are only valid while it stays loaded.
    unsafe fn bind(library: &Library, path: PathBuf) -> Result<Self, libloading::Error> {
        let open = *library.get::<FnOpen>(b"dvdcss_open\0")?;
        let close = *library.get::<FnClose>(b"dvdcss_close\0")?;
        let seek = *library.get::<FnSeek>(b"dvdcss_seek\0")?;
        let read = *library.get::<FnRead>(b"dvdcss_read\0")?;
        let error = *library.get::<FnError>(b"dvdcss_error\0")?;
        Ok(Self {
            open,
            close,
            seek,
            read,
            error,
            path,
        })
    }
}

/// Where CSS decryption is available, and from where.
#[derive(Debug, Clone)]
pub enum DvdcssStatus {
    /// A usable `libdvdcss` was found.
    Available { path: PathBuf },
    /// No usable `libdvdcss` was found; the reason is human-readable.
    Unavailable { reason: String },
}

impl DvdcssStatus {
    /// Whether a usable library was found.
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available { .. })
    }
}

/// The process-wide loaded library, or the reason it could not be loaded.
fn api() -> Result<&'static DvdcssApi, &'static str> {
    static API: OnceLock<Result<DvdcssApi, String>> = OnceLock::new();
    API.get_or_init(load).as_ref().map_err(String::as_str)
}

/// Report whether CSS decryption is available and where it came from.
pub fn status() -> DvdcssStatus {
    match api() {
        Ok(api) => DvdcssStatus::Available {
            path: api.path.clone(),
        },
        Err(reason) => DvdcssStatus::Unavailable {
            reason: reason.to_string(),
        },
    }
}

/// Candidate library locations, in priority order.
fn candidates() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();

    // 1. An explicit override: either the library itself or a directory of
    //    them. This lets a user point at an unpacked copy without touching the
    //    system.
    if let Some(value) = std::env::var_os("PACKRAT_DVDCSS") {
        let path = PathBuf::from(value);
        if path.is_dir() {
            out.extend(LIBRARY_NAMES.iter().map(|name| path.join(name)));
        } else {
            out.push(path);
        }
    }

    // 2. Beside the running executable, so a portable copy travels with it.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            out.extend(LIBRARY_NAMES.iter().map(|name| dir.join(name)));
        }
    }

    // 3. Well-known package-manager prefixes.
    for dir in library_dirs() {
        out.extend(LIBRARY_NAMES.iter().map(|name| dir.join(name)));
    }

    // 4. Bare sonames, resolved through the platform loader's own search path.
    out.extend(LIBRARY_NAMES.iter().map(PathBuf::from));

    out
}

#[cfg(target_os = "macos")]
fn library_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(prefix) = std::env::var_os("HOMEBREW_PREFIX") {
        dirs.push(PathBuf::from(prefix).join("lib"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/lib"));
    dirs.push(PathBuf::from("/usr/local/lib"));
    dirs
}

#[cfg(target_os = "windows")]
fn library_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    // VLC ships a libdvdcss alongside its player, so a user who already has it
    // installed can often go without a separate download.
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(base) = std::env::var_os(var) {
            dirs.push(PathBuf::from(base).join("VideoLAN").join("VLC"));
        }
    }
    dirs
}

#[cfg(all(unix, not(target_os = "macos")))]
fn library_dirs() -> Vec<PathBuf> {
    // `dlopen` also searches the loader's own paths; these help when the
    // library sits somewhere the loader does not cover by default.
    vec![
        PathBuf::from("/usr/lib"),
        PathBuf::from("/usr/lib64"),
        PathBuf::from("/usr/local/lib"),
        PathBuf::from("/lib"),
    ]
}

/// Try each candidate in turn and bind the first usable one.
fn load() -> Result<DvdcssApi, String> {
    load_from(&candidates())
}

fn load_from(candidates: &[PathBuf]) -> Result<DvdcssApi, String> {
    let mut attempts: Vec<String> = Vec::new();
    for candidate in candidates {
        match unsafe { Library::new(candidate) } {
            Ok(library) => match unsafe { DvdcssApi::bind(&library, candidate.clone()) } {
                Ok(api) => {
                    // Keep the library mapped for the rest of the process; the
                    // bound function pointers depend on it.
                    std::mem::forget(library);
                    return Ok(api);
                }
                Err(error) => {
                    attempts.push(format!("{}: missing symbol ({error})", candidate.display()));
                }
            },
            Err(error) => attempts.push(format!("{}: {error}", candidate.display())),
        }
    }

    Err(format!(
        "libdvdcss not found (tried {}). Install it for your platform, or set \
         PACKRAT_DVDCSS to the library path. Run `packrat doctor` for details.",
        attempts.join("; ")
    ))
}

/// An open libdvdcss handle.
pub struct Dvdcss {
    api: &'static DvdcssApi,
    handle: *mut c_void,
}

// The handle is only used from one thread at a time by the remuxer.
unsafe impl Send for Dvdcss {}

impl Dvdcss {
    /// Open `device` (e.g. `/dev/sr0`) for decrypted sector reads.
    pub fn open(device: &Path) -> Result<Self, DiscError> {
        let api = api().map_err(|reason| DiscError::Dvdcss(reason.to_string()))?;
        let path = CString::new(device.to_string_lossy().into_owned())
            .map_err(|e| DiscError::Dvdcss(format!("invalid device path: {e}")))?;
        let handle = unsafe { (api.open)(path.as_ptr()) };
        if handle.is_null() {
            return Err(DiscError::Dvdcss(format!(
                "libdvdcss could not open {}",
                device.display()
            )));
        }
        Ok(Self { api, handle })
    }

    /// Read one 2048-byte, decrypted sector at absolute disc LBA `lba`.
    pub fn read_sector(&mut self, lba: u64, buf: &mut [u8; 2048]) -> Result<(), DiscError> {
        let blocks =
            c_int::try_from(lba).map_err(|_| DiscError::Remux("LBA out of range".into()))?;
        let seeked = unsafe { (self.api.seek)(self.handle, blocks, 0) };
        if seeked < 0 {
            return Err(DiscError::Remux(format!(
                "dvdcss_seek({lba}): {}",
                self.error()
            )));
        }
        let read = unsafe {
            (self.api.read)(
                self.handle,
                buf.as_mut_ptr() as *mut c_void,
                1,
                DVDCSS_READ_DECRYPT,
            )
        };
        if read != 1 {
            return Err(DiscError::Remux(format!(
                "dvdcss_read({lba}): {}",
                self.error()
            )));
        }
        Ok(())
    }

    fn error(&self) -> String {
        unsafe {
            let ptr = (self.api.error)(self.handle);
            if ptr.is_null() {
                "unknown error".to_string()
            } else {
                CStr::from_ptr(ptr).to_string_lossy().into_owned()
            }
        }
    }
}

impl Drop for Dvdcss {
    fn drop(&mut self) {
        unsafe {
            (self.api.close)(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A candidate that cannot exist must yield a readable reason rather than
    /// panicking, so a missing library degrades cleanly.
    #[test]
    fn missing_candidates_report_a_reason() {
        let missing = [PathBuf::from("/nonexistent/packrat/libdvdcss.so.2")];
        let error = match load_from(&missing) {
            Ok(_) => panic!("a nonexistent path must not load"),
            Err(error) => error,
        };
        assert!(error.contains("libdvdcss not found"), "{error}");
        assert!(error.contains("/nonexistent/packrat"), "{error}");
    }

    /// Probing the real environment must be safe to call from anywhere.
    #[test]
    fn status_is_queryable() {
        let status = status();
        let _ = status.is_available();
    }
}

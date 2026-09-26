//! Minimal FFI to `libdvdcss` for CSS decryption on raw optical devices.
//!
//! Only the handful of calls the ripper needs are bound. The feature is
//! optional so the default build has no native dependency; without it the
//! device path still reads raw sectors directly, which is enough for
//! unencrypted discs.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::Path;

use crate::error::DiscError;

/// `dvdcss_read` flag: decrypt the sectors we read.
const DVDCSS_READ_DECRYPT: c_int = 1 << 0;

#[link(name = "dvdcss")]
extern "C" {
    fn dvdcss_open(target: *const c_char) -> *mut c_void;
    fn dvdcss_close(handle: *mut c_void) -> c_int;
    fn dvdcss_seek(handle: *mut c_void, blocks: c_int, flags: c_int) -> c_int;
    fn dvdcss_read(handle: *mut c_void, buffer: *mut c_void, blocks: c_int, flags: c_int) -> c_int;
    fn dvdcss_error(handle: *mut c_void) -> *const c_char;
}

/// An open libdvdcss handle.
pub struct Dvdcss {
    handle: *mut c_void,
}

// The handle is only used from one thread at a time by the remuxer.
unsafe impl Send for Dvdcss {}

impl Dvdcss {
    /// Open `device` (e.g. `/dev/sr0`) for decrypted sector reads.
    pub fn open(device: &Path) -> Result<Self, DiscError> {
        let path = CString::new(device.to_string_lossy().into_owned())
            .map_err(|e| DiscError::Remux(format!("invalid device path: {e}")))?;
        let handle = unsafe { dvdcss_open(path.as_ptr()) };
        if handle.is_null() {
            return Err(DiscError::Remux(format!(
                "libdvdcss could not open {}",
                device.display()
            )));
        }
        Ok(Self { handle })
    }

    /// Read one 2048-byte, decrypted sector at absolute disc LBA `lba`.
    pub fn read_sector(&mut self, lba: u64, buf: &mut [u8; 2048]) -> Result<(), DiscError> {
        let blocks =
            c_int::try_from(lba).map_err(|_| DiscError::Remux("LBA out of range".into()))?;
        let seeked = unsafe { dvdcss_seek(self.handle, blocks, 0) };
        if seeked < 0 {
            return Err(DiscError::Remux(format!(
                "dvdcss_seek({lba}): {}",
                self.error()
            )));
        }
        let read = unsafe {
            dvdcss_read(
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
            let ptr = dvdcss_error(self.handle);
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
            dvdcss_close(self.handle);
        }
    }
}

//! Reading a title's VOB chain straight from the optical device.
//!
//! The mounted folder is still used for the IFO structure (filesystem metadata
//! is never encrypted), but the VOB data is read from the raw device at
//! absolute LBAs. When the `dvdcss` feature is on, those sector reads go
//! through libdvdcss so CSS-encrypted discs decrypt transparently; otherwise
//! the device is read directly, which handles unencrypted discs.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use oxideav_dvd::{DvdDisc, DvdFileKind};

use crate::error::DiscError;

/// DVD logical sector size.
const SECTOR: usize = 2048;

/// Reads a title set's VOB chain from an optical device, addressed by the
/// same chain-relative sector numbers the IFO uses.
pub struct DeviceChainReader {
    backend: Backend,
    base_lba: u64,
    pos: u64,
    cache: Box<[u8; SECTOR]>,
    cache_lba: Option<u64>,
}

enum Backend {
    /// Direct device reads, used when the `dvdcss` feature is off.
    #[allow(dead_code)]
    Plain(File),
    #[cfg(feature = "dvdcss")]
    Dvdcss(crate::dvdcss::Dvdcss),
}

impl DeviceChainReader {
    /// Open `device` and find the start of `VTS_xx_1.VOB`, the origin for the
    /// cell-relative sectors in the IFO.
    ///
    /// The ISO 9660 bridge is used rather than UDF because ISO directory
    /// records carry absolute disc LBAs, whereas UDF addresses are relative to
    /// the partition (which does not start at sector 0 on real discs).
    pub fn open(device: &Path, vts_number: u8) -> Result<Self, DiscError> {
        let file = File::open(device).map_err(|source| DiscError::Io {
            path: device.to_path_buf(),
            source,
        })?;
        let disc = DvdDisc::from_iso9660(file)
            .map_err(|e| DiscError::Remux(format!("reading ISO 9660 from device: {e}")))?;
        let base_lba = disc
            .video_ts_files
            .iter()
            .find(|f| matches!(f.kind, DvdFileKind::VtsTitle { ts, vob: 1 } if ts == vts_number))
            .map(|f| u64::from(f.lba))
            .ok_or_else(|| DiscError::Remux(format!("device has no VTS_{vts_number:02}_1.VOB")))?;

        Ok(Self {
            backend: Backend::open(device)?,
            base_lba,
            pos: 0,
            cache: Box::new([0u8; SECTOR]),
            cache_lba: None,
        })
    }

    fn sector(&mut self, lba: u64) -> io::Result<&[u8; SECTOR]> {
        if self.cache_lba != Some(lba) {
            self.backend.read_sector(lba, &mut self.cache)?;
            self.cache_lba = Some(lba);
        }
        Ok(&self.cache)
    }
}

impl Backend {
    fn open(device: &Path) -> Result<Self, DiscError> {
        #[cfg(feature = "dvdcss")]
        {
            crate::dvdcss::Dvdcss::open(device).map(Backend::Dvdcss)
        }
        #[cfg(not(feature = "dvdcss"))]
        {
            let file = File::open(device).map_err(|source| DiscError::Io {
                path: device.to_path_buf(),
                source,
            })?;
            Ok(Backend::Plain(file))
        }
    }

    fn read_sector(&mut self, lba: u64, buf: &mut [u8; SECTOR]) -> io::Result<()> {
        match self {
            Backend::Plain(file) => {
                file.seek(SeekFrom::Start(lba * SECTOR as u64))?;
                file.read_exact(buf)
            }
            #[cfg(feature = "dvdcss")]
            Backend::Dvdcss(css) => css.read_sector(lba, buf).map_err(io::Error::other),
        }
    }
}

impl Read for DeviceChainReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut written = 0;
        while written < buf.len() {
            let sector = self.base_lba + self.pos / SECTOR as u64;
            let offset = (self.pos % SECTOR as u64) as usize;
            let take = (SECTOR - offset).min(buf.len() - written);
            let data = self.sector(sector)?;
            buf[written..written + take].copy_from_slice(&data[offset..offset + take]);
            written += take;
            self.pos += take as u64;
        }
        Ok(written)
    }
}

impl Seek for DeviceChainReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::Current(delta) => self.pos as i64 + delta,
            SeekFrom::End(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "seek from end of device chain is not supported",
                ))
            }
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before start of device chain",
            ));
        }
        self.pos = target as u64;
        Ok(self.pos)
    }
}

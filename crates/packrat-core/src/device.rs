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

use oxideav_dvd::udf::UdfVolume;
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
    /// UDF (the DVD-mandated filesystem) is tried first because some
    /// copy-protection schemes deliberately damage the ISO 9660 bridge — the
    /// directory records a ripper would use to find the VOBs — while leaving
    /// UDF intact so ordinary players still work. UDF block addresses are
    /// relative to the partition, so the partition start is added to get an
    /// absolute LBA. The ISO 9660 bridge is the fallback; its directory records
    /// carry absolute disc LBAs already.
    pub fn open(device: &Path, vts_number: u8) -> Result<Self, DiscError> {
        let base_lba = vts_chain_base_lba(device, vts_number)?;
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

/// Absolute LBA of `VTS_xx_1.VOB` on `device`.
///
/// Prefers UDF and adds its partition offset, then falls back to the ISO 9660
/// bridge. See [`DeviceChainReader::open`] for why UDF comes first.
fn vts_chain_base_lba(device: &Path, vts_number: u8) -> Result<u64, DiscError> {
    // UDF first: its block addresses are partition-relative, so add the
    // partition start to turn them into absolute disc LBAs.
    if let Ok(file) = File::open(device) {
        if let Ok(mut udf) = UdfVolume::open(file) {
            let partition_start = udf.partition_start_sector;
            if let Ok(disc) = DvdDisc::from_udf(&mut udf) {
                if let Some(lba) = absolute_vts_base(&disc, partition_start, vts_number) {
                    return Ok(lba);
                }
            }
        }
    }

    // Fall back to the ISO 9660 bridge, whose directory records already carry
    // absolute disc LBAs.
    let file = File::open(device).map_err(|source| DiscError::Io {
        path: device.to_path_buf(),
        source,
    })?;
    let disc = DvdDisc::from_iso9660(file)
        .map_err(|e| DiscError::Remux(format!("reading ISO 9660 from device: {e}")))?;
    absolute_vts_base(&disc, 0, vts_number)
        .ok_or_else(|| DiscError::Remux(format!("device has no VTS_{vts_number:02}_1.VOB")))
}

/// Absolute LBA of the first `VTS_xx_1.VOB` in `disc`, whose file LBAs are
/// relative to a partition starting at `partition_start` (0 for ISO 9660,
/// whose records are already absolute).
fn absolute_vts_base(disc: &DvdDisc, partition_start: u64, vts_number: u8) -> Option<u64> {
    disc.video_ts_files
        .iter()
        .find(|f| matches!(f.kind, DvdFileKind::VtsTitle { ts, vob: 1 } if ts == vts_number))
        .map(|f| partition_start + u64::from(f.lba))
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

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_dvd::DvdFile;

    fn disc_with(files: Vec<DvdFile>) -> DvdDisc {
        DvdDisc {
            volume_id: "TEST".into(),
            title_set_count: 9,
            video_ts_files: files,
            audio_ts_files: Vec::new(),
        }
    }

    fn vob(ts: u8, vob: u8, lba: u32) -> DvdFile {
        DvdFile {
            kind: DvdFileKind::VtsTitle { ts, vob },
            name: format!("VTS_{ts:02}_{vob}.VOB"),
            lba,
            size: 1024 * 1024,
            title_set: ts,
            vob_index: vob,
        }
    }

    /// UDF file LBAs are partition-relative, so the partition start is added.
    #[test]
    fn udf_base_lba_includes_the_partition_offset() {
        let disc = disc_with(vec![vob(4, 1, 1000)]);
        assert_eq!(absolute_vts_base(&disc, 262, 4), Some(1262));
    }

    /// ISO 9660 directory records are already absolute, so no offset is added.
    #[test]
    fn iso_base_lba_is_used_as_is() {
        let disc = disc_with(vec![vob(4, 1, 1000)]);
        assert_eq!(absolute_vts_base(&disc, 0, 4), Some(1000));
    }

    /// The origin is the first VOB of the requested title set, not another one.
    #[test]
    fn base_lba_selects_vob_one_of_the_title_set() {
        let disc = disc_with(vec![vob(3, 1, 10), vob(4, 2, 20), vob(4, 1, 30)]);
        assert_eq!(absolute_vts_base(&disc, 262, 4), Some(292));
        assert_eq!(absolute_vts_base(&disc, 262, 5), None);
    }
}

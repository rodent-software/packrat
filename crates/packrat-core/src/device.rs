//! Reading a title's VOB chain straight from the optical device.
//!
//! The mounted folder is still used for the IFO structure (filesystem metadata
//! is never encrypted), but the VOB data is read from the raw device at
//! absolute LBAs. When the user has supplied libdvdcss, those sector reads go
//! through it so CSS-encrypted discs decrypt transparently; without it the
//! device is read directly, which handles unencrypted discs.

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
    /// Direct device reads, used when libdvdcss is not available.
    Plain(File),
    /// Decrypted reads through the user-supplied libdvdcss.
    Dvdcss(crate::dvdcss::Dvdcss),
    /// Test double that records the requests it is handed, so a test can assert
    /// how many device requests a read produced and how large they were.
    #[cfg(test)]
    Recording(RecordedRequests),
}

/// Requests recorded by the test backend, as `(lba, bytes)`.
#[cfg(test)]
type RecordedRequests = std::sync::Arc<std::sync::Mutex<Vec<(u64, usize)>>>;

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
    if let Ok(file) = crate::drives::open_device(device) {
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
    let file = crate::drives::open_device(device).map_err(|source| DiscError::Io {
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
        // Prefer the decryption backend. Without libdvdcss, fall back to plain
        // reads, which still serve unencrypted discs; an encrypted disc then
        // fails on its scrambled sectors rather than failing to open at all.
        match crate::dvdcss::Dvdcss::open(device) {
            Ok(css) => Ok(Backend::Dvdcss(css)),
            Err(_) => {
                let file = crate::drives::open_device(device).map_err(|source| DiscError::Io {
                    path: device.to_path_buf(),
                    source,
                })?;
                Ok(Backend::Plain(file))
            }
        }
    }

    fn read_sector(&mut self, lba: u64, buf: &mut [u8; SECTOR]) -> io::Result<()> {
        self.read_range(lba, buf)
    }

    /// Read whole sectors in a single request.
    ///
    /// `buf` must hold a whole number of sectors. One request per span is what
    /// lets the drive stream: a 2 KiB request per sector is answered at the
    /// drive's command rate, roughly a millisecond each, which pins a rip near
    /// 2 MB/s however fast the media is.
    fn read_range(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()> {
        debug_assert!(!buf.is_empty() && buf.len() % SECTOR == 0);
        match self {
            Backend::Plain(file) => {
                file.seek(SeekFrom::Start(lba * SECTOR as u64))?;
                file.read_exact(buf)
            }
            Backend::Dvdcss(css) => css.read_blocks(lba, buf).map_err(from_disc_error),
            #[cfg(test)]
            Backend::Recording(requests) => {
                requests
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((lba, buf.len()));
                Ok(())
            }
        }
    }
}

/// Keep "the drive is not ready yet" a retryable `WouldBlock` all the way out
/// to the remuxer, which waits it out rather than treating it as damage.
fn from_disc_error(error: DiscError) -> io::Error {
    match error {
        DiscError::NotReady(message) => io::Error::new(io::ErrorKind::WouldBlock, message),
        other => io::Error::other(other),
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

            // A sector-aligned span goes to the drive as one request. The
            // sector cache below only has to serve the unaligned head or tail
            // of a request, which the remuxer never produces because it reads
            // whole windows, but the `Read` contract requires supporting.
            if offset == 0 {
                let whole = (buf.len() - written) / SECTOR;
                if whole > 0 {
                    let bytes = whole * SECTOR;
                    self.backend
                        .read_range(sector, &mut buf[written..written + bytes])?;
                    written += bytes;
                    self.pos += bytes as u64;
                    continue;
                }
            }

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

    /// A reader whose backend records what it is asked for.
    fn recording() -> (RecordedRequests, DeviceChainReader) {
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let reader = DeviceChainReader {
            backend: Backend::Recording(requests.clone()),
            base_lba: 100,
            pos: 0,
            cache: Box::new([0u8; SECTOR]),
            cache_lba: None,
        };
        (requests, reader)
    }

    fn requests_of(requests: &RecordedRequests) -> Vec<(u64, usize)> {
        requests.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// A read spanning many sectors must reach the drive as one request. Asking
    /// per sector is what pins a rip at the drive's command rate — about 2 MB/s
    /// — instead of letting its media rate through.
    #[test]
    fn a_multi_sector_read_becomes_one_device_request() {
        let (requests, mut reader) = recording();
        let mut buf = vec![0u8; 64 * SECTOR];

        reader.read_exact(&mut buf).expect("read the window");

        assert_eq!(requests_of(&requests), vec![(100, 64 * SECTOR)]);
    }

    /// A request that starts and ends mid-sector still hands the drive its
    /// whole aligned body as one request, instead of dropping to one request
    /// per sector for the bulk of it.
    #[test]
    fn an_unaligned_read_only_splits_its_edges() {
        let (requests, mut reader) = recording();
        reader.seek(SeekFrom::Start(10)).expect("seek");
        let mut buf = vec![0u8; 32 * SECTOR + 10];

        reader.read_exact(&mut buf).expect("read");

        // The head costs the sector it starts in, the 31 whole sectors in the
        // middle arrive in a single request, and the tail costs its sector.
        assert_eq!(
            requests_of(&requests),
            vec![(100, SECTOR), (101, 31 * SECTOR), (132, SECTOR)],
        );
    }

    /// "The drive is not ready yet" has to stay retryable all the way out to
    /// the remuxer, which waits a `WouldBlock` out instead of writing the
    /// sector off as unreadable.
    #[test]
    fn a_not_ready_drive_becomes_a_retryable_would_block() {
        let not_ready = from_disc_error(DiscError::NotReady("spinning up".into()));
        assert_eq!(not_ready.kind(), std::io::ErrorKind::WouldBlock);

        let damage = from_disc_error(DiscError::Remux("medium error".into()));
        assert_eq!(damage.kind(), std::io::ErrorKind::Other);
    }
}

//! Packrat-owned M2TS title reader.
//!
//! A Blu-ray title is a sequence of 192-byte BDAV source packets: a 4-byte
//! TP_extra header followed by a standard 188-byte MPEG-TS packet. The reader
//! walks the playlist's clips, decrypts complete AACS units when a key is
//! available, strips TP_extra, and exposes the clean TS byte stream the
//! demuxer already expects.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use super::aacs::UnitDecryptor;
use super::mpls;

const AACS_UNIT_LEN: usize = 6144;
const M2TS_PACKET_LEN: usize = 192;
const TS_PACKET_LEN: usize = 188;
const TP_EXTRA_HEADER_LEN: usize = 4;

pub(super) struct TitleSource {
    bdmv: PathBuf,
    clips: Vec<Clip>,
    clip_index: usize,
    current: Option<File>,
    clip_offset: u64,
    pending: Vec<u8>,
    pending_pos: usize,
    output_pos: u64,
    output_total: u64,
    decryptor: Option<Box<dyn UnitDecryptor>>,
}

struct Clip {
    stem: String,
}

impl TitleSource {
    pub(super) fn open(
        root: &Path,
        playlist: &mpls::Playlist,
        decryptor: Option<Box<dyn UnitDecryptor>>,
    ) -> io::Result<Self> {
        let bdmv = root.join("BDMV");
        let mut clips = Vec::with_capacity(playlist.play_items.len());
        let mut total = 0u64;
        for item in &playlist.play_items {
            let path = bdmv.join("STREAM").join(format!("{}.m2ts", item.clip_id));
            let len = std::fs::metadata(&path)?.len();
            let usable = len - (len % M2TS_PACKET_LEN as u64);
            clips.push(Clip {
                stem: item.clip_id.clone(),
            });
            total += usable / M2TS_PACKET_LEN as u64 * TS_PACKET_LEN as u64;
        }
        Ok(Self {
            bdmv,
            clips,
            clip_index: 0,
            current: None,
            clip_offset: 0,
            pending: Vec::new(),
            pending_pos: 0,
            output_pos: 0,
            output_total: total,
            decryptor,
        })
    }

    pub(super) fn output_total(&self) -> u64 {
        self.output_total
    }

    pub(super) fn seek_to(&mut self, _pts_90k: u64) -> io::Result<u64> {
        self.clip_index = 0;
        self.current = None;
        self.clip_offset = 0;
        self.pending.clear();
        self.pending_pos = 0;
        self.output_pos = 0;
        Ok(0)
    }

    fn open_current(&mut self) -> io::Result<()> {
        let Some(clip) = self.clips.get(self.clip_index) else {
            return Ok(());
        };
        let path = self.bdmv.join("STREAM").join(format!("{}.m2ts", clip.stem));
        self.current = Some(File::open(path)?);
        Ok(())
    }

    fn refill(&mut self) -> io::Result<bool> {
        loop {
            if self.current.is_none() {
                self.open_current()?;
                if self.current.is_none() {
                    return Ok(false);
                }
            }
            let some_clip = self.clips.get(self.clip_index);
            let Some(_clip) = some_clip else {
                return Ok(false);
            };
            let mut unit = [0u8; AACS_UNIT_LEN];
            let mut filled = 0usize;
            while filled < AACS_UNIT_LEN {
                let Some(file) = self.current.as_mut() else {
                    break;
                };
                match file.read(&mut unit[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
            if filled == 0 {
                self.clip_index += 1;
                self.current = None;
                self.clip_offset = 0;
                if self.clip_index >= self.clips.len() {
                    return Ok(false);
                }
                continue;
            }

            let usable = filled - (filled % M2TS_PACKET_LEN);
            let decrypt_len = usable - (usable % AACS_UNIT_LEN);
            if decrypt_len > 0 {
                if let Some(decryptor) = self.decryptor.as_mut() {
                    decryptor
                        .decrypt_units(&mut unit[..decrypt_len], self.clip_offset)
                        .map_err(|error| io::Error::other(error.to_string()))?;
                }
            }
            self.clip_offset += usable as u64;

            let packets = usable / M2TS_PACKET_LEN;
            self.pending.resize(packets * TS_PACKET_LEN, 0);
            self.pending_pos = 0;
            for index in 0..packets {
                let source = index * M2TS_PACKET_LEN + TP_EXTRA_HEADER_LEN;
                let target = index * TS_PACKET_LEN;
                self.pending[target..target + TS_PACKET_LEN]
                    .copy_from_slice(&unit[source..source + TS_PACKET_LEN]);
            }
            return Ok(true);
        }
    }
}

impl Read for TitleSource {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || self.output_pos >= self.output_total {
            return Ok(0);
        }
        if self.pending_pos >= self.pending.len() && !self.refill()? {
            return Ok(0);
        }
        let available = self.pending.len() - self.pending_pos;
        let take = available.min(out.len());
        out[..take].copy_from_slice(&self.pending[self.pending_pos..self.pending_pos + take]);
        self.pending_pos += take;
        self.output_pos += take as u64;
        Ok(take)
    }
}

impl Seek for TitleSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match pos {
            SeekFrom::Start(0) => {
                self.seek_to(0)?;
                Ok(0)
            }
            // The demuxer's duration probe asks for the end; answer it without
            // walking the clips.
            SeekFrom::End(0) => Ok(self.output_total),
            SeekFrom::Current(0) => Ok(self.output_pos),
            _ => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "M2TS title source only supports rewinding to the start",
            )),
        }
    }
}

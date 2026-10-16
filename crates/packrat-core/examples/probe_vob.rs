//! Dump the MPEG-PS pack layout of a disc's video, sector by sector.
//!
//! ```sh
//! cargo run -p packrat-core --example probe_vob -- /dev/sr1 [offset] [count]
//! ```
//!
//! The remuxer reads a VOB as a sequence of 2048-byte packs and expects each
//! pack's payload to start a PES packet. A disc authored the other way — a PES
//! packet running across several packs, so that later packs are continuations
//! with no header of their own — defeats that, and the sectors it cannot parse
//! are dropped. This prints what is actually there, so "the disc is authored
//! that way" is something seen rather than inferred.
//!
//! `offset` is a sector count from the start of `VTS_01_1.VOB` (default 0) and
//! `count` how many sectors to read one at a time (default 200). Reads go
//! through libdvdcss when it is available, so an encrypted disc works too. A
//! plain `.VOB` file can be given instead, which is how the tool itself was
//! checked.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use oxideav_dvd::vob::{
    looks_like_nav_pack, PackHeader, PesPacket, SC_PADDING_STREAM, SC_PRIVATE_STREAM_2,
    SC_SYSTEM_HEADER,
};

const SECTOR: usize = 2048;

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let force_css = args.iter().any(|a| a == "--css");
    args.retain(|a| a != "--css");
    let vts: u8 = args
        .iter()
        .position(|a| a == "--vts")
        .and_then(|at| args.remove(at + 1).parse().ok())
        .unwrap_or(1);
    args.retain(|a| a != "--vts");
    let dump = args
        .iter()
        .position(|a| a == "--dump")
        .map(|at| args.remove(at + 1));
    args.retain(|a| a != "--dump");
    let mut args = args.into_iter();
    let Some(device) = args.next() else {
        eprintln!(
            "usage: probe_vob <device|file.vob> [offset] [count] [--vts N] [--css] [--dump out.bin]"
        );
        eprintln!("  --vts   which title set's VOB 1 the offset counts from (default 1)");
        eprintln!("  --css   read through libdvdcss even for a file (it cracks the title");
        eprintln!("          key from the data, so no drive is needed)");
        eprintln!("  --dump  write each sector as read to a file, to compare elsewhere");
        std::process::exit(2);
    };
    let offset: u64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(0);
    let count: u64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(200);
    let device = Path::new(&device);

    let from_file = device.is_file();
    let opened = if force_css {
        Reader::open(device)
    } else if from_file {
        Reader::plain(device)
    } else {
        Reader::open(device)
    };
    let Ok(mut reader) = opened else {
        eprintln!("could not open {device:?}");
        std::process::exit(1);
    };
    // The same sectors read a second time without libdvdcss, which yields the
    // disc's bytes as they are: that is what says whether a sector was
    // scrambled, since a descrambled sector comes back with the flag cleared
    // and can no longer say so itself.
    let mut raw = Reader::plain(device).ok();
    let mut dumper = match &dump {
        Some(path) if force_css => match std::fs::File::create(path) {
            Ok(file) => Some(std::io::BufWriter::new(file)),
            Err(e) => {
                eprintln!("could not write {path}: {e}");
                std::process::exit(1);
            }
        },
        _ => None,
    };

    let base = if from_file {
        0
    } else {
        match vob_base(device, vts) {
            Some(base) => base,
            None => {
                eprintln!("could not find VTS_{vts:02}_1.VOB on {device:?}");
                std::process::exit(1);
            }
        }
    };
    let first = base + offset;
    println!(
        "{} starts at sector {base}; reading {count} sectors from {first}\n(via {})\n",
        if from_file {
            "the file".to_string()
        } else {
            format!("VTS_{vts:02}_1.VOB")
        },
        reader.how()
    );
    // As a rip does when it opens the title set: settle the CSS title key here,
    // once, so the reads below are descrambled. A plain read has no key to
    // settle and simply gets the disc's bytes.
    if reader.check_title_key(base).is_err() {
        eprintln!("could not settle the title key at sector {base}");
        std::process::exit(1);
    }

    let mut tallies: Vec<(&str, u64)> = vec![
        ("nav pack", 0),
        ("PES packet starts here", 0),
        ("PES spans past this sector", 0),
        ("continuation / not a PES start", 0),
        ("header will not parse", 0),
    ];
    let mut shown_continuations = 0;
    let mut shown_spans = 0;
    // What the disc marks as scrambled has to come back as valid MPEG. A
    // sector that is flagged scrambled and whose payload still holds no start
    // code has not been descrambled, whatever else looks healthy.
    // What the disc says versus what the read produced. A sector holds valid
    // MPEG video (start codes in its payload) either way if it was never
    // scrambled; if the raw copy has none and the read copy has some, it was
    // scrambled and has now been descrambled. Raw none and read none is the
    // failure that matters: the payload went to the file as noise.
    //
    // Only video packets are judged this way. An AC-3 or subpicture payload has
    // no MPEG start codes in it whether it was encrypted or not, so a scrambled
    // audio sector would otherwise look like a failure.
    let mut never_scrambled = 0u64;
    let mut scrambled_readable = 0u64;
    let mut scrambled_noise = 0u64;
    let mut not_video = 0u64;
    let mut raw_missing = 0u64;

    for index in 0..count {
        let lba = first + index;
        let Ok(sector) = reader.sector(lba) else {
            println!("sector {lba}: READ FAILED");
            continue;
        };
        if let Some(out) = dumper.as_mut() {
            let _ = out.write_all(&sector);
        }

        let start_codes = |s: &[u8]| {
            s[0x80..0x800]
                .windows(3)
                .filter(|w| *w == [0x00, 0x00, 0x01])
                .count()
        };
        // 0xE0..=0xEF is the video stream; anything else is audio, a
        // subpicture or a nav pack, and not what this test is about.
        let is_video = (0xE0..=0xEF).contains(&sector[0x11]);
        match raw.as_mut().and_then(|raw| raw.sector(lba).ok()) {
            Some(raw_sector) if is_video => {
                let before = start_codes(&raw_sector);
                let after = start_codes(&sector);
                if before > 0 {
                    never_scrambled += 1;
                } else if after > 0 {
                    scrambled_readable += 1;
                } else {
                    scrambled_noise += 1;
                }
            }
            Some(_) => not_video += 1,
            None => raw_missing += 1,
        }

        if looks_like_nav_pack(&sector) {
            tallies[0].1 += 1;
            continue;
        }
        let Ok(pack) = PackHeader::parse(&sector) else {
            tallies[4].1 += 1;
            println!("sector {lba}: pack header will not parse");
            continue;
        };
        let cursor = PackHeader::SIZE + pack.stuffing_bytes as usize;
        if cursor + 6 > sector.len() {
            tallies[3].1 += 1;
            continue;
        }
        if sector[cursor..cursor + 3] != [0x00, 0x00, 0x01] {
            // This is the shape that matters: a pack whose payload continues
            // the packet the pack before it started.
            tallies[3].1 += 1;
            if shown_continuations < 6 {
                shown_continuations += 1;
                println!(
                    "sector {lba}: payload at {cursor:#x} is not a start code: {}",
                    hex(&sector[cursor..cursor + 12])
                );
            }
            continue;
        }

        let stream_id = sector[cursor + 3];
        if matches!(
            stream_id,
            SC_SYSTEM_HEADER | SC_PADDING_STREAM | SC_PRIVATE_STREAM_2
        ) {
            tallies[1].1 += 1;
            continue;
        }

        match PesPacket::parse(&sector[cursor..]) {
            Ok(pes) => {
                tallies[1].1 += 1;
                if index < 8 {
                    println!(
                        "sector {lba}: PES id={stream_id:#04x} length={} payload={} fit=yes",
                        pes.wire_size,
                        pes.payload.len()
                    );
                }
            }
            Err(error) => {
                // The declared packet does not fit in this sector: either it
                // runs on into the next packs, or the sector is damaged.
                tallies[2].1 += 1;
                if shown_spans < 6 {
                    shown_spans += 1;
                    let declared = if cursor + 6 <= sector.len() {
                        (usize::from(sector[cursor + 4]) << 8) | usize::from(sector[cursor + 5])
                    } else {
                        0
                    };
                    println!(
                        "sector {lba}: PES id={stream_id:#04x} declares {declared} bytes, {} left \
                         in the sector: {error}",
                        sector.len() - cursor - 6
                    );
                }
            }
        }
    }

    println!("\n== {count} sectors from {first} ==");
    for (what, n) in tallies {
        println!("  {n:>5}  {what}");
    }
    if raw_missing > 0 || never_scrambled + scrambled_readable + scrambled_noise > 0 {
        println!("\n  what the read did with the disc's payload:");
        println!(
            "    {never_scrambled:>5}  were never scrambled (raw copy already held start codes)"
        );
        println!("    {scrambled_readable:>5}  were scrambled, and came back as valid MPEG");
        println!("    {scrambled_noise:>5}  were scrambled, and came back as NOISE");
        if not_video > 0 {
            println!("    {not_video:>5}  were audio, subpictures or nav packs (not judged)");
        }
        if raw_missing > 0 {
            println!("    {raw_missing:>5}  could not be read a second time for comparison");
        }
        if scrambled_noise > 0 {
            let how = reader.how();
            if how == "libdvdcss" {
                println!(
                    "\n  Scrambled sectors came back as noise: the title key was not the one\n  \
                     this disc needs, so every such sector was descrambled wrongly. (A file\n  \
                     given to libdvdcss has the same result: it cannot get a key without the\n  \
                     drive.)"
                );
            } else {
                println!(
                    "\n  Scrambled sectors came back as noise because nothing descrambled\n  \
                     them: this is a plain read ({how})."
                );
            }
        } else if scrambled_readable > 0 {
            println!(
                "\n  Every scrambled video sector that was read came back as valid MPEG:\n  \
                 the title key is the right one and the payload is being decrypted."
            );
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The absolute sector `VTS_01_1.VOB` starts at, so a raw read lands on video.
fn vob_base(device: &Path, vts: u8) -> Option<u64> {
    let mut file = packrat_core::drives::open_device(device).ok()?;
    let disc = oxideav_dvd::DvdDisc::from_reader(&mut file).ok()?;
    let partition = oxideav_dvd::udf::UdfVolume::open(&mut file)
        .map(|udf| udf.partition_start_sector)
        .unwrap_or(0);
    disc.video_ts_files
        .iter()
        .find(|f| matches!(f.kind, oxideav_dvd::DvdFileKind::VtsTitle { ts, vob: 1 } if ts == vts))
        .map(|f| partition + u64::from(f.lba))
}

/// One sector at a time, decrypted when libdvdcss is there to do it.
enum Reader {
    Css(packrat_core::dvdcss::Dvdcss),
    Plain(std::fs::File),
}

impl Reader {
    fn open(device: &Path) -> Result<Self, ()> {
        if let Ok(css) = packrat_core::dvdcss::Dvdcss::open(device) {
            return Ok(Self::Css(css));
        }
        Self::plain(device)
    }

    fn plain(device: &Path) -> Result<Self, ()> {
        std::fs::File::open(device).map(Self::Plain).map_err(|_| ())
    }

    fn how(&self) -> &'static str {
        match self {
            Self::Css(_) => "libdvdcss",
            Self::Plain(_) => "plain device reads",
        }
    }

    /// Settle the title key for the set starting at `lba`, once, as a rip does
    /// when it opens the set. Without this the sectors read below would not be
    /// descrambled at all.
    fn check_title_key(&mut self, lba: u64) -> Result<(), ()> {
        match self {
            Self::Css(css) => css.check_title_key(lba).map_err(|_| ()),
            Self::Plain(_) => Ok(()),
        }
    }

    fn sector(&mut self, lba: u64) -> Result<[u8; SECTOR], ()> {
        let mut buf = [0u8; SECTOR];
        match self {
            Self::Css(css) => css.read_blocks(lba, &mut buf).map_err(|_| ())?,
            Self::Plain(file) => {
                file.seek(SeekFrom::Start(lba * SECTOR as u64))
                    .map_err(|_| ())?;
                file.read_exact(&mut buf).map_err(|_| ())?;
            }
        };
        Ok(buf)
    }
}

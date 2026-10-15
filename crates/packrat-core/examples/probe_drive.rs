//! Print what packrat can see of one drive, for when a disc will not appear.
//!
//! ```sh
//! cargo run -p packrat-core --example probe_drive -- /dev/sr1
//! ```
//!
//! Each layer is asked in turn — the kernel's list of drives, whether the
//! device opens, what the drive answers when asked directly, the disc's own
//! file table, and then every `IFO` the structure points at — so a failure can
//! be placed in one of them rather than guessed at. Nothing here is fatal, so
//! run it with the disc in the drive and read where it stops.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use packrat_core::{drives, read_disc, DiscSource};

const SECTOR: u64 = 2048;
/// Cap on what the probe will read for one file, so a malformed length in a
/// directory record cannot ask for gigabytes.
const MAX_EXTENT: u64 = 4 * 1024 * 1024;

fn main() {
    let Some(argument) = std::env::args().nth(1) else {
        eprintln!("usage: probe_drive <device>   e.g. probe_drive /dev/sr1");
        std::process::exit(2);
    };
    let device = Path::new(&argument);

    println!("== what the kernel lists ==");
    for drive in drives::list() {
        println!(
            "  {:<12} has_disc={:<5} mount={}",
            drive.device.display(),
            drive.has_disc,
            drive
                .mount
                .as_ref()
                .map(|m| m.display().to_string())
                .unwrap_or_else(|| "not mounted".into())
        );
    }

    println!("\n== {argument} ==");
    if let Some(name) = device.file_name().and_then(|n| n.to_str()) {
        if let Ok(text) = std::fs::read_to_string(format!("/sys/block/{name}/size")) {
            let sectors: u64 = text.trim().parse().unwrap_or(0);
            println!(
                "  /sys/block size   {sectors} sectors ({} bytes)",
                sectors * 512
            );
        }
    }
    match drives::open_device(device) {
        Ok(_) => println!("  open (O_NONBLOCK) ok"),
        Err(e) => {
            println!("  open (O_NONBLOCK) FAILED: {e}");
            println!("\n  That is the problem: nothing else can be tried without it.");
            return;
        }
    }
    println!("  drive says        {:?}", drives::media(device));

    println!("\n== the disc's file table ==");
    let mut reader = drives::open_device(device).expect("the open succeeded above");
    let disc = match oxideav_dvd::DvdDisc::from_reader(&mut reader) {
        Ok(disc) => disc,
        Err(e) => {
            println!("  FAILED: {e}");
            println!("\n  The file table is what everything else is addressed from.");
            return;
        }
    };
    println!(
        "  volume id         {:?}  ({} title sets, {} files)",
        disc.volume_id,
        disc.title_set_count,
        disc.video_ts_files.len()
    );

    // The offset every file address is added to. UDF entries are
    // partition-relative and ISO 9660 ones are already absolute, so this is the
    // number that decides whether the reads below land anywhere sensible.
    let base = match oxideav_dvd::udf::UdfVolume::open(&mut reader) {
        Ok(udf) => {
            println!(
                "  udf partition      {} (file addresses are relative to it)",
                udf.partition_start_sector
            );
            udf.partition_start_sector
        }
        Err(e) => {
            println!("  udf partition      none ({e}); file addresses are absolute");
            0
        }
    };

    println!("\n== reading each file from sector `base + lba` ==");
    for file in &disc.video_ts_files {
        let start = (base + u64::from(file.lba)) * SECTOR;
        let wanted = file.size.min(MAX_EXTENT);
        let mut buf = vec![0u8; wanted as usize];
        let outcome = reader
            .seek(SeekFrom::Start(start))
            .and_then(|_| reader.read_exact(&mut buf))
            .map(|_| ());
        println!(
            "  {:<16} lba={:<8} abs={:<8} {:>9} bytes  {}",
            file.name,
            file.lba,
            start / SECTOR,
            file.size,
            match outcome {
                Ok(()) => "ok".to_string(),
                Err(e) => format!("FAILED: {e}"),
            }
        );
    }

    println!("\n== can the drive read anywhere at all? ==");
    for sector in [0u64, 16, 256, 1000] {
        let mut buf = vec![0u8; SECTOR as usize];
        let outcome = reader
            .seek(SeekFrom::Start(sector * SECTOR))
            .and_then(|_| reader.read_exact(&mut buf));
        println!(
            "  sector {sector:<8} {}",
            match outcome {
                Ok(()) => "ok".to_string(),
                Err(e) => format!("FAILED: {e}"),
            }
        );
    }

    println!("\n== the whole thing, as the window does it ==");
    drop(reader);
    match DiscSource::discover_device(device, None).and_then(|source| read_disc(&source)) {
        Ok(disc) => {
            println!("  {} titles", disc.titles.len());
            println!("\n  A disc that reads here but not in the window is a window problem.");
        }
        Err(e) => println!("  FAILED: {e}"),
    }
}

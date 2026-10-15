//! Print the file LBAs `oxideav-dvd` derives, for debugging the device path.
//!
//! UDF file entries carry an *in-partition* block number, so the partition's
//! start sector has to be added to reach the disc. This prints that, reads the
//! disc with no mount, checks the result against the same `IFO` files through
//! an ordinary folder, and then dumps the parsed `IFO` internals.
//!
//! ```sh
//! cargo run -p packrat-core --example device_lba -- /dev/sr0
//! ```

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const SECTOR: u64 = 2048;

fn open(device: &str) -> std::fs::File {
    packrat_core::drives::open_device(std::path::Path::new(device)).expect("open device")
}

/// The absolute sector the disc's UDF file entries are addressed from.
fn base_sector(device: &str) -> u64 {
    match oxideav_dvd::udf::UdfVolume::open(open(device)) {
        Ok(volume) => volume.partition_start_sector,
        Err(_) => 0,
    }
}

/// Copy just the `IFO` files off the device into a temporary `VIDEO_TS`, so the
/// ordinary folder reader can be pointed at the same bytes.
fn dump_ifos(device: &str) -> std::io::Result<PathBuf> {
    let disc = oxideav_dvd::DvdDisc::open(device).expect("open device");
    let base = base_sector(device);

    let root = std::env::temp_dir().join(format!("packrat-ifo-{}", std::process::id()));
    let video_ts = root.join("VIDEO_TS");
    std::fs::create_dir_all(&video_ts)?;

    let mut file = open(device);
    for entry in &disc.video_ts_files {
        if !entry.name.to_ascii_uppercase().ends_with(".IFO") {
            continue;
        }
        file.seek(SeekFrom::Start((base + u64::from(entry.lba)) * SECTOR))?;
        let mut buf = vec![0u8; entry.size as usize];
        file.read_exact(&mut buf)?;
        std::fs::write(video_ts.join(&entry.name), buf)?;
    }

    Ok(root)
}

fn summary(label: &str, titles: &[packrat_core::Title]) {
    println!("{label}: {} titles", titles.len());
    for title in titles.iter() {
        println!(
            "  T{:02}  vts {}  ch {:>3}  {:?}",
            title.number, title.vts, title.chapters, title.duration
        );
    }
}

/// The parser's own view of the disc, straight from the dumped `IFO` files.
fn analyse(video_ts: &Path) {
    let main = std::fs::read(video_ts.join("VIDEO_TS.IFO")).expect("read VIDEO_TS.IFO");
    let vmg = oxideav_dvd::VmgIfo::parse(&main).expect("parse VMGI");
    println!(
        "\nVMGI: title_sets {}  tt_srpt_sector {}",
        vmg.number_of_title_sets, vmg.tt_srpt_sector
    );

    let offset = vmg.tt_srpt_sector as usize * SECTOR as usize;
    let tt = oxideav_dvd::TtSrpt::parse(&main[offset..]).expect("parse TT_SRPT");
    println!(
        "TT_SRPT: declared {} entries, parsed {}",
        tt.title_count,
        tt.entries.len()
    );
    for (i, e) in tt.entries.iter().enumerate() {
        println!(
            "  #{:02}  vts {}  ttn {:>2}  chapters {:>3}  angles {}  vts_start {}",
            i + 1,
            e.vts_number,
            e.vts_title_number,
            e.chapter_count,
            e.angle_count,
            e.vts_start_sector
        );
    }

    for n in 1..=vmg.number_of_title_sets {
        let Ok(nu8) = u8::try_from(n) else { continue };
        let path = video_ts.join(format!("VTS_{nu8:02}_0.IFO"));
        let Ok(buf) = std::fs::read(&path) else {
            continue;
        };
        match oxideav_dvd::VtsIfo::parse(&buf, nu8) {
            Ok(vts) => {
                println!(
                    "VTS {:02}: declared {} titles, parsed {} titles, {} pgcs",
                    nu8,
                    vts.title_count,
                    vts.titles.len(),
                    vts.pgcs.len()
                );
                for t in vts.titles.iter() {
                    let first = t.chapters.first();
                    let pgc = first
                        .and_then(|c| vts.pgcs.get(usize::from(c.pgcn).wrapping_sub(1)))
                        .map(|p| p.playback_time.total_seconds());
                    println!(
                        "    ttn {:>2}  chapters {:>3}  first pgcn {:?}  pgc seconds {:?}",
                        t.number,
                        t.chapters.len(),
                        first.map(|c| c.pgcn),
                        pgc
                    );
                }
            }
            Err(e) => println!("VTS {nu8:02}: parse failed: {e}"),
        }
    }
}

fn main() {
    let device = std::env::args().nth(1).unwrap_or_else(|| "/dev/sr0".into());

    let udf = oxideav_dvd::DvdDisc::open(&device).expect("open device (UDF)");
    println!("UDF volume_id = {}", udf.volume_id);
    println!(
        "\nUDF partition starts at sector {} (absolute = that + lba)",
        base_sector(&device)
    );

    println!("\n--- reading the disc with no mount ---");
    let source = packrat_core::DiscSource::discover_device(&device, None).expect("device source");
    let from_device = match packrat_core::read_disc(&source) {
        Ok(disc) => {
            println!("volume   : {}", disc.volume_id);
            println!("provider : {}", disc.provider_id);
            summary("device", &disc.titles);
            let classification = packrat_core::classify(&disc);
            println!(
                "classified as {:?} ({}% sure)",
                classification.kind, classification.confidence
            );
            for reason in &classification.reasons {
                println!("  - {reason}");
            }
            println!(
                "preferred titles: {:?}",
                packrat_core::preferred_titles(&disc)
            );
            Some(disc)
        }
        Err(e) => {
            println!("read_disc failed: {e}");
            None
        }
    };

    println!("\n--- the same IFO files through a folder ---");
    let Ok(root) = dump_ifos(&device) else {
        println!("could not dump IFO files");
        return;
    };
    let video_ts = root.join("VIDEO_TS");

    let folder = packrat_core::DiscSource::discover(&root).expect("folder source");
    match packrat_core::read_disc(&folder) {
        Ok(disc) => {
            summary("folder", &disc.titles);
            if let Some(device_disc) = &from_device {
                let same = device_disc.titles.len() == disc.titles.len()
                    && device_disc.titles.iter().zip(&disc.titles).all(|(a, b)| {
                        a.number == b.number
                            && a.vts == b.vts
                            && a.chapters == b.chapters
                            && a.duration == b.duration
                    });
                println!("\nidentical to the device read: {same}");
            }
        }
        Err(e) => println!("folder read failed: {e}"),
    }

    println!("\n--- what the parsers see ---");
    analyse(&video_ts);

    let _ = std::fs::remove_dir_all(&root);
}

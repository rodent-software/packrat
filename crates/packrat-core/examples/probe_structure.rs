//! Print how a disc's titles are laid out across its VOB files.
//!
//! ```sh
//! cargo run -p packrat-core --example probe_structure -- /run/media/you/DISC
//! ```
//!
//! Each title's chapters name the cells they play, and each cell names the
//! sector range it covers, so this shows which sectors a rip of a given title
//! actually reads — and, where those ranges overlap or double back, whether the
//! disc interleaves several angles or cuts in the same VOB.

use packrat_core::{read_disc, read_vts, DiscSource};

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: probe_structure <disc folder or device>");
        std::process::exit(2);
    };
    let source = match DiscSource::discover(&path) {
        Ok(source) => source,
        Err(e) => {
            eprintln!("not a disc: {e}");
            std::process::exit(1);
        }
    };
    let disc = match read_disc(&source) {
        Ok(disc) => disc,
        Err(e) => {
            eprintln!("could not read the disc: {e}");
            std::process::exit(1);
        }
    };

    println!("volume: {}", disc.volume_id);
    println!("titles: {}", disc.titles.len());
    for title in &disc.titles {
        println!(
            "  T{:02}  vts {}  ttn {}  angles {}  chapters {:>3}  {}",
            title.number,
            title.vts,
            title.vts_ttn,
            title.angles,
            title.chapters,
            title
                .duration
                .map(|d| format!("{}m{:02}s", d.as_secs() / 60, d.as_secs() % 60))
                .unwrap_or_else(|| "?".into())
        );
    }

    for vts in 1..=disc.vts_count {
        let Ok(ifo) = read_vts(&source, vts as u8) else {
            continue;
        };
        println!("\n== VTS_{vts:02} ==");
        for (n, pgc) in ifo.pgcs.iter().enumerate() {
            let mut ranges: Vec<(usize, u32, u32)> = pgc
                .cells
                .iter()
                .enumerate()
                .map(|(index, c)| (index + 1, c.first_vobu_start_sector, c.last_vobu_end_sector))
                .collect();
            ranges.sort_by_key(|(_, first, _)| *first);
            let first = ranges.first().map(|(_, f, _)| *f).unwrap_or(0);
            let last = ranges.last().map(|(_, _, l)| *l).unwrap_or(0);
            let overlapping = ranges.windows(2).filter(|w| w[1].1 <= w[0].2).count();
            println!(
                "  PGC {}: {} cells, sectors {first}..{last} ({} sectors, {} MB)",
                n + 1,
                ranges.len(),
                last.saturating_sub(first) + 1,
                (u64::from(last.saturating_sub(first) + 1) * 2048) / 1_000_000
            );
            if overlapping > 0 {
                println!("    !! {overlapping} cells overlap the one before them");
            }
            for (number, f, l) in ranges.iter().take(12) {
                println!(
                    "      cell {number:>3}: {f:>8}..{l:<8} ({} sectors)",
                    l - f + 1
                );
            }
            if ranges.len() > 12 {
                println!("      … {} more cells", ranges.len() - 12);
            }
        }
    }
}

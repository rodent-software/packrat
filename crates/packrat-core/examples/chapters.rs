//! Dump per-chapter durations for content titles, to tune detection.
//!
//! ```sh
//! cargo run -p packrat-core --example chapters -- /run/media/$USER/DRAGON_BALL_S1_D1
//! ```

use std::time::Duration;

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: chapters <disc root or VIDEO_TS dir>");
        std::process::exit(2);
    };

    let source = packrat_core::DiscSource::discover(&path).expect("discover disc");
    let disc = packrat_core::read_disc(&source).expect("read disc");

    for title in &disc.titles {
        let total = title.duration.unwrap_or_default();
        if total < Duration::from_secs(5 * 60) {
            continue;
        }
        let chapters: Vec<u64> = title
            .chapter_durations
            .iter()
            .map(|d| d.as_secs())
            .collect();
        let segments = packrat_core::split_title(title);
        println!(
            "Title {:>2}  total={:>6}s  chapters={:>2}  pattern={:?}  episodes={}",
            title.number,
            total.as_secs(),
            chapters.len(),
            packrat_core::chapter_pattern(&title.chapter_durations),
            segments.len()
        );
        println!("    {chapters:?}");
        for (i, seg) in segments.iter().enumerate() {
            println!(
                "    E{:<2} ch {:>3}-{:<3}  {}s",
                i + 1,
                seg.start_chapter,
                seg.end_chapter,
                seg.duration.as_secs()
            );
        }
    }
}

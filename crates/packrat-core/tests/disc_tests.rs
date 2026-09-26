//! Disc discovery and structure tests.
//!
//! The live-disc test is opt-in (it needs a real disc mounted):
//!
//! ```sh
//! PACKRAT_TEST_DISC=/run/media/$USER/DRAGON_BALL_S1_D1 cargo test
//! ```
//!
//! The reference numbers come from `HandBrakeCLI -i <disc> -t 0 --scan`.

use std::fs;
use std::path::PathBuf;

use packrat_core::{
    read_disc, read_vts, remux_chain, remux_chain_with_progress, remux_title, DiscError,
    DiscSource, RemuxPhase,
};

fn unique_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!("packrat-test-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn discover_accepts_disc_root() {
    let root = unique_dir("root");
    fs::create_dir_all(root.join("VIDEO_TS")).unwrap();
    fs::write(root.join("VIDEO_TS/VIDEO_TS.IFO"), b"x").unwrap();

    let source = DiscSource::discover(&root).expect("disc root should be accepted");
    assert_eq!(source.video_ts(), root.join("VIDEO_TS"));
    assert!(source.main_ifo().is_some());

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn discover_accepts_video_ts_dir() {
    let root = unique_dir("vtsdir");
    let video_ts = root.join("VIDEO_TS");
    fs::create_dir_all(&video_ts).unwrap();

    let source = DiscSource::discover(&video_ts).expect("VIDEO_TS dir should be accepted");
    assert_eq!(source.root(), root);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn discover_rejects_paths_without_video_ts() {
    let err = DiscSource::discover("/definitely/not/a/real/disc/path").unwrap_err();
    assert!(matches!(err, DiscError::NoVideoTs(_)));
}

/// VTS IFO discovery is sorted and ignores the backup (`_0.BUP`) files.
#[test]
fn vts_ifos_are_sorted_and_filtered() {
    let root = unique_dir("vtslist");
    let video_ts = root.join("VIDEO_TS");
    fs::create_dir_all(&video_ts).unwrap();
    for name in [
        "VIDEO_TS.IFO",
        "VTS_02_0.IFO",
        "VTS_01_0.IFO",
        "VTS_01_0.BUP",
        "VTS_10_0.IFO",
    ] {
        fs::write(video_ts.join(name), b"x").unwrap();
    }

    let source = DiscSource::discover(&root).unwrap();
    let names: Vec<String> = source
        .vts_ifos()
        .unwrap()
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["VTS_01_0.IFO", "VTS_02_0.IFO", "VTS_10_0.IFO"]);

    let _ = fs::remove_dir_all(&root);
}

/// Validates against a real mounted disc. Skipped unless `PACKRAT_TEST_DISC`
/// points at one.
#[test]
fn live_disc_matches_handbrake_reference() {
    let Ok(path) = std::env::var("PACKRAT_TEST_DISC") else {
        eprintln!("PACKRAT_TEST_DISC not set; skipping live-disc validation");
        return;
    };

    let source = DiscSource::discover(&path).expect("open test disc");
    let disc = read_disc(&source).expect("read test disc");

    assert_eq!(disc.vts_count, 6, "title sets");
    assert_eq!(disc.titles.len(), 20, "titles");

    let title = |n: u16| {
        disc.titles
            .iter()
            .find(|t| t.number == n)
            .expect("title present")
    };

    // HandBrake reference: title 11 = 02:49:27 / 37 chapters.
    let t11 = title(11);
    assert_eq!(t11.vts, 6);
    assert_eq!(t11.chapters, 37);
    assert_eq!(t11.duration.expect("duration").as_secs(), 10_167);

    // HandBrake reference: title 12 = 02:26:51 / 23 chapters.
    let t12 = title(12);
    assert_eq!(t12.vts, 6);
    assert_eq!(t12.chapters, 23);
    assert_eq!(t12.duration.expect("duration").as_secs(), 8_811);

    // Short menu/warning titles still match.
    assert_eq!(title(3).duration.expect("duration").as_secs(), 14);
    assert_eq!(title(4).duration.expect("duration").as_secs(), 61);
}

/// Remux the 14-second title 3 and check we produced a real Matroska file.
/// Fast enough to run whenever the live disc is available.
#[test]
fn remuxes_a_short_title_to_mkv() {
    let Ok(path) = std::env::var("PACKRAT_TEST_DISC") else {
        eprintln!("PACKRAT_TEST_DISC not set; skipping remux validation");
        return;
    };

    let source = DiscSource::discover(&path).expect("open test disc");
    let disc = read_disc(&source).expect("read test disc");
    let title = disc
        .titles
        .iter()
        .find(|t| t.number == 3)
        .expect("title 3")
        .clone();
    let vts = read_vts(&source, title.vts).expect("read VTS");

    let out = std::env::temp_dir().join(format!("packrat-remux-{}.mkv", std::process::id()));
    let report = remux_title(&source, &vts, &title, &out).expect("remux title 3");

    assert!(report.packets > 0, "wrote packets");
    let bytes = fs::read(&out).expect("read output");
    assert!(bytes.len() > 64 * 1024, "output looks empty");
    assert_eq!(
        &bytes[0..4],
        &[0x1A, 0x45, 0xDF, 0xA3],
        "Matroska files start with the EBML magic"
    );

    let _ = fs::remove_file(&out);
}

/// Progress must be reported for the probe pass and then for muxing, with byte
/// counts that span both passes and finish exactly at the total. Uses the
/// 14-second title 3 so it stays fast.
#[test]
fn progress_reports_probe_then_muxing() {
    let Ok(path) = std::env::var("PACKRAT_TEST_DISC") else {
        eprintln!("PACKRAT_TEST_DISC not set; skipping progress validation");
        return;
    };

    let source = DiscSource::discover(&path).expect("open test disc");
    let disc = read_disc(&source).expect("read test disc");
    let title = disc
        .titles
        .iter()
        .find(|t| t.number == 3)
        .expect("title 3")
        .clone();
    let vts = read_vts(&source, title.vts).expect("read VTS");

    let mut events: Vec<(RemuxPhase, u64, u64)> = Vec::new();
    let out = std::env::temp_dir().join(format!("packrat-progress-{}.mkv", std::process::id()));
    remux_chain_with_progress(&source, &vts, &title, 1, title.chapters, &out, &mut |p| {
        events.push((p.phase, p.bytes_done, p.bytes_total))
    })
    .expect("remux title 3");
    let _ = fs::remove_file(&out);

    let first_probe = events
        .iter()
        .position(|(phase, _, _)| *phase == RemuxPhase::Probing)
        .expect("probe phase reported");
    let first_mux = events
        .iter()
        .position(|(phase, _, _)| *phase == RemuxPhase::Muxing)
        .expect("muxing phase reported");
    assert!(first_probe < first_mux, "probe precedes muxing");

    let bytes_total = events[0].2;
    assert!(bytes_total > 0, "total bytes are known up front");
    assert!(
        events.iter().all(|(_, _, total)| *total == bytes_total),
        "total stays stable across the run"
    );
    assert!(
        events.windows(2).all(|w| w[0].1 <= w[1].1),
        "bytes read never go backwards"
    );
    assert!(
        events.iter().all(|(_, done, total)| done <= total),
        "progress never exceeds the total"
    );
    assert_eq!(
        events.last().unwrap().1,
        bytes_total,
        "exactly the planned bytes are read"
    );
}

/// Device reads must produce exactly the same bytes as the mounted-folder
/// reads. Opt in with `PACKRAT_TEST_DEVICE=/dev/sr0` (and access to the device).
#[test]
fn device_reads_match_folder_reads() {
    let (Ok(mount), Ok(device)) = (
        std::env::var("PACKRAT_TEST_DISC"),
        std::env::var("PACKRAT_TEST_DEVICE"),
    ) else {
        eprintln!("PACKRAT_TEST_DEVICE not set; skipping device validation");
        return;
    };

    let folder = DiscSource::discover(&mount).expect("open folder");
    let raw = DiscSource::discover_device(&device, &mount).expect("open device source");
    let disc = read_disc(&folder).expect("read disc");
    let title = disc
        .titles
        .iter()
        .find(|t| t.number == 3)
        .expect("title 3")
        .clone();
    let vts = read_vts(&folder, title.vts).expect("read VTS");

    let a = std::env::temp_dir().join(format!("packrat-folder-{}.mkv", std::process::id()));
    let b = std::env::temp_dir().join(format!("packrat-device-{}.mkv", std::process::id()));
    remux_chain(&folder, &vts, &title, 1, title.chapters, &a).expect("folder remux");
    remux_chain(&raw, &vts, &title, 1, title.chapters, &b).expect("device remux");

    assert_eq!(fs::read(&a).unwrap(), fs::read(&b).unwrap());

    let _ = fs::remove_file(&a);
    let _ = fs::remove_file(&b);
}

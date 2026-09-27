//! Print the file LBAs `oxideav-dvd` derives, for debugging the device path.
//!
//! ```sh
//! cargo run -p packrat-core --example device_lba -- /dev/sr0
//! ```

fn main() {
    let device = std::env::args().nth(1).unwrap_or_else(|| "/dev/sr0".into());

    let udf = oxideav_dvd::DvdDisc::open(&device).expect("open device (UDF)");
    println!("UDF volume_id = {}", udf.volume_id);
    for f in udf.video_ts_files.iter().take(5) {
        println!("UDF {:?} lba={} size={}", f.kind, f.lba, f.size);
    }

    let file =
        packrat_core::drives::open_device(std::path::Path::new(&device)).expect("open device");
    let iso = oxideav_dvd::DvdDisc::from_iso9660(file).expect("open device (ISO9660)");
    println!("\nISO volume_id = {}", iso.volume_id);
    for f in iso.video_ts_files.iter().take(5) {
        println!("ISO {:?} lba={} size={}", f.kind, f.lba, f.size);
    }
}

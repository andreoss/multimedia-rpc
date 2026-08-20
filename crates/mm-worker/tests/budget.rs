use std::path::Path;

use mm_domain::OutputProfile;
use mm_worker::{DiskBudget, admits, ladder_estimate_bytes};

#[test]
fn admission_accounts_for_reservations() {
    assert!(admits(100, 1000, 0));
    assert!(admits(100, 1000, 900));
    assert!(!admits(101, 1000, 900));
    assert!(!admits(2000, 1000, 0));
}

#[test]
fn a_fixed_budget_reports_its_size() {
    assert_eq!(
        DiskBudget::Fixed(4096).available(Path::new(".")),
        Some(4096)
    );
}

#[test]
fn probing_the_working_directory_reports_space() {
    let available = DiskBudget::Probe
        .available(Path::new("."))
        .expect("the working directory is statable");
    assert!(available > 0);
}

#[test]
fn probing_a_missing_path_reports_nothing() {
    assert_eq!(
        DiskBudget::Probe.available(Path::new("./absent-budget-probe-path")),
        None
    );
}

fn rung(width: u32, height: u32, video_bitrate: u32, audio_bitrate: u32) -> OutputProfile {
    OutputProfile::from_raw(
        24.0,
        video_bitrate,
        23,
        audio_bitrate,
        48_000,
        width,
        height,
    )
    .unwrap()
}

#[test]
fn a_ladder_is_admitted_on_what_it_writes_not_only_on_what_it_reads() {
    let rungs = vec![
        rung(640, 360, 1_000_000, 96_000),
        rung(1280, 720, 4_000_000, 96_000),
    ];
    let without = ladder_estimate_bytes(&rungs, 60.0, None, 1_000);
    assert!(
        without > 1_000,
        "the estimate reserved only the input: {without}"
    );
    let expected = 1_000
        + (1_096_000_f64 / 8.0 * 60.0 * 1.1) as u64
        + (4_096_000_f64 / 8.0 * 60.0 * 1.1) as u64;
    assert!(
        (without as i64 - expected as i64).abs() <= 2,
        "estimate {without} against {expected}"
    );
}

#[test]
fn a_packaged_ladder_reserves_its_segments_on_top_of_its_encodes() {
    let rungs = vec![rung(640, 360, 1_000_000, 96_000)];
    let encoded_only = ladder_estimate_bytes(&rungs, 60.0, None, 0);
    let packaged = ladder_estimate_bytes(&rungs, 60.0, Some(6.0), 0);
    let segments = packaged - encoded_only;
    assert_eq!(
        segments,
        10 * 4 * 1024 * 1024,
        "ten segments at the per-segment estimate: {segments}"
    );
}

#[test]
fn a_six_rung_ladder_reserves_far_more_than_its_input() {
    let rungs: Vec<OutputProfile> = (0..6)
        .map(|index| {
            let step = index as u32 + 1;
            rung(320 * step, 180 * step, 400_000 * step, 96_000)
        })
        .collect();
    let estimate = ladder_estimate_bytes(&rungs, 600.0, Some(6.0), 50_000_000);
    assert!(
        estimate > 50_000_000,
        "the estimate is the input's own size: {estimate}"
    );
}

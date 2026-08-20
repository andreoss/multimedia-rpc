mod common;

use std::time::Duration;

use common::{fixture, scratch_dir};
use mm_ffmpeg::{
    ImageFormat, ProbeLimits, Prober, RunLimits, RunOutcome, Runner, ThumbnailTarget, Timestamp,
    build_thumbnail_args,
};

const TEST_CAP: Duration = Duration::from_secs(180);

fn target(width: u32, height: u32) -> ThumbnailTarget {
    ThumbnailTarget {
        width,
        height,
        format: ImageFormat::Jpeg,
    }
}

fn limits() -> RunLimits {
    RunLimits {
        total_duration: Duration::from_secs(2),
        deadline: Duration::from_secs(120),
        stderr_tail_bytes: 8 * 1024,
    }
}

async fn extract(input: &std::path::Path, offset: f64, name: &str) -> std::path::PathBuf {
    let output = scratch_dir().join(name);
    let args = build_thumbnail_args(
        input.to_string_lossy().as_ref(),
        Timestamp::from_micros((offset * 1_000_000.0).round() as u64),
        &target(160, 120),
        output.to_string_lossy().as_ref(),
    );
    let run = Runner::default()
        .start(&args, &limits())
        .expect("the tool starts");
    let outcome = tokio::time::timeout(TEST_CAP, run.wait())
        .await
        .expect("the run finishes inside the cap")
        .expect("the run is waited");
    assert_eq!(outcome, RunOutcome::Succeeded, "extracting {name}");
    output
}

#[tokio::test(flavor = "multi_thread")]
async fn a_thumbnail_lands_an_image_at_the_requested_size() {
    let input = fixture("thumbnail-in.mp4", 6, 30, 640, 360);

    let output = extract(&input, 1.0, "thumbnail-1.jpg").await;

    let still = Prober::default()
        .still(&output.to_string_lossy(), &ProbeLimits::default())
        .await
        .expect("the still image reports");
    assert_eq!((still.width, still.height), (160, 120));
    assert!(
        still.size_bytes.unwrap_or_default() > 0,
        "the image is empty"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn thumbnails_at_several_offsets_land_separate_images() {
    let input = fixture("thumbnail-offsets.mp4", 8, 30, 640, 360);

    let first = extract(&input, 0.0, "thumbnail-first.jpg").await;
    let second = extract(&input, 3.0, "thumbnail-second.jpg").await;

    let first_still = Prober::default()
        .still(&first.to_string_lossy(), &ProbeLimits::default())
        .await
        .expect("the first still reports");
    let second_still = Prober::default()
        .still(&second.to_string_lossy(), &ProbeLimits::default())
        .await
        .expect("the second still reports");
    assert_eq!(
        (first_still.width, first_still.height),
        (second_still.width, second_still.height)
    );
    assert_ne!(
        first_still.size_bytes, second_still.size_bytes,
        "both offsets produced the same frame"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_thumbnail_at_a_size_larger_than_the_source_pads_it() {
    let input = fixture("thumbnail-pad.mp4", 4, 30, 320, 180);
    let output = scratch_dir().join("thumbnail-pad.jpg");
    let args = build_thumbnail_args(
        input.to_string_lossy().as_ref(),
        Timestamp::from_micros(1_000_000),
        &target(640, 480),
        output.to_string_lossy().as_ref(),
    );
    let run = Runner::default()
        .start(&args, &limits())
        .expect("the tool starts");

    let outcome = tokio::time::timeout(TEST_CAP, run.wait())
        .await
        .expect("the run finishes inside the cap")
        .expect("the run is waited");

    assert_eq!(outcome, RunOutcome::Succeeded);
    let still = Prober::default()
        .still(&output.to_string_lossy(), &ProbeLimits::default())
        .await
        .expect("the padded still reports");
    assert_eq!((still.width, still.height), (640, 480));
}

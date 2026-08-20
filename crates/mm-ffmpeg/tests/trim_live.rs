mod common;

use std::time::Duration;

use common::{fixture, keyed_fixture, rational_fixture, scratch_dir};
use mm_ffmpeg::{
    ProbeLimits, Prober, RunLimits, RunOutcome, Runner, Timestamp, TrimStrategy, TrimTarget,
    aligned_to_keyframe, build_trim_args,
};

const TEST_CAP: Duration = Duration::from_secs(180);

fn at(seconds: f64) -> Timestamp {
    Timestamp::from_micros((seconds * 1_000_000.0).round() as u64)
}

const NTSC_FRAME_MICROS: u64 = 41_708;

fn limits(total: Duration) -> RunLimits {
    RunLimits {
        total_duration: total,
        deadline: Duration::from_secs(120),
        stderr_tail_bytes: 8 * 1024,
    }
}

fn trim_target(strategy: TrimStrategy) -> TrimTarget {
    TrimTarget {
        strategy,
        ..TrimTarget::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_copy_trim_from_a_keyframe_lands_the_requested_length() {
    let input = keyed_fixture("trim-copy-keyed.mp4", 10, 30, 640, 360, 30);
    let keyframes = Prober::default()
        .keyframes(input.to_string_lossy().as_ref(), &ProbeLimits::default())
        .await
        .expect("keyframes are read");
    let start = *keyframes
        .iter()
        .find(|keyframe| **keyframe > 2.0)
        .expect("the keyed fixture carries a later keyframe");
    let output = scratch_dir().join("trim-copy.mp4");
    let args = build_trim_args(
        input.to_string_lossy().as_ref(),
        &trim_target(TrimStrategy::Copy),
        at(start),
        at(2.0),
        output.to_string_lossy().as_ref(),
    )
    .expect("copy argv builds");
    let run = Runner::default()
        .start(&args, &limits(Duration::from_secs(2)))
        .expect("the tool starts");

    let outcome = tokio::time::timeout(TEST_CAP, run.wait())
        .await
        .expect("the run finishes inside the cap")
        .expect("the run is waited");

    assert_eq!(outcome, RunOutcome::Succeeded);
    let info = Prober::default()
        .probe(output.to_string_lossy().as_ref(), &ProbeLimits::default())
        .await
        .expect("the trimmed clip probes");
    assert!(
        (info.duration.as_secs_f64() - 2.0).abs() < 0.2,
        "the copied clip is {}s long from a cut at {start}s",
        info.duration.as_secs_f64()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_copy_trim_between_keyframes_starts_at_the_earlier_keyframe() {
    let input = keyed_fixture("trim-copy-unaligned.mp4", 10, 30, 640, 360, 30);
    let keyframes = Prober::default()
        .keyframes(input.to_string_lossy().as_ref(), &ProbeLimits::default())
        .await
        .expect("keyframes are read");
    let unaligned = keyframes
        .iter()
        .copied()
        .find(|keyframe| *keyframe > 2.0)
        .expect("the keyed fixture carries a later keyframe")
        + 0.5;
    assert!(!aligned_to_keyframe(&keyframes, unaligned));
    let output = scratch_dir().join("trim-copy-unaligned.mp4");
    let args = build_trim_args(
        input.to_string_lossy().as_ref(),
        &trim_target(TrimStrategy::Copy),
        at(unaligned),
        at(2.0),
        output.to_string_lossy().as_ref(),
    )
    .expect("copy argv builds");
    let run = Runner::default()
        .start(&args, &limits(Duration::from_secs(3)))
        .expect("the tool starts");

    let outcome = tokio::time::timeout(TEST_CAP, run.wait())
        .await
        .expect("the run finishes inside the cap")
        .expect("the run is waited");

    assert_eq!(outcome, RunOutcome::Succeeded);
    let info = Prober::default()
        .probe(output.to_string_lossy().as_ref(), &ProbeLimits::default())
        .await
        .expect("the trimmed clip probes");
    assert!(
        info.duration.as_secs_f64() > 2.0,
        "a copied cut between keyframes lands longer than asked, not shorter"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reencoding_trim_normalizes_and_lands_the_requested_length() {
    let input = fixture("trim-reencode-in.mp4", 8, 30, 640, 360);
    let output = scratch_dir().join("trim-reencode.mp4");
    let args = build_trim_args(
        input.to_string_lossy().as_ref(),
        &trim_target(TrimStrategy::Reencode),
        at(1.0),
        at(2.5),
        output.to_string_lossy().as_ref(),
    )
    .expect("re-encoding argv builds");
    let run = Runner::default()
        .start(&args, &limits(Duration::from_secs(3)))
        .expect("the tool starts");

    let outcome = tokio::time::timeout(TEST_CAP, run.wait())
        .await
        .expect("the run finishes inside the cap")
        .expect("the run is waited");

    assert_eq!(outcome, RunOutcome::Succeeded);
    let info = Prober::default()
        .probe(output.to_string_lossy().as_ref(), &ProbeLimits::default())
        .await
        .expect("the trimmed clip probes");
    assert!(
        (info.duration.as_secs_f64() - 2.5).abs() < 0.3,
        "the re-encoded clip is {}s long",
        info.duration.as_secs_f64()
    );
    let video = info.video.first().expect("the clip carries video");
    let audio = info.audio.first().expect("the clip carries audio");
    assert_eq!(video.codec, "h264");
    assert_eq!(audio.codec, "aac");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_first_frame_of_a_fixture_is_reported_as_a_keyframe() {
    let input = fixture("trim-keyframes-in.mp4", 4, 30, 320, 180);

    let keyframes = Prober::default()
        .keyframes(input.to_string_lossy().as_ref(), &ProbeLimits::default())
        .await
        .expect("keyframes are read");

    assert!(!keyframes.is_empty(), "no keyframe was reported");
    assert!(aligned_to_keyframe(&keyframes, 0.0));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_trim_at_an_ntsc_frame_boundary_lands_one_frame_of_error_or_less() {
    let input = rational_fixture("trim-ntsc-in.mp4", 4, "24000/1001", 320, 180);
    let output = scratch_dir().join("trim-ntsc.mp4");
    let start = Timestamp::from_micros(24 * NTSC_FRAME_MICROS);
    let duration = Timestamp::from_micros(24 * NTSC_FRAME_MICROS);
    let args = build_trim_args(
        input.to_string_lossy().as_ref(),
        &trim_target(TrimStrategy::Reencode),
        start,
        duration,
        output.to_string_lossy().as_ref(),
    )
    .expect("re-encoding argv builds");
    let run = Runner::default()
        .start(&args, &limits(Duration::from_secs(2)))
        .expect("the tool starts");

    let outcome = tokio::time::timeout(TEST_CAP, run.wait())
        .await
        .expect("the run finishes inside the cap")
        .expect("the run is waited");

    assert_eq!(outcome, RunOutcome::Succeeded);
    let info = Prober::default()
        .probe(output.to_string_lossy().as_ref(), &ProbeLimits::default())
        .await
        .expect("the trimmed clip probes");
    let frame_seconds = NTSC_FRAME_MICROS as f64 / 1_000_000.0;
    let wanted = duration.seconds();
    assert!(
        (info.duration.as_secs_f64() - wanted).abs() <= frame_seconds,
        "the clip is {}s for a {wanted}s cut at a {frame_seconds}s frame",
        info.duration.as_secs_f64()
    );
}

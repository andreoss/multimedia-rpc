mod common;

use std::time::Duration;

use mm_ffmpeg::{
    ProbeLimits, Prober, ScaleMode, SourceCut, TargetProfile, TranscodeTarget, build_transcode_args,
};

#[tokio::test(flavor = "multi_thread")]
async fn transcoding_a_fixture_matches_the_requested_parameters() {
    let input = common::fixture("transcode-in-30fps-720p.mp4", 4, 30, 1280, 720);
    let output = common::scratch_dir().join("transcode-out.mp4");

    let target = TranscodeTarget {
        scale_mode: ScaleMode::PadToFit,
        profile: TargetProfile {
            resolution: Some((640, 360)),
            fps: 24,
            audio_sample_rate_hz: 48_000,
            ..TargetProfile::default()
        },
        ..TranscodeTarget::default()
    };
    let args =
        build_transcode_args(input.to_str().unwrap(), &target, output.to_str().unwrap()).unwrap();

    let status = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new("ffmpeg")
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .expect("transcode must finish inside the timeout")
    .unwrap();
    assert!(status.success());

    let info = Prober::default()
        .probe(output.to_str().unwrap(), &ProbeLimits::default())
        .await
        .unwrap();

    assert!(info.container.split(',').any(|token| token == "mp4"));
    assert!(
        (info.duration.as_secs_f64() - 4.0).abs() < 0.5,
        "duration {:?} drifted from the source",
        info.duration
    );
    assert_eq!(info.video.len(), 1);
    assert_eq!(info.video[0].codec, "h264");
    assert_eq!(info.video[0].width, 640);
    assert_eq!(info.video[0].height, 360);
    assert!((info.video[0].avg_fps.unwrap() - 24.0).abs() < 0.1);
    assert_eq!(info.audio.len(), 1);
    assert_eq!(info.audio[0].codec, "aac");
    assert_eq!(info.audio[0].channels, 2);
    assert_eq!(info.audio[0].sample_rate, Some(48_000));

    assert!(
        common::moov_precedes_mdat(&output),
        "the output is not faststart"
    );
}

async fn run(args: &[String]) {
    let status = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new("ffmpeg")
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .expect("transcode must finish inside the timeout")
    .unwrap();
    assert!(status.success());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_source_at_half_speed_lands_twice_the_duration() {
    let input = common::keyed_fixture("cuts-half-speed-8s.mp4", 8, 30, 640, 360, 1);
    let output = common::scratch_dir().join("cuts-half-speed.mp4");

    let target = TranscodeTarget {
        speed: Some(0.5),
        ..TranscodeTarget::default()
    };
    let args =
        build_transcode_args(input.to_str().unwrap(), &target, output.to_str().unwrap()).unwrap();
    run(&args).await;

    let info = Prober::default()
        .probe(output.to_str().unwrap(), &ProbeLimits::default())
        .await
        .unwrap();
    assert!(
        (info.duration.as_secs_f64() - 16.0).abs() < 1.0,
        "half speed produced {:?}",
        info.duration
    );
    assert_eq!(info.video.len(), 1);
    assert_eq!(info.audio.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_trimmed_source_lands_the_requested_duration() {
    let input = common::keyed_fixture("cuts-trim-8s.mp4", 8, 30, 640, 360, 1);
    let output = common::scratch_dir().join("cuts-trim.mp4");

    let target = TranscodeTarget {
        cut: Some(SourceCut {
            start_seconds: 2.0,
            duration_seconds: 3.0,
        }),
        ..TranscodeTarget::default()
    };
    let args =
        build_transcode_args(input.to_str().unwrap(), &target, output.to_str().unwrap()).unwrap();
    run(&args).await;

    let info = Prober::default()
        .probe(output.to_str().unwrap(), &ProbeLimits::default())
        .await
        .unwrap();
    assert!(
        (info.duration.as_secs_f64() - 3.0).abs() < 0.6,
        "the trimmed source produced {:?}",
        info.duration
    );
}

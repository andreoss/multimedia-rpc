mod common;

use std::time::Duration;

use mm_ffmpeg::{ProbeLimits, Prober, TargetProfile, build_concat_args};

#[tokio::test(flavor = "multi_thread")]
async fn merging_two_fixtures_yields_a_faststart_mp4_with_the_summed_duration() {
    let first = common::fixture("merge-a-30fps-720p.mp4", 6, 30, 1280, 720);
    let second = common::fixture("merge-b-25fps-480p.mp4", 4, 25, 640, 480);
    let output = common::scratch_dir().join("merge-basic.mp4");

    let inputs = vec![
        first.to_str().unwrap().to_string(),
        second.to_str().unwrap().to_string(),
    ];
    let args = build_concat_args(
        &inputs,
        &TargetProfile::default(),
        Some((1280, 720)),
        output.to_str().unwrap(),
    )
    .unwrap();

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
    .expect("merge must finish inside the timeout")
    .unwrap();
    assert!(status.success());

    let info = Prober::default()
        .probe(output.to_str().unwrap(), &ProbeLimits::default())
        .await
        .unwrap();

    assert!(info.container.split(',').any(|token| token == "mp4"));
    assert!(
        (info.duration.as_secs_f64() - 10.0).abs() < 0.5,
        "duration {:?} is not the sum of the inputs",
        info.duration
    );
    assert_eq!(info.video.len(), 1);
    assert_eq!(info.video[0].codec, "h264");
    assert_eq!(info.video[0].width, 1280);
    assert_eq!(info.video[0].height, 720);
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

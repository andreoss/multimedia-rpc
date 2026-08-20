use mm_ffmpeg::{ScaleMode, TranscodeTarget, build_transcode_args};
use proptest::prelude::*;

fn target(speed: f64, volume: f64) -> TranscodeTarget {
    TranscodeTarget {
        scale_mode: ScaleMode::PadToFit,
        speed: Some(speed),
        volume: Some(volume),
        ..TranscodeTarget::default()
    }
}

proptest! {
    #[test]
    fn a_speed_factor_reaches_both_the_video_and_audio_graphs(
        speed in 0.5f64..=2.0,
    ) {
        let args = build_transcode_args("in.mp4", &target(speed, 1.0), "out.mp4").unwrap();
        let video = &args[args.iter().position(|arg| arg == "-vf").unwrap() + 1];
        let audio = &args[args.iter().position(|arg| arg == "-af").unwrap() + 1];
        let video_expected = format!("setpts=PTS/{speed:?}");
        let audio_expected = format!("atempo={speed:?}");
        prop_assert!(video.contains(&video_expected));
        prop_assert!(audio.contains(&audio_expected));
    }

    #[test]
    fn a_volume_factor_reaches_the_audio_graph(
        volume in 0.0f64..=8.0,
    ) {
        let args = build_transcode_args("in.mp4", &target(1.0, volume), "out.mp4").unwrap();
        let audio = &args[args.iter().position(|arg| arg == "-af").unwrap() + 1];
        let expected = format!("volume={volume:?}");
        prop_assert!(audio.ends_with(&expected));
    }
}

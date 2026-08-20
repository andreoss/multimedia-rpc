use mm_ffmpeg::{
    KEYFRAME_TOLERANCE_SECONDS, TargetProfile, TrimError, TrimStrategy, TrimTarget,
    aligned_to_keyframe, build_trim_args,
};

const INPUT: &str = "http://127.0.0.1:9/in.mp4";
const OUTPUT: &str = "scratch/out.mp4";

fn target(strategy: TrimStrategy) -> TrimTarget {
    TrimTarget {
        strategy,
        profile: TargetProfile::default(),
    }
}

#[test]
fn a_copy_trim_seeks_before_the_input_and_copies_streams() {
    let args = build_trim_args(INPUT, &target(TrimStrategy::Copy), 2.0, 3.5, OUTPUT).unwrap();

    let expected: Vec<String> = [
        "-y",
        "-nostdin",
        "-hide_banner",
        "-ss",
        "2.000",
        "-i",
        INPUT,
        "-t",
        "3.500",
        "-c",
        "copy",
        "-avoid_negative_ts",
        "make_zero",
        "-movflags",
        "+faststart",
        "-f",
        "mp4",
        OUTPUT,
    ]
    .into_iter()
    .map(str::to_string)
    .collect();

    assert_eq!(args, expected);
}

#[test]
fn a_reencoding_trim_normalizes_with_the_shared_profile() {
    let args = build_trim_args(INPUT, &target(TrimStrategy::Reencode), 0.5, 2.0, OUTPUT).unwrap();

    let expected: Vec<String> = [
        "-y",
        "-nostdin",
        "-hide_banner",
        "-ss",
        "0.500",
        "-i",
        INPUT,
        "-t",
        "2.000",
        "-vf",
        "fps=24,format=yuv420p",
        "-af",
        "aformat=sample_fmts=fltp:sample_rates=48000:channel_layouts=stereo",
        "-c:v",
        "libx264",
        "-crf",
        "23",
        "-maxrate",
        "5000k",
        "-bufsize",
        "10000k",
        "-c:a",
        "aac",
        "-b:a",
        "128k",
        "-ar",
        "48000",
        "-ac",
        "2",
        "-movflags",
        "+faststart",
        "-f",
        "mp4",
        OUTPUT,
    ]
    .into_iter()
    .map(str::to_string)
    .collect();

    assert_eq!(args, expected);
}

#[test]
fn a_copy_trim_carries_no_encoder_flags() {
    let args = build_trim_args(INPUT, &target(TrimStrategy::Copy), 0.0, 1.0, OUTPUT).unwrap();

    assert!(!args.iter().any(|arg| arg == "-c:v"));
    assert!(!args.iter().any(|arg| arg == "-crf"));
    assert!(args.windows(2).any(|pair| pair == ["-c", "copy"]));
}

#[test]
fn a_start_on_a_keyframe_is_aligned() {
    let keyframes = [0.0, 1.0, 2.0, 3.0];

    assert!(aligned_to_keyframe(&keyframes, 0.0));
    assert!(aligned_to_keyframe(&keyframes, 2.0));
}

#[test]
fn a_start_between_keyframes_is_not_aligned() {
    let keyframes = [0.0, 1.0, 2.0, 3.0];

    assert!(!aligned_to_keyframe(&keyframes, 1.5));
    assert!(!aligned_to_keyframe(&keyframes, 4.0));
}

#[test]
fn a_start_inside_the_keyframe_tolerance_is_aligned() {
    let keyframes = [0.0, 2.0];

    assert!(aligned_to_keyframe(
        &keyframes,
        2.0 + KEYFRAME_TOLERANCE_SECONDS / 2.0
    ));
    assert!(!aligned_to_keyframe(
        &keyframes,
        2.0 + KEYFRAME_TOLERANCE_SECONDS * 2.0
    ));
}

#[test]
fn a_missing_keyframe_list_never_aligns() {
    assert!(!aligned_to_keyframe(&[], 0.0));
}

#[test]
fn an_unsupported_channel_count_is_rejected_before_argv() {
    let trim = TrimTarget {
        strategy: TrimStrategy::Reencode,
        profile: TargetProfile {
            audio_channels: 3,
            ..TargetProfile::default()
        },
    };

    let error = build_trim_args(INPUT, &trim, 0.0, 1.0, OUTPUT).unwrap_err();

    assert!(matches!(
        error,
        TrimError::UnsupportedChannelCount { channels: 3 }
    ));
}

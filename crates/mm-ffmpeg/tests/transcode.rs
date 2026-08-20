use mm_ffmpeg::{
    AudioCodec, ScaleMode, SourceCut, TargetProfile, Timestamp, TranscodeError, TranscodeTarget,
    VideoCodec, build_transcode_args,
};

const INPUT: &str = "http://127.0.0.1:9/in.mp4";

fn target(resolution: Option<(u32, u32)>, scale_mode: ScaleMode) -> TranscodeTarget {
    TranscodeTarget {
        video_codec: VideoCodec::H264,
        audio_codec: AudioCodec::Aac,
        scale_mode,
        profile: TargetProfile {
            resolution,
            ..TargetProfile::default()
        },
        ..TranscodeTarget::default()
    }
}

fn argument_after<'a>(args: &'a [String], flag: &str) -> &'a str {
    let position = args.iter().position(|arg| arg == flag).unwrap();
    &args[position + 1]
}

#[test]
fn a_resolution_target_normalizes_with_scale_and_pad_by_default() {
    let args = build_transcode_args(
        INPUT,
        &target(Some((640, 360)), ScaleMode::PadToFit),
        "scratch/out.mp4",
    )
    .unwrap();

    let expected: Vec<String> = [
        "-y",
        "-nostdin",
        "-hide_banner",
        "-i",
        INPUT,
        "-vf",
        "scale=w=640:h=360:force_original_aspect_ratio=decrease,\
         pad=w=640:h=360:x=(ow-iw)/2:y=(oh-ih)/2,setsar=1,fps=24,format=yuv420p",
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
        "scratch/out.mp4",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();

    assert_eq!(args, expected);
}

#[test]
fn a_stretch_target_scales_without_padding() {
    let args = build_transcode_args(
        INPUT,
        &target(Some((640, 360)), ScaleMode::Stretch),
        "scratch/out.mp4",
    )
    .unwrap();

    let filter_index = args.iter().position(|arg| arg == "-vf").unwrap();
    assert_eq!(
        args[filter_index + 1],
        "scale=w=640:h=360,setsar=1,fps=24,format=yuv420p"
    );
    assert!(!args[filter_index + 1].contains("pad="));
}

#[test]
fn no_resolution_keeps_the_source_frame_size() {
    let args =
        build_transcode_args(INPUT, &target(None, ScaleMode::PadToFit), "scratch/out.mp4").unwrap();

    let filter_index = args.iter().position(|arg| arg == "-vf").unwrap();
    assert_eq!(args[filter_index + 1], "fps=24,format=yuv420p");
    assert!(!args.iter().any(|arg| arg.contains("scale=")));
}

#[test]
fn explicit_profile_overrides_replace_every_default() {
    let mut target = target(Some((320, 180)), ScaleMode::PadToFit);
    target.profile.fps = 30;
    target.profile.video_crf = 20;
    target.profile.video_max_bitrate_kbps = 2500;
    target.profile.audio_bitrate_kbps = 96;
    target.profile.audio_sample_rate_hz = 44_100;
    target.profile.audio_channels = 1;
    let args = build_transcode_args(INPUT, &target, "scratch/o.mp4").unwrap();

    let position = |flag: &str| args.iter().position(|arg| arg == flag).unwrap();
    assert!(args[position("-vf") + 1].contains("fps=30,"));
    assert_eq!(
        args[position("-af") + 1],
        "aformat=sample_fmts=fltp:sample_rates=44100:channel_layouts=mono"
    );
    assert_eq!(args[position("-crf") + 1], "20");
    assert_eq!(args[position("-maxrate") + 1], "2500k");
    assert_eq!(args[position("-bufsize") + 1], "5000k");
    assert_eq!(args[position("-b:a") + 1], "96k");
    assert_eq!(args[position("-ar") + 1], "44100");
    assert_eq!(args[position("-ac") + 1], "1");
}

#[test]
fn an_unsupported_channel_count_is_rejected() {
    let mut target = target(None, ScaleMode::PadToFit);
    target.profile.audio_channels = 6;
    let error = build_transcode_args(INPUT, &target, "scratch/out.mp4").unwrap_err();
    assert!(matches!(
        error,
        TranscodeError::UnsupportedChannelCount { channels: 6 }
    ));
}

#[test]
fn an_uncut_source_reads_the_input_without_seeking() {
    let args =
        build_transcode_args(INPUT, &target(None, ScaleMode::PadToFit), "scratch/out.mp4").unwrap();
    let head: Vec<&String> = args[..5].iter().collect();
    assert_eq!(head[0], "-y");
    assert_eq!(head[3], "-i");
    assert_eq!(head[4], INPUT);
    assert!(!args.iter().any(|arg| arg == "-ss" || arg == "-t"));
}

#[test]
fn a_trimmed_source_bounds_what_it_reads_before_the_input() {
    let mut target = target(None, ScaleMode::PadToFit);
    target.cut = Some(SourceCut {
        start: Timestamp::from_micros(1_000_000),
        duration: Timestamp::from_micros(2_500_000),
    });
    let args = build_transcode_args(INPUT, &target, "scratch/out.mp4").unwrap();

    let input_position = args.iter().position(|arg| arg == "-i").unwrap();
    let head: Vec<&String> = args[..input_position + 2].iter().collect();
    assert_eq!(
        head,
        vec![
            "-y",
            "-nostdin",
            "-hide_banner",
            "-ss",
            "1.000000",
            "-t",
            "2.500000",
            "-i",
            INPUT
        ]
    );
}

#[test]
fn a_slow_source_retimes_the_picture_and_the_sound_together() {
    let mut target = target(None, ScaleMode::PadToFit);
    target.speed = Some(0.5);
    let args = build_transcode_args(INPUT, &target, "scratch/out.mp4").unwrap();

    assert_eq!(
        argument_after(&args, "-vf"),
        "setpts=PTS/0.5,fps=24,format=yuv420p"
    );
    assert_eq!(
        argument_after(&args, "-af"),
        "aformat=sample_fmts=fltp:sample_rates=48000:channel_layouts=stereo,atempo=0.5"
    );
}

#[test]
fn a_quiet_source_scales_the_sound_only() {
    let mut target = target(None, ScaleMode::PadToFit);
    target.volume = Some(0.25);
    let args = build_transcode_args(INPUT, &target, "scratch/out.mp4").unwrap();

    assert_eq!(argument_after(&args, "-vf"), "fps=24,format=yuv420p");
    assert_eq!(
        argument_after(&args, "-af"),
        "aformat=sample_fmts=fltp:sample_rates=48000:channel_layouts=stereo,volume=0.25"
    );
}

#[test]
fn a_cut_a_speed_and_a_volume_travel_on_one_command() {
    let mut target = target(Some((640, 360)), ScaleMode::PadToFit);
    target.cut = Some(SourceCut {
        start: Timestamp::from_micros(500_000),
        duration: Timestamp::from_micros(3_000_000),
    });
    target.speed = Some(2.0);
    target.volume = Some(1.5);
    let args = build_transcode_args(INPUT, &target, "scratch/out.mp4").unwrap();

    assert_eq!(argument_after(&args, "-ss"), "0.500000");
    assert_eq!(argument_after(&args, "-t"), "3.000000");
    assert!(argument_after(&args, "-vf").starts_with("setpts=PTS/2.0,"));
    assert!(argument_after(&args, "-af").ends_with(",atempo=2.0,volume=1.5"));
}

#[test]
fn a_range_bounds_the_output_after_the_input() {
    let mut target = target(None, ScaleMode::PadToFit);
    target.range = Some((
        Timestamp::from_micros(1_000_000),
        Timestamp::from_micros(2_000_000),
    ));
    let args = build_transcode_args(INPUT, &target, "scratch/out.mp4").unwrap();
    let input_position = args.iter().position(|arg| arg == "-i").unwrap();
    assert_eq!(
        &args[input_position + 2..input_position + 6],
        &["-ss", "1.000000", "-t", "2.000000"]
    );
}

use mm_ffmpeg::{ConcatError, TargetProfile, build_concat_args};

fn urls(count: usize) -> Vec<String> {
    (0..count)
        .map(|index| format!("http://127.0.0.1:9/in-{index}.mp4"))
        .collect()
}

#[test]
fn two_inputs_with_defaults_normalize_to_the_first_input_resolution() {
    let inputs = urls(2);
    let args = build_concat_args(
        &inputs,
        &TargetProfile::default(),
        Some((1280, 720)),
        "scratch/out.mp4",
    )
    .unwrap();

    let filter = "[0:v]scale=w=1280:h=720:force_original_aspect_ratio=decrease,\
         pad=w=1280:h=720:x=(ow-iw)/2:y=(oh-ih)/2,setsar=1,fps=24,format=yuv420p[v0];\
         [0:a]aformat=sample_fmts=fltp:sample_rates=48000:channel_layouts=stereo[a0];\
         [1:v]scale=w=1280:h=720:force_original_aspect_ratio=decrease,\
         pad=w=1280:h=720:x=(ow-iw)/2:y=(oh-ih)/2,setsar=1,fps=24,format=yuv420p[v1];\
         [1:a]aformat=sample_fmts=fltp:sample_rates=48000:channel_layouts=stereo[a1];\
         [v0][a0][v1][a1]concat=n=2:v=1:a=1[vout][aout]"
        .to_string();

    let expected: Vec<String> = [
        "-y",
        "-nostdin",
        "-hide_banner",
        "-i",
        "http://127.0.0.1:9/in-0.mp4",
        "-i",
        "http://127.0.0.1:9/in-1.mp4",
        "-filter_complex",
        &filter,
        "-map",
        "[vout]",
        "-map",
        "[aout]",
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
fn unknown_first_input_resolution_falls_back_to_full_hd() {
    let inputs = urls(3);
    let args =
        build_concat_args(&inputs, &TargetProfile::default(), None, "scratch/out.mp4").unwrap();

    let filter_index = args
        .iter()
        .position(|arg| arg == "-filter_complex")
        .unwrap();
    let filter = &args[filter_index + 1];
    assert!(filter.contains("[0:v]scale=w=1920:h=1080:"));
    assert!(filter.contains("[2:v]scale=w=1920:h=1080:"));
    assert!(filter.contains("[v0][a0][v1][a1][v2][a2]concat=n=3:v=1:a=1[vout][aout]"));
    assert_eq!(args.iter().filter(|arg| *arg == "-i").count(), 3);
}

#[test]
fn explicit_profile_overrides_replace_every_default() {
    let inputs = urls(2);
    let profile = TargetProfile {
        video_encoder: "libx264",
        resolution: Some((640, 360)),
        fps: 30,
        video_crf: 20,
        video_max_bitrate_kbps: 2500,
        audio_bitrate_kbps: 96,
        audio_sample_rate_hz: 44_100,
        audio_channels: 1,
    };
    let args = build_concat_args(&inputs, &profile, Some((1280, 720)), "scratch/o.mp4").unwrap();

    let filter_index = args
        .iter()
        .position(|arg| arg == "-filter_complex")
        .unwrap();
    let filter = &args[filter_index + 1];
    assert!(filter.contains("scale=w=640:h=360:"));
    assert!(filter.contains("pad=w=640:h=360:"));
    assert!(filter.contains("fps=30,"));
    assert!(filter.contains("sample_rates=44100:channel_layouts=mono"));

    let tail: Vec<&str> = args.iter().map(String::as_str).collect();
    let position = |flag: &str| tail.iter().position(|arg| *arg == flag).unwrap();
    assert_eq!(tail[position("-crf") + 1], "20");
    assert_eq!(tail[position("-maxrate") + 1], "2500k");
    assert_eq!(tail[position("-bufsize") + 1], "5000k");
    assert_eq!(tail[position("-b:a") + 1], "96k");
    assert_eq!(tail[position("-ar") + 1], "44100");
    assert_eq!(tail[position("-ac") + 1], "1");
}

#[test]
fn fewer_than_two_inputs_is_rejected() {
    let inputs = urls(1);
    let error =
        build_concat_args(&inputs, &TargetProfile::default(), None, "scratch/out.mp4").unwrap_err();
    assert!(matches!(error, ConcatError::TooFewInputs { count: 1 }));
}

#[test]
fn an_unsupported_channel_count_is_rejected() {
    let inputs = urls(2);
    let profile = TargetProfile {
        audio_channels: 6,
        ..TargetProfile::default()
    };
    let error = build_concat_args(&inputs, &profile, None, "scratch/out.mp4").unwrap_err();
    assert!(matches!(
        error,
        ConcatError::UnsupportedChannelCount { channels: 6 }
    ));
}

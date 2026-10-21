use mm_ffmpeg::{
    TargetProfile, build_caption_extract_args, build_subtitle_burn_args, build_subtitle_mux_args,
    build_subtitle_strip_args,
};

const INPUT: &str = "http://127.0.0.1:9/in.mp4";
const CAPTIONS: &str = "scratch/captions.srt";
const OUTPUT: &str = "scratch/out.mp4";

fn argument_after<'a>(args: &'a [String], flag: &str) -> &'a str {
    let position = args.iter().position(|arg| arg == flag).unwrap();
    &args[position + 1]
}

#[test]
fn a_burn_reads_the_caption_file_into_the_video_filter() {
    let args = build_subtitle_burn_args(INPUT, CAPTIONS, &TargetProfile::default(), OUTPUT)
        .expect("the burn argv builds");
    assert_eq!(args[0..4], ["-y", "-nostdin", "-hide_banner", "-i"]);
    assert!(argument_after(&args, "-vf").starts_with("subtitles=filename='scratch/captions.srt'"));
    assert!(argument_after(&args, "-vf").ends_with("fps=24,format=yuv420p"));
    assert_eq!(argument_after(&args, "-c:v"), "libx264");
    assert_eq!(args.last().map(String::as_str), Some(OUTPUT));
}

#[test]
fn a_burn_escapes_a_path_that_the_filter_would_read_as_syntax() {
    let args = build_subtitle_burn_args(
        INPUT,
        "scratch/a:b,c'd.srt",
        &TargetProfile::default(),
        OUTPUT,
    )
    .expect("the burn argv builds");
    let filter = argument_after(&args, "-vf");
    assert!(filter.contains("a\\:b\\,c\\'d.srt"), "{filter}");
}

#[test]
fn a_mux_copies_the_media_and_maps_the_caption_track() {
    let args = build_subtitle_mux_args(INPUT, CAPTIONS, OUTPUT);
    assert_eq!(argument_after(&args, "-map"), "0:v?");
    assert_eq!(args.windows(2).filter(|pair| pair[0] == "-map").count(), 3);
    assert!(args.windows(2).any(|pair| pair == ["-map", "1:0"]));
    assert_eq!(argument_after(&args, "-c"), "copy");
    assert_eq!(argument_after(&args, "-c:s"), "mov_text");
    assert_eq!(args.last().map(String::as_str), Some(OUTPUT));
}

#[test]
fn a_strip_copies_every_stream_and_disables_subtitles() {
    let args = build_subtitle_strip_args(INPUT, OUTPUT);
    assert_eq!(argument_after(&args, "-map"), "0");
    assert!(args.windows(2).any(|pair| pair == ["-sn", "-movflags"]));
    assert_eq!(argument_after(&args, "-c"), "copy");
    assert_eq!(args.last().map(String::as_str), Some(OUTPUT));
}

#[test]
fn a_caption_extraction_reads_the_first_subtitle_track_as_text() {
    let args = build_caption_extract_args(INPUT, OUTPUT);
    assert_eq!(
        args,
        vec![
            "-y".to_string(),
            "-nostdin".to_string(),
            "-hide_banner".to_string(),
            "-txt_format".to_string(),
            "text".to_string(),
            "-txt_duration".to_string(),
            "2000".to_string(),
            "-i".to_string(),
            INPUT.to_string(),
            "-map".to_string(),
            "0:s:0".to_string(),
            "-c:s".to_string(),
            "webvtt".to_string(),
            "-f".to_string(),
            "webvtt".to_string(),
            OUTPUT.to_string(),
        ]
    );
}

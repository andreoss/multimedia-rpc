use mm_ffmpeg::{ImageFormat, ThumbnailTarget, Timestamp, build_thumbnail_args};

const INPUT: &str = "http://127.0.0.1:9/in.mp4";

fn target(width: u32, height: u32) -> ThumbnailTarget {
    ThumbnailTarget {
        width,
        height,
        format: ImageFormat::Jpeg,
    }
}

#[test]
fn a_thumbnail_seeks_to_the_offset_and_takes_one_frame_at_the_requested_size() {
    let args = build_thumbnail_args(
        INPUT,
        Timestamp::from_micros(2_500_000),
        &target(320, 180),
        "scratch/frame.jpg",
    );

    let expected: Vec<String> = [
        "-y",
        "-nostdin",
        "-hide_banner",
        "-ss",
        "2.500000",
        "-i",
        INPUT,
        "-frames:v",
        "1",
        "-vf",
        "scale=w=320:h=180:force_original_aspect_ratio=decrease,\
         pad=w=320:h=180:x=(ow-iw)/2:y=(oh-ih)/2,setsar=1,fps=1,format=yuv420p",
        "-q:v",
        "2",
        "-f",
        "image2",
        "scratch/frame.jpg",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();

    assert_eq!(args, expected);
}

#[test]
fn each_offset_reaches_argv_as_its_own_seek() {
    let first = build_thumbnail_args(INPUT, Timestamp::ZERO, &target(160, 120), "scratch/a.jpg");
    let second = build_thumbnail_args(
        INPUT,
        Timestamp::from_micros(1_250_000),
        &target(160, 120),
        "scratch/b.jpg",
    );

    assert_eq!(first.last().map(String::as_str), Some("scratch/a.jpg"));
    assert_eq!(second.last().map(String::as_str), Some("scratch/b.jpg"));
    assert_eq!(seek_of(&first).as_deref(), Some("0.000000"));
    assert_eq!(seek_of(&second).as_deref(), Some("1.250000"));
}

#[test]
fn the_requested_format_names_the_extension() {
    assert_eq!(ImageFormat::Jpeg.extension(), "jpg");
}

fn seek_of(args: &[String]) -> Option<String> {
    args.iter()
        .position(|arg| arg == "-ss")
        .and_then(|index| args.get(index + 1))
        .cloned()
}

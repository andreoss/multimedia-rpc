use mm_magick::{
    ImageFormat, ResizeMode, composite_args, convert_args, identify_args, resize_args,
};

const INPUT: &str = "http://127.0.0.1:9/in.png";
const OUTPUT: &str = "scratch/out.jpg";

#[test]
fn identify_asks_for_one_typed_line_per_frame() {
    let args = identify_args("%m %w %h %[colorspace] %[depth] %b\n", INPUT);

    let expected: Vec<String> = [
        "identify",
        "-format",
        "%m %w %h %[colorspace] %[depth] %b\n",
        INPUT,
    ]
    .into_iter()
    .map(str::to_string)
    .collect();

    assert_eq!(args, expected);
}

#[test]
fn a_fit_resize_bounds_the_frame_inside_the_requested_box() {
    let args = resize_args(INPUT, 320, 240, ResizeMode::Fit, ImageFormat::Jpeg, OUTPUT);

    assert_eq!(
        args,
        vec![
            INPUT.to_string(),
            "-resize".to_string(),
            "320x240".to_string(),
            "jpg:scratch/out.jpg".to_string(),
        ]
    );
}

#[test]
fn an_exact_resize_forces_the_requested_frame() {
    let args = resize_args(
        INPUT,
        320,
        240,
        ResizeMode::Exact,
        ImageFormat::Png,
        "scratch/o.png",
    );

    assert!(args.iter().any(|arg| arg == "320x240!"), "{args:?}");
    assert_eq!(args.last().map(String::as_str), Some("png:scratch/o.png"));
}

#[test]
fn a_fill_resize_covers_the_box_and_extents_to_it() {
    let args = resize_args(INPUT, 100, 100, ResizeMode::Fill, ImageFormat::Jpeg, OUTPUT);

    assert!(args.iter().any(|arg| arg == "100x100^"), "{args:?}");
    assert!(args.iter().any(|arg| arg == "-extent"), "{args:?}");
    assert!(args.iter().any(|arg| arg == "center"), "{args:?}");
    assert_eq!(args.last().map(String::as_str), Some("jpg:scratch/out.jpg"));
}

#[test]
fn a_convert_writes_the_requested_coder_and_keeps_the_frame() {
    let args = convert_args(INPUT, ImageFormat::Jpeg, OUTPUT);

    assert_eq!(
        args,
        vec![INPUT.to_string(), "jpg:scratch/out.jpg".to_string()]
    );
    assert_eq!(ImageFormat::Jpeg.extension(), "jpg");
    assert_eq!(ImageFormat::Png.extension(), "png");
}

#[test]
fn a_composite_places_the_overlay_over_the_base() {
    let args = composite_args(
        "http://127.0.0.1:9/overlay.png",
        INPUT,
        ImageFormat::Jpeg,
        OUTPUT,
    );

    let expected: Vec<String> = [
        "composite",
        "-gravity",
        "center",
        "http://127.0.0.1:9/overlay.png",
        INPUT,
        "jpg:scratch/out.jpg",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();

    assert_eq!(args, expected);
}

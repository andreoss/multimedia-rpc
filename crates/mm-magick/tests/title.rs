use mm_magick::{ImageFormat, title_args};

#[test]
fn a_transparent_title_starts_from_an_empty_canvas() {
    let args = title_args(
        "Hello",
        640,
        240,
        48,
        "#ffffff",
        None,
        ImageFormat::Png,
        "scratch/title.png",
    );
    assert_eq!(
        args,
        vec![
            "-size",
            "640x240",
            "xc:none",
            "-gravity",
            "center",
            "-font",
            "DejaVu-Sans",
            "-pointsize",
            "48",
            "-fill",
            "#ffffff",
            "-annotate",
            "+0+0",
            "Hello",
            "png:scratch/title.png",
        ]
    );
}

#[test]
fn a_background_fills_the_canvas_and_the_annotate_text_is_escaped() {
    let args = title_args(
        "a@b",
        320,
        120,
        24,
        "#102030",
        Some("#405060"),
        ImageFormat::Jpeg,
        "scratch/title.jpg",
    );
    assert_eq!(args[2], "xc:#405060");
    assert_eq!(args[13], "a\\@b");
    assert_eq!(
        args.last().map(String::as_str),
        Some("jpg:scratch/title.jpg")
    );
}

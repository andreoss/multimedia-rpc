use mm_magick::{ParseError, parse_identify_output};

#[test]
fn a_full_report_parses() {
    let info = parse_identify_output("PNG 640 480 sRGB 8 362B\n").unwrap();

    assert_eq!(info.format, "PNG");
    assert_eq!(info.width, 640);
    assert_eq!(info.height, 480);
    assert_eq!(info.colorspace, "sRGB");
    assert_eq!(info.depth, 8);
    assert_eq!(info.size_bytes, Some(362));
}

#[test]
fn the_first_frame_line_wins() {
    let info = parse_identify_output("PNG 640 480 sRGB 8 362B\nPNG 64 64 sRGB 8 100B\n").unwrap();

    assert_eq!((info.width, info.height), (640, 480));
}

#[test]
fn sizes_with_unit_suffixes_parse() {
    assert_eq!(
        parse_identify_output("JPEG 16 16 sRGB 8 2KB\n")
            .unwrap()
            .size_bytes,
        Some(2048)
    );
    assert_eq!(
        parse_identify_output("JPEG 16 16 sRGB 8 3MB\n")
            .unwrap()
            .size_bytes,
        Some(3 * 1024 * 1024)
    );
}

#[test]
fn empty_output_is_an_error() {
    assert_eq!(parse_identify_output("  \n"), Err(ParseError::Empty));
}

#[test]
fn a_short_line_is_an_error() {
    assert_eq!(
        parse_identify_output("PNG 640 480 sRGB\n"),
        Err(ParseError::FieldCount { found: 4 })
    );
}

#[test]
fn a_non_numeric_frame_is_an_error() {
    assert_eq!(
        parse_identify_output("PNG wide 480 sRGB 8 362B\n"),
        Err(ParseError::Field {
            field: "width",
            value: "wide".to_string()
        })
    );
}

#[test]
fn an_unknown_size_unit_is_an_error() {
    assert!(matches!(
        parse_identify_output("PNG 640 480 sRGB 8 12ZB\n"),
        Err(ParseError::Field { field: "size", .. })
    ));
}

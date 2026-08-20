use mm_domain::{
    Colour, ImageFormat, MediaRef, OpResult, OpSpec, OpSpecError, TitleResult, TitleSpec,
};
use mm_proto::v1;

fn object() -> MediaRef {
    MediaRef::object_uri("s3://bucket/file.png").unwrap()
}

#[test]
fn a_title_spec_round_trips_its_text_box_colours_and_format() {
    let spec = TitleSpec::new(
        "Hello",
        640,
        240,
        48,
        Colour::parse("text", "#ffffff").unwrap(),
        Some(Colour::parse("background", "#112233").unwrap()),
        ImageFormat::Png,
    )
    .unwrap();
    let decoded = OpSpec::try_from(v1::OpSpec::from(&OpSpec::Title(spec.clone()))).unwrap();
    assert_eq!(decoded, OpSpec::Title(spec));
}

#[test]
fn a_title_spec_with_a_bad_colour_is_refused_by_name() {
    let spec = v1::OpSpec {
        op: Some(v1::op_spec::Op::Title(v1::TitleSpec {
            text: "Hello".to_string(),
            width: 320,
            height: 120,
            point_size: 24,
            colour: "not-a-colour".to_string(),
            background: None,
            format: v1::ImageFormat::Png as i32,
        })),
    };
    assert!(matches!(
        OpSpec::try_from(spec),
        Err(OpSpecError::InvalidColour { field: "text", .. })
    ));
}

#[test]
fn a_title_result_round_trips_its_output() {
    let result = TitleResult::new(object());
    let decoded = OpResult::try_from(v1::OpResult::from(&OpResult::Title(result.clone()))).unwrap();
    assert_eq!(decoded, OpResult::Title(result));
}

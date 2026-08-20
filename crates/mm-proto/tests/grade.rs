use mm_domain::{
    GradeAdjustments, GradeResult, GradeScope, GradeSpec, InputSource, MediaRef, OpResult, OpSpec,
    OpSpecError,
};
use mm_proto::v1;

fn media() -> v1::InputSource {
    v1::InputSource {
        source: Some(v1::input_source::Source::Media(v1::MediaSource {
            media: Some(v1::MediaRef {
                r#ref: Some(v1::media_ref::Ref::ObjectUri(
                    "s3://bucket/file.mp4".to_string(),
                )),
            }),
            trim: None,
            speed: None,
            volume: None,
        })),
    }
}

fn source() -> InputSource {
    InputSource::from(object())
}

fn object() -> MediaRef {
    MediaRef::object_uri("s3://bucket/file.mp4").unwrap()
}

#[test]
fn a_grade_spec_round_trips_adjustments_a_table_and_a_scope() {
    let spec = GradeSpec::new(
        source(),
        GradeAdjustments::new(0.1, 1.2, 1.3, 1.1).unwrap(),
        Some(source()),
        Some(GradeScope::new(320, 180).unwrap()),
    )
    .unwrap();
    let decoded = OpSpec::try_from(v1::OpSpec::from(&OpSpec::Grade(spec.clone()))).unwrap();
    assert_eq!(decoded, OpSpec::Grade(spec));
}

#[test]
fn a_grade_spec_off_the_contract_is_refused_by_name() {
    let spec = v1::OpSpec {
        op: Some(v1::op_spec::Op::Grade(v1::GradeSpec {
            input: Some(media()),
            adjustments: Some(v1::GradeAdjustments {
                brightness: 2.0,
                contrast: 1.0,
                saturation: 1.0,
                gamma: 1.0,
            }),
            lookup_table: None,
            scope: None,
        })),
    };
    assert!(matches!(
        OpSpec::try_from(spec),
        Err(OpSpecError::InvalidBrightness { .. })
    ));
}

#[test]
fn a_grade_result_round_trips_its_two_objects() {
    let result = GradeResult::new(object(), Some(object()));
    let decoded = OpResult::try_from(v1::OpResult::from(&OpResult::Grade(result.clone()))).unwrap();
    assert_eq!(decoded, OpResult::Grade(result));
}

use mm_domain::{
    GradeAdjustments, GradeResult, GradeScope, GradeSpec, InputSource, MediaRef, OpResult,
    OpResultError, OpSpec, OpSpecError, ScopeData, ScopeMode,
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
    let result = GradeResult::new(object(), Some(object()), None);
    let decoded = OpResult::try_from(v1::OpResult::from(&OpResult::Grade(result.clone()))).unwrap();
    assert_eq!(decoded, OpResult::Grade(result));
}

#[test]
fn a_grade_result_round_trips_its_scope_data() {
    let data = ScopeData::new(
        ScopeMode::Parade,
        3,
        2,
        4,
        vec![
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
        ],
    )
    .unwrap();
    let result = GradeResult::new(object(), None, Some(data.clone()));
    let decoded = OpResult::try_from(v1::OpResult::from(&OpResult::Grade(result.clone()))).unwrap();
    let OpResult::Grade(decoded) = decoded else {
        panic!("expected a grade result");
    };
    assert_eq!(decoded.data(), Some(&data));
    assert!(decoded.scope().is_none());
}

#[test]
fn scope_data_that_does_not_fill_its_grid_is_refused_by_name() {
    assert!(matches!(
        OpResult::try_from(v1::OpResult {
            op: Some(v1::op_result::Op::Grade(v1::GradeResult {
                output: Some(v1::MediaRef::from(&object())),
                scope: None,
                data: Some(v1::ScopeData {
                    mode: v1::ScopeMode::Waveform as i32,
                    components: 1,
                    columns: 2,
                    bins: 4,
                    counts: vec![1, 2, 3],
                }),
            })),
        }),
        Err(OpResultError::Scope(_))
    ));
}

#[test]
fn a_scope_asking_for_data_round_trips_its_output_and_grid() {
    use mm_domain::ScopeOutput;

    let scope = GradeScope::new(160, 120)
        .unwrap()
        .with_mode(ScopeMode::Vectorscope)
        .with_output(ScopeOutput::Data)
        .with_grid(96, 48);
    let spec = GradeSpec::new(
        source(),
        GradeAdjustments::new(0.1, 1.0, 1.0, 1.0).unwrap(),
        None,
        Some(scope),
    )
    .unwrap();
    let decoded = OpSpec::try_from(v1::OpSpec::from(&OpSpec::Grade(spec.clone()))).unwrap();
    assert_eq!(decoded, OpSpec::Grade(spec));
    let OpSpec::Grade(grade) = decoded else {
        panic!("expected a grade spec");
    };
    let scope = grade.scope().expect("a scope");
    assert_eq!(scope.output(), ScopeOutput::Data);
    assert_eq!(scope.columns(), 96);
    assert_eq!(scope.bins(), 48);
}

#[test]
fn a_scope_asking_for_data_on_the_wire_is_refused_by_name() {
    let spec = v1::OpSpec {
        op: Some(v1::op_spec::Op::Grade(v1::GradeSpec {
            input: Some(media()),
            adjustments: Some(v1::GradeAdjustments {
                brightness: 0.1,
                contrast: 1.0,
                saturation: 1.0,
                gamma: 1.0,
            }),
            lookup_table: None,
            scope: Some(v1::GradeScope {
                width: 160,
                height: 120,
                mode: v1::ScopeMode::Frame as i32,
                output: v1::ScopeOutput::Data as i32,
                columns: 0,
                bins: 0,
            }),
        })),
    };
    assert!(matches!(
        OpSpec::try_from(spec),
        Err(OpSpecError::FrameScopeData)
    ));
}

#[test]
fn an_unknown_scope_output_is_refused_by_name() {
    let spec = v1::OpSpec {
        op: Some(v1::op_spec::Op::Grade(v1::GradeSpec {
            input: Some(media()),
            adjustments: Some(v1::GradeAdjustments {
                brightness: 0.1,
                contrast: 1.0,
                saturation: 1.0,
                gamma: 1.0,
            }),
            lookup_table: None,
            scope: Some(v1::GradeScope {
                width: 160,
                height: 120,
                mode: v1::ScopeMode::Waveform as i32,
                output: 99,
                columns: 0,
                bins: 0,
            }),
        })),
    };
    assert!(matches!(
        OpSpec::try_from(spec),
        Err(OpSpecError::UnknownScopeOutput { value: 99 })
    ));
}

#[test]
fn a_scope_mode_round_trips_for_every_kind() {
    use mm_domain::ScopeMode;

    for mode in [
        ScopeMode::Frame,
        ScopeMode::Waveform,
        ScopeMode::Parade,
        ScopeMode::Histogram,
        ScopeMode::Vectorscope,
    ] {
        let spec = GradeSpec::new(
            source(),
            GradeAdjustments::new(0.1, 1.0, 1.0, 1.0).unwrap(),
            None,
            Some(GradeScope::new(320, 180).unwrap().with_mode(mode)),
        )
        .unwrap();
        let decoded = OpSpec::try_from(v1::OpSpec::from(&OpSpec::Grade(spec.clone()))).unwrap();
        assert_eq!(decoded, OpSpec::Grade(spec));
    }
}

#[test]
fn an_unknown_scope_mode_is_refused_by_name() {
    let spec = v1::OpSpec {
        op: Some(v1::op_spec::Op::Grade(v1::GradeSpec {
            input: Some(media()),
            adjustments: Some(v1::GradeAdjustments {
                brightness: 0.1,
                contrast: 1.0,
                saturation: 1.0,
                gamma: 1.0,
            }),
            lookup_table: None,
            scope: Some(v1::GradeScope {
                width: 320,
                height: 180,
                mode: 99,
                output: v1::ScopeOutput::Image as i32,
                columns: 0,
                bins: 0,
            }),
        })),
    };
    assert!(matches!(
        OpSpec::try_from(spec),
        Err(OpSpecError::UnknownScopeMode { value: 99 })
    ));
}

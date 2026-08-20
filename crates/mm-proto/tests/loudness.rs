use mm_domain::{
    LoudnessResult, LoudnessSpec, LoudnessTarget, OpResult, OpSpec, OpSpecError, OutputProfile,
};
use mm_proto::v1;

fn media() -> v1::InputSource {
    v1::InputSource {
        source: Some(v1::input_source::Source::Media(v1::MediaSource {
            media: Some(v1::MediaRef {
                r#ref: Some(v1::media_ref::Ref::ObjectUri(
                    "s3://bucket/input.mp4".to_string(),
                )),
            }),
            trim: None,
            speed: None,
            volume: None,
        })),
    }
}

fn source() -> mm_domain::InputSource {
    mm_domain::InputSource::from(mm_domain::MediaRef::object_uri("s3://bucket/input.mp4").unwrap())
}

#[test]
fn a_loudness_spec_round_trips_with_and_without_a_target() {
    let measured = LoudnessSpec::new(source(), None).unwrap();
    let proto = v1::OpSpec::from(&OpSpec::Loudness(measured.clone()));
    let decoded = OpSpec::try_from(proto).unwrap();
    let OpSpec::Loudness(decoded) = decoded else {
        panic!("expected loudness");
    };
    assert_eq!(decoded, measured);

    let target = LoudnessTarget::new(-16.0, -1.5).unwrap();
    let normalize = LoudnessSpec::new(source(), Some(target)).unwrap();
    let proto = v1::OpSpec::from(&OpSpec::Loudness(normalize.clone()));
    let v1::op_spec::Op::Loudness(loudness) = proto.op.unwrap() else {
        panic!("expected loudness arm");
    };
    assert_eq!(
        loudness.target,
        Some(v1::LoudnessTarget {
            integrated_lufs: -16.0,
            true_peak_db: -1.5,
        })
    );
    let decoded = OpSpec::try_from(v1::OpSpec {
        op: Some(v1::op_spec::Op::Loudness(loudness)),
    })
    .unwrap();
    assert_eq!(decoded, OpSpec::Loudness(normalize));
}

#[test]
fn a_loudness_target_outside_its_bounds_is_refused() {
    let spec = v1::OpSpec {
        op: Some(v1::op_spec::Op::Loudness(v1::LoudnessSpec {
            input: Some(media()),
            target: Some(v1::LoudnessTarget {
                integrated_lufs: -100.0,
                true_peak_db: -1.5,
            }),
        })),
    };
    assert!(matches!(
        OpSpec::try_from(spec),
        Err(OpSpecError::InvalidLoudnessTarget { .. })
    ));
}

#[test]
fn a_loudness_result_round_trips_measured_and_normalized_shapes() {
    let measured = LoudnessResult::new(None, -23.0, -3.5, 7.0).unwrap();
    let proto = v1::OpResult::from(&OpResult::Loudness(measured.clone()));
    let v1::op_result::Op::Loudness(result) = proto.op.unwrap() else {
        panic!("expected loudness result");
    };
    assert_eq!(result.output, None);
    let decoded = OpResult::try_from(v1::OpResult {
        op: Some(v1::op_result::Op::Loudness(result)),
    })
    .unwrap();
    assert_eq!(decoded, OpResult::Loudness(measured));

    let output = mm_domain::MediaRef::object_uri("s3://bucket/out.mp4").unwrap();
    let normalized = LoudnessResult::new(Some(output), -16.0, -1.5, 4.0).unwrap();
    let proto = v1::OpResult::from(&OpResult::Loudness(normalized.clone()));
    let decoded = OpResult::try_from(proto).unwrap();
    assert_eq!(decoded, OpResult::Loudness(normalized));
}

#[test]
fn a_loudness_result_off_the_contract_is_refused() {
    let result = v1::OpResult {
        op: Some(v1::op_result::Op::Loudness(v1::LoudnessResult {
            output: None,
            integrated_lufs: f64::NAN,
            true_peak_db: -1.5,
            loudness_range_lu: 4.0,
        })),
    };
    assert!(OpResult::try_from(result).is_err());
}

#[test]
fn the_loudness_spec_keeps_the_profile_untouched() {
    let profile = OutputProfile::from_raw(24.0, 1_000_000, 23, 96_000, 48_000, 640, 360).unwrap();
    let proto = v1::OutputProfile::from(&profile);
    assert_eq!(proto.audio_bitrate, 96_000);
}

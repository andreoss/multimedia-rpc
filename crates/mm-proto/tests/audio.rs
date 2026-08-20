use mm_domain::{
    AudioResult, AudioSpec, InputSource, MediaRef, OpResult, OpSpec, OpSpecError, Ticks,
};
use mm_proto::v1;

fn source() -> InputSource {
    InputSource::from(MediaRef::object_uri("s3://bucket/file.mp4").unwrap())
}

fn object() -> MediaRef {
    MediaRef::object_uri("s3://bucket/file.mp4").unwrap()
}

#[test]
fn an_audio_spec_round_trips_every_field() {
    let spec = AudioSpec::new(
        vec![source(), source()],
        6.0,
        Ticks::from_seconds(1.5).unwrap(),
        Ticks::from_seconds(0.5).unwrap(),
        Some(80),
        Some(12_000),
        Some(-1.0),
    )
    .unwrap();
    let decoded = OpSpec::try_from(v1::OpSpec::from(&OpSpec::Audio(spec.clone()))).unwrap();
    assert_eq!(decoded, OpSpec::Audio(spec));
}

#[test]
fn an_audio_spec_off_the_contract_is_refused_by_name() {
    let spec = v1::OpSpec {
        op: Some(v1::op_spec::Op::Audio(v1::AudioSpec {
            inputs: vec![v1::InputSource {
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
            }],
            gain_db: 30.0,
            fade_in_ticks: 0,
            fade_out_ticks: 0,
            high_pass_hz: None,
            low_pass_hz: None,
            limiter_true_peak_db: None,
        })),
    };
    assert!(matches!(
        OpSpec::try_from(spec),
        Err(OpSpecError::InvalidGain { .. })
    ));
}

#[test]
fn an_audio_result_round_trips_its_duration() {
    let result = AudioResult::new(object(), 5.5).unwrap();
    let decoded = OpResult::try_from(v1::OpResult::from(&OpResult::Audio(result.clone()))).unwrap();
    assert_eq!(decoded, OpResult::Audio(result));
}

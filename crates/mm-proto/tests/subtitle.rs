use mm_domain::{
    CaptionFormat, InputSource, MediaRef, OpResult, OpSpec, OpSpecError, SubtitleMode,
    SubtitleResult, SubtitleSpec,
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
    InputSource::from(MediaRef::object_uri("s3://bucket/file.mp4").unwrap())
}

#[test]
fn a_subtitle_spec_round_trips_in_each_mode() {
    let burn = SubtitleSpec::new(
        source(),
        Some(source()),
        Some(CaptionFormat::WebVtt),
        SubtitleMode::Burn,
    )
    .unwrap();
    let decoded = OpSpec::try_from(v1::OpSpec::from(&OpSpec::Subtitle(burn.clone()))).unwrap();
    assert_eq!(decoded, OpSpec::Subtitle(burn));

    let mux = SubtitleSpec::new(
        source(),
        Some(source()),
        Some(CaptionFormat::Srt),
        SubtitleMode::Mux,
    )
    .unwrap();
    let decoded = OpSpec::try_from(v1::OpSpec::from(&OpSpec::Subtitle(mux.clone()))).unwrap();
    assert_eq!(decoded, OpSpec::Subtitle(mux));

    let strip = SubtitleSpec::new(source(), None, None, SubtitleMode::Strip).unwrap();
    let decoded = OpSpec::try_from(v1::OpSpec::from(&OpSpec::Subtitle(strip.clone()))).unwrap();
    assert_eq!(decoded, OpSpec::Subtitle(strip));
}

#[test]
fn a_subtitle_spec_off_the_contract_is_refused() {
    let spec = v1::OpSpec {
        op: Some(v1::op_spec::Op::Subtitle(v1::SubtitleSpec {
            input: Some(media()),
            captions: None,
            format: v1::CaptionFormat::Srt as i32,
            mode: v1::SubtitleMode::Burn as i32,
        })),
    };
    assert_eq!(
        OpSpec::try_from(spec),
        Err(OpSpecError::SubtitleCaptionsRequired)
    );
}

#[test]
fn a_subtitle_result_round_trips_its_output() {
    let output = MediaRef::object_uri("s3://bucket/out.mp4").unwrap();
    let result = SubtitleResult::new(output);
    let decoded =
        OpResult::try_from(v1::OpResult::from(&OpResult::Subtitle(result.clone()))).unwrap();
    assert_eq!(decoded, OpResult::Subtitle(result));
}

#[test]
fn every_caption_format_round_trips() {
    for (format, raw) in [
        (CaptionFormat::Srt, v1::CaptionFormat::Srt as i32),
        (CaptionFormat::WebVtt, v1::CaptionFormat::Webvtt as i32),
        (CaptionFormat::Scc, v1::CaptionFormat::Scc as i32),
        (CaptionFormat::Embedded, v1::CaptionFormat::Embedded as i32),
    ] {
        let spec =
            SubtitleSpec::new(source(), Some(source()), Some(format), SubtitleMode::Mux).unwrap();
        let wire = v1::OpSpec::from(&OpSpec::Subtitle(spec.clone()));
        let v1::op_spec::Op::Subtitle(on_the_wire) = wire.op.clone().unwrap() else {
            panic!("expected a subtitle spec");
        };
        assert_eq!(on_the_wire.format, raw);
        let decoded = OpSpec::try_from(wire).unwrap();
        assert_eq!(decoded, OpSpec::Subtitle(spec));
    }
}

#[test]
fn an_scc_document_reaches_the_wire_unchanged() {
    let spec = SubtitleSpec::new(
        source(),
        Some(source()),
        Some(CaptionFormat::Scc),
        SubtitleMode::Burn,
    )
    .unwrap();
    let decoded = OpSpec::try_from(v1::OpSpec::from(&OpSpec::Subtitle(spec.clone()))).unwrap();
    assert_eq!(decoded, OpSpec::Subtitle(spec));
}

#[test]
fn a_caption_format_off_the_contract_is_read_as_no_format() {
    let spec = v1::OpSpec {
        op: Some(v1::op_spec::Op::Subtitle(v1::SubtitleSpec {
            input: Some(media()),
            captions: Some(media()),
            format: 42,
            mode: v1::SubtitleMode::Burn as i32,
        })),
    };
    assert!(matches!(
        OpSpec::try_from(spec),
        Err(OpSpecError::SubtitleFormatRequired)
    ));
}

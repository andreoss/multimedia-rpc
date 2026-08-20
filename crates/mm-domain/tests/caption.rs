use mm_domain::{
    CaptionDocument, CaptionError, CaptionFormat, InputSource, MediaRef, OpKind, OpSpec,
    OpSpecError, SubtitleMode, SubtitleSpec, Ticks,
};

const SRT: &str =
    "1\n00:00:01,000 --> 00:00:02,500\nHello\n\n2\n00:00:03,000 --> 00:00:04,000\nWorld\n";

const VTT: &str =
    "WEBVTT\n\n00:00:01.000 --> 00:00:02.500\nHello\n\n00:00:03.000 --> 00:00:04.000\nWorld\n";

fn object() -> MediaRef {
    MediaRef::object_uri("s3://bucket/path/file.mp4").unwrap()
}

fn source() -> InputSource {
    InputSource::from(object())
}

#[test]
fn an_srt_document_parses_into_cues() {
    let document = CaptionDocument::parse(CaptionFormat::Srt, SRT).unwrap();
    assert_eq!(document.format(), CaptionFormat::Srt);
    assert_eq!(document.cues().len(), 2);
    assert_eq!(document.cues()[0].start(), Ticks::new(254_016_000_000));
    assert_eq!(document.cues()[0].end(), Ticks::new(635_040_000_000));
    assert_eq!(document.cues()[0].text(), "Hello");
    assert_eq!(document.cues()[1].text(), "World");
}

#[test]
fn a_webvtt_document_parses_into_cues() {
    let document = CaptionDocument::parse(CaptionFormat::WebVtt, VTT).unwrap();
    assert_eq!(document.format(), CaptionFormat::WebVtt);
    assert_eq!(document.cues().len(), 2);
    assert_eq!(document.cues()[1].start(), Ticks::new(762_048_000_000));
    assert_eq!(document.cues()[1].text(), "World");
}

#[test]
fn a_webvtt_cue_identifier_is_not_read_as_text() {
    let text = "WEBVTT\n\ncue-one\n00:00:01.000 --> 00:00:02.000\nHello\n";
    let document = CaptionDocument::parse(CaptionFormat::WebVtt, text).unwrap();
    assert_eq!(document.cues()[0].text(), "Hello");
}

#[test]
fn a_malformed_timestamp_is_refused_naming_the_line() {
    let text = "1\n00:00:01,000 --> bad\nHello\n";
    assert_eq!(
        CaptionDocument::parse(CaptionFormat::Srt, text),
        Err(CaptionError::MalformedTimestamp {
            line: 2,
            value: "bad".to_string()
        })
    );
}

#[test]
fn a_cue_that_ends_before_it_starts_is_refused_naming_the_line() {
    let text = "1\n00:00:02,000 --> 00:00:01,000\nHello\n";
    assert_eq!(
        CaptionDocument::parse(CaptionFormat::Srt, text),
        Err(CaptionError::EndBeforeStart { line: 2 })
    );
}

#[test]
fn a_cue_without_text_is_refused_naming_the_line() {
    let text = "1\n00:00:01,000 --> 00:00:02,000\n";
    assert_eq!(
        CaptionDocument::parse(CaptionFormat::Srt, text),
        Err(CaptionError::EmptyCue { line: 2 })
    );
}

#[test]
fn a_document_without_a_webvtt_header_is_refused() {
    assert_eq!(
        CaptionDocument::parse(CaptionFormat::WebVtt, "00:00:01.000 --> 00:00:02.000\nHi\n"),
        Err(CaptionError::MissingWebVttHeader)
    );
}

#[test]
fn an_empty_document_is_refused() {
    assert_eq!(
        CaptionDocument::parse(CaptionFormat::Srt, ""),
        Err(CaptionError::EmptyDocument)
    );
}

#[test]
fn a_subtitle_spec_requires_captions_and_a_format_for_burn_and_mux() {
    assert_eq!(
        SubtitleSpec::new(source(), None, Some(CaptionFormat::Srt), SubtitleMode::Burn),
        Err(OpSpecError::SubtitleCaptionsRequired)
    );
    assert_eq!(
        SubtitleSpec::new(source(), Some(source()), None, SubtitleMode::Mux),
        Err(OpSpecError::SubtitleFormatRequired)
    );
    let spec = SubtitleSpec::new(
        source(),
        Some(source()),
        Some(CaptionFormat::Srt),
        SubtitleMode::Burn,
    )
    .unwrap();
    assert_eq!(OpSpec::Subtitle(spec.clone()).kind(), OpKind::Subtitle);
    assert_eq!(spec.mode(), SubtitleMode::Burn);
    assert_eq!(spec.format(), Some(CaptionFormat::Srt));
    assert!(spec.captions().is_some());
}

#[test]
fn a_strip_spec_takes_no_captions() {
    assert_eq!(
        SubtitleSpec::new(source(), Some(source()), None, SubtitleMode::Strip),
        Err(OpSpecError::SubtitleCaptionsUnexpected)
    );
    let spec = SubtitleSpec::new(source(), None, None, SubtitleMode::Strip).unwrap();
    assert_eq!(spec.mode(), SubtitleMode::Strip);
}

#[test]
fn a_caption_format_parses_from_its_names() {
    assert_eq!(CaptionFormat::parse("srt"), Some(CaptionFormat::Srt));
    assert_eq!(CaptionFormat::parse("webvtt"), Some(CaptionFormat::WebVtt));
    assert_eq!(CaptionFormat::parse("ass"), None);
    assert_eq!(CaptionFormat::Srt.extension(), "srt");
    assert_eq!(CaptionFormat::WebVtt.extension(), "vtt");
}

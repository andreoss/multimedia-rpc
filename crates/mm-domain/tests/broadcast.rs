use mm_domain::{
    BroadcastCodec, CaptionDocument, CaptionError, CaptionFormat, InputSource, MediaRef,
    OpSpecError, SubtitleMode, SubtitleSpec, Ticks,
};

const SCC: &str = "\
Scenarist_SCC V1.0

00:00:00:00\t9420 9420 9425 9425 4865 6c6c 6f20 776f 726c 6421 942f 942f

00:00:01:00\t9420 9420 942c 942c

00:00:01:00\t9420 9420 9425 9425 5365 636f 6e64 942f 942f

00:00:03:15\t9420 9420 942c 942c

00:00:03:15\t9420 9420 9425 9425 4174 2073 6978 7479 942f 942f

00:01:00:00\t9420 9420 942c 942c
";

fn object() -> MediaRef {
    MediaRef::object_uri("s3://bucket/path/file.mov").unwrap()
}

fn source() -> InputSource {
    InputSource::from(object())
}

#[test]
fn an_scc_document_reads_its_cues_and_their_drop_frame_times() {
    let document = CaptionDocument::parse(CaptionFormat::Scc, SCC).unwrap();
    assert_eq!(document.format(), CaptionFormat::Scc);
    let cues = document.cues();
    assert_eq!(cues.len(), 3);
    assert_eq!(cues[0].text(), "Hello world!");
    assert_eq!(cues[0].start(), Ticks::ZERO);
    assert_eq!(cues[0].end().get(), 30 * 8_475_667_200);
    assert_eq!(cues[1].text(), "Second");
    assert_eq!(cues[1].start().get(), 30 * 8_475_667_200);
    assert_eq!(cues[1].end().get(), 105 * 8_475_667_200);
    assert_eq!(cues[2].text(), "At sixty");
    assert_eq!(cues[2].start().get(), 105 * 8_475_667_200);
    assert_eq!(cues[2].end().get(), 1798 * 8_475_667_200);
}

#[test]
fn an_scc_label_at_a_dropped_frame_is_read_as_drop_frame_timecode() {
    let document = CaptionDocument::parse(CaptionFormat::Scc, SCC).unwrap();
    let end = document.cues()[2].end();
    assert_eq!(end.get(), 1798 * 8_475_667_200);
    assert_eq!(end.micros(), 59_993_267);
    assert_ne!(
        end.get(),
        1800 * 8_475_667_200,
        "the label read as 00:01:00:00"
    );
}

#[test]
fn an_scc_document_without_its_header_is_refused_by_name() {
    let text = "00:00:00:00\t922e 922e\n";
    assert!(matches!(
        CaptionDocument::parse(CaptionFormat::Scc, text),
        Err(CaptionError::MissingSccHeader)
    ));
}

#[test]
fn an_scc_word_that_is_not_hex_is_refused_by_name() {
    let text = "Scenarist_SCC V1.0\n\n00:00:00:00\t922e zz2e 922e\n";
    assert!(matches!(
        CaptionDocument::parse(CaptionFormat::Scc, text),
        Err(CaptionError::MalformedSccWord { line: 3, .. })
    ));
}

#[test]
fn an_scc_word_of_odd_length_is_refused_by_name() {
    let text = "Scenarist_SCC V1.0\n\n00:00:00:00\t922e 92 922e\n";
    assert!(matches!(
        CaptionDocument::parse(CaptionFormat::Scc, text),
        Err(CaptionError::MalformedSccWord { line: 3, .. })
    ));
}

#[test]
fn an_scc_timecode_that_is_not_a_timecode_is_refused_by_name() {
    let text = "Scenarist_SCC V1.0\n\n00:00:00\t922e 922e\n";
    assert!(matches!(
        CaptionDocument::parse(CaptionFormat::Scc, text),
        Err(CaptionError::MalformedSccTimecode { line: 3, .. })
    ));
}

#[test]
fn an_scc_timecode_above_thirty_frames_is_refused_by_name() {
    let text = "Scenarist_SCC V1.0\n\n00:00:00:31\t922e 922e\n";
    assert!(matches!(
        CaptionDocument::parse(CaptionFormat::Scc, text),
        Err(CaptionError::MalformedSccTimecode { line: 3, .. })
    ));
}

#[test]
fn an_scc_document_whose_last_cue_never_ends_is_refused_by_name() {
    let text = "Scenarist_SCC V1.0\n\n00:00:00:00\t9420 4865 6c6c 6f21 942f 942f\n";
    assert!(matches!(
        CaptionDocument::parse(CaptionFormat::Scc, text),
        Err(CaptionError::UnterminatedCue { line: 3 })
    ));
}

#[test]
fn an_scc_document_with_no_printable_text_is_refused_by_name() {
    let text = "Scenarist_SCC V1.0\n\n00:00:00:00\t9420 9425 942c 942c\n";
    assert!(matches!(
        CaptionDocument::parse(CaptionFormat::Scc, text),
        Err(CaptionError::EmptyDocument)
    ));
}

#[test]
fn an_scc_control_code_carries_no_text() {
    let text = "Scenarist_SCC V1.0\n\n00:00:00:00\t9420 9420 9425 9425 4869 942f 942f\n\n00:00:01:00\t9420 942c 942c\n";
    let document = CaptionDocument::parse(CaptionFormat::Scc, text).unwrap();
    assert_eq!(document.cues().len(), 1);
    assert_eq!(document.cues()[0].text(), "Hi");
}

#[test]
fn an_scc_document_renders_the_cues_it_read_as_webvtt() {
    let document = CaptionDocument::parse(CaptionFormat::Scc, SCC).unwrap();
    assert_eq!(
        document.render_webvtt(),
        "WEBVTT\n\n\
         00:00:00.000 --> 00:00:01.001\nHello world!\n\n\
         00:00:01.001 --> 00:00:03.504\nSecond\n\n\
         00:00:03.504 --> 00:00:59.993\nAt sixty\n\n"
    );
}

#[test]
fn every_caption_format_names_its_extension_and_parses_back() {
    for format in [
        CaptionFormat::Srt,
        CaptionFormat::WebVtt,
        CaptionFormat::Scc,
        CaptionFormat::Embedded,
    ] {
        assert!(!format.name().is_empty());
        assert!(!format.extension().is_empty());
    }
    for format in [
        CaptionFormat::Srt,
        CaptionFormat::WebVtt,
        CaptionFormat::Scc,
    ] {
        assert_eq!(CaptionFormat::parse(format.extension()), Some(format));
    }
    assert_eq!(CaptionFormat::parse("scc"), Some(CaptionFormat::Scc));
    assert_eq!(
        CaptionFormat::parse("embedded"),
        Some(CaptionFormat::Embedded)
    );
    assert_eq!(CaptionFormat::Embedded.name(), "embedded");
    assert_eq!(CaptionFormat::Scc.name(), "scc");
    assert_eq!(CaptionFormat::Srt.to_string(), "Srt");
    assert_eq!(CaptionFormat::WebVtt.to_string(), "WebVtt");
    assert_eq!(CaptionFormat::Scc.to_string(), "Scc");
    assert_eq!(CaptionFormat::Embedded.to_string(), "Embedded");
}

#[test]
fn a_burn_of_an_embedded_track_needs_no_caption_file() {
    let spec = SubtitleSpec::new(
        source(),
        Some(source()),
        Some(CaptionFormat::Embedded),
        SubtitleMode::Burn,
    )
    .unwrap();
    assert_eq!(spec.format(), Some(CaptionFormat::Embedded));
}

#[test]
fn an_embedded_format_without_a_captions_input_is_refused_by_name() {
    assert!(matches!(
        SubtitleSpec::new(
            source(),
            None,
            Some(CaptionFormat::Embedded),
            SubtitleMode::Burn
        ),
        Err(OpSpecError::SubtitleCaptionsRequired)
    ));
}

#[test]
fn the_broadcast_codecs_this_service_reads_are_named() {
    for (codec, name) in [
        (BroadcastCodec::Eia608, "eia_608"),
        (BroadcastCodec::DvbTeletext, "dvb_teletext"),
        (BroadcastCodec::DvbSubtitle, "dvb_subtitle"),
        (BroadcastCodec::DvdSubtitle, "dvd_subtitle"),
        (BroadcastCodec::MovText, "mov_text"),
        (BroadcastCodec::SubRip, "subrip"),
        (BroadcastCodec::Ass, "ass"),
        (BroadcastCodec::WebVtt, "webvtt"),
    ] {
        assert_eq!(codec.name(), name);
        assert_eq!(BroadcastCodec::parse(name), Some(codec));
    }
}

#[test]
fn a_codec_with_no_text_behind_it_is_not_a_broadcast_caption() {
    assert_eq!(BroadcastCodec::parse("hdmv_pgs_subtitle"), None);
    assert_eq!(BroadcastCodec::parse("xsub"), None);
    assert_eq!(BroadcastCodec::parse(""), None);
}
#[test]
fn a_webvtt_timestamp_may_omit_its_hours() {
    let text = "WEBVTT\n\n00:00.500 --> 00:02.500\nNO HOURS\n";
    let document = CaptionDocument::parse(CaptionFormat::WebVtt, text).unwrap();
    let cue = &document.cues()[0];
    assert_eq!(cue.start().get(), 500 * 254_016_000);
    assert_eq!(cue.end().get(), 2_500 * 254_016_000);
}

#[test]
fn a_timestamp_with_four_parts_is_still_refused() {
    let text = "WEBVTT\n\n00:00:00:00.500 --> 00:02.500\nTOO MANY\n";
    assert!(matches!(
        CaptionDocument::parse(CaptionFormat::WebVtt, text),
        Err(CaptionError::MalformedTimestamp { line: 3, .. })
    ));
}

use mm_gst::{BusError, BusKind, first_error, parse_bus_line, parse_bus_output, render_playlist};

#[test]
fn an_error_line_carries_its_source_and_details() {
    let message = parse_bus_line("ERROR qtmux0 : Internal data stream error").unwrap();

    assert_eq!(message.kind, BusKind::Error);
    assert_eq!(message.source.as_deref(), Some("qtmux0"));
    assert_eq!(message.details, "Internal data stream error");
}

#[test]
fn an_eos_line_carries_no_source() {
    let message = parse_bus_line("EOS").unwrap();

    assert_eq!(message.kind, BusKind::Eos);
    assert!(message.source.is_none());
}

#[test]
fn a_warning_line_is_read_as_a_warning() {
    let message = parse_bus_line("WARN filesrc0 : resource not found").unwrap();

    assert_eq!(message.kind, BusKind::Warning);
    assert_eq!(message.details, "resource not found");
}

#[test]
fn an_empty_line_carries_no_severity() {
    assert_eq!(parse_bus_line("   "), Err(BusError::MissingSeverity));
}

#[test]
fn a_line_whose_first_word_is_no_severity_is_unknown() {
    assert_eq!(
        parse_bus_line("something happened"),
        Err(BusError::UnknownSeverity("SOMETHING".to_string()))
    );
}

#[test]
fn an_unknown_severity_is_named_in_the_error() {
    assert_eq!(
        parse_bus_line("NOTE nothing to see"),
        Err(BusError::UnknownSeverity("NOTE".to_string()))
    );
}

#[test]
fn the_first_error_of_a_captured_run_is_found() {
    let text = "STATE pipeline : changed\nERROR qtmux0 : stream error\nERROR filesrc0 : late\n";

    let messages = parse_bus_output(text);

    assert_eq!(messages.len(), 3);
    assert_eq!(
        first_error(&messages).map(|m| m.details.clone()),
        Some("stream error".to_string())
    );
}

#[test]
fn a_playlist_carries_the_target_duration_and_every_segment() {
    let playlist = render_playlist(
        Some("init.mp4"),
        2.0,
        &["seg00001.m4s".to_string(), "seg00002.m4s".to_string()],
    );

    assert!(playlist.starts_with("#EXTM3U\n"));
    assert!(playlist.contains("#EXT-X-MAP:URI=\"init.mp4\"\n"));
    assert!(playlist.contains("#EXT-X-TARGETDURATION:2\n"));
    assert!(playlist.contains("#EXTINF:2.000,\nseg00001.m4s\n"));
    assert!(playlist.contains("#EXTINF:2.000,\nseg00002.m4s\n"));
    assert!(playlist.trim_end().ends_with("#EXT-X-ENDLIST"));
}

#[test]
fn a_fractional_segment_rounds_the_target_duration_up() {
    let playlist = render_playlist(None, 1.5, &["seg00001.m4s".to_string()]);

    assert!(playlist.contains("#EXT-X-TARGETDURATION:2\n"));
    assert!(playlist.contains("#EXTINF:1.500,\n"));
    assert!(!playlist.contains("#EXT-X-MAP"));
}

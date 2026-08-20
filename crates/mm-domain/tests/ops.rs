use mm_domain::{
    ConcatSpec, MediaRef, MediaRefError, OpKind, OpResultError, OpSpec, OpSpecError, OutputProfile,
    OutputProfileError, ProbeResult, ProbeSpec, ThumbnailResult, ThumbnailSpec, TimedOutput,
    TranscodeSpec, TrimSpec, UnsupportedOp,
};

fn object() -> MediaRef {
    MediaRef::object_uri("s3://bucket/path/file.mp4").unwrap()
}

fn http() -> MediaRef {
    MediaRef::http_url("https://host.example/file.mp4").unwrap()
}

fn profile() -> OutputProfile {
    OutputProfile::from_raw(30.0, 4_000_000, 23, 128_000, 48_000, 1280, 720).unwrap()
}

#[test]
fn object_uri_parses_bucket_and_key() {
    match object() {
        MediaRef::Object(object_ref) => {
            assert_eq!(object_ref.bucket(), "bucket");
            assert_eq!(object_ref.key(), "path/file.mp4");
        }
        MediaRef::Http(_) => panic!("expected object ref"),
    }
}

#[test]
fn object_uri_round_trips_to_uri() {
    assert_eq!(object().to_uri(), "s3://bucket/path/file.mp4");
    assert_eq!(http().to_uri(), "https://host.example/file.mp4");
}

#[test]
fn object_uri_rejects_unknown_scheme() {
    for uri in ["gs://bucket/key", "file:///a", "bucket/key", ""] {
        assert!(matches!(
            MediaRef::object_uri(uri),
            Err(MediaRefError::UnknownScheme { .. })
        ));
    }
}

#[test]
fn object_uri_rejects_malformed_forms() {
    for uri in ["s3://bucket", "s3:///key", "s3://bucket/", "s3://"] {
        assert!(matches!(
            MediaRef::object_uri(uri),
            Err(MediaRefError::MalformedObjectUri { .. })
        ));
    }
}

#[test]
fn http_url_accepts_both_schemes() {
    assert!(MediaRef::http_url("http://host/file").is_ok());
    assert!(MediaRef::http_url("https://host/file").is_ok());
}

#[test]
fn http_url_rejects_unknown_scheme() {
    for url in ["ftp://host/file", "s3://bucket/key", "host/file", ""] {
        assert!(matches!(
            MediaRef::http_url(url),
            Err(MediaRefError::UnknownScheme { .. })
        ));
    }
}

#[test]
fn http_url_rejects_empty_host() {
    for url in ["https://", "http://", "https:///path"] {
        assert!(matches!(
            MediaRef::http_url(url),
            Err(MediaRefError::MalformedHttpUrl { .. })
        ));
    }
}

#[test]
fn output_profile_treats_zero_as_unset() {
    let profile = OutputProfile::from_raw(0.0, 0, 0, 0, 0, 0, 0).unwrap();
    assert!(profile.is_empty());
    assert_eq!(profile.frame_rate(), None);
    assert_eq!(profile.video_max_bitrate(), None);
}

#[test]
fn output_profile_keeps_positive_values() {
    let profile = profile();
    assert!(!profile.is_empty());
    assert_eq!(profile.frame_rate(), Some(30.0));
    assert_eq!(profile.video_max_bitrate(), Some(4_000_000));
    assert_eq!(profile.video_quality(), Some(23));
    assert_eq!(profile.audio_bitrate(), Some(128_000));
    assert_eq!(profile.audio_sample_rate(), Some(48_000));
}

#[test]
fn output_profile_keeps_a_full_resolution() {
    assert_eq!(profile().resolution(), Some((1280, 720)));
    let unset = OutputProfile::from_raw(30.0, 0, 0, 0, 0, 0, 0).unwrap();
    assert_eq!(unset.resolution(), None);
}

#[test]
fn output_profile_rejects_a_partial_resolution() {
    for (width, height) in [(1280, 0), (0, 720)] {
        assert_eq!(
            OutputProfile::from_raw(0.0, 0, 0, 0, 0, width, height).unwrap_err(),
            OutputProfileError::PartialResolution { width, height }
        );
    }
}

#[test]
fn a_resolution_alone_makes_the_profile_nonempty() {
    let profile = OutputProfile::from_raw(0.0, 0, 0, 0, 0, 640, 360).unwrap();
    assert!(!profile.is_empty());
    assert_eq!(profile.resolution(), Some((640, 360)));
}

#[test]
fn output_profile_rejects_bad_frame_rate() {
    for value in [-1.0, f64::NAN, f64::INFINITY, -0.5] {
        assert!(matches!(
            OutputProfile::from_raw(value, 0, 0, 0, 0, 0, 0),
            Err(OutputProfileError::InvalidFrameRate { .. })
        ));
    }
}

#[test]
fn concat_requires_at_least_two_inputs() {
    assert_eq!(
        ConcatSpec::new(vec![], None).unwrap_err(),
        OpSpecError::EmptyInputs
    );
    assert_eq!(
        ConcatSpec::new(vec![object()], None).unwrap_err(),
        OpSpecError::TooFewConcatInputs { count: 1 }
    );
    assert!(ConcatSpec::new(vec![object(), http()], None).is_ok());
}

#[test]
fn concat_rejects_empty_profile() {
    let empty = OutputProfile::from_raw(0.0, 0, 0, 0, 0, 0, 0).unwrap();
    assert_eq!(
        ConcatSpec::new(vec![object(), http()], Some(empty)).unwrap_err(),
        OpSpecError::EmptyProfile
    );
    assert!(ConcatSpec::new(vec![object(), http()], Some(profile())).is_ok());
}

#[test]
fn probe_takes_any_valid_ref() {
    let spec = ProbeSpec::new(http());
    assert_eq!(spec.input(), &http());
}

#[test]
fn transcode_requires_a_nonempty_profile() {
    let empty = OutputProfile::from_raw(0.0, 0, 0, 0, 0, 0, 0).unwrap();
    assert_eq!(
        TranscodeSpec::new(object(), empty).unwrap_err(),
        OpSpecError::EmptyProfile
    );
    assert!(TranscodeSpec::new(object(), profile()).is_ok());
}

#[test]
fn trim_rejects_invalid_times() {
    assert!(matches!(
        TrimSpec::new(object(), -1.0, 5.0),
        Err(OpSpecError::InvalidStart { .. })
    ));
    assert!(matches!(
        TrimSpec::new(object(), f64::NAN, 5.0),
        Err(OpSpecError::InvalidStart { .. })
    ));
    assert!(matches!(
        TrimSpec::new(object(), 0.0, 0.0),
        Err(OpSpecError::NonPositiveDuration { .. })
    ));
    assert!(matches!(
        TrimSpec::new(object(), 0.0, -2.0),
        Err(OpSpecError::NonPositiveDuration { .. })
    ));
    assert!(matches!(
        TrimSpec::new(object(), 0.0, f64::INFINITY),
        Err(OpSpecError::NonPositiveDuration { .. })
    ));
}

#[test]
fn trim_accepts_valid_times() {
    let spec = TrimSpec::new(object(), 0.0, 2.5).unwrap();
    assert_eq!(spec.start_seconds(), 0.0);
    assert_eq!(spec.duration_seconds(), 2.5);
}

#[test]
fn thumbnail_rejects_invalid_specs() {
    assert_eq!(
        ThumbnailSpec::new(object(), vec![], 320, 180).unwrap_err(),
        OpSpecError::EmptyOffsets
    );
    assert!(matches!(
        ThumbnailSpec::new(object(), vec![0.0, -1.0], 320, 180),
        Err(OpSpecError::InvalidOffset { .. })
    ));
    assert!(matches!(
        ThumbnailSpec::new(object(), vec![f64::NAN], 320, 180),
        Err(OpSpecError::InvalidOffset { .. })
    ));
    assert_eq!(
        ThumbnailSpec::new(object(), vec![1.0], 0, 180).unwrap_err(),
        OpSpecError::ZeroDimension { dimension: "width" }
    );
    assert_eq!(
        ThumbnailSpec::new(object(), vec![1.0], 320, 0).unwrap_err(),
        OpSpecError::ZeroDimension {
            dimension: "height"
        }
    );
}

#[test]
fn thumbnail_accepts_valid_specs() {
    let spec = ThumbnailSpec::new(object(), vec![0.0, 1.5], 320, 180).unwrap();
    assert_eq!(spec.offsets_seconds(), &[0.0, 1.5]);
    assert_eq!(spec.width(), 320);
    assert_eq!(spec.height(), 180);
}

fn specs() -> Vec<(OpSpec, OpKind)> {
    vec![
        (
            OpSpec::Concat(ConcatSpec::new(vec![object(), http()], None).unwrap()),
            OpKind::Concat,
        ),
        (OpSpec::Probe(ProbeSpec::new(object())), OpKind::Probe),
        (
            OpSpec::Transcode(TranscodeSpec::new(object(), profile()).unwrap()),
            OpKind::Transcode,
        ),
        (
            OpSpec::Trim(TrimSpec::new(object(), 0.0, 2.0).unwrap()),
            OpKind::Trim,
        ),
        (
            OpSpec::Thumbnail(ThumbnailSpec::new(object(), vec![1.0], 320, 180).unwrap()),
            OpKind::Thumbnail,
        ),
    ]
}

#[test]
fn every_spec_names_its_kind() {
    for (spec, kind) in specs() {
        assert_eq!(spec.kind(), kind);
    }
}

#[test]
fn executable_kinds_pass_the_support_gate() {
    for kind in [OpKind::Concat, OpKind::Probe, OpKind::Transcode] {
        assert!(kind.ensure_executable().is_ok());
    }
}

#[test]
fn trim_and_thumbnail_answer_a_typed_unsupported_error() {
    assert_eq!(
        OpKind::Trim.ensure_executable().unwrap_err(),
        UnsupportedOp { kind: OpKind::Trim }
    );
    let error = OpKind::Thumbnail.ensure_executable().unwrap_err();
    assert_eq!(
        error,
        UnsupportedOp {
            kind: OpKind::Thumbnail
        }
    );
    assert_eq!(error.to_string(), "thumbnail is not supported yet");
    assert_eq!(
        OpKind::Trim.ensure_executable().unwrap_err().to_string(),
        "trim is not supported yet"
    );
}

#[test]
fn timed_output_rejects_invalid_duration() {
    for value in [-1.0, f64::NAN, f64::INFINITY] {
        assert!(matches!(
            TimedOutput::new(object(), value),
            Err(OpResultError::InvalidDuration { .. })
        ));
    }
    let result = TimedOutput::new(object(), 12.5).unwrap();
    assert_eq!(result.duration_seconds(), 12.5);
    assert_eq!(result.output(), &object());
}

#[test]
fn probe_result_rejects_invalid_duration() {
    assert!(matches!(
        ProbeResult::new(-0.1, 1, 1, "mp4".to_string()),
        Err(OpResultError::InvalidDuration { .. })
    ));
    let result = ProbeResult::new(10.0, 1, 2, "mp4".to_string()).unwrap();
    assert_eq!(result.duration_seconds(), 10.0);
    assert_eq!(result.video_streams(), 1);
    assert_eq!(result.audio_streams(), 2);
    assert_eq!(result.container(), "mp4");
}

#[test]
fn thumbnail_result_rejects_empty_outputs() {
    assert_eq!(
        ThumbnailResult::new(vec![]).unwrap_err(),
        OpResultError::EmptyOutputs
    );
    let result = ThumbnailResult::new(vec![object()]).unwrap();
    assert_eq!(result.outputs().len(), 1);
}

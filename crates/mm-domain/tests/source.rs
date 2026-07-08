use mm_domain::OutputProfile;
use mm_domain::{
    ConcatSpec, ImageOp, ImageSpec, InputSource, MediaRef, MediaSource, NodeRef, OpSpec,
    PackageSpec, ProbeSpec, ResizeMode, SourceError, ThumbnailSpec, TranscodeSpec, TrimRange,
    TrimSpec,
};

fn object() -> MediaRef {
    MediaRef::object_uri("s3://bucket/path/file.mp4").unwrap()
}

fn profile() -> OutputProfile {
    OutputProfile::from_raw(24.0, 0, 0, 0, 48_000, 640, 360).unwrap()
}

fn source() -> MediaSource {
    MediaSource::of(object())
}

#[test]
fn a_trim_range_keeps_the_start_and_the_duration() {
    let trim = TrimRange::new(1.5, 4.0).unwrap();
    assert_eq!(trim.start_seconds(), 1.5);
    assert_eq!(trim.duration_seconds(), 4.0);
}

#[test]
fn a_trim_range_refuses_a_negative_start() {
    for start in [-0.001, -1.0, f64::NAN, f64::INFINITY] {
        assert!(
            matches!(
                TrimRange::new(start, 1.0),
                Err(SourceError::InvalidTrimStart { .. })
            ),
            "start {start} was accepted"
        );
    }
}

#[test]
fn a_trim_range_refuses_a_non_positive_duration() {
    for duration in [0.0, -1.0, f64::NAN, f64::NEG_INFINITY] {
        assert!(
            matches!(
                TrimRange::new(0.0, duration),
                Err(SourceError::InvalidTrimDuration { .. })
            ),
            "duration {duration} was accepted"
        );
    }
}

#[test]
fn every_refusal_names_the_field_it_refused() {
    let refusals = [
        (
            TrimRange::new(-1.0, 1.0).unwrap_err().to_string(),
            "trim start_seconds",
        ),
        (
            TrimRange::new(0.0, 0.0).unwrap_err().to_string(),
            "trim duration_seconds",
        ),
        (
            MediaSource::new(object(), None, Some(0.0), None)
                .unwrap_err()
                .to_string(),
            "speed",
        ),
        (
            MediaSource::new(object(), None, None, Some(-0.5))
                .unwrap_err()
                .to_string(),
            "volume",
        ),
    ];
    for (message, field) in refusals {
        assert!(message.contains(field), "{message:?} does not name {field}");
    }
}

#[test]
fn a_source_carries_its_cuts() {
    let trim = TrimRange::new(1.0, 2.0).unwrap();
    let source = MediaSource::new(object(), Some(trim.clone()), Some(0.5), Some(0.25)).unwrap();
    assert_eq!(source.media(), &object());
    assert_eq!(source.trim(), Some(&trim));
    assert_eq!(source.speed(), Some(0.5));
    assert_eq!(source.volume(), Some(0.25));
}

#[test]
fn an_unset_cut_is_not_a_cut_of_zero() {
    let source = source();
    assert_eq!(source.trim(), None);
    assert_eq!(source.speed(), None);
    assert_eq!(source.volume(), None);
}

#[test]
fn a_source_refuses_a_speed_that_is_not_positive() {
    for speed in [0.0, -1.0, f64::INFINITY] {
        assert!(
            matches!(
                MediaSource::new(object(), None, Some(speed), None),
                Err(SourceError::InvalidSpeed { .. })
            ),
            "speed {speed} was accepted"
        );
    }
}

#[test]
fn a_source_refuses_a_volume_that_is_negative() {
    for volume in [-0.1, f64::NAN, f64::NEG_INFINITY] {
        assert!(
            matches!(
                MediaSource::new(object(), None, None, Some(volume)),
                Err(SourceError::InvalidVolume { .. })
            ),
            "volume {volume} was accepted"
        );
    }
}

#[test]
fn a_bare_reference_is_a_source_that_cuts_nothing() {
    let input = InputSource::from(object());
    assert_eq!(input.media(), Some(&source()));
}

#[test]
fn a_transcode_accepts_a_source_that_cuts() {
    let input = InputSource::from(
        MediaSource::new(
            object(),
            Some(TrimRange::new(1.0, 2.0).unwrap()),
            Some(0.5),
            Some(0.5),
        )
        .unwrap(),
    );
    let spec = TranscodeSpec::new(input, profile()).unwrap();
    assert_eq!(
        spec.input()
            .media()
            .unwrap()
            .trim()
            .unwrap()
            .start_seconds(),
        1.0
    );
    assert_eq!(spec.input().media().unwrap().speed(), Some(0.5));
    assert_eq!(spec.input().media().unwrap().volume(), Some(0.5));
}

#[test]
fn concat_refuses_a_cut_it_cannot_apply() {
    let cut = InputSource::from(
        MediaSource::new(
            object(),
            Some(TrimRange::new(1.0, 2.0).unwrap()),
            None,
            None,
        )
        .unwrap(),
    );
    let error = ConcatSpec::new(vec![InputSource::from(object()), cut], None).unwrap_err();
    assert!(matches!(
        error,
        mm_domain::OpSpecError::InputCut {
            op: "concat",
            field: "trim"
        }
    ));
}

#[test]
fn every_op_that_cannot_cut_names_the_op_and_the_field() {
    let trim = InputSource::from(
        MediaSource::new(
            object(),
            Some(TrimRange::new(0.0, 1.0).unwrap()),
            None,
            None,
        )
        .unwrap(),
    );
    let speed = InputSource::from(MediaSource::new(object(), None, Some(2.0), None).unwrap());
    let volume = InputSource::from(MediaSource::new(object(), None, None, Some(0.5)).unwrap());

    let refusals = [
        ProbeSpec::new(trim.clone()).err(),
        TrimSpec::new(speed, 0.0, 1.0).err(),
        ThumbnailSpec::new(volume.clone(), vec![0.0], 64, 64).err(),
        PackageSpec::new(trim.clone(), 4.0).err(),
        ImageSpec::new(
            vec![volume.clone()],
            ImageOp::Resize,
            64,
            64,
            ResizeMode::Fit,
            None,
        )
        .err(),
    ];
    let named: Vec<String> = refusals
        .into_iter()
        .map(|refusal| refusal.expect("a cut is refused").to_string())
        .collect();
    assert_eq!(named.len(), 5, "a cut was accepted");
    for message in &named {
        for field in ["trim", "speed", "volume"] {
            if message.contains(field) {
                assert!(message.contains("cannot apply"), "{message}");
            }
        }
    }
}

#[test]
fn a_node_reference_names_a_step_output() {
    let node = NodeRef::new("trim-step", 1).expect("a valid node id");
    assert_eq!(node.id().as_str(), "trim-step");
    assert_eq!(node.output(), 1);
    let input = InputSource::from(node.clone());
    assert_eq!(input.media(), None);
    assert_eq!(input.node(), Some(&node));
    assert_eq!(input.cuts(), None);
}

#[test]
fn a_node_reference_refuses_a_malformed_id_by_name() {
    for id in ["", "has space", "bad\tid"] {
        let error = NodeRef::new(id, 0).expect_err("a malformed node id is refused");
        assert!(error.to_string().contains("node id"), "{error}");
    }
}

#[test]
fn a_spec_naming_a_node_is_refused_as_unresolvable() {
    let node = NodeRef::new("join", 0).expect("a valid node id");
    let spec = OpSpec::Transcode(
        TranscodeSpec::new(InputSource::from(node), profile()).expect("a node is not a cut"),
    );
    let error = spec
        .ensure_resolvable()
        .expect_err("a node has no producer yet");
    assert_eq!(error.node, "join");
    assert!(error.to_string().contains("join"));
}

#[test]
fn a_spec_reading_only_sources_is_resolvable() {
    let spec = OpSpec::Transcode(
        TranscodeSpec::new(InputSource::from(object()), profile()).expect("valid spec"),
    );
    assert!(spec.ensure_resolvable().is_ok());
}

#[test]
fn a_spec_lists_the_inputs_it_reads() {
    let spec =
        OpSpec::Transcode(TranscodeSpec::new(InputSource::from(object()), profile()).unwrap());
    assert_eq!(spec.inputs().len(), 1);

    let concat = OpSpec::Concat(
        ConcatSpec::new(
            vec![InputSource::from(object()), InputSource::from(object())],
            None,
        )
        .unwrap(),
    );
    assert_eq!(concat.inputs().len(), 2);
}

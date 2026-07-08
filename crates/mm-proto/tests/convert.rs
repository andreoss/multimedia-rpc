use mm_domain::{
    BatchResult, BatchSpec, ConcatSpec, ImageFormat, ImageOp, ImageResult, ImageSpec, InputSource,
    JobFailure, MediaRef, MediaRefError, MediaSource, OpResult, OpResultError, OpSpec, OpSpecError,
    OutputProfile, PackageResult, PackageSpec, ProbeResult, ProbeSpec, ResizeMode, SourceError,
    StepOutcome, StepState, ThumbnailResult, ThumbnailSpec, TimedOutput, TranscodeResult,
    TranscodeSpec, TrimRange, TrimSpec,
};
use mm_proto::v1;
use proptest::prelude::*;

fn media_source(media: &MediaRef) -> v1::InputSource {
    v1::InputSource::from(&InputSource::from(media.clone()))
}

fn transcode_spec(input: v1::InputSource) -> v1::op_spec::Op {
    v1::op_spec::Op::Transcode(v1::TranscodeSpec {
        input: Some(input),
        profile: Some(v1::OutputProfile {
            frame_rate: 24.0,
            ..Default::default()
        }),
    })
}

fn bucket_strategy() -> impl Strategy<Value = String> {
    "[a-z0-9][a-z0-9.-]{2,20}"
}

fn key_strategy() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_.-][a-zA-Z0-9/_.-]{0,40}"
}

fn media_ref_strategy() -> impl Strategy<Value = MediaRef> {
    prop_oneof![
        (bucket_strategy(), key_strategy()).prop_map(|(bucket, key)| {
            MediaRef::object_uri(&format!("s3://{bucket}/{key}")).unwrap()
        }),
        ("[a-z0-9.-]{1,20}", "[a-zA-Z0-9/_.-]{0,30}").prop_map(|(host, path)| {
            MediaRef::http_url(&format!("https://{host}/{path}")).unwrap()
        }),
    ]
}

fn resolution_strategy() -> impl Strategy<Value = (u32, u32)> {
    prop_oneof![Just((0u32, 0u32)), (16u32..7680, 16u32..4320)]
}

fn profile_strategy() -> impl Strategy<Value = OutputProfile> {
    (
        0.1f64..240.0,
        1u32..10_000_000,
        1u32..52,
        1u32..512_000,
        8_000u32..192_000,
        resolution_strategy(),
    )
        .prop_map(
            |(
                frame_rate,
                video_max_bitrate,
                video_quality,
                audio_bitrate,
                sample_rate,
                (width, height),
            )| {
                OutputProfile::from_raw(
                    frame_rate,
                    video_max_bitrate,
                    video_quality,
                    audio_bitrate,
                    sample_rate,
                    width,
                    height,
                )
                .unwrap()
            },
        )
}

fn op_spec_strategy() -> impl Strategy<Value = OpSpec> {
    prop_oneof![
        (
            prop::collection::vec(media_ref_strategy(), 2..5),
            prop::option::of(profile_strategy()),
        )
            .prop_map(|(inputs, profile)| {
                OpSpec::Concat(
                    ConcatSpec::new(inputs.into_iter().map(InputSource::from).collect(), profile)
                        .unwrap(),
                )
            }),
        media_ref_strategy()
            .prop_map(|input| OpSpec::Probe(ProbeSpec::new(InputSource::from(input)).unwrap())),
        (media_ref_strategy(), profile_strategy()).prop_map(|(input, profile)| {
            OpSpec::Transcode(TranscodeSpec::new(InputSource::from(input), profile).unwrap())
        }),
        (media_ref_strategy(), 0.0f64..3600.0, 0.001f64..3600.0).prop_map(
            |(input, start, duration)| {
                OpSpec::Trim(TrimSpec::new(InputSource::from(input), start, duration).unwrap())
            }
        ),
        (
            media_ref_strategy(),
            prop::collection::vec(0.0f64..3600.0, 1..5),
            1u32..4096,
            1u32..4096,
        )
            .prop_map(|(input, offsets, width, height)| {
                OpSpec::Thumbnail(
                    ThumbnailSpec::new(InputSource::from(input), offsets, width, height).unwrap(),
                )
            }),
    ]
}

fn op_result_strategy() -> impl Strategy<Value = OpResult> {
    prop_oneof![
        (media_ref_strategy(), 0.0f64..7200.0).prop_map(|(output, duration)| {
            OpResult::Concat(TimedOutput::new(output, duration).unwrap())
        }),
        (0.0f64..7200.0, 0u32..5, 0u32..5, "[a-z0-9]{2,8}").prop_map(
            |(duration, video, audio, container)| {
                OpResult::Probe(ProbeResult::new(duration, video, audio, container).unwrap())
            }
        ),
        media_ref_strategy().prop_map(|output| OpResult::Transcode(TranscodeResult::new(output))),
        (media_ref_strategy(), 0.0f64..7200.0).prop_map(|(output, duration)| {
            OpResult::Trim(TimedOutput::new(output, duration).unwrap())
        }),
        prop::collection::vec(media_ref_strategy(), 1..4)
            .prop_map(|outputs| OpResult::Thumbnail(ThumbnailResult::new(outputs).unwrap())),
    ]
}

fn probe_spec_with_object_uri(uri: String) -> v1::OpSpec {
    v1::OpSpec {
        op: Some(v1::op_spec::Op::Probe(v1::ProbeSpec {
            input: Some(v1::InputSource {
                source: Some(v1::input_source::Source::Media(v1::MediaSource {
                    media: Some(v1::MediaRef {
                        r#ref: Some(v1::media_ref::Ref::ObjectUri(uri)),
                    }),
                    trim: None,
                    speed: None,
                    volume: None,
                })),
            }),
        })),
    }
}

proptest! {
    #[test]
    fn valid_specs_round_trip(spec in op_spec_strategy()) {
        let proto = v1::OpSpec::from(&spec);
        let back = OpSpec::try_from(proto).unwrap();
        prop_assert_eq!(back, spec);
    }

    #[test]
    fn valid_results_round_trip(result in op_result_strategy()) {
        let proto = v1::OpResult::from(&result);
        let back = OpResult::try_from(proto).unwrap();
        prop_assert_eq!(back, result);
    }

    #[test]
    fn single_input_concat_is_rejected(input in media_ref_strategy()) {
        let proto = v1::OpSpec {
            op: Some(v1::op_spec::Op::Concat(v1::ConcatSpec {
                inputs: vec![media_source(&input)],
                profile: None,
            })),
        };
        prop_assert_eq!(
            OpSpec::try_from(proto).unwrap_err(),
            OpSpecError::TooFewConcatInputs { count: 1 }
        );
    }

    #[test]
    fn unknown_object_scheme_is_rejected(scheme in "[a-z]{2,5}", rest in "[a-z0-9/]{3,20}") {
        prop_assume!(scheme != "s3");
        let uri = format!("{scheme}://{rest}");
        let result = OpSpec::try_from(probe_spec_with_object_uri(uri));
        let rejected = matches!(
            result,
            Err(OpSpecError::Source(SourceError::MediaRef(
                MediaRefError::UnknownScheme { .. }
            )))
        );
        prop_assert!(rejected);
    }

    #[test]
    fn malformed_object_uri_is_rejected(bucket in "[a-z0-9]{1,10}") {
        for uri in [
            format!("s3://{bucket}"),
            format!("s3://{bucket}/"),
            "s3:///orphan-key".to_string(),
        ] {
            let result = OpSpec::try_from(probe_spec_with_object_uri(uri));
            let rejected = matches!(
                result,
                Err(OpSpecError::Source(SourceError::MediaRef(
                    MediaRefError::MalformedObjectUri { .. }
                )))
            );
            prop_assert!(rejected);
        }
    }

    #[test]
    fn negative_trim_start_is_rejected(
        input in media_ref_strategy(),
        start in -3600.0f64..-0.001,
    ) {
        let proto = v1::OpSpec {
            op: Some(v1::op_spec::Op::Trim(v1::TrimSpec {
                input: Some(media_source(&input)),
                start_seconds: start,
                duration_seconds: 1.0,
            })),
        };
        let rejected = matches!(
            OpSpec::try_from(proto),
            Err(OpSpecError::InvalidStart { .. })
        );
        prop_assert!(rejected);
    }

    #[test]
    fn nonpositive_trim_duration_is_rejected(
        input in media_ref_strategy(),
        duration in -3600.0f64..=0.0,
    ) {
        let proto = v1::OpSpec {
            op: Some(v1::op_spec::Op::Trim(v1::TrimSpec {
                input: Some(media_source(&input)),
                start_seconds: 0.0,
                duration_seconds: duration,
            })),
        };
        let rejected = matches!(
            OpSpec::try_from(proto),
            Err(OpSpecError::NonPositiveDuration { .. })
        );
        prop_assert!(rejected);
    }

    #[test]
    fn zero_thumbnail_dimension_is_rejected(
        input in media_ref_strategy(),
        side in 1u32..4096,
        width_is_zero in any::<bool>(),
    ) {
        let (width, height) = if width_is_zero { (0, side) } else { (side, 0) };
        let proto = v1::OpSpec {
            op: Some(v1::op_spec::Op::Thumbnail(v1::ThumbnailSpec {
                input: Some(media_source(&input)),
                offsets_seconds: vec![1.0],
                width,
                height,
            })),
        };
        let rejected = matches!(
            OpSpec::try_from(proto),
            Err(OpSpecError::ZeroDimension { .. })
        );
        prop_assert!(rejected);
    }

    #[test]
    fn negative_result_duration_is_rejected(
        output in media_ref_strategy(),
        duration in -7200.0f64..-0.001,
    ) {
        let proto = v1::OpResult {
            op: Some(v1::op_result::Op::Trim(v1::TrimResult {
                output: Some(v1::MediaRef::from(&output)),
                duration_seconds: duration,
            })),
        };
        let rejected = matches!(
            OpResult::try_from(proto),
            Err(OpResultError::InvalidDuration { .. })
        );
        prop_assert!(rejected);
    }
}

#[test]
fn empty_op_spec_is_rejected() {
    assert_eq!(
        OpSpec::try_from(v1::OpSpec { op: None }).unwrap_err(),
        OpSpecError::MissingOp
    );
}

#[test]
fn empty_op_result_is_rejected() {
    assert_eq!(
        OpResult::try_from(v1::OpResult { op: None }).unwrap_err(),
        OpResultError::MissingOp
    );
}

#[test]
fn missing_input_is_rejected() {
    let proto = v1::OpSpec {
        op: Some(v1::op_spec::Op::Probe(v1::ProbeSpec { input: None })),
    };
    assert_eq!(
        OpSpec::try_from(proto).unwrap_err(),
        OpSpecError::Source(SourceError::MissingSource)
    );
}

#[test]
fn empty_concat_inputs_are_rejected() {
    let proto = v1::OpSpec {
        op: Some(v1::op_spec::Op::Concat(v1::ConcatSpec {
            inputs: vec![],
            profile: None,
        })),
    };
    assert_eq!(
        OpSpec::try_from(proto).unwrap_err(),
        OpSpecError::EmptyInputs
    );
}

#[test]
fn empty_transcode_profile_is_rejected() {
    let proto = v1::OpSpec {
        op: Some(v1::op_spec::Op::Transcode(v1::TranscodeSpec {
            input: Some(media_source(&MediaRef::object_uri("s3://b/k").unwrap())),
            profile: None,
        })),
    };
    assert_eq!(
        OpSpec::try_from(proto).unwrap_err(),
        OpSpecError::EmptyProfile
    );
}

#[test]
fn empty_thumbnail_result_outputs_are_rejected() {
    let proto = v1::OpResult {
        op: Some(v1::op_result::Op::Thumbnail(v1::ThumbnailResult {
            outputs: vec![],
        })),
    };
    assert_eq!(
        OpResult::try_from(proto).unwrap_err(),
        OpResultError::EmptyOutputs
    );
}

fn image_spec(op: ImageOp, format: Option<ImageFormat>, inputs: usize) -> OpSpec {
    let refs: Vec<MediaRef> = (0..inputs)
        .map(|index| MediaRef::object_uri(&format!("s3://media/in/{index}.png")).unwrap())
        .collect();
    OpSpec::Image(ImageSpec::new(refs, op, 320, 180, ResizeMode::Exact, format).unwrap())
}

#[test]
fn an_image_spec_round_trips_through_the_contract() {
    for spec in [
        image_spec(ImageOp::Resize, None, 1),
        image_spec(ImageOp::Convert, Some(ImageFormat::Jpeg), 1),
        image_spec(ImageOp::Composite, None, 2),
    ] {
        let wire = v1::OpSpec::from(&spec);
        assert_eq!(OpSpec::try_from(wire).unwrap(), spec);
    }
}

#[test]
fn an_image_result_round_trips_through_the_contract() {
    let outputs = vec![
        MediaRef::object_uri("s3://media/out/a.png").unwrap(),
        MediaRef::object_uri("s3://media/out/b.jpg").unwrap(),
    ];
    let result = OpResult::Image(ImageResult::new(outputs.clone()).unwrap());

    let wire = v1::OpResult::from(&result);

    assert_eq!(OpResult::try_from(wire).unwrap(), result);
}

#[test]
fn an_image_spec_without_its_operation_is_rejected() {
    let wire = v1::OpSpec {
        op: Some(v1::op_spec::Op::Image(v1::ImageSpec {
            inputs: vec![],
            op: v1::ImageOp::Unspecified as i32,
            width: 0,
            height: 0,
            resize_mode: v1::ResizeMode::Unspecified as i32,
            format: v1::ImageFormat::Unspecified as i32,
        })),
    };

    assert!(matches!(
        OpSpec::try_from(wire),
        Err(OpSpecError::MissingOp)
    ));
}

#[test]
fn a_declared_image_format_round_trips_for_every_image_op() {
    let refs: Vec<MediaRef> = (0..2)
        .map(|index| MediaRef::object_uri(&format!("s3://media/in/{index}.png")).unwrap())
        .collect();
    let composite = OpSpec::Image(
        ImageSpec::new(
            refs,
            ImageOp::Composite,
            0,
            0,
            ResizeMode::Fit,
            Some(ImageFormat::Jpeg),
        )
        .unwrap(),
    );

    let wire = v1::OpSpec::from(&composite);
    let decoded = OpSpec::try_from(wire).unwrap();
    let OpSpec::Image(image) = decoded else {
        panic!("expected an image spec");
    };

    assert_eq!(image.declared_format(), Some(ImageFormat::Jpeg));
    assert_eq!(image.format(), ImageFormat::Jpeg);
}

#[test]
fn a_package_spec_and_result_round_trip_through_the_contract() {
    let spec = OpSpec::Package(
        PackageSpec::new(MediaRef::object_uri("s3://media/in/a.mp4").unwrap(), 2.0).unwrap(),
    );
    let wire = v1::OpSpec::from(&spec);
    assert_eq!(OpSpec::try_from(wire).unwrap(), spec);

    let result = OpResult::Package(
        PackageResult::new(
            MediaRef::object_uri("s3://media/out/playlist.m3u8").unwrap(),
            vec![
                MediaRef::object_uri("s3://media/out/seg00000.m4s").unwrap(),
                MediaRef::object_uri("s3://media/out/seg00001.m4s").unwrap(),
            ],
        )
        .unwrap(),
    );
    let wire = v1::OpResult::from(&result);
    assert_eq!(OpResult::try_from(wire).unwrap(), result);
}

#[test]
fn a_package_spec_with_a_non_positive_segment_length_is_rejected() {
    let wire = v1::OpSpec {
        op: Some(v1::op_spec::Op::Package(v1::PackageSpec {
            input: Some(media_source(
                &MediaRef::object_uri("s3://media/in/a.mp4").unwrap(),
            )),
            segment_seconds: 0.0,
        })),
    };

    assert!(matches!(
        OpSpec::try_from(wire),
        Err(OpSpecError::InvalidSegmentLength { .. })
    ));
}

fn wire_source(
    media: &MediaRef,
    trim: Option<v1::TrimRange>,
    speed: Option<f64>,
    volume: Option<f64>,
) -> v1::InputSource {
    v1::InputSource {
        source: Some(v1::input_source::Source::Media(v1::MediaSource {
            media: Some(v1::MediaRef::from(media)),
            trim,
            speed,
            volume,
        })),
    }
}

#[test]
fn a_source_with_cuts_round_trips_in_both_directions() {
    let media = MediaRef::object_uri("s3://media/in/a.mp4").unwrap();
    let spec = OpSpec::Transcode(
        TranscodeSpec::new(
            InputSource::from(
                MediaSource::new(
                    media.clone(),
                    Some(TrimRange::new(1.5, 4.25).unwrap()),
                    Some(0.5),
                    Some(0.75),
                )
                .unwrap(),
            ),
            OutputProfile::from_raw(24.0, 0, 0, 0, 48_000, 640, 360).unwrap(),
        )
        .unwrap(),
    );

    let wire = v1::OpSpec::from(&spec);
    assert_eq!(OpSpec::try_from(wire.clone()).unwrap(), spec);
    assert_eq!(
        v1::OpSpec::from(&OpSpec::try_from(wire).unwrap()),
        v1::OpSpec::from(&spec)
    );
}

#[test]
fn an_unset_scalar_arrives_absent_rather_than_zero() {
    let media = MediaRef::object_uri("s3://media/in/a.mp4").unwrap();
    let wire = v1::OpSpec {
        op: Some(transcode_spec(wire_source(&media, None, None, None))),
    };
    let OpSpec::Transcode(spec) = OpSpec::try_from(wire).unwrap() else {
        panic!("expected a transcode spec");
    };
    assert_eq!(spec.input().media().unwrap().speed(), None);
    assert_eq!(spec.input().media().unwrap().volume(), None);
    assert_eq!(spec.input().media().unwrap().trim(), None);
}

#[test]
fn a_speed_of_zero_off_the_wire_is_refused_naming_the_field() {
    let media = MediaRef::object_uri("s3://media/in/a.mp4").unwrap();
    let wire = v1::OpSpec {
        op: Some(transcode_spec(wire_source(&media, None, Some(0.0), None))),
    };
    let error = OpSpec::try_from(wire).unwrap_err();
    assert!(matches!(
        error,
        OpSpecError::Source(SourceError::InvalidSpeed { .. })
    ));
    assert!(error.to_string().contains("speed"), "{error}");
}

#[test]
fn a_negative_trim_start_off_the_wire_is_refused_naming_the_field() {
    let media = MediaRef::object_uri("s3://media/in/a.mp4").unwrap();
    let trim = v1::TrimRange {
        start_seconds: -1.0,
        duration_seconds: 2.0,
    };
    let wire = v1::OpSpec {
        op: Some(transcode_spec(wire_source(&media, Some(trim), None, None))),
    };
    let error = OpSpec::try_from(wire).unwrap_err();
    assert!(matches!(
        error,
        OpSpecError::Source(SourceError::InvalidTrimStart { .. })
    ));
    assert!(error.to_string().contains("trim start_seconds"), "{error}");
}

#[test]
fn a_negative_volume_off_the_wire_is_refused_naming_the_field() {
    let media = MediaRef::object_uri("s3://media/in/a.mp4").unwrap();
    let wire = v1::OpSpec {
        op: Some(transcode_spec(wire_source(&media, None, None, Some(-0.5)))),
    };
    let error = OpSpec::try_from(wire).unwrap_err();
    assert!(matches!(
        error,
        OpSpecError::Source(SourceError::InvalidVolume { .. })
    ));
    assert!(error.to_string().contains("volume"), "{error}");
}

#[test]
fn a_cut_an_op_cannot_apply_is_refused_naming_the_op_and_the_field() {
    let media = MediaRef::object_uri("s3://media/in/a.mp4").unwrap();
    let trim = v1::TrimRange {
        start_seconds: 0.0,
        duration_seconds: 1.0,
    };
    let wire = v1::OpSpec {
        op: Some(v1::op_spec::Op::Probe(v1::ProbeSpec {
            input: Some(wire_source(&media, Some(trim), None, None)),
        })),
    };
    let error = OpSpec::try_from(wire).unwrap_err();
    assert!(
        matches!(
            error,
            OpSpecError::InputCut {
                op: "probe",
                field: "trim"
            }
        ),
        "{error}"
    );
}

#[test]
fn a_source_that_names_no_media_is_refused() {
    let wire = v1::OpSpec {
        op: Some(v1::op_spec::Op::Probe(v1::ProbeSpec {
            input: Some(v1::InputSource {
                source: Some(v1::input_source::Source::Media(v1::MediaSource {
                    media: None,
                    trim: None,
                    speed: None,
                    volume: None,
                })),
            }),
        })),
    };
    assert_eq!(
        OpSpec::try_from(wire).unwrap_err(),
        OpSpecError::Source(SourceError::MissingMedia)
    );
}

#[test]
fn a_node_reference_round_trips_and_keeps_its_identity() {
    let wire = v1::OpSpec {
        op: Some(v1::op_spec::Op::Probe(v1::ProbeSpec {
            input: Some(v1::InputSource {
                source: Some(v1::input_source::Source::Node(v1::NodeRef {
                    id: "join".to_string(),
                    output: 2,
                })),
            }),
        })),
    };
    let OpSpec::Probe(spec) = OpSpec::try_from(wire.clone()).expect("a node parses") else {
        panic!("expected a probe spec");
    };
    let node = spec.input().node().expect("a node input");
    assert_eq!(node.id().as_str(), "join");
    assert_eq!(node.output(), 2);
    assert_eq!(v1::OpSpec::from(&OpSpec::Probe(spec)), wire);
}

#[test]
fn a_node_reference_with_an_empty_id_is_refused_by_name() {
    let wire = v1::OpSpec {
        op: Some(v1::op_spec::Op::Probe(v1::ProbeSpec {
            input: Some(v1::InputSource {
                source: Some(v1::input_source::Source::Node(v1::NodeRef {
                    id: String::new(),
                    output: 0,
                })),
            }),
        })),
    };
    let error = OpSpec::try_from(wire).expect_err("an empty node id is refused");
    assert!(error.to_string().contains("node id"), "{error}");
}

fn domain_probe() -> OpSpec {
    OpSpec::Probe(ProbeSpec::new(MediaRef::object_uri("s3://bucket/key").unwrap()).unwrap())
}

fn wire_probe() -> v1::OpSpec {
    v1::OpSpec {
        op: Some(v1::op_spec::Op::Probe(v1::ProbeSpec {
            input: Some(media_source(
                &MediaRef::object_uri("s3://bucket/key").unwrap(),
            )),
        })),
    }
}

#[test]
fn a_batch_spec_round_trips_with_its_policy() {
    let spec = OpSpec::Batch(BatchSpec::new(vec![domain_probe()], false).unwrap());
    let wire = v1::OpSpec::from(&spec);
    assert_eq!(OpSpec::try_from(wire.clone()).unwrap(), spec);
    assert_eq!(
        v1::OpSpec::from(&OpSpec::try_from(wire).unwrap()),
        v1::OpSpec::from(&spec)
    );
}

#[test]
fn an_unset_policy_stops_on_failure() {
    let wire = v1::OpSpec {
        op: Some(v1::op_spec::Op::Batch(v1::BatchSpec {
            steps: vec![wire_probe()],
            stop_on_failure: None,
        })),
    };
    let OpSpec::Batch(batch) = OpSpec::try_from(wire).unwrap() else {
        panic!("expected a batch spec");
    };
    assert!(batch.stop_on_failure());
}

#[test]
fn a_batch_result_round_trips_a_success_and_a_failure() {
    let result = OpResult::Batch(
        BatchResult::new(vec![
            StepOutcome::new(
                0,
                StepState::Result(OpResult::Probe(
                    ProbeResult::new(1.0, 1, 1, "mp4".to_string()).unwrap(),
                )),
            ),
            StepOutcome::new(
                1,
                StepState::Failure(JobFailure::new("probe_failed", "boom").unwrap()),
            ),
        ])
        .unwrap(),
    );
    let wire = v1::OpResult::from(&result);
    assert_eq!(OpResult::try_from(wire.clone()).unwrap(), result);
    assert_eq!(
        v1::OpResult::from(&OpResult::try_from(wire).unwrap()),
        v1::OpResult::from(&result)
    );
}

use std::time::Duration;

use mm_domain::{
    AudioReport, BlockCode, InputReport, MediaKind, OpKind, Verdict, VerdictCode, VideoReport,
    WarnCode, validate_concat_inputs, validate_inputs,
};

fn video() -> VideoReport {
    VideoReport {
        codec: "h264".to_string(),
        width: 1280,
        height: 720,
        avg_fps: Some(24.0),
        real_fps: Some(24.0),
        rotation: 0,
        sample_aspect_ratio: Some((1, 1)),
        interlaced: false,
    }
}

fn audio() -> AudioReport {
    AudioReport {
        codec: "aac".to_string(),
        channels: 2,
        sample_rate: Some(44_100),
        loudness_lufs: Some(-16.0),
    }
}

fn input() -> InputReport {
    InputReport {
        container: "mov,mp4,m4a,3gp,3g2,mj2".to_string(),
        duration: Duration::from_secs(20),
        size_bytes: Some(5 * 1024 * 1024),
        drm: false,
        kind: MediaKind::Video,
        video: vec![video()],
        audio: vec![audio()],
    }
}

fn blocks(verdicts: &[Verdict]) -> Vec<(Option<usize>, BlockCode)> {
    verdicts
        .iter()
        .filter_map(|verdict| match verdict.code {
            VerdictCode::Block(code) => Some((verdict.input, code)),
            VerdictCode::Warn(_) => None,
        })
        .collect()
}

fn warns(verdicts: &[Verdict]) -> Vec<(Option<usize>, WarnCode)> {
    verdicts
        .iter()
        .filter_map(|verdict| match verdict.code {
            VerdictCode::Warn(code) => Some((verdict.input, code)),
            VerdictCode::Block(_) => None,
        })
        .collect()
}

#[test]
fn a_well_formed_pair_passes_clean() {
    let verdicts = validate_concat_inputs(&[input(), input()]);
    assert!(verdicts.is_empty(), "unexpected verdicts: {verdicts:?}");
}

#[test]
fn block_matrix() {
    struct Case {
        name: &'static str,
        inputs: Vec<InputReport>,
        expected: (Option<usize>, BlockCode),
    }
    let cases = vec![
        Case {
            name: "unsupported container",
            inputs: vec![
                InputReport {
                    container: "matroska,webm".to_string(),
                    ..input()
                },
                input(),
            ],
            expected: (Some(0), BlockCode::UnsupportedContainer),
        },
        Case {
            name: "unsupported video codec",
            inputs: vec![
                input(),
                InputReport {
                    video: vec![VideoReport {
                        codec: "vp9".to_string(),
                        ..video()
                    }],
                    ..input()
                },
            ],
            expected: (Some(1), BlockCode::UnsupportedVideoCodec),
        },
        Case {
            name: "unsupported audio codec",
            inputs: vec![
                InputReport {
                    audio: vec![AudioReport {
                        codec: "opus".to_string(),
                        ..audio()
                    }],
                    ..input()
                },
                input(),
            ],
            expected: (Some(0), BlockCode::UnsupportedAudioCodec),
        },
        Case {
            name: "drm protected",
            inputs: vec![
                input(),
                InputReport {
                    drm: true,
                    ..input()
                },
            ],
            expected: (Some(1), BlockCode::DrmProtected),
        },
        Case {
            name: "file too large",
            inputs: vec![
                InputReport {
                    size_bytes: Some(500 * 1024 * 1024),
                    ..input()
                },
                input(),
            ],
            expected: (Some(0), BlockCode::FileTooLarge),
        },
        Case {
            name: "no video stream",
            inputs: vec![
                input(),
                InputReport {
                    video: vec![],
                    ..input()
                },
            ],
            expected: (Some(1), BlockCode::NoVideoStream),
        },
        Case {
            name: "fewer than two inputs",
            inputs: vec![input()],
            expected: (None, BlockCode::TooFewInputs),
        },
        Case {
            name: "resolution mismatch",
            inputs: vec![
                input(),
                InputReport {
                    video: vec![VideoReport {
                        width: 1920,
                        height: 1080,
                        ..video()
                    }],
                    ..input()
                },
            ],
            expected: (None, BlockCode::ResolutionMismatch),
        },
        Case {
            name: "total duration too long",
            inputs: vec![
                InputReport {
                    duration: Duration::from_secs(12 * 3600),
                    ..input()
                },
                InputReport {
                    duration: Duration::from_secs(12 * 3600),
                    ..input()
                },
            ],
            expected: (None, BlockCode::TotalDurationTooLong),
        },
    ];
    for case in cases {
        let verdicts = validate_concat_inputs(&case.inputs);
        assert!(
            blocks(&verdicts).contains(&case.expected),
            "{}: expected {:?} in {verdicts:?}",
            case.name,
            case.expected
        );
    }
}

#[test]
fn warn_matrix() {
    struct Case {
        name: &'static str,
        inputs: Vec<InputReport>,
        expected: (Option<usize>, WarnCode),
    }
    let cases = vec![
        Case {
            name: "aspect drift",
            inputs: vec![
                input(),
                InputReport {
                    video: vec![VideoReport {
                        sample_aspect_ratio: Some((4, 3)),
                        ..video()
                    }],
                    ..input()
                },
            ],
            expected: (None, WarnCode::AspectDrift),
        },
        Case {
            name: "fps drift",
            inputs: vec![
                input(),
                InputReport {
                    video: vec![VideoReport {
                        avg_fps: Some(25.0),
                        real_fps: Some(25.0),
                        ..video()
                    }],
                    ..input()
                },
            ],
            expected: (None, WarnCode::FpsDrift),
        },
        Case {
            name: "audio channel drift",
            inputs: vec![
                input(),
                InputReport {
                    audio: vec![AudioReport {
                        channels: 1,
                        ..audio()
                    }],
                    ..input()
                },
            ],
            expected: (None, WarnCode::AudioChannelDrift),
        },
        Case {
            name: "sample rate drift",
            inputs: vec![
                input(),
                InputReport {
                    audio: vec![AudioReport {
                        sample_rate: Some(48_000),
                        ..audio()
                    }],
                    ..input()
                },
            ],
            expected: (None, WarnCode::SampleRateDrift),
        },
        Case {
            name: "variable frame rate",
            inputs: vec![
                InputReport {
                    video: vec![VideoReport {
                        avg_fps: Some(29.97),
                        real_fps: Some(60.0),
                        ..video()
                    }],
                    ..input()
                },
                input(),
            ],
            expected: (Some(0), WarnCode::VariableFrameRate),
        },
        Case {
            name: "interlaced",
            inputs: vec![
                input(),
                InputReport {
                    video: vec![VideoReport {
                        interlaced: true,
                        ..video()
                    }],
                    ..input()
                },
            ],
            expected: (Some(1), WarnCode::Interlaced),
        },
        Case {
            name: "rotation",
            inputs: vec![
                InputReport {
                    video: vec![VideoReport {
                        rotation: -90,
                        ..video()
                    }],
                    ..input()
                },
                input(),
            ],
            expected: (Some(0), WarnCode::Rotation),
        },
        Case {
            name: "non-square pixels",
            inputs: vec![
                InputReport {
                    video: vec![VideoReport {
                        sample_aspect_ratio: Some((16, 11)),
                        ..video()
                    }],
                    ..input()
                },
                InputReport {
                    video: vec![VideoReport {
                        sample_aspect_ratio: Some((16, 11)),
                        ..video()
                    }],
                    ..input()
                },
            ],
            expected: (Some(0), WarnCode::NonSquarePixels),
        },
    ];
    for case in cases {
        let verdicts = validate_concat_inputs(&case.inputs);
        assert!(
            warns(&verdicts).contains(&case.expected),
            "{}: expected {:?} in {verdicts:?}",
            case.name,
            case.expected
        );
        assert!(
            blocks(&verdicts).is_empty(),
            "{}: unexpected blocks in {verdicts:?}",
            case.name
        );
    }
}

#[test]
fn matched_non_square_pixels_carry_no_aspect_drift() {
    let anamorphic = InputReport {
        video: vec![VideoReport {
            sample_aspect_ratio: Some((16, 11)),
            ..video()
        }],
        ..input()
    };
    let verdicts = validate_concat_inputs(&[anamorphic.clone(), anamorphic]);
    let warn_codes: Vec<WarnCode> = warns(&verdicts).into_iter().map(|(_, code)| code).collect();
    assert!(!warn_codes.contains(&WarnCode::AspectDrift));
    assert!(warn_codes.contains(&WarnCode::NonSquarePixels));
}

#[test]
fn sample_aspect_changes_display_aspect() {
    let anamorphic = InputReport {
        video: vec![VideoReport {
            sample_aspect_ratio: Some((4, 3)),
            ..video()
        }],
        ..input()
    };
    let verdicts = validate_concat_inputs(&[anamorphic, input()]);
    let warn_codes: Vec<WarnCode> = warns(&verdicts).into_iter().map(|(_, code)| code).collect();
    assert!(warn_codes.contains(&WarnCode::AspectDrift));
}

#[test]
fn size_just_below_the_cap_passes() {
    let verdicts = validate_concat_inputs(&[
        InputReport {
            size_bytes: Some(500 * 1024 * 1024 - 1),
            ..input()
        },
        input(),
    ]);
    assert!(blocks(&verdicts).is_empty());
}

#[test]
fn an_empty_input_list_is_too_few() {
    let verdicts = validate_concat_inputs(&[]);
    assert_eq!(blocks(&verdicts), vec![(None, BlockCode::TooFewInputs)]);
}

#[test]
fn inputs_without_audio_pass_the_audio_blocks() {
    let silent = InputReport {
        audio: vec![],
        ..input()
    };
    let verdicts = validate_concat_inputs(&[silent.clone(), silent]);
    assert!(
        blocks(&verdicts).is_empty(),
        "a silent input still merges: {verdicts:?}"
    );
}

#[test]
fn multiple_defects_accumulate() {
    let defective = InputReport {
        container: "avi".to_string(),
        drm: true,
        video: vec![],
        ..input()
    };
    let verdicts = validate_concat_inputs(&[defective]);
    let codes = blocks(&verdicts);
    assert!(codes.contains(&(Some(0), BlockCode::UnsupportedContainer)));
    assert!(codes.contains(&(Some(0), BlockCode::DrmProtected)));
    assert!(codes.contains(&(Some(0), BlockCode::NoVideoStream)));
    assert!(codes.contains(&(None, BlockCode::TooFewInputs)));
}

#[test]
fn mp3_audio_is_supported() {
    let with_mp3 = InputReport {
        audio: vec![AudioReport {
            codec: "mp3".to_string(),
            ..audio()
        }],
        ..input()
    };
    let verdicts = validate_concat_inputs(&[with_mp3.clone(), with_mp3]);
    assert!(blocks(&verdicts).is_empty());
}

#[test]
fn concat_policy_matches_the_concat_entry_point() {
    let reports = [input()];
    assert_eq!(
        validate_inputs(OpKind::Concat, &reports),
        validate_concat_inputs(&reports)
    );
    assert_eq!(
        blocks(&validate_inputs(OpKind::Concat, &reports)),
        vec![(None, BlockCode::TooFewInputs)]
    );
}

#[test]
fn a_single_input_satisfies_the_transcode_policy() {
    let verdicts = validate_inputs(OpKind::Transcode, &[input()]);
    assert!(verdicts.is_empty(), "{verdicts:?}");
}

#[test]
fn transcode_policy_still_blocks_a_defective_input() {
    let mut defective = input();
    defective.container = "matroska,webm".to_string();
    defective.drm = true;
    let found = blocks(&validate_inputs(OpKind::Transcode, &[defective]));
    assert!(found.contains(&(Some(0), BlockCode::UnsupportedContainer)));
    assert!(found.contains(&(Some(0), BlockCode::DrmProtected)));
}

#[test]
fn transcode_policy_rejects_no_inputs() {
    assert_eq!(
        blocks(&validate_inputs(OpKind::Transcode, &[])),
        vec![(None, BlockCode::TooFewInputs)]
    );
}

#[test]
fn transcode_policy_skips_cross_input_drift() {
    let mut other = input();
    other.video[0].width = 640;
    other.video[0].height = 480;
    other.video[0].avg_fps = Some(25.0);
    other.video[0].real_fps = Some(25.0);
    other.audio[0].loudness_lufs = Some(-40.0);
    let verdicts = validate_inputs(OpKind::Transcode, &[input(), other]);
    assert_eq!(
        warns(&verdicts),
        vec![(Some(1), WarnCode::FrameRateOffTarget)],
        "the per-input rows fire for one input of its own, the cross-input ones do not: \
         {verdicts:?}"
    );
}

#[test]
fn probe_policy_validates_nothing() {
    let mut defective = input();
    defective.container = "matroska,webm".to_string();
    defective.drm = true;
    defective.video.clear();
    assert!(validate_inputs(OpKind::Probe, &[defective]).is_empty());
    assert!(validate_inputs(OpKind::Probe, &[]).is_empty());
}

#[test]
fn transcode_policy_keeps_the_total_duration_cap() {
    let mut long = input();
    long.duration = Duration::from_secs(25 * 3600);
    assert_eq!(
        blocks(&validate_inputs(OpKind::Transcode, &[long])),
        vec![(None, BlockCode::TotalDurationTooLong)]
    );
}

#[test]
fn rendered_verdicts_round_trip_through_parse() {
    use mm_domain::{InputFailure, parse_rendered_verdicts, render_verdicts};
    let verdicts = vec![
        Verdict {
            input: Some(0),
            code: VerdictCode::Block(BlockCode::UnsupportedContainer),
        },
        Verdict {
            input: None,
            code: VerdictCode::Block(BlockCode::TooFewInputs),
        },
        Verdict {
            input: Some(2),
            code: VerdictCode::Warn(WarnCode::FpsDrift),
        },
    ];
    let text = render_verdicts(&verdicts);
    assert_eq!(
        text,
        "input 0: UnsupportedContainer; inputs: TooFewInputs; input 2: FpsDrift"
    );
    assert_eq!(
        parse_rendered_verdicts(&text).expect("the rendered grammar parses"),
        vec![
            InputFailure::new(None, None, Some(0), "UnsupportedContainer").expect("valid failure"),
            InputFailure::new(None, None, None, "TooFewInputs").expect("valid failure"),
            InputFailure::new(None, None, Some(2), "FpsDrift").expect("valid failure"),
        ]
    );
}

#[test]
fn a_message_outside_the_grammar_answers_a_typed_refusal() {
    use mm_domain::{InputFailureError, TextIdError, VerdictParseError, parse_rendered_verdicts};
    for text in [
        "",
        "connection reset by peer",
        "inputs TooFewInputs",
        "input 0: UnsupportedContainer; connection reset by peer",
    ] {
        assert_eq!(
            parse_rendered_verdicts(text),
            Err(VerdictParseError::Segment {
                segment: text
                    .split("; ")
                    .find(|segment| !segment.contains(": "))
                    .unwrap_or(text)
                    .to_string()
            }),
            "{text:?}"
        );
    }
    assert_eq!(
        parse_rendered_verdicts("input x: Broken"),
        Err(VerdictParseError::Subject {
            subject: "input x".to_string()
        })
    );
    assert_eq!(
        parse_rendered_verdicts("input 0: two words"),
        Err(VerdictParseError::Code {
            code: "two words".to_string()
        })
    );
    let long = "A".repeat(65);
    assert_eq!(
        parse_rendered_verdicts(&format!("inputs: {long}")),
        Err(VerdictParseError::Invalid {
            code: long.clone(),
            source: InputFailureError::Code(TextIdError::TooLong {
                length: 65,
                max: 64
            }),
        })
    );
}

fn image_input() -> InputReport {
    InputReport {
        container: "png_pipe".to_string(),
        duration: Duration::ZERO,
        size_bytes: Some(1024),
        drm: false,
        kind: MediaKind::Image,
        video: vec![VideoReport {
            codec: "png".to_string(),
            width: 640,
            height: 480,
            avg_fps: None,
            real_fps: None,
            rotation: 0,
            sample_aspect_ratio: None,
            interlaced: false,
        }],
        audio: Vec::new(),
    }
}

#[test]
fn an_image_input_blocks_a_video_op_with_the_media_kind_code() {
    let verdicts = validate_inputs(OpKind::Transcode, &[image_input()]);

    assert_eq!(
        blocks(&verdicts),
        vec![(Some(0), BlockCode::MediaKindMismatch)]
    );
}

#[test]
fn a_video_input_still_passes_a_video_op() {
    let verdicts = validate_inputs(OpKind::Transcode, &[input()]);

    assert!(blocks(&verdicts).is_empty(), "{verdicts:?}");
}

#[test]
fn an_image_input_passes_an_op_that_declares_images() {
    let verdicts = validate_inputs(OpKind::Resize, &[image_input()]);

    assert!(blocks(&verdicts).is_empty(), "{verdicts:?}");
}

#[test]
fn a_video_input_blocks_an_op_that_declares_images() {
    let verdicts = validate_inputs(OpKind::Resize, &[input()]);

    assert_eq!(
        blocks(&verdicts),
        vec![(Some(0), BlockCode::MediaKindMismatch)]
    );
}

#[test]
fn a_composite_needs_two_inputs_and_both_must_be_images() {
    let one = validate_inputs(OpKind::Composite, &[image_input()]);
    assert_eq!(blocks(&one), vec![(None, BlockCode::TooFewInputs)]);

    let both = validate_inputs(OpKind::Composite, &[image_input(), image_input()]);
    assert!(blocks(&both).is_empty(), "{both:?}");

    let mixed = validate_inputs(OpKind::Composite, &[image_input(), input()]);
    assert_eq!(
        blocks(&mixed),
        vec![(Some(1), BlockCode::MediaKindMismatch)]
    );
}

#[test]
fn a_video_input_without_an_audio_track_warns() {
    let silent = InputReport {
        audio: vec![],
        ..input()
    };
    let verdicts = validate_concat_inputs(&[silent.clone(), silent]);
    assert_eq!(
        warns(&verdicts),
        vec![
            (Some(0), WarnCode::NoAudioStream),
            (Some(1), WarnCode::NoAudioStream)
        ]
    );
    assert!(blocks(&verdicts).is_empty(), "a silent input still merges");
}

#[test]
fn a_still_without_an_audio_track_does_not_warn() {
    let still = InputReport {
        kind: MediaKind::Image,
        container: "png_pipe".to_string(),
        duration: Duration::ZERO,
        video: vec![VideoReport {
            codec: "png".to_string(),
            ..video()
        }],
        audio: vec![],
        ..input()
    };
    let verdicts = validate_concat_inputs(&[still.clone(), still]);
    assert!(
        !warns(&verdicts)
            .iter()
            .any(|(_, code)| *code == WarnCode::NoAudioStream),
        "a still has no audio track to warn about: {verdicts:?}"
    );
}

#[test]
fn a_frame_rate_off_the_reference_target_warns() {
    let thirty = InputReport {
        video: vec![VideoReport {
            avg_fps: Some(30.0),
            real_fps: Some(30.0),
            ..video()
        }],
        ..input()
    };
    let verdicts = validate_concat_inputs(&[thirty.clone(), thirty]);
    assert_eq!(
        warns(&verdicts),
        vec![
            (Some(0), WarnCode::FrameRateOffTarget),
            (Some(1), WarnCode::FrameRateOffTarget)
        ]
    );
}

#[test]
fn a_frame_rate_on_the_target_and_just_off_it_do_not_warn() {
    let on_target = InputReport {
        video: vec![VideoReport {
            avg_fps: Some(24.04),
            real_fps: Some(24.0),
            ..video()
        }],
        ..input()
    };
    let verdicts = validate_concat_inputs(&[on_target.clone(), on_target]);
    assert_eq!(
        warns(&verdicts),
        vec![
            (Some(0), WarnCode::VariableFrameRate),
            (Some(1), WarnCode::VariableFrameRate)
        ],
        "the target tolerance is 0.05fps, so 24.04 is on target"
    );
}

#[test]
fn a_loudness_difference_above_six_db_warns() {
    let quiet = InputReport {
        audio: vec![AudioReport {
            loudness_lufs: Some(-22.0),
            ..audio()
        }],
        ..input()
    };
    let loud = InputReport {
        audio: vec![AudioReport {
            loudness_lufs: Some(-14.0),
            ..audio()
        }],
        ..input()
    };
    let verdicts = validate_concat_inputs(&[quiet, loud]);
    assert!(
        warns(&verdicts).contains(&(None, WarnCode::LoudnessDrift)),
        "an eight dB gap warns: {verdicts:?}"
    );
}

#[test]
fn a_loudness_difference_of_exactly_six_db_does_not_warn() {
    let quiet = InputReport {
        audio: vec![AudioReport {
            loudness_lufs: Some(-20.0),
            ..audio()
        }],
        ..input()
    };
    let loud = InputReport {
        audio: vec![AudioReport {
            loudness_lufs: Some(-14.0),
            ..audio()
        }],
        ..input()
    };
    let verdicts = validate_concat_inputs(&[quiet, loud]);
    assert!(
        !warns(&verdicts)
            .iter()
            .any(|(_, code)| *code == WarnCode::LoudnessDrift),
        "six dB is the reference's threshold, not beyond it: {verdicts:?}"
    );
}

#[test]
fn an_unmeasured_loudness_leaves_the_row_silent() {
    let unmeasured = InputReport {
        audio: vec![AudioReport {
            loudness_lufs: None,
            ..audio()
        }],
        ..input()
    };
    let loud = InputReport {
        audio: vec![AudioReport {
            loudness_lufs: Some(-40.0),
            ..audio()
        }],
        ..input()
    };
    let verdicts = validate_concat_inputs(&[unmeasured, loud]);
    assert!(
        !warns(&verdicts)
            .iter()
            .any(|(_, code)| *code == WarnCode::LoudnessDrift),
        "a level nobody measured is not a claim about the other one: {verdicts:?}"
    );
}

#[test]
fn every_reference_warn_row_is_in_the_matrix() {
    let silent = InputReport {
        audio: vec![],
        ..input()
    };
    let off_target = InputReport {
        video: vec![VideoReport {
            avg_fps: Some(30.0),
            real_fps: Some(30.0),
            ..video()
        }],
        ..input()
    };
    let quiet = InputReport {
        audio: vec![AudioReport {
            loudness_lufs: Some(-30.0),
            ..audio()
        }],
        ..input()
    };
    let loud = InputReport {
        audio: vec![AudioReport {
            loudness_lufs: Some(-14.0),
            ..audio()
        }],
        ..input()
    };
    let rows: [(WarnCode, Vec<InputReport>); 3] = [
        (WarnCode::NoAudioStream, vec![silent.clone(), silent]),
        (
            WarnCode::FrameRateOffTarget,
            vec![off_target.clone(), off_target],
        ),
        (WarnCode::LoudnessDrift, vec![quiet, loud]),
    ];
    for (code, inputs) in rows {
        let verdicts = validate_concat_inputs(&inputs);
        assert!(
            warns(&verdicts).contains(&(None, code)) || warns(&verdicts).contains(&(Some(0), code)),
            "row {code:?} is not produced by its fixture: {verdicts:?}"
        );
        assert!(blocks(&verdicts).is_empty(), "row {code:?} is a warning");
    }
}

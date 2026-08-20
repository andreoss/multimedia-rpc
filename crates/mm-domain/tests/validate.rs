use std::time::Duration;

use mm_domain::{
    AudioReport, BlockCode, InputReport, Verdict, VerdictCode, VideoReport, WarnCode,
    validate_concat_inputs,
};

fn video() -> VideoReport {
    VideoReport {
        codec: "h264".to_string(),
        width: 1280,
        height: 720,
        avg_fps: Some(30.0),
        real_fps: Some(30.0),
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
    }
}

fn input() -> InputReport {
    InputReport {
        container: "mov,mp4,m4a,3gp,3g2,mj2".to_string(),
        duration: Duration::from_secs(20),
        size_bytes: Some(5 * 1024 * 1024),
        drm: false,
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
            name: "resolution drift",
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
            expected: (None, WarnCode::ResolutionDrift),
        },
        Case {
            name: "aspect drift",
            inputs: vec![
                input(),
                InputReport {
                    video: vec![VideoReport {
                        width: 1280,
                        height: 960,
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
fn inputs_without_audio_pass_audio_checks() {
    let silent = InputReport {
        audio: vec![],
        ..input()
    };
    let verdicts = validate_concat_inputs(&[silent.clone(), silent]);
    assert!(verdicts.is_empty(), "unexpected verdicts: {verdicts:?}");
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

use mm_domain::{
    BlockCode, InputFailure, JobFailure, VerdictCode, WarnCode, failure_code, verdict_code,
};

fn all_block_codes() -> Vec<BlockCode> {
    vec![
        BlockCode::UnsupportedContainer,
        BlockCode::UnsupportedVideoCodec,
        BlockCode::UnsupportedAudioCodec,
        BlockCode::DrmProtected,
        BlockCode::FileTooLarge,
        BlockCode::NoVideoStream,
        BlockCode::TooFewInputs,
        BlockCode::TotalDurationTooLong,
        BlockCode::MediaKindMismatch,
        BlockCode::ResolutionMismatch,
    ]
}

fn all_warn_codes() -> Vec<WarnCode> {
    vec![
        WarnCode::AspectDrift,
        WarnCode::FpsDrift,
        WarnCode::AudioChannelDrift,
        WarnCode::SampleRateDrift,
        WarnCode::VariableFrameRate,
        WarnCode::Interlaced,
        WarnCode::Rotation,
        WarnCode::NonSquarePixels,
    ]
}

#[test]
fn every_block_code_carries_the_reference_vocabulary() {
    let expected = [
        (
            BlockCode::UnsupportedContainer,
            "ERROR_UNSUPPORTED_CONTAINER",
        ),
        (
            BlockCode::UnsupportedVideoCodec,
            "ERROR_UNSUPPORTED_VIDEO_CODEC",
        ),
        (
            BlockCode::UnsupportedAudioCodec,
            "ERROR_UNSUPPORTED_AUDIO_CODEC",
        ),
        (BlockCode::DrmProtected, "ERROR_DRM_PROTECTED"),
        (BlockCode::FileTooLarge, "ERROR_FILE_TOO_LARGE"),
        (BlockCode::NoVideoStream, "ERROR_NO_VIDEO_TRACK"),
        (BlockCode::TooFewInputs, "ERROR_TOO_FEW_INPUTS"),
        (
            BlockCode::TotalDurationTooLong,
            "ERROR_TOTAL_DURATION_EXCEEDED",
        ),
        (BlockCode::MediaKindMismatch, "ERROR_MEDIA_KIND_MISMATCH"),
        (BlockCode::ResolutionMismatch, "ERROR_RESOLUTION_MISMATCH"),
    ];
    for (code, reference) in expected {
        let name = VerdictCode::Block(code).name();
        assert_eq!(verdict_code(name.as_str()), reference);
    }
}

#[test]
fn every_block_code_is_covered_by_the_matrix() {
    let names: Vec<String> = all_block_codes()
        .into_iter()
        .map(|code| VerdictCode::Block(code).name())
        .collect();
    for name in &names {
        assert!(
            verdict_code(name).starts_with("ERROR_"),
            "code {name} does not reach the wire in the reference shape"
        );
    }
}

#[test]
fn a_warn_code_is_carried_unchanged() {
    for code in all_warn_codes() {
        let name = VerdictCode::Warn(code).name();
        assert_eq!(verdict_code(name.as_str()), name);
    }
}

#[test]
fn an_unknown_verdict_name_is_carried_unchanged() {
    assert_eq!(verdict_code("NoSuchVerdict"), "NoSuchVerdict");
}

#[test]
fn mapping_a_verdict_twice_changes_nothing() {
    for code in all_block_codes() {
        let name = VerdictCode::Block(code).name();
        let once = verdict_code(name.as_str());
        assert_eq!(verdict_code(once), once);
    }
}

fn failure(code: &str, inputs: Vec<InputFailure>) -> JobFailure {
    JobFailure::new(code, "message")
        .expect("valid failure")
        .with_inputs(inputs)
}

fn input(code: &str) -> InputFailure {
    InputFailure::new(None, None, Some(0), code).expect("valid input failure")
}

#[test]
fn every_worker_failure_code_carries_the_reference_vocabulary() {
    let expected = [
        ("unsupported_operation", "ERROR_UNSUPPORTED_OPERATION"),
        ("input_unresolvable", "ERROR_INVALID_MEDIA_ID"),
        ("probe_failed", "ERROR_FILE_UNREADABLE"),
        ("validation_blocked", "ERROR_VALIDATION_FAILED"),
        ("timeout", "ERROR_JOB_TIMEOUT"),
        ("merge_launch_failed", "ERROR_MEDIACONVERT_SUBMIT_FAILED"),
        ("merge_failed", "ERROR_MEDIACONVERT_FAILED"),
        ("output_invalid", "ERROR_FINALIZE_OUTPUT_MISSING"),
        ("output_upload_failed", "ERROR_ASSET_REGISTRATION_FAILED"),
        ("image_launch_failed", "ERROR_IMAGE_LAUNCH_FAILED"),
        ("image_failed", "ERROR_IMAGE_FAILED"),
        ("package_launch_failed", "ERROR_PACKAGE_LAUNCH_FAILED"),
        ("package_failed", "ERROR_PACKAGE_FAILED"),
    ];
    for (code, reference) in expected {
        assert_eq!(failure_code(&failure(code, Vec::new())), reference);
    }
}

#[test]
fn a_blocked_validation_carries_the_first_block_of_its_own_inputs() {
    let blocked = failure(
        "validation_blocked",
        vec![input("UnsupportedContainer"), input("TooFewInputs")],
    );
    assert_eq!(failure_code(&blocked), "ERROR_UNSUPPORTED_CONTAINER");
}

#[test]
fn a_blocked_validation_whose_input_carries_the_reference_code_keeps_it() {
    let blocked = failure("validation_blocked", vec![input("ERROR_FILE_TOO_LARGE")]);
    assert_eq!(failure_code(&blocked), "ERROR_FILE_TOO_LARGE");
}

#[test]
fn a_code_the_reference_does_not_expose_is_left_alone() {
    assert_eq!(
        failure_code(&failure("ERROR_INTERNAL", Vec::new())),
        "ERROR_INTERNAL"
    );
}

#[test]
fn a_failure_code_nothing_maps_answers_the_reference_catch_all() {
    assert_eq!(
        failure_code(&failure("no_such_failure", Vec::new())),
        "ERROR_INTERNAL"
    );
}

#[test]
fn a_reference_failure_code_fits_the_failure_code_bound() {
    for code in [
        "ERROR_UNSUPPORTED_CONTAINER",
        "ERROR_TOTAL_DURATION_EXCEEDED",
        "ERROR_MEDIACONVERT_SUBMIT_FAILED",
        "ERROR_ASSET_REGISTRATION_FAILED",
    ] {
        assert!(JobFailure::new(code, "message").is_ok(), "code {code}");
    }
}

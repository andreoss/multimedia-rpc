use mm_domain::{BlockCode, VerdictCode, WarnCode, verdict_code};

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

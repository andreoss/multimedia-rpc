use crate::jobs::JobFailure;

const INTERNAL: &str = "ERROR_INTERNAL";

pub fn verdict_code(name: &str) -> &str {
    match name {
        "UnsupportedContainer" => "ERROR_UNSUPPORTED_CONTAINER",
        "UnsupportedVideoCodec" => "ERROR_UNSUPPORTED_VIDEO_CODEC",
        "UnsupportedAudioCodec" => "ERROR_UNSUPPORTED_AUDIO_CODEC",
        "DrmProtected" => "ERROR_DRM_PROTECTED",
        "FileTooLarge" => "ERROR_FILE_TOO_LARGE",
        "NoVideoStream" => "ERROR_NO_VIDEO_TRACK",
        "TooFewInputs" => "ERROR_TOO_FEW_INPUTS",
        "TotalDurationTooLong" => "ERROR_TOTAL_DURATION_EXCEEDED",
        "MediaKindMismatch" => "ERROR_MEDIA_KIND_MISMATCH",
        "ResolutionMismatch" => "ERROR_RESOLUTION_MISMATCH",
        "RungUpscale" => "ERROR_UPSCALE_NOT_ALLOWED",
        "NoAudioTrack" => "ERROR_NO_AUDIO_TRACK",
        other => other,
    }
}

pub fn failure_code(failure: &JobFailure) -> &str {
    match failure.code() {
        "unsupported_operation" => "ERROR_UNSUPPORTED_OPERATION",
        "input_unresolvable" => "ERROR_INVALID_MEDIA_ID",
        "probe_failed" => "ERROR_FILE_UNREADABLE",
        "validation_blocked" => match failure.inputs().first() {
            Some(input) => verdict_code(input.code()),
            None => "ERROR_VALIDATION_FAILED",
        },
        "timeout" => "ERROR_JOB_TIMEOUT",
        "merge_launch_failed" => "ERROR_MEDIACONVERT_SUBMIT_FAILED",
        "merge_failed" => "ERROR_MEDIACONVERT_FAILED",
        "output_invalid" => "ERROR_FINALIZE_OUTPUT_MISSING",
        "output_upload_failed" => "ERROR_ASSET_REGISTRATION_FAILED",
        "image_launch_failed" => "ERROR_IMAGE_LAUNCH_FAILED",
        "image_failed" => "ERROR_IMAGE_FAILED",
        "package_launch_failed" => "ERROR_PACKAGE_LAUNCH_FAILED",
        "package_failed" => "ERROR_PACKAGE_FAILED",
        "ladder_pack_failed" => "ERROR_LADDER_PACK_FAILED",
        "loudness_measure_failed" => "ERROR_LOUDNESS_MEASURE_FAILED",
        "loudness_failed" => "ERROR_LOUDNESS_FAILED",
        "caption_invalid" => "ERROR_SUBTITLE_INVALID",
        "grade_invalid" => "ERROR_GRADE_INVALID",
        "audio_invalid" => "ERROR_AUDIO_INVALID",
        "title_failed" => "ERROR_TITLE_FAILED",
        code if code.starts_with("ERROR_") => code,
        _ => INTERNAL,
    }
}

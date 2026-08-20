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
        other => other,
    }
}

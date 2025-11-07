mod concat;
mod parse;
mod probe;
mod profile;
mod progress;
mod runner;
mod thumbnail;
mod transcode;
mod trim;

pub use concat::{ConcatError, build_concat_args};
pub use parse::{
    AudioStream, MediaInfo, ParseError, StillInfo, VideoStream, parse_keyframe_times,
    parse_media_info, parse_still_info,
};
pub use probe::{ProbeError, ProbeLimits, Prober};
pub use profile::TargetProfile;
pub use progress::{ProgressParser, ProgressUpdate};
pub use runner::{CancelHandle, MergeRun, RunError, RunLimits, RunOutcome, Runner};
pub use thumbnail::{IMAGE_QUALITY, ImageFormat, ThumbnailTarget, build_thumbnail_args};
pub use transcode::{
    AudioCodec, ScaleMode, TranscodeError, TranscodeTarget, VideoCodec, build_transcode_args,
};
pub use trim::{
    KEYFRAME_TOLERANCE_SECONDS, TrimError, TrimStrategy, TrimTarget, aligned_to_keyframe,
    build_trim_args,
};

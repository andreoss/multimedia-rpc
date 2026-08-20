use crate::profile::{
    Scaling, TargetProfile, audio_filter, channel_layout, encode_args, video_filter,
};
use crate::timestamp::Timestamp;

pub const KEYFRAME_TOLERANCE_SECONDS: f64 = 0.02;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrimStrategy {
    Copy,
    Reencode,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrimTarget {
    pub strategy: TrimStrategy,
    pub profile: TargetProfile,
}

impl Default for TrimTarget {
    fn default() -> Self {
        Self {
            strategy: TrimStrategy::Reencode,
            profile: TargetProfile::default(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TrimError {
    #[error("unsupported audio channel count {channels}")]
    UnsupportedChannelCount { channels: u32 },
}

pub fn aligned_to_keyframe(keyframes: &[f64], start_seconds: f64) -> bool {
    keyframes
        .iter()
        .any(|keyframe| (keyframe - start_seconds).abs() <= KEYFRAME_TOLERANCE_SECONDS)
}

pub fn seconds_arg(value: Timestamp) -> String {
    value.argument()
}

pub fn build_trim_args(
    input: &str,
    target: &TrimTarget,
    start: Timestamp,
    duration: Timestamp,
    output: &str,
) -> Result<Vec<String>, TrimError> {
    let head: Vec<String> = [
        "-y",
        "-nostdin",
        "-hide_banner",
        "-ss",
        &seconds_arg(start),
        "-i",
        input,
        "-t",
        &seconds_arg(duration),
    ]
    .into_iter()
    .map(str::to_string)
    .collect();

    let mut args = match target.strategy {
        TrimStrategy::Copy => {
            let mut copy = head;
            copy.extend(
                [
                    "-c",
                    "copy",
                    "-avoid_negative_ts",
                    "make_zero",
                    "-movflags",
                    "+faststart",
                    "-f",
                    "mp4",
                ]
                .into_iter()
                .map(str::to_string),
            );
            copy
        }
        TrimStrategy::Reencode => {
            let layout = channel_layout(target.profile.audio_channels)
                .map_err(|channels| TrimError::UnsupportedChannelCount { channels })?;
            let mut reencode = head;
            reencode.push("-vf".to_string());
            reencode.push(video_filter(&Scaling::Keep, target.profile.fps));
            reencode.push("-af".to_string());
            reencode.push(audio_filter(target.profile.audio_sample_rate_hz, layout));
            reencode.extend(encode_args(&target.profile, "libx264", "aac"));
            reencode
        }
    };
    args.push(output.to_string());
    Ok(args)
}

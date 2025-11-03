use crate::profile::{
    Scaling, TargetProfile, audio_filter, channel_layout, encode_args, video_filter,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
}

impl VideoCodec {
    fn encoder(self) -> &'static str {
        match self {
            VideoCodec::H264 => "libx264",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    Aac,
}

impl AudioCodec {
    fn encoder(self) -> &'static str {
        match self {
            AudioCodec::Aac => "aac",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScaleMode {
    PadToFit,
    Stretch,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TranscodeTarget {
    pub video_codec: VideoCodec,
    pub audio_codec: AudioCodec,
    pub scale_mode: ScaleMode,
    pub profile: TargetProfile,
}

impl Default for TranscodeTarget {
    fn default() -> Self {
        Self {
            video_codec: VideoCodec::H264,
            audio_codec: AudioCodec::Aac,
            scale_mode: ScaleMode::PadToFit,
            profile: TargetProfile::default(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TranscodeError {
    #[error("unsupported audio channel count {channels}")]
    UnsupportedChannelCount { channels: u32 },
}

pub fn build_transcode_args(
    input: &str,
    target: &TranscodeTarget,
    output: &str,
) -> Result<Vec<String>, TranscodeError> {
    let layout = channel_layout(target.profile.audio_channels)
        .map_err(|channels| TranscodeError::UnsupportedChannelCount { channels })?;
    let scaling = match (target.profile.resolution, target.scale_mode) {
        (None, _) => Scaling::Keep,
        (Some((width, height)), ScaleMode::PadToFit) => Scaling::PadToFit(width, height),
        (Some((width, height)), ScaleMode::Stretch) => Scaling::Stretch(width, height),
    };

    let mut args: Vec<String> = ["-y", "-nostdin", "-hide_banner", "-i", input, "-vf"]
        .into_iter()
        .map(str::to_string)
        .collect();
    args.push(video_filter(&scaling, target.profile.fps));
    args.push("-af".to_string());
    args.push(audio_filter(target.profile.audio_sample_rate_hz, layout));
    args.extend(encode_args(
        &target.profile,
        target.video_codec.encoder(),
        target.audio_codec.encoder(),
    ));
    args.push(output.to_string());
    Ok(args)
}

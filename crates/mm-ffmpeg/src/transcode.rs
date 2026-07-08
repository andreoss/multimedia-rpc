use crate::profile::{
    Scaling, TargetProfile, audio_filter, channel_layout, encode_args, video_filter,
};
use crate::trim::seconds_arg;

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
    pub cut: Option<SourceCut>,
    pub speed: Option<f64>,
    pub volume: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SourceCut {
    pub start_seconds: f64,
    pub duration_seconds: f64,
}

impl Default for TranscodeTarget {
    fn default() -> Self {
        Self {
            video_codec: VideoCodec::H264,
            audio_codec: AudioCodec::Aac,
            scale_mode: ScaleMode::PadToFit,
            profile: TargetProfile::default(),
            cut: None,
            speed: None,
            volume: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TranscodeError {
    #[error("unsupported audio channel count {channels}")]
    UnsupportedChannelCount { channels: u32 },
}

fn factor_arg(value: f64) -> String {
    format!("{value:?}")
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

    let mut args: Vec<String> = ["-y", "-nostdin", "-hide_banner"]
        .into_iter()
        .map(str::to_string)
        .collect();
    if let Some(cut) = target.cut {
        args.push("-ss".to_string());
        args.push(seconds_arg(cut.start_seconds));
        args.push("-t".to_string());
        args.push(seconds_arg(cut.duration_seconds));
    }
    args.push("-i".to_string());
    args.push(input.to_string());
    args.push("-vf".to_string());
    let mut video = video_filter(&scaling, target.profile.fps);
    if let Some(speed) = target.speed {
        video = format!("setpts=PTS/{},{}", factor_arg(speed), video);
    }
    args.push(video);
    args.push("-af".to_string());
    let mut audio = audio_filter(target.profile.audio_sample_rate_hz, layout);
    if let Some(speed) = target.speed {
        audio.push_str(&format!(",atempo={}", factor_arg(speed)));
    }
    if let Some(volume) = target.volume {
        audio.push_str(&format!(",volume={}", factor_arg(volume)));
    }
    args.push(audio);
    args.extend(encode_args(
        &target.profile,
        target.video_codec.encoder(),
        target.audio_codec.encoder(),
    ));
    args.push(output.to_string());
    Ok(args)
}

#[derive(Debug, Clone, PartialEq)]
pub struct TargetProfile {
    pub resolution: Option<(u32, u32)>,
    pub fps: u32,
    pub video_crf: u32,
    pub video_max_bitrate_kbps: u32,
    pub audio_bitrate_kbps: u32,
    pub audio_sample_rate_hz: u32,
    pub audio_channels: u32,
}

impl Default for TargetProfile {
    fn default() -> Self {
        Self {
            resolution: None,
            fps: 24,
            video_crf: 23,
            video_max_bitrate_kbps: 5000,
            audio_bitrate_kbps: 128,
            audio_sample_rate_hz: 48_000,
            audio_channels: 2,
        }
    }
}

pub(crate) fn channel_layout(channels: u32) -> Result<&'static str, u32> {
    match channels {
        1 => Ok("mono"),
        2 => Ok("stereo"),
        other => Err(other),
    }
}

pub(crate) enum Scaling {
    PadToFit(u32, u32),
    Stretch(u32, u32),
    Keep,
}

pub(crate) fn video_filter(scaling: &Scaling, fps: u32) -> String {
    match scaling {
        Scaling::PadToFit(width, height) => format!(
            "scale=w={width}:h={height}:force_original_aspect_ratio=decrease,\
             pad=w={width}:h={height}:x=(ow-iw)/2:y=(oh-ih)/2,setsar=1,fps={fps},\
             format=yuv420p"
        ),
        Scaling::Stretch(width, height) => {
            format!("scale=w={width}:h={height},setsar=1,fps={fps},format=yuv420p")
        }
        Scaling::Keep => format!("fps={fps},format=yuv420p"),
    }
}

pub(crate) fn audio_filter(sample_rate_hz: u32, layout: &str) -> String {
    format!(
        "aformat=sample_fmts=fltp:sample_rates={sample_rate_hz}:\
         channel_layouts={layout}"
    )
}

pub(crate) fn encode_args(
    profile: &TargetProfile,
    video_encoder: &str,
    audio_encoder: &str,
) -> Vec<String> {
    [
        "-c:v",
        video_encoder,
        "-crf",
        &profile.video_crf.to_string(),
        "-maxrate",
        &format!("{}k", profile.video_max_bitrate_kbps),
        "-bufsize",
        &format!("{}k", profile.video_max_bitrate_kbps * 2),
        "-c:a",
        audio_encoder,
        "-b:a",
        &format!("{}k", profile.audio_bitrate_kbps),
        "-ar",
        &profile.audio_sample_rate_hz.to_string(),
        "-ac",
        &profile.audio_channels.to_string(),
        "-movflags",
        "+faststart",
        "-f",
        "mp4",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

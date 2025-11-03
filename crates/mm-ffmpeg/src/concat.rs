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

#[derive(Debug, thiserror::Error)]
pub enum ConcatError {
    #[error("concat needs at least two inputs, got {count}")]
    TooFewInputs { count: usize },
    #[error("unsupported audio channel count {channels}")]
    UnsupportedChannelCount { channels: u32 },
}

fn channel_layout(channels: u32) -> Result<&'static str, ConcatError> {
    match channels {
        1 => Ok("mono"),
        2 => Ok("stereo"),
        other => Err(ConcatError::UnsupportedChannelCount { channels: other }),
    }
}

pub fn build_concat_args(
    inputs: &[String],
    profile: &TargetProfile,
    first_input_resolution: Option<(u32, u32)>,
    output: &str,
) -> Result<Vec<String>, ConcatError> {
    if inputs.len() < 2 {
        return Err(ConcatError::TooFewInputs {
            count: inputs.len(),
        });
    }
    let (width, height) = profile
        .resolution
        .or(first_input_resolution)
        .unwrap_or((1920, 1080));
    let layout = channel_layout(profile.audio_channels)?;

    let mut filter = String::new();
    for index in 0..inputs.len() {
        filter.push_str(&format!(
            "[{index}:v]scale=w={width}:h={height}:force_original_aspect_ratio=decrease,\
             pad=w={width}:h={height}:x=(ow-iw)/2:y=(oh-ih)/2,setsar=1,fps={fps},\
             format=yuv420p[v{index}];\
             [{index}:a]aformat=sample_fmts=fltp:sample_rates={rate}:\
             channel_layouts={layout}[a{index}];",
            fps = profile.fps,
            rate = profile.audio_sample_rate_hz,
        ));
    }
    for index in 0..inputs.len() {
        filter.push_str(&format!("[v{index}][a{index}]"));
    }
    filter.push_str(&format!(
        "concat=n={count}:v=1:a=1[vout][aout]",
        count = inputs.len()
    ));

    let mut args: Vec<String> = ["-y", "-nostdin", "-hide_banner"]
        .into_iter()
        .map(str::to_string)
        .collect();
    for input in inputs {
        args.push("-i".to_string());
        args.push(input.clone());
    }
    args.extend(
        [
            "-filter_complex",
            &filter,
            "-map",
            "[vout]",
            "-map",
            "[aout]",
            "-c:v",
            "libx264",
            "-crf",
            &profile.video_crf.to_string(),
            "-maxrate",
            &format!("{}k", profile.video_max_bitrate_kbps),
            "-bufsize",
            &format!("{}k", profile.video_max_bitrate_kbps * 2),
            "-c:a",
            "aac",
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
            output,
        ]
        .into_iter()
        .map(str::to_string),
    );
    Ok(args)
}

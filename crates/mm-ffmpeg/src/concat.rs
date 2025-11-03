use crate::profile::{
    Scaling, TargetProfile, audio_filter, channel_layout, encode_args, video_filter,
};

#[derive(Debug, thiserror::Error)]
pub enum ConcatError {
    #[error("concat needs at least two inputs, got {count}")]
    TooFewInputs { count: usize },
    #[error("unsupported audio channel count {channels}")]
    UnsupportedChannelCount { channels: u32 },
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
    let layout = channel_layout(profile.audio_channels)
        .map_err(|channels| ConcatError::UnsupportedChannelCount { channels })?;
    let normalize = video_filter(&Scaling::PadToFit(width, height), profile.fps);
    let voice = audio_filter(profile.audio_sample_rate_hz, layout);

    let mut filter = String::new();
    for index in 0..inputs.len() {
        filter.push_str(&format!(
            "[{index}:v]{normalize}[v{index}];[{index}:a]{voice}[a{index}];"
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
        ]
        .into_iter()
        .map(str::to_string),
    );
    args.extend(encode_args(profile, "libx264", "aac"));
    args.push(output.to_string());
    Ok(args)
}

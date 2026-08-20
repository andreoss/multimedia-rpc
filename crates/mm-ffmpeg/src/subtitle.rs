use crate::profile::{
    Scaling, TargetProfile, audio_filter, channel_layout, encode_args, video_filter,
};

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SubtitleError {
    #[error("unsupported audio channel count {channels}")]
    UnsupportedChannelCount { channels: u32 },
}

fn filter_path(path: &str) -> String {
    path.replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace(':', "\\:")
        .replace(',', "\\,")
}

pub fn build_subtitle_burn_args(
    input: &str,
    captions: &str,
    profile: &TargetProfile,
    output: &str,
) -> Result<Vec<String>, SubtitleError> {
    let layout = channel_layout(profile.audio_channels)
        .map_err(|channels| SubtitleError::UnsupportedChannelCount { channels })?;
    let filter = format!(
        "subtitles=filename='{}',{}",
        filter_path(captions),
        video_filter(&Scaling::Keep, profile.fps)
    );
    let mut args: Vec<String> = [
        "-y",
        "-nostdin",
        "-hide_banner",
        "-i",
        input,
        "-vf",
        &filter,
        "-af",
        &audio_filter(profile.audio_sample_rate_hz, layout),
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    args.extend(encode_args(profile, "libx264", "aac"));
    args.push(output.to_string());
    Ok(args)
}

pub fn build_subtitle_mux_args(input: &str, captions: &str, output: &str) -> Vec<String> {
    [
        "-y".to_string(),
        "-nostdin".to_string(),
        "-hide_banner".to_string(),
        "-i".to_string(),
        input.to_string(),
        "-i".to_string(),
        captions.to_string(),
        "-map".to_string(),
        "0:v?".to_string(),
        "-map".to_string(),
        "0:a?".to_string(),
        "-map".to_string(),
        "1:0".to_string(),
        "-c".to_string(),
        "copy".to_string(),
        "-c:s".to_string(),
        "mov_text".to_string(),
        "-movflags".to_string(),
        "+faststart".to_string(),
        "-f".to_string(),
        "mp4".to_string(),
        output.to_string(),
    ]
    .to_vec()
}

pub fn build_subtitle_strip_args(input: &str, output: &str) -> Vec<String> {
    [
        "-y".to_string(),
        "-nostdin".to_string(),
        "-hide_banner".to_string(),
        "-i".to_string(),
        input.to_string(),
        "-map".to_string(),
        "0".to_string(),
        "-c".to_string(),
        "copy".to_string(),
        "-sn".to_string(),
        "-movflags".to_string(),
        "+faststart".to_string(),
        "-f".to_string(),
        "mp4".to_string(),
        output.to_string(),
    ]
    .to_vec()
}

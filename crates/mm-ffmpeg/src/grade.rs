use crate::profile::{
    Scaling, TargetProfile, audio_filter, channel_layout, encode_args, video_filter,
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GradeTarget {
    pub brightness: f64,
    pub contrast: f64,
    pub saturation: f64,
    pub gamma: f64,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum GradeError {
    #[error("unsupported audio channel count {channels}")]
    UnsupportedChannelCount { channels: u32 },
    #[error("the lookup table declares no LUT_3D_SIZE")]
    MissingLutSize,
    #[error("the lookup table size {value:?} is not a number")]
    MalformedLutSize { value: String },
    #[error("the lookup table size {size} is outside 2 to 256")]
    LutSizeOutOfRange { size: usize },
    #[error("the lookup table holds {found} entries, expected {expected}")]
    LutEntries { found: usize, expected: usize },
}

fn filter_path(path: &str) -> String {
    path.replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace(':', "\\:")
        .replace(',', "\\,")
}

pub fn parse_cube_table(text: &str) -> Result<usize, GradeError> {
    let mut size = None;
    let mut entries = 0usize;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(value) = line.strip_prefix("LUT_3D_SIZE") {
            let value = value.trim();
            let parsed: usize = value.parse().map_err(|_| GradeError::MalformedLutSize {
                value: value.to_string(),
            })?;
            size = Some(parsed);
        } else if line
            .split_whitespace()
            .all(|part| part.parse::<f64>().is_ok())
            && line.split_whitespace().count() == 3
        {
            entries += 1;
        }
    }
    let size = size.ok_or(GradeError::MissingLutSize)?;
    if !(2..=256).contains(&size) {
        return Err(GradeError::LutSizeOutOfRange { size });
    }
    let expected = size * size * size;
    if entries < expected {
        return Err(GradeError::LutEntries {
            found: entries,
            expected,
        });
    }
    Ok(size)
}

pub fn build_grade_args(
    input: &str,
    target: &GradeTarget,
    lookup_table: Option<&str>,
    profile: &TargetProfile,
    output: &str,
) -> Result<Vec<String>, GradeError> {
    let layout = channel_layout(profile.audio_channels)
        .map_err(|channels| GradeError::UnsupportedChannelCount { channels })?;
    let mut filter = format!(
        "eq=brightness={}:contrast={}:saturation={}:gamma={}",
        target.brightness, target.contrast, target.saturation, target.gamma
    );
    if let Some(lookup_table) = lookup_table {
        filter.push_str(&format!(",lut3d=file='{}'", filter_path(lookup_table)));
    }
    filter.push(',');
    filter.push_str(&video_filter(&Scaling::Keep, profile.fps));
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

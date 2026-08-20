use crate::profile::video_filter;
use crate::timestamp::Timestamp;

pub const IMAGE_QUALITY: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Jpeg,
}

impl ImageFormat {
    pub fn extension(self) -> &'static str {
        match self {
            ImageFormat::Jpeg => "jpg",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThumbnailTarget {
    pub width: u32,
    pub height: u32,
    pub format: ImageFormat,
}

pub fn build_thumbnail_args(
    input: &str,
    offset: Timestamp,
    target: &ThumbnailTarget,
    output: &str,
) -> Vec<String> {
    let offset = offset.argument();
    [
        "-y",
        "-nostdin",
        "-hide_banner",
        "-ss",
        &offset,
        "-i",
        input,
        "-frames:v",
        "1",
        "-vf",
        &video_filter(
            &crate::profile::Scaling::PadToFit(target.width, target.height),
            1,
        ),
        "-q:v",
        &IMAGE_QUALITY.to_string(),
        "-f",
        "image2",
    ]
    .into_iter()
    .map(str::to_string)
    .chain(std::iter::once(output.to_string()))
    .collect()
}

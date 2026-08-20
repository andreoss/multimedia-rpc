use crate::profile::{Scaling, video_filter};
use crate::thumbnail::IMAGE_QUALITY;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeKind {
    Frame,
    Waveform,
    Parade,
    Histogram,
    Vectorscope,
}

fn filter(kind: ScopeKind, width: u32, height: u32) -> String {
    match kind {
        ScopeKind::Frame => video_filter(&Scaling::PadToFit(width, height), 1),
        ScopeKind::Waveform => {
            format!(
                "format=yuv444p,waveform=mode=column:components=1:display=overlay,scale={width}:{height}"
            )
        }
        ScopeKind::Parade => {
            format!(
                "format=yuv444p,waveform=mode=column:components=7:display=parade,scale={width}:{height}"
            )
        }
        ScopeKind::Histogram => {
            format!("format=yuv444p,histogram=display_mode=overlay,scale={width}:{height}")
        }
        ScopeKind::Vectorscope => {
            format!("format=yuv444p,vectorscope=mode=color3,scale={width}:{height}")
        }
    }
}

pub fn build_scope_args(
    input: &str,
    kind: ScopeKind,
    width: u32,
    height: u32,
    output: &str,
) -> Vec<String> {
    [
        "-y",
        "-nostdin",
        "-hide_banner",
        "-ss",
        "0",
        "-i",
        input,
        "-frames:v",
        "1",
        "-vf",
        &filter(kind, width, height),
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

pub fn build_frame_sample_args(input: &str, output: &str) -> Vec<String> {
    [
        "-y",
        "-nostdin",
        "-hide_banner",
        "-i",
        input,
        "-frames:v",
        "1",
        "-pix_fmt",
        "rgb24",
        "-f",
        "rawvideo",
    ]
    .into_iter()
    .map(str::to_string)
    .chain(std::iter::once(output.to_string()))
    .collect()
}

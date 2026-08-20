use std::time::Duration;

use crate::jobs::{InputFailure, InputFailureError};
use crate::op::OpKind;
use crate::op::OutputProfile;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Video,
    Image,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InputReport {
    pub container: String,
    pub duration: Duration,
    pub size_bytes: Option<u64>,
    pub drm: bool,
    pub kind: MediaKind,
    pub video: Vec<VideoReport>,
    pub audio: Vec<AudioReport>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoReport {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub avg_fps: Option<f64>,
    pub real_fps: Option<f64>,
    pub rotation: i32,
    pub sample_aspect_ratio: Option<(u32, u32)>,
    pub interlaced: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioReport {
    pub codec: String,
    pub channels: u32,
    pub sample_rate: Option<u32>,
    pub loudness_lufs: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockCode {
    UnsupportedContainer,
    UnsupportedVideoCodec,
    UnsupportedAudioCodec,
    DrmProtected,
    FileTooLarge,
    NoVideoStream,
    TooFewInputs,
    TotalDurationTooLong,
    MediaKindMismatch,
    ResolutionMismatch,
    RungUpscale,
    NoAudioTrack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WarnCode {
    AspectDrift,
    FpsDrift,
    FrameRateOffTarget,
    AudioChannelDrift,
    SampleRateDrift,
    LoudnessDrift,
    NoAudioStream,
    VariableFrameRate,
    Interlaced,
    Rotation,
    NonSquarePixels,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VerdictCode {
    Block(BlockCode),
    Warn(WarnCode),
}

impl VerdictCode {
    pub fn name(self) -> String {
        match self {
            VerdictCode::Block(code) => format!("{code:?}"),
            VerdictCode::Warn(code) => format!("{code:?}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Verdict {
    pub input: Option<usize>,
    pub rung: Option<usize>,
    pub code: VerdictCode,
}

const SUPPORTED_CONTAINERS: [&str; 2] = ["mp4", "mov"];
const SUPPORTED_VIDEO_CODECS: [&str; 1] = ["h264"];
const SUPPORTED_AUDIO_CODECS: [&str; 2] = ["aac", "mp3"];
const MAX_INPUT_BYTES: u64 = 500 * 1024 * 1024;
const MAX_TOTAL_DURATION: Duration = Duration::from_secs(24 * 3600);
const MIN_CONCAT_INPUTS: usize = 2;
const FPS_TOLERANCE: f64 = 0.01;
const ASPECT_TOLERANCE: f64 = 0.01;
const TARGET_FPS: f64 = 24.0;
const TARGET_FPS_TOLERANCE: f64 = 0.05;
const MAX_LOUDNESS_DELTA_DB: f64 = 6.0;

fn block(input: Option<usize>, code: BlockCode) -> Verdict {
    Verdict {
        rung: None,
        input,
        code: VerdictCode::Block(code),
    }
}

fn warn(input: Option<usize>, code: WarnCode) -> Verdict {
    Verdict {
        rung: None,
        input,
        code: VerdictCode::Warn(code),
    }
}

fn container_is_supported(container: &str, supported: &[&str]) -> bool {
    container
        .split(',')
        .any(|token| supported.contains(&token.trim()))
}

fn display_aspect(video: &VideoReport) -> Option<f64> {
    if video.height == 0 {
        return None;
    }
    let (sar_width, sar_height) = video.sample_aspect_ratio.unwrap_or((1, 1));
    if sar_height == 0 {
        return None;
    }
    Some(
        (f64::from(video.width) * f64::from(sar_width))
            / (f64::from(video.height) * f64::from(sar_height)),
    )
}

fn push_unique(verdicts: &mut Vec<Verdict>, verdict: Verdict) {
    if !verdicts.contains(&verdict) {
        verdicts.push(verdict);
    }
}

fn check_one(
    index: usize,
    report: &InputReport,
    policy: &InputPolicy,
    verdicts: &mut Vec<Verdict>,
) {
    let input = Some(index);
    if !policy.kinds.contains(&report.kind) {
        verdicts.push(block(input, BlockCode::MediaKindMismatch));
        return;
    }
    if !container_is_supported(&report.container, policy.containers) {
        verdicts.push(block(input, BlockCode::UnsupportedContainer));
    }
    if report.drm {
        verdicts.push(block(input, BlockCode::DrmProtected));
    }
    if report
        .size_bytes
        .is_some_and(|size| size >= MAX_INPUT_BYTES)
    {
        verdicts.push(block(input, BlockCode::FileTooLarge));
    }
    if report.video.is_empty() {
        verdicts.push(block(input, BlockCode::NoVideoStream));
    }
    for video in &report.video {
        if !policy.video_codecs.contains(&video.codec.as_str()) {
            push_unique(verdicts, block(input, BlockCode::UnsupportedVideoCodec));
        }
        if let (Some(avg), Some(real)) = (video.avg_fps, video.real_fps)
            && (avg - real).abs() > FPS_TOLERANCE
        {
            push_unique(verdicts, warn(input, WarnCode::VariableFrameRate));
        }
        if video.interlaced {
            push_unique(verdicts, warn(input, WarnCode::Interlaced));
        }
        if video.rotation.rem_euclid(360) != 0 {
            push_unique(verdicts, warn(input, WarnCode::Rotation));
        }
        if let Some((sar_width, sar_height)) = video.sample_aspect_ratio
            && sar_width != sar_height
        {
            push_unique(verdicts, warn(input, WarnCode::NonSquarePixels));
        }
        if video
            .avg_fps
            .is_some_and(|fps| (fps - TARGET_FPS).abs() > TARGET_FPS_TOLERANCE)
        {
            push_unique(verdicts, warn(input, WarnCode::FrameRateOffTarget));
        }
    }
    if report.audio.is_empty() && !report.video.is_empty() {
        push_unique(verdicts, warn(input, WarnCode::NoAudioStream));
    }
    for audio in &report.audio {
        if !policy.audio_codecs.contains(&audio.codec.as_str()) {
            push_unique(verdicts, block(input, BlockCode::UnsupportedAudioCodec));
        }
    }
}

fn check_drift(inputs: &[InputReport], verdicts: &mut Vec<Verdict>) {
    let leads: Vec<&VideoReport> = inputs
        .iter()
        .filter_map(|report| report.video.first())
        .collect();
    if let Some((first, rest)) = leads.split_first() {
        if rest
            .iter()
            .any(|video| (video.width, video.height) != (first.width, first.height))
        {
            verdicts.push(block(None, BlockCode::ResolutionMismatch));
        }
        if let Some(reference) = display_aspect(first)
            && rest.iter().any(|video| {
                display_aspect(video)
                    .is_some_and(|aspect| (aspect - reference).abs() > ASPECT_TOLERANCE)
            })
        {
            verdicts.push(warn(None, WarnCode::AspectDrift));
        }
        if let Some(reference) = first.avg_fps
            && rest.iter().any(|video| {
                video
                    .avg_fps
                    .is_some_and(|fps| (fps - reference).abs() > FPS_TOLERANCE)
            })
        {
            verdicts.push(warn(None, WarnCode::FpsDrift));
        }
    }

    let voices: Vec<&AudioReport> = inputs
        .iter()
        .filter_map(|report| report.audio.first())
        .collect();
    if let Some((first, rest)) = voices.split_first() {
        if rest.iter().any(|audio| audio.channels != first.channels) {
            verdicts.push(warn(None, WarnCode::AudioChannelDrift));
        }
        if rest
            .iter()
            .any(|audio| audio.sample_rate != first.sample_rate)
        {
            verdicts.push(warn(None, WarnCode::SampleRateDrift));
        }
        if let Some(reference) = first.loudness_lufs
            && rest.iter().any(|audio| {
                audio
                    .loudness_lufs
                    .is_some_and(|level| (level - reference).abs() > MAX_LOUDNESS_DELTA_DB)
            })
        {
            verdicts.push(warn(None, WarnCode::LoudnessDrift));
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputPolicy {
    pub min_inputs: usize,
    pub cross_input_checks: bool,
    pub kinds: &'static [MediaKind],
    pub containers: &'static [&'static str],
    pub video_codecs: &'static [&'static str],
    pub audio_codecs: &'static [&'static str],
}

const VIDEO_KINDS: &[MediaKind] = &[MediaKind::Video];
const IMAGE_KINDS: &[MediaKind] = &[MediaKind::Image];
const SUPPORTED_IMAGE_CONTAINERS: [&str; 5] =
    ["image2", "image_pipe", "png_pipe", "jpeg_pipe", "webp_pipe"];
const SUPPORTED_IMAGE_CODECS: [&str; 3] = ["png", "mjpeg", "webp"];

impl OpKind {
    pub fn input_policy(self) -> Option<InputPolicy> {
        match self {
            OpKind::Concat => Some(InputPolicy {
                min_inputs: MIN_CONCAT_INPUTS,
                cross_input_checks: true,
                kinds: VIDEO_KINDS,
                containers: &SUPPORTED_CONTAINERS,
                video_codecs: &SUPPORTED_VIDEO_CODECS,
                audio_codecs: &SUPPORTED_AUDIO_CODECS,
            }),
            OpKind::Transcode
            | OpKind::Trim
            | OpKind::Thumbnail
            | OpKind::Ladder
            | OpKind::Loudness
            | OpKind::Subtitle
            | OpKind::Grade => Some(InputPolicy {
                min_inputs: 1,
                cross_input_checks: false,
                kinds: VIDEO_KINDS,
                containers: &SUPPORTED_CONTAINERS,
                video_codecs: &SUPPORTED_VIDEO_CODECS,
                audio_codecs: &SUPPORTED_AUDIO_CODECS,
            }),
            OpKind::Resize | OpKind::Convert => Some(InputPolicy {
                min_inputs: 1,
                cross_input_checks: false,
                kinds: IMAGE_KINDS,
                containers: &SUPPORTED_IMAGE_CONTAINERS,
                video_codecs: &SUPPORTED_IMAGE_CODECS,
                audio_codecs: &[],
            }),
            OpKind::Composite => Some(InputPolicy {
                min_inputs: 2,
                cross_input_checks: false,
                kinds: IMAGE_KINDS,
                containers: &SUPPORTED_IMAGE_CONTAINERS,
                video_codecs: &SUPPORTED_IMAGE_CODECS,
                audio_codecs: &[],
            }),
            OpKind::Package => Some(InputPolicy {
                min_inputs: 1,
                cross_input_checks: false,
                kinds: VIDEO_KINDS,
                containers: &SUPPORTED_CONTAINERS,
                video_codecs: &SUPPORTED_VIDEO_CODECS,
                audio_codecs: &SUPPORTED_AUDIO_CODECS,
            }),
            OpKind::Probe | OpKind::Batch | OpKind::Graph => None,
        }
    }
}

pub fn validate_inputs(kind: OpKind, inputs: &[InputReport]) -> Vec<Verdict> {
    let Some(policy) = kind.input_policy() else {
        return Vec::new();
    };
    let mut verdicts = Vec::new();
    if inputs.len() < policy.min_inputs {
        verdicts.push(block(None, BlockCode::TooFewInputs));
    }
    let total: Duration = inputs.iter().map(|report| report.duration).sum();
    if total >= MAX_TOTAL_DURATION {
        verdicts.push(block(None, BlockCode::TotalDurationTooLong));
    }
    for (index, report) in inputs.iter().enumerate() {
        check_one(index, report, &policy, &mut verdicts);
    }
    if policy.cross_input_checks {
        check_drift(inputs, &mut verdicts);
    }
    if kind == OpKind::Loudness {
        for (index, report) in inputs.iter().enumerate() {
            if report.audio.is_empty() {
                push_unique(&mut verdicts, block(Some(index), BlockCode::NoAudioTrack));
            }
        }
    }
    verdicts
}

pub fn validate_ladder_rungs(rungs: &[OutputProfile], source: (u32, u32)) -> Vec<Verdict> {
    rungs
        .iter()
        .enumerate()
        .filter(|(_, profile)| {
            let Some((width, height)) = profile.resolution() else {
                return false;
            };
            width > source.0 || height > source.1
        })
        .map(|(rung, _)| Verdict {
            input: None,
            rung: Some(rung),
            code: VerdictCode::Block(BlockCode::RungUpscale),
        })
        .collect()
}

pub fn validate_concat_inputs(inputs: &[InputReport]) -> Vec<Verdict> {
    validate_inputs(OpKind::Concat, inputs)
}

pub fn render_verdicts(verdicts: &[Verdict]) -> String {
    verdicts
        .iter()
        .map(|verdict| {
            let code = verdict.code.name();
            match (verdict.input, verdict.rung) {
                (Some(index), _) => format!("input {index}: {code}"),
                (None, Some(rung)) => format!("rung {rung}: {code}"),
                (None, None) => format!("inputs: {code}"),
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum VerdictParseError {
    #[error("the segment {segment:?} is not a rendered verdict")]
    Segment { segment: String },
    #[error("the subject {subject:?} names no input")]
    Subject { subject: String },
    #[error("the code {code:?} is not a verdict code")]
    Code { code: String },
    #[error("the code {code:?} is not an input failure: {source}")]
    Invalid {
        code: String,
        source: InputFailureError,
    },
}

pub fn parse_rendered_verdicts(text: &str) -> Result<Vec<InputFailure>, VerdictParseError> {
    text.split("; ")
        .map(|segment| {
            let Some((subject, code)) = segment.split_once(": ") else {
                return Err(VerdictParseError::Segment {
                    segment: segment.to_string(),
                });
            };
            if code.is_empty() || !code.chars().all(|letter| letter.is_ascii_alphanumeric()) {
                return Err(VerdictParseError::Code {
                    code: code.to_string(),
                });
            }
            let parsed = |raw: &str| {
                raw.parse::<usize>()
                    .map_err(|_| VerdictParseError::Subject {
                        subject: subject.to_string(),
                    })
            };
            let input = if subject == "inputs" {
                None
            } else if let Some(raw) = subject.strip_prefix("input ") {
                Some(parsed(raw)?)
            } else if subject.starts_with("rung ") {
                None
            } else {
                return Err(VerdictParseError::Subject {
                    subject: subject.to_string(),
                });
            };
            let rung = match subject.strip_prefix("rung ") {
                Some(raw) => Some(parsed(raw)?),
                None => None,
            };
            let failure = InputFailure::new(None, None, input, code).map_err(|source| {
                VerdictParseError::Invalid {
                    code: code.to_string(),
                    source,
                }
            })?;
            match rung {
                Some(rung) => Ok(failure.with_rung(rung)),
                None => Ok(failure),
            }
        })
        .collect()
}

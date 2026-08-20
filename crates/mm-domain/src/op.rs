use thiserror::Error;

use crate::caption::CaptionFormat;
use crate::id::{NodeId, NodeIdError};
use crate::jobs::{JobFailure, JobFailureError};
use crate::source::{InputSource, SourceError};
use crate::time::Ticks;

pub const MAX_BATCH_STEPS: usize = 32;
pub const MAX_GRAPH_NODES: usize = 64;
pub const MAX_NESTING_DEPTH: usize = 4;
pub const MAX_TREE_STEPS: usize = 128;
pub const MIN_LADDER_RUNGS: usize = 2;
pub const MAX_LADDER_RUNGS: usize = 6;
pub const MIN_LOUDNESS_LUFS: f64 = -70.0;
pub const MAX_LOUDNESS_LUFS: f64 = 0.0;
pub const MIN_TRUE_PEAK_DB: f64 = -20.0;
pub const MAX_TRUE_PEAK_DB: f64 = 0.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectRef {
    bucket: String,
    key: String,
}

impl ObjectRef {
    pub fn new(bucket: &str, key: &str) -> Result<Self, MediaRefError> {
        if bucket.is_empty() || key.is_empty() {
            return Err(MediaRefError::MalformedObjectUri {
                uri: format!("s3://{bucket}/{key}"),
            });
        }
        Ok(Self {
            bucket: bucket.to_string(),
            key: key.to_string(),
        })
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    pub fn key(&self) -> &str {
        &self.key
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRef {
    url: String,
}

impl HttpRef {
    pub fn url(&self) -> &str {
        &self.url
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaRef {
    Object(ObjectRef),
    Http(HttpRef),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MediaRefError {
    #[error("media ref is missing")]
    Missing,
    #[error("unknown media ref scheme in {value:?}")]
    UnknownScheme { value: String },
    #[error("malformed object uri {uri:?}")]
    MalformedObjectUri { uri: String },
    #[error("malformed http url {url:?}")]
    MalformedHttpUrl { url: String },
}

impl MediaRef {
    pub fn object_uri(uri: &str) -> Result<Self, MediaRefError> {
        let rest = uri
            .strip_prefix("s3://")
            .ok_or_else(|| MediaRefError::UnknownScheme {
                value: uri.to_string(),
            })?;
        let (bucket, key) =
            rest.split_once('/')
                .ok_or_else(|| MediaRefError::MalformedObjectUri {
                    uri: uri.to_string(),
                })?;
        if bucket.is_empty() || key.is_empty() {
            return Err(MediaRefError::MalformedObjectUri {
                uri: uri.to_string(),
            });
        }
        Ok(MediaRef::Object(ObjectRef {
            bucket: bucket.to_string(),
            key: key.to_string(),
        }))
    }

    pub fn http_url(url: &str) -> Result<Self, MediaRefError> {
        let rest = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .ok_or_else(|| MediaRefError::UnknownScheme {
                value: url.to_string(),
            })?;
        let host = rest.split('/').next().unwrap_or("");
        if host.is_empty() {
            return Err(MediaRefError::MalformedHttpUrl {
                url: url.to_string(),
            });
        }
        Ok(MediaRef::Http(HttpRef {
            url: url.to_string(),
        }))
    }

    pub fn to_uri(&self) -> String {
        match self {
            MediaRef::Object(object_ref) => {
                format!("s3://{}/{}", object_ref.bucket, object_ref.key)
            }
            MediaRef::Http(http_ref) => http_ref.url.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputProfile {
    frame_rate: Option<f64>,
    video_max_bitrate: Option<u32>,
    video_quality: Option<u32>,
    audio_bitrate: Option<u32>,
    audio_sample_rate: Option<u32>,
    resolution: Option<(u32, u32)>,
    poster: Option<Ticks>,
    range: Option<OutputRange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputRange {
    start: Ticks,
    duration: Ticks,
}

impl OutputRange {
    pub fn new(start_seconds: f64, duration_seconds: f64) -> Result<Self, OutputProfileError> {
        let start = Ticks::from_seconds(start_seconds).map_err(|_| {
            OutputProfileError::InvalidRangeStart {
                value: start_seconds,
            }
        })?;
        if !duration_seconds.is_finite() || duration_seconds <= 0.0 {
            return Err(OutputProfileError::InvalidRangeDuration {
                value: duration_seconds,
            });
        }
        let duration = Ticks::from_seconds(duration_seconds).map_err(|_| {
            OutputProfileError::InvalidRangeDuration {
                value: duration_seconds,
            }
        })?;
        Self::from_ticks(start, duration).map_err(|_| OutputProfileError::InvalidRangeDuration {
            value: duration_seconds,
        })
    }

    pub fn from_ticks(start: Ticks, duration: Ticks) -> Result<Self, OutputProfileError> {
        if duration.is_zero() {
            return Err(OutputProfileError::InvalidRangeDuration { value: 0.0 });
        }
        Ok(Self { start, duration })
    }

    pub fn start(&self) -> Ticks {
        self.start
    }

    pub fn duration(&self) -> Ticks {
        self.duration
    }

    pub fn start_seconds(&self) -> f64 {
        self.start.seconds()
    }

    pub fn duration_seconds(&self) -> f64 {
        self.duration.seconds()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Error)]
pub enum OutputProfileError {
    #[error("frame rate {value} is not positive and finite")]
    InvalidFrameRate { value: f64 },
    #[error("resolution {width}x{height} sets only one dimension")]
    PartialResolution { width: u32, height: u32 },
    #[error("poster timestamp {value} is negative or not finite")]
    InvalidPoster { value: f64 },
    #[error("range start {value} is negative or not finite")]
    InvalidRangeStart { value: f64 },
    #[error("range duration {value} is not positive and finite")]
    InvalidRangeDuration { value: f64 },
}

fn nonzero(value: u32) -> Option<u32> {
    (value != 0).then_some(value)
}

impl OutputProfile {
    pub fn from_raw(
        frame_rate: f64,
        video_max_bitrate: u32,
        video_quality: u32,
        audio_bitrate: u32,
        audio_sample_rate: u32,
        width: u32,
        height: u32,
    ) -> Result<Self, OutputProfileError> {
        let frame_rate = if frame_rate == 0.0 {
            None
        } else if frame_rate.is_finite() && frame_rate > 0.0 {
            Some(frame_rate)
        } else {
            return Err(OutputProfileError::InvalidFrameRate { value: frame_rate });
        };
        let resolution = match (width, height) {
            (0, 0) => None,
            (width, height) if width > 0 && height > 0 => Some((width, height)),
            (width, height) => {
                return Err(OutputProfileError::PartialResolution { width, height });
            }
        };
        Ok(Self {
            frame_rate,
            video_max_bitrate: nonzero(video_max_bitrate),
            video_quality: nonzero(video_quality),
            audio_bitrate: nonzero(audio_bitrate),
            audio_sample_rate: nonzero(audio_sample_rate),
            resolution,
            poster: None,
            range: None,
        })
    }

    pub fn with_poster(mut self, poster_seconds: f64) -> Result<Self, OutputProfileError> {
        let poster =
            Ticks::from_seconds(poster_seconds).map_err(|_| OutputProfileError::InvalidPoster {
                value: poster_seconds,
            })?;
        self.poster = Some(poster);
        Ok(self)
    }

    pub fn with_poster_ticks(mut self, poster: Ticks) -> Self {
        self.poster = Some(poster);
        self
    }

    pub fn with_range(mut self, range: OutputRange) -> Self {
        self.range = Some(range);
        self
    }

    pub fn poster(&self) -> Option<Ticks> {
        self.poster
    }

    pub fn poster_seconds(&self) -> Option<f64> {
        self.poster.map(Ticks::seconds)
    }

    pub fn range(&self) -> Option<&OutputRange> {
        self.range.as_ref()
    }

    pub fn is_empty(&self) -> bool {
        self.frame_rate.is_none()
            && self.video_max_bitrate.is_none()
            && self.video_quality.is_none()
            && self.audio_bitrate.is_none()
            && self.audio_sample_rate.is_none()
            && self.resolution.is_none()
            && self.poster.is_none()
            && self.range.is_none()
    }

    pub fn frame_rate(&self) -> Option<f64> {
        self.frame_rate
    }

    pub fn video_max_bitrate(&self) -> Option<u32> {
        self.video_max_bitrate
    }

    pub fn video_quality(&self) -> Option<u32> {
        self.video_quality
    }

    pub fn audio_bitrate(&self) -> Option<u32> {
        self.audio_bitrate
    }

    pub fn audio_sample_rate(&self) -> Option<u32> {
        self.audio_sample_rate
    }

    pub fn resolution(&self) -> Option<(u32, u32)> {
        self.resolution
    }
}

#[derive(Debug, Clone, PartialEq, Error)]
pub enum OpSpecError {
    #[error("op spec is missing")]
    MissingOp,
    #[error("concat has no inputs")]
    EmptyInputs,
    #[error("concat needs at least 2 inputs, got {count}")]
    TooFewConcatInputs { count: usize },
    #[error("output profile is empty")]
    EmptyProfile,
    #[error("start {value} is negative or not finite")]
    InvalidStart { value: f64 },
    #[error("duration {value} is not positive and finite")]
    NonPositiveDuration { value: f64 },
    #[error("thumbnail has no offsets")]
    EmptyOffsets,
    #[error("segment length {value} is not positive and finite")]
    InvalidSegmentLength { value: f64 },
    #[error("ladder has {count} rungs, below the 2 a ladder needs")]
    EmptyLadder { count: usize },
    #[error("ladder has {count} rungs, above the {max} allowed")]
    TooManyRungs { count: usize, max: usize },
    #[error("rung {index} names no resolution")]
    RungWithoutResolution { index: usize },
    #[error("rung {index} names no video bitrate")]
    RungWithoutVideoBitrate { index: usize },
    #[error("rung {index} is not larger than the rung below it")]
    NonMonotonicRung { index: usize },
    #[error("rung {index} raises the audio bitrate above the rung below it")]
    RungAudioBitrateRises { index: usize },
    #[error("rung {index} carries its own poster; the ladder carries one")]
    RungCarriesPoster { index: usize },
    #[error("rung {index} carries its own range; the ladder's input carries one")]
    RungCarriesRange { index: usize },
    #[error("offset {value} is negative or not finite")]
    InvalidOffset { value: f64 },
    #[error("integrated loudness {value} is outside {min} to {max} LUFS")]
    InvalidLoudnessTarget { value: f64, min: f64, max: f64 },
    #[error("true peak {value} is outside {min} to {max} dBTP")]
    InvalidTruePeak { value: f64, min: f64, max: f64 },
    #[error("{dimension} is zero")]
    ZeroDimension { dimension: &'static str },
    #[error("{op} takes exactly {expected} inputs, got {got}")]
    WrongInputCount {
        op: &'static str,
        expected: usize,
        got: usize,
    },
    #[error("convert needs an output format")]
    MissingImageFormat,
    #[error("burning or muxing a subtitle needs a caption input")]
    SubtitleCaptionsRequired,
    #[error("stripping a subtitle takes no caption input")]
    SubtitleCaptionsUnexpected,
    #[error("burning or muxing a subtitle needs its text format")]
    SubtitleFormatRequired,
    #[error("brightness {value} is outside -1 to 1")]
    InvalidBrightness { value: f64 },
    #[error("contrast {value} is outside 0 to 2")]
    InvalidContrast { value: f64 },
    #[error("saturation {value} is outside 0 to 3")]
    InvalidSaturation { value: f64 },
    #[error("gamma {value} is outside 0.1 to 10")]
    InvalidGamma { value: f64 },
    #[error("grade has no adjustment and no lookup table")]
    EmptyGrade,
    #[error("title text is empty")]
    EmptyTitle,
    #[error("title text has {length} characters, above the {max} allowed")]
    TitleTooLong { length: usize, max: usize },
    #[error("title {dimension} {value} is not between 1 and {max}")]
    TitleDimension {
        dimension: &'static str,
        value: u32,
        max: u32,
    },
    #[error("title point size {value} is not between 1 and {max}")]
    InvalidPointSize { value: u32, max: u32 },
    #[error("title {field} colour {value:?} is not hex")]
    InvalidColour { field: &'static str, value: String },
    #[error("a jpeg title cannot have a transparent background")]
    TransparentJpeg,
    #[error("audio has no inputs")]
    EmptyAudioInputs,
    #[error("audio has {count} inputs, above the {max} allowed")]
    TooManyAudioInputs { count: usize, max: usize },
    #[error("gain {value} dB is outside -60 to 24 dB")]
    InvalidGain { value: f64 },
    #[error("high-pass {value} Hz is outside 20 to 20000 Hz")]
    InvalidHighPass { value: u32 },
    #[error("low-pass {value} Hz is outside 20 to 20000 Hz")]
    InvalidLowPass { value: u32 },
    #[error("low-pass {low} Hz is not above high-pass {high} Hz")]
    LowPassBelowHighPass { high: u32, low: u32 },
    #[error("limiter ceiling {value} dBTP is outside -20 to 0 dBTP")]
    InvalidLimiter { value: f64 },
    #[error("batch has no steps")]
    EmptyBatch,
    #[error("batch has {count} steps, above the {max} allowed")]
    BatchTooLarge { count: usize, max: usize },
    #[error("composition nests {depth} levels, above the {max} allowed")]
    NestingTooDeep { depth: usize, max: usize },
    #[error("composition holds {count} steps, above the {max} allowed")]
    TreeTooLarge { count: usize, max: usize },
    #[error("graph has no nodes")]
    EmptyGraph,
    #[error("graph has {count} nodes, above the {max} allowed")]
    GraphTooLarge { count: usize, max: usize },
    #[error("graph node id {id:?} is not valid: {source}")]
    InvalidNodeId {
        id: String,
        #[source]
        source: NodeIdError,
    },
    #[error("graph holds two nodes named {id:?}")]
    DuplicateNode { id: String },
    #[error("node {node} references {reference}, which is not in the graph")]
    DanglingReference { node: String, reference: String },
    #[error("node {node} closes a cycle")]
    Cycle { node: String },
    #[error("{op} cannot apply {field} to its input")]
    InputCut {
        op: &'static str,
        field: &'static str,
    },
    #[error(transparent)]
    MediaRef(#[from] MediaRefError),
    #[error(transparent)]
    Profile(#[from] OutputProfileError),
    #[error(transparent)]
    Source(#[from] SourceError),
}

fn reject_cuts(op: &'static str, inputs: &[InputSource]) -> Result<(), OpSpecError> {
    match inputs.iter().find_map(InputSource::cuts) {
        Some(field) => Err(OpSpecError::InputCut { op, field }),
        None => Ok(()),
    }
}

fn sources<I: Into<InputSource>>(inputs: Vec<I>) -> Vec<InputSource> {
    inputs.into_iter().map(Into::into).collect()
}

fn source<I: Into<InputSource>>(input: I) -> InputSource {
    input.into()
}

fn parse_nonnegative(value: f64, error: fn(f64) -> OpSpecError) -> Result<Ticks, OpSpecError> {
    Ticks::from_seconds(value).map_err(|_| error(value))
}

fn parse_positive(value: f64) -> Result<Ticks, OpSpecError> {
    if !(value.is_finite() && value > 0.0) {
        return Err(OpSpecError::NonPositiveDuration { value });
    }
    let ticks =
        Ticks::from_seconds(value).map_err(|_| OpSpecError::NonPositiveDuration { value })?;
    if ticks.is_zero() {
        return Err(OpSpecError::NonPositiveDuration { value });
    }
    Ok(ticks)
}

fn parse_segment(value: f64) -> Result<Ticks, OpSpecError> {
    parse_positive(value).map_err(|_| OpSpecError::InvalidSegmentLength { value })
}

fn parse_poster(value: f64) -> Result<Ticks, OpSpecError> {
    Ticks::from_seconds(value)
        .map_err(|_| OpSpecError::from(OutputProfileError::InvalidPoster { value }))
}

fn parse_profile(profile: Option<OutputProfile>) -> Result<Option<OutputProfile>, OpSpecError> {
    match profile {
        Some(profile) if profile.is_empty() => Err(OpSpecError::EmptyProfile),
        other => Ok(other),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConcatSpec {
    inputs: Vec<InputSource>,
    profile: Option<OutputProfile>,
}

impl ConcatSpec {
    pub fn new<I: Into<InputSource>>(
        inputs: Vec<I>,
        profile: Option<OutputProfile>,
    ) -> Result<Self, OpSpecError> {
        let inputs = sources(inputs);
        if inputs.is_empty() {
            return Err(OpSpecError::EmptyInputs);
        }
        if inputs.len() < 2 {
            return Err(OpSpecError::TooFewConcatInputs {
                count: inputs.len(),
            });
        }
        reject_cuts("concat", &inputs)?;
        Ok(Self {
            inputs,
            profile: parse_profile(profile)?,
        })
    }

    pub fn inputs(&self) -> &[InputSource] {
        &self.inputs
    }

    pub fn profile(&self) -> Option<&OutputProfile> {
        self.profile.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProbeSpec {
    input: InputSource,
}

impl ProbeSpec {
    pub fn new<I: Into<InputSource>>(input: I) -> Result<Self, OpSpecError> {
        let input = source(input);
        reject_cuts("probe", std::slice::from_ref(&input))?;
        Ok(Self { input })
    }

    pub fn input(&self) -> &InputSource {
        &self.input
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TranscodeSpec {
    input: InputSource,
    profile: OutputProfile,
}

impl TranscodeSpec {
    pub fn new<I: Into<InputSource>>(
        input: I,
        profile: OutputProfile,
    ) -> Result<Self, OpSpecError> {
        if profile.is_empty() {
            return Err(OpSpecError::EmptyProfile);
        }
        Ok(Self {
            input: source(input),
            profile,
        })
    }

    pub fn input(&self) -> &InputSource {
        &self.input
    }

    pub fn profile(&self) -> &OutputProfile {
        &self.profile
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrimSpec {
    input: InputSource,
    start: Ticks,
    duration: Ticks,
}

impl TrimSpec {
    pub fn new<I: Into<InputSource>>(
        input: I,
        start_seconds: f64,
        duration_seconds: f64,
    ) -> Result<Self, OpSpecError> {
        let start = parse_nonnegative(start_seconds, |value| OpSpecError::InvalidStart { value })?;
        let duration = parse_positive(duration_seconds)?;
        Self::from_ticks(input, start, duration)
    }

    pub fn from_ticks<I: Into<InputSource>>(
        input: I,
        start: Ticks,
        duration: Ticks,
    ) -> Result<Self, OpSpecError> {
        let input = source(input);
        reject_cuts("trim", std::slice::from_ref(&input))?;
        if duration.is_zero() {
            return Err(OpSpecError::NonPositiveDuration { value: 0.0 });
        }
        Ok(Self {
            input,
            start,
            duration,
        })
    }

    pub fn input(&self) -> &InputSource {
        &self.input
    }

    pub fn start(&self) -> Ticks {
        self.start
    }

    pub fn duration(&self) -> Ticks {
        self.duration
    }

    pub fn start_seconds(&self) -> f64 {
        self.start.seconds()
    }

    pub fn duration_seconds(&self) -> f64 {
        self.duration.seconds()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ThumbnailSpec {
    input: InputSource,
    offsets: Vec<Ticks>,
    width: u32,
    height: u32,
    sprite_columns: u32,
}

impl ThumbnailSpec {
    pub fn new<I: Into<InputSource>>(
        input: I,
        offsets_seconds: Vec<f64>,
        width: u32,
        height: u32,
    ) -> Result<Self, OpSpecError> {
        let offsets = offsets_seconds
            .into_iter()
            .map(|offset| parse_nonnegative(offset, |value| OpSpecError::InvalidOffset { value }))
            .collect::<Result<Vec<Ticks>, OpSpecError>>()?;
        Self::from_ticks(input, offsets, width, height)
    }

    pub fn from_ticks<I: Into<InputSource>>(
        input: I,
        offsets: Vec<Ticks>,
        width: u32,
        height: u32,
    ) -> Result<Self, OpSpecError> {
        let input = source(input);
        reject_cuts("thumbnail", std::slice::from_ref(&input))?;
        if offsets.is_empty() {
            return Err(OpSpecError::EmptyOffsets);
        }
        if width == 0 {
            return Err(OpSpecError::ZeroDimension { dimension: "width" });
        }
        if height == 0 {
            return Err(OpSpecError::ZeroDimension {
                dimension: "height",
            });
        }
        Ok(Self {
            input,
            offsets,
            width,
            height,
            sprite_columns: 0,
        })
    }

    pub fn with_sprite(mut self, sprite_columns: u32) -> Self {
        self.sprite_columns = sprite_columns;
        self
    }

    pub fn input(&self) -> &InputSource {
        &self.input
    }

    pub fn offsets(&self) -> &[Ticks] {
        &self.offsets
    }

    pub fn offsets_seconds(&self) -> Vec<f64> {
        self.offsets.iter().map(|offset| offset.seconds()).collect()
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn sprite_columns(&self) -> u32 {
        self.sprite_columns
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ImageOp {
    #[error("resize")]
    Resize,
    #[error("convert")]
    Convert,
    #[error("composite")]
    Composite,
}

impl ImageOp {
    pub fn kind(self) -> OpKind {
        match self {
            ImageOp::Resize => OpKind::Resize,
            ImageOp::Convert => OpKind::Convert,
            ImageOp::Composite => OpKind::Composite,
        }
    }

    pub fn input_count(self) -> usize {
        match self {
            ImageOp::Resize | ImageOp::Convert => 1,
            ImageOp::Composite => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeMode {
    Fit,
    Exact,
    Fill,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Jpeg,
    Png,
}

impl ImageFormat {
    pub fn extension(self) -> &'static str {
        match self {
            ImageFormat::Jpeg => "jpg",
            ImageFormat::Png => "png",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "jpeg" | "jpg" => Some(ImageFormat::Jpeg),
            "png" => Some(ImageFormat::Png),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImageSpec {
    inputs: Vec<InputSource>,
    op: ImageOp,
    width: u32,
    height: u32,
    mode: ResizeMode,
    format: Option<ImageFormat>,
}

impl ImageSpec {
    pub fn new<I: Into<InputSource>>(
        inputs: Vec<I>,
        op: ImageOp,
        width: u32,
        height: u32,
        mode: ResizeMode,
        format: Option<ImageFormat>,
    ) -> Result<Self, OpSpecError> {
        let inputs = sources(inputs);
        reject_cuts(op.to_string().leak(), &inputs)?;
        if inputs.len() != op.input_count() {
            return Err(OpSpecError::WrongInputCount {
                op: op.to_string().leak(),
                expected: op.input_count(),
                got: inputs.len(),
            });
        }
        if op == ImageOp::Resize && (width == 0 || height == 0) {
            return Err(OpSpecError::ZeroDimension {
                dimension: if width == 0 { "width" } else { "height" },
            });
        }
        if op == ImageOp::Convert && format.is_none() {
            return Err(OpSpecError::MissingImageFormat);
        }
        Ok(Self {
            inputs,
            op,
            width,
            height,
            mode,
            format,
        })
    }

    pub fn inputs(&self) -> &[InputSource] {
        &self.inputs
    }

    pub fn op(&self) -> ImageOp {
        self.op
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn mode(&self) -> ResizeMode {
        self.mode
    }

    pub fn format(&self) -> ImageFormat {
        self.format.unwrap_or(ImageFormat::Png)
    }

    pub fn declared_format(&self) -> Option<ImageFormat> {
        self.format
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PackageSpec {
    input: InputSource,
    segment: Ticks,
}

impl PackageSpec {
    pub fn new<I: Into<InputSource>>(input: I, segment_seconds: f64) -> Result<Self, OpSpecError> {
        let segment = parse_segment(segment_seconds)?;
        Self::from_ticks(input, segment)
    }

    pub fn from_ticks<I: Into<InputSource>>(input: I, segment: Ticks) -> Result<Self, OpSpecError> {
        let input = source(input);
        reject_cuts("package", std::slice::from_ref(&input))?;
        if segment.is_zero() {
            return Err(OpSpecError::InvalidSegmentLength { value: 0.0 });
        }
        Ok(Self { input, segment })
    }

    pub fn input(&self) -> &InputSource {
        &self.input
    }

    pub fn segment(&self) -> Ticks {
        self.segment
    }

    pub fn segment_seconds(&self) -> f64 {
        self.segment.seconds()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioSpec {
    inputs: Vec<InputSource>,
    gain_db: f64,
    fade_in: Ticks,
    fade_out: Ticks,
    high_pass_hz: Option<u32>,
    low_pass_hz: Option<u32>,
    limiter_true_peak_db: Option<f64>,
}

impl AudioSpec {
    pub const MIN_GAIN_DB: f64 = -60.0;
    pub const MAX_GAIN_DB: f64 = 24.0;
    pub const MIN_FILTER_HZ: u32 = 20;
    pub const MAX_FILTER_HZ: u32 = 20_000;
    pub const MIN_LIMITER_DB: f64 = -20.0;
    pub const MAX_LIMITER_DB: f64 = 0.0;
    pub const MAX_INPUTS: usize = 8;

    pub fn new<I: Into<InputSource>>(
        inputs: Vec<I>,
        gain_db: f64,
        fade_in: Ticks,
        fade_out: Ticks,
        high_pass_hz: Option<u32>,
        low_pass_hz: Option<u32>,
        limiter_true_peak_db: Option<f64>,
    ) -> Result<Self, OpSpecError> {
        if inputs.is_empty() {
            return Err(OpSpecError::EmptyAudioInputs);
        }
        if inputs.len() > Self::MAX_INPUTS {
            return Err(OpSpecError::TooManyAudioInputs {
                count: inputs.len(),
                max: Self::MAX_INPUTS,
            });
        }
        if !gain_db.is_finite() || !(Self::MIN_GAIN_DB..=Self::MAX_GAIN_DB).contains(&gain_db) {
            return Err(OpSpecError::InvalidGain { value: gain_db });
        }
        if let Some(hz) = high_pass_hz
            && !(Self::MIN_FILTER_HZ..=Self::MAX_FILTER_HZ).contains(&hz)
        {
            return Err(OpSpecError::InvalidHighPass { value: hz });
        }
        if let Some(hz) = low_pass_hz
            && !(Self::MIN_FILTER_HZ..=Self::MAX_FILTER_HZ).contains(&hz)
        {
            return Err(OpSpecError::InvalidLowPass { value: hz });
        }
        if let (Some(high), Some(low)) = (high_pass_hz, low_pass_hz)
            && low <= high
        {
            return Err(OpSpecError::LowPassBelowHighPass { high, low });
        }
        if let Some(ceiling) = limiter_true_peak_db
            && (!ceiling.is_finite()
                || !(Self::MIN_LIMITER_DB..=Self::MAX_LIMITER_DB).contains(&ceiling))
        {
            return Err(OpSpecError::InvalidLimiter { value: ceiling });
        }
        let inputs = sources(inputs);
        reject_cuts("audio", &inputs)?;
        Ok(Self {
            inputs,
            gain_db,
            fade_in,
            fade_out,
            high_pass_hz,
            low_pass_hz,
            limiter_true_peak_db,
        })
    }

    pub fn inputs(&self) -> &[InputSource] {
        &self.inputs
    }

    pub fn gain_db(&self) -> f64 {
        self.gain_db
    }

    pub fn fade_in(&self) -> Ticks {
        self.fade_in
    }

    pub fn fade_out(&self) -> Ticks {
        self.fade_out
    }

    pub fn high_pass_hz(&self) -> Option<u32> {
        self.high_pass_hz
    }

    pub fn low_pass_hz(&self) -> Option<u32> {
        self.low_pass_hz
    }

    pub fn limiter_true_peak_db(&self) -> Option<f64> {
        self.limiter_true_peak_db
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colour {
    red: u8,
    green: u8,
    blue: u8,
    alpha: u8,
}

impl Colour {
    pub fn parse(field: &'static str, value: &str) -> Result<Self, OpSpecError> {
        let text = value.strip_prefix('#').unwrap_or(value);
        let digits = |slice: &str| u8::from_str_radix(slice, 16).ok();
        let (red, green, blue, alpha) = match text.len() {
            3 => {
                let expand = |slice: &str| digits(slice).map(|value| value * 16 + value);
                (
                    expand(&text[0..1]),
                    expand(&text[1..2]),
                    expand(&text[2..3]),
                    Some(255),
                )
            }
            6 => (
                digits(&text[0..2]),
                digits(&text[2..4]),
                digits(&text[4..6]),
                Some(255),
            ),
            8 => (
                digits(&text[0..2]),
                digits(&text[2..4]),
                digits(&text[4..6]),
                digits(&text[6..8]),
            ),
            _ => (None, None, None, None),
        };
        match (red, green, blue, alpha) {
            (Some(red), Some(green), Some(blue), Some(alpha)) => Ok(Self {
                red,
                green,
                blue,
                alpha,
            }),
            _ => Err(OpSpecError::InvalidColour {
                field,
                value: value.to_string(),
            }),
        }
    }

    pub fn hex(self) -> String {
        if self.alpha == 255 {
            format!("#{:02x}{:02x}{:02x}", self.red, self.green, self.blue)
        } else {
            format!(
                "#{:02x}{:02x}{:02x}{:02x}",
                self.red, self.green, self.blue, self.alpha
            )
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TitleSpec {
    text: String,
    width: u32,
    height: u32,
    point_size: u32,
    colour: Colour,
    background: Option<Colour>,
    format: ImageFormat,
}

impl TitleSpec {
    pub const MAX_TEXT_CHARS: usize = 1024;
    pub const MAX_POINT_SIZE: u32 = 512;
    pub const MAX_DIMENSION: u32 = 8192;

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        text: impl Into<String>,
        width: u32,
        height: u32,
        point_size: u32,
        colour: Colour,
        background: Option<Colour>,
        format: ImageFormat,
    ) -> Result<Self, OpSpecError> {
        let text = text.into();
        if text.trim().is_empty() {
            return Err(OpSpecError::EmptyTitle);
        }
        if text.chars().count() > Self::MAX_TEXT_CHARS {
            return Err(OpSpecError::TitleTooLong {
                length: text.chars().count(),
                max: Self::MAX_TEXT_CHARS,
            });
        }
        for (dimension, value) in [("width", width), ("height", height)] {
            if value == 0 || value > Self::MAX_DIMENSION {
                return Err(OpSpecError::TitleDimension {
                    dimension,
                    value,
                    max: Self::MAX_DIMENSION,
                });
            }
        }
        if point_size == 0 || point_size > Self::MAX_POINT_SIZE {
            return Err(OpSpecError::InvalidPointSize {
                value: point_size,
                max: Self::MAX_POINT_SIZE,
            });
        }
        if format == ImageFormat::Jpeg && background.is_none() {
            return Err(OpSpecError::TransparentJpeg);
        }
        Ok(Self {
            text,
            width,
            height,
            point_size,
            colour,
            background,
            format,
        })
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn point_size(&self) -> u32 {
        self.point_size
    }

    pub fn colour(&self) -> Colour {
        self.colour
    }

    pub fn background(&self) -> Option<Colour> {
        self.background
    }

    pub fn format(&self) -> ImageFormat {
        self.format
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GradeAdjustments {
    brightness: f64,
    contrast: f64,
    saturation: f64,
    gamma: f64,
}

impl Default for GradeAdjustments {
    fn default() -> Self {
        Self {
            brightness: 0.0,
            contrast: 1.0,
            saturation: 1.0,
            gamma: 1.0,
        }
    }
}

impl GradeAdjustments {
    pub const MIN_BRIGHTNESS: f64 = -1.0;
    pub const MAX_BRIGHTNESS: f64 = 1.0;
    pub const MIN_CONTRAST: f64 = 0.0;
    pub const MAX_CONTRAST: f64 = 2.0;
    pub const MIN_SATURATION: f64 = 0.0;
    pub const MAX_SATURATION: f64 = 3.0;
    pub const MIN_GAMMA: f64 = 0.1;
    pub const MAX_GAMMA: f64 = 10.0;

    pub fn new(
        brightness: f64,
        contrast: f64,
        saturation: f64,
        gamma: f64,
    ) -> Result<Self, OpSpecError> {
        if !brightness.is_finite()
            || !(Self::MIN_BRIGHTNESS..=Self::MAX_BRIGHTNESS).contains(&brightness)
        {
            return Err(OpSpecError::InvalidBrightness { value: brightness });
        }
        if !contrast.is_finite() || !(Self::MIN_CONTRAST..=Self::MAX_CONTRAST).contains(&contrast) {
            return Err(OpSpecError::InvalidContrast { value: contrast });
        }
        if !saturation.is_finite()
            || !(Self::MIN_SATURATION..=Self::MAX_SATURATION).contains(&saturation)
        {
            return Err(OpSpecError::InvalidSaturation { value: saturation });
        }
        if !gamma.is_finite() || !(Self::MIN_GAMMA..=Self::MAX_GAMMA).contains(&gamma) {
            return Err(OpSpecError::InvalidGamma { value: gamma });
        }
        Ok(Self {
            brightness,
            contrast,
            saturation,
            gamma,
        })
    }

    pub fn brightness(&self) -> f64 {
        self.brightness
    }

    pub fn contrast(&self) -> f64 {
        self.contrast
    }

    pub fn saturation(&self) -> f64 {
        self.saturation
    }

    pub fn gamma(&self) -> f64 {
        self.gamma
    }

    pub fn is_identity(&self) -> bool {
        self.brightness == 0.0
            && self.contrast == 1.0
            && self.saturation == 1.0
            && self.gamma == 1.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GradeScope {
    width: u32,
    height: u32,
}

impl GradeScope {
    pub fn new(width: u32, height: u32) -> Result<Self, OpSpecError> {
        if width == 0 {
            return Err(OpSpecError::ZeroDimension { dimension: "width" });
        }
        if height == 0 {
            return Err(OpSpecError::ZeroDimension {
                dimension: "height",
            });
        }
        Ok(Self { width, height })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GradeSpec {
    input: InputSource,
    adjustments: GradeAdjustments,
    lookup_table: Option<InputSource>,
    scope: Option<GradeScope>,
}

impl GradeSpec {
    pub fn new<I: Into<InputSource>>(
        input: I,
        adjustments: GradeAdjustments,
        lookup_table: Option<InputSource>,
        scope: Option<GradeScope>,
    ) -> Result<Self, OpSpecError> {
        let input = source(input);
        reject_cuts("grade", std::slice::from_ref(&input))?;
        if let Some(lookup_table) = &lookup_table {
            reject_cuts("grade", std::slice::from_ref(lookup_table))?;
        }
        if adjustments.is_identity() && lookup_table.is_none() {
            return Err(OpSpecError::EmptyGrade);
        }
        Ok(Self {
            input,
            adjustments,
            lookup_table,
            scope,
        })
    }

    pub fn input(&self) -> &InputSource {
        &self.input
    }

    pub fn adjustments(&self) -> GradeAdjustments {
        self.adjustments
    }

    pub fn lookup_table(&self) -> Option<&InputSource> {
        self.lookup_table.as_ref()
    }

    pub fn scope(&self) -> Option<GradeScope> {
        self.scope
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleMode {
    Burn,
    Mux,
    Strip,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SubtitleSpec {
    input: InputSource,
    captions: Option<InputSource>,
    format: Option<CaptionFormat>,
    mode: SubtitleMode,
}

impl SubtitleSpec {
    pub fn new<I: Into<InputSource>>(
        input: I,
        captions: Option<InputSource>,
        format: Option<CaptionFormat>,
        mode: SubtitleMode,
    ) -> Result<Self, OpSpecError> {
        let input = source(input);
        reject_cuts("subtitle", std::slice::from_ref(&input))?;
        if let Some(captions) = &captions {
            reject_cuts("subtitle", std::slice::from_ref(captions))?;
        }
        match mode {
            SubtitleMode::Burn | SubtitleMode::Mux if captions.is_none() => {
                return Err(OpSpecError::SubtitleCaptionsRequired);
            }
            SubtitleMode::Strip if captions.is_some() => {
                return Err(OpSpecError::SubtitleCaptionsUnexpected);
            }
            _ => {}
        }
        if matches!(mode, SubtitleMode::Burn | SubtitleMode::Mux) && format.is_none() {
            return Err(OpSpecError::SubtitleFormatRequired);
        }
        Ok(Self {
            input,
            captions,
            format,
            mode,
        })
    }

    pub fn input(&self) -> &InputSource {
        &self.input
    }

    pub fn captions(&self) -> Option<&InputSource> {
        self.captions.as_ref()
    }

    pub fn format(&self) -> Option<CaptionFormat> {
        self.format
    }

    pub fn mode(&self) -> SubtitleMode {
        self.mode
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoudnessTarget {
    integrated_lufs: f64,
    true_peak_db: f64,
}

impl LoudnessTarget {
    pub fn new(integrated_lufs: f64, true_peak_db: f64) -> Result<Self, OpSpecError> {
        if !integrated_lufs.is_finite()
            || !(MIN_LOUDNESS_LUFS..=MAX_LOUDNESS_LUFS).contains(&integrated_lufs)
        {
            return Err(OpSpecError::InvalidLoudnessTarget {
                value: integrated_lufs,
                min: MIN_LOUDNESS_LUFS,
                max: MAX_LOUDNESS_LUFS,
            });
        }
        if !true_peak_db.is_finite()
            || !(MIN_TRUE_PEAK_DB..=MAX_TRUE_PEAK_DB).contains(&true_peak_db)
        {
            return Err(OpSpecError::InvalidTruePeak {
                value: true_peak_db,
                min: MIN_TRUE_PEAK_DB,
                max: MAX_TRUE_PEAK_DB,
            });
        }
        Ok(Self {
            integrated_lufs,
            true_peak_db,
        })
    }

    pub fn integrated_lufs(&self) -> f64 {
        self.integrated_lufs
    }

    pub fn true_peak_db(&self) -> f64 {
        self.true_peak_db
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoudnessSpec {
    input: InputSource,
    target: Option<LoudnessTarget>,
}

impl LoudnessSpec {
    pub fn new<I: Into<InputSource>>(
        input: I,
        target: Option<LoudnessTarget>,
    ) -> Result<Self, OpSpecError> {
        let input = source(input);
        reject_cuts("loudness", std::slice::from_ref(&input))?;
        Ok(Self { input, target })
    }

    pub fn input(&self) -> &InputSource {
        &self.input
    }

    pub fn target(&self) -> Option<LoudnessTarget> {
        self.target
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LadderSpec {
    input: InputSource,
    rungs: Vec<OutputProfile>,
    segment: Option<Ticks>,
    poster: Option<Ticks>,
}

impl LadderSpec {
    pub fn new<I: Into<InputSource>>(
        input: I,
        rungs: Vec<OutputProfile>,
    ) -> Result<Self, OpSpecError> {
        if rungs.len() < MIN_LADDER_RUNGS {
            return Err(OpSpecError::EmptyLadder { count: rungs.len() });
        }
        if rungs.len() > MAX_LADDER_RUNGS {
            return Err(OpSpecError::TooManyRungs {
                count: rungs.len(),
                max: MAX_LADDER_RUNGS,
            });
        }
        for (index, rung) in rungs.iter().enumerate() {
            if rung.is_empty() {
                return Err(OpSpecError::EmptyProfile);
            }
            if rung.resolution().is_none() {
                return Err(OpSpecError::RungWithoutResolution { index });
            }
            if rung.video_max_bitrate().is_none() {
                return Err(OpSpecError::RungWithoutVideoBitrate { index });
            }
            if rung.poster_seconds().is_some() {
                return Err(OpSpecError::RungCarriesPoster { index });
            }
            if rung.range().is_some() {
                return Err(OpSpecError::RungCarriesRange { index });
            }
        }
        for index in 1..rungs.len() {
            let lower = &rungs[index - 1];
            let upper = &rungs[index];
            let (lower_width, lower_height) = lower.resolution().unwrap_or((0, 0));
            let (upper_width, upper_height) = upper.resolution().unwrap_or((0, 0));
            let lower_area = u64::from(lower_width) * u64::from(lower_height);
            let upper_area = u64::from(upper_width) * u64::from(upper_height);
            if upper_area <= lower_area {
                return Err(OpSpecError::NonMonotonicRung { index });
            }
            if upper.video_max_bitrate() <= lower.video_max_bitrate() {
                return Err(OpSpecError::NonMonotonicRung { index });
            }
            if upper.audio_bitrate() > lower.audio_bitrate() {
                return Err(OpSpecError::RungAudioBitrateRises { index });
            }
        }
        Ok(Self {
            input: source(input),
            rungs,
            segment: None,
            poster: None,
        })
    }

    pub fn with_segment_seconds(self, segment_seconds: f64) -> Result<Self, OpSpecError> {
        let segment = parse_segment(segment_seconds)?;
        self.with_segment(segment)
    }

    pub fn with_segment(mut self, segment: Ticks) -> Result<Self, OpSpecError> {
        if segment.is_zero() {
            return Err(OpSpecError::InvalidSegmentLength { value: 0.0 });
        }
        self.segment = Some(segment);
        Ok(self)
    }

    pub fn with_poster(mut self, poster_seconds: f64) -> Result<Self, OpSpecError> {
        self.poster = Some(parse_poster(poster_seconds)?);
        Ok(self)
    }

    pub fn with_poster_ticks(mut self, poster: Ticks) -> Self {
        self.poster = Some(poster);
        self
    }

    pub fn input(&self) -> &InputSource {
        &self.input
    }

    pub fn rungs(&self) -> &[OutputProfile] {
        &self.rungs
    }

    pub fn segment(&self) -> Option<Ticks> {
        self.segment
    }

    pub fn poster(&self) -> Option<Ticks> {
        self.poster
    }

    pub fn segment_seconds(&self) -> Option<f64> {
        self.segment.map(Ticks::seconds)
    }

    pub fn poster_seconds(&self) -> Option<f64> {
        self.poster.map(Ticks::seconds)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StepOutcome {
    position: u32,
    state: StepState,
}

impl StepOutcome {
    pub fn new(position: u32, state: StepState) -> Self {
        Self { position, state }
    }

    pub fn position(&self) -> u32 {
        self.position
    }

    pub fn state(&self) -> &StepState {
        &self.state
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum StepState {
    Result(OpResult),
    Failure(JobFailure),
}

#[derive(Debug, Clone, PartialEq)]
pub struct BatchResult {
    steps: Vec<StepOutcome>,
}

impl BatchResult {
    pub fn new(steps: Vec<StepOutcome>) -> Result<Self, OpResultError> {
        if steps.is_empty() {
            return Err(OpResultError::EmptyBatchResult);
        }
        Ok(Self { steps })
    }

    pub fn steps(&self) -> &[StepOutcome] {
        &self.steps
    }
}

fn tree_units(spec: &OpSpec) -> usize {
    match spec {
        OpSpec::Batch(batch) => batch.steps().iter().map(tree_units).sum(),
        OpSpec::Graph(graph) => graph
            .nodes()
            .iter()
            .map(|node| tree_units(node.spec()))
            .sum(),
        _ => 1,
    }
}

fn tree_depth(spec: &OpSpec) -> usize {
    match spec {
        OpSpec::Batch(batch) => 1 + batch.steps().iter().map(tree_depth).max().unwrap_or(0),
        OpSpec::Graph(graph) => {
            1 + graph
                .nodes()
                .iter()
                .map(|node| tree_depth(node.spec()))
                .max()
                .unwrap_or(0)
        }
        _ => 0,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GraphNode {
    id: NodeId,
    spec: OpSpec,
}

impl GraphNode {
    pub fn new(id: &str, spec: OpSpec) -> Result<Self, OpSpecError> {
        let id = id
            .parse::<NodeId>()
            .map_err(|source| OpSpecError::InvalidNodeId {
                id: id.to_string(),
                source,
            })?;
        Ok(Self { id, spec })
    }

    pub fn id(&self) -> &NodeId {
        &self.id
    }

    pub fn spec(&self) -> &OpSpec {
        &self.spec
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GraphSpec {
    nodes: Vec<GraphNode>,
}

impl GraphSpec {
    pub fn new(nodes: Vec<GraphNode>) -> Result<Self, OpSpecError> {
        if nodes.is_empty() {
            return Err(OpSpecError::EmptyGraph);
        }
        if nodes.len() > MAX_GRAPH_NODES {
            return Err(OpSpecError::GraphTooLarge {
                count: nodes.len(),
                max: MAX_GRAPH_NODES,
            });
        }
        for (index, node) in nodes.iter().enumerate() {
            if nodes[index + 1..]
                .iter()
                .any(|other| other.id.as_str() == node.id.as_str())
            {
                return Err(OpSpecError::DuplicateNode {
                    id: node.id.as_str().to_string(),
                });
            }
        }
        let count: usize = nodes.iter().map(|node| tree_units(node.spec())).sum();
        if count > MAX_TREE_STEPS {
            return Err(OpSpecError::TreeTooLarge {
                count,
                max: MAX_TREE_STEPS,
            });
        }
        let depth = 1 + nodes
            .iter()
            .map(|node| tree_depth(node.spec()))
            .max()
            .unwrap_or(0);
        if depth > MAX_NESTING_DEPTH {
            return Err(OpSpecError::NestingTooDeep {
                depth,
                max: MAX_NESTING_DEPTH,
            });
        }
        let graph = Self { nodes };
        graph.check_graph()?;
        Ok(graph)
    }

    pub fn nodes(&self) -> &[GraphNode] {
        &self.nodes
    }

    pub fn ensure_resolvable(&self) -> Result<(), UnsupportedInput> {
        Ok(())
    }

    fn check_graph(&self) -> Result<(), OpSpecError> {
        let mut edges: Vec<Vec<usize>> = vec![Vec::new(); self.nodes.len()];
        for (index, node) in self.nodes.iter().enumerate() {
            match node.spec() {
                OpSpec::Batch(batch) => {
                    batch
                        .ensure_resolvable()
                        .map_err(|error| OpSpecError::DanglingReference {
                            node: node.id().as_str().to_string(),
                            reference: error.node,
                        })?
                }
                OpSpec::Graph(nested) => nested.check_graph()?,
                leaf => {
                    for reference in leaf.inputs().into_iter().filter_map(InputSource::node) {
                        let Some(target) = self
                            .nodes
                            .iter()
                            .position(|other| other.id.as_str() == reference.id().as_str())
                        else {
                            return Err(OpSpecError::DanglingReference {
                                node: node.id().as_str().to_string(),
                                reference: reference.id().as_str().to_string(),
                            });
                        };
                        edges[index].push(target);
                    }
                }
            }
        }
        let mut colour = vec![0u8; self.nodes.len()];
        for start in 0..self.nodes.len() {
            if colour[start] == 0 {
                self.visit(start, &edges, &mut colour)?;
            }
        }
        Ok(())
    }

    fn visit(
        &self,
        node: usize,
        edges: &[Vec<usize>],
        colour: &mut [u8],
    ) -> Result<(), OpSpecError> {
        colour[node] = 1;
        for &next in &edges[node] {
            match colour[next] {
                1 => {
                    return Err(OpSpecError::Cycle {
                        node: self.nodes[node].id().as_str().to_string(),
                    });
                }
                0 => self.visit(next, edges, colour)?,
                _ => {}
            }
        }
        colour[node] = 2;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodeOutcome {
    id: NodeId,
    state: StepState,
}

impl NodeOutcome {
    pub fn new(id: NodeId, state: StepState) -> Self {
        Self { id, state }
    }

    pub fn id(&self) -> &NodeId {
        &self.id
    }

    pub fn state(&self) -> &StepState {
        &self.state
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GraphResult {
    nodes: Vec<NodeOutcome>,
}

impl GraphResult {
    pub fn new(nodes: Vec<NodeOutcome>) -> Result<Self, OpResultError> {
        if nodes.is_empty() {
            return Err(OpResultError::EmptyGraphResult);
        }
        Ok(Self { nodes })
    }

    pub fn nodes(&self) -> &[NodeOutcome] {
        &self.nodes
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BatchSpec {
    steps: Vec<OpSpec>,
    stop_on_failure: bool,
}

impl BatchSpec {
    pub fn new(steps: Vec<OpSpec>, stop_on_failure: bool) -> Result<Self, OpSpecError> {
        if steps.is_empty() {
            return Err(OpSpecError::EmptyBatch);
        }
        if steps.len() > MAX_BATCH_STEPS {
            return Err(OpSpecError::BatchTooLarge {
                count: steps.len(),
                max: MAX_BATCH_STEPS,
            });
        }
        let count: usize = steps.iter().map(tree_units).sum();
        if count > MAX_TREE_STEPS {
            return Err(OpSpecError::TreeTooLarge {
                count,
                max: MAX_TREE_STEPS,
            });
        }
        let depth = 1 + steps.iter().map(tree_depth).max().unwrap_or(0);
        if depth > MAX_NESTING_DEPTH {
            return Err(OpSpecError::NestingTooDeep {
                depth,
                max: MAX_NESTING_DEPTH,
            });
        }
        Ok(Self {
            steps,
            stop_on_failure,
        })
    }

    pub fn steps(&self) -> &[OpSpec] {
        &self.steps
    }

    pub fn stop_on_failure(&self) -> bool {
        self.stop_on_failure
    }

    pub fn ensure_resolvable(&self) -> Result<(), UnsupportedInput> {
        for (position, step) in self.steps.iter().enumerate() {
            match step {
                OpSpec::Batch(nested) => nested.ensure_resolvable()?,
                OpSpec::Graph(nested) => nested.ensure_resolvable()?,
                leaf => {
                    for node in leaf.inputs().into_iter().filter_map(InputSource::node) {
                        let resolves = node
                            .id()
                            .as_str()
                            .parse::<usize>()
                            .map(|earlier| earlier < position)
                            .unwrap_or(false);
                        if !resolves {
                            return Err(UnsupportedInput {
                                node: node.id().as_str().to_string(),
                            });
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum OpSpec {
    Concat(ConcatSpec),
    Probe(ProbeSpec),
    Transcode(TranscodeSpec),
    Trim(TrimSpec),
    Thumbnail(ThumbnailSpec),
    Image(ImageSpec),
    Package(PackageSpec),
    Batch(BatchSpec),
    Graph(GraphSpec),
    Ladder(LadderSpec),
    Loudness(LoudnessSpec),
    Subtitle(SubtitleSpec),
    Grade(GradeSpec),
    Audio(AudioSpec),
    Title(TitleSpec),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpKind {
    Concat,
    Probe,
    Transcode,
    Trim,
    Thumbnail,
    Resize,
    Convert,
    Composite,
    Package,
    Batch,
    Graph,
    Ladder,
    Loudness,
    Subtitle,
    Grade,
    Audio,
    Title,
}

impl std::fmt::Display for OpKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            OpKind::Concat => "concat",
            OpKind::Probe => "probe",
            OpKind::Transcode => "transcode",
            OpKind::Trim => "trim",
            OpKind::Thumbnail => "thumbnail",
            OpKind::Resize => "resize",
            OpKind::Convert => "convert",
            OpKind::Composite => "composite",
            OpKind::Package => "package",
            OpKind::Batch => "batch",
            OpKind::Graph => "graph",
            OpKind::Ladder => "ladder",
            OpKind::Loudness => "loudness",
            OpKind::Subtitle => "subtitle",
            OpKind::Grade => "grade",
            OpKind::Audio => "audio",
            OpKind::Title => "title",
        };
        formatter.write_str(name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("{kind} is not supported yet")]
pub struct UnsupportedOp {
    pub kind: OpKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("input references node {node}, which has no producer yet")]
pub struct UnsupportedInput {
    pub node: String,
}

impl OpKind {
    pub fn ensure_executable(self) -> Result<(), UnsupportedOp> {
        match self {
            OpKind::Concat
            | OpKind::Probe
            | OpKind::Transcode
            | OpKind::Trim
            | OpKind::Thumbnail
            | OpKind::Resize
            | OpKind::Convert
            | OpKind::Composite
            | OpKind::Package
            | OpKind::Batch
            | OpKind::Graph
            | OpKind::Ladder
            | OpKind::Loudness
            | OpKind::Subtitle
            | OpKind::Grade
            | OpKind::Audio
            | OpKind::Title => Ok(()),
        }
    }
}

impl OpSpec {
    pub fn kind(&self) -> OpKind {
        match self {
            OpSpec::Concat(_) => OpKind::Concat,
            OpSpec::Probe(_) => OpKind::Probe,
            OpSpec::Transcode(_) => OpKind::Transcode,
            OpSpec::Trim(_) => OpKind::Trim,
            OpSpec::Thumbnail(_) => OpKind::Thumbnail,
            OpSpec::Image(spec) => spec.op.kind(),
            OpSpec::Package(_) => OpKind::Package,
            OpSpec::Batch(_) => OpKind::Batch,
            OpSpec::Graph(_) => OpKind::Graph,
            OpSpec::Ladder(_) => OpKind::Ladder,
            OpSpec::Loudness(_) => OpKind::Loudness,
            OpSpec::Subtitle(_) => OpKind::Subtitle,
            OpSpec::Grade(_) => OpKind::Grade,
            OpSpec::Audio(_) => OpKind::Audio,
            OpSpec::Title(_) => OpKind::Title,
        }
    }

    pub fn inputs(&self) -> Vec<&InputSource> {
        match self {
            OpSpec::Concat(concat) => concat.inputs().iter().collect(),
            OpSpec::Probe(probe) => vec![probe.input()],
            OpSpec::Transcode(transcode) => vec![transcode.input()],
            OpSpec::Trim(trim) => vec![trim.input()],
            OpSpec::Thumbnail(thumbnail) => vec![thumbnail.input()],
            OpSpec::Image(image) => image.inputs().iter().collect(),
            OpSpec::Package(package) => vec![package.input()],
            OpSpec::Batch(batch) => batch.steps().iter().flat_map(OpSpec::inputs).collect(),
            OpSpec::Graph(graph) => graph
                .nodes()
                .iter()
                .flat_map(|node| node.spec().inputs())
                .collect(),
            OpSpec::Ladder(ladder) => vec![ladder.input()],
            OpSpec::Loudness(loudness) => vec![loudness.input()],
            OpSpec::Subtitle(subtitle) => {
                let mut inputs = vec![subtitle.input()];
                if let Some(captions) = subtitle.captions() {
                    inputs.push(captions);
                }
                inputs
            }
            OpSpec::Grade(grade) => {
                let mut inputs = vec![grade.input()];
                if let Some(lookup_table) = grade.lookup_table() {
                    inputs.push(lookup_table);
                }
                inputs
            }
            OpSpec::Audio(audio) => audio.inputs().iter().collect(),
            OpSpec::Title(_) => Vec::new(),
        }
    }

    pub fn ensure_resolvable(&self) -> Result<(), UnsupportedInput> {
        match self {
            OpSpec::Batch(batch) => batch.ensure_resolvable(),
            OpSpec::Graph(graph) => graph.ensure_resolvable(),
            _ => match self.inputs().into_iter().find_map(InputSource::node) {
                Some(node) => Err(UnsupportedInput {
                    node: node.id().as_str().to_string(),
                }),
                None => Ok(()),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Error)]
pub enum OpResultError {
    #[error("op result is missing")]
    MissingOp,
    #[error("duration {value} is negative or not finite")]
    InvalidDuration { value: f64 },
    #[error("measurement {field} {value} is not finite")]
    InvalidMeasurement { field: &'static str, value: f64 },
    #[error("thumbnail result has no outputs")]
    EmptyOutputs,
    #[error("batch result has no steps")]
    EmptyBatchResult,
    #[error("graph result has no nodes")]
    EmptyGraphResult,
    #[error("graph node id {id:?} is not valid: {source}")]
    InvalidNodeId {
        id: String,
        #[source]
        source: NodeIdError,
    },
    #[error(transparent)]
    JobFailure(#[from] JobFailureError),
    #[error(transparent)]
    MediaRef(#[from] MediaRefError),
    #[error(transparent)]
    Profile(#[from] OutputProfileError),
}

fn parse_duration(value: f64) -> Result<f64, OpResultError> {
    if value.is_finite() && value >= 0.0 {
        Ok(value)
    } else {
        Err(OpResultError::InvalidDuration { value })
    }
}

fn millis(seconds: f64) -> i64 {
    (seconds * 1000.0).round() as i64
}

#[derive(Debug, Clone, PartialEq)]
pub struct TimedOutput {
    output: MediaRef,
    duration_seconds: f64,
}

impl TimedOutput {
    pub fn new(output: MediaRef, duration_seconds: f64) -> Result<Self, OpResultError> {
        Ok(Self {
            output,
            duration_seconds: parse_duration(duration_seconds)?,
        })
    }

    pub fn output(&self) -> &MediaRef {
        &self.output
    }

    pub fn duration_seconds(&self) -> f64 {
        self.duration_seconds
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoStreamInfo {
    codec: String,
    width: u32,
    height: u32,
    frame_rate: f64,
    rotation: i32,
}

impl VideoStreamInfo {
    pub fn new(
        codec: String,
        width: u32,
        height: u32,
        frame_rate: f64,
        rotation: i32,
    ) -> Result<Self, OpResultError> {
        Ok(Self {
            codec,
            width,
            height,
            frame_rate: parse_duration(frame_rate)?,
            rotation,
        })
    }

    pub fn codec(&self) -> &str {
        &self.codec
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn frame_rate(&self) -> f64 {
        self.frame_rate
    }

    pub fn rotation(&self) -> i32 {
        self.rotation
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioStreamInfo {
    codec: String,
    sample_rate: u32,
    channels: u32,
}

impl AudioStreamInfo {
    pub fn new(codec: String, sample_rate: u32, channels: u32) -> Self {
        Self {
            codec,
            sample_rate,
            channels,
        }
    }

    pub fn codec(&self) -> &str {
        &self.codec
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u32 {
        self.channels
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProbeResult {
    duration_seconds: f64,
    video_streams: u32,
    audio_streams: u32,
    container: String,
    video: Vec<VideoStreamInfo>,
    audio: Vec<AudioStreamInfo>,
}

impl ProbeResult {
    pub fn new(
        duration_seconds: f64,
        video_streams: u32,
        audio_streams: u32,
        container: String,
    ) -> Result<Self, OpResultError> {
        Ok(Self {
            duration_seconds: parse_duration(duration_seconds)?,
            video_streams,
            audio_streams,
            container,
            video: Vec::new(),
            audio: Vec::new(),
        })
    }

    pub fn with_streams(
        mut self,
        video: Vec<VideoStreamInfo>,
        audio: Vec<AudioStreamInfo>,
    ) -> Self {
        self.video = video;
        self.audio = audio;
        self
    }

    pub fn duration_seconds(&self) -> f64 {
        self.duration_seconds
    }

    pub fn video_streams(&self) -> u32 {
        self.video_streams
    }

    pub fn audio_streams(&self) -> u32 {
        self.audio_streams
    }

    pub fn video(&self) -> &[VideoStreamInfo] {
        &self.video
    }

    pub fn audio(&self) -> &[AudioStreamInfo] {
        &self.audio
    }

    pub fn container(&self) -> &str {
        &self.container
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TranscodeResult {
    output: MediaRef,
}

impl TranscodeResult {
    pub fn new(output: MediaRef) -> Self {
        Self { output }
    }

    pub fn output(&self) -> &MediaRef {
        &self.output
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ThumbnailResult {
    outputs: Vec<MediaRef>,
}

impl ThumbnailResult {
    pub fn new(outputs: Vec<MediaRef>) -> Result<Self, OpResultError> {
        if outputs.is_empty() {
            return Err(OpResultError::EmptyOutputs);
        }
        Ok(Self { outputs })
    }

    pub fn outputs(&self) -> &[MediaRef] {
        &self.outputs
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImageResult {
    outputs: Vec<MediaRef>,
}

impl ImageResult {
    pub fn new(outputs: Vec<MediaRef>) -> Result<Self, OpResultError> {
        if outputs.is_empty() {
            return Err(OpResultError::EmptyOutputs);
        }
        Ok(Self { outputs })
    }

    pub fn outputs(&self) -> &[MediaRef] {
        &self.outputs
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PackageResult {
    playlist: MediaRef,
    segments: Vec<MediaRef>,
    init_segment: Option<MediaRef>,
}

impl PackageResult {
    pub fn new(
        playlist: MediaRef,
        segments: Vec<MediaRef>,
        init_segment: Option<MediaRef>,
    ) -> Result<Self, OpResultError> {
        if segments.is_empty() {
            return Err(OpResultError::EmptyOutputs);
        }
        Ok(Self {
            playlist,
            segments,
            init_segment,
        })
    }

    pub fn playlist(&self) -> &MediaRef {
        &self.playlist
    }

    pub fn segments(&self) -> &[MediaRef] {
        &self.segments
    }

    pub fn init_segment(&self) -> Option<&MediaRef> {
        self.init_segment.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RenditionResult {
    profile: OutputProfile,
    media: MediaRef,
    playlist: Option<MediaRef>,
    segments: Vec<MediaRef>,
    init_segment: Option<MediaRef>,
    duration_seconds: f64,
    width: u32,
    height: u32,
    bandwidth_bps: u32,
}

impl RenditionResult {
    pub fn new(
        profile: OutputProfile,
        media: MediaRef,
        playlist: Option<MediaRef>,
        segments: Vec<MediaRef>,
        duration_seconds: f64,
    ) -> Result<Self, OpResultError> {
        Ok(Self {
            profile,
            media,
            playlist,
            segments,
            init_segment: None,
            duration_seconds: parse_duration(duration_seconds)?,
            width: 0,
            height: 0,
            bandwidth_bps: 0,
        })
    }

    pub fn with_init_segment(mut self, init_segment: MediaRef) -> Self {
        self.init_segment = Some(init_segment);
        self
    }

    pub fn init_segment(&self) -> Option<&MediaRef> {
        self.init_segment.as_ref()
    }

    pub fn with_measurement(mut self, width: u32, height: u32, bandwidth_bps: u32) -> Self {
        self.width = width;
        self.height = height;
        self.bandwidth_bps = bandwidth_bps;
        self
    }

    pub fn profile(&self) -> &OutputProfile {
        &self.profile
    }

    pub fn media(&self) -> &MediaRef {
        &self.media
    }

    pub fn playlist(&self) -> Option<&MediaRef> {
        self.playlist.as_ref()
    }

    pub fn segments(&self) -> &[MediaRef] {
        &self.segments
    }

    pub fn duration_seconds(&self) -> f64 {
        self.duration_seconds
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn bandwidth_bps(&self) -> u32 {
        self.bandwidth_bps
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TitleResult {
    output: MediaRef,
}

impl TitleResult {
    pub fn new(output: MediaRef) -> Self {
        Self { output }
    }

    pub fn output(&self) -> &MediaRef {
        &self.output
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioResult {
    output: MediaRef,
    duration_seconds: f64,
}

impl AudioResult {
    pub fn new(output: MediaRef, duration_seconds: f64) -> Result<Self, OpResultError> {
        Ok(Self {
            output,
            duration_seconds: parse_duration(duration_seconds)?,
        })
    }

    pub fn output(&self) -> &MediaRef {
        &self.output
    }

    pub fn duration_seconds(&self) -> f64 {
        self.duration_seconds
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GradeResult {
    output: MediaRef,
    scope: Option<MediaRef>,
}

impl GradeResult {
    pub fn new(output: MediaRef, scope: Option<MediaRef>) -> Self {
        Self { output, scope }
    }

    pub fn output(&self) -> &MediaRef {
        &self.output
    }

    pub fn scope(&self) -> Option<&MediaRef> {
        self.scope.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SubtitleResult {
    output: MediaRef,
}

impl SubtitleResult {
    pub fn new(output: MediaRef) -> Self {
        Self { output }
    }

    pub fn output(&self) -> &MediaRef {
        &self.output
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoudnessResult {
    output: Option<MediaRef>,
    integrated_lufs: f64,
    true_peak_db: f64,
    loudness_range_lu: f64,
}

impl LoudnessResult {
    pub fn new(
        output: Option<MediaRef>,
        integrated_lufs: f64,
        true_peak_db: f64,
        loudness_range_lu: f64,
    ) -> Result<Self, OpResultError> {
        for (field, value) in [
            ("integrated_lufs", integrated_lufs),
            ("true_peak_db", true_peak_db),
            ("loudness_range_lu", loudness_range_lu),
        ] {
            if !value.is_finite() || (field == "loudness_range_lu" && value < 0.0) {
                return Err(OpResultError::InvalidMeasurement { field, value });
            }
        }
        Ok(Self {
            output,
            integrated_lufs,
            true_peak_db,
            loudness_range_lu,
        })
    }

    pub fn output(&self) -> Option<&MediaRef> {
        self.output.as_ref()
    }

    pub fn integrated_lufs(&self) -> f64 {
        self.integrated_lufs
    }

    pub fn true_peak_db(&self) -> f64 {
        self.true_peak_db
    }

    pub fn loudness_range_lu(&self) -> f64 {
        self.loudness_range_lu
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LadderResult {
    master_playlist: Option<MediaRef>,
    renditions: Vec<RenditionResult>,
}

impl LadderResult {
    pub fn new(
        master_playlist: Option<MediaRef>,
        renditions: Vec<RenditionResult>,
    ) -> Result<Self, OpResultError> {
        if renditions.is_empty() {
            return Err(OpResultError::EmptyOutputs);
        }
        Ok(Self {
            master_playlist,
            renditions,
        })
    }

    pub fn master_playlist(&self) -> Option<&MediaRef> {
        self.master_playlist.as_ref()
    }

    pub fn renditions(&self) -> &[RenditionResult] {
        &self.renditions
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum OpResult {
    Concat(TimedOutput),
    Probe(ProbeResult),
    Transcode(TranscodeResult),
    Trim(TimedOutput),
    Thumbnail(ThumbnailResult),
    Image(ImageResult),
    Package(PackageResult),
    Batch(BatchResult),
    Graph(GraphResult),
    Ladder(LadderResult),
    Loudness(LoudnessResult),
    Subtitle(SubtitleResult),
    Grade(GradeResult),
    Audio(AudioResult),
    Title(TitleResult),
}

impl OpResult {
    pub fn primary_output(&self) -> Option<&MediaRef> {
        match self {
            OpResult::Concat(output) => Some(output.output()),
            OpResult::Transcode(result) => Some(result.output()),
            OpResult::Trim(output) => Some(output.output()),
            OpResult::Thumbnail(result) => result.outputs().first(),
            OpResult::Image(result) => result.outputs().first(),
            OpResult::Package(result) => Some(result.playlist()),
            OpResult::Ladder(result) => result
                .master_playlist()
                .or_else(|| result.renditions().first().map(RenditionResult::media)),
            OpResult::Loudness(result) => result.output(),
            OpResult::Subtitle(result) => Some(result.output()),
            OpResult::Grade(result) => Some(result.output()),
            OpResult::Audio(result) => Some(result.output()),
            OpResult::Title(result) => Some(result.output()),
            OpResult::Probe(_) | OpResult::Batch(_) | OpResult::Graph(_) => None,
        }
    }

    pub fn merged_duration_ms(&self) -> Option<i64> {
        match self {
            OpResult::Concat(timed) => Some(millis(timed.duration_seconds())),
            OpResult::Ladder(result) => result
                .renditions()
                .first()
                .map(|rendition| millis(rendition.duration_seconds())),
            _ => None,
        }
    }
}

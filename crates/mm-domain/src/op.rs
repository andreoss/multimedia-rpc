use thiserror::Error;

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
}

#[derive(Debug, Clone, Copy, PartialEq, Error)]
pub enum OutputProfileError {
    #[error("frame rate {value} is not positive and finite")]
    InvalidFrameRate { value: f64 },
    #[error("resolution {width}x{height} sets only one dimension")]
    PartialResolution { width: u32, height: u32 },
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
        })
    }

    pub fn is_empty(&self) -> bool {
        self.frame_rate.is_none()
            && self.video_max_bitrate.is_none()
            && self.video_quality.is_none()
            && self.audio_bitrate.is_none()
            && self.audio_sample_rate.is_none()
            && self.resolution.is_none()
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
    #[error("offset {value} is negative or not finite")]
    InvalidOffset { value: f64 },
    #[error("{dimension} is zero")]
    ZeroDimension { dimension: &'static str },
    #[error(transparent)]
    MediaRef(#[from] MediaRefError),
    #[error(transparent)]
    Profile(#[from] OutputProfileError),
}

fn parse_nonnegative(value: f64, error: fn(f64) -> OpSpecError) -> Result<f64, OpSpecError> {
    if value.is_finite() && value >= 0.0 {
        Ok(value)
    } else {
        Err(error(value))
    }
}

fn parse_positive(value: f64) -> Result<f64, OpSpecError> {
    if value.is_finite() && value > 0.0 {
        Ok(value)
    } else {
        Err(OpSpecError::NonPositiveDuration { value })
    }
}

fn parse_profile(profile: Option<OutputProfile>) -> Result<Option<OutputProfile>, OpSpecError> {
    match profile {
        Some(profile) if profile.is_empty() => Err(OpSpecError::EmptyProfile),
        other => Ok(other),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConcatSpec {
    inputs: Vec<MediaRef>,
    profile: Option<OutputProfile>,
}

impl ConcatSpec {
    pub fn new(inputs: Vec<MediaRef>, profile: Option<OutputProfile>) -> Result<Self, OpSpecError> {
        if inputs.is_empty() {
            return Err(OpSpecError::EmptyInputs);
        }
        if inputs.len() < 2 {
            return Err(OpSpecError::TooFewConcatInputs {
                count: inputs.len(),
            });
        }
        Ok(Self {
            inputs,
            profile: parse_profile(profile)?,
        })
    }

    pub fn inputs(&self) -> &[MediaRef] {
        &self.inputs
    }

    pub fn profile(&self) -> Option<&OutputProfile> {
        self.profile.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProbeSpec {
    input: MediaRef,
}

impl ProbeSpec {
    pub fn new(input: MediaRef) -> Self {
        Self { input }
    }

    pub fn input(&self) -> &MediaRef {
        &self.input
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TranscodeSpec {
    input: MediaRef,
    profile: OutputProfile,
}

impl TranscodeSpec {
    pub fn new(input: MediaRef, profile: OutputProfile) -> Result<Self, OpSpecError> {
        if profile.is_empty() {
            return Err(OpSpecError::EmptyProfile);
        }
        Ok(Self { input, profile })
    }

    pub fn input(&self) -> &MediaRef {
        &self.input
    }

    pub fn profile(&self) -> &OutputProfile {
        &self.profile
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrimSpec {
    input: MediaRef,
    start_seconds: f64,
    duration_seconds: f64,
}

impl TrimSpec {
    pub fn new(
        input: MediaRef,
        start_seconds: f64,
        duration_seconds: f64,
    ) -> Result<Self, OpSpecError> {
        Ok(Self {
            input,
            start_seconds: parse_nonnegative(start_seconds, |value| OpSpecError::InvalidStart {
                value,
            })?,
            duration_seconds: parse_positive(duration_seconds)?,
        })
    }

    pub fn input(&self) -> &MediaRef {
        &self.input
    }

    pub fn start_seconds(&self) -> f64 {
        self.start_seconds
    }

    pub fn duration_seconds(&self) -> f64 {
        self.duration_seconds
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ThumbnailSpec {
    input: MediaRef,
    offsets_seconds: Vec<f64>,
    width: u32,
    height: u32,
}

impl ThumbnailSpec {
    pub fn new(
        input: MediaRef,
        offsets_seconds: Vec<f64>,
        width: u32,
        height: u32,
    ) -> Result<Self, OpSpecError> {
        if offsets_seconds.is_empty() {
            return Err(OpSpecError::EmptyOffsets);
        }
        for offset in &offsets_seconds {
            parse_nonnegative(*offset, |value| OpSpecError::InvalidOffset { value })?;
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
            offsets_seconds,
            width,
            height,
        })
    }

    pub fn input(&self) -> &MediaRef {
        &self.input
    }

    pub fn offsets_seconds(&self) -> &[f64] {
        &self.offsets_seconds
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum OpSpec {
    Concat(ConcatSpec),
    Probe(ProbeSpec),
    Transcode(TranscodeSpec),
    Trim(TrimSpec),
    Thumbnail(ThumbnailSpec),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpKind {
    Concat,
    Probe,
    Transcode,
    Trim,
    Thumbnail,
}

impl std::fmt::Display for OpKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            OpKind::Concat => "concat",
            OpKind::Probe => "probe",
            OpKind::Transcode => "transcode",
            OpKind::Trim => "trim",
            OpKind::Thumbnail => "thumbnail",
        };
        formatter.write_str(name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("{kind} is not supported yet")]
pub struct UnsupportedOp {
    pub kind: OpKind,
}

impl OpKind {
    pub fn ensure_executable(self) -> Result<(), UnsupportedOp> {
        match self {
            OpKind::Concat | OpKind::Probe | OpKind::Transcode | OpKind::Trim => Ok(()),
            OpKind::Thumbnail => Err(UnsupportedOp { kind: self }),
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
        }
    }
}

#[derive(Debug, Clone, PartialEq, Error)]
pub enum OpResultError {
    #[error("op result is missing")]
    MissingOp,
    #[error("duration {value} is negative or not finite")]
    InvalidDuration { value: f64 },
    #[error("thumbnail result has no outputs")]
    EmptyOutputs,
    #[error(transparent)]
    MediaRef(#[from] MediaRefError),
}

fn parse_duration(value: f64) -> Result<f64, OpResultError> {
    if value.is_finite() && value >= 0.0 {
        Ok(value)
    } else {
        Err(OpResultError::InvalidDuration { value })
    }
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
pub struct ProbeResult {
    duration_seconds: f64,
    video_streams: u32,
    audio_streams: u32,
    container: String,
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
        })
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
pub enum OpResult {
    Concat(TimedOutput),
    Probe(ProbeResult),
    Transcode(TranscodeResult),
    Trim(TimedOutput),
    Thumbnail(ThumbnailResult),
}

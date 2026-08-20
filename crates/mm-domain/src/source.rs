use thiserror::Error;

use crate::id::{NodeId, NodeIdError, ParameterName, ParameterNameError};
use crate::op::{MediaRef, MediaRefError};
use crate::time::Ticks;

const MIN_SPEED: f64 = 0.5;
const MAX_SPEED: f64 = 2.0;

#[derive(Debug, Clone, PartialEq, Error)]
pub enum SourceError {
    #[error("source media is missing")]
    MissingMedia,
    #[error("input source is missing")]
    MissingSource,
    #[error("trim start_seconds {value} is negative or not finite")]
    InvalidTrimStart { value: f64 },
    #[error("trim duration_seconds {value} is not positive and finite")]
    InvalidTrimDuration { value: f64 },
    #[error(
        "speed {value} is outside the supported range {} to {}",
        MIN_SPEED,
        MAX_SPEED
    )]
    InvalidSpeed { value: f64 },
    #[error("volume {value} is negative or not finite")]
    InvalidVolume { value: f64 },
    #[error("node id {value:?} is not a valid node id: {source}")]
    InvalidNodeId {
        value: String,
        #[source]
        source: NodeIdError,
    },
    #[error("parameter name {value:?} is not valid: {source}")]
    InvalidParameterName {
        value: String,
        #[source]
        source: ParameterNameError,
    },
    #[error(transparent)]
    MediaRef(#[from] MediaRefError),
}

fn nonnegative(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}

fn positive(value: f64) -> bool {
    value.is_finite() && value > 0.0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrimRange {
    start: Ticks,
    duration: Ticks,
}

impl TrimRange {
    pub fn new(start_seconds: f64, duration_seconds: f64) -> Result<Self, SourceError> {
        if !nonnegative(start_seconds) {
            return Err(SourceError::InvalidTrimStart {
                value: start_seconds,
            });
        }
        if !positive(duration_seconds) {
            return Err(SourceError::InvalidTrimDuration {
                value: duration_seconds,
            });
        }
        let start =
            Ticks::from_seconds(start_seconds).map_err(|_| SourceError::InvalidTrimStart {
                value: start_seconds,
            })?;
        let duration = Ticks::from_seconds(duration_seconds).map_err(|_| {
            SourceError::InvalidTrimDuration {
                value: duration_seconds,
            }
        })?;
        Self::from_ticks(start, duration).map_err(|_| SourceError::InvalidTrimDuration {
            value: duration_seconds,
        })
    }

    pub fn from_ticks(start: Ticks, duration: Ticks) -> Result<Self, SourceError> {
        if duration.is_zero() {
            return Err(SourceError::InvalidTrimDuration { value: 0.0 });
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

#[derive(Debug, Clone, PartialEq)]
pub struct MediaSource {
    media: MediaRef,
    trim: Option<TrimRange>,
    speed: Option<f64>,
    volume: Option<f64>,
}

impl MediaSource {
    pub fn of(media: MediaRef) -> Self {
        Self {
            media,
            trim: None,
            speed: None,
            volume: None,
        }
    }

    pub fn new(
        media: MediaRef,
        trim: Option<TrimRange>,
        speed: Option<f64>,
        volume: Option<f64>,
    ) -> Result<Self, SourceError> {
        if let Some(value) = speed
            && !(MIN_SPEED..=MAX_SPEED).contains(&value)
        {
            return Err(SourceError::InvalidSpeed { value });
        }
        if let Some(value) = volume
            && !nonnegative(value)
        {
            return Err(SourceError::InvalidVolume { value });
        }
        Ok(Self {
            media,
            trim,
            speed,
            volume,
        })
    }

    pub fn media(&self) -> &MediaRef {
        &self.media
    }

    pub fn trim(&self) -> Option<&TrimRange> {
        self.trim.as_ref()
    }

    pub fn speed(&self) -> Option<f64> {
        self.speed
    }

    pub fn volume(&self) -> Option<f64> {
        self.volume
    }

    pub fn cuts(&self) -> Option<&'static str> {
        if self.trim.is_some() {
            return Some("trim");
        }
        if self.speed.is_some() {
            return Some("speed");
        }
        if self.volume.is_some() {
            return Some("volume");
        }
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRef {
    id: NodeId,
    output: u32,
}

impl NodeRef {
    pub fn new(id: &str, output: u32) -> Result<Self, SourceError> {
        let parsed = id
            .parse::<NodeId>()
            .map_err(|source| SourceError::InvalidNodeId {
                value: id.to_string(),
                source,
            })?;
        Ok(Self { id: parsed, output })
    }

    pub fn id(&self) -> &NodeId {
        &self.id
    }

    pub fn output(&self) -> u32 {
        self.output
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum InputSource {
    Media(MediaSource),
    Node(NodeRef),
    Parameter(ParameterName),
}

impl InputSource {
    pub fn media(&self) -> Option<&MediaSource> {
        match self {
            Self::Media(source) => Some(source),
            Self::Node(_) | Self::Parameter(_) => None,
        }
    }

    pub fn node(&self) -> Option<&NodeRef> {
        match self {
            Self::Node(node) => Some(node),
            Self::Media(_) | Self::Parameter(_) => None,
        }
    }

    pub fn parameter(&self) -> Option<&ParameterName> {
        match self {
            Self::Parameter(name) => Some(name),
            Self::Media(_) | Self::Node(_) => None,
        }
    }

    pub fn cuts(&self) -> Option<&'static str> {
        self.media().and_then(MediaSource::cuts)
    }
}

impl From<MediaRef> for InputSource {
    fn from(media: MediaRef) -> Self {
        Self::Media(MediaSource::of(media))
    }
}

impl From<MediaSource> for InputSource {
    fn from(source: MediaSource) -> Self {
        Self::Media(source)
    }
}

impl From<NodeRef> for InputSource {
    fn from(node: NodeRef) -> Self {
        Self::Node(node)
    }
}

impl From<ParameterName> for InputSource {
    fn from(name: ParameterName) -> Self {
        Self::Parameter(name)
    }
}

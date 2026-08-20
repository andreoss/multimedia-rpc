use thiserror::Error;

use crate::id::{NodeId, NodeIdError};
use crate::jobs::{JobFailure, JobFailureError};
use crate::source::{InputSource, SourceError};

pub const MAX_BATCH_STEPS: usize = 32;
pub const MAX_GRAPH_NODES: usize = 64;
pub const MAX_NESTING_DEPTH: usize = 4;
pub const MAX_TREE_STEPS: usize = 128;

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
    poster_seconds: Option<f64>,
    range: Option<OutputRange>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputRange {
    start_seconds: f64,
    duration_seconds: f64,
}

impl OutputRange {
    pub fn new(start_seconds: f64, duration_seconds: f64) -> Result<Self, OutputProfileError> {
        if !start_seconds.is_finite() || start_seconds < 0.0 {
            return Err(OutputProfileError::InvalidRangeStart {
                value: start_seconds,
            });
        }
        if !duration_seconds.is_finite() || duration_seconds <= 0.0 {
            return Err(OutputProfileError::InvalidRangeDuration {
                value: duration_seconds,
            });
        }
        Ok(Self {
            start_seconds,
            duration_seconds,
        })
    }

    pub fn start_seconds(&self) -> f64 {
        self.start_seconds
    }

    pub fn duration_seconds(&self) -> f64 {
        self.duration_seconds
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
            poster_seconds: None,
            range: None,
        })
    }

    pub fn with_poster(mut self, poster_seconds: f64) -> Result<Self, OutputProfileError> {
        if !poster_seconds.is_finite() || poster_seconds < 0.0 {
            return Err(OutputProfileError::InvalidPoster {
                value: poster_seconds,
            });
        }
        self.poster_seconds = Some(poster_seconds);
        Ok(self)
    }

    pub fn with_range(mut self, range: OutputRange) -> Self {
        self.range = Some(range);
        self
    }

    pub fn poster_seconds(&self) -> Option<f64> {
        self.poster_seconds
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
            && self.poster_seconds.is_none()
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
    #[error("offset {value} is negative or not finite")]
    InvalidOffset { value: f64 },
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
    start_seconds: f64,
    duration_seconds: f64,
}

impl TrimSpec {
    pub fn new<I: Into<InputSource>>(
        input: I,
        start_seconds: f64,
        duration_seconds: f64,
    ) -> Result<Self, OpSpecError> {
        let input = source(input);
        reject_cuts("trim", std::slice::from_ref(&input))?;
        Ok(Self {
            input,
            start_seconds: parse_nonnegative(start_seconds, |value| OpSpecError::InvalidStart {
                value,
            })?,
            duration_seconds: parse_positive(duration_seconds)?,
        })
    }

    pub fn input(&self) -> &InputSource {
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
    input: InputSource,
    offsets_seconds: Vec<f64>,
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
        let input = source(input);
        reject_cuts("thumbnail", std::slice::from_ref(&input))?;
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

    pub fn offsets_seconds(&self) -> &[f64] {
        &self.offsets_seconds
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
    segment_seconds: f64,
}

impl PackageSpec {
    pub fn new<I: Into<InputSource>>(input: I, segment_seconds: f64) -> Result<Self, OpSpecError> {
        let input = source(input);
        reject_cuts("package", std::slice::from_ref(&input))?;
        if !segment_seconds.is_finite() || segment_seconds <= 0.0 {
            return Err(OpSpecError::InvalidSegmentLength {
                value: segment_seconds,
            });
        }
        Ok(Self {
            input,
            segment_seconds,
        })
    }

    pub fn input(&self) -> &InputSource {
        &self.input
    }

    pub fn segment_seconds(&self) -> f64 {
        self.segment_seconds
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
            | OpKind::Graph => Ok(()),
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
}

impl PackageResult {
    pub fn new(playlist: MediaRef, segments: Vec<MediaRef>) -> Result<Self, OpResultError> {
        if segments.is_empty() {
            return Err(OpResultError::EmptyOutputs);
        }
        Ok(Self { playlist, segments })
    }

    pub fn playlist(&self) -> &MediaRef {
        &self.playlist
    }

    pub fn segments(&self) -> &[MediaRef] {
        &self.segments
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
            OpResult::Probe(_) | OpResult::Batch(_) | OpResult::Graph(_) => None,
        }
    }
}

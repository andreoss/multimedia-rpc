use crate::v1;
use mm_domain::{
    AudioStreamInfo, BatchResult, BatchSpec, ConcatSpec, GraphNode, GraphResult, GraphSpec,
    ImageFormat, ImageOp, ImageResult, ImageSpec, InputFailure, InputFailureError, InputOrder,
    InputOrderError, InputSource, JobFailure, JobFailureError, JobRecord, JobState, LadderResult,
    LadderSpec, LoudnessResult, LoudnessSpec, LoudnessTarget, MediaRef, MediaRefError, MediaSource,
    NodeId, NodeOutcome, NodeRef, OpResult, OpResultError, OpSpec, OpSpecError, OutputProfile,
    OutputProfileError, OutputRange, PackageResult, PackageSpec, ParameterDeclaration,
    ParameterName, ParameterType, ParameterValue, ProbeResult, ProbeSpec, QueueUrl,
    RenditionResult, ResizeMode, RungProgress, RungState, SourceError, StepOutcome, StepState,
    Template, TemplateError, TextIdError, ThumbnailResult, ThumbnailSpec, TimedOutput,
    TranscodeResult, TranscodeSpec, TrimRange, TrimSpec, VideoStreamInfo, failure_code,
    verdict_code,
};

impl TryFrom<v1::MediaRef> for MediaRef {
    type Error = MediaRefError;

    fn try_from(value: v1::MediaRef) -> Result<Self, Self::Error> {
        match value.r#ref {
            None => Err(MediaRefError::Missing),
            Some(v1::media_ref::Ref::ObjectUri(uri)) => MediaRef::object_uri(&uri),
            Some(v1::media_ref::Ref::HttpUrl(url)) => MediaRef::http_url(&url),
        }
    }
}

impl From<&MediaRef> for v1::MediaRef {
    fn from(value: &MediaRef) -> Self {
        let r#ref = match value {
            MediaRef::Object(_) => v1::media_ref::Ref::ObjectUri(value.to_uri()),
            MediaRef::Http(http_ref) => v1::media_ref::Ref::HttpUrl(http_ref.url().to_string()),
        };
        v1::MediaRef { r#ref: Some(r#ref) }
    }
}

fn optional_ref(value: Option<v1::MediaRef>) -> Result<Option<MediaRef>, MediaRefError> {
    match value {
        Some(value) => Ok(Some(value.try_into()?)),
        None => Ok(None),
    }
}

fn required_ref(value: Option<v1::MediaRef>) -> Result<MediaRef, MediaRefError> {
    value.ok_or(MediaRefError::Missing)?.try_into()
}

fn ref_list(values: Vec<v1::MediaRef>) -> Result<Vec<MediaRef>, MediaRefError> {
    values.into_iter().map(MediaRef::try_from).collect()
}

impl TryFrom<v1::TrimRange> for TrimRange {
    type Error = SourceError;

    fn try_from(value: v1::TrimRange) -> Result<Self, Self::Error> {
        TrimRange::new(value.start_seconds, value.duration_seconds)
    }
}

impl From<&TrimRange> for v1::TrimRange {
    fn from(value: &TrimRange) -> Self {
        Self {
            start_seconds: value.start_seconds(),
            duration_seconds: value.duration_seconds(),
        }
    }
}

impl TryFrom<v1::MediaSource> for MediaSource {
    type Error = SourceError;

    fn try_from(value: v1::MediaSource) -> Result<Self, Self::Error> {
        let media = MediaRef::try_from(value.media.ok_or(SourceError::MissingMedia)?)?;
        let trim = value.trim.map(TrimRange::try_from).transpose()?;
        MediaSource::new(media, trim, value.speed, value.volume)
    }
}

impl From<&MediaSource> for v1::MediaSource {
    fn from(value: &MediaSource) -> Self {
        Self {
            media: Some(v1::MediaRef::from(value.media())),
            trim: value.trim().map(v1::TrimRange::from),
            speed: value.speed(),
            volume: value.volume(),
        }
    }
}

impl TryFrom<v1::NodeRef> for NodeRef {
    type Error = SourceError;

    fn try_from(value: v1::NodeRef) -> Result<Self, Self::Error> {
        NodeRef::new(&value.id, value.output)
    }
}

impl From<&NodeRef> for v1::NodeRef {
    fn from(value: &NodeRef) -> Self {
        Self {
            id: value.id().as_str().to_string(),
            output: value.output(),
        }
    }
}

impl TryFrom<v1::InputSource> for InputSource {
    type Error = SourceError;

    fn try_from(value: v1::InputSource) -> Result<Self, Self::Error> {
        match value.source {
            Some(v1::input_source::Source::Media(source)) => {
                MediaSource::try_from(source).map(InputSource::from)
            }
            Some(v1::input_source::Source::Node(node)) => {
                NodeRef::try_from(node).map(InputSource::from)
            }
            Some(v1::input_source::Source::Parameter(parameter)) => parameter
                .name
                .parse::<ParameterName>()
                .map(InputSource::from)
                .map_err(|source| SourceError::InvalidParameterName {
                    value: parameter.name,
                    source,
                }),
            None => Err(SourceError::MissingSource),
        }
    }
}

impl From<&InputSource> for v1::InputSource {
    fn from(value: &InputSource) -> Self {
        match value {
            InputSource::Media(source) => v1::InputSource {
                source: Some(v1::input_source::Source::Media(v1::MediaSource::from(
                    source,
                ))),
            },
            InputSource::Node(node) => v1::InputSource {
                source: Some(v1::input_source::Source::Node(v1::NodeRef::from(node))),
            },
            InputSource::Parameter(name) => v1::InputSource {
                source: Some(v1::input_source::Source::Parameter(v1::ParameterRef {
                    name: name.as_str().to_string(),
                })),
            },
        }
    }
}

fn required_source(value: Option<v1::InputSource>) -> Result<InputSource, SourceError> {
    value.ok_or(SourceError::MissingSource)?.try_into()
}

fn source_list(values: Vec<v1::InputSource>) -> Result<Vec<InputSource>, SourceError> {
    values.into_iter().map(InputSource::try_from).collect()
}

impl TryFrom<v1::BatchSpec> for BatchSpec {
    type Error = OpSpecError;

    fn try_from(value: v1::BatchSpec) -> Result<Self, Self::Error> {
        let steps = value
            .steps
            .into_iter()
            .map(OpSpec::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        BatchSpec::new(steps, value.stop_on_failure.unwrap_or(true))
    }
}

impl From<&BatchSpec> for v1::BatchSpec {
    fn from(value: &BatchSpec) -> Self {
        Self {
            steps: value.steps().iter().map(v1::OpSpec::from).collect(),
            stop_on_failure: Some(value.stop_on_failure()),
        }
    }
}

impl TryFrom<v1::GraphNode> for GraphNode {
    type Error = OpSpecError;

    fn try_from(value: v1::GraphNode) -> Result<Self, Self::Error> {
        let spec = OpSpec::try_from(value.spec.ok_or(OpSpecError::MissingOp)?)?;
        GraphNode::new(&value.id, spec)
    }
}

impl From<&GraphNode> for v1::GraphNode {
    fn from(value: &GraphNode) -> Self {
        Self {
            id: value.id().as_str().to_string(),
            spec: Some(v1::OpSpec::from(value.spec())),
        }
    }
}

impl TryFrom<v1::GraphSpec> for GraphSpec {
    type Error = OpSpecError;

    fn try_from(value: v1::GraphSpec) -> Result<Self, Self::Error> {
        let nodes = value
            .nodes
            .into_iter()
            .map(GraphNode::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        GraphSpec::new(nodes)
    }
}

impl From<&GraphSpec> for v1::GraphSpec {
    fn from(value: &GraphSpec) -> Self {
        Self {
            nodes: value.nodes().iter().map(v1::GraphNode::from).collect(),
        }
    }
}

fn parameter_type_from_raw(value: i32) -> Result<ParameterType, TemplateError> {
    match value {
        raw if raw == v1::ParameterType::Uri as i32 => Ok(ParameterType::Uri),
        raw if raw == v1::ParameterType::Text as i32 => Ok(ParameterType::Text),
        other => Err(TemplateError::UnknownParameterType { value: other }),
    }
}

fn parameter_type_to_raw(value: ParameterType) -> i32 {
    match value {
        ParameterType::Uri => v1::ParameterType::Uri as i32,
        ParameterType::Text => v1::ParameterType::Text as i32,
    }
}

impl TryFrom<v1::ParameterDeclaration> for ParameterDeclaration {
    type Error = TemplateError;

    fn try_from(value: v1::ParameterDeclaration) -> Result<Self, Self::Error> {
        ParameterDeclaration::new(&value.name, parameter_type_from_raw(value.r#type)?)
    }
}

impl From<&ParameterDeclaration> for v1::ParameterDeclaration {
    fn from(value: &ParameterDeclaration) -> Self {
        Self {
            name: value.name().as_str().to_string(),
            r#type: parameter_type_to_raw(value.kind()),
        }
    }
}

impl From<&[ParameterDeclaration]> for v1::ParameterDeclarations {
    fn from(value: &[ParameterDeclaration]) -> Self {
        Self {
            declarations: value.iter().map(v1::ParameterDeclaration::from).collect(),
        }
    }
}

impl TryFrom<v1::ParameterDeclarations> for Vec<ParameterDeclaration> {
    type Error = TemplateError;

    fn try_from(value: v1::ParameterDeclarations) -> Result<Self, Self::Error> {
        value
            .declarations
            .into_iter()
            .map(ParameterDeclaration::try_from)
            .collect()
    }
}

impl TryFrom<v1::ParameterValue> for ParameterValue {
    type Error = TemplateError;

    fn try_from(value: v1::ParameterValue) -> Result<Self, Self::Error> {
        match value.value {
            Some(v1::parameter_value::Value::Uri(media)) => MediaRef::try_from(media)
                .map(ParameterValue::Uri)
                .map_err(|error| TemplateError::Spec(OpSpecError::from(error))),
            Some(v1::parameter_value::Value::Text(text)) => Ok(ParameterValue::Text(text)),
            None => Err(TemplateError::MissingValue),
        }
    }
}

impl From<&ParameterValue> for v1::ParameterValue {
    fn from(value: &ParameterValue) -> Self {
        let value = match value {
            ParameterValue::Uri(media) => {
                v1::parameter_value::Value::Uri(v1::MediaRef::from(media))
            }
            ParameterValue::Text(text) => v1::parameter_value::Value::Text(text.clone()),
        };
        Self { value: Some(value) }
    }
}

impl From<&Template> for v1::Template {
    fn from(value: &Template) -> Self {
        Self {
            template_id: value.id().to_string(),
            name: value.name().as_str().to_string(),
            spec: Some(v1::OpSpec::from(value.spec())),
            parameters: value
                .parameters()
                .iter()
                .map(v1::ParameterDeclaration::from)
                .collect(),
        }
    }
}

impl TryFrom<v1::OutputProfile> for OutputProfile {
    type Error = OutputProfileError;

    fn try_from(value: v1::OutputProfile) -> Result<Self, Self::Error> {
        let profile = OutputProfile::from_raw(
            value.frame_rate,
            value.video_max_bitrate,
            value.video_quality,
            value.audio_bitrate,
            value.audio_sample_rate,
            value.width,
            value.height,
        )?;
        let profile = match value.poster_seconds {
            Some(seconds) => profile.with_poster(seconds)?,
            None => profile,
        };
        match value.range {
            Some(range) => Ok(profile.with_range(OutputRange::new(
                range.start_seconds,
                range.duration_seconds,
            )?)),
            None => Ok(profile),
        }
    }
}

impl From<&OutputProfile> for v1::OutputProfile {
    fn from(value: &OutputProfile) -> Self {
        let (width, height) = value.resolution().unwrap_or((0, 0));
        v1::OutputProfile {
            frame_rate: value.frame_rate().unwrap_or(0.0),
            video_max_bitrate: value.video_max_bitrate().unwrap_or(0),
            video_quality: value.video_quality().unwrap_or(0),
            audio_bitrate: value.audio_bitrate().unwrap_or(0),
            audio_sample_rate: value.audio_sample_rate().unwrap_or(0),
            width,
            height,
            poster_seconds: value.poster_seconds(),
            range: value.range().map(|range| v1::OutputRange {
                start_seconds: range.start_seconds(),
                duration_seconds: range.duration_seconds(),
            }),
        }
    }
}

impl TryFrom<v1::LadderSpec> for LadderSpec {
    type Error = OpSpecError;

    fn try_from(value: v1::LadderSpec) -> Result<Self, Self::Error> {
        let rungs = value
            .rungs
            .into_iter()
            .map(OutputProfile::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let spec = LadderSpec::new(required_source(value.input)?, rungs)?;
        let spec = match value.segment_seconds {
            Some(seconds) => spec.with_segment_seconds(seconds)?,
            None => spec,
        };
        match value.poster_seconds {
            Some(seconds) => Ok(spec.with_poster(seconds)?),
            None => Ok(spec),
        }
    }
}

impl From<&LadderSpec> for v1::LadderSpec {
    fn from(value: &LadderSpec) -> Self {
        Self {
            input: Some(v1::InputSource::from(value.input())),
            rungs: value.rungs().iter().map(v1::OutputProfile::from).collect(),
            segment_seconds: value.segment_seconds(),
            poster_seconds: value.poster_seconds(),
        }
    }
}

impl TryFrom<v1::LoudnessSpec> for LoudnessSpec {
    type Error = OpSpecError;

    fn try_from(value: v1::LoudnessSpec) -> Result<Self, Self::Error> {
        let target = value
            .target
            .map(|target| LoudnessTarget::new(target.integrated_lufs, target.true_peak_db))
            .transpose()?;
        LoudnessSpec::new(required_source(value.input)?, target)
    }
}

impl From<&LoudnessSpec> for v1::LoudnessSpec {
    fn from(value: &LoudnessSpec) -> Self {
        Self {
            input: Some(v1::InputSource::from(value.input())),
            target: value.target().map(|target| v1::LoudnessTarget {
                integrated_lufs: target.integrated_lufs(),
                true_peak_db: target.true_peak_db(),
            }),
        }
    }
}

impl TryFrom<v1::OpSpec> for OpSpec {
    type Error = OpSpecError;

    fn try_from(value: v1::OpSpec) -> Result<Self, Self::Error> {
        match value.op.ok_or(OpSpecError::MissingOp)? {
            v1::op_spec::Op::Concat(concat) => {
                let inputs = source_list(concat.inputs)?;
                let profile = concat.profile.map(OutputProfile::try_from).transpose()?;
                Ok(OpSpec::Concat(ConcatSpec::new(inputs, profile)?))
            }
            v1::op_spec::Op::Probe(probe) => Ok(OpSpec::Probe(ProbeSpec::new(required_source(
                probe.input,
            )?)?)),
            v1::op_spec::Op::Transcode(transcode) => {
                let input = required_source(transcode.input)?;
                let profile = transcode
                    .profile
                    .ok_or(OpSpecError::EmptyProfile)
                    .and_then(|profile| Ok(OutputProfile::try_from(profile)?))?;
                Ok(OpSpec::Transcode(TranscodeSpec::new(input, profile)?))
            }
            v1::op_spec::Op::Trim(trim) => Ok(OpSpec::Trim(TrimSpec::new(
                required_source(trim.input)?,
                trim.start_seconds,
                trim.duration_seconds,
            )?)),
            v1::op_spec::Op::Thumbnail(thumbnail) => Ok(OpSpec::Thumbnail(
                ThumbnailSpec::new(
                    required_source(thumbnail.input)?,
                    thumbnail.offsets_seconds,
                    thumbnail.width,
                    thumbnail.height,
                )?
                .with_sprite(thumbnail.sprite_columns),
            )),
            v1::op_spec::Op::Package(package) => Ok(OpSpec::Package(PackageSpec::new(
                required_source(package.input)?,
                package.segment_seconds,
            )?)),
            v1::op_spec::Op::Image(image) => Ok(OpSpec::Image(ImageSpec::new(
                source_list(image.inputs)?,
                image_op_from_raw(image.op)?,
                image.width,
                image.height,
                resize_mode_from_raw(image.resize_mode),
                image_format_from_raw(image.format),
            )?)),
            v1::op_spec::Op::Batch(batch) => Ok(OpSpec::Batch(BatchSpec::try_from(batch)?)),
            v1::op_spec::Op::Graph(graph) => Ok(OpSpec::Graph(GraphSpec::try_from(graph)?)),
            v1::op_spec::Op::Ladder(ladder) => Ok(OpSpec::Ladder(LadderSpec::try_from(ladder)?)),
            v1::op_spec::Op::Loudness(loudness) => {
                Ok(OpSpec::Loudness(LoudnessSpec::try_from(loudness)?))
            }
        }
    }
}

impl From<&OpSpec> for v1::OpSpec {
    fn from(value: &OpSpec) -> Self {
        let op = match value {
            OpSpec::Concat(concat) => v1::op_spec::Op::Concat(v1::ConcatSpec {
                inputs: concat.inputs().iter().map(v1::InputSource::from).collect(),
                profile: concat.profile().map(v1::OutputProfile::from),
            }),
            OpSpec::Probe(probe) => v1::op_spec::Op::Probe(v1::ProbeSpec {
                input: Some(v1::InputSource::from(probe.input())),
            }),
            OpSpec::Transcode(transcode) => v1::op_spec::Op::Transcode(v1::TranscodeSpec {
                input: Some(v1::InputSource::from(transcode.input())),
                profile: Some(v1::OutputProfile::from(transcode.profile())),
            }),
            OpSpec::Trim(trim) => v1::op_spec::Op::Trim(v1::TrimSpec {
                input: Some(v1::InputSource::from(trim.input())),
                start_seconds: trim.start_seconds(),
                duration_seconds: trim.duration_seconds(),
            }),
            OpSpec::Thumbnail(thumbnail) => v1::op_spec::Op::Thumbnail(v1::ThumbnailSpec {
                input: Some(v1::InputSource::from(thumbnail.input())),
                offsets_seconds: thumbnail.offsets_seconds(),
                width: thumbnail.width(),
                height: thumbnail.height(),
                sprite_columns: thumbnail.sprite_columns(),
            }),
            OpSpec::Package(package) => v1::op_spec::Op::Package(v1::PackageSpec {
                input: Some(v1::InputSource::from(package.input())),
                segment_seconds: package.segment_seconds(),
            }),
            OpSpec::Image(image) => v1::op_spec::Op::Image(v1::ImageSpec {
                inputs: image.inputs().iter().map(v1::InputSource::from).collect(),
                op: image_op_to_raw(image.op()),
                width: image.width(),
                height: image.height(),
                resize_mode: resize_mode_to_raw(image.mode()),
                format: image_format_to_raw(image.declared_format()),
            }),
            OpSpec::Batch(batch) => v1::op_spec::Op::Batch(v1::BatchSpec::from(batch)),
            OpSpec::Graph(graph) => v1::op_spec::Op::Graph(v1::GraphSpec::from(graph)),
            OpSpec::Ladder(ladder) => v1::op_spec::Op::Ladder(v1::LadderSpec::from(ladder)),
            OpSpec::Loudness(loudness) => {
                v1::op_spec::Op::Loudness(v1::LoudnessSpec::from(loudness))
            }
        };
        v1::OpSpec { op: Some(op) }
    }
}

impl TryFrom<v1::LoudnessResult> for LoudnessResult {
    type Error = OpResultError;

    fn try_from(value: v1::LoudnessResult) -> Result<Self, Self::Error> {
        LoudnessResult::new(
            optional_ref(value.output)?,
            value.integrated_lufs,
            value.true_peak_db,
            value.loudness_range_lu,
        )
    }
}

impl From<&LoudnessResult> for v1::LoudnessResult {
    fn from(value: &LoudnessResult) -> Self {
        Self {
            output: value.output().map(v1::MediaRef::from),
            integrated_lufs: value.integrated_lufs(),
            true_peak_db: value.true_peak_db(),
            loudness_range_lu: value.loudness_range_lu(),
        }
    }
}

impl TryFrom<v1::OpResult> for OpResult {
    type Error = OpResultError;

    fn try_from(value: v1::OpResult) -> Result<Self, Self::Error> {
        match value.op.ok_or(OpResultError::MissingOp)? {
            v1::op_result::Op::Concat(concat) => Ok(OpResult::Concat(TimedOutput::new(
                required_ref(concat.output)?,
                concat.duration_seconds,
            )?)),
            v1::op_result::Op::Probe(probe) => {
                let video = probe
                    .video
                    .iter()
                    .map(|stream| {
                        VideoStreamInfo::new(
                            stream.codec.clone(),
                            stream.width,
                            stream.height,
                            stream.frame_rate,
                            stream.rotation,
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let audio = probe
                    .audio
                    .iter()
                    .map(|stream| {
                        AudioStreamInfo::new(
                            stream.codec.clone(),
                            stream.sample_rate,
                            stream.channels,
                        )
                    })
                    .collect();
                Ok(OpResult::Probe(
                    ProbeResult::new(
                        probe.duration_seconds,
                        probe.video_streams,
                        probe.audio_streams,
                        probe.container,
                    )?
                    .with_streams(video, audio),
                ))
            }
            v1::op_result::Op::Transcode(transcode) => Ok(OpResult::Transcode(
                TranscodeResult::new(required_ref(transcode.output)?),
            )),
            v1::op_result::Op::Trim(trim) => Ok(OpResult::Trim(TimedOutput::new(
                required_ref(trim.output)?,
                trim.duration_seconds,
            )?)),
            v1::op_result::Op::Thumbnail(thumbnail) => Ok(OpResult::Thumbnail(
                ThumbnailResult::new(ref_list(thumbnail.outputs)?)?,
            )),
            v1::op_result::Op::Image(image) => {
                Ok(OpResult::Image(ImageResult::new(ref_list(image.outputs)?)?))
            }
            v1::op_result::Op::Package(package) => Ok(OpResult::Package(PackageResult::new(
                required_ref(package.playlist)?,
                ref_list(package.segments)?,
                optional_ref(package.init_segment)?,
            )?)),
            v1::op_result::Op::Batch(batch) => Ok(OpResult::Batch(BatchResult::try_from(batch)?)),
            v1::op_result::Op::Graph(graph) => Ok(OpResult::Graph(GraphResult::try_from(graph)?)),
            v1::op_result::Op::Ladder(ladder) => {
                Ok(OpResult::Ladder(LadderResult::try_from(ladder)?))
            }
            v1::op_result::Op::Loudness(loudness) => {
                Ok(OpResult::Loudness(LoudnessResult::try_from(loudness)?))
            }
        }
    }
}

impl TryFrom<v1::RenditionResult> for RenditionResult {
    type Error = OpResultError;

    fn try_from(value: v1::RenditionResult) -> Result<Self, Self::Error> {
        let profile = value.profile.ok_or(OpResultError::MissingOp)?;
        let media = required_ref(value.media)?;
        let playlist = match value.playlist {
            Some(playlist) => Some(playlist.try_into()?),
            None => None,
        };
        let init_segment = optional_ref(value.init_segment)?;
        let rendition = RenditionResult::new(
            OutputProfile::try_from(profile)?,
            media,
            playlist,
            ref_list(value.segments)?,
            value.duration_seconds,
        )?
        .with_measurement(value.width, value.height, value.bandwidth_bps);
        Ok(match init_segment {
            Some(init_segment) => rendition.with_init_segment(init_segment),
            None => rendition,
        })
    }
}

impl From<&RenditionResult> for v1::RenditionResult {
    fn from(value: &RenditionResult) -> Self {
        Self {
            profile: Some(v1::OutputProfile::from(value.profile())),
            media: Some(v1::MediaRef::from(value.media())),
            playlist: value.playlist().map(v1::MediaRef::from),
            segments: value.segments().iter().map(v1::MediaRef::from).collect(),
            init_segment: value.init_segment().map(v1::MediaRef::from),
            duration_seconds: value.duration_seconds(),
            width: value.width(),
            height: value.height(),
            bandwidth_bps: value.bandwidth_bps(),
        }
    }
}

impl TryFrom<v1::LadderResult> for LadderResult {
    type Error = OpResultError;

    fn try_from(value: v1::LadderResult) -> Result<Self, Self::Error> {
        let master_playlist = match value.master_playlist {
            Some(playlist) => Some(playlist.try_into()?),
            None => None,
        };
        let renditions = value
            .renditions
            .into_iter()
            .map(RenditionResult::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        LadderResult::new(master_playlist, renditions)
    }
}

impl From<&LadderResult> for v1::LadderResult {
    fn from(value: &LadderResult) -> Self {
        Self {
            master_playlist: value.master_playlist().map(v1::MediaRef::from),
            renditions: value
                .renditions()
                .iter()
                .map(v1::RenditionResult::from)
                .collect(),
        }
    }
}

impl TryFrom<v1::GraphResult> for GraphResult {
    type Error = OpResultError;

    fn try_from(value: v1::GraphResult) -> Result<Self, Self::Error> {
        let nodes = value
            .nodes
            .iter()
            .map(NodeOutcome::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        GraphResult::new(nodes)
    }
}

impl From<&GraphResult> for v1::GraphResult {
    fn from(value: &GraphResult) -> Self {
        Self {
            nodes: value.nodes().iter().map(v1::NodeOutcome::from).collect(),
        }
    }
}

impl TryFrom<&v1::NodeOutcome> for NodeOutcome {
    type Error = OpResultError;

    fn try_from(value: &v1::NodeOutcome) -> Result<Self, Self::Error> {
        let state = match &value.outcome {
            Some(v1::node_outcome::Outcome::Result(result)) => {
                StepState::Result(OpResult::try_from(result.clone())?)
            }
            Some(v1::node_outcome::Outcome::Failure(failure)) => {
                StepState::Failure(JobFailure::try_from(failure)?)
            }
            None => return Err(OpResultError::MissingOp),
        };
        let id = value
            .id
            .parse::<NodeId>()
            .map_err(|source| OpResultError::InvalidNodeId {
                id: value.id.clone(),
                source,
            })?;
        Ok(NodeOutcome::new(id, state))
    }
}

impl From<&NodeOutcome> for v1::NodeOutcome {
    fn from(value: &NodeOutcome) -> Self {
        let outcome = match value.state() {
            StepState::Result(result) => {
                v1::node_outcome::Outcome::Result(v1::OpResult::from(result))
            }
            StepState::Failure(failure) => {
                v1::node_outcome::Outcome::Failure(v1::StepFailure::from(failure))
            }
        };
        Self {
            id: value.id().as_str().to_string(),
            outcome: Some(outcome),
        }
    }
}

impl TryFrom<v1::BatchResult> for BatchResult {
    type Error = OpResultError;

    fn try_from(value: v1::BatchResult) -> Result<Self, Self::Error> {
        let steps = value
            .steps
            .iter()
            .map(StepOutcome::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        BatchResult::new(steps)
    }
}

impl From<&BatchResult> for v1::BatchResult {
    fn from(value: &BatchResult) -> Self {
        Self {
            steps: value.steps().iter().map(v1::StepOutcome::from).collect(),
        }
    }
}

impl TryFrom<&v1::StepOutcome> for StepOutcome {
    type Error = OpResultError;

    fn try_from(value: &v1::StepOutcome) -> Result<Self, Self::Error> {
        let state = match &value.outcome {
            Some(v1::step_outcome::Outcome::Result(result)) => {
                StepState::Result(OpResult::try_from(result.clone())?)
            }
            Some(v1::step_outcome::Outcome::Failure(failure)) => {
                StepState::Failure(JobFailure::try_from(failure)?)
            }
            None => return Err(OpResultError::MissingOp),
        };
        Ok(StepOutcome::new(value.position, state))
    }
}

impl From<&StepOutcome> for v1::StepOutcome {
    fn from(value: &StepOutcome) -> Self {
        let outcome = match value.state() {
            StepState::Result(result) => {
                v1::step_outcome::Outcome::Result(v1::OpResult::from(result))
            }
            StepState::Failure(failure) => {
                v1::step_outcome::Outcome::Failure(v1::StepFailure::from(failure))
            }
        };
        Self {
            position: value.position(),
            outcome: Some(outcome),
        }
    }
}

impl From<&JobFailure> for v1::StepFailure {
    fn from(value: &JobFailure) -> Self {
        Self {
            code: failure_code(value).to_string(),
            message: value.message().to_string(),
            file_errors: value.inputs().iter().map(input_failure).collect(),
        }
    }
}

impl TryFrom<&v1::StepFailure> for JobFailure {
    type Error = JobFailureError;

    fn try_from(value: &v1::StepFailure) -> Result<Self, Self::Error> {
        let inputs = value
            .file_errors
            .iter()
            .map(InputFailure::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(JobFailure::new(&value.code, &value.message)?.with_inputs(inputs))
    }
}

impl From<JobState> for v1::JobState {
    fn from(value: JobState) -> Self {
        match value {
            JobState::Pending => v1::JobState::Pending,
            JobState::Running => v1::JobState::Running,
            JobState::Succeeded => v1::JobState::Succeeded,
            JobState::Failed => v1::JobState::Failed,
            JobState::Cancelled => v1::JobState::Cancelled,
        }
    }
}

fn spec_inputs(spec: &OpSpec) -> Vec<Option<&MediaRef>> {
    spec.inputs()
        .iter()
        .map(|input| input.media().map(MediaSource::media))
        .collect()
}

fn position(index: usize) -> u32 {
    u32::try_from(index).unwrap_or(u32::MAX)
}

fn input_failure(failure: &InputFailure) -> v1::FileError {
    v1::FileError {
        media_ref: None,
        code: verdict_code(failure.code()).to_string(),
        step_index: failure.step().map(position),
        node_id: failure.node().map(|node| node.as_str().to_string()),
        input_index: failure.input().map(position),
        rung: failure.rung().map(position),
    }
}

fn file_errors(record: &JobRecord) -> Vec<v1::FileError> {
    let Some(failure) = &record.failure else {
        return Vec::new();
    };
    let inputs = spec_inputs(&record.spec);
    failure
        .inputs()
        .iter()
        .map(|failure| {
            let mut proto = input_failure(failure);
            proto.media_ref = failure
                .input()
                .and_then(|position| inputs.get(position))
                .and_then(|input| *input)
                .map(v1::MediaRef::from);
            proto
        })
        .collect()
}

impl TryFrom<&v1::FileError> for InputFailure {
    type Error = InputFailureError;

    fn try_from(value: &v1::FileError) -> Result<Self, Self::Error> {
        let failure = InputFailure::at_index(
            value.step_index,
            value.node_id.as_deref(),
            value.input_index,
            &value.code,
        )?;
        match value.rung {
            Some(rung) => Ok(failure.with_rung(rung as usize)),
            None => Ok(failure),
        }
    }
}

impl From<&[InputFailure]> for v1::InputFailureList {
    fn from(value: &[InputFailure]) -> Self {
        Self {
            failures: value.iter().map(input_failure).collect(),
        }
    }
}

impl TryFrom<v1::InputFailureList> for Vec<InputFailure> {
    type Error = InputFailureError;

    fn try_from(value: v1::InputFailureList) -> Result<Self, Self::Error> {
        value.failures.iter().map(InputFailure::try_from).collect()
    }
}

impl From<RungState> for v1::RungState {
    fn from(value: RungState) -> Self {
        match value {
            RungState::Pending => v1::RungState::Pending,
            RungState::Running => v1::RungState::Running,
            RungState::Finished => v1::RungState::Finished,
            RungState::Failed => v1::RungState::Failed,
        }
    }
}

impl From<v1::RungState> for RungState {
    fn from(value: v1::RungState) -> Self {
        match value {
            v1::RungState::Running => RungState::Running,
            v1::RungState::Finished => RungState::Finished,
            v1::RungState::Failed => RungState::Failed,
            v1::RungState::Unspecified | v1::RungState::Pending => RungState::Pending,
        }
    }
}

impl From<&RungProgress> for v1::RungEvent {
    fn from(value: &RungProgress) -> Self {
        Self {
            rung: value.rung() as u32,
            state: v1::RungState::from(value.state()) as i32,
            error_code: value.error_code().unwrap_or("").to_string(),
        }
    }
}

impl TryFrom<&v1::RungEvent> for RungProgress {
    type Error = TextIdError;

    fn try_from(value: &v1::RungEvent) -> Result<Self, Self::Error> {
        let state = RungState::from(v1::RungState::try_from(value.state).unwrap_or_default());
        if value.error_code.is_empty() {
            Ok(RungProgress::new(value.rung as usize, state))
        } else {
            RungProgress::failed(value.rung as usize, &value.error_code)
        }
    }
}

impl From<&[RungProgress]> for v1::RungProgressList {
    fn from(value: &[RungProgress]) -> Self {
        Self {
            rungs: value.iter().map(v1::RungEvent::from).collect(),
        }
    }
}

impl TryFrom<v1::RungProgressList> for Vec<RungProgress> {
    type Error = TextIdError;

    fn try_from(value: v1::RungProgressList) -> Result<Self, Self::Error> {
        value.rungs.iter().map(RungProgress::try_from).collect()
    }
}

impl From<&JobRecord> for v1::Job {
    fn from(record: &JobRecord) -> Self {
        v1::Job {
            job_id: record.id.to_string(),
            user_id: record.owner.to_string(),
            request_id: record.request.to_string(),
            state: v1::JobState::from(record.state) as i32,
            spec: Some(v1::OpSpec::from(&record.spec)),
            result: record.result.as_ref().map(v1::OpResult::from),
            file_errors: file_errors(record),
            progress_percent: u32::from(record.progress.value()),
            callback: record.callback.as_ref().map(|queue| v1::Callback {
                queue_url: QueueUrl::as_str(queue).to_string(),
            }),
        }
    }
}

impl From<&JobRecord> for v1::JobOutcome {
    fn from(record: &JobRecord) -> Self {
        v1::JobOutcome {
            job_id: record.id.to_string(),
            request_id: record.request.to_string(),
            state: v1::JobState::from(record.state) as i32,
            result: record.result.as_ref().map(v1::OpResult::from),
            error_code: record
                .failure
                .as_ref()
                .map(|failure| failure_code(failure).to_string())
                .unwrap_or_default(),
            error_message: record
                .failure
                .as_ref()
                .map(|failure| failure.message().to_string())
                .unwrap_or_default(),
            file_errors: file_errors(record),
        }
    }
}

impl From<&JobRecord> for v1::VideoStitchResult {
    fn from(record: &JobRecord) -> Self {
        v1::VideoStitchResult {
            request_id: record.request.to_string(),
            status: stitch_status(record.state) as i32,
            asset_id: None,
            r2_uri: record
                .result
                .as_ref()
                .and_then(OpResult::primary_output)
                .map(MediaRef::to_uri),
            error_code: record
                .failure
                .as_ref()
                .map(|failure| failure_code(failure).to_string()),
            file_errors: file_errors(record),
            duration_ms: record.duration_ms,
        }
    }
}

fn stitch_status(state: JobState) -> v1::StitchStatus {
    match state {
        JobState::Succeeded => v1::StitchStatus::Completed,
        JobState::Failed => v1::StitchStatus::Failed,
        JobState::Cancelled => v1::StitchStatus::Cancelled,
        JobState::Pending | JobState::Running => v1::StitchStatus::Unspecified,
    }
}

impl From<&JobRecord> for v1::JobCallback {
    fn from(record: &JobRecord) -> Self {
        v1::JobCallback {
            outcome: Some(v1::JobOutcome::from(record)),
            stitch: Some(v1::VideoStitchResult::from(record)),
        }
    }
}

impl From<&OpResult> for v1::OpResult {
    fn from(value: &OpResult) -> Self {
        let op = match value {
            OpResult::Concat(concat) => v1::op_result::Op::Concat(v1::ConcatResult {
                output: Some(v1::MediaRef::from(concat.output())),
                duration_seconds: concat.duration_seconds(),
            }),
            OpResult::Probe(probe) => v1::op_result::Op::Probe(v1::ProbeResult {
                duration_seconds: probe.duration_seconds(),
                video_streams: probe.video_streams(),
                audio_streams: probe.audio_streams(),
                container: probe.container().to_string(),
                video: probe
                    .video()
                    .iter()
                    .map(|stream| v1::VideoStreamInfo {
                        codec: stream.codec().to_string(),
                        width: stream.width(),
                        height: stream.height(),
                        frame_rate: stream.frame_rate(),
                        rotation: stream.rotation(),
                    })
                    .collect(),
                audio: probe
                    .audio()
                    .iter()
                    .map(|stream| v1::AudioStreamInfo {
                        codec: stream.codec().to_string(),
                        sample_rate: stream.sample_rate(),
                        channels: stream.channels(),
                    })
                    .collect(),
            }),
            OpResult::Transcode(transcode) => v1::op_result::Op::Transcode(v1::TranscodeResult {
                output: Some(v1::MediaRef::from(transcode.output())),
            }),
            OpResult::Trim(trim) => v1::op_result::Op::Trim(v1::TrimResult {
                output: Some(v1::MediaRef::from(trim.output())),
                duration_seconds: trim.duration_seconds(),
            }),
            OpResult::Thumbnail(thumbnail) => v1::op_result::Op::Thumbnail(v1::ThumbnailResult {
                outputs: thumbnail.outputs().iter().map(v1::MediaRef::from).collect(),
            }),
            OpResult::Package(package) => v1::op_result::Op::Package(v1::PackageResult {
                playlist: Some(v1::MediaRef::from(package.playlist())),
                segments: package.segments().iter().map(v1::MediaRef::from).collect(),
                init_segment: package.init_segment().map(v1::MediaRef::from),
            }),
            OpResult::Image(image) => v1::op_result::Op::Image(v1::ImageResult {
                outputs: image.outputs().iter().map(v1::MediaRef::from).collect(),
            }),
            OpResult::Batch(batch) => v1::op_result::Op::Batch(v1::BatchResult::from(batch)),
            OpResult::Graph(graph) => v1::op_result::Op::Graph(v1::GraphResult::from(graph)),
            OpResult::Ladder(ladder) => v1::op_result::Op::Ladder(v1::LadderResult::from(ladder)),
            OpResult::Loudness(loudness) => {
                v1::op_result::Op::Loudness(v1::LoudnessResult::from(loudness))
            }
        };
        v1::OpResult { op: Some(op) }
    }
}

fn image_op_from_raw(value: i32) -> Result<ImageOp, OpSpecError> {
    match value {
        raw if raw == v1::ImageOp::Resize as i32 => Ok(ImageOp::Resize),
        raw if raw == v1::ImageOp::Convert as i32 => Ok(ImageOp::Convert),
        raw if raw == v1::ImageOp::Composite as i32 => Ok(ImageOp::Composite),
        _ => Err(OpSpecError::MissingOp),
    }
}

fn image_op_to_raw(value: ImageOp) -> i32 {
    match value {
        ImageOp::Resize => v1::ImageOp::Resize as i32,
        ImageOp::Convert => v1::ImageOp::Convert as i32,
        ImageOp::Composite => v1::ImageOp::Composite as i32,
    }
}

fn resize_mode_from_raw(value: i32) -> ResizeMode {
    if value == v1::ResizeMode::Exact as i32 {
        ResizeMode::Exact
    } else if value == v1::ResizeMode::Fill as i32 {
        ResizeMode::Fill
    } else {
        ResizeMode::Fit
    }
}

fn resize_mode_to_raw(value: ResizeMode) -> i32 {
    match value {
        ResizeMode::Fit => v1::ResizeMode::Fit as i32,
        ResizeMode::Exact => v1::ResizeMode::Exact as i32,
        ResizeMode::Fill => v1::ResizeMode::Fill as i32,
    }
}

fn image_format_from_raw(value: i32) -> Option<ImageFormat> {
    if value == v1::ImageFormat::Jpeg as i32 {
        Some(ImageFormat::Jpeg)
    } else if value == v1::ImageFormat::Png as i32 {
        Some(ImageFormat::Png)
    } else {
        None
    }
}

fn image_format_to_raw(value: Option<ImageFormat>) -> i32 {
    match value {
        Some(ImageFormat::Jpeg) => v1::ImageFormat::Jpeg as i32,
        Some(ImageFormat::Png) => v1::ImageFormat::Png as i32,
        None => v1::ImageFormat::Unspecified as i32,
    }
}

impl From<&InputOrder> for v1::InputSequence {
    fn from(value: &InputOrder) -> Self {
        v1::InputSequence {
            sequence: value.sequence(),
        }
    }
}

impl TryFrom<v1::InputSequence> for InputOrder {
    type Error = InputOrderError;

    fn try_from(value: v1::InputSequence) -> Result<Self, Self::Error> {
        InputOrder::new(value.sequence)
    }
}

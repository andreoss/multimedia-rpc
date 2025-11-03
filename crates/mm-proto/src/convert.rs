use crate::v1;
use mm_domain::{
    ConcatSpec, JobRecord, JobState, MediaRef, MediaRefError, OpResult, OpResultError, OpSpec,
    OpSpecError, OutputProfile, OutputProfileError, ProbeResult, ProbeSpec, QueueUrl,
    ThumbnailResult, ThumbnailSpec, TimedOutput, TranscodeResult, TranscodeSpec, TrimSpec,
    parse_rendered_verdicts,
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

fn required_ref(value: Option<v1::MediaRef>) -> Result<MediaRef, MediaRefError> {
    value.ok_or(MediaRefError::Missing)?.try_into()
}

fn ref_list(values: Vec<v1::MediaRef>) -> Result<Vec<MediaRef>, MediaRefError> {
    values.into_iter().map(MediaRef::try_from).collect()
}

impl TryFrom<v1::OutputProfile> for OutputProfile {
    type Error = OutputProfileError;

    fn try_from(value: v1::OutputProfile) -> Result<Self, Self::Error> {
        OutputProfile::from_raw(
            value.frame_rate,
            value.video_max_bitrate,
            value.video_quality,
            value.audio_bitrate,
            value.audio_sample_rate,
            value.width,
            value.height,
        )
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
        }
    }
}

impl TryFrom<v1::OpSpec> for OpSpec {
    type Error = OpSpecError;

    fn try_from(value: v1::OpSpec) -> Result<Self, Self::Error> {
        match value.op.ok_or(OpSpecError::MissingOp)? {
            v1::op_spec::Op::Concat(concat) => {
                let inputs = ref_list(concat.inputs)?;
                let profile = concat.profile.map(OutputProfile::try_from).transpose()?;
                Ok(OpSpec::Concat(ConcatSpec::new(inputs, profile)?))
            }
            v1::op_spec::Op::Probe(probe) => {
                Ok(OpSpec::Probe(ProbeSpec::new(required_ref(probe.input)?)))
            }
            v1::op_spec::Op::Transcode(transcode) => {
                let input = required_ref(transcode.input)?;
                let profile = transcode
                    .profile
                    .ok_or(OpSpecError::EmptyProfile)
                    .and_then(|profile| Ok(OutputProfile::try_from(profile)?))?;
                Ok(OpSpec::Transcode(TranscodeSpec::new(input, profile)?))
            }
            v1::op_spec::Op::Trim(trim) => Ok(OpSpec::Trim(TrimSpec::new(
                required_ref(trim.input)?,
                trim.start_seconds,
                trim.duration_seconds,
            )?)),
            v1::op_spec::Op::Thumbnail(thumbnail) => Ok(OpSpec::Thumbnail(ThumbnailSpec::new(
                required_ref(thumbnail.input)?,
                thumbnail.offsets_seconds,
                thumbnail.width,
                thumbnail.height,
            )?)),
        }
    }
}

impl From<&OpSpec> for v1::OpSpec {
    fn from(value: &OpSpec) -> Self {
        let op = match value {
            OpSpec::Concat(concat) => v1::op_spec::Op::Concat(v1::ConcatSpec {
                inputs: concat.inputs().iter().map(v1::MediaRef::from).collect(),
                profile: concat.profile().map(v1::OutputProfile::from),
            }),
            OpSpec::Probe(probe) => v1::op_spec::Op::Probe(v1::ProbeSpec {
                input: Some(v1::MediaRef::from(probe.input())),
            }),
            OpSpec::Transcode(transcode) => v1::op_spec::Op::Transcode(v1::TranscodeSpec {
                input: Some(v1::MediaRef::from(transcode.input())),
                profile: Some(v1::OutputProfile::from(transcode.profile())),
            }),
            OpSpec::Trim(trim) => v1::op_spec::Op::Trim(v1::TrimSpec {
                input: Some(v1::MediaRef::from(trim.input())),
                start_seconds: trim.start_seconds(),
                duration_seconds: trim.duration_seconds(),
            }),
            OpSpec::Thumbnail(thumbnail) => v1::op_spec::Op::Thumbnail(v1::ThumbnailSpec {
                input: Some(v1::MediaRef::from(thumbnail.input())),
                offsets_seconds: thumbnail.offsets_seconds().to_vec(),
                width: thumbnail.width(),
                height: thumbnail.height(),
            }),
        };
        v1::OpSpec { op: Some(op) }
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
            v1::op_result::Op::Probe(probe) => Ok(OpResult::Probe(ProbeResult::new(
                probe.duration_seconds,
                probe.video_streams,
                probe.audio_streams,
                probe.container,
            )?)),
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
        }
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

fn spec_inputs(spec: &OpSpec) -> Vec<&MediaRef> {
    match spec {
        OpSpec::Concat(concat) => concat.inputs().iter().collect(),
        OpSpec::Probe(probe) => vec![probe.input()],
        OpSpec::Transcode(transcode) => vec![transcode.input()],
        OpSpec::Trim(trim) => vec![trim.input()],
        OpSpec::Thumbnail(thumbnail) => vec![thumbnail.input()],
    }
}

fn file_errors(record: &JobRecord) -> Vec<v1::FileError> {
    let Some(failure) = &record.failure else {
        return Vec::new();
    };
    let inputs = spec_inputs(&record.spec);
    parse_rendered_verdicts(failure.message())
        .into_iter()
        .map(|(index, code)| v1::FileError {
            media_ref: index
                .and_then(|position| inputs.get(position))
                .map(|input| v1::MediaRef::from(*input)),
            code,
        })
        .collect()
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
                .map(|failure| failure.code().to_string())
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
        };
        v1::OpResult { op: Some(op) }
    }
}

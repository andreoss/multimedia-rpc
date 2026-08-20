use mm_domain::{
    ConcatSpec, InputOrder, InputOrderError, JobRecord, JobRequester, JobState, MediaRef,
    MediaRefError, OpSpec, OpSpecError, OutputProfile, OutputProfileError, RequesterId,
    RequesterType, RequesterTypeError, TextIdError,
};

use crate::v1;
use crate::vm::v1::{
    CancelJobResponse, CreateJobRequest, CreateJobResponse, GetJobResponse, InputFile,
    ListUserJobsRequest, ListUserJobsResponse, OutputProfile as MergeProfile, Requester,
    VideoMergeJob,
};

#[derive(Debug, thiserror::Error)]
pub enum MergeJobError {
    #[error("a reference job names no request id")]
    MissingRequestId,
    #[error("a reference job names no requester")]
    MissingRequester,
    #[error("a reference job names fewer than two inputs")]
    TooFewInputs,
    #[error("an input's sequence order is refused: {0}")]
    InputOrder(#[from] InputOrderError),
    #[error("an input's media id is not a media ref this service accepts: {0}")]
    Media(#[from] MediaRefError),
    #[error("a reference spec is refused: {0}")]
    Spec(#[from] OpSpecError),
    #[error("a reference output profile is refused: {0}")]
    Profile(#[from] OutputProfileError),
    #[error("a requester type is not one this service knows: {0}")]
    Requester(#[from] RequesterTypeError),
    #[error("a requester id is not a text id: {0}")]
    RequesterId(#[from] TextIdError),
    #[error("status {value} is not one a settled job can carry")]
    UnsupportedStatus { value: i32 },
}

pub fn requester_kind(value: Requester) -> Result<RequesterType, MergeJobError> {
    match value {
        Requester::Unspecified => Err(MergeJobError::MissingRequester),
        Requester::ChatMessage => Ok(RequesterType::ChatMessage),
        Requester::CanvasExecutionTask => Ok(RequesterType::CanvasExecutionTask),
        Requester::SeriesEpisode => Ok(RequesterType::SeriesEpisode),
        Requester::Studio => Ok(RequesterType::Studio),
    }
}

pub fn requester_value(kind: Option<RequesterType>) -> Requester {
    match kind {
        Some(RequesterType::ChatMessage) => Requester::ChatMessage,
        Some(RequesterType::CanvasExecutionTask) => Requester::CanvasExecutionTask,
        Some(RequesterType::SeriesEpisode) => Requester::SeriesEpisode,
        Some(RequesterType::Studio) => Requester::Studio,
        None => Requester::Unspecified,
    }
}

fn ordered(job: &VideoMergeJob) -> Result<Vec<&InputFile>, MergeJobError> {
    let mut files: Vec<&InputFile> = job.input_files.iter().collect();
    files.sort_by_key(|file| file.sequence_order);
    if files.len() < 2 {
        return Err(MergeJobError::TooFewInputs);
    }
    let mut seen: Vec<i32> = files.iter().map(|file| file.sequence_order).collect();
    seen.dedup();
    if seen.len() != files.len() {
        return Err(InputOrderError::Duplicate {
            position: seen.last().copied().unwrap_or_default(),
        }
        .into());
    }
    Ok(files)
}

pub fn media_refs(job: &VideoMergeJob) -> Result<Vec<MediaRef>, MergeJobError> {
    ordered(job)?
        .into_iter()
        .map(|file| MediaRef::object_uri(&file.media_id))
        .collect::<Result<Vec<_>, _>>()
        .map_err(MergeJobError::Media)
}

pub fn input_order(job: &VideoMergeJob) -> Result<InputOrder, MergeJobError> {
    let files = ordered(job)?;
    let mut sequence: Vec<i32> = files.iter().map(|file| file.sequence_order).collect();
    sequence.sort_unstable();
    InputOrder::for_inputs(&OpSpec::Concat(spec(job)?), sequence).map_err(MergeJobError::InputOrder)
}

pub fn profile(job: &VideoMergeJob) -> Result<Option<OutputProfile>, MergeJobError> {
    let Some(value) = job.output.as_ref() else {
        return Ok(None);
    };
    let built = OutputProfile::from_raw(
        value.frame_rate.unwrap_or_default() as f64,
        value.video_max_bitrate.unwrap_or_default() as u32,
        value.video_qvbr_quality.unwrap_or_default() as u32,
        value.audio_bitrate.unwrap_or_default() as u32,
        value.audio_sample_rate.unwrap_or_default() as u32,
        0,
        0,
    )?;
    Ok((!built.is_empty()).then_some(built))
}

pub fn spec(job: &VideoMergeJob) -> Result<ConcatSpec, MergeJobError> {
    Ok(ConcatSpec::new(media_refs(job)?, profile(job)?)?)
}

pub fn op_spec(job: &VideoMergeJob) -> Result<OpSpec, MergeJobError> {
    Ok(OpSpec::Concat(spec(job)?))
}

pub fn requester(job: &VideoMergeJob) -> Result<JobRequester, MergeJobError> {
    Ok(JobRequester::new(
        RequesterId::parse(&job.request_id)?,
        requester_kind(Requester::try_from(job.requester).unwrap_or(Requester::Unspecified))?,
    ))
}

pub fn request_id(message: &CreateJobRequest) -> Result<&str, MergeJobError> {
    let job = message
        .job
        .as_ref()
        .ok_or(MergeJobError::MissingRequestId)?;
    let candidate = if job.request_id.is_empty() {
        message.idempotency_key.as_str()
    } else {
        job.request_id.as_str()
    };
    if candidate.is_empty() {
        return Err(MergeJobError::MissingRequestId);
    }
    Ok(candidate)
}

pub fn input_files(record: &JobRecord) -> Vec<InputFile> {
    let order = record
        .input_order
        .as_ref()
        .map(InputOrder::sequence)
        .unwrap_or_default();
    record
        .spec
        .inputs()
        .iter()
        .enumerate()
        .map(|(position, input)| InputFile {
            sequence_order: order.get(position).copied().unwrap_or(position as i32),
            media_id: input
                .media()
                .map(|media| media.media().to_uri())
                .unwrap_or_default(),
        })
        .collect()
}

pub fn to_proto(record: &JobRecord) -> VideoMergeJob {
    VideoMergeJob {
        job_id: record.id.to_string(),
        request_id: record.request.to_string(),
        requester: requester_value(record.requester.as_ref().map(JobRequester::kind)) as i32,
        status: stitch_status(record.state).map(|status| status as i32),
        input_files: input_files(record),
        created_at_millis: millis(record.created_at),
        error_code: record
            .failure
            .as_ref()
            .map(|failure| mm_domain::failure_code(failure).to_string()),
        file_errors: file_errors(record),
        output: merge_profile(record),
        r2_uri: record
            .result
            .as_ref()
            .and_then(mm_domain::OpResult::primary_output)
            .map(MediaRef::to_uri),
        ..Default::default()
    }
}

fn merge_profile(record: &JobRecord) -> Option<MergeProfile> {
    let OpSpec::Concat(concat) = &record.spec else {
        return None;
    };
    let profile = concat.profile()?;
    Some(MergeProfile {
        frame_rate: profile.frame_rate().map(|rate| rate as i32),
        video_max_bitrate: profile.video_max_bitrate().map(|bitrate| bitrate as i32),
        video_qvbr_quality: profile.video_quality().map(|quality| quality as i32),
        audio_bitrate: profile.audio_bitrate().map(|bitrate| bitrate as i32),
        audio_sample_rate: profile.audio_sample_rate().map(|rate| rate as i32),
    })
}

fn file_errors(record: &JobRecord) -> Vec<v1::FileError> {
    let Some(failure) = &record.failure else {
        return Vec::new();
    };
    failure
        .inputs()
        .iter()
        .map(|input| v1::FileError {
            media_ref: None,
            code: mm_domain::verdict_code(input.code()).to_string(),
            step_index: input.step().map(|step| step as u32),
            node_id: input.node().map(|node| node.as_str().to_string()),
            input_index: input.input().map(|position| position as u32),
        })
        .collect()
}

pub fn millis(value: std::time::SystemTime) -> i64 {
    value
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

pub fn created_response(record: &JobRecord, created: bool) -> CreateJobResponse {
    CreateJobResponse {
        job_id: record.id.to_string(),
        request_id: record.request.to_string(),
        created_at_millis: millis(record.created_at),
        created,
    }
}

pub fn get_response(record: &JobRecord) -> GetJobResponse {
    GetJobResponse {
        job: Some(to_proto(record)),
    }
}

pub fn cancel_response(record: &JobRecord, accepted: bool) -> CancelJobResponse {
    CancelJobResponse {
        job_id: record.id.to_string(),
        status: stitch_status(record.state).unwrap_or(v1::StitchStatus::Unspecified) as i32,
        cancellation_accepted: accepted,
    }
}

fn stitch_status(state: JobState) -> Option<v1::StitchStatus> {
    match state {
        mm_domain::JobState::Succeeded => Some(v1::StitchStatus::Completed),
        mm_domain::JobState::Failed => Some(v1::StitchStatus::Failed),
        mm_domain::JobState::Cancelled => Some(v1::StitchStatus::Cancelled),
        mm_domain::JobState::Pending | mm_domain::JobState::Running => None,
    }
}

pub fn list_response(jobs: Vec<JobRecord>, next: Option<String>) -> ListUserJobsResponse {
    ListUserJobsResponse {
        jobs: jobs.iter().map(to_proto).collect(),
        next_page_token: next.unwrap_or_default(),
    }
}

pub fn states(message: &ListUserJobsRequest) -> Result<Vec<mm_domain::JobState>, MergeJobError> {
    message
        .statuses
        .iter()
        .map(|value| match v1::StitchStatus::try_from(*value) {
            Ok(v1::StitchStatus::Completed) => Ok(mm_domain::JobState::Succeeded),
            Ok(v1::StitchStatus::Failed) => Ok(mm_domain::JobState::Failed),
            Ok(v1::StitchStatus::Cancelled) => Ok(mm_domain::JobState::Cancelled),
            _ => Err(MergeJobError::UnsupportedStatus { value: *value }),
        })
        .collect()
}

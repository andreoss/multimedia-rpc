use std::num::NonZeroU32;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;

use mm_domain::{
    JobId, JobRecord, JobRepository, JobState, JobStoreError, NewJob, OpSpec, RequestId, UserId,
};
use mm_proto::v1;
use mm_proto::v1::media_job_service_server::MediaJobService;
use tokio_stream::Stream;
use tonic::{Request, Response, Status};

use crate::cursor;

const DEFAULT_PAGE_SIZE: u32 = 20;
const MAX_PAGE_SIZE: u32 = 100;

pub(crate) struct MediaJobs {
    repository: Arc<dyn JobRepository>,
}

impl MediaJobs {
    pub(crate) fn new(repository: Arc<dyn JobRepository>) -> Self {
        Self { repository }
    }

    async fn refetch(&self, id: JobId) -> Result<JobRecord, Status> {
        self.repository
            .get(id)
            .await
            .map_err(store_failure)?
            .ok_or_else(not_found)
    }
}

fn parse_user(value: &str) -> Result<UserId, Status> {
    UserId::from_str(value)
        .map_err(|error| Status::invalid_argument(format!("user_id is invalid: {error}")))
}

fn parse_request_id(value: &str) -> Result<RequestId, Status> {
    RequestId::from_str(value)
        .map_err(|error| Status::invalid_argument(format!("idempotency_key is invalid: {error}")))
}

fn parse_spec(value: Option<v1::OpSpec>) -> Result<OpSpec, Status> {
    let spec = value.ok_or_else(|| Status::invalid_argument("spec is required"))?;
    OpSpec::try_from(spec)
        .map_err(|error| Status::invalid_argument(format!("spec is invalid: {error}")))
}

fn parse_job_id(value: &str) -> Result<JobId, Status> {
    JobId::from_str(value)
        .map_err(|error| Status::invalid_argument(format!("job_id is invalid: {error}")))
}

fn store_failure(error: JobStoreError) -> Status {
    Status::internal(format!("job store failed: {error}"))
}

fn not_found() -> Status {
    Status::not_found("job does not exist")
}

fn parse_states(raw: &[i32]) -> Result<Vec<JobState>, Status> {
    raw.iter()
        .map(|value| match v1::JobState::try_from(*value) {
            Ok(v1::JobState::Pending) => Ok(JobState::Pending),
            Ok(v1::JobState::Running) => Ok(JobState::Running),
            Ok(v1::JobState::Succeeded) => Ok(JobState::Succeeded),
            Ok(v1::JobState::Failed) => Ok(JobState::Failed),
            Ok(v1::JobState::Cancelled) => Ok(JobState::Cancelled),
            Ok(v1::JobState::Unspecified) | Err(_) => Err(Status::invalid_argument(format!(
                "statuses contains an unknown job state: {value}"
            ))),
        })
        .collect()
}

fn page_limit(requested: u32) -> NonZeroU32 {
    let size = if requested == 0 {
        DEFAULT_PAGE_SIZE
    } else {
        requested.min(MAX_PAGE_SIZE)
    };
    NonZeroU32::new(size).expect("page size is nonzero")
}

fn parse_page_token(token: &str) -> Result<Option<mm_domain::PageCursor>, Status> {
    if token.is_empty() {
        return Ok(None);
    }
    cursor::decode(token)
        .map(Some)
        .map_err(|error| Status::invalid_argument(error.to_string()))
}

fn state_to_proto(state: JobState) -> v1::JobState {
    match state {
        JobState::Pending => v1::JobState::Pending,
        JobState::Running => v1::JobState::Running,
        JobState::Succeeded => v1::JobState::Succeeded,
        JobState::Failed => v1::JobState::Failed,
        JobState::Cancelled => v1::JobState::Cancelled,
    }
}

fn job_to_proto(record: &JobRecord) -> v1::Job {
    v1::Job {
        job_id: record.id.to_string(),
        user_id: record.owner.to_string(),
        request_id: record.request.to_string(),
        state: state_to_proto(record.state) as i32,
        spec: Some(v1::OpSpec::from(&record.spec)),
        result: record.result.as_ref().map(v1::OpResult::from),
        file_errors: Vec::new(),
        progress_percent: u32::from(record.progress.value()),
        callback: None,
    }
}

#[tonic::async_trait]
impl MediaJobService for MediaJobs {
    async fn create_job(
        &self,
        request: Request<v1::CreateJobRequest>,
    ) -> Result<Response<v1::CreateJobResponse>, Status> {
        let message = request.into_inner();
        let owner = parse_user(&message.user_id)?;
        let request_id = parse_request_id(&message.idempotency_key)?;
        let spec = parse_spec(message.spec)?;
        let job = NewJob::new(JobId::generate(), owner.clone(), request_id, spec, None);
        match self.repository.create(job).await {
            Ok(record) => Ok(Response::new(v1::CreateJobResponse {
                job_id: record.id.to_string(),
                request_id: record.request.to_string(),
                created: true,
            })),
            Err(JobStoreError::AlreadyExists { existing }) => {
                let record = self
                    .repository
                    .get(existing)
                    .await
                    .map_err(store_failure)?
                    .ok_or_else(|| Status::internal("replayed job disappeared"))?;
                if record.owner != owner {
                    return Err(Status::already_exists(
                        "idempotency key is already in use by another owner",
                    ));
                }
                Ok(Response::new(v1::CreateJobResponse {
                    job_id: record.id.to_string(),
                    request_id: record.request.to_string(),
                    created: false,
                }))
            }
            Err(error) => Err(store_failure(error)),
        }
    }

    async fn get_job(
        &self,
        request: Request<v1::GetJobRequest>,
    ) -> Result<Response<v1::GetJobResponse>, Status> {
        let message = request.into_inner();
        let owner = parse_user(&message.user_id)?;
        let id = parse_job_id(&message.job_id)?;
        let record = self
            .repository
            .get_owned(&owner, id)
            .await
            .map_err(store_failure)?
            .ok_or_else(not_found)?;
        Ok(Response::new(v1::GetJobResponse {
            job: Some(job_to_proto(&record)),
        }))
    }

    async fn cancel_job(
        &self,
        request: Request<v1::CancelJobRequest>,
    ) -> Result<Response<v1::CancelJobResponse>, Status> {
        let message = request.into_inner();
        let owner = parse_user(&message.user_id)?;
        let id = parse_job_id(&message.job_id)?;
        let record = self
            .repository
            .get_owned(&owner, id)
            .await
            .map_err(store_failure)?
            .ok_or_else(not_found)?;
        let (record, accepted) = match record.state {
            JobState::Pending => match self.repository.mark_cancelled(id).await {
                Ok(cancelled) => (cancelled, true),
                Err(JobStoreError::InvalidTransition { .. }) => (self.refetch(id).await?, false),
                Err(error) => return Err(store_failure(error)),
            },
            JobState::Running => match self.repository.request_cancel(id).await {
                Ok(()) => (self.refetch(id).await?, true),
                Err(JobStoreError::InvalidTransition { .. }) => (self.refetch(id).await?, false),
                Err(error) => return Err(store_failure(error)),
            },
            JobState::Succeeded | JobState::Failed | JobState::Cancelled => (record, false),
        };
        Ok(Response::new(v1::CancelJobResponse {
            job: Some(job_to_proto(&record)),
            cancellation_accepted: accepted,
        }))
    }

    async fn list_jobs(
        &self,
        request: Request<v1::ListJobsRequest>,
    ) -> Result<Response<v1::ListJobsResponse>, Status> {
        let message = request.into_inner();
        let owner = parse_user(&message.user_id)?;
        let states = parse_states(&message.statuses)?;
        let limit = page_limit(message.page_size);
        let cursor = parse_page_token(&message.page_token)?;
        let page = self
            .repository
            .list(&owner, &states, limit, cursor)
            .await
            .map_err(store_failure)?;
        Ok(Response::new(v1::ListJobsResponse {
            jobs: page.jobs.iter().map(job_to_proto).collect(),
            next_page_token: page.next.as_ref().map(cursor::encode).unwrap_or_default(),
        }))
    }

    type WatchJobStream =
        Pin<Box<dyn Stream<Item = Result<v1::WatchJobResponse, Status>> + Send + 'static>>;

    async fn watch_job(
        &self,
        _request: Request<v1::WatchJobRequest>,
    ) -> Result<Response<Self::WatchJobStream>, Status> {
        Err(Status::unimplemented(
            "WatchJob arrives with a later sprint",
        ))
    }
}

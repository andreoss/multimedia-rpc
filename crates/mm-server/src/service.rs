use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;

use mm_domain::{JobId, JobRepository, JobStoreError, NewJob, OpSpec, RequestId, UserId};
use mm_proto::v1;
use mm_proto::v1::media_job_service_server::MediaJobService;
use tokio_stream::Stream;
use tonic::{Request, Response, Status};

pub(crate) struct MediaJobs {
    repository: Arc<dyn JobRepository>,
}

impl MediaJobs {
    pub(crate) fn new(repository: Arc<dyn JobRepository>) -> Self {
        Self { repository }
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

fn store_failure(error: JobStoreError) -> Status {
    Status::internal(format!("job store failed: {error}"))
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
        _request: Request<v1::GetJobRequest>,
    ) -> Result<Response<v1::GetJobResponse>, Status> {
        Err(Status::unimplemented("GetJob is not implemented yet"))
    }

    async fn cancel_job(
        &self,
        _request: Request<v1::CancelJobRequest>,
    ) -> Result<Response<v1::CancelJobResponse>, Status> {
        Err(Status::unimplemented("CancelJob is not implemented yet"))
    }

    async fn list_jobs(
        &self,
        _request: Request<v1::ListJobsRequest>,
    ) -> Result<Response<v1::ListJobsResponse>, Status> {
        Err(Status::unimplemented("ListJobs is not implemented yet"))
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

use std::num::NonZeroU32;
use std::str::FromStr;
use std::sync::Arc;

use mm_domain::{
    JobId, JobRecord, JobRepository, JobState, JobStoreError, MediaRef, NewJob, ObjectStore,
    ObjectStoreError, OpSpec, QueueUrl, RequestId,
};
use mm_proto::merge::{self, MergeJobError};
use mm_proto::vm::v1::{
    CancelJobRequest, CancelJobResponse, CreateJobRequest, CreateJobResponse, GetJobRequest,
    GetJobResponse, ListUserJobsRequest, ListUserJobsResponse, Requester,
    video_merge_service_server::VideoMergeService,
};
use tonic::{Request, Response, Status};

use crate::cursor;

const DEFAULT_PAGE_SIZE: u32 = 20;
const MAX_PAGE_SIZE: u32 = 100;

pub(crate) struct MergeJobs {
    repository: Arc<dyn JobRepository>,
    store: Arc<dyn ObjectStore>,
}

impl MergeJobs {
    pub(crate) fn new(repository: Arc<dyn JobRepository>, store: Arc<dyn ObjectStore>) -> Self {
        Self { repository, store }
    }
}

#[tonic::async_trait]
impl VideoMergeService for MergeJobs {
    async fn create_job(
        &self,
        request: Request<CreateJobRequest>,
    ) -> Result<Response<CreateJobResponse>, Status> {
        let owner = crate::interceptor::owner_of(&request)?;
        let message = request.into_inner();
        let job = message
            .job
            .clone()
            .ok_or_else(|| Status::invalid_argument("a reference create names no job"))?;
        let request_id = RequestId::from_str(merge::request_id(&message).map_err(invalid_merge)?)
            .map_err(invalid)?;
        let spec = merge::op_spec(&job).map_err(invalid_merge)?;
        let order = merge::input_order(&job).map_err(invalid_merge)?;
        let requester = merge::requester(&job).map_err(invalid_merge)?;
        self.ensure_media(&spec).await?;

        let mut new_job = NewJob::new(JobId::generate(), owner.clone(), request_id, spec, None)
            .with_requester(requester.id().clone(), requester.kind())
            .with_input_order(order);
        if let Some(callback) = message
            .callback
            .as_ref()
            .map(|callback| callback.response_queue_url.as_str())
            .map(QueueUrl::new)
            .transpose()
            .map_err(invalid)?
        {
            new_job = new_job.with_callback(callback);
        }

        match self.repository.create(new_job).await {
            Ok(record) => Ok(Response::new(merge::created_response(&record, true))),
            Err(JobStoreError::AlreadyExists { existing }) => {
                let record = self
                    .repository
                    .get(existing)
                    .await
                    .map_err(store)?
                    .ok_or_else(|| Status::internal("the replayed job disappeared"))?;
                if record.owner != owner {
                    return Err(Status::already_exists(
                        "the request id is already in use by another owner",
                    ));
                }
                Ok(Response::new(merge::created_response(&record, false)))
            }
            Err(other) => Err(store(other)),
        }
    }

    async fn cancel_job(
        &self,
        request: Request<CancelJobRequest>,
    ) -> Result<Response<CancelJobResponse>, Status> {
        let owner = crate::interceptor::owner_of(&request)?;
        let message = request.into_inner();
        let record = self.owned(&owner, &message.job_id).await?;
        if matches!(
            record.state,
            JobState::Succeeded | JobState::Failed | JobState::Cancelled
        ) {
            return Ok(Response::new(merge::cancel_response(&record, false)));
        }
        self.repository
            .request_cancel(record.id)
            .await
            .map_err(store)?;
        let current = self
            .repository
            .get(record.id)
            .await
            .map_err(store)?
            .unwrap_or(record);
        Ok(Response::new(merge::cancel_response(&current, true)))
    }

    async fn get_job(
        &self,
        request: Request<GetJobRequest>,
    ) -> Result<Response<GetJobResponse>, Status> {
        let owner = crate::interceptor::owner_of(&request)?;
        let message = request.into_inner();
        let record = self.owned(&owner, &message.job_id).await?;
        Ok(Response::new(merge::get_response(&record)))
    }

    async fn list_user_jobs(
        &self,
        request: Request<ListUserJobsRequest>,
    ) -> Result<Response<ListUserJobsResponse>, Status> {
        let owner = crate::interceptor::owner_of(&request)?;
        let message = request.into_inner();
        if message.page_size < 0 {
            return Err(invalid("page size is negative"));
        }
        let limit = if message.page_size == 0 {
            DEFAULT_PAGE_SIZE
        } else {
            u32::try_from(message.page_size)
                .unwrap_or(MAX_PAGE_SIZE)
                .min(MAX_PAGE_SIZE)
        };
        let token = match message.page_token.as_str() {
            "" => None,
            token => Some(cursor::decode(token).map_err(invalid)?),
        };
        let requester = message.requester.and_then(|r| {
            Requester::try_from(r)
                .ok()
                .and_then(|r| mm_proto::merge::requester_kind(r).ok())
        });
        let page = self
            .repository
            .list(
                &owner,
                &merge::states(&message).map_err(invalid_merge)?,
                NonZeroU32::new(limit).expect("a page size is nonzero"),
                token,
                requester,
            )
            .await
            .map_err(store)?;
        Ok(Response::new(merge::list_response(
            page.jobs,
            page.next.map(|next| cursor::encode(&next)),
        )))
    }
}

impl MergeJobs {
    async fn ensure_media(&self, spec: &OpSpec) -> Result<(), Status> {
        let OpSpec::Concat(concat) = spec else {
            return Ok(());
        };
        for input in concat.inputs() {
            let Some(source) = input.media() else {
                continue;
            };
            let MediaRef::Object(object) = source.media() else {
                continue;
            };
            match self.store.head(object).await {
                Ok(_) => {}
                Err(ObjectStoreError::NotFound { .. }) => {
                    return Err(Status::failed_precondition(format!(
                        "media {} does not exist",
                        object.key()
                    )));
                }
                Err(other) => return Err(Status::internal(other.to_string())),
            }
        }
        Ok(())
    }

    async fn owned(&self, owner: &mm_domain::UserId, raw: &str) -> Result<JobRecord, Status> {
        let id = JobId::from_str(raw).map_err(invalid)?;
        self.repository
            .get_owned(owner, id)
            .await
            .map_err(store)?
            .ok_or_else(|| Status::not_found(format!("job {id} does not exist")))
    }
}

fn store(error: JobStoreError) -> Status {
    match error {
        JobStoreError::AlreadyExists { .. } => Status::already_exists(error.to_string()),
        JobStoreError::NotFound { .. } => Status::not_found(error.to_string()),
        other => Status::internal(other.to_string()),
    }
}

fn invalid(error: impl ToString) -> Status {
    Status::invalid_argument(error.to_string())
}

fn invalid_merge(error: MergeJobError) -> Status {
    Status::invalid_argument(error.to_string())
}

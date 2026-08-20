use std::num::NonZeroU32;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use mm_domain::{
    JobEventNotifications, JobId, JobRecord, JobRepository, JobState, JobStoreError, NewJob,
    OpSpec, ProgressPercent, QueueUrl, RequestId, UserId,
};
use mm_proto::v1;
use mm_proto::v1::media_job_service_server::MediaJobService;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use crate::cursor;

const DEFAULT_PAGE_SIZE: u32 = 20;
const MAX_PAGE_SIZE: u32 = 100;
const WATCH_FANOUT_CAPACITY: usize = 256;
const WATCH_BUFFER: usize = 16;

#[derive(Clone)]
pub(crate) struct WatchHub {
    changes: Option<broadcast::Sender<JobId>>,
    poll: Duration,
}

impl WatchHub {
    pub(crate) fn start(
        events: Option<Arc<dyn JobEventNotifications>>,
        poll: Duration,
    ) -> (Self, Option<JoinHandle<()>>) {
        match events {
            None => (Self { changes: None, poll }, None),
            Some(events) => {
                let (sender, _) = broadcast::channel(WATCH_FANOUT_CAPACITY);
                let feeder = sender.clone();
                let feed = tokio::spawn(async move {
                    loop {
                        if let Ok(id) = events.changed().await {
                            let _ = feeder.send(id);
                        }
                    }
                });
                (
                    Self {
                        changes: Some(sender),
                        poll,
                    },
                    Some(feed),
                )
            }
        }
    }

    fn subscribe(&self) -> Option<broadcast::Receiver<JobId>> {
        self.changes.as_ref().map(|sender| sender.subscribe())
    }
}

pub(crate) struct MediaJobs {
    repository: Arc<dyn JobRepository>,
    watch: WatchHub,
}

impl MediaJobs {
    pub(crate) fn new(repository: Arc<dyn JobRepository>, watch: WatchHub) -> Self {
        Self { repository, watch }
    }

    async fn refetch(&self, id: JobId) -> Result<JobRecord, Status> {
        self.repository
            .get(id)
            .await
            .map_err(store_failure)?
            .ok_or_else(not_found)
    }
}

fn state_event(state: JobState) -> v1::WatchJobResponse {
    v1::WatchJobResponse {
        event: Some(v1::JobEvent {
            kind: Some(v1::job_event::Kind::State(state_to_proto(state) as i32)),
        }),
    }
}

fn progress_event(progress: ProgressPercent) -> v1::WatchJobResponse {
    v1::WatchJobResponse {
        event: Some(v1::JobEvent {
            kind: Some(v1::job_event::Kind::ProgressPercent(u32::from(
                progress.value(),
            ))),
        }),
    }
}

async fn wait_for_change(
    changes: &mut Option<broadcast::Receiver<JobId>>,
    poll: Duration,
    id: JobId,
) {
    let Some(receiver) = changes else {
        tokio::time::sleep(poll).await;
        return;
    };
    let sleep = tokio::time::sleep(poll);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = &mut sleep => return,
            received = receiver.recv() => match received {
                Ok(changed) if changed == id => return,
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(_)) => return,
                Err(broadcast::error::RecvError::Closed) => {
                    (&mut sleep).await;
                    return;
                }
            }
        }
    }
}

async fn follow_job(
    repository: Arc<dyn JobRepository>,
    mut changes: Option<broadcast::Receiver<JobId>>,
    poll: Duration,
    snapshot: JobRecord,
    sink: mpsc::Sender<Result<v1::WatchJobResponse, Status>>,
) {
    let id = snapshot.id;
    let mut state = snapshot.state;
    let mut progress = snapshot.progress;
    if sink.send(Ok(state_event(state))).await.is_err() || state.is_terminal() {
        return;
    }
    if progress > ProgressPercent::ZERO && sink.send(Ok(progress_event(progress))).await.is_err() {
        return;
    }
    loop {
        wait_for_change(&mut changes, poll, id).await;
        let current = match repository.get(id).await {
            Ok(Some(current)) => current,
            Ok(None) => {
                let _ = sink.send(Err(not_found())).await;
                return;
            }
            Err(error) => {
                let _ = sink.send(Err(store_failure(error))).await;
                return;
            }
        };
        if current.state != state
            && !current.state.is_terminal()
            && sink.send(Ok(state_event(current.state))).await.is_err()
        {
            return;
        }
        if current.progress != progress
            && sink.send(Ok(progress_event(current.progress))).await.is_err()
        {
            return;
        }
        if current.state != state && current.state.is_terminal() {
            let _ = sink.send(Ok(state_event(current.state))).await;
            return;
        }
        state = current.state;
        progress = current.progress;
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

fn parse_callback(value: Option<v1::Callback>) -> Result<Option<QueueUrl>, Status> {
    value
        .map(|callback| {
            QueueUrl::new(&callback.queue_url).map_err(|error| {
                Status::invalid_argument(format!("callback queue_url is invalid: {error}"))
            })
        })
        .transpose()
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
        callback: record.callback.as_ref().map(|queue| v1::Callback {
            queue_url: queue.as_str().to_string(),
        }),
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
        let callback = parse_callback(message.callback)?;
        let mut job = NewJob::new(JobId::generate(), owner.clone(), request_id, spec, None);
        if let Some(queue) = callback {
            job = job.with_callback(queue);
        }
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
        request: Request<v1::WatchJobRequest>,
    ) -> Result<Response<Self::WatchJobStream>, Status> {
        let message = request.into_inner();
        let owner = parse_user(&message.user_id)?;
        let id = parse_job_id(&message.job_id)?;
        let changes = self.watch.subscribe();
        let snapshot = self
            .repository
            .get_owned(&owner, id)
            .await
            .map_err(store_failure)?
            .ok_or_else(not_found)?;
        let (sink, events) = mpsc::channel(WATCH_BUFFER);
        tokio::spawn(follow_job(
            self.repository.clone(),
            changes,
            self.watch.poll,
            snapshot,
            sink,
        ));
        Ok(Response::new(Box::pin(ReceiverStream::new(events))))
    }
}

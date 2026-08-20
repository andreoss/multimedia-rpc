use crate::callback::QueueUrl;
use crate::id::{JobId, NodeId, RequestId, TextIdError, TraceId, UserId, WorkerId, parse_text_id};
use crate::op::{OpResult, OpSpec};
use crate::state::JobState;
use crate::validate::Verdict;
use async_trait::async_trait;
use std::num::NonZeroU32;
use std::time::{Duration, SystemTime};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProgressPercent(u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ProgressError {
    #[error("progress {value} exceeds 100")]
    OutOfRange { value: u8 },
}

impl ProgressPercent {
    pub const ZERO: ProgressPercent = ProgressPercent(0);
    pub const COMPLETE: ProgressPercent = ProgressPercent(100);

    pub fn new(value: u8) -> Result<Self, ProgressError> {
        if value > 100 {
            return Err(ProgressError::OutOfRange { value });
        }
        Ok(Self(value))
    }

    pub fn value(self) -> u8 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputFailure {
    step: Option<usize>,
    node: Option<NodeId>,
    input: Option<usize>,
    code: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InputFailureError {
    #[error("input failure code is invalid: {0}")]
    Code(TextIdError),
    #[error("input failure node is invalid: {0}")]
    Node(TextIdError),
    #[error("input failure index {index} is not a position")]
    Index { index: u32 },
}

impl InputFailure {
    pub const CODE_MAX_LENGTH: usize = 64;

    pub fn new(
        step: Option<usize>,
        node: Option<&str>,
        input: Option<usize>,
        code: &str,
    ) -> Result<Self, InputFailureError> {
        Ok(Self {
            step,
            node: node
                .map(str::parse)
                .transpose()
                .map_err(InputFailureError::Node)?,
            input,
            code: parse_text_id(code, Self::CODE_MAX_LENGTH).map_err(InputFailureError::Code)?,
        })
    }

    pub fn at_index(
        step: Option<u32>,
        node: Option<&str>,
        input: Option<u32>,
        code: &str,
    ) -> Result<Self, InputFailureError> {
        Self::new(index(step)?, node, index(input)?, code)
    }

    pub fn step(&self) -> Option<usize> {
        self.step
    }

    pub fn node(&self) -> Option<&NodeId> {
        self.node.as_ref()
    }

    pub fn input(&self) -> Option<usize> {
        self.input
    }

    pub fn code(&self) -> &str {
        &self.code
    }
}

fn index(value: Option<u32>) -> Result<Option<usize>, InputFailureError> {
    value
        .map(|position| {
            usize::try_from(position).map_err(|_| InputFailureError::Index { index: position })
        })
        .transpose()
}

impl From<&Verdict> for InputFailure {
    fn from(verdict: &Verdict) -> Self {
        Self {
            step: None,
            node: None,
            input: verdict.input,
            code: verdict.code.name(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobFailure {
    code: String,
    message: String,
    inputs: Vec<InputFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum JobFailureError {
    #[error("failure code is invalid: {0}")]
    Code(#[from] TextIdError),
    #[error(transparent)]
    Inputs(#[from] InputFailureError),
}

impl JobFailure {
    pub const CODE_MAX_LENGTH: usize = 64;

    pub fn new(code: &str, message: &str) -> Result<Self, JobFailureError> {
        Ok(Self {
            code: parse_text_id(code, Self::CODE_MAX_LENGTH)?,
            message: message.to_string(),
            inputs: Vec::new(),
        })
    }

    pub fn with_inputs(mut self, inputs: Vec<InputFailure>) -> Self {
        self.inputs = inputs;
        self
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn inputs(&self) -> &[InputFailure] {
        &self.inputs
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewJob {
    id: JobId,
    owner: UserId,
    request: RequestId,
    spec: OpSpec,
    deadline_at: Option<SystemTime>,
    callback: Option<QueueUrl>,
    trace_id: Option<TraceId>,
}

impl NewJob {
    pub fn new(
        id: JobId,
        owner: UserId,
        request: RequestId,
        spec: OpSpec,
        deadline_at: Option<SystemTime>,
    ) -> Self {
        Self {
            id,
            owner,
            request,
            spec,
            deadline_at,
            callback: None,
            trace_id: None,
        }
    }

    pub fn with_callback(mut self, callback: QueueUrl) -> Self {
        self.callback = Some(callback);
        self
    }

    pub fn callback(&self) -> Option<&QueueUrl> {
        self.callback.as_ref()
    }

    pub fn with_trace_id(mut self, trace_id: TraceId) -> Self {
        self.trace_id = Some(trace_id);
        self
    }

    pub fn trace_id(&self) -> Option<TraceId> {
        self.trace_id
    }

    pub fn id(&self) -> JobId {
        self.id
    }

    pub fn owner(&self) -> &UserId {
        &self.owner
    }

    pub fn request(&self) -> &RequestId {
        &self.request
    }

    pub fn spec(&self) -> &OpSpec {
        &self.spec
    }

    pub fn deadline_at(&self) -> Option<SystemTime> {
        self.deadline_at
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct JobRecord {
    pub id: JobId,
    pub owner: UserId,
    pub request: RequestId,
    pub spec: OpSpec,
    pub state: JobState,
    pub progress: ProgressPercent,
    pub failure: Option<JobFailure>,
    pub result: Option<OpResult>,
    pub created_at: SystemTime,
    pub started_at: Option<SystemTime>,
    pub finished_at: Option<SystemTime>,
    pub attempts: u32,
    pub next_attempt_at: Option<SystemTime>,
    pub deadline_at: Option<SystemTime>,
    pub claimed_by: Option<WorkerId>,
    pub lease_expires_at: Option<SystemTime>,
    pub cancel_requested: bool,
    pub callback: Option<QueueUrl>,
    pub trace_id: Option<TraceId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageCursor {
    pub created_at: SystemTime,
    pub id: JobId,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JobPage {
    pub jobs: Vec<JobRecord>,
    pub next: Option<PageCursor>,
}

#[derive(Debug, Clone, PartialEq, Error)]
pub enum JobStoreError {
    #[error("a job with this request id already exists: {existing}")]
    AlreadyExists { existing: JobId },
    #[error("job {id} does not exist")]
    NotFound { id: JobId },
    #[error("job {id} cannot move from {from} to {to}")]
    InvalidTransition {
        id: JobId,
        from: JobState,
        to: JobState,
    },
    #[error("job {id} is not leased by worker {worker}")]
    LeaseNotHeld { id: JobId, worker: WorkerId },
    #[error("job {id} record is corrupt: {detail}")]
    CorruptRecord { id: JobId, detail: String },
    #[error("job store backend error: {0}")]
    Backend(String),
}

#[async_trait]
pub trait JobNotifications: Send + Sync {
    async fn notified(&self) -> Result<(), JobStoreError>;
}

#[async_trait]
pub trait CancelNotifications: Send + Sync {
    async fn cancelled(&self) -> Result<JobId, JobStoreError>;
}

#[async_trait]
pub trait JobEventNotifications: Send + Sync {
    async fn changed(&self) -> Result<JobId, JobStoreError>;
}

#[async_trait]
pub trait JobRepository: Send + Sync {
    async fn create(&self, job: NewJob) -> Result<JobRecord, JobStoreError>;
    async fn get(&self, id: JobId) -> Result<Option<JobRecord>, JobStoreError>;
    async fn get_owned(
        &self,
        owner: &UserId,
        id: JobId,
    ) -> Result<Option<JobRecord>, JobStoreError>;
    async fn list(
        &self,
        owner: &UserId,
        states: &[JobState],
        limit: NonZeroU32,
        cursor: Option<PageCursor>,
    ) -> Result<JobPage, JobStoreError>;
    async fn claim(
        &self,
        worker: &WorkerId,
        limit: NonZeroU32,
        lease: Duration,
    ) -> Result<Vec<JobRecord>, JobStoreError>;
    async fn heartbeat(
        &self,
        id: JobId,
        worker: &WorkerId,
        lease: Duration,
    ) -> Result<SystemTime, JobStoreError>;
    async fn requeue_expired(&self) -> Result<Vec<JobId>, JobStoreError>;
    async fn fail_overdue(&self) -> Result<Vec<JobId>, JobStoreError>;
    async fn release(
        &self,
        id: JobId,
        worker: &WorkerId,
        retry_after: Duration,
    ) -> Result<(), JobStoreError>;
    async fn retry(
        &self,
        id: JobId,
        worker: &WorkerId,
        failure: JobFailure,
        retry_after: Duration,
    ) -> Result<(), JobStoreError>;
    async fn set_progress(
        &self,
        id: JobId,
        worker: &WorkerId,
        progress: ProgressPercent,
    ) -> Result<(), JobStoreError>;
    async fn mark_succeeded(
        &self,
        id: JobId,
        worker: &WorkerId,
        result: OpResult,
    ) -> Result<JobRecord, JobStoreError>;
    async fn mark_failed(
        &self,
        id: JobId,
        worker: &WorkerId,
        failure: JobFailure,
    ) -> Result<JobRecord, JobStoreError>;
    async fn mark_cancelled(&self, id: JobId) -> Result<JobRecord, JobStoreError>;
    async fn request_cancel(&self, id: JobId) -> Result<(), JobStoreError>;
    async fn claim_callback(&self, id: JobId) -> Result<bool, JobStoreError>;
    async fn unpublished_callbacks(
        &self,
        limit: NonZeroU32,
    ) -> Result<Vec<JobRecord>, JobStoreError>;
    async fn pending_count(&self) -> Result<u64, JobStoreError>;
}

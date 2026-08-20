use crate::callback::QueueUrl;
use crate::id::{
    JobId, NodeId, RequestId, TemplateId, TextIdError, TraceId, UserId, WorkerId, parse_text_id,
};
use crate::op::{MediaRef, OpResult, OpSpec};
use crate::state::JobState;
use crate::template::Template;
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
    rung: Option<usize>,
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
            rung: None,
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

    pub fn rung(&self) -> Option<usize> {
        self.rung
    }

    pub fn with_rung(mut self, rung: usize) -> Self {
        self.rung = Some(rung);
        self
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
            rung: verdict.rung,
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
    requester: Option<JobRequester>,
    input_order: Option<InputOrder>,
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
            requester: None,
            input_order: None,
        }
    }

    pub fn with_requester(mut self, id: RequesterId, kind: RequesterType) -> Self {
        self.requester = Some(JobRequester::new(id, kind));
        self
    }

    pub fn with_input_order(mut self, order: InputOrder) -> Self {
        self.input_order = Some(order);
        self
    }

    pub fn requester(&self) -> Option<&JobRequester> {
        self.requester.as_ref()
    }

    pub fn input_order(&self) -> Option<&InputOrder> {
        self.input_order.as_ref()
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
    pub requester: Option<JobRequester>,
    pub input_order: Option<InputOrder>,
    pub duration_ms: Option<i64>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputMetadata(Vec<u8>);

impl InputMetadata {
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
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
    #[error("a template named {name:?} already exists for this owner")]
    TemplateNameTaken { name: String },
    #[error("template {id} does not exist")]
    TemplateNotFound { id: TemplateId },
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
    async fn find_by_request(
        &self,
        owner: &UserId,
        request: &RequestId,
    ) -> Result<Option<JobRecord>, JobStoreError>;
    async fn list(
        &self,
        owner: &UserId,
        states: &[JobState],
        limit: NonZeroU32,
        cursor: Option<PageCursor>,
        requester: Option<RequesterType>,
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
        result: Option<OpResult>,
    ) -> Result<JobRecord, JobStoreError>;
    async fn mark_cancelled(&self, id: JobId) -> Result<JobRecord, JobStoreError>;
    async fn request_cancel(&self, id: JobId) -> Result<(), JobStoreError>;
    async fn claim_callback(&self, id: JobId) -> Result<bool, JobStoreError>;
    async fn unpublished_callbacks(
        &self,
        limit: NonZeroU32,
    ) -> Result<Vec<JobRecord>, JobStoreError>;
    async fn pending_count(&self) -> Result<u64, JobStoreError>;
    async fn create_template(&self, template: Template) -> Result<Template, JobStoreError>;
    async fn get_template(
        &self,
        owner: &UserId,
        id: TemplateId,
    ) -> Result<Option<Template>, JobStoreError>;
    async fn list_templates(&self, owner: &UserId) -> Result<Vec<Template>, JobStoreError>;
    async fn delete_template(&self, owner: &UserId, id: TemplateId) -> Result<bool, JobStoreError>;
    async fn input_metadata(
        &self,
        media: &MediaRef,
    ) -> Result<Option<InputMetadata>, JobStoreError>;
    async fn put_input_metadata(
        &self,
        media: &MediaRef,
        metadata: &InputMetadata,
    ) -> Result<(), JobStoreError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequesterType {
    ChatMessage,
    CanvasExecutionTask,
    SeriesEpisode,
    Studio,
}

impl RequesterType {
    pub fn as_str(self) -> &'static str {
        match self {
            RequesterType::ChatMessage => "CHAT_MESSAGE",
            RequesterType::CanvasExecutionTask => "CANVAS_EXECUTION_TASK",
            RequesterType::SeriesEpisode => "SERIES_EPISODE",
            RequesterType::Studio => "STUDIO",
        }
    }

    pub fn parse(value: &str) -> Result<Self, RequesterTypeError> {
        match value {
            "CHAT_MESSAGE" => Ok(RequesterType::ChatMessage),
            "CANVAS_EXECUTION_TASK" => Ok(RequesterType::CanvasExecutionTask),
            "SERIES_EPISODE" => Ok(RequesterType::SeriesEpisode),
            "STUDIO" => Ok(RequesterType::Studio),
            other => Err(RequesterTypeError {
                value: other.to_string(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("requester type {value:?} is not one this service knows")]
pub struct RequesterTypeError {
    value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequesterId(String);

impl RequesterId {
    pub const MAX_LENGTH: usize = 128;

    pub fn parse(value: &str) -> Result<Self, TextIdError> {
        Ok(Self(parse_text_id(value, Self::MAX_LENGTH)?))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRequester {
    id: RequesterId,
    kind: RequesterType,
}

impl JobRequester {
    pub fn new(id: RequesterId, kind: RequesterType) -> Self {
        Self { id, kind }
    }

    pub fn id(&self) -> &RequesterId {
        &self.id
    }

    pub fn kind(&self) -> RequesterType {
        self.kind
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InputOrderError {
    #[error("an input order is empty")]
    Empty,
    #[error("the input order names {position} twice")]
    Duplicate { position: i32 },
    #[error("the input order names {count} inputs and the spec has {expected}")]
    CountMismatch { count: usize, expected: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputOrder {
    sequence: Vec<i32>,
}

impl InputOrder {
    pub fn new(sequence: Vec<i32>) -> Result<Self, InputOrderError> {
        if sequence.is_empty() {
            return Err(InputOrderError::Empty);
        }
        let mut seen: Vec<i32> = sequence.clone();
        seen.sort_unstable();
        seen.dedup();
        if seen.len() != sequence.len() {
            let repeated = sequence
                .iter()
                .find(|position| {
                    sequence
                        .iter()
                        .filter(|other| **other == **position)
                        .count()
                        > 1
                })
                .copied()
                .unwrap_or_default();
            return Err(InputOrderError::Duplicate { position: repeated });
        }
        Ok(Self { sequence })
    }

    pub fn for_inputs(spec: &OpSpec, sequence: Vec<i32>) -> Result<Self, InputOrderError> {
        let order = Self::new(sequence)?;
        let expected = spec.inputs().len();
        if order.sequence.len() != expected {
            return Err(InputOrderError::CountMismatch {
                count: order.sequence.len(),
                expected,
            });
        }
        Ok(order)
    }

    pub fn sequence(&self) -> Vec<i32> {
        self.sequence.clone()
    }
}

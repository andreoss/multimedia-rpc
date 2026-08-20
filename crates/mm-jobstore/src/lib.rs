use std::num::NonZeroU32;
use std::str::FromStr;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use mm_domain::{
    CancelNotifications, InputFailure, InputOrder, JobEventNotifications, JobFailure, JobId,
    JobNotifications, JobPage, JobRecord, JobRepository, JobRequester, JobState, JobStoreError,
    NewJob, OpResult, OpSpec, PageCursor, ParameterDeclaration, ProgressPercent, QueueUrl,
    RequestId, RequesterId, RequesterType, Template, TemplateId, UserId, WorkerId,
};
use prost::Message;
use sqlx::PgPool;
use sqlx::migrate::Migrator;
use sqlx::postgres::PgListener;
use time::OffsetDateTime;
use uuid::Uuid;

pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

pub const JOB_READY_CHANNEL: &str = "mm_job_ready";

const RECONNECT_BACKOFF_BASE: Duration = Duration::from_millis(100);
const RECONNECT_BACKOFF_CAP: Duration = Duration::from_secs(5);

pub fn reconnect_backoff(failures: u32) -> Duration {
    match failures {
        0 => Duration::ZERO,
        _ => RECONNECT_BACKOFF_BASE
            .saturating_mul(2u32.saturating_pow((failures - 1).min(31)))
            .min(RECONNECT_BACKOFF_CAP),
    }
}

struct ListenerState {
    listener: PgListener,
    failures: u32,
}

struct ChannelListener {
    state: tokio::sync::Mutex<ListenerState>,
}

impl ChannelListener {
    async fn connect(url: &str, channel: &str) -> Result<Self, sqlx::Error> {
        let mut listener = PgListener::connect(url).await?;
        listener.listen(channel).await?;
        Ok(Self {
            state: tokio::sync::Mutex::new(ListenerState {
                listener,
                failures: 0,
            }),
        })
    }

    async fn next(&self) -> Result<Option<String>, JobStoreError> {
        let mut state = self.state.lock().await;
        tokio::time::sleep(reconnect_backoff(state.failures)).await;
        match state.listener.try_recv().await {
            Ok(notification) => {
                state.failures = 0;
                Ok(notification.map(|notification| notification.payload().to_string()))
            }
            Err(error) => {
                state.failures = state.failures.saturating_add(1);
                Err(backend(error))
            }
        }
    }
}

pub const JOB_CANCEL_CHANNEL: &str = "mm_job_cancel";

pub struct PgCancelListener {
    inner: ChannelListener,
}

impl PgCancelListener {
    pub async fn connect(url: &str) -> Result<Self, sqlx::Error> {
        Ok(Self {
            inner: ChannelListener::connect(url, JOB_CANCEL_CHANNEL).await?,
        })
    }
}

#[async_trait]
impl CancelNotifications for PgCancelListener {
    async fn cancelled(&self) -> Result<JobId, JobStoreError> {
        loop {
            if let Some(payload) = self.inner.next().await? {
                return Uuid::parse_str(&payload)
                    .map_err(|error| JobStoreError::Backend(error.to_string()))
                    .and_then(|uuid| {
                        JobId::from_uuid(uuid)
                            .map_err(|error| JobStoreError::Backend(error.to_string()))
                    });
            }
        }
    }
}

pub const JOB_EVENTS_CHANNEL: &str = "mm_job_events";

pub struct PgJobEventListener {
    inner: ChannelListener,
}

impl PgJobEventListener {
    pub async fn connect(url: &str) -> Result<Self, sqlx::Error> {
        Ok(Self {
            inner: ChannelListener::connect(url, JOB_EVENTS_CHANNEL).await?,
        })
    }
}

#[async_trait]
impl JobEventNotifications for PgJobEventListener {
    async fn changed(&self) -> Result<JobId, JobStoreError> {
        loop {
            if let Some(payload) = self.inner.next().await? {
                return Uuid::parse_str(&payload)
                    .map_err(|error| JobStoreError::Backend(error.to_string()))
                    .and_then(|uuid| {
                        JobId::from_uuid(uuid)
                            .map_err(|error| JobStoreError::Backend(error.to_string()))
                    });
            }
        }
    }
}

pub struct PgJobListener {
    inner: ChannelListener,
}

impl PgJobListener {
    pub async fn connect(url: &str) -> Result<Self, sqlx::Error> {
        Ok(Self {
            inner: ChannelListener::connect(url, JOB_READY_CHANNEL).await?,
        })
    }
}

#[async_trait]
impl JobNotifications for PgJobListener {
    async fn notified(&self) -> Result<(), JobStoreError> {
        self.inner.next().await.map(|_| ())
    }
}

pub struct PgJobRepository {
    pool: PgPool,
}

impl PgJobRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

struct JobRow {
    id: Uuid,
    user_id: String,
    request_id: String,
    spec: Vec<u8>,
    state: String,
    progress_percent: i16,
    error_code: Option<String>,
    error_message: Option<String>,
    result: Option<Vec<u8>>,
    created_at: OffsetDateTime,
    started_at: Option<OffsetDateTime>,
    finished_at: Option<OffsetDateTime>,
    attempts: i32,
    next_attempt_at: Option<OffsetDateTime>,
    deadline_at: Option<OffsetDateTime>,
    claimed_by: Option<String>,
    lease_expires_at: Option<OffsetDateTime>,
    cancel_requested: bool,
    callback_queue_url: Option<String>,
    trace_id: Option<String>,
    input_failures: Option<Vec<u8>>,
    requester_id: Option<String>,
    requester_type: Option<String>,
    input_sequence: Option<Vec<u8>>,
}

fn backend(error: sqlx::Error) -> JobStoreError {
    JobStoreError::Backend(error.to_string())
}

fn corrupt(id: JobId, detail: impl ToString) -> JobStoreError {
    JobStoreError::CorruptRecord {
        id,
        detail: detail.to_string(),
    }
}

fn encode_spec(spec: &OpSpec) -> Vec<u8> {
    mm_proto::v1::OpSpec::from(spec).encode_to_vec()
}

fn encode_parameters(parameters: &[ParameterDeclaration]) -> Vec<u8> {
    mm_proto::v1::ParameterDeclarations::from(parameters).encode_to_vec()
}

#[derive(Debug)]
struct TemplateRow {
    id: Uuid,
    user_id: String,
    name: String,
    spec: Vec<u8>,
    parameters: Vec<u8>,
}

fn to_template(row: TemplateRow) -> Result<Template, JobStoreError> {
    let spec = mm_proto::v1::OpSpec::decode(&row.spec[..])
        .map_err(|error| JobStoreError::Backend(error.to_string()))?;
    let spec = OpSpec::try_from(spec).map_err(|error| JobStoreError::Backend(error.to_string()))?;
    let declarations = mm_proto::v1::ParameterDeclarations::decode(&row.parameters[..])
        .map_err(|error| JobStoreError::Backend(error.to_string()))?;
    let parameters = Vec::<ParameterDeclaration>::try_from(declarations)
        .map_err(|error| JobStoreError::Backend(error.to_string()))?;
    Template::from_parts(
        TemplateId::from_uuid(row.id).map_err(|error| JobStoreError::Backend(error.to_string()))?,
        UserId::from_str(&row.user_id)
            .map_err(|error| JobStoreError::Backend(error.to_string()))?,
        &row.name,
        spec,
        parameters,
    )
    .map_err(|error| JobStoreError::Backend(error.to_string()))
}

fn encode_result(result: &OpResult) -> Vec<u8> {
    mm_proto::v1::OpResult::from(result).encode_to_vec()
}

fn encode_input_order(order: &InputOrder) -> Vec<u8> {
    mm_proto::v1::InputSequence::from(order).encode_to_vec()
}

fn decode_input_order(id: JobId, bytes: &[u8]) -> Result<InputOrder, JobStoreError> {
    mm_proto::v1::InputSequence::decode(bytes)
        .map_err(|error| corrupt(id, error))
        .and_then(|sequence| {
            InputOrder::try_from(sequence).map_err(|error| corrupt(id, error.to_string()))
        })
}

fn requester(
    id: JobId,
    requester_id: Option<String>,
    requester_type: Option<String>,
) -> Result<Option<JobRequester>, JobStoreError> {
    match (requester_id, requester_type) {
        (Some(who), Some(kind)) => Ok(Some(JobRequester::new(
            RequesterId::parse(&who).map_err(|error| corrupt(id, error))?,
            RequesterType::parse(&kind).map_err(|error| corrupt(id, error))?,
        ))),
        (None, None) => Ok(None),
        _ => Err(corrupt(id, "a requester carries a type or neither")),
    }
}

fn encode_input_failures(inputs: &[InputFailure]) -> Vec<u8> {
    mm_proto::v1::InputFailureList::from(inputs).encode_to_vec()
}

fn decode_input_failures(id: JobId, bytes: &[u8]) -> Result<Vec<InputFailure>, JobStoreError> {
    mm_proto::v1::InputFailureList::decode(bytes)
        .map_err(|error| corrupt(id, error))
        .and_then(|list| Vec::<InputFailure>::try_from(list).map_err(|error| corrupt(id, error)))
}

fn to_system(value: OffsetDateTime) -> SystemTime {
    value.into()
}

fn to_offset(value: SystemTime) -> OffsetDateTime {
    value.into()
}

fn to_record(row: JobRow) -> Result<JobRecord, JobStoreError> {
    let id = JobId::from_uuid(row.id).map_err(|error| JobStoreError::CorruptRecord {
        id: JobId::generate(),
        detail: error.to_string(),
    })?;
    let spec = mm_proto::v1::OpSpec::decode(row.spec.as_slice())
        .map_err(|error| corrupt(id, error))
        .and_then(|proto| OpSpec::try_from(proto).map_err(|error| corrupt(id, error)))?;
    let result = row
        .result
        .as_deref()
        .map(|bytes| {
            mm_proto::v1::OpResult::decode(bytes)
                .map_err(|error| corrupt(id, error))
                .and_then(|proto| OpResult::try_from(proto).map_err(|error| corrupt(id, error)))
        })
        .transpose()?;
    let failure = match (row.error_code, row.error_message) {
        (Some(code), message) => Some(
            JobFailure::new(&code, message.as_deref().unwrap_or(""))
                .map_err(|error| corrupt(id, error))?
                .with_inputs(
                    row.input_failures
                        .as_deref()
                        .map(|bytes| decode_input_failures(id, bytes))
                        .transpose()?
                        .unwrap_or_default(),
                ),
        ),
        (None, _) => None,
    };
    let progress = u8::try_from(row.progress_percent)
        .map_err(|error| corrupt(id, error))
        .and_then(|value| ProgressPercent::new(value).map_err(|error| corrupt(id, error)))?;
    Ok(JobRecord {
        id,
        owner: UserId::from_str(&row.user_id).map_err(|error| corrupt(id, error))?,
        request: RequestId::from_str(&row.request_id).map_err(|error| corrupt(id, error))?,
        spec,
        state: JobState::from_str(&row.state).map_err(|error| corrupt(id, error))?,
        progress,
        failure,
        result,
        created_at: to_system(row.created_at),
        started_at: row.started_at.map(to_system),
        finished_at: row.finished_at.map(to_system),
        attempts: u32::try_from(row.attempts).map_err(|error| corrupt(id, error))?,
        next_attempt_at: row.next_attempt_at.map(to_system),
        deadline_at: row.deadline_at.map(to_system),
        claimed_by: row
            .claimed_by
            .as_deref()
            .map(WorkerId::from_str)
            .transpose()
            .map_err(|error| corrupt(id, error))?,
        lease_expires_at: row.lease_expires_at.map(to_system),
        cancel_requested: row.cancel_requested,
        callback: row
            .callback_queue_url
            .as_deref()
            .map(QueueUrl::new)
            .transpose()
            .map_err(|error| corrupt(id, error))?,
        trace_id: row
            .trace_id
            .as_deref()
            .map(str::parse)
            .transpose()
            .map_err(|error| corrupt(id, error))?,
        requester: requester(id, row.requester_id, row.requester_type)?,
        input_order: row
            .input_sequence
            .as_deref()
            .map(|bytes| decode_input_order(id, bytes))
            .transpose()?,
    })
}

impl PgJobRepository {
    async fn transition_conflict(
        &self,
        id: JobId,
        to: JobState,
        claimant: Option<&WorkerId>,
    ) -> JobStoreError {
        let row = match sqlx::query!(
            "SELECT state, claimed_by FROM jobs WHERE id = $1",
            id.as_uuid()
        )
        .fetch_optional(&self.pool)
        .await
        {
            Ok(row) => row,
            Err(error) => return backend(error),
        };
        let Some(row) = row else {
            return JobStoreError::NotFound { id };
        };
        let from = match JobState::from_str(&row.state) {
            Ok(state) => state,
            Err(error) => return corrupt(id, error),
        };
        if let Some(worker) = claimant
            && from == JobState::Running
            && row.claimed_by.as_deref() != Some(worker.as_str())
        {
            return JobStoreError::LeaseNotHeld {
                id,
                worker: worker.clone(),
            };
        }
        JobStoreError::InvalidTransition { id, from, to }
    }
}

#[async_trait]
impl JobRepository for PgJobRepository {
    async fn create(&self, job: NewJob) -> Result<JobRecord, JobStoreError> {
        let inserted = sqlx::query_as!(
            JobRow,
            "INSERT INTO jobs (id, user_id, request_id, spec, deadline_at, \
             callback_queue_url, trace_id, requester_id, requester_type, input_sequence) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             ON CONFLICT (request_id) DO NOTHING \
             RETURNING id, user_id, request_id, spec, state, progress_percent, \
             error_code, error_message, result, created_at, started_at, \
             finished_at, attempts, next_attempt_at, deadline_at, claimed_by, \
             lease_expires_at, cancel_requested, callback_queue_url, trace_id, \
             input_failures, requester_id, requester_type, input_sequence",
            job.id().as_uuid(),
            job.owner().as_str(),
            job.request().as_str(),
            encode_spec(job.spec()),
            job.deadline_at().map(to_offset),
            job.callback().map(QueueUrl::as_str),
            job.trace_id().map(|trace| trace.to_string()),
            job.requester().map(|who| who.id().as_str().to_string()),
            job.requester().map(|who| who.kind().as_str()),
            job.input_order().map(encode_input_order),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        match inserted {
            Some(row) => to_record(row),
            None => {
                let existing = sqlx::query_scalar!(
                    "SELECT id FROM jobs WHERE request_id = $1",
                    job.request().as_str()
                )
                .fetch_one(&self.pool)
                .await
                .map_err(backend)?;
                Err(JobStoreError::AlreadyExists {
                    existing: JobId::from_uuid(existing)
                        .map_err(|error| JobStoreError::Backend(error.to_string()))?,
                })
            }
        }
    }

    async fn get(&self, id: JobId) -> Result<Option<JobRecord>, JobStoreError> {
        sqlx::query_as!(
            JobRow,
            "SELECT id, user_id, request_id, spec, state, progress_percent, \
             error_code, error_message, result, created_at, started_at, \
             finished_at, attempts, next_attempt_at, deadline_at, claimed_by, \
             lease_expires_at, cancel_requested, callback_queue_url, trace_id, \
             input_failures, requester_id, requester_type, input_sequence \
             FROM jobs WHERE id = $1",
            id.as_uuid()
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?
        .map(to_record)
        .transpose()
    }

    async fn get_owned(
        &self,
        owner: &UserId,
        id: JobId,
    ) -> Result<Option<JobRecord>, JobStoreError> {
        sqlx::query_as!(
            JobRow,
            "SELECT id, user_id, request_id, spec, state, progress_percent, \
             error_code, error_message, result, created_at, started_at, \
             finished_at, attempts, next_attempt_at, deadline_at, claimed_by, \
             lease_expires_at, cancel_requested, callback_queue_url, trace_id, \
             input_failures, requester_id, requester_type, input_sequence \
             FROM jobs WHERE id = $1 AND user_id = $2",
            id.as_uuid(),
            owner.as_str()
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?
        .map(to_record)
        .transpose()
    }

    async fn find_by_request(
        &self,
        owner: &UserId,
        request: &RequestId,
    ) -> Result<Option<JobRecord>, JobStoreError> {
        sqlx::query_as!(
            JobRow,
            "SELECT id, user_id, request_id, spec, state, progress_percent, \
             error_code, error_message, result, created_at, started_at, \
             finished_at, attempts, next_attempt_at, deadline_at, claimed_by, \
             lease_expires_at, cancel_requested, callback_queue_url, trace_id, \
             input_failures, requester_id, requester_type, input_sequence \
             FROM jobs WHERE user_id = $1 AND request_id = $2",
            owner.as_str(),
            request.as_str()
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?
        .map(to_record)
        .transpose()
    }

    async fn list(
        &self,
        owner: &UserId,
        states: &[JobState],
        limit: NonZeroU32,
        cursor: Option<PageCursor>,
        requester: Option<RequesterType>,
    ) -> Result<JobPage, JobStoreError> {
        let state_names: Vec<String> = states
            .iter()
            .map(|state| state.name().to_string())
            .collect();
        let requester_type = requester.map(|r| r.as_str().to_string());
        let rows = sqlx::query_as!(
            JobRow,
            "SELECT id, user_id, request_id, spec, state, progress_percent, \
             error_code, error_message, result, created_at, started_at, \
             finished_at, attempts, next_attempt_at, deadline_at, claimed_by, \
             lease_expires_at, cancel_requested, callback_queue_url, trace_id, \
             input_failures, requester_id, requester_type, input_sequence \
             FROM jobs \
             WHERE user_id = $1 \
               AND ($2::timestamptz IS NULL \
                    OR (created_at, id) < ($2::timestamptz, $3::uuid)) \
               AND (cardinality($4::text[]) = 0 OR state = ANY($4::text[])) \
               AND ($5::text IS NULL OR requester_type = $5) \
             ORDER BY created_at DESC, id DESC \
             LIMIT $6",
            owner.as_str(),
            cursor.map(|cursor| to_offset(cursor.created_at)),
            cursor.map(|cursor| cursor.id.as_uuid()),
            &state_names,
            requester_type.as_deref(),
            i64::from(limit.get())
        )
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        let full_page = rows.len() == limit.get() as usize;
        let jobs = rows
            .into_iter()
            .map(to_record)
            .collect::<Result<Vec<_>, _>>()?;
        let next = full_page
            .then(|| {
                jobs.last().map(|job| PageCursor {
                    created_at: job.created_at,
                    id: job.id,
                })
            })
            .flatten();
        Ok(JobPage { jobs, next })
    }

    async fn claim(
        &self,
        worker: &WorkerId,
        limit: NonZeroU32,
        lease: Duration,
    ) -> Result<Vec<JobRecord>, JobStoreError> {
        sqlx::query_as!(
            JobRow,
            "UPDATE jobs SET state = 'RUNNING', claimed_by = $1, \
             lease_expires_at = now() + make_interval(secs => $2), \
             started_at = COALESCE(started_at, now()), \
             attempts = attempts + 1 \
             WHERE id IN ( \
                 SELECT id FROM jobs \
                 WHERE state = 'PENDING' \
                   AND (next_attempt_at IS NULL OR next_attempt_at <= now()) \
                 ORDER BY created_at \
                 LIMIT $3 \
                 FOR UPDATE SKIP LOCKED \
             ) \
             RETURNING id, user_id, request_id, spec, state, progress_percent, \
             error_code, error_message, result, created_at, started_at, \
             finished_at, attempts, next_attempt_at, deadline_at, claimed_by, \
             lease_expires_at, cancel_requested, callback_queue_url, trace_id, \
             input_failures, requester_id, requester_type, input_sequence",
            worker.as_str(),
            lease.as_secs_f64(),
            i64::from(limit.get())
        )
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?
        .into_iter()
        .map(to_record)
        .collect::<Result<Vec<_>, _>>()
        .map(|mut records| {
            records.sort_by_key(|record| (record.created_at, record.id));
            records
        })
    }

    async fn heartbeat(
        &self,
        id: JobId,
        worker: &WorkerId,
        lease: Duration,
    ) -> Result<SystemTime, JobStoreError> {
        let renewed = sqlx::query_scalar!(
            "UPDATE jobs SET lease_expires_at = now() + make_interval(secs => $3) \
             WHERE id = $1 AND claimed_by = $2 AND state = 'RUNNING' \
             RETURNING lease_expires_at",
            id.as_uuid(),
            worker.as_str(),
            lease.as_secs_f64()
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        match renewed.flatten() {
            Some(expiry) => Ok(to_system(expiry)),
            None => Err(self
                .transition_conflict(id, JobState::Running, Some(worker))
                .await),
        }
    }

    async fn requeue_expired(&self) -> Result<Vec<JobId>, JobStoreError> {
        sqlx::query_scalar!(
            "UPDATE jobs SET state = 'PENDING', claimed_by = NULL, \
             lease_expires_at = NULL, progress_percent = 0 \
             WHERE state = 'RUNNING' AND lease_expires_at <= now() \
             RETURNING id"
        )
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?
        .into_iter()
        .map(|id| JobId::from_uuid(id).map_err(|error| JobStoreError::Backend(error.to_string())))
        .collect()
    }

    async fn fail_overdue(&self) -> Result<Vec<JobId>, JobStoreError> {
        sqlx::query_scalar!(
            "UPDATE jobs SET state = 'FAILED', error_code = 'timeout', \
             error_message = 'job deadline passed', finished_at = now(), \
             claimed_by = NULL, lease_expires_at = NULL \
             WHERE state IN ('PENDING', 'RUNNING') AND deadline_at <= now() \
             RETURNING id"
        )
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?
        .into_iter()
        .map(|id| JobId::from_uuid(id).map_err(|error| JobStoreError::Backend(error.to_string())))
        .collect()
    }

    async fn release(
        &self,
        id: JobId,
        worker: &WorkerId,
        retry_after: Duration,
    ) -> Result<(), JobStoreError> {
        let updated = sqlx::query!(
            "UPDATE jobs SET state = 'PENDING', claimed_by = NULL, \
             lease_expires_at = NULL, progress_percent = 0, \
             attempts = GREATEST(attempts - 1, 0), \
             next_attempt_at = now() + make_interval(secs => $3) \
             WHERE id = $1 AND claimed_by = $2 AND state = 'RUNNING'",
            id.as_uuid(),
            worker.as_str(),
            retry_after.as_secs_f64()
        )
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        if updated.rows_affected() == 0 {
            return Err(self
                .transition_conflict(id, JobState::Pending, Some(worker))
                .await);
        }
        Ok(())
    }

    async fn retry(
        &self,
        id: JobId,
        worker: &WorkerId,
        failure: JobFailure,
        retry_after: Duration,
    ) -> Result<(), JobStoreError> {
        let updated = sqlx::query!(
            "UPDATE jobs SET state = 'PENDING', claimed_by = NULL, \
             lease_expires_at = NULL, progress_percent = 0, \
             error_code = $3, error_message = $4, input_failures = $5, \
             next_attempt_at = now() + make_interval(secs => $6) \
             WHERE id = $1 AND claimed_by = $2 AND state = 'RUNNING'",
            id.as_uuid(),
            worker.as_str(),
            failure.code(),
            failure.message(),
            encode_input_failures(failure.inputs()),
            retry_after.as_secs_f64()
        )
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        if updated.rows_affected() == 0 {
            return Err(self
                .transition_conflict(id, JobState::Pending, Some(worker))
                .await);
        }
        Ok(())
    }

    async fn set_progress(
        &self,
        id: JobId,
        worker: &WorkerId,
        progress: ProgressPercent,
    ) -> Result<(), JobStoreError> {
        let updated = sqlx::query!(
            "UPDATE jobs SET progress_percent = $3 \
             WHERE id = $1 AND claimed_by = $2 AND state = 'RUNNING'",
            id.as_uuid(),
            worker.as_str(),
            i16::from(progress.value())
        )
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        if updated.rows_affected() == 0 {
            return Err(self
                .transition_conflict(id, JobState::Running, Some(worker))
                .await);
        }
        Ok(())
    }

    async fn mark_succeeded(
        &self,
        id: JobId,
        worker: &WorkerId,
        result: OpResult,
    ) -> Result<JobRecord, JobStoreError> {
        let row = sqlx::query_as!(
            JobRow,
            "UPDATE jobs SET state = 'SUCCEEDED', result = $3, \
             progress_percent = 100, finished_at = now(), lease_expires_at = NULL, \
             error_code = NULL, error_message = NULL, input_failures = NULL \
             WHERE id = $1 AND claimed_by = $2 AND state = 'RUNNING' \
             RETURNING id, user_id, request_id, spec, state, progress_percent, \
             error_code, error_message, result, created_at, started_at, \
             finished_at, attempts, next_attempt_at, deadline_at, claimed_by, \
             lease_expires_at, cancel_requested, callback_queue_url, trace_id, \
             input_failures, requester_id, requester_type, input_sequence",
            id.as_uuid(),
            worker.as_str(),
            encode_result(&result)
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        match row {
            Some(row) => to_record(row),
            None => Err(self
                .transition_conflict(id, JobState::Succeeded, Some(worker))
                .await),
        }
    }

    async fn mark_failed(
        &self,
        id: JobId,
        worker: &WorkerId,
        failure: JobFailure,
        result: Option<OpResult>,
    ) -> Result<JobRecord, JobStoreError> {
        let row = sqlx::query_as!(
            JobRow,
            "UPDATE jobs SET state = 'FAILED', error_code = $3, error_message = $4, \
             input_failures = $5, result = $6, \
             finished_at = now(), lease_expires_at = NULL \
             WHERE id = $1 AND claimed_by = $2 AND state = 'RUNNING' \
             RETURNING id, user_id, request_id, spec, state, progress_percent, \
             error_code, error_message, result, created_at, started_at, \
             finished_at, attempts, next_attempt_at, deadline_at, claimed_by, \
             lease_expires_at, cancel_requested, callback_queue_url, trace_id, \
             input_failures, requester_id, requester_type, input_sequence",
            id.as_uuid(),
            worker.as_str(),
            failure.code(),
            failure.message(),
            encode_input_failures(failure.inputs()),
            result.as_ref().map(encode_result)
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        match row {
            Some(row) => to_record(row),
            None => Err(self
                .transition_conflict(id, JobState::Failed, Some(worker))
                .await),
        }
    }

    async fn mark_cancelled(&self, id: JobId) -> Result<JobRecord, JobStoreError> {
        let row = sqlx::query_as!(
            JobRow,
            "UPDATE jobs SET state = 'CANCELLED', finished_at = now(), \
             claimed_by = NULL, lease_expires_at = NULL \
             WHERE id = $1 AND state IN ('PENDING', 'RUNNING') \
             RETURNING id, user_id, request_id, spec, state, progress_percent, \
             error_code, error_message, result, created_at, started_at, \
             finished_at, attempts, next_attempt_at, deadline_at, claimed_by, \
             lease_expires_at, cancel_requested, callback_queue_url, trace_id, \
             input_failures, requester_id, requester_type, input_sequence",
            id.as_uuid()
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        match row {
            Some(row) => to_record(row),
            None => Err(self
                .transition_conflict(id, JobState::Cancelled, None)
                .await),
        }
    }

    async fn claim_callback(&self, id: JobId) -> Result<bool, JobStoreError> {
        let updated = sqlx::query!(
            "UPDATE jobs SET callback_published_at = now() \
             WHERE id = $1 AND callback_published_at IS NULL \
               AND callback_queue_url IS NOT NULL \
               AND state IN ('SUCCEEDED', 'FAILED', 'CANCELLED')",
            id.as_uuid()
        )
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(updated.rows_affected() > 0)
    }

    async fn unpublished_callbacks(
        &self,
        limit: NonZeroU32,
    ) -> Result<Vec<JobRecord>, JobStoreError> {
        sqlx::query_as!(
            JobRow,
            "SELECT id, user_id, request_id, spec, state, progress_percent, \
             error_code, error_message, result, created_at, started_at, \
             finished_at, attempts, next_attempt_at, deadline_at, claimed_by, \
             lease_expires_at, cancel_requested, callback_queue_url, trace_id, \
             input_failures, requester_id, requester_type, input_sequence \
             FROM jobs \
             WHERE state IN ('SUCCEEDED', 'FAILED', 'CANCELLED') \
               AND callback_queue_url IS NOT NULL \
               AND callback_published_at IS NULL \
             ORDER BY finished_at \
             LIMIT $1",
            i64::from(limit.get())
        )
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?
        .into_iter()
        .map(to_record)
        .collect()
    }

    async fn pending_count(&self) -> Result<u64, JobStoreError> {
        let count =
            sqlx::query_scalar!("SELECT count(*) AS count FROM jobs WHERE state = 'PENDING'")
                .fetch_one(&self.pool)
                .await
                .map_err(backend)?;
        let count = count.unwrap_or_default();
        u64::try_from(count).map_err(|error| JobStoreError::Backend(error.to_string()))
    }

    async fn create_template(&self, template: Template) -> Result<Template, JobStoreError> {
        let row = sqlx::query_as!(
            TemplateRow,
            "INSERT INTO templates (id, user_id, name, spec, parameters) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (user_id, name) DO NOTHING \
             RETURNING id, user_id, name, spec, parameters",
            template.id().as_uuid(),
            template.owner().as_str(),
            template.name().as_str(),
            encode_spec(template.spec()),
            encode_parameters(template.parameters())
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        match row {
            Some(row) => to_template(row),
            None => Err(JobStoreError::TemplateNameTaken {
                name: template.name().as_str().to_string(),
            }),
        }
    }

    async fn get_template(
        &self,
        owner: &UserId,
        id: TemplateId,
    ) -> Result<Option<Template>, JobStoreError> {
        let row = sqlx::query_as!(
            TemplateRow,
            "SELECT id, user_id, name, spec, parameters FROM templates \
             WHERE id = $1 AND user_id = $2",
            id.as_uuid(),
            owner.as_str()
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        row.map(to_template).transpose()
    }

    async fn list_templates(&self, owner: &UserId) -> Result<Vec<Template>, JobStoreError> {
        let rows = sqlx::query_as!(
            TemplateRow,
            "SELECT id, user_id, name, spec, parameters FROM templates \
             WHERE user_id = $1 ORDER BY created_at, id",
            owner.as_str()
        )
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.into_iter().map(to_template).collect()
    }

    async fn delete_template(&self, owner: &UserId, id: TemplateId) -> Result<bool, JobStoreError> {
        let deleted = sqlx::query!(
            "DELETE FROM templates WHERE id = $1 AND user_id = $2",
            id.as_uuid(),
            owner.as_str()
        )
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(deleted.rows_affected() > 0)
    }

    async fn request_cancel(&self, id: JobId) -> Result<(), JobStoreError> {
        let updated = sqlx::query!(
            "UPDATE jobs SET cancel_requested = true \
             WHERE id = $1 AND state IN ('PENDING', 'RUNNING')",
            id.as_uuid()
        )
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        if updated.rows_affected() == 0 {
            return Err(self
                .transition_conflict(id, JobState::Cancelled, None)
                .await);
        }
        Ok(())
    }
}

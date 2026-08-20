use std::env;
use std::net::{TcpStream, ToSocketAddrs};
use std::num::NonZeroU32;
use std::str::FromStr;
use std::time::{Duration, SystemTime};

use mm_domain::{
    CancelNotifications, ConcatSpec, JobEventNotifications, JobFailure, JobId, JobNotifications,
    JobRepository, JobState, JobStoreError, MediaRef, NewJob, OpResult, OpSpec, ProbeSpec,
    ProgressPercent, QueueUrl, RequestId, Template, TimedOutput, TraceId, UserId, WorkerId,
};
use mm_jobstore::{
    MIGRATOR, PgCancelListener, PgJobEventListener, PgJobListener, PgJobRepository,
    reconnect_backoff,
};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

const DEFAULT_ADMIN_URL: &str = "postgres://mm:mm@127.0.0.1:5433/mm";

fn admin_url() -> String {
    env::var("MM_DATABASE_URL").unwrap_or_else(|_| DEFAULT_ADMIN_URL.to_string())
}

fn require_stack() {
    let url = admin_url();
    let authority = url
        .split('@')
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .expect("database url must contain an authority");
    let address = authority
        .to_socket_addrs()
        .expect("database authority must resolve")
        .next()
        .expect("database authority must resolve to an address");
    if TcpStream::connect_timeout(&address, Duration::from_secs(2)).is_err() {
        panic!(
            "database at {authority} is unreachable; \
             start the compose stack with `docker compose up -d` \
             before running mm-jobstore integration tests"
        );
    }
}

struct TestDb {
    admin: PgPool,
    pool: PgPool,
    url: String,
    name: String,
}

async fn fresh_db() -> TestDb {
    require_stack();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url())
        .await
        .expect("admin connection");
    let name = format!("mm_test_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&admin)
        .await
        .expect("create test database");
    let base = admin_url();
    let url = format!(
        "{}/{name}",
        base.rsplit_once('/').expect("database url has a path").0
    );
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await
        .expect("test database connection");
    MIGRATOR.run(&pool).await.expect("migrations apply");
    TestDb {
        admin,
        pool,
        url,
        name,
    }
}

impl TestDb {
    fn repository(&self) -> PgJobRepository {
        PgJobRepository::new(self.pool.clone())
    }

    async fn dispose(self) {
        self.pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE {} WITH (FORCE)",
            self.name
        )))
        .execute(&self.admin)
        .await
        .expect("drop test database");
    }
}

fn owner(value: &str) -> UserId {
    UserId::from_str(value).expect("valid user id")
}

fn worker(value: &str) -> WorkerId {
    WorkerId::from_str(value).expect("valid worker id")
}

fn request(value: &str) -> RequestId {
    RequestId::from_str(value).expect("valid request id")
}

fn limit(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).expect("nonzero limit")
}

fn concat_spec() -> OpSpec {
    OpSpec::Concat(
        ConcatSpec::new(
            vec![
                MediaRef::object_uri("s3://media/in/a.mp4").expect("valid ref"),
                MediaRef::object_uri("s3://media/in/b.mp4").expect("valid ref"),
            ],
            None,
        )
        .expect("valid concat spec"),
    )
}

fn new_job(owner_id: &str, request_id: &str) -> NewJob {
    new_job_due(
        owner_id,
        request_id,
        Some(SystemTime::now() + Duration::from_secs(3600)),
    )
}

fn new_job_due(owner_id: &str, request_id: &str, deadline: Option<SystemTime>) -> NewJob {
    NewJob::new(
        JobId::generate(),
        owner(owner_id),
        request(request_id),
        concat_spec(),
        deadline,
    )
}

async fn db_now(pool: &PgPool) -> SystemTime {
    let now: time::OffsetDateTime = sqlx::query_scalar("SELECT now()")
        .fetch_one(pool)
        .await
        .expect("database clock readable");
    SystemTime::from(now)
}

fn concat_result() -> OpResult {
    OpResult::Concat(
        TimedOutput::new(
            MediaRef::object_uri("s3://media/jobs/x/out.mp4").expect("valid ref"),
            120.5,
        )
        .expect("valid result"),
    )
}

#[tokio::test]
async fn migrations_apply_fresh_and_rerun_idempotently() {
    let db = fresh_db().await;
    MIGRATOR.run(&db.pool).await.expect("second run idempotent");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
        .fetch_one(&db.pool)
        .await
        .expect("jobs table queryable");
    assert_eq!(count, 0);
    db.dispose().await;
}

#[tokio::test]
async fn create_returns_pending_record_with_spec_roundtrip() {
    let db = fresh_db().await;
    let repo = db.repository();
    let job = new_job("owner-1", "req-1");
    let before = SystemTime::now() - Duration::from_secs(5);
    let record = repo.create(job.clone()).await.expect("create succeeds");
    assert_eq!(record.id, job.id());
    assert_eq!(&record.owner, job.owner());
    assert_eq!(&record.request, job.request());
    assert_eq!(&record.spec, job.spec());
    assert_eq!(record.state, JobState::Pending);
    assert_eq!(record.progress, ProgressPercent::ZERO);
    assert_eq!(record.attempts, 0);
    assert!(record.created_at > before);
    assert!(record.started_at.is_none());
    assert!(record.finished_at.is_none());
    assert!(record.failure.is_none());
    assert!(record.result.is_none());
    assert!(record.claimed_by.is_none());
    assert!(record.lease_expires_at.is_none());
    assert!(record.next_attempt_at.is_none());
    assert!(!record.cancel_requested);
    let deadline = record.deadline_at.expect("deadline persisted");
    let expected = job.deadline_at().expect("deadline set");
    let drift = deadline
        .duration_since(expected)
        .unwrap_or_else(|gap| gap.duration());
    assert!(drift < Duration::from_millis(5));
    db.dispose().await;
}

#[tokio::test]
async fn duplicate_request_id_is_already_exists_with_original_id() {
    let db = fresh_db().await;
    let repo = db.repository();
    let first = repo
        .create(new_job("owner-1", "req-dup"))
        .await
        .expect("first create succeeds");
    let error = repo
        .create(new_job("owner-2", "req-dup"))
        .await
        .expect_err("duplicate request id rejected");
    assert_eq!(error, JobStoreError::AlreadyExists { existing: first.id });
    db.dispose().await;
}

#[tokio::test]
async fn get_is_owner_scoped_only_in_get_owned() {
    let db = fresh_db().await;
    let repo = db.repository();
    let record = repo
        .create(new_job("owner-1", "req-1"))
        .await
        .expect("create succeeds");
    assert!(repo.get(record.id).await.expect("get succeeds").is_some());
    assert!(
        repo.get(JobId::generate())
            .await
            .expect("get succeeds")
            .is_none()
    );
    assert!(
        repo.get_owned(&owner("owner-1"), record.id)
            .await
            .expect("owner get succeeds")
            .is_some()
    );
    assert!(
        repo.get_owned(&owner("owner-2"), record.id)
            .await
            .expect("foreign get succeeds")
            .is_none()
    );
    db.dispose().await;
}

#[tokio::test]
async fn list_pages_newest_first_with_keyset_cursor() {
    let db = fresh_db().await;
    let repo = db.repository();
    let mut created = Vec::new();
    for index in 0..5 {
        created.push(
            repo.create(new_job("owner-1", &format!("req-{index}")))
                .await
                .expect("create succeeds")
                .id,
        );
    }
    repo.create(new_job("owner-2", "req-other"))
        .await
        .expect("create succeeds");
    created.reverse();
    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = repo
            .list(&owner("owner-1"), &[], limit(2), cursor)
            .await
            .expect("list succeeds");
        seen.extend(page.jobs.iter().map(|job| job.id));
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(seen, created);
    db.dispose().await;
}

#[tokio::test]
async fn list_filters_by_requested_states() {
    let db = fresh_db().await;
    let repo = db.repository();
    let mut pending = Vec::new();
    for index in 0..3 {
        pending.push(
            repo.create(new_job("owner-1", &format!("req-{index}")))
                .await
                .expect("create succeeds")
                .id,
        );
    }
    let cancelled = pending.remove(0);
    repo.mark_cancelled(cancelled)
        .await
        .expect("cancel succeeds");
    let only_cancelled = repo
        .list(&owner("owner-1"), &[JobState::Cancelled], limit(10), None)
        .await
        .expect("list succeeds");
    assert_eq!(
        only_cancelled
            .jobs
            .iter()
            .map(|job| job.id)
            .collect::<Vec<_>>(),
        vec![cancelled]
    );
    let only_pending = repo
        .list(&owner("owner-1"), &[JobState::Pending], limit(10), None)
        .await
        .expect("list succeeds");
    assert_eq!(only_pending.jobs.len(), 2);
    let both = repo
        .list(
            &owner("owner-1"),
            &[JobState::Pending, JobState::Cancelled],
            limit(10),
            None,
        )
        .await
        .expect("list succeeds");
    assert_eq!(both.jobs.len(), 3);
    db.dispose().await;
}

#[tokio::test]
async fn guarded_transitions_reject_illegal_moves_and_freeze_terminal_rows() {
    let db = fresh_db().await;
    let repo = db.repository();
    let pending = repo
        .create(new_job("owner-1", "req-1"))
        .await
        .expect("create succeeds");
    let a = worker("worker-a");
    let error = repo
        .mark_succeeded(pending.id, &a, concat_result())
        .await
        .expect_err("pending job cannot succeed");
    assert_eq!(
        error,
        JobStoreError::InvalidTransition {
            id: pending.id,
            from: JobState::Pending,
            to: JobState::Succeeded,
        }
    );
    let claimed = repo
        .claim(&a, limit(1), Duration::from_secs(30))
        .await
        .expect("claim succeeds");
    assert_eq!(claimed.len(), 1);
    let done = repo
        .mark_succeeded(pending.id, &a, concat_result())
        .await
        .expect("running job succeeds");
    assert_eq!(done.state, JobState::Succeeded);
    assert_eq!(done.progress, ProgressPercent::COMPLETE);
    assert_eq!(done.result, Some(concat_result()));
    assert!(done.finished_at.is_some());
    let frozen = JobStoreError::InvalidTransition {
        id: pending.id,
        from: JobState::Succeeded,
        to: JobState::Failed,
    };
    assert_eq!(
        repo.mark_failed(
            pending.id,
            &a,
            JobFailure::new("TOOL_FAILED", "boom").expect("valid failure"),
            None,
        )
        .await
        .expect_err("terminal row immutable"),
        frozen
    );
    assert!(matches!(
        repo.mark_cancelled(pending.id)
            .await
            .expect_err("terminal row immutable"),
        JobStoreError::InvalidTransition {
            from: JobState::Succeeded,
            to: JobState::Cancelled,
            ..
        }
    ));
    assert!(matches!(
        repo.set_progress(pending.id, &a, ProgressPercent::new(10).expect("valid"))
            .await
            .expect_err("terminal row immutable"),
        JobStoreError::InvalidTransition { .. }
    ));
    assert!(matches!(
        repo.request_cancel(pending.id)
            .await
            .expect_err("terminal row immutable"),
        JobStoreError::InvalidTransition { .. }
    ));
    assert!(matches!(
        repo.heartbeat(pending.id, &a, Duration::from_secs(30))
            .await
            .expect_err("terminal row immutable"),
        JobStoreError::InvalidTransition { .. }
    ));
    assert!(matches!(
        repo.mark_succeeded(JobId::generate(), &a, concat_result())
            .await
            .expect_err("unknown job"),
        JobStoreError::NotFound { .. }
    ));
    db.dispose().await;
}

#[tokio::test]
async fn mark_failed_records_code_and_message() {
    let db = fresh_db().await;
    let repo = db.repository();
    let record = repo
        .create(new_job("owner-1", "req-1"))
        .await
        .expect("create succeeds");
    let a = worker("worker-a");
    repo.claim(&a, limit(1), Duration::from_secs(30))
        .await
        .expect("claim succeeds");
    let failure = JobFailure::new("TOOL_FAILED", "exit status 1").expect("valid failure");
    let failed = repo
        .mark_failed(record.id, &a, failure.clone(), None)
        .await
        .expect("mark failed succeeds");
    assert_eq!(failed.state, JobState::Failed);
    assert_eq!(failed.failure, Some(failure));
    assert!(failed.finished_at.is_some());
    db.dispose().await;
}

#[tokio::test]
async fn cancel_paths_cover_pending_running_and_flag() {
    let db = fresh_db().await;
    let repo = db.repository();
    let pending = repo
        .create(new_job("owner-1", "req-1"))
        .await
        .expect("create succeeds");
    let cancelled = repo
        .mark_cancelled(pending.id)
        .await
        .expect("pending cancel succeeds");
    assert_eq!(cancelled.state, JobState::Cancelled);
    assert!(cancelled.finished_at.is_some());

    let running = repo
        .create(new_job("owner-1", "req-2"))
        .await
        .expect("create succeeds");
    let a = worker("worker-a");
    repo.claim(&a, limit(1), Duration::from_secs(30))
        .await
        .expect("claim succeeds");
    repo.request_cancel(running.id)
        .await
        .expect("cancel request succeeds");
    let flagged = repo
        .get(running.id)
        .await
        .expect("get succeeds")
        .expect("job exists");
    assert!(flagged.cancel_requested);
    assert_eq!(flagged.state, JobState::Running);
    let stopped = repo
        .mark_cancelled(running.id)
        .await
        .expect("running cancel succeeds");
    assert_eq!(stopped.state, JobState::Cancelled);
    assert!(stopped.claimed_by.is_none());
    db.dispose().await;
}

#[tokio::test]
async fn claim_takes_oldest_pending_sets_lease_and_counts_attempt() {
    let db = fresh_db().await;
    let repo = db.repository();
    let mut ids = Vec::new();
    for index in 0..3 {
        ids.push(
            repo.create(new_job("owner-1", &format!("req-{index}")))
                .await
                .expect("create succeeds")
                .id,
        );
    }
    let a = worker("worker-a");
    let before = SystemTime::now();
    let lease = Duration::from_secs(30);
    let claimed = repo
        .claim(&a, limit(2), lease)
        .await
        .expect("claim succeeds");
    assert_eq!(
        claimed.iter().map(|job| job.id).collect::<Vec<_>>(),
        ids[..2]
    );
    for job in &claimed {
        assert_eq!(job.state, JobState::Running);
        assert_eq!(job.claimed_by.as_ref(), Some(&a));
        assert_eq!(job.attempts, 1);
        assert!(job.started_at.is_some());
        let expiry = job.lease_expires_at.expect("lease set");
        assert!(expiry > before + lease - Duration::from_secs(5));
        assert!(expiry < before + lease + Duration::from_secs(60));
    }
    let rest = repo
        .claim(&a, limit(5), lease)
        .await
        .expect("claim succeeds");
    assert_eq!(rest.len(), 1);
    assert!(
        repo.claim(&a, limit(5), lease)
            .await
            .expect("claim succeeds")
            .is_empty()
    );
    let progress = ProgressPercent::new(40).expect("valid progress");
    repo.set_progress(ids[0], &a, progress)
        .await
        .expect("progress persists");
    let seen = repo
        .get(ids[0])
        .await
        .expect("get succeeds")
        .expect("job exists");
    assert_eq!(seen.progress, progress);
    db.dispose().await;
}

#[tokio::test]
async fn claim_skips_jobs_scheduled_for_later() {
    let db = fresh_db().await;
    let repo = db.repository();
    let record = repo
        .create(new_job("owner-1", "req-1"))
        .await
        .expect("create succeeds");
    sqlx::query("UPDATE jobs SET next_attempt_at = now() + interval '1 hour' WHERE id = $1")
        .bind(record.id.as_uuid())
        .execute(&db.pool)
        .await
        .expect("schedule update succeeds");
    assert!(
        repo.claim(&worker("worker-a"), limit(5), Duration::from_secs(30))
            .await
            .expect("claim succeeds")
            .is_empty()
    );
    sqlx::query("UPDATE jobs SET next_attempt_at = now() - interval '1 second' WHERE id = $1")
        .bind(record.id.as_uuid())
        .execute(&db.pool)
        .await
        .expect("schedule update succeeds");
    assert_eq!(
        repo.claim(&worker("worker-a"), limit(5), Duration::from_secs(30))
            .await
            .expect("claim succeeds")
            .len(),
        1
    );
    db.dispose().await;
}

#[tokio::test]
async fn two_concurrent_claimers_split_hundred_jobs_without_overlap() {
    let db = fresh_db().await;
    let repo = db.repository();
    let mut expected = Vec::new();
    for index in 0..100 {
        expected.push(
            repo.create(new_job("owner-1", &format!("req-{index}")))
                .await
                .expect("create succeeds")
                .id,
        );
    }
    let mut claimers = Vec::new();
    for name in ["worker-a", "worker-b"] {
        let url = db.url.clone();
        claimers.push(tokio::spawn(async move {
            let pool = PgPoolOptions::new()
                .max_connections(2)
                .connect(&url)
                .await
                .expect("claimer connection");
            let repo = PgJobRepository::new(pool);
            let me = worker(name);
            let mut mine = Vec::new();
            loop {
                let batch = repo
                    .claim(&me, limit(7), Duration::from_secs(60))
                    .await
                    .expect("claim succeeds");
                if batch.is_empty() {
                    break;
                }
                for job in batch {
                    assert_eq!(job.claimed_by.as_ref(), Some(&me));
                    mine.push(job.id);
                }
            }
            mine
        }));
    }
    let mut all = Vec::new();
    let mut per_worker = Vec::new();
    for claimer in claimers {
        let mine = claimer.await.expect("claimer finishes");
        per_worker.push(mine.len());
        all.extend(mine);
    }
    assert_eq!(all.len(), 100);
    let mut unique = all.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 100);
    let mut sorted_expected = expected;
    sorted_expected.sort();
    assert_eq!(unique, sorted_expected);
    assert!(per_worker.iter().all(|count| *count > 0));
    db.dispose().await;
}

#[tokio::test]
async fn expired_lease_requeues_and_allows_reclaim() {
    let db = fresh_db().await;
    let repo = db.repository();
    let record = repo
        .create(new_job("owner-1", "req-1"))
        .await
        .expect("create succeeds");
    let a = worker("worker-a");
    let b = worker("worker-b");
    let claimed = repo
        .claim(&a, limit(1), Duration::from_secs(600))
        .await
        .expect("claim succeeds");
    assert_eq!(claimed.len(), 1);
    assert!(
        repo.requeue_expired()
            .await
            .expect("requeue succeeds")
            .is_empty()
    );
    sqlx::query("UPDATE jobs SET lease_expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(record.id.as_uuid())
        .execute(&db.pool)
        .await
        .expect("lease shrinks");
    assert_eq!(
        repo.requeue_expired().await.expect("requeue succeeds"),
        vec![record.id]
    );
    let requeued = repo
        .get(record.id)
        .await
        .expect("get succeeds")
        .expect("job exists");
    assert_eq!(requeued.state, JobState::Pending);
    assert!(requeued.claimed_by.is_none());
    assert!(requeued.lease_expires_at.is_none());
    assert_eq!(requeued.attempts, 1);
    let reclaimed = repo
        .claim(&b, limit(1), Duration::from_secs(30))
        .await
        .expect("reclaim succeeds");
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].attempts, 2);
    assert_eq!(
        repo.mark_succeeded(record.id, &a, concat_result())
            .await
            .expect_err("stale claimant rejected"),
        JobStoreError::LeaseNotHeld {
            id: record.id,
            worker: a,
        }
    );
    db.dispose().await;
}

#[tokio::test]
async fn heartbeat_extends_lease_for_claimant_only() {
    let db = fresh_db().await;
    let repo = db.repository();
    let record = repo
        .create(new_job("owner-1", "req-1"))
        .await
        .expect("create succeeds");
    let a = worker("worker-a");
    let b = worker("worker-b");
    let claimed = repo
        .claim(&a, limit(1), Duration::from_secs(10))
        .await
        .expect("claim succeeds");
    let initial = claimed[0].lease_expires_at.expect("lease set");
    let extended = repo
        .heartbeat(record.id, &a, Duration::from_secs(120))
        .await
        .expect("heartbeat succeeds");
    assert!(extended > initial);
    assert_eq!(
        repo.heartbeat(record.id, &b, Duration::from_secs(120))
            .await
            .expect_err("foreign heartbeat rejected"),
        JobStoreError::LeaseNotHeld {
            id: record.id,
            worker: b,
        }
    );
    assert!(matches!(
        repo.heartbeat(JobId::generate(), &a, Duration::from_secs(120))
            .await
            .expect_err("unknown job"),
        JobStoreError::NotFound { .. }
    ));
    db.dispose().await;
}

#[tokio::test]
async fn corrupt_spec_bytes_surface_as_corrupt_record() {
    let db = fresh_db().await;
    let repo = db.repository();
    let record = repo
        .create(new_job("owner-1", "req-1"))
        .await
        .expect("create succeeds");
    sqlx::query("UPDATE jobs SET spec = '\\x00ff'::bytea WHERE id = $1")
        .bind(record.id.as_uuid())
        .execute(&db.pool)
        .await
        .expect("spec update succeeds");
    assert!(matches!(
        repo.get(record.id).await.expect_err("corrupt spec rejected"),
        JobStoreError::CorruptRecord { id, .. } if id == record.id
    ));
    db.dispose().await;
}

#[tokio::test]
async fn release_returns_a_claimed_job_to_pending_with_a_retry_delay() {
    let db = fresh_db().await;
    let repo = db.repository();
    let job = repo
        .create(new_job("owner-1", "req-release"))
        .await
        .expect("create succeeds");
    let claimant = worker("w-release");
    let claimed = repo
        .claim(&claimant, limit(1), Duration::from_secs(30))
        .await
        .expect("claim succeeds");
    assert_eq!(claimed.len(), 1);

    repo.release(job.id, &claimant, Duration::from_secs(60))
        .await
        .expect("release succeeds");
    let record = repo
        .get(job.id)
        .await
        .expect("get succeeds")
        .expect("job exists");
    assert_eq!(record.state, JobState::Pending);
    assert!(record.claimed_by.is_none());
    assert!(record.lease_expires_at.is_none());
    assert_eq!(record.attempts, 0);
    assert_eq!(record.progress, ProgressPercent::ZERO);
    let due = record.next_attempt_at.expect("retry scheduled");
    assert!(due > SystemTime::now() + Duration::from_secs(30));

    let reclaim = repo
        .claim(&worker("w-other"), limit(1), Duration::from_secs(30))
        .await
        .expect("claim succeeds");
    assert!(reclaim.is_empty(), "scheduled-for-later job was claimed");

    let stale = repo
        .mark_failed(
            job.id,
            &claimant,
            JobFailure::new("stale", "finalize after release").expect("valid failure"),
            None,
        )
        .await;
    assert!(matches!(
        stale,
        Err(JobStoreError::InvalidTransition { .. })
    ));
    let repeated = repo
        .release(job.id, &claimant, Duration::from_secs(1))
        .await;
    assert!(matches!(
        repeated,
        Err(JobStoreError::InvalidTransition { .. })
    ));
    db.dispose().await;
}

#[tokio::test]
async fn retry_requeues_with_failure_and_schedule_keeping_attempts() {
    let db = fresh_db().await;
    let repo = db.repository();
    let job = repo
        .create(new_job("owner-1", "req-retry"))
        .await
        .expect("create succeeds");
    let claimant = worker("w-retry");
    let claimed = repo
        .claim(&claimant, limit(1), Duration::from_secs(30))
        .await
        .expect("claim succeeds");
    assert_eq!(claimed.len(), 1);
    let failure = JobFailure::new("output_upload_failed", "store unreachable").expect("valid");
    let before_retry = db_now(&db.pool).await;
    repo.retry(job.id, &claimant, failure.clone(), Duration::from_secs(2))
        .await
        .expect("retry succeeds");
    let after_retry = db_now(&db.pool).await;

    let record = repo
        .get(job.id)
        .await
        .expect("get succeeds")
        .expect("job exists");
    assert_eq!(record.state, JobState::Pending);
    assert_eq!(record.attempts, 1);
    assert!(record.claimed_by.is_none());
    assert!(record.lease_expires_at.is_none());
    assert_eq!(record.progress, ProgressPercent::ZERO);
    assert_eq!(record.failure, Some(failure));
    let due = record.next_attempt_at.expect("retry scheduled");
    assert!(due >= before_retry + Duration::from_secs(2), "{due:?}");
    assert!(due <= after_retry + Duration::from_secs(2), "{due:?}");

    let early = repo
        .claim(&claimant, limit(1), Duration::from_secs(30))
        .await
        .expect("claim succeeds");
    let reclaimed = if early.is_empty() {
        let remaining = due
            .duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO);
        tokio::time::sleep(remaining + Duration::from_millis(100)).await;
        repo.claim(&claimant, limit(1), Duration::from_secs(30))
            .await
            .expect("claim succeeds")
    } else {
        assert!(
            SystemTime::now() >= due,
            "retry was claimable before its schedule"
        );
        early
    };
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].attempts, 2);
    let done = repo
        .mark_succeeded(job.id, &claimant, concat_result())
        .await
        .expect("mark succeeded succeeds");
    assert!(done.failure.is_none(), "stale failure kept after success");
    db.dispose().await;
}

#[tokio::test]
async fn overdue_jobs_fail_with_timeout_whatever_their_phase() {
    let db = fresh_db().await;
    let repo = db.repository();
    let past = Some(SystemTime::now() - Duration::from_secs(5));
    let overdue_pending = repo
        .create(new_job_due("owner-1", "req-overdue-pending", past))
        .await
        .expect("create succeeds");
    let overdue_running = repo
        .create(new_job_due("owner-1", "req-overdue-running", past))
        .await
        .expect("create succeeds");
    let fresh = repo
        .create(new_job("owner-1", "req-fresh"))
        .await
        .expect("create succeeds");
    let finished = repo
        .create(new_job_due("owner-1", "req-finished", past))
        .await
        .expect("create succeeds");
    let claimant = worker("w-reaper");
    sqlx::query(
        "UPDATE jobs SET state = 'RUNNING', claimed_by = $1, \
         lease_expires_at = now() + interval '30 seconds', \
         started_at = now(), attempts = 1 WHERE id = $2",
    )
    .bind(claimant.as_str())
    .bind(overdue_running.id.as_uuid())
    .execute(&db.pool)
    .await
    .expect("stage running job");
    sqlx::query("UPDATE jobs SET state = 'SUCCEEDED', finished_at = now() WHERE id = $1")
        .bind(finished.id.as_uuid())
        .execute(&db.pool)
        .await
        .expect("stage finished job");

    let mut reaped = repo.fail_overdue().await.expect("reap succeeds");
    reaped.sort();
    let mut expected = vec![overdue_pending.id, overdue_running.id];
    expected.sort();
    assert_eq!(reaped, expected);

    for id in [overdue_pending.id, overdue_running.id] {
        let record = repo
            .get(id)
            .await
            .expect("get succeeds")
            .expect("job exists");
        assert_eq!(record.state, JobState::Failed);
        let failure = record.failure.expect("failure recorded");
        assert_eq!(failure.code(), "timeout");
        assert!(record.finished_at.is_some());
        assert!(record.claimed_by.is_none());
        assert!(record.lease_expires_at.is_none());
    }
    let untouched = repo
        .get(fresh.id)
        .await
        .expect("get succeeds")
        .expect("job exists");
    assert_eq!(untouched.state, JobState::Pending);
    let kept = repo
        .get(finished.id)
        .await
        .expect("get succeeds")
        .expect("job exists");
    assert_eq!(kept.state, JobState::Succeeded);
    assert!(repo.fail_overdue().await.expect("reap succeeds").is_empty());
    db.dispose().await;
}

#[tokio::test]
async fn retry_is_rejected_for_unclaimed_jobs_and_foreign_claimants() {
    let db = fresh_db().await;
    let repo = db.repository();
    let job = repo
        .create(new_job("owner-1", "req-retry-guard"))
        .await
        .expect("create succeeds");
    let failure = JobFailure::new("merge_failed", "exit 1").expect("valid");
    assert!(matches!(
        repo.retry(
            job.id,
            &worker("w-a"),
            failure.clone(),
            Duration::from_secs(1)
        )
        .await
        .expect_err("pending job cannot retry"),
        JobStoreError::InvalidTransition { .. }
    ));
    repo.claim(&worker("w-a"), limit(1), Duration::from_secs(30))
        .await
        .expect("claim succeeds");
    assert_eq!(
        repo.retry(job.id, &worker("w-b"), failure, Duration::from_secs(1))
            .await
            .expect_err("foreign retry rejected"),
        JobStoreError::LeaseNotHeld {
            id: job.id,
            worker: worker("w-b"),
        }
    );
    db.dispose().await;
}

#[tokio::test]
async fn job_insert_and_requeue_notify_listeners() {
    let db = fresh_db().await;
    let listener = PgJobListener::connect(&db.url)
        .await
        .expect("listener connects");
    let repo = db.repository();
    repo.create(new_job("owner-1", "req-notify"))
        .await
        .expect("create succeeds");
    tokio::time::timeout(mm_testkit::budget::NOTIFY, listener.notified())
        .await
        .expect("insert notifies within the budget")
        .expect("notification received");

    let claimed = repo
        .claim(&worker("w-notify"), limit(1), Duration::from_millis(100))
        .await
        .expect("claim succeeds");
    assert_eq!(claimed.len(), 1);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let requeued = repo.requeue_expired().await.expect("requeue succeeds");
    assert_eq!(requeued.len(), 1);
    tokio::time::timeout(mm_testkit::budget::NOTIFY, listener.notified())
        .await
        .expect("requeue notifies within the budget")
        .expect("notification received");
    db.dispose().await;
}

#[test]
fn reconnect_backoff_grows_exponentially_to_a_cap() {
    assert_eq!(reconnect_backoff(0), Duration::ZERO);
    assert_eq!(reconnect_backoff(1), Duration::from_millis(100));
    assert_eq!(reconnect_backoff(2), Duration::from_millis(200));
    assert_eq!(reconnect_backoff(4), Duration::from_millis(800));
    assert_eq!(reconnect_backoff(10), Duration::from_secs(5));
    assert_eq!(reconnect_backoff(u32::MAX), Duration::from_secs(5));
}

#[tokio::test]
async fn a_cancel_request_notifies_listeners_with_the_job_id() {
    let db = fresh_db().await;
    let listener = PgCancelListener::connect(&db.url)
        .await
        .expect("listener connects");
    let repo = db.repository();
    let job = repo
        .create(new_job("owner-1", "req-cancel-notify"))
        .await
        .expect("create succeeds");
    repo.claim(&worker("w-cancel"), limit(1), Duration::from_secs(30))
        .await
        .expect("claim succeeds");
    repo.request_cancel(job.id).await.expect("cancel succeeds");
    let cancelled = tokio::time::timeout(mm_testkit::budget::NOTIFY, listener.cancelled())
        .await
        .expect("cancel notifies within the budget")
        .expect("notification received");
    assert_eq!(cancelled, job.id);
    db.dispose().await;
}

#[tokio::test]
async fn a_severed_listener_connection_recovers_and_wakes() {
    let db = fresh_db().await;
    let listener = PgJobListener::connect(&db.url)
        .await
        .expect("listener connects");
    let repo = db.repository();
    repo.create(new_job("owner-1", "req-before-cut"))
        .await
        .expect("create succeeds");
    tokio::time::timeout(mm_testkit::budget::NOTIFY, listener.notified())
        .await
        .expect("insert notifies within the budget")
        .expect("notification received");

    sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = $1 AND query ILIKE 'LISTEN%'",
    )
    .bind(&db.name)
    .fetch_all(&db.admin)
    .await
    .expect("terminate listener backend");

    let woken = tokio::time::timeout(mm_testkit::budget::LISTENER_RECOVERY, async {
        loop {
            if listener.notified().await.is_ok() {
                return;
            }
        }
    })
    .await;
    assert!(woken.is_ok(), "listener never recovered from the cut");

    repo.create(new_job("owner-1", "req-after-cut"))
        .await
        .expect("create succeeds");
    tokio::time::timeout(mm_testkit::budget::LISTENER_RECOVERY, async {
        loop {
            if listener.notified().await.is_ok() {
                return;
            }
        }
    })
    .await
    .expect("reconnected listener never delivered a notification");
    db.dispose().await;
}

#[tokio::test]
async fn an_unreachable_listener_errors_with_growing_backoff() {
    let db = fresh_db().await;
    let listener = PgJobListener::connect(&db.url)
        .await
        .expect("listener connects");
    db.dispose().await;

    let first = std::time::Instant::now();
    let outcome = tokio::time::timeout(mm_testkit::budget::LISTENER_RECOVERY, async {
        loop {
            if listener.notified().await.is_err() {
                return first.elapsed();
            }
        }
    })
    .await;
    assert!(outcome.is_ok(), "listener hung instead of erroring");

    let second = std::time::Instant::now();
    assert!(
        tokio::time::timeout(mm_testkit::budget::LISTENER_RECOVERY, listener.notified())
            .await
            .expect("backoff stays bounded")
            .is_err()
    );
    assert!(
        second.elapsed() >= Duration::from_millis(100),
        "no backoff before the second attempt: {:?}",
        second.elapsed()
    );

    let third = std::time::Instant::now();
    assert!(
        tokio::time::timeout(mm_testkit::budget::LISTENER_RECOVERY, listener.notified())
            .await
            .expect("backoff stays bounded")
            .is_err()
    );
    assert!(
        third.elapsed() >= Duration::from_millis(200),
        "backoff never grew: {:?}",
        third.elapsed()
    );
}

#[tokio::test]
async fn a_callback_queue_url_persists_from_create_to_terminal_reads() {
    let db = fresh_db().await;
    let repo = db.repository();
    let queue = QueueUrl::new("http://127.0.0.1:9324/queue/results").expect("valid url");
    let created = repo
        .create(new_job("owner-1", "req-callback").with_callback(queue.clone()))
        .await
        .expect("create succeeds");
    assert_eq!(created.callback, Some(queue.clone()));
    let fetched = repo
        .get(created.id)
        .await
        .expect("get succeeds")
        .expect("job present");
    assert_eq!(fetched.callback, Some(queue.clone()));
    let claimant = worker("w-cb");
    let claimed = repo
        .claim(&claimant, limit(1), Duration::from_secs(30))
        .await
        .expect("claim succeeds");
    assert_eq!(claimed[0].callback, Some(queue.clone()));
    let done = repo
        .mark_succeeded(created.id, &claimant, concat_result())
        .await
        .expect("job succeeds");
    assert_eq!(done.callback, Some(queue));

    let plain = repo
        .create(new_job("owner-1", "req-no-callback"))
        .await
        .expect("create succeeds");
    assert_eq!(plain.callback, None);
    db.dispose().await;
}

#[tokio::test]
async fn state_and_progress_changes_notify_the_events_listener() {
    let db = fresh_db().await;
    let listener = PgJobEventListener::connect(&db.url)
        .await
        .expect("listener connects");
    let repo = db.repository();
    let job = repo
        .create(new_job("owner-1", "req-events"))
        .await
        .expect("create succeeds");
    let claimant = worker("w-events");
    repo.claim(&claimant, limit(1), Duration::from_secs(30))
        .await
        .expect("claim succeeds");
    let changed = tokio::time::timeout(Duration::from_secs(1), listener.changed())
        .await
        .expect("claim notifies within a second")
        .expect("notification received");
    assert_eq!(changed, job.id);

    repo.set_progress(
        job.id,
        &claimant,
        ProgressPercent::new(40).expect("progress"),
    )
    .await
    .expect("progress persists");
    let changed = tokio::time::timeout(Duration::from_secs(1), listener.changed())
        .await
        .expect("progress notifies within a second")
        .expect("notification received");
    assert_eq!(changed, job.id);

    repo.heartbeat(job.id, &claimant, Duration::from_secs(30))
        .await
        .expect("heartbeat succeeds");
    assert!(
        tokio::time::timeout(Duration::from_millis(300), listener.changed())
            .await
            .is_err(),
        "a lease-only update must not notify the events channel"
    );

    repo.mark_succeeded(job.id, &claimant, concat_result())
        .await
        .expect("job succeeds");
    let changed = tokio::time::timeout(Duration::from_secs(1), listener.changed())
        .await
        .expect("terminal state notifies within a second")
        .expect("notification received");
    assert_eq!(changed, job.id);
    db.dispose().await;
}

#[tokio::test]
async fn unpublished_callbacks_cover_terminal_rows_with_a_queue_only() {
    let db = fresh_db().await;
    let repository = db.repository();
    let queue = QueueUrl::new("http://127.0.0.1:9324/queue/outcomes").expect("valid url");

    let plain = repository
        .create(new_job("owner-1", "req-plain"))
        .await
        .expect("create succeeds");
    let waiting = repository
        .create(new_job("owner-1", "req-waiting-cb").with_callback(queue.clone()))
        .await
        .expect("create succeeds");
    let cancelled = repository
        .create(new_job("owner-1", "req-cancelled-cb").with_callback(queue.clone()))
        .await
        .expect("create succeeds");
    repository
        .mark_cancelled(cancelled.id)
        .await
        .expect("cancel succeeds");
    repository
        .mark_cancelled(plain.id)
        .await
        .expect("cancel succeeds");

    let unpublished = repository
        .unpublished_callbacks(limit(10))
        .await
        .expect("listing succeeds");
    let ids: Vec<JobId> = unpublished.iter().map(|record| record.id).collect();
    assert_eq!(ids, vec![cancelled.id]);

    assert!(
        repository
            .claim_callback(cancelled.id)
            .await
            .expect("claim succeeds"),
        "the first claim wins"
    );
    assert!(
        !repository
            .claim_callback(cancelled.id)
            .await
            .expect("claim succeeds"),
        "a second claim must lose"
    );
    assert!(
        repository
            .unpublished_callbacks(limit(10))
            .await
            .expect("listing succeeds")
            .is_empty()
    );
    assert!(
        !repository
            .claim_callback(waiting.id)
            .await
            .expect("claim succeeds"),
        "a non-terminal job is not publishable"
    );
    assert!(
        !repository
            .claim_callback(plain.id)
            .await
            .expect("claim succeeds"),
        "a job without a callback is not publishable"
    );
    db.dispose().await;
}

#[tokio::test]
async fn a_reaped_job_with_a_callback_becomes_publishable() {
    let db = fresh_db().await;
    let repository = db.repository();
    let queue = QueueUrl::new("http://127.0.0.1:9324/queue/outcomes").expect("valid url");

    let overdue = repository
        .create(
            new_job_due(
                "owner-1",
                "req-overdue-cb",
                Some(SystemTime::now() - Duration::from_secs(1)),
            )
            .with_callback(queue),
        )
        .await
        .expect("create succeeds");
    let reaped = repository.fail_overdue().await.expect("reap succeeds");
    assert_eq!(reaped, vec![overdue.id]);

    let unpublished = repository
        .unpublished_callbacks(limit(10))
        .await
        .expect("listing succeeds");
    assert_eq!(unpublished.len(), 1);
    assert_eq!(unpublished[0].id, overdue.id);
    assert_eq!(unpublished[0].state, JobState::Failed);
    assert_eq!(
        unpublished[0]
            .failure
            .as_ref()
            .map(|failure| failure.code()),
        Some("timeout")
    );
    db.dispose().await;
}

#[tokio::test]
async fn a_job_keeps_the_trace_it_was_created_under() {
    let db = fresh_db().await;
    let repository = db.repository();
    let trace = TraceId::generate();
    let job = new_job("owner-trace", "req-trace-1").with_trace_id(trace);
    let created = repository.create(job).await.expect("create succeeds");
    assert_eq!(created.trace_id, Some(trace));
    let read = repository
        .get(created.id)
        .await
        .expect("read succeeds")
        .expect("job exists");
    assert_eq!(read.trace_id, Some(trace));
    let owned = repository
        .get_owned(&owner("owner-trace"), created.id)
        .await
        .expect("owned read succeeds")
        .expect("job exists");
    assert_eq!(owned.trace_id, Some(trace));
    let claimed = repository
        .claim(&worker("worker-trace"), limit(1), Duration::from_secs(30))
        .await
        .expect("claim succeeds");
    assert_eq!(claimed[0].trace_id, Some(trace));
    let page = repository
        .list(&owner("owner-trace"), &[], limit(10), None)
        .await
        .expect("list succeeds");
    assert_eq!(page.jobs[0].trace_id, Some(trace));
    db.dispose().await;
}

#[tokio::test]
async fn a_job_created_without_a_trace_reads_back_without_one() {
    let db = fresh_db().await;
    let repository = db.repository();
    let created = repository
        .create(new_job("owner-no-trace", "req-trace-2"))
        .await
        .expect("create succeeds");
    assert_eq!(created.trace_id, None);
    db.dispose().await;
}

#[tokio::test]
async fn the_pending_count_is_the_jobs_waiting_to_be_claimed() {
    let db = fresh_db().await;
    let repository = db.repository();
    assert_eq!(repository.pending_count().await.expect("count succeeds"), 0);
    let first = repository
        .create(new_job("owner-depth", "req-depth-1"))
        .await
        .expect("create succeeds");
    let second = repository
        .create(new_job("owner-depth", "req-depth-2"))
        .await
        .expect("create succeeds");
    assert_eq!(repository.pending_count().await.expect("count succeeds"), 2);

    let claimed = repository
        .claim(&worker("worker-depth"), limit(1), Duration::from_secs(30))
        .await
        .expect("claim succeeds");
    assert_eq!(claimed.len(), 1);
    assert_eq!(repository.pending_count().await.expect("count succeeds"), 1);

    repository
        .mark_cancelled(second.id)
        .await
        .expect("cancel succeeds");
    assert_eq!(repository.pending_count().await.expect("count succeeds"), 0);
    assert_ne!(claimed[0].id, second.id);
    assert_ne!(first.id, second.id);
    db.dispose().await;
}

#[tokio::test]
async fn a_template_round_trips_and_is_owner_scoped() {
    let db = fresh_db().await;
    let repository = db.repository();
    let spec =
        OpSpec::Probe(ProbeSpec::new(MediaRef::object_uri("s3://bucket/key").unwrap()).unwrap());
    let template =
        Template::new(owner("owner-1"), "recipe", spec.clone(), vec![]).expect("a valid template");
    let stored = repository
        .create_template(template.clone())
        .await
        .expect("template stores");
    assert_eq!(stored.id(), template.id());
    assert_eq!(stored.owner(), template.owner());
    assert_eq!(stored.name().as_str(), "recipe");

    assert!(
        repository
            .get_template(&owner("owner-1"), template.id())
            .await
            .expect("read succeeds")
            .is_some()
    );
    assert!(
        repository
            .get_template(&owner("owner-2"), template.id())
            .await
            .expect("read succeeds")
            .is_none(),
        "another owner read the template"
    );

    let clash = Template::new(owner("owner-1"), "recipe", spec, vec![]).unwrap();
    assert!(matches!(
        repository.create_template(clash).await.unwrap_err(),
        JobStoreError::TemplateNameTaken { .. }
    ));

    assert_eq!(
        repository
            .list_templates(&owner("owner-1"))
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        repository
            .list_templates(&owner("owner-2"))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        !repository
            .delete_template(&owner("owner-2"), template.id())
            .await
            .unwrap()
    );
    assert!(
        repository
            .delete_template(&owner("owner-1"), template.id())
            .await
            .unwrap()
    );
    assert!(
        repository
            .get_template(&owner("owner-1"), template.id())
            .await
            .unwrap()
            .is_none()
    );
    db.dispose().await;
}

use std::env;
use std::net::{TcpStream, ToSocketAddrs};
use std::num::NonZeroU32;
use std::str::FromStr;
use std::time::{Duration, SystemTime};

use mm_domain::{
    ConcatSpec, JobFailure, JobId, JobRepository, JobState, JobStoreError, MediaRef, NewJob,
    OpResult, OpSpec, ProgressPercent, RequestId, TimedOutput, UserId, WorkerId,
};
use mm_jobstore::{MIGRATOR, PgJobRepository};
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
    NewJob::new(
        JobId::generate(),
        owner(owner_id),
        request(request_id),
        concat_spec(),
        Some(SystemTime::now() + Duration::from_secs(3600)),
    )
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
            .list(&owner("owner-1"), limit(2), cursor)
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
            JobFailure::new("TOOL_FAILED", "boom").expect("valid failure")
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
        .mark_failed(record.id, &a, failure.clone())
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
        .claim(&a, limit(1), Duration::from_millis(200))
        .await
        .expect("claim succeeds");
    assert_eq!(claimed.len(), 1);
    assert!(
        repo.requeue_expired()
            .await
            .expect("requeue succeeds")
            .is_empty()
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
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

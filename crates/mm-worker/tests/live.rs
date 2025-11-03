use std::env;
use std::net::{TcpStream, ToSocketAddrs};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mm_domain::{
    ConcatSpec, JobId, JobRecord, JobRepository, JobState, MediaRef, NewJob, ObjectRef,
    ObjectStore, OpResult, OpSpec, ProbeSpec, RequestId, UserId, WorkerId,
};
use mm_jobstore::{MIGRATOR, PgJobRepository};
use mm_storage::{CredentialSource, S3ObjectStore, S3Settings};
use mm_worker::{WorkerConfig, codes, start};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

const DEFAULT_ADMIN_URL: &str = "postgres://mm:mm@127.0.0.1:5433/mm";
const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:8333";
const DEFAULT_ACCESS_KEY: &str = "mm-test-access";
const DEFAULT_SECRET_KEY: &str = "mm-test-secret";
const BUCKET: &str = "mm-worker-it";

fn env_or(name: &str, fallback: &str) -> String {
    env::var(name).unwrap_or_else(|_| fallback.to_string())
}

fn admin_url() -> String {
    env_or("MM_DATABASE_URL", DEFAULT_ADMIN_URL)
}

fn store_endpoint() -> String {
    env_or("MM_OBJECTSTORE_ENDPOINT", DEFAULT_ENDPOINT)
}

fn require_reachable(authority: &str, what: &str) {
    let address = authority
        .to_socket_addrs()
        .expect("authority must resolve")
        .next()
        .expect("authority must resolve to an address");
    if TcpStream::connect_timeout(&address, Duration::from_secs(2)).is_err() {
        panic!(
            "{what} at {authority} is unreachable; \
             start the compose stack with `docker compose up -d` \
             before running mm-worker integration tests"
        );
    }
}

fn require_stack() {
    let db = admin_url();
    let db_authority = db
        .split('@')
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .expect("database url must contain an authority");
    require_reachable(db_authority, "database");
    let store = store_endpoint();
    let store_authority = store
        .strip_prefix("http://")
        .or_else(|| store.strip_prefix("https://"))
        .expect("endpoint must be an http(s) url")
        .trim_end_matches('/');
    require_reachable(store_authority, "objectstore");
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
    fn repository(&self) -> Arc<dyn JobRepository> {
        Arc::new(PgJobRepository::new(self.pool.clone()))
    }

    #[allow(dead_code)]
    fn url(&self) -> &str {
        &self.url
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

async fn test_store() -> Arc<S3ObjectStore> {
    let store = S3ObjectStore::connect(S3Settings {
        endpoint: Some(store_endpoint()),
        region: Some("us-east-1".to_string()),
        force_path_style: true,
        credentials: CredentialSource::Static {
            access_key_id: env_or("MM_OBJECTSTORE_ACCESS_KEY", DEFAULT_ACCESS_KEY),
            secret_access_key: env_or("MM_OBJECTSTORE_SECRET_KEY", DEFAULT_SECRET_KEY),
        },
    })
    .await;
    store.ensure_bucket(BUCKET).await.unwrap();
    Arc::new(store)
}

fn scratch_dir(part: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../scratch/s08/{part}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn fixture(name: &str, seconds: u32, fps: u32, container: &str) -> PathBuf {
    static GENERATION: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = GENERATION.lock().unwrap();
    let path = scratch_dir("fixtures").join(name);
    if path.is_file() && path.metadata().unwrap().len() > 0 {
        return path;
    }
    let mut command = Command::new("ffmpeg");
    command
        .arg("-y")
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg(format!(
            "testsrc2=duration={seconds}:size=1280x720:rate={fps}"
        ))
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg(format!(
            "sine=frequency=440:sample_rate=44100:duration={seconds}"
        ))
        .arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("veryfast")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-c:a")
        .arg("aac")
        .arg("-b:a")
        .arg("128k");
    if container == "mp4" {
        command.arg("-movflags").arg("+faststart");
    }
    let output = command
        .arg("-f")
        .arg(container)
        .arg(&path)
        .output()
        .expect("ffmpeg must be runnable to generate fixtures");
    assert!(output.status.success(), "fixture generation failed: {name}");
    path
}

async fn upload_input(store: &S3ObjectStore, path: &Path, key: &str) -> MediaRef {
    let payload = tokio::fs::read(path).await.unwrap();
    let length = payload.len() as u64;
    let object = ObjectRef::new(BUCKET, key).unwrap();
    store
        .put(&object, mm_domain::body_from_bytes(payload), length)
        .await
        .unwrap();
    MediaRef::object_uri(&format!("s3://{BUCKET}/{key}")).unwrap()
}

fn owner() -> UserId {
    UserId::from_str("worker-owner").unwrap()
}

fn worker_id(name: &str) -> WorkerId {
    WorkerId::from_str(name).unwrap()
}

fn concat_job(inputs: Vec<MediaRef>) -> NewJob {
    let id = JobId::generate();
    NewJob::new(
        id,
        owner(),
        RequestId::from_str(&format!("req-{id}")).unwrap(),
        OpSpec::Concat(ConcatSpec::new(inputs, None).unwrap()),
        None,
    )
}

fn base_config(db: &TestDb, store: Arc<S3ObjectStore>, name: &str) -> WorkerConfig {
    let mut config = WorkerConfig::new(
        db.repository(),
        store,
        BUCKET.to_string(),
        worker_id(name),
        scratch_dir("staging"),
    );
    config.reconcile_interval = Duration::from_millis(200);
    config
}

async fn await_terminal(
    repository: &Arc<dyn JobRepository>,
    id: JobId,
    timeout: Duration,
) -> JobRecord {
    let deadline = Instant::now() + timeout;
    loop {
        let record = repository
            .get(id)
            .await
            .expect("job read succeeds")
            .expect("job exists");
        if record.state.is_terminal() {
            return record;
        }
        if Instant::now() > deadline {
            panic!(
                "job {id} never reached a terminal state: {:?}",
                record.state
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn await_state(
    repository: &Arc<dyn JobRepository>,
    id: JobId,
    wanted: JobState,
    timeout: Duration,
) -> JobRecord {
    let deadline = Instant::now() + timeout;
    loop {
        let record = repository
            .get(id)
            .await
            .expect("job read succeeds")
            .expect("job exists");
        if record.state == wanted {
            return record;
        }
        if Instant::now() > deadline {
            panic!("job {id} never reached {wanted:?}: {:?}", record.state);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_concat_job_runs_to_succeeded_with_an_uploaded_output() {
    let db = fresh_db().await;
    let store = test_store().await;
    let first = fixture("merge-a-4s.mp4", 4, 30, "mp4");
    let second = fixture("merge-b-3s.mp4", 3, 30, "mp4");
    let refs = vec![
        upload_input(&store, &first, "inputs/merge-a-4s.mp4").await,
        upload_input(&store, &second, "inputs/merge-b-3s.mp4").await,
    ];
    let repository = db.repository();
    let job = repository.create(concat_job(refs)).await.unwrap();

    let handle = start(base_config(&db, store.clone(), "worker-single"));
    let record = await_terminal(&repository, job.id, Duration::from_secs(120)).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    assert_eq!(record.progress.value(), 100);
    let result = record.result.expect("result recorded");
    let OpResult::Concat(timed) = result else {
        panic!("expected a concat result");
    };
    assert!(
        (timed.duration_seconds() - 7.0).abs() < 0.6,
        "duration {}",
        timed.duration_seconds()
    );
    let MediaRef::Object(object) = timed.output() else {
        panic!("expected an object output");
    };
    assert_eq!(object.key(), format!("jobs/{}/output.mp4", job.id));
    let size = store.head(object).await.expect("output exists");
    assert!(size > 0);
    let staged = scratch_dir("staging").join(format!("{}.mp4", job.id));
    assert!(!staged.exists(), "staging file not cleaned: {staged:?}");
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn parallel_jobs_never_exceed_the_concurrency_bound() {
    let db = fresh_db().await;
    let store = test_store().await;
    let first = fixture("par-a-8s.mp4", 8, 30, "mp4");
    let second = fixture("par-b-8s.mp4", 8, 25, "mp4");
    let refs = vec![
        upload_input(&store, &first, "inputs/par-a-8s.mp4").await,
        upload_input(&store, &second, "inputs/par-b-8s.mp4").await,
    ];
    let repository = db.repository();
    let mut ids = Vec::new();
    for _ in 0..5 {
        ids.push(
            repository
                .create(concat_job(refs.clone()))
                .await
                .unwrap()
                .id,
        );
    }

    let mut config = base_config(&db, store.clone(), "worker-parallel");
    config.max_concurrent = 2;
    config.claim_batch = NonZeroU32::new(4).unwrap();
    let handle = start(config);

    let deadline = Instant::now() + Duration::from_secs(240);
    let mut max_running = 0usize;
    loop {
        let mut running = 0usize;
        let mut terminal = 0usize;
        for id in &ids {
            let record = repository.get(*id).await.unwrap().unwrap();
            match record.state {
                JobState::Running => running += 1,
                state if state.is_terminal() => terminal += 1,
                _ => {}
            }
        }
        max_running = max_running.max(running);
        if terminal == ids.len() {
            break;
        }
        assert!(Instant::now() < deadline, "jobs never finished");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    handle.shutdown().await;

    assert!(max_running <= 2, "observed {max_running} concurrent jobs");
    assert_eq!(max_running, 2, "concurrency never reached the bound");
    for id in ids {
        let record = repository.get(id).await.unwrap().unwrap();
        assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    }
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_killed_workers_lease_expires_and_another_worker_completes_the_job() {
    let db = fresh_db().await;
    let store = test_store().await;
    let first = fixture("long-a-30s.mp4", 30, 30, "mp4");
    let second = fixture("long-b-30s.mp4", 30, 30, "mp4");
    let refs = vec![
        upload_input(&store, &first, "inputs/long-a-30s.mp4").await,
        upload_input(&store, &second, "inputs/long-b-30s.mp4").await,
    ];
    let repository = db.repository();
    let job = repository.create(concat_job(refs)).await.unwrap();

    let mut doomed = base_config(&db, store.clone(), "worker-doomed");
    doomed.lease = Duration::from_secs(2);
    doomed.heartbeat_interval = Duration::from_secs(3600);
    doomed.reconcile_interval = Duration::from_millis(100);
    let doomed_handle = start(doomed);

    await_state(
        &repository,
        job.id,
        JobState::Running,
        Duration::from_secs(15),
    )
    .await;
    let watched = Instant::now() + Duration::from_secs(20);
    let mut last_progress = 0u8;
    loop {
        let record = repository.get(job.id).await.unwrap().unwrap();
        assert!(
            record.progress.value() >= last_progress,
            "progress regressed from {last_progress} to {}",
            record.progress.value()
        );
        last_progress = record.progress.value();
        if last_progress > 0 {
            break;
        }
        assert!(
            Instant::now() < watched,
            "no progress was persisted while running"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    doomed_handle.abort();

    let mut rescuer = base_config(&db, store.clone(), "worker-rescuer");
    rescuer.lease = Duration::from_secs(15);
    rescuer.heartbeat_interval = Duration::from_secs(2);
    rescuer.reconcile_interval = Duration::from_millis(200);
    let rescuer_handle = start(rescuer);

    let record = await_terminal(&repository, job.id, Duration::from_secs(240)).await;
    rescuer_handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    assert_eq!(record.claimed_by, Some(worker_id("worker-rescuer")));
    assert_eq!(record.attempts, 2);
    let OpResult::Concat(timed) = record.result.expect("result recorded") else {
        panic!("expected a concat result");
    };
    let MediaRef::Object(object) = timed.output() else {
        panic!("expected an object output");
    };
    assert!(store.head(object).await.expect("output exists") > 0);
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn blocking_validation_fails_the_job_with_input_codes() {
    let db = fresh_db().await;
    let store = test_store().await;
    let foreign = fixture("blocked.mkv", 2, 30, "matroska");
    let good = fixture("merge-b-3s.mp4", 3, 30, "mp4");
    let refs = vec![
        upload_input(&store, &foreign, "inputs/blocked.mkv").await,
        upload_input(&store, &good, "inputs/merge-b-3s.mp4").await,
    ];
    let repository = db.repository();
    let job = repository.create(concat_job(refs)).await.unwrap();

    let handle = start(base_config(&db, store.clone(), "worker-blocked"));
    let record = await_terminal(&repository, job.id, Duration::from_secs(60)).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Failed);
    let failure = record.failure.expect("failure recorded");
    assert_eq!(failure.code(), codes::VALIDATION_BLOCKED);
    assert!(
        failure.message().contains("input 0: UnsupportedContainer"),
        "{}",
        failure.message()
    );
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unsupported_operation_fails_with_a_typed_code() {
    let db = fresh_db().await;
    let store = test_store().await;
    let repository = db.repository();
    let id = JobId::generate();
    let job = repository
        .create(NewJob::new(
            id,
            owner(),
            RequestId::from_str(&format!("req-{id}")).unwrap(),
            OpSpec::Probe(ProbeSpec::new(
                MediaRef::object_uri(&format!("s3://{BUCKET}/inputs/none.mp4")).unwrap(),
            )),
            None,
        ))
        .await
        .unwrap();

    let handle = start(base_config(&db, store, "worker-unsupported"));
    let record = await_terminal(&repository, job.id, Duration::from_secs(30)).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Failed);
    let failure = record.failure.expect("failure recorded");
    assert_eq!(failure.code(), codes::UNSUPPORTED_OPERATION);
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_passed_deadline_fails_before_any_work() {
    let db = fresh_db().await;
    let store = test_store().await;
    let repository = db.repository();
    let id = JobId::generate();
    let refs = vec![
        MediaRef::object_uri(&format!("s3://{BUCKET}/inputs/a.mp4")).unwrap(),
        MediaRef::object_uri(&format!("s3://{BUCKET}/inputs/b.mp4")).unwrap(),
    ];
    let job = repository
        .create(NewJob::new(
            id,
            owner(),
            RequestId::from_str(&format!("req-{id}")).unwrap(),
            OpSpec::Concat(ConcatSpec::new(refs, None).unwrap()),
            Some(std::time::SystemTime::now() - Duration::from_secs(60)),
        ))
        .await
        .unwrap();

    let handle = start(base_config(&db, store, "worker-overdue"));
    let record = await_terminal(&repository, job.id, Duration::from_secs(30)).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Failed);
    let failure = record.failure.expect("failure recorded");
    assert_eq!(failure.code(), codes::INPUT_UNRESOLVABLE);
    db.dispose().await;
}

use std::env;
use std::net::{TcpStream, ToSocketAddrs};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use mm_domain::{
    CallbackError, CallbackPublisher, ConcatSpec, JobId, JobRecord, JobRepository, JobState,
    MediaRef, NewJob, ObjectBody, ObjectRef, ObjectStore, ObjectStoreError, OpResult, OpSpec,
    OutputProfile, ProbeSpec, QueueUrl, RequestId, TranscodeSpec, TrimSpec, UserId, WorkerId,
    collect_body,
};
use mm_jobstore::{MIGRATOR, PgJobListener, PgJobRepository};
use mm_storage::{CredentialSource, S3ObjectStore, S3Settings};
use mm_worker::{DiskBudget, RetryPolicy, WorkerConfig, codes, start};
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
    mm_testkit::upload_object(store, path, BUCKET, key).await
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

fn base_config(db: &TestDb, store: Arc<dyn ObjectStore>, name: &str) -> WorkerConfig {
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
    let _slot = mm_testkit::stack_slot();
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

fn single_input_job(spec: OpSpec) -> NewJob {
    let id = JobId::generate();
    NewJob::new(
        id,
        owner(),
        RequestId::from_str(&format!("req-{id}")).unwrap(),
        spec,
        None,
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_probe_job_succeeds_with_stream_metadata() {
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("probe-4s.mp4", 4, 30, "mp4");
    let input = upload_input(&store, &source, "inputs/probe-4s.mp4").await;
    let repository = db.repository();
    let job = repository
        .create(single_input_job(OpSpec::Probe(ProbeSpec::new(input))))
        .await
        .unwrap();

    let handle = start(base_config(&db, store.clone(), "worker-probe"));
    let record = await_terminal(&repository, job.id, Duration::from_secs(60)).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let OpResult::Probe(probe) = record.result.expect("result recorded") else {
        panic!("expected a probe result");
    };
    assert!(
        (probe.duration_seconds() - 4.0).abs() < 0.5,
        "duration {}",
        probe.duration_seconds()
    );
    assert_eq!(probe.video_streams(), 1);
    assert_eq!(probe.audio_streams(), 1);
    assert!(
        probe.container().split(',').any(|token| token == "mp4"),
        "container {}",
        probe.container()
    );
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_transcode_job_succeeds_with_a_verified_output() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("transcode-30fps-4s.mp4", 4, 30, "mp4");
    let input = upload_input(&store, &source, "inputs/transcode-30fps-4s.mp4").await;
    let profile = OutputProfile::from_raw(24.0, 0, 0, 0, 48_000, 640, 360).unwrap();
    let repository = db.repository();
    let job = repository
        .create(single_input_job(OpSpec::Transcode(
            TranscodeSpec::new(input, profile).unwrap(),
        )))
        .await
        .unwrap();

    let handle = start(base_config(&db, store.clone(), "worker-transcode"));
    let record = await_terminal(&repository, job.id, Duration::from_secs(120)).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    assert_eq!(record.progress.value(), 100);
    let OpResult::Transcode(result) = record.result.expect("result recorded") else {
        panic!("expected a transcode result");
    };
    let MediaRef::Object(object) = result.output().clone() else {
        panic!("expected an object output");
    };
    assert_eq!(object.key(), format!("jobs/{}/output.mp4", job.id));

    let body = store.get(&object).await.expect("output fetches");
    let bytes = collect_body(body).await.expect("output body collects");
    let local = scratch_dir("verify").join(format!("{}.mp4", job.id));
    tokio::fs::write(&local, &bytes).await.unwrap();
    let info = mm_ffmpeg::Prober::default()
        .probe(&local.to_string_lossy(), &mm_ffmpeg::ProbeLimits::default())
        .await
        .expect("output probes");
    assert!(info.container.split(',').any(|token| token == "mp4"));
    assert!((info.duration.as_secs_f64() - 4.0).abs() < 0.5);
    assert_eq!(info.video.len(), 1);
    assert_eq!(info.video[0].codec, "h264");
    assert_eq!(info.video[0].width, 640);
    assert_eq!(info.video[0].height, 360);
    assert!((info.video[0].avg_fps.unwrap() - 24.0).abs() < 0.1);
    assert_eq!(info.audio[0].codec, "aac");
    assert_eq!(info.audio[0].sample_rate, Some(48_000));
    let staged = scratch_dir("staging").join(format!("{}.mp4", job.id));
    assert!(!staged.exists(), "staging file not cleaned: {staged:?}");
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blocked_transcode_fails_with_the_input_code() {
    let db = fresh_db().await;
    let store = test_store().await;
    let foreign = fixture("blocked.mkv", 2, 30, "matroska");
    let input = upload_input(&store, &foreign, "inputs/blocked.mkv").await;
    let profile = OutputProfile::from_raw(24.0, 0, 0, 0, 0, 0, 0).unwrap();
    let repository = db.repository();
    let job = repository
        .create(single_input_job(OpSpec::Transcode(
            TranscodeSpec::new(input, profile).unwrap(),
        )))
        .await
        .unwrap();

    let handle = start(base_config(&db, store.clone(), "worker-transcode-block"));
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
    assert!(
        !failure.message().contains("TooFewInputs"),
        "single input tripped the concat input-count rule: {}",
        failure.message()
    );
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn parallel_jobs_never_exceed_the_concurrency_bound() {
    let _slot = mm_testkit::stack_slot();
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
    let _slot = mm_testkit::stack_slot();
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
    let watched = Instant::now() + Duration::from_secs(60);
    loop {
        let record = repository.get(job.id).await.unwrap().unwrap();
        if record.progress.value() > 0 {
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
    assert!(record.attempts >= 2, "attempts {}", record.attempts);
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
    assert_eq!(record.attempts, 1, "permanent failure was retried");
    assert!(record.next_attempt_at.is_none());
    db.dispose().await;
}

#[derive(Default)]
struct CountingFailures {
    attempts: std::sync::atomic::AtomicU32,
}

#[async_trait]
impl CallbackPublisher for CountingFailures {
    async fn publish(&self, _queue: &QueueUrl, _message: &str) -> Result<(), CallbackError> {
        self.attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(CallbackError::Backend {
            message: "injected callback failure".to_string(),
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_callback_publish_failure_never_changes_the_terminal_state() {
    let db = fresh_db().await;
    let store = test_store().await;
    let foreign = fixture("blocked.mkv", 2, 30, "matroska");
    let good = fixture("merge-b-3s.mp4", 3, 30, "mp4");
    let refs = vec![
        upload_input(&store, &foreign, "inputs/blocked-cb.mkv").await,
        upload_input(&store, &good, "inputs/merge-b-3s-cb.mp4").await,
    ];
    let repository = db.repository();
    let queue = QueueUrl::new("http://127.0.0.1:9324/queue/mm-worker-absent").unwrap();
    let job = repository
        .create(concat_job(refs).with_callback(queue))
        .await
        .unwrap();

    let publisher = Arc::new(CountingFailures::default());
    let mut config = base_config(&db, store.clone(), "worker-cb-fail");
    config.retry = RetryPolicy {
        max_attempts: 2,
        base: Duration::from_millis(50),
        cap: Duration::from_millis(100),
    };
    config.callbacks = Some(publisher.clone());
    let handle = start(config);
    let record = await_terminal(&repository, job.id, Duration::from_secs(60)).await;

    let settle = Instant::now() + Duration::from_secs(10);
    while publisher.attempts.load(std::sync::atomic::Ordering::SeqCst) < 2
        && Instant::now() < settle
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Failed);
    assert_eq!(
        publisher.attempts.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "publish attempts are capped by the retry policy"
    );
    let refreshed = repository
        .get(job.id)
        .await
        .expect("get succeeds")
        .expect("job present");
    assert_eq!(refreshed.state, JobState::Failed);
    assert_eq!(
        refreshed.failure.expect("failure recorded").code(),
        codes::VALIDATION_BLOCKED
    );
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blocked_job_publishes_its_error_code_and_file_errors() {
    let db = fresh_db().await;
    let store = test_store().await;
    let queue_endpoint = env_or("MM_QUEUE_ENDPOINT", "http://127.0.0.1:9324");
    let queue_authority = queue_endpoint
        .strip_prefix("http://")
        .or_else(|| queue_endpoint.strip_prefix("https://"))
        .expect("endpoint must be an http(s) url")
        .trim_end_matches('/');
    require_reachable(queue_authority, "queue");
    let publisher = Arc::new(
        mm_storage::SqsCallbackPublisher::connect(mm_storage::SqsSettings {
            endpoint: Some(queue_endpoint.clone()),
            region: Some("local".to_string()),
            credentials: CredentialSource::Static {
                access_key_id: "queue-access".to_string(),
                secret_access_key: "queue-secret".to_string(),
            },
        })
        .await,
    );
    let queue = publisher
        .ensure_queue(&format!("mm-worker-cb-{}", Uuid::now_v7().simple()))
        .await
        .expect("queue exists");

    let foreign = fixture("blocked.mkv", 2, 30, "matroska");
    let good = fixture("merge-b-3s.mp4", 3, 30, "mp4");
    let refs = vec![
        upload_input(&store, &foreign, "inputs/blocked-queue.mkv").await,
        upload_input(&store, &good, "inputs/merge-b-3s-queue.mp4").await,
    ];
    let repository = db.repository();
    let job = repository
        .create(concat_job(refs.clone()).with_callback(queue.clone()))
        .await
        .unwrap();

    let mut config = base_config(&db, store.clone(), "worker-cb-blocked");
    config.callbacks = Some(publisher.clone());
    let handle = start(config);
    let record = await_terminal(&repository, job.id, Duration::from_secs(60)).await;
    assert_eq!(record.state, JobState::Failed);

    let settle = Instant::now() + mm_testkit::budget::QUEUE_SETTLE;
    let bodies = loop {
        let received = tokio::time::timeout(
            mm_testkit::budget::QUEUE_RECEIVE,
            publisher.receive(&queue, Duration::from_secs(2)),
        )
        .await
        .expect("receive answers in time")
        .expect("receive succeeds");
        if !received.is_empty() {
            break received;
        }
        assert!(Instant::now() < settle, "no callback message arrived");
    };
    handle.shutdown().await;

    assert_eq!(bodies.len(), 1);
    let decoded = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(&bodies[0])
            .expect("message is base64")
    };
    let outcome = <mm_proto::v1::JobOutcome as prost::Message>::decode(decoded.as_slice())
        .expect("message is a job outcome");
    assert_eq!(outcome.job_id, job.id.to_string());
    assert_eq!(outcome.request_id, job.request.to_string());
    assert_eq!(outcome.state, mm_proto::v1::JobState::Failed as i32);
    assert_eq!(outcome.error_code, codes::VALIDATION_BLOCKED);
    assert!(outcome.result.is_none());
    assert!(!outcome.file_errors.is_empty());
    assert_eq!(outcome.file_errors[0].code, "UnsupportedContainer");
    assert_eq!(
        outcome.file_errors[0].media_ref,
        Some(mm_proto::v1::MediaRef::from(&refs[0]))
    );
    publisher.delete_queue(&queue).await.expect("queue removed");
    db.dispose().await;
}

struct FailingUploads {
    inner: Arc<S3ObjectStore>,
}

#[async_trait]
impl ObjectStore for FailingUploads {
    async fn get(&self, object: &ObjectRef) -> Result<ObjectBody, ObjectStoreError> {
        self.inner.get(object).await
    }

    async fn put(
        &self,
        object: &ObjectRef,
        body: ObjectBody,
        length: u64,
    ) -> Result<(), ObjectStoreError> {
        self.inner.put(object, body, length).await
    }

    async fn put_multipart(
        &self,
        _object: &ObjectRef,
        _body: ObjectBody,
    ) -> Result<(), ObjectStoreError> {
        Err(ObjectStoreError::Backend {
            message: "injected upload outage".to_string(),
        })
    }

    async fn head(&self, object: &ObjectRef) -> Result<u64, ObjectStoreError> {
        self.inner.head(object).await
    }

    async fn delete(&self, object: &ObjectRef) -> Result<(), ObjectStoreError> {
        self.inner.delete(object).await
    }

    async fn presign_get(
        &self,
        object: &ObjectRef,
        expires_in: Duration,
    ) -> Result<String, ObjectStoreError> {
        self.inner.presign_get(object, expires_in).await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_transient_upload_failure_retries_with_growing_gaps_until_failed() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let first = fixture("retry-a-1s.mp4", 1, 30, "mp4");
    let second = fixture("retry-b-1s.mp4", 1, 30, "mp4");
    let refs = vec![
        upload_input(&store, &first, "inputs/retry-a-1s.mp4").await,
        upload_input(&store, &second, "inputs/retry-b-1s.mp4").await,
    ];
    let repository = db.repository();
    let job = repository.create(concat_job(refs)).await.unwrap();

    let flaky = Arc::new(FailingUploads {
        inner: store.clone(),
    });
    let mut config = base_config(&db, flaky, "worker-flaky");
    config.retry = RetryPolicy {
        max_attempts: 3,
        base: Duration::from_secs(1),
        cap: Duration::from_secs(60),
    };
    config.reconcile_interval = Duration::from_millis(100);
    let handle = start(config);

    let mut schedules: Vec<std::time::SystemTime> = Vec::new();
    let mut seen = 0u32;
    let deadline = Instant::now() + Duration::from_secs(120);
    let record = loop {
        let record = repository.get(job.id).await.unwrap().unwrap();
        if record.state == JobState::Pending
            && record.attempts > seen
            && let Some(due) = record.next_attempt_at
        {
            seen = record.attempts;
            schedules.push(due);
        }
        if record.state.is_terminal() {
            break record;
        }
        assert!(Instant::now() < deadline, "job never reached FAILED");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Failed, "{:?}", record.failure);
    let failure = record.failure.expect("failure recorded");
    assert_eq!(failure.code(), codes::OUTPUT_UPLOAD_FAILED);
    assert!(
        failure.message().contains("injected"),
        "{}",
        failure.message()
    );
    assert_eq!(record.attempts, 3);
    assert_eq!(
        schedules.len(),
        2,
        "observed retry schedules: {schedules:?}"
    );
    assert!(
        schedules[1] > schedules[0],
        "the second schedule never advanced past the first: {schedules:?}"
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
            OpSpec::Trim(
                TrimSpec::new(
                    MediaRef::object_uri(&format!("s3://{BUCKET}/inputs/none.mp4")).unwrap(),
                    0.0,
                    2.0,
                )
                .unwrap(),
            ),
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
    assert!(failure.message().contains("trim"), "{}", failure.message());
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
    assert_eq!(failure.code(), codes::TIMEOUT);
    assert_eq!(record.attempts, 0, "overdue pending job was executed");
    assert!(record.started_at.is_none());
    db.dispose().await;
}

fn process_mentions(pid: u32, needle: &str) -> bool {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|bytes| String::from_utf8_lossy(&bytes).contains(needle))
        .unwrap_or(false)
}

fn find_process(needle: &str) -> Option<u32> {
    let entries = std::fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        if let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>()
            && process_mentions(pid, needle)
        {
            return Some(pid);
        }
    }
    None
}

#[tokio::test(flavor = "multi_thread")]
async fn an_overdue_running_job_is_killed_and_fails_with_timeout() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let first = fixture("long-a-120s.mp4", 120, 30, "mp4");
    let second = fixture("long-b-120s.mp4", 120, 30, "mp4");
    let refs = vec![
        upload_input(&store, &first, "inputs/long-a-120s.mp4").await,
        upload_input(&store, &second, "inputs/long-b-120s.mp4").await,
    ];
    let repository = db.repository();
    let job = repository.create(concat_job(refs)).await.unwrap();

    let mut config = base_config(&db, store.clone(), "worker-reaped");
    config.heartbeat_interval = Duration::from_millis(300);
    let handle = start(config);
    await_state(
        &repository,
        job.id,
        JobState::Running,
        Duration::from_secs(60),
    )
    .await;
    let needle = job.id.to_string();
    let found = Instant::now() + Duration::from_secs(120);
    let pid = loop {
        if let Some(pid) = find_process(&needle) {
            break pid;
        }
        assert!(Instant::now() < found, "merge process never appeared");
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    sqlx::query("UPDATE jobs SET deadline_at = now() WHERE id = $1")
        .bind(job.id.as_uuid())
        .execute(&db.pool)
        .await
        .expect("deadline update succeeds");

    let record = await_terminal(&repository, job.id, Duration::from_secs(30)).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Failed, "{:?}", record.failure);
    let failure = record.failure.expect("failure recorded");
    assert_eq!(failure.code(), codes::TIMEOUT);
    let gone = Instant::now() + mm_testkit::budget::PROCESS_EXIT;
    while process_mentions(pid, &needle) {
        assert!(Instant::now() < gone, "merge process survived the reap");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let staged = scratch_dir("staging").join(format!("{}.mp4", job.id));
    assert!(!staged.exists(), "staging file not cleaned: {staged:?}");
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_job_is_released_until_disk_frees() {
    let _slot = mm_testkit::stack_slot();
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

    let mut tight = base_config(&db, store.clone(), "worker-tight");
    tight.disk_budget = DiskBudget::Fixed(1024);
    tight.release_retry = Duration::from_millis(300);
    let tight_handle = start(tight);

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let record = repository.get(job.id).await.unwrap().unwrap();
        assert!(
            !record.state.is_terminal(),
            "job finished under a tight budget: {:?}",
            record.state
        );
        if record.state == JobState::Pending && record.next_attempt_at.is_some() {
            assert_eq!(record.attempts, 0);
            break;
        }
        assert!(Instant::now() < deadline, "job was never released");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    tight_handle.shutdown().await;

    let roomy = base_config(&db, store.clone(), "worker-roomy");
    let roomy_handle = start(roomy);
    let record = await_terminal(&repository, job.id, Duration::from_secs(120)).await;
    roomy_handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_created_job_is_claimed_via_notify_well_before_any_tick() {
    let db = fresh_db().await;
    let store = test_store().await;
    let repository = db.repository();

    let mut config = base_config(&db, store, "worker-notified");
    config.reconcile_interval = Duration::from_secs(60);
    config.notifications = Some(Arc::new(
        PgJobListener::connect(db.url())
            .await
            .expect("listener connects"),
    ));
    let handle = start(config);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut fastest = Duration::MAX;
    for attempt in 0..3 {
        let id = JobId::generate();
        let created = Instant::now();
        repository
            .create(NewJob::new(
                id,
                owner(),
                RequestId::from_str(&format!("req-{id}-{attempt}")).unwrap(),
                OpSpec::Trim(
                    TrimSpec::new(
                        MediaRef::object_uri(&format!("s3://{BUCKET}/inputs/none.mp4")).unwrap(),
                        0.0,
                        2.0,
                    )
                    .unwrap(),
                ),
                None,
            ))
            .await
            .unwrap();
        let waited = Instant::now() + Duration::from_secs(10);
        loop {
            let record = repository.get(id).await.unwrap().unwrap();
            if record.state != JobState::Pending {
                break;
            }
            assert!(
                Instant::now() < waited,
                "the job was never claimed; the notify wakeup is broken"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        fastest = fastest.min(created.elapsed());
        if fastest < Duration::from_secs(1) {
            break;
        }
    }
    assert!(
        fastest < mm_testkit::budget::PICKUP,
        "no claim landed inside the notify budget over three attempts, \
         far below the one-minute tick; fastest {fastest:?}"
    );
    handle.shutdown().await;
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_listener_a_job_still_starts_within_one_tick() {
    let db = fresh_db().await;
    let store = test_store().await;
    let repository = db.repository();

    let mut config = base_config(&db, store, "worker-ticking");
    config.reconcile_interval = Duration::from_secs(1);
    config.notifications = None;
    let handle = start(config);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let id = JobId::generate();
    let created = Instant::now();
    repository
        .create(NewJob::new(
            id,
            owner(),
            RequestId::from_str(&format!("req-{id}")).unwrap(),
            OpSpec::Trim(
                TrimSpec::new(
                    MediaRef::object_uri(&format!("s3://{BUCKET}/inputs/none.mp4")).unwrap(),
                    0.0,
                    2.0,
                )
                .unwrap(),
            ),
            None,
        ))
        .await
        .unwrap();
    loop {
        let record = repository.get(id).await.unwrap().unwrap();
        if record.state != JobState::Pending {
            break;
        }
        assert!(
            created.elapsed() < mm_testkit::budget::TICK_PICKUP,
            "the reconcile tick never claimed the job inside the budget"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    handle.shutdown().await;
    db.dispose().await;
}

async fn queue_publisher() -> Arc<mm_storage::SqsCallbackPublisher> {
    let queue_endpoint = env_or("MM_QUEUE_ENDPOINT", "http://127.0.0.1:9324");
    let queue_authority = queue_endpoint
        .strip_prefix("http://")
        .or_else(|| queue_endpoint.strip_prefix("https://"))
        .expect("endpoint must be an http(s) url")
        .trim_end_matches('/');
    require_reachable(queue_authority, "queue");
    Arc::new(
        mm_storage::SqsCallbackPublisher::connect(mm_storage::SqsSettings {
            endpoint: Some(queue_endpoint),
            region: Some("local".to_string()),
            credentials: CredentialSource::Static {
                access_key_id: "queue-access".to_string(),
                secret_access_key: "queue-secret".to_string(),
            },
        })
        .await,
    )
}

async fn one_outcome(
    publisher: &mm_storage::SqsCallbackPublisher,
    queue: &QueueUrl,
) -> mm_proto::v1::JobOutcome {
    let settle = Instant::now() + mm_testkit::budget::QUEUE_SETTLE;
    let bodies = loop {
        let received = tokio::time::timeout(
            mm_testkit::budget::QUEUE_RECEIVE,
            publisher.receive(queue, Duration::from_secs(2)),
        )
        .await
        .expect("receive answers in time")
        .expect("receive succeeds");
        if !received.is_empty() {
            break received;
        }
        assert!(Instant::now() < settle, "no callback message arrived");
    };
    assert_eq!(bodies.len(), 1, "exactly one callback message");
    let leftover = tokio::time::timeout(
        mm_testkit::budget::QUEUE_RECEIVE,
        publisher.receive(queue, Duration::from_secs(1)),
    )
    .await
    .expect("receive answers in time")
    .expect("receive succeeds");
    assert!(leftover.is_empty(), "a second callback message arrived");
    let decoded = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(&bodies[0])
            .expect("message is base64")
    };
    <mm_proto::v1::JobOutcome as prost::Message>::decode(decoded.as_slice())
        .expect("message is a job outcome")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_cancelled_while_pending_still_delivers_one_callback() {
    let db = fresh_db().await;
    let store = test_store().await;
    let publisher = queue_publisher().await;
    let queue = publisher
        .ensure_queue(&format!("mm-worker-pc-{}", Uuid::now_v7().simple()))
        .await
        .expect("queue exists");

    let refs = vec![
        MediaRef::object_uri(&format!("s3://{BUCKET}/inputs/none-a.mp4")).unwrap(),
        MediaRef::object_uri(&format!("s3://{BUCKET}/inputs/none-b.mp4")).unwrap(),
    ];
    let repository = db.repository();
    let job = repository
        .create(concat_job(refs).with_callback(queue.clone()))
        .await
        .unwrap();
    repository.mark_cancelled(job.id).await.unwrap();

    let mut config = base_config(&db, store, "worker-pending-cancel");
    config.callbacks = Some(publisher.clone());
    let handle = start(config);

    let outcome = one_outcome(&publisher, &queue).await;
    handle.shutdown().await;
    assert_eq!(outcome.job_id, job.id.to_string());
    assert_eq!(outcome.request_id, job.request.to_string());
    assert_eq!(outcome.state, mm_proto::v1::JobState::Cancelled as i32);
    assert_eq!(outcome.error_code, "");
    assert!(outcome.result.is_none());
    publisher.delete_queue(&queue).await.expect("queue removed");
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deadline_reaped_job_still_delivers_one_callback() {
    let db = fresh_db().await;
    let store = test_store().await;
    let publisher = queue_publisher().await;
    let queue = publisher
        .ensure_queue(&format!("mm-worker-rc-{}", Uuid::now_v7().simple()))
        .await
        .expect("queue exists");

    let refs = vec![
        MediaRef::object_uri(&format!("s3://{BUCKET}/inputs/none-a.mp4")).unwrap(),
        MediaRef::object_uri(&format!("s3://{BUCKET}/inputs/none-b.mp4")).unwrap(),
    ];
    let repository = db.repository();
    let id = JobId::generate();
    let job = repository
        .create(
            NewJob::new(
                id,
                owner(),
                RequestId::from_str(&format!("req-{id}")).unwrap(),
                OpSpec::Concat(ConcatSpec::new(refs, None).unwrap()),
                Some(std::time::SystemTime::now() - Duration::from_secs(1)),
            )
            .with_callback(queue.clone()),
        )
        .await
        .unwrap();

    let mut config = base_config(&db, store, "worker-reaped");
    config.callbacks = Some(publisher.clone());
    let handle = start(config);

    let outcome = one_outcome(&publisher, &queue).await;
    handle.shutdown().await;
    let record = repository.get(job.id).await.unwrap().expect("job exists");
    assert_eq!(record.state, JobState::Failed);
    assert_eq!(outcome.job_id, job.id.to_string());
    assert_eq!(outcome.state, mm_proto::v1::JobState::Failed as i32);
    assert_eq!(outcome.error_code, codes::TIMEOUT);
    publisher.delete_queue(&queue).await.expect("queue removed");
    db.dispose().await;
}

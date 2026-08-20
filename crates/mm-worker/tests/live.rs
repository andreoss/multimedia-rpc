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
    BatchSpec, CallbackError, CallbackPublisher, ConcatSpec, GraphNode, GraphSpec, ImageFormat,
    ImageOp, ImageSpec, InputSource, JobId, JobRecord, JobRepository, JobState, LadderSpec,
    MediaRef, NewJob, NodeRef, ObjectBody, ObjectRef, ObjectStore, ObjectStoreError, OpResult,
    OpSpec, OutputProfile, PackageSpec, ProbeSpec, QueueUrl, RequestId, ResizeMode, StepState,
    ThumbnailSpec, TranscodeSpec, TrimSpec, UserId, WorkerId, collect_body,
};
use mm_jobstore::{MIGRATOR, PgCancelListener, PgJobListener, PgJobRepository};
use mm_storage::{CredentialSource, S3ObjectStore, S3Settings};
use mm_worker::{
    DiskBudget, FfmpegRungPacker, LadderPackError, LadderPacked, RetryPolicy, RungPacker,
    WorkerConfig, codes, start,
};
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

fn blocked_spec(input: MediaRef) -> OpSpec {
    OpSpec::Transcode(
        TranscodeSpec::new(
            input,
            OutputProfile::from_raw(24.0, 0, 0, 0, 0, 0, 0).unwrap(),
        )
        .unwrap(),
    )
}

async fn blocked_input(store: &S3ObjectStore) -> MediaRef {
    let foreign = fixture("pickup-blocked.mkv", 1, 30, "matroska");
    upload_input(store, &foreign, "inputs/pickup-blocked.mkv").await
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
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    assert_eq!(record.progress.value(), 100);
    let duration_ms = record
        .duration_ms
        .expect("a settled concat carries the merged duration");
    assert!(
        (duration_ms - 7_000).abs() < 600,
        "the merged duration {duration_ms}ms is not the 4s+3s sum"
    );
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
        .create(single_input_job(OpSpec::Probe(
            ProbeSpec::new(input).expect("a bare reference cuts nothing"),
        )))
        .await
        .unwrap();

    let handle = start(base_config(&db, store.clone(), "worker-probe"));
    let record = await_terminal(&repository, job.id, Duration::from_secs(60)).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    assert_eq!(
        record.duration_ms, None,
        "a probe merges nothing, so it stores no merged duration"
    );
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
    let video = probe.video();
    assert_eq!(video.len(), 1);
    assert_eq!(video[0].codec(), "h264");
    assert_eq!(video[0].width(), 1280);
    assert_eq!(video[0].height(), 720);
    assert!(
        (video[0].frame_rate() - 30.0).abs() < 0.5,
        "frame rate {}",
        video[0].frame_rate()
    );
    assert_eq!(video[0].rotation(), 0);
    let audio = probe.audio();
    assert_eq!(audio.len(), 1);
    assert_eq!(audio[0].codec(), "aac");
    assert_eq!(audio[0].channels(), 1);
    assert_eq!(audio[0].sample_rate(), 44_100);
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_worker_reads_the_shared_metadata_without_probing() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("shared-metadata-4s.mp4", 4, 30, "mp4");
    let input = upload_input(&store, &source, "inputs/shared-metadata-4s.mp4").await;
    let repository = db.repository();

    let first_handle = start(base_config(&db, store.clone(), "worker-shared-one"));
    let first = repository
        .create(single_input_job(OpSpec::Probe(
            ProbeSpec::new(input.clone()).expect("a bare reference cuts nothing"),
        )))
        .await
        .unwrap();
    let first_record = await_terminal(&repository, first.id, Duration::from_secs(60)).await;
    first_handle.shutdown().await;
    assert_eq!(first_record.state, JobState::Succeeded);

    let mut second_config = base_config(&db, store.clone(), "worker-shared-two");
    second_config.prober = mm_ffmpeg::Prober::with_binary("/nonexistent/ffprobe");
    let second_handle = start(second_config);

    let second = repository
        .create(single_input_job(OpSpec::Probe(
            ProbeSpec::new(input).expect("a bare reference cuts nothing"),
        )))
        .await
        .unwrap();
    let second_record = await_terminal(&repository, second.id, Duration::from_secs(60)).await;
    second_handle.shutdown().await;

    assert_eq!(
        second_record.state,
        JobState::Succeeded,
        "a worker with no usable prober read the shared row: {:?}",
        second_record.failure
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
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
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
async fn a_trim_job_succeeds_with_a_verified_output() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("trim-30fps-6s.mp4", 6, 30, "mp4");
    let input = upload_input(&store, &source, "inputs/trim-30fps-6s.mp4").await;
    let repository = db.repository();
    let job = repository
        .create(single_input_job(OpSpec::Trim(
            TrimSpec::new(input, 1.0, 2.0).unwrap(),
        )))
        .await
        .unwrap();

    let handle = start(base_config(&db, store.clone(), "worker-trim"));
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    assert_eq!(record.progress.value(), 100);
    let OpResult::Trim(result) = record.result.expect("result recorded") else {
        panic!("expected a trim result");
    };
    let MediaRef::Object(object) = result.output().clone() else {
        panic!("expected an object output");
    };
    assert_eq!(object.key(), format!("jobs/{}/output.mp4", job.id));
    let wanted = result.duration_seconds();
    assert!((wanted - 2.0).abs() < 0.3, "the result claims {wanted}s");

    let body = store.get(&object).await.expect("output fetches");
    let bytes = collect_body(body).await.expect("output body collects");
    let local = scratch_dir("verify").join(format!("{}.mp4", job.id));
    tokio::fs::write(&local, &bytes).await.unwrap();
    let info = mm_ffmpeg::Prober::default()
        .probe(&local.to_string_lossy(), &mm_ffmpeg::ProbeLimits::default())
        .await
        .expect("output probes");
    assert!(
        (info.duration.as_secs_f64() - 2.0).abs() < 0.3,
        "the stored trim is {}s long",
        info.duration.as_secs_f64()
    );
    assert!(info.container.split(',').any(|token| token == "mp4"));
    let staged = scratch_dir("staging").join(format!("{}.mp4", job.id));
    assert!(!staged.exists(), "staging file not cleaned: {staged:?}");
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_thumbnail_job_lands_every_requested_key() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("thumbnail-30fps-6s.mp4", 6, 30, "mp4");
    let input = upload_input(&store, &source, "inputs/thumbnail-30fps-6s.mp4").await;
    let repository = db.repository();
    let job = repository
        .create(single_input_job(OpSpec::Thumbnail(
            ThumbnailSpec::new(input, vec![0.0, 1.5, 3.0], 160, 120).unwrap(),
        )))
        .await
        .unwrap();

    let handle = start(base_config(&db, store.clone(), "worker-thumbnail"));
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    assert_eq!(record.progress.value(), 100);
    let OpResult::Thumbnail(result) = record.result.expect("result recorded") else {
        panic!("expected a thumbnail result");
    };
    assert_eq!(result.outputs().len(), 3);
    let mut sizes = Vec::new();
    for (index, output) in result.outputs().iter().enumerate() {
        let MediaRef::Object(object) = output else {
            panic!("expected an object output");
        };
        assert_eq!(object.key(), format!("jobs/{}/thumb-{index}.jpg", job.id));
        let body = store.get(object).await.expect("output fetches");
        let bytes = collect_body(body).await.expect("output body collects");
        let local = scratch_dir("verify").join(format!("{}-{index}.jpg", job.id));
        tokio::fs::write(&local, &bytes).await.unwrap();
        let still = mm_ffmpeg::Prober::default()
            .still(&local.to_string_lossy(), &mm_ffmpeg::ProbeLimits::default())
            .await
            .expect("the stored thumbnail reports");
        assert_eq!((still.width, still.height), (160, 120));
        sizes.push(still.size_bytes.unwrap_or_default());
    }
    assert!(
        sizes.windows(2).any(|pair| pair[0] != pair[1]),
        "every offset produced the same frame: {sizes:?}"
    );
    for index in 0..3 {
        let staged = scratch_dir("staging").join(format!("{}-{index}.jpg", job.id));
        assert!(!staged.exists(), "staging file not cleaned: {staged:?}");
    }
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_thumbnail_upload_failure_cleans_every_key_it_landed() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("thumbnail-cleanup-4s.mp4", 4, 30, "mp4");
    let input = upload_input(&store, &source, "inputs/thumbnail-cleanup-4s.mp4").await;
    let repository = db.repository();
    let job = repository
        .create(single_input_job(OpSpec::Thumbnail(
            ThumbnailSpec::new(input, vec![0.0, 1.0, 2.0], 160, 120).unwrap(),
        )))
        .await
        .unwrap();

    let flaky: Arc<dyn ObjectStore> = Arc::new(FailingNthUpload {
        inner: store.clone(),
        fail_on: 2,
        seen: std::sync::Mutex::new(0),
    });
    let mut config = base_config(&db, flaky, "worker-thumbnail-cleanup");
    config.retry = RetryPolicy {
        max_attempts: 1,
        base: Duration::from_millis(100),
        cap: Duration::from_millis(100),
    };
    let handle = start(config);
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Failed);
    let failure = record.failure.expect("failure recorded");
    assert_eq!(failure.code(), codes::OUTPUT_UPLOAD_FAILED);
    assert!(record.result.is_none(), "a failed job recorded a result");
    for index in 0..3 {
        let key = ObjectRef::new(BUCKET, &format!("jobs/{}/thumb-{index}.jpg", job.id)).unwrap();
        assert!(
            store.head(&key).await.is_err(),
            "key {index} survived the cleanup"
        );
    }
    for index in 0..3 {
        let staged = scratch_dir("staging").join(format!("{}-{index}.jpg", job.id));
        assert!(!staged.exists(), "staging file not cleaned: {staged:?}");
    }
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_during_a_thumbnail_op_leaves_no_output_and_no_staged_file() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("thumbnail-cancel-30s.mp4", 30, 30, "mp4");
    let input = upload_input(&store, &source, "inputs/thumbnail-cancel-30s.mp4").await;
    let repository = db.repository();
    let job = repository
        .create(single_input_job(OpSpec::Thumbnail(
            ThumbnailSpec::new(input, vec![10.0, 15.0, 20.0, 25.0], 640, 360).unwrap(),
        )))
        .await
        .unwrap();

    let mut config = base_config(&db, store.clone(), "worker-thumbnail-cancel");
    config.cancellations = Some(Arc::new(
        PgCancelListener::connect(db.url())
            .await
            .expect("cancel listener connects"),
    ));
    let handle = start(config);
    let running = await_state(
        &repository,
        job.id,
        JobState::Running,
        Duration::from_secs(60),
    )
    .await;
    assert_eq!(running.state, JobState::Running);
    repository
        .request_cancel(job.id)
        .await
        .expect("cancel requested");
    let record = await_terminal(&repository, job.id, mm_testkit::budget::CANCEL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Cancelled);
    for index in 0..4 {
        let key = ObjectRef::new(BUCKET, &format!("jobs/{}/thumb-{index}.jpg", job.id)).unwrap();
        assert!(
            store.head(&key).await.is_err(),
            "key {index} survived the cancel"
        );
    }
    let leftover: Vec<_> = std::fs::read_dir(scratch_dir("staging"))
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .contains(&job.id.to_string())
                })
                .map(|entry| entry.path())
                .collect()
        })
        .unwrap_or_default();
    assert!(leftover.is_empty(), "staged files survived: {leftover:?}");
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_thumbnail_set_over_the_disk_budget_is_released_until_it_fits() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("thumbnail-budget-4s.mp4", 4, 30, "mp4");
    let input = upload_input(&store, &source, "inputs/thumbnail-budget-4s.mp4").await;
    let repository = db.repository();
    let job = repository
        .create(single_input_job(OpSpec::Thumbnail(
            ThumbnailSpec::new(input, vec![0.0, 1.0], 640, 360).unwrap(),
        )))
        .await
        .unwrap();

    let mut tight = base_config(&db, store.clone(), "worker-thumbnail-budget");
    tight.disk_budget = DiskBudget::Fixed(1024);
    tight.release_retry = Duration::from_millis(300);
    let tight_handle = start(tight);

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let record = repository.get(job.id).await.unwrap().unwrap();
        assert!(
            !record.state.is_terminal(),
            "the set was admitted under a budget it exceeds: {:?}",
            record.state
        );
        if record.state == JobState::Pending && record.next_attempt_at.is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "the job was never released");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    tight_handle.shutdown().await;

    let roomy = start(base_config(&db, store, "worker-thumbnail-roomy"));
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    roomy.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    db.dispose().await;
}

fn keyed_fixture(name: &str, seconds: u32) -> PathBuf {
    let dir = scratch_dir("fixtures");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    if path.is_file() && path.metadata().map(|meta| meta.len()).unwrap_or(0) > 0 {
        return path;
    }
    let output = Command::new("ffmpeg")
        .arg("-y")
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg(format!("testsrc2=duration={seconds}:size=640x360:rate=30"))
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
        .arg("-g")
        .arg("30")
        .arg("-keyint_min")
        .arg("30")
        .arg("-sc_threshold")
        .arg("0")
        .arg("-c:a")
        .arg("aac")
        .arg("-movflags")
        .arg("+faststart")
        .arg(&path)
        .output()
        .expect("the media tool must be runnable to generate fixtures");
    assert!(output.status.success(), "fixture generation failed: {name}");
    path
}

fn image_fixture(name: &str, width: u32, height: u32, colour: &str) -> PathBuf {
    let dir = scratch_dir("fixtures");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    if path.is_file() && path.metadata().map(|meta| meta.len()).unwrap_or(0) > 0 {
        return path;
    }
    let output = Command::new("magick")
        .arg("-size")
        .arg(format!("{width}x{height}"))
        .arg(format!("xc:{colour}"))
        .arg(&path)
        .output()
        .expect("the image tool must be runnable to generate fixtures");
    assert!(output.status.success(), "fixture generation failed: {name}");
    path
}

async fn identify_frame(store: &Arc<S3ObjectStore>, object: &ObjectRef) -> (u32, u32, String) {
    let body = store.get(object).await.expect("output fetches");
    let bytes = collect_body(body).await.expect("output body collects");
    let name = object.key().replace('/', "_");
    let local = scratch_dir("verify").join(name);
    tokio::fs::write(&local, &bytes).await.unwrap();
    let info = mm_magick::Tool::default()
        .identify(&local.to_string_lossy(), &mm_magick::ToolLimits::default())
        .await
        .expect("the produced image reports");
    (info.width, info.height, info.format)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resize_job_lands_an_image_at_the_requested_frame() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = image_fixture("resize-source.png", 800, 600, "green");
    let input = upload_input(&store, &source, "inputs/resize-source.png").await;
    let repository = db.repository();
    let job = repository
        .create(single_input_job(OpSpec::Image(
            ImageSpec::new(
                vec![input],
                ImageOp::Resize,
                320,
                240,
                ResizeMode::Exact,
                None,
            )
            .unwrap(),
        )))
        .await
        .unwrap();

    let handle = start(base_config(&db, store.clone(), "worker-resize"));
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let OpResult::Image(result) = record.result.expect("result recorded") else {
        panic!("expected an image result");
    };
    assert_eq!(result.outputs().len(), 1);
    let MediaRef::Object(object) = &result.outputs()[0] else {
        panic!("expected an object output");
    };
    assert_eq!(object.key(), format!("jobs/{}/image.png", job.id));
    let (width, height, format) = identify_frame(&store, object).await;
    assert_eq!((width, height), (320, 240));
    assert_eq!(format, "PNG");
    let staged = scratch_dir("staging").join(format!("{}.png", job.id));
    assert!(!staged.exists(), "staging file not cleaned: {staged:?}");
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_convert_job_lands_an_image_in_the_requested_format() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = image_fixture("convert-source.png", 200, 150, "blue");
    let input = upload_input(&store, &source, "inputs/convert-source.png").await;
    let repository = db.repository();
    let job = repository
        .create(single_input_job(OpSpec::Image(
            ImageSpec::new(
                vec![input],
                ImageOp::Convert,
                0,
                0,
                ResizeMode::Fit,
                Some(ImageFormat::Jpeg),
            )
            .unwrap(),
        )))
        .await
        .unwrap();

    let handle = start(base_config(&db, store.clone(), "worker-convert"));
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let OpResult::Image(result) = record.result.expect("result recorded") else {
        panic!("expected an image result");
    };
    let MediaRef::Object(object) = &result.outputs()[0] else {
        panic!("expected an object output");
    };
    assert_eq!(object.key(), format!("jobs/{}/image.jpg", job.id));
    let (width, height, format) = identify_frame(&store, object).await;
    assert_eq!((width, height), (200, 150));
    assert_eq!(format, "JPEG");
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_composite_job_lands_the_base_frame_with_the_overlay() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let base = image_fixture("composite-base.png", 400, 300, "white");
    let overlay = image_fixture("composite-overlay.png", 100, 100, "red");
    let base_ref = upload_input(&store, &base, "inputs/composite-base.png").await;
    let overlay_ref = upload_input(&store, &overlay, "inputs/composite-overlay.png").await;
    let repository = db.repository();
    let job = repository
        .create(single_input_job(OpSpec::Image(
            ImageSpec::new(
                vec![overlay_ref, base_ref],
                ImageOp::Composite,
                0,
                0,
                ResizeMode::Fit,
                Some(ImageFormat::Jpeg),
            )
            .unwrap(),
        )))
        .await
        .unwrap();

    let handle = start(base_config(&db, store.clone(), "worker-composite"));
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let OpResult::Image(result) = record.result.expect("result recorded") else {
        panic!("expected an image result");
    };
    let MediaRef::Object(object) = &result.outputs()[0] else {
        panic!("expected an object output");
    };
    assert_eq!(object.key(), format!("jobs/{}/image.jpg", job.id));
    let (width, height, format) = identify_frame(&store, object).await;
    assert_eq!((width, height), (400, 300));
    assert_eq!(format, "JPEG");
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_video_input_blocks_an_image_op_with_the_media_kind_code() {
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("image-op-video.mp4", 2, 30, "mp4");
    let input = upload_input(&store, &source, "inputs/image-op-video.mp4").await;
    let repository = db.repository();
    let job = repository
        .create(single_input_job(OpSpec::Image(
            ImageSpec::new(
                vec![input],
                ImageOp::Resize,
                64,
                64,
                ResizeMode::Exact,
                None,
            )
            .unwrap(),
        )))
        .await
        .unwrap();

    let handle = start(base_config(&db, store, "worker-image-blocked"));
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Failed);
    let failure = record.failure.expect("failure recorded");
    assert_eq!(failure.code(), codes::VALIDATION_BLOCKED);
    assert!(
        failure.message().contains("MediaKindMismatch"),
        "{}",
        failure.message()
    );
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

    let deadline = Instant::now() + mm_testkit::budget::MERGE_TERMINAL;
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
    let first_lease = repository
        .get(job.id)
        .await
        .unwrap()
        .unwrap()
        .lease_expires_at
        .expect("a running job holds a lease");
    let renewed = Instant::now() + Duration::from_secs(90);
    loop {
        let record = repository.get(job.id).await.unwrap().unwrap();
        if record
            .lease_expires_at
            .is_some_and(|lease| lease > first_lease)
        {
            break;
        }
        assert!(
            Instant::now() < renewed,
            "the lease was never renewed, so the run never started"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    doomed_handle.abort();

    let mut rescuer = base_config(&db, store.clone(), "worker-rescuer");
    rescuer.lease = Duration::from_secs(15);
    rescuer.heartbeat_interval = Duration::from_secs(2);
    rescuer.reconcile_interval = Duration::from_millis(200);
    let rescuer_handle = start(rescuer);

    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
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
    assert_eq!(
        failure
            .inputs()
            .iter()
            .map(|input| (input.input(), input.code().to_string()))
            .collect::<Vec<_>>(),
        vec![(Some(0), "ERROR_UNSUPPORTED_CONTAINER".to_string())],
        "the per-input failure is read off the row, not back out of the message"
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
    let callback = <mm_proto::v1::JobCallback as prost::Message>::decode(decoded.as_slice())
        .expect("message is a job callback");
    let outcome = callback.outcome.expect("the callback carries the outcome");
    let stitch = callback
        .stitch
        .expect("the callback carries the reference half");
    assert_eq!(outcome.job_id, job.id.to_string());
    assert_eq!(outcome.request_id, job.request.to_string());
    assert_eq!(outcome.state, mm_proto::v1::JobState::Failed as i32);
    assert_eq!(
        outcome.error_code, "ERROR_UNSUPPORTED_CONTAINER",
        "the published outcome carries the reference vocabulary while the row keeps the worker's code"
    );
    assert!(outcome.result.is_none());
    assert!(!outcome.file_errors.is_empty());
    assert_eq!(stitch.status, mm_proto::v1::StitchStatus::Failed as i32);
    assert_eq!(stitch.request_id, job.request.to_string());
    assert_eq!(
        stitch.error_code,
        Some("ERROR_UNSUPPORTED_CONTAINER".to_string())
    );
    assert_eq!(stitch.r2_uri, None);
    assert_eq!(stitch.file_errors[0].code, "ERROR_UNSUPPORTED_CONTAINER");
    assert_eq!(outcome.file_errors[0].code, "ERROR_UNSUPPORTED_CONTAINER");
    assert_eq!(
        outcome.file_errors[0].media_ref,
        Some(mm_proto::v1::MediaRef::from(&refs[0]))
    );
    assert_eq!(
        outcome.file_errors[0],
        mm_proto::v1::FileError {
            media_ref: Some(mm_proto::v1::MediaRef::from(&refs[0])),
            code: "ERROR_UNSUPPORTED_CONTAINER".to_string(),
            step_index: None,
            node_id: None,
            input_index: Some(0),
            rung: None,
        }
    );
    let recorded = record
        .failure
        .as_ref()
        .expect("failure recorded")
        .inputs()
        .iter()
        .map(|input| (input.input(), input.code().to_string()))
        .collect::<Vec<_>>();
    assert_eq!(
        recorded,
        vec![(Some(0), "ERROR_UNSUPPORTED_CONTAINER".to_string())]
    );
    publisher.delete_queue(&queue).await.expect("queue removed");
    db.dispose().await;
}

struct FailingUploads {
    inner: Arc<S3ObjectStore>,
}

struct FailingNthUpload {
    inner: Arc<S3ObjectStore>,
    fail_on: usize,
    seen: std::sync::Mutex<usize>,
}

#[async_trait]
impl ObjectStore for FailingNthUpload {
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
        object: &ObjectRef,
        body: ObjectBody,
    ) -> Result<(), ObjectStoreError> {
        let ordinal = {
            let mut seen = self.seen.lock().expect("upload counter locks");
            *seen += 1;
            *seen
        };
        if ordinal == self.fail_on {
            return Err(ObjectStoreError::Backend {
                message: "injected upload outage".to_string(),
            });
        }
        self.inner.put_multipart(object, body).await
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
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    roomy_handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_created_job_is_claimed_via_notify_well_before_any_tick() {
    let db = fresh_db().await;
    let store = test_store().await;
    let repository = db.repository();
    let blocked = blocked_input(&store).await;

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
                blocked_spec(blocked.clone()),
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
    let blocked = blocked_input(&store).await;

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
            blocked_spec(blocked.clone()),
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
    <mm_proto::v1::JobCallback as prost::Message>::decode(decoded.as_slice())
        .expect("message is a job callback")
        .outcome
        .expect("the callback carries this service's outcome")
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
    assert_eq!(outcome.error_code, "ERROR_JOB_TIMEOUT");
    publisher.delete_queue(&queue).await.expect("queue removed");
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_package_job_lands_a_playlist_and_its_segments() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = keyed_fixture("package-source.mp4", 4);
    let input = upload_input(&store, &source, "inputs/package-source.mp4").await;
    let repository = db.repository();
    let job = repository
        .create(single_input_job(OpSpec::Package(
            PackageSpec::new(input, 1.0).unwrap(),
        )))
        .await
        .unwrap();

    let handle = start(base_config(&db, store.clone(), "worker-package"));
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let OpResult::Package(result) = record.result.expect("result recorded") else {
        panic!("expected a package result");
    };
    assert!(
        result.segments().len() >= 2,
        "segments: {:?}",
        result.segments()
    );
    let MediaRef::Object(playlist) = result.playlist() else {
        panic!("expected an object playlist");
    };
    assert_eq!(playlist.key(), format!("jobs/{}/playlist.m3u8", job.id));
    let playlist_body = store.get(playlist).await.expect("playlist fetches");
    let playlist_text = String::from_utf8(collect_body(playlist_body).await.unwrap()).unwrap();
    assert!(playlist_text.starts_with("#EXTM3U"), "{playlist_text}");
    assert!(playlist_text.contains("#EXT-X-MAP:URI=\"init.mp4\""));
    assert!(playlist_text.contains("#EXTINF:1.000,"));
    assert!(playlist_text.trim_end().ends_with("#EXT-X-ENDLIST"));

    let named_init = result
        .init_segment()
        .expect("a packaged result names the init segment its playlist points at");
    let MediaRef::Object(named_init) = named_init else {
        panic!("expected an object init segment");
    };
    assert_eq!(named_init.key(), format!("jobs/{}/init.mp4", job.id));

    let init = ObjectRef::new(BUCKET, &format!("jobs/{}/init.mp4", job.id)).unwrap();
    assert!(store.head(&init).await.expect("init exists") > 0);
    for (index, segment) in result.segments().iter().enumerate() {
        let MediaRef::Object(object) = segment else {
            panic!("expected an object segment");
        };
        assert_eq!(object.key(), format!("jobs/{}/seg{index:05}.m4s", job.id));
        assert!(store.head(object).await.expect("segment exists") > 0);
    }
    let staging = scratch_dir("staging").join(job.id.to_string());
    assert!(!staging.exists(), "staging survived: {staging:?}");
    db.dispose().await;
}

fn batch_step_reading(node: &str, output: u32) -> OpSpec {
    let source = InputSource::from(NodeRef::new(node, output).expect("valid node ref"));
    OpSpec::Transcode(
        TranscodeSpec::new(
            source,
            OutputProfile::from_raw(24.0, 0, 0, 0, 48_000, 640, 360).unwrap(),
        )
        .unwrap(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_batch_runs_its_steps_and_names_their_outputs() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("batch-4s.mp4", 4, 30, "mp4");
    let input = upload_input(&store, &source, "inputs/batch-4s.mp4").await;
    let trim = OpSpec::Trim(TrimSpec::new(input, 0.5, 2.0).unwrap());
    let probe =
        OpSpec::Probe(ProbeSpec::new(InputSource::from(NodeRef::new("1", 0).unwrap())).unwrap());
    let spec =
        OpSpec::Batch(BatchSpec::new(vec![trim, batch_step_reading("0", 0), probe], true).unwrap());

    let repository = db.repository();
    let job = repository.create(single_input_job(spec)).await.unwrap();
    let handle = start(base_config(&db, store.clone(), "worker-batch"));
    let record = await_terminal(&repository, job.id, Duration::from_secs(180)).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let OpResult::Batch(batch) = record.result.expect("result recorded") else {
        panic!("expected a batch result");
    };
    assert_eq!(batch.steps().len(), 3);
    let StepState::Result(OpResult::Trim(timed)) = batch.steps()[0].state() else {
        panic!("expected a trim result at step 0");
    };
    let MediaRef::Object(trimmed) = timed.output().clone() else {
        panic!("expected an object output");
    };
    assert_eq!(trimmed.key(), format!("jobs/{}/steps/0/output.mp4", job.id));
    let StepState::Result(OpResult::Transcode(transcoded)) = batch.steps()[1].state() else {
        panic!("expected a transcode result at step 1");
    };
    let MediaRef::Object(object) = transcoded.output().clone() else {
        panic!("expected an object output");
    };
    assert_eq!(object.key(), format!("jobs/{}/steps/1/output.mp4", job.id));
    let StepState::Result(OpResult::Probe(probed)) = batch.steps()[2].state() else {
        panic!("expected a probe result at step 2");
    };
    assert!(
        (probed.duration_seconds() - 2.0).abs() < 0.5,
        "the probed duration is {}",
        probed.duration_seconds()
    );
    db.dispose().await;
}

async fn failing_batch(store: &S3ObjectStore, db: &TestDb, stop: bool) -> JobRecord {
    let source = fixture("batch-fail-4s.mp4", 4, 30, "mp4");
    let input = upload_input(store, &source, "inputs/batch-fail-4s.mp4").await;
    let trim = OpSpec::Trim(TrimSpec::new(input, 0.5, 2.0).unwrap());
    let bad = batch_step_reading("0", 9);
    let spec = OpSpec::Batch(BatchSpec::new(vec![trim, bad], stop).unwrap());
    let repository = db.repository();
    let job = repository.create(single_input_job(spec)).await.unwrap();
    let mut config = base_config(db, Arc::new(store.clone()), "worker-batch-fail");
    config.retry.max_attempts = 1;
    let handle = start(config);
    let record = await_terminal(&repository, job.id, Duration::from_secs(180)).await;
    handle.shutdown().await;
    record
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_batch_discards_the_outputs_of_earlier_steps() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let record = failing_batch(&store, &db, true).await;

    assert_eq!(record.state, JobState::Failed);
    assert!(record.result.is_none(), "a stopped batch keeps no result");
    let object = ObjectRef::new(BUCKET, &format!("jobs/{}/steps/0/output.mp4", record.id)).unwrap();
    assert!(
        store.head(&object).await.is_err(),
        "the earlier step's output was left behind"
    );
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_continued_batch_reports_what_succeeded() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let record = failing_batch(&store, &db, false).await;

    assert_eq!(record.state, JobState::Failed);
    let OpResult::Batch(batch) = record.result.expect("a continued batch keeps its result") else {
        panic!("expected a batch result");
    };
    assert!(matches!(
        batch.steps()[0].state(),
        StepState::Result(OpResult::Trim(_))
    ));
    assert!(matches!(batch.steps()[1].state(), StepState::Failure(_)));
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_diamond_graph_lands_all_four_results() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("graph-diamond-4s.mp4", 4, 30, "mp4");
    let input = upload_input(&store, &source, "inputs/graph-diamond-4s.mp4").await;
    let profile = OutputProfile::from_raw(24.0, 0, 0, 0, 48_000, 640, 360).unwrap();
    let trim = OpSpec::Trim(TrimSpec::new(input, 0.0, 2.0).unwrap());
    let left = batch_step_reading("source", 0);
    let right = batch_step_reading("source", 0);
    let join = OpSpec::Concat(
        ConcatSpec::new(
            vec![
                InputSource::from(NodeRef::new("left", 0).unwrap()),
                InputSource::from(NodeRef::new("right", 0).unwrap()),
            ],
            Some(profile),
        )
        .unwrap(),
    );
    let spec = OpSpec::Graph(
        GraphSpec::new(vec![
            GraphNode::new("source", trim).unwrap(),
            GraphNode::new("left", left).unwrap(),
            GraphNode::new("right", right).unwrap(),
            GraphNode::new("join", join).unwrap(),
        ])
        .unwrap(),
    );

    let repository = db.repository();
    let job = repository.create(single_input_job(spec)).await.unwrap();
    let handle = start(base_config(&db, store.clone(), "worker-graph"));
    let record = await_terminal(&repository, job.id, Duration::from_secs(240)).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let OpResult::Graph(graph) = record.result.expect("result recorded") else {
        panic!("expected a graph result");
    };
    assert_eq!(graph.nodes().len(), 4);
    let join = graph
        .nodes()
        .iter()
        .find(|node| node.id().as_str() == "join")
        .expect("the join node is present");
    let StepState::Result(OpResult::Concat(timed)) = join.state() else {
        panic!("expected the join result");
    };
    assert!(
        (timed.duration_seconds() - 4.0).abs() < 0.8,
        "the join duration is {}",
        timed.duration_seconds()
    );
    let MediaRef::Object(object) = timed.output().clone() else {
        panic!("expected an object output");
    };
    assert_eq!(
        object.key(),
        format!("jobs/{}/nodes/join/output.mp4", job.id)
    );

    let body = store.get(&object).await.expect("output fetches");
    let bytes = collect_body(body).await.expect("output body collects");
    let local = scratch_dir("verify").join(format!("{}-join.mp4", job.id));
    tokio::fs::write(&local, &bytes).await.unwrap();
    let info = mm_ffmpeg::Prober::default()
        .probe(&local.to_string_lossy(), &Default::default())
        .await
        .unwrap();
    assert_eq!(info.video[0].width, 640);
    assert_eq!(info.video[0].height, 360);
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_graph_failure_leaves_no_key_under_the_job_prefix() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("graph-fail-4s.mp4", 4, 30, "mp4");
    let input = upload_input(&store, &source, "inputs/graph-fail-4s.mp4").await;
    let trim = OpSpec::Trim(TrimSpec::new(input, 0.0, 2.0).unwrap());
    let bad = batch_step_reading("source", 9);
    let spec = OpSpec::Graph(
        GraphSpec::new(vec![
            GraphNode::new("source", trim).unwrap(),
            GraphNode::new("bad", bad).unwrap(),
        ])
        .unwrap(),
    );

    let repository = db.repository();
    let job = repository.create(single_input_job(spec)).await.unwrap();
    let mut config = base_config(&db, store.clone(), "worker-graph-fail");
    config.retry.max_attempts = 1;
    let handle = start(config);
    let record = await_terminal(&repository, job.id, Duration::from_secs(180)).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Failed);
    let object =
        ObjectRef::new(BUCKET, &format!("jobs/{}/nodes/source/output.mp4", job.id)).unwrap();
    assert!(
        store.head(&object).await.is_err(),
        "a graph failure left a key under the job prefix"
    );
    db.dispose().await;
}

fn frame_gray(path: &Path, at_seconds: Option<f64>) -> Vec<u8> {
    let mut command = Command::new("ffmpeg");
    command.arg("-v").arg("quiet").arg("-nostdin");
    if let Some(seconds) = at_seconds {
        command.arg("-ss").arg(format!("{seconds:.3}"));
    }
    let output = command
        .arg("-i")
        .arg(path)
        .arg("-frames:v")
        .arg("1")
        .arg("-vf")
        .arg("scale=16:16")
        .arg("-pix_fmt")
        .arg("gray")
        .arg("-f")
        .arg("rawvideo")
        .arg("-")
        .output()
        .expect("ffmpeg runs");
    assert!(output.status.success(), "frame extraction failed");
    output.stdout
}

fn frame_difference(left: &[u8], right: &[u8]) -> u32 {
    left.iter()
        .zip(right)
        .map(|(left, right)| u32::from(left.abs_diff(*right)))
        .sum()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_transcode_with_a_poster_lands_a_matching_second_object() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("poster-4s.mp4", 4, 30, "mp4");
    let input = upload_input(&store, &source, "inputs/poster-4s.mp4").await;
    let profile = OutputProfile::from_raw(24.0, 0, 0, 0, 48_000, 640, 360)
        .unwrap()
        .with_poster(1.0)
        .unwrap();
    let repository = db.repository();
    let job = repository
        .create(single_input_job(OpSpec::Transcode(
            TranscodeSpec::new(input, profile).unwrap(),
        )))
        .await
        .unwrap();
    let handle = start(base_config(&db, store.clone(), "worker-poster"));
    let record = await_terminal(&repository, job.id, Duration::from_secs(120)).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let OpResult::Transcode(result) = record.result.expect("result recorded") else {
        panic!("expected a transcode result");
    };
    let MediaRef::Object(output) = result.output().clone() else {
        panic!("expected an object output");
    };
    let poster = ObjectRef::new(BUCKET, &format!("jobs/{}/poster.jpg", job.id)).unwrap();
    assert!(store.head(&poster).await.expect("poster exists") > 0);

    let dir = scratch_dir("verify");
    let output_path = dir.join(format!("{}-poster-output.mp4", job.id));
    let poster_path = dir.join(format!("{}-poster.jpg", job.id));
    let output_bytes = collect_body(store.get(&output).await.expect("output fetches"))
        .await
        .expect("output body");
    let poster_bytes = collect_body(store.get(&poster).await.expect("poster fetches"))
        .await
        .expect("poster body");
    tokio::fs::write(&output_path, &output_bytes).await.unwrap();
    tokio::fs::write(&poster_path, &poster_bytes).await.unwrap();

    let from_poster = frame_gray(&poster_path, None);
    let at_one = frame_difference(&from_poster, &frame_gray(&output_path, Some(1.0)));
    let at_zero = frame_difference(&from_poster, &frame_gray(&output_path, Some(0.0)));
    assert!(
        at_one < at_zero,
        "the poster is not the frame at 1.0s: 1.0 differs by {at_one}, 0.0 by {at_zero}"
    );
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_thumbnail_sprite_tiles_the_requested_offsets() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("sprite-4s.mp4", 4, 30, "mp4");
    let input = upload_input(&store, &source, "inputs/sprite-4s.mp4").await;
    let spec = OpSpec::Thumbnail(
        ThumbnailSpec::new(input, vec![0.0, 1.0, 2.0, 3.0], 160, 90)
            .unwrap()
            .with_sprite(2),
    );
    let repository = db.repository();
    let job = repository.create(single_input_job(spec)).await.unwrap();
    let handle = start(base_config(&db, store.clone(), "worker-sprite"));
    let record = await_terminal(&repository, job.id, Duration::from_secs(120)).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let OpResult::Thumbnail(result) = record.result.expect("result recorded") else {
        panic!("expected a thumbnail result");
    };
    assert_eq!(result.outputs().len(), 5, "four frames and one sprite");
    let MediaRef::Object(sprite) = result.outputs()[4].clone() else {
        panic!("expected an object output");
    };
    assert_eq!(sprite.key(), format!("jobs/{}/sprite.jpg", job.id));
    let bytes = collect_body(store.get(&sprite).await.expect("sprite fetches"))
        .await
        .expect("sprite body");
    let local = scratch_dir("verify").join(format!("{}-sprite.jpg", job.id));
    tokio::fs::write(&local, &bytes).await.unwrap();
    let info = mm_ffmpeg::Prober::default()
        .probe(&local.to_string_lossy(), &Default::default())
        .await
        .unwrap();
    assert_eq!(info.video[0].width, 320);
    assert_eq!(info.video[0].height, 180);
    db.dispose().await;
}

fn ladder_job(source: &MediaRef) -> NewJob {
    let rungs = vec![
        OutputProfile::from_raw(24.0, 400_000, 28, 64_000, 48_000, 640, 360).unwrap(),
        OutputProfile::from_raw(24.0, 900_000, 26, 64_000, 48_000, 1280, 720).unwrap(),
    ];
    single_input_job(OpSpec::Ladder(
        LadderSpec::new(source.clone(), rungs).unwrap(),
    ))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_ladder_job_lands_one_verified_object_per_rung() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("ladder-source.mp4", 2, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/ladder-source.mp4").await;
    let repository = db.repository();
    let job = repository.create(ladder_job(&input)).await.unwrap();

    let handle = start(base_config(&db, store.clone(), "worker-ladder"));
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let OpResult::Ladder(result) = record.result.expect("result recorded") else {
        panic!("expected a ladder result");
    };
    assert_eq!(result.renditions().len(), 2);
    assert_eq!(result.master_playlist(), None);
    for (index, rendition) in result.renditions().iter().enumerate() {
        let MediaRef::Object(media) = rendition.media() else {
            panic!("expected an object rendition");
        };
        assert_eq!(
            media.key(),
            format!("jobs/{}/renditions/{index}/output.mp4", job.id)
        );
        assert!(store.head(media).await.expect("rendition exists") > 0);
    }
    assert_eq!(result.renditions()[0].width(), 640);
    assert_eq!(result.renditions()[0].height(), 360);
    assert_eq!(result.renditions()[1].width(), 1280);
    assert_eq!(result.renditions()[1].height(), 720);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_ladder_upload_failure_cleans_every_key_it_landed() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("ladder-source-fail.mp4", 2, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/ladder-source-fail.mp4").await;
    let repository = db.repository();
    let job = repository.create(ladder_job(&input)).await.unwrap();

    let flaky: Arc<dyn ObjectStore> = Arc::new(FailingNthUpload {
        inner: store.clone(),
        fail_on: 2,
        seen: std::sync::Mutex::new(0),
    });
    let mut config = base_config(&db, flaky, "worker-ladder-fail");
    config.retry = RetryPolicy {
        max_attempts: 1,
        base: Duration::from_millis(100),
        cap: Duration::from_millis(100),
    };
    let handle = start(config);
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Failed, "{:?}", record.failure);
    let failure = record.failure.expect("failure recorded");
    assert_eq!(failure.code(), codes::OUTPUT_UPLOAD_FAILED);
    assert!(record.result.is_none(), "a failed job recorded a result");
    for index in 0..2 {
        let key = ObjectRef::new(
            BUCKET,
            &format!("jobs/{}/renditions/{index}/output.mp4", job.id),
        )
        .unwrap();
        assert!(
            store.head(&key).await.is_err(),
            "key {index} survived the cleanup"
        );
        let staged = scratch_dir("staging").join(format!("{}-rung{index}.mp4", job.id));
        assert!(!staged.exists(), "staging file not cleaned: {staged:?}");
    }
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_during_a_ladder_leaves_no_key_and_no_staged_file() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("ladder-cancel-30s.mp4", 30, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/ladder-cancel-30s.mp4").await;
    let repository = db.repository();
    let rungs = vec![
        OutputProfile::from_raw(24.0, 400_000, 28, 64_000, 48_000, 640, 360).unwrap(),
        OutputProfile::from_raw(24.0, 900_000, 26, 64_000, 48_000, 1280, 720).unwrap(),
    ];
    let job = repository
        .create(single_input_job(OpSpec::Ladder(
            LadderSpec::new(input, rungs).unwrap(),
        )))
        .await
        .unwrap();

    let mut config = base_config(&db, store.clone(), "worker-ladder-cancel");
    config.cancellations = Some(Arc::new(
        PgCancelListener::connect(db.url())
            .await
            .expect("cancel listener connects"),
    ));
    let handle = start(config);
    await_state(
        &repository,
        job.id,
        JobState::Running,
        Duration::from_secs(60),
    )
    .await;
    repository
        .request_cancel(job.id)
        .await
        .expect("cancel requested");
    let record = await_terminal(&repository, job.id, mm_testkit::budget::CANCEL).await;
    handle.shutdown().await;

    if record.state == JobState::Succeeded {
        return;
    }
    assert_eq!(record.state, JobState::Cancelled, "{:?}", record.failure);
    for index in 0..2 {
        let key = ObjectRef::new(
            BUCKET,
            &format!("jobs/{}/renditions/{index}/output.mp4", job.id),
        )
        .unwrap();
        assert!(
            store.head(&key).await.is_err(),
            "key {index} survived the cancel"
        );
        let staged = scratch_dir("staging").join(format!("{}-rung{index}.mp4", job.id));
        assert!(!staged.exists(), "staging file not cleaned: {staged:?}");
    }
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_ladder_rung_above_the_source_blocks_naming_the_rung() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("ladder-upscale-source.mp4", 2, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/ladder-upscale-source.mp4").await;
    let repository = db.repository();
    let rungs = vec![
        OutputProfile::from_raw(24.0, 400_000, 28, 64_000, 48_000, 640, 360).unwrap(),
        OutputProfile::from_raw(24.0, 4_000_000, 23, 64_000, 48_000, 3840, 2160).unwrap(),
    ];
    let job = repository
        .create(single_input_job(OpSpec::Ladder(
            LadderSpec::new(input, rungs).unwrap(),
        )))
        .await
        .unwrap();

    let handle = start(base_config(&db, store.clone(), "worker-ladder-upscale"));
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Failed, "{:?}", record.failure);
    let failure = record.failure.expect("failure recorded");
    assert_eq!(failure.code(), codes::VALIDATION_BLOCKED);
    assert_eq!(failure.inputs().len(), 1);
    let rung_failure = &failure.inputs()[0];
    assert_eq!(rung_failure.rung(), Some(1));
    assert_eq!(rung_failure.code(), "ERROR_UPSCALE_NOT_ALLOWED");
    assert!(
        failure.message().contains("rung 1: RungUpscale"),
        "{failure:?}"
    );
    for index in 0..2 {
        let key = ObjectRef::new(
            BUCKET,
            &format!("jobs/{}/renditions/{index}/output.mp4", job.id),
        )
        .unwrap();
        assert!(store.head(&key).await.is_err(), "key {index} landed");
    }
    db.dispose().await;
}

struct RecordingPacker {
    calls: tokio::sync::Mutex<Vec<(JobId, usize, f64)>>,
}

#[async_trait]
impl RungPacker for RecordingPacker {
    fn name(&self) -> &'static str {
        "recording"
    }

    async fn pack(
        &self,
        job: JobId,
        rung: usize,
        _media: &ObjectRef,
        staging_dir: &Path,
        segment_seconds: f64,
    ) -> Result<LadderPacked, LadderPackError> {
        self.calls.lock().await.push((job, rung, segment_seconds));
        Ok(stub_packed(staging_dir, rung))
    }
}

struct FailingPacker {
    calls: tokio::sync::Mutex<usize>,
}

#[async_trait]
impl RungPacker for FailingPacker {
    fn name(&self) -> &'static str {
        "failing"
    }

    async fn pack(
        &self,
        _job: JobId,
        _rung: usize,
        _media: &ObjectRef,
        _staging_dir: &Path,
        _segment_seconds: f64,
    ) -> Result<LadderPacked, LadderPackError> {
        let mut calls = self.calls.lock().await;
        *calls += 1;
        Err(LadderPackError::Refused(
            "the packer refused this rung".to_string(),
        ))
    }
}

fn stub_packed(staging: &Path, rung: usize) -> LadderPacked {
    std::fs::create_dir_all(staging).expect("staging dir");
    let segment_name = format!("seg{rung:05}.m4s");
    let init = staging.join("init.mp4");
    std::fs::write(&init, b"init").expect("init staged");
    let segment = staging.join(&segment_name);
    std::fs::write(&segment, b"segment").expect("segment staged");
    let segments = vec![segment];
    let playlist = staging.join("playlist.m3u8");
    std::fs::write(
        &playlist,
        format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1.000,\n{segment_name}\n#EXT-X-ENDLIST\n"
        ),
    )
    .expect("playlist staged");
    LadderPacked {
        init,
        playlist,
        segments,
    }
}

fn packed_ladder_job(source: &MediaRef) -> NewJob {
    let rungs = vec![
        OutputProfile::from_raw(24.0, 400_000, 28, 64_000, 48_000, 640, 360).unwrap(),
        OutputProfile::from_raw(24.0, 900_000, 26, 64_000, 48_000, 1280, 720).unwrap(),
    ];
    let spec = LadderSpec::new(source.clone(), rungs)
        .unwrap()
        .with_segment_seconds(1.0)
        .unwrap();
    single_input_job(OpSpec::Ladder(spec))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_registered_packer_answers_a_ladder_end_to_end() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("packer-port-source.mp4", 2, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/packer-port-source.mp4").await;
    let repository = db.repository();
    let job = repository.create(packed_ladder_job(&input)).await.unwrap();

    let packer = Arc::new(RecordingPacker {
        calls: tokio::sync::Mutex::new(Vec::new()),
    });
    let mut config = base_config(&db, store.clone(), "worker-packer-port");
    config.ladder_packer = Some(packer.clone());
    let handle = start(config);
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let calls = packer.calls.lock().await.clone();
    assert_eq!(
        calls.len(),
        2,
        "the port was called once per rung: {calls:?}"
    );
    assert_eq!(calls[0].0, job.id);
    assert_eq!(calls[0].1, 0);
    assert_eq!(calls[1].1, 1);
    assert_eq!(calls[0].2, 1.0);
    let OpResult::Ladder(result) = record.result.expect("result recorded") else {
        panic!("expected a ladder result");
    };
    assert!(
        result.master_playlist().is_some(),
        "a packaged ladder answers a master playlist"
    );
    for (index, rendition) in result.renditions().iter().enumerate() {
        assert!(
            rendition.playlist().is_some(),
            "rung {index} has no playlist"
        );
        let named_init = rendition
            .init_segment()
            .unwrap_or_else(|| panic!("rung {index} names no init segment, so nothing removes it"));
        let MediaRef::Object(named_init) = named_init else {
            panic!("expected an object init segment");
        };
        assert_eq!(
            named_init.key(),
            format!("jobs/{}/renditions/{index}/init.mp4", job.id)
        );
        assert!(
            store
                .head(&ObjectRef::new(BUCKET, named_init.key()).unwrap())
                .await
                .expect("the rung init exists")
                > 0,
            "rung {index} names an init segment that was never written"
        );
        assert_eq!(rendition.segments().len(), 1);
        let MediaRef::Object(playlist) = rendition.playlist().expect("playlist") else {
            panic!("expected an object playlist");
        };
        assert_eq!(
            playlist.key(),
            format!("jobs/{}/renditions/{index}/playlist.m3u8", job.id)
        );
        assert!(store.head(playlist).await.expect("playlist exists") > 0);
    }
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_packer_refusal_ends_the_ladder_and_cleans_every_key() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("packer-port-fail.mp4", 2, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/packer-port-fail.mp4").await;
    let repository = db.repository();
    let job = repository.create(packed_ladder_job(&input)).await.unwrap();

    let packer = Arc::new(FailingPacker {
        calls: tokio::sync::Mutex::new(0),
    });
    let mut config = base_config(&db, store.clone(), "worker-packer-port-fail");
    config.retry = RetryPolicy {
        max_attempts: 1,
        base: Duration::from_millis(100),
        cap: Duration::from_millis(100),
    };
    config.ladder_packer = Some(packer.clone());
    let handle = start(config);
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Failed, "{:?}", record.failure);
    let failure = record.failure.expect("failure recorded");
    assert_eq!(failure.code(), codes::LADDER_PACK_FAILED);
    assert_eq!(
        *packer.calls.lock().await,
        1,
        "the ladder stopped at the first refusal"
    );
    for index in 0..2 {
        let key = ObjectRef::new(
            BUCKET,
            &format!("jobs/{}/renditions/{index}/output.mp4", job.id),
        )
        .unwrap();
        assert!(store.head(&key).await.is_err(), "key {index} survived");
    }
    db.dispose().await;
}
struct MediaPacker;

#[async_trait::async_trait]
impl RungPacker for MediaPacker {
    fn name(&self) -> &'static str {
        "media"
    }

    async fn pack(
        &self,
        _job: mm_domain::JobId,
        _rung: usize,
        _media: &ObjectRef,
        staging_dir: &Path,
        _segment_seconds: f64,
    ) -> Result<LadderPacked, mm_worker::LadderPackError> {
        let init = staging_dir.join("init.mp4");
        std::fs::write(&init, b"init").expect("init staged");
        let segment = staging_dir.join("seg00000.m4s");
        std::fs::write(&segment, b"segment").expect("segment staged");
        let playlist = staging_dir.join("playlist.m3u8");
        std::fs::write(
            &playlist,
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXT-X-TARGETDURATION:1\n#EXTINF:1.000,\nseg00000.m4s\n#EXT-X-ENDLIST\n",
        )
        .expect("playlist staged");
        Ok(LadderPacked {
            init,
            playlist,
            segments: vec![segment],
        })
    }
}

fn master_ladder_job(source: &MediaRef) -> NewJob {
    let rungs = vec![
        OutputProfile::from_raw(24.0, 400_000, 28, 64_000, 48_000, 640, 360).unwrap(),
        OutputProfile::from_raw(24.0, 900_000, 26, 64_000, 48_000, 1280, 720).unwrap(),
    ];
    let spec = LadderSpec::new(source.clone(), rungs)
        .unwrap()
        .with_segment_seconds(1.0)
        .unwrap();
    single_input_job(OpSpec::Ladder(spec))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_packaged_ladder_lands_a_master_playlist_naming_every_rendition() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("master-source.mp4", 2, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/master-source.mp4").await;
    let repository = db.repository();
    let job = repository.create(master_ladder_job(&input)).await.unwrap();

    let mut config = base_config(&db, store.clone(), "worker-master");
    config.ladder_packer = Some(Arc::new(MediaPacker));
    let handle = start(config);
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let OpResult::Ladder(result) = record.result.expect("result recorded") else {
        panic!("expected a ladder result");
    };
    let MediaRef::Object(master) = result.master_playlist().expect("a master playlist") else {
        panic!("expected an object master playlist");
    };
    assert_eq!(master.key(), format!("jobs/{}/master.m3u8", job.id));
    let body = store.get(master).await.expect("master playlist fetches");
    let text = String::from_utf8(collect_body(body).await.unwrap()).expect("utf-8");
    assert!(text.starts_with("#EXTM3U\n#EXT-X-VERSION:7\n"), "{text}");
    assert!(text.contains("#EXT-X-INDEPENDENT-SEGMENTS\n"), "{text}");

    let rows: Vec<&str> = text
        .lines()
        .filter(|line| line.starts_with("#EXT-X-STREAM-INF"))
        .collect();
    assert_eq!(rows.len(), 2, "{text}");
    let bandwidths: Vec<u32> = rows
        .iter()
        .map(|row| {
            let field = row
                .split("BANDWIDTH=")
                .nth(1)
                .and_then(|rest| rest.split(',').next())
                .expect("a bandwidth field");
            field.parse().expect("a numeric bandwidth")
        })
        .collect();
    assert!(bandwidths[0] < bandwidths[1], "ascending: {bandwidths:?}");
    assert!(rows[0].contains("RESOLUTION=640x360"), "{}", rows[0]);
    assert!(rows[1].contains("RESOLUTION=1280x720"), "{}", rows[1]);
    assert!(text.contains("renditions/0/playlist.m3u8"), "{text}");
    assert!(text.contains("renditions/1/playlist.m3u8"), "{text}");

    for (index, rendition) in result.renditions().iter().enumerate() {
        let MediaRef::Object(playlist) = rendition.playlist().expect("rung playlist") else {
            panic!("expected an object playlist");
        };
        assert_eq!(
            playlist.key(),
            format!("jobs/{}/renditions/{index}/playlist.m3u8", job.id)
        );
        assert!(store.head(playlist).await.expect("playlist exists") > 0);
        for segment in rendition.segments() {
            let MediaRef::Object(segment) = segment else {
                panic!("expected an object segment");
            };
            assert_eq!(
                segment.key(),
                format!("jobs/{}/renditions/{index}/seg00000.m4s", job.id)
            );
            assert!(store.head(segment).await.expect("segment exists") > 0);
        }
    }
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_ladder_without_a_segment_length_lands_no_playlist_at_all() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("master-none-source.mp4", 2, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/master-none-source.mp4").await;
    let repository = db.repository();
    let rungs = vec![
        OutputProfile::from_raw(24.0, 400_000, 28, 64_000, 48_000, 640, 360).unwrap(),
        OutputProfile::from_raw(24.0, 900_000, 26, 64_000, 48_000, 1280, 720).unwrap(),
    ];
    let job = repository
        .create(single_input_job(OpSpec::Ladder(
            LadderSpec::new(input, rungs).unwrap(),
        )))
        .await
        .unwrap();

    let handle = start(base_config(&db, store.clone(), "worker-master-none"));
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let OpResult::Ladder(result) = record.result.expect("result recorded") else {
        panic!("expected a ladder result");
    };
    assert_eq!(result.master_playlist(), None);
    for rendition in result.renditions() {
        assert_eq!(rendition.playlist(), None);
        assert!(rendition.segments().is_empty());
    }
    let master = ObjectRef::new(BUCKET, &format!("jobs/{}/master.m3u8", job.id)).unwrap();
    assert!(
        store.head(&master).await.is_err(),
        "a master playlist landed"
    );
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn two_rungs_of_one_ladder_never_share_a_segment_name() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("master-collide-source.mp4", 2, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/master-collide-source.mp4").await;
    let repository = db.repository();
    let job = repository.create(master_ladder_job(&input)).await.unwrap();

    let mut config = base_config(&db, store.clone(), "worker-master-collide");
    config.ladder_packer = Some(Arc::new(MediaPacker));
    let handle = start(config);
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let OpResult::Ladder(result) = record.result.expect("result recorded") else {
        panic!("expected a ladder result");
    };
    let mut keys = Vec::new();
    for rendition in result.renditions() {
        for segment in rendition.segments() {
            let MediaRef::Object(segment) = segment else {
                panic!("expected an object segment");
            };
            keys.push(segment.key().to_string());
        }
    }
    let mut unique = keys.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), keys.len(), "two rungs share a key: {keys:?}");
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_ladder_runs_one_rung_at_a_time_by_default() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("ladder-serial-source.mp4", 2, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/ladder-serial-source.mp4").await;
    let repository = db.repository();
    let job = repository.create(master_ladder_job(&input)).await.unwrap();

    let packer = Arc::new(RecordingPacker {
        calls: tokio::sync::Mutex::new(Vec::new()),
    });
    let mut config = base_config(&db, store.clone(), "worker-ladder-serial");
    config.ladder_packer = Some(packer.clone());
    assert_eq!(
        config.ladder_concurrency, 1,
        "the default is one rung at a time"
    );
    let handle = start(config);
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let calls = packer.calls.lock().await.clone();
    let order: Vec<usize> = calls.iter().map(|call| call.1).collect();
    assert_eq!(order, vec![0, 1], "the default count runs rungs in order");
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_ladder_above_one_rung_encodes_concurrently_and_lands_every_rung() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("ladder-parallel-source.mp4", 2, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/ladder-parallel-source.mp4").await;
    let repository = db.repository();
    let job = repository.create(master_ladder_job(&input)).await.unwrap();

    let packer = Arc::new(RecordingPacker {
        calls: tokio::sync::Mutex::new(Vec::new()),
    });
    let mut config = base_config(&db, store.clone(), "worker-ladder-parallel");
    config.ladder_packer = Some(packer.clone());
    config.ladder_concurrency = 2;
    let handle = start(config);
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let OpResult::Ladder(result) = record.result.expect("result recorded") else {
        panic!("expected a ladder result");
    };
    assert_eq!(result.renditions().len(), 2);
    for (index, rendition) in result.renditions().iter().enumerate() {
        let MediaRef::Object(media) = rendition.media() else {
            panic!("expected an object rendition");
        };
        assert_eq!(
            media.key(),
            format!("jobs/{}/renditions/{index}/output.mp4", job.id)
        );
        assert!(store.head(media).await.expect("rendition exists") > 0);
    }
    let calls = packer.calls.lock().await.clone();
    let mut rungs: Vec<usize> = calls.iter().map(|call| call.1).collect();
    rungs.sort();
    assert_eq!(rungs, vec![0, 1], "both rungs were packed");
    assert!(result.master_playlist().is_some());
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_rung_above_one_stops_the_others_and_leaves_no_key() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("ladder-parallel-fail.mp4", 2, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/ladder-parallel-fail.mp4").await;
    let repository = db.repository();
    let job = repository.create(master_ladder_job(&input)).await.unwrap();

    let packer = Arc::new(FailingPacker {
        calls: tokio::sync::Mutex::new(0),
    });
    let mut config = base_config(&db, store.clone(), "worker-ladder-parallel-fail");
    config.ladder_packer = Some(packer.clone());
    config.ladder_concurrency = 2;
    config.retry = RetryPolicy {
        max_attempts: 1,
        base: Duration::from_millis(100),
        cap: Duration::from_millis(100),
    };
    let handle = start(config);
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Failed, "{:?}", record.failure);
    assert_eq!(
        *packer.calls.lock().await,
        1,
        "the ladder stopped at the first refusal"
    );
    for index in 0..2 {
        let key = ObjectRef::new(
            BUCKET,
            &format!("jobs/{}/renditions/{index}/output.mp4", job.id),
        )
        .unwrap();
        assert!(store.head(&key).await.is_err(), "key {index} survived");
    }
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_ffmpeg_packer_serves_the_same_verified_ladder_as_the_gst_one() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("ffmpeg-packer-source.mp4", 3, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/ffmpeg-packer-source.mp4").await;
    let repository = db.repository();
    let job = repository.create(master_ladder_job(&input)).await.unwrap();

    let mut config = base_config(&db, store.clone(), "worker-ffmpeg-packer");
    config.ladder_packer = Some(Arc::new(FfmpegRungPacker));
    let handle = start(config);
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let OpResult::Ladder(result) = record.result.expect("result recorded") else {
        panic!("expected a ladder result");
    };
    assert_eq!(result.renditions().len(), 2);
    let MediaRef::Object(master) = result.master_playlist().expect("a master playlist") else {
        panic!("expected an object master playlist");
    };
    assert_eq!(master.key(), format!("jobs/{}/master.m3u8", job.id));
    let body = store.get(master).await.expect("master playlist fetches");
    let text = String::from_utf8(collect_body(body).await.unwrap()).expect("utf-8");
    let rows: Vec<&str> = text
        .lines()
        .filter(|line| line.starts_with("#EXT-X-STREAM-INF"))
        .collect();
    assert_eq!(rows.len(), 2, "{text}");
    assert!(rows[0].contains("RESOLUTION=640x360"), "{text}");
    assert!(rows[1].contains("RESOLUTION=1280x720"), "{text}");

    for (index, rendition) in result.renditions().iter().enumerate() {
        let MediaRef::Object(media) = rendition.media() else {
            panic!("expected an object rendition");
        };
        assert_eq!(
            media.key(),
            format!("jobs/{}/renditions/{index}/output.mp4", job.id)
        );
        assert!(store.head(media).await.expect("rendition exists") > 0);
        assert_eq!(rendition.width(), 640 * (index as u32 + 1));
        assert_eq!(rendition.height(), 360 * (index as u32 + 1));
        assert!(rendition.bandwidth_bps() > 0);

        let MediaRef::Object(playlist) = rendition.playlist().expect("a media playlist") else {
            panic!("expected an object playlist");
        };
        assert_eq!(
            playlist.key(),
            format!("jobs/{}/renditions/{index}/playlist.m3u8", job.id)
        );
        let body = store.get(playlist).await.expect("playlist fetches");
        let media_playlist = String::from_utf8(collect_body(body).await.unwrap()).expect("utf-8");
        assert!(media_playlist.starts_with("#EXTM3U"), "{media_playlist}");
        assert!(
            media_playlist.contains("#EXT-X-MAP:URI=\"init.mp4\""),
            "{media_playlist}"
        );
        assert!(
            !rendition.segments().is_empty(),
            "rung {index} has no segments"
        );
        for (position, segment) in rendition.segments().iter().enumerate() {
            let MediaRef::Object(segment) = segment else {
                panic!("expected an object segment");
            };
            assert_eq!(
                segment.key(),
                format!("jobs/{}/renditions/{index}/seg{position:05}.m4s", job.id)
            );
            assert!(store.head(segment).await.expect("segment exists") > 0);
        }
        let init = ObjectRef::new(
            BUCKET,
            &format!("jobs/{}/renditions/{index}/init.mp4", job.id),
        )
        .unwrap();
        assert!(store.head(&init).await.expect("init segment exists") > 0);
    }
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_default_packers_are_gst_and_the_configuration_can_name_the_other() {
    let db = fresh_db().await;
    let store = test_store().await;
    let mut config = base_config(&db, store.clone(), "worker-packer-default");
    assert_eq!(config.ladder_packer_name, "gst");
    assert!(
        config.ladder_packer.is_none(),
        "no packer is registered by default"
    );
    config.ladder_packer_name = String::from("ffmpeg");
    assert_eq!(config.ladder_packer_name, "ffmpeg");
    db.dispose().await;
}

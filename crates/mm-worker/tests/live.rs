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
    AudioSpec, BatchSpec, CallbackError, CallbackPublisher, CaptionFormat, Colour, ConcatSpec,
    GradeAdjustments, GradeScope, GradeSpec, GraphNode, GraphSpec, ImageFormat, ImageOp, ImageSpec,
    InputSource, JobFailure, JobId, JobRecord, JobRepository, JobState, LadderSpec, LoudnessSpec,
    LoudnessTarget, MediaRef, NewJob, NodeRef, ObjectBody, ObjectRef, ObjectStore,
    ObjectStoreError, OpResult, OpSpec, OutputProfile, PackageSpec, ProbeSpec, QueueUrl, RequestId,
    ResizeMode, RungState, ScopeMode, ScopeOutput, StepState, SubtitleMode, SubtitleSpec,
    ThumbnailSpec, Ticks, TitleSpec, TranscodeSpec, TrimSpec, UserId, VideoEncoder, WorkerId,
    collect_body,
};
use mm_ffmpeg::{
    ProbeLimits, Prober, RunLimits as ToolLimits, RunOutcome as ToolOutcome, Runner as ToolRunner,
    build_loudness_measure_args, parse_loudnorm_report,
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
        scratch_dir(&format!("staging/{}", db.name)),
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

async fn states_snapshot(db: &TestDb, ids: &[JobId]) -> (usize, usize) {
    let uuids: Vec<Uuid> = ids.iter().map(|id| id.as_uuid()).collect();
    let (running, terminal): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE state = 'RUNNING'), \
         count(*) FILTER (WHERE state IN ('SUCCEEDED', 'FAILED', 'CANCELLED')) \
         FROM jobs WHERE id = ANY($1)",
    )
    .bind(&uuids)
    .fetch_one(&db.pool)
    .await
    .expect("the states snapshot reads");
    (running as usize, terminal as usize)
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
        let (running, terminal) = states_snapshot(&db, &ids).await;
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
    assert_eq!(record.rungs.len(), 1, "{:?}", record.rungs);
    assert_eq!(record.rungs[0].rung(), 1);
    assert_eq!(record.rungs[0].state(), RungState::Failed);
    assert_eq!(
        record.rungs[0].error_code(),
        Some("ERROR_UPSCALE_NOT_ALLOWED")
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
    calls: tokio::sync::Mutex<Vec<(JobId, usize, Ticks)>>,
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
        segment: Ticks,
    ) -> Result<LadderPacked, LadderPackError> {
        self.calls.lock().await.push((job, rung, segment));
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
        _segment: Ticks,
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
    assert_eq!(calls[0].2, Ticks::from_seconds(1.0).unwrap());
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
        _segment: Ticks,
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

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_terminal_write_keeps_its_row_counted_against_the_bound() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("bound-src-4s.mp4", 4, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/bound-src.mp4").await;
    let repository = db.repository();
    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(
            repository
                .create(concat_job(vec![input.clone(), input.clone()]))
                .await
                .unwrap()
                .id,
        );
    }

    sqlx::query(
        "CREATE FUNCTION mm_refuse_terminal() RETURNS trigger AS $$ \
         BEGIN \
           IF NEW.state IN ('SUCCEEDED', 'FAILED', 'CANCELLED') \
              AND OLD.state = 'RUNNING' THEN \
             RAISE EXCEPTION 'injected terminal write refusal'; \
           END IF; \
           RETURN NEW; \
         END; \
         $$ LANGUAGE plpgsql",
    )
    .execute(&db.pool)
    .await
    .expect("the refusal function installs");
    sqlx::query(
        "CREATE TRIGGER mm_refuse_terminal BEFORE UPDATE ON jobs \
         FOR EACH ROW EXECUTE FUNCTION mm_refuse_terminal()",
    )
    .execute(&db.pool)
    .await
    .expect("the refusal trigger installs");

    let bound = 2usize;
    let mut config = base_config(&db, store.clone(), "worker-bound-refused");
    config.max_concurrent = bound;
    config.claim_batch = NonZeroU32::new(3).unwrap();
    config.lease = Duration::from_secs(60);
    config.heartbeat_interval = Duration::from_secs(3600);
    let handle = start(config);

    let deadline = Instant::now() + Duration::from_secs(180);
    let mut max_running = 0usize;
    let mut reclaimed = false;
    loop {
        let mut running = 0usize;
        for id in &ids {
            let record = repository.get(*id).await.unwrap().unwrap();
            if record.state == JobState::Running {
                running += 1;
            }
            if record.attempts >= 2 {
                reclaimed = true;
            }
        }
        max_running = max_running.max(running);
        if max_running > bound || reclaimed {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the refused write was never reclaimed"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    sqlx::query("DROP TRIGGER mm_refuse_terminal ON jobs")
        .execute(&db.pool)
        .await
        .expect("the refusal trigger drops");
    handle.shutdown().await;

    assert!(
        max_running <= bound,
        "a refused terminal write put {max_running} rows in Running under a bound of {bound}"
    );
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_measure_only_loudness_job_lands_the_numbers_and_no_object() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("loudness-measure.mp4", 4, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/loudness-measure.mp4").await;
    let repository = db.repository();
    let spec = LoudnessSpec::new(input, None).expect("a measure-only spec is valid");
    let job = repository
        .create(single_input_job(OpSpec::Loudness(spec)))
        .await
        .unwrap();
    let handle = start(base_config(&db, store.clone(), "worker-loudness-measure"));
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let OpResult::Loudness(result) = record.result.expect("result recorded") else {
        panic!("expected a loudness result");
    };
    assert!(
        result.output().is_none(),
        "a measure-only job names no output"
    );
    assert!(result.integrated_lufs().is_finite());
    assert!(result.true_peak_db().is_finite());
    assert!(result.loudness_range_lu() >= 0.0);
    let key = ObjectRef::new(BUCKET, &format!("jobs/{}/output.mp4", job.id)).unwrap();
    assert!(
        store.head(&key).await.is_err(),
        "a measure-only job landed an output object"
    );
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_normalised_loudness_job_lands_an_output_at_the_target_and_ceiling() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("loudness-normalize.mp4", 4, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/loudness-normalize.mp4").await;
    let repository = db.repository();
    let target = LoudnessTarget::new(-16.0, -1.5).expect("the target is in bounds");
    let spec = LoudnessSpec::new(input, Some(target)).expect("a normalising spec is valid");
    let job = repository
        .create(single_input_job(OpSpec::Loudness(spec)))
        .await
        .unwrap();
    let handle = start(base_config(&db, store.clone(), "worker-loudness-normalize"));
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let OpResult::Loudness(result) = record.result.expect("result recorded") else {
        panic!("expected a loudness result");
    };
    let output = result.output().expect("a normalising job names its output");
    let MediaRef::Object(object) = output else {
        panic!("expected an object output");
    };
    assert_eq!(object.key(), format!("jobs/{}/output.mp4", job.id));
    let path = scratch_dir("loudness").join(format!("{}.mp4", job.id));
    let bytes = collect_body(store.get(object).await.expect("the output is readable"))
        .await
        .expect("the output body is read");
    std::fs::write(&path, bytes).expect("the output is staged");
    let measured = measure_file(&path).await;
    assert!(
        (measured.integrated_lufs + 16.0).abs() <= 1.0,
        "measured {} LUFS is outside one unit of -16",
        measured.integrated_lufs
    );
    assert!(
        measured.true_peak_db <= -1.5,
        "measured true peak {} is above the -1.5 ceiling",
        measured.true_peak_db
    );
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_loudness_job_on_an_input_without_audio_blocks_by_name() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture_without_audio("loudness-silent.mp4", 3);
    let input = upload_input(&store, &source, "inputs/loudness-silent.mp4").await;
    let repository = db.repository();
    let spec = LoudnessSpec::new(input, None).expect("a measure-only spec is valid");
    let job = repository
        .create(single_input_job(OpSpec::Loudness(spec)))
        .await
        .unwrap();
    let handle = start(base_config(&db, store.clone(), "worker-loudness-silent"));
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Failed, "{:?}", record.result);
    let failure = record.failure.expect("failure recorded");
    assert_eq!(failure.code(), codes::VALIDATION_BLOCKED);
    let input = failure.inputs().first().expect("the input is named");
    assert_eq!(input.code(), "ERROR_NO_AUDIO_TRACK");
    assert_eq!(
        mm_domain::failure_code(&failure),
        "ERROR_NO_AUDIO_TRACK",
        "the reference code reaches the caller"
    );
    let key = ObjectRef::new(BUCKET, &format!("jobs/{}/output.mp4", job.id)).unwrap();
    assert!(
        store.head(&key).await.is_err(),
        "a blocked job landed an output"
    );
    db.dispose().await;
}

fn fixture_without_audio(name: &str, seconds: u32) -> PathBuf {
    static GENERATION: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = GENERATION.lock().unwrap();
    let path = scratch_dir("fixtures").join(name);
    if path.is_file() && path.metadata().unwrap().len() > 0 {
        return path;
    }
    let output = Command::new("ffmpeg")
        .arg("-y")
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg(format!("testsrc2=duration={seconds}:size=320x180:rate=24"))
        .arg("-c:v")
        .arg("libx264")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-an")
        .arg("-f")
        .arg("mp4")
        .arg(&path)
        .output()
        .expect("the media tool must be runnable");
    assert!(output.status.success(), "fixture generation failed: {name}");
    path
}

async fn measure_file(path: &Path) -> mm_ffmpeg::LoudnessMeasurement {
    let args = build_loudness_measure_args(path.to_string_lossy().as_ref());
    let limits = ToolLimits {
        total_duration: Duration::from_secs(30),
        deadline: Duration::from_secs(120),
        stderr_tail_bytes: 64 * 1024,
    };
    let run = ToolRunner::default()
        .start(&args, &limits)
        .expect("the meter starts");
    let (outcome, stderr) = run.wait_with_stderr().await.expect("the run is waited");
    assert_eq!(outcome, ToolOutcome::Succeeded, "{stderr}");
    parse_loudnorm_report(&stderr).expect("the meter reports")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_burned_subtitle_changes_the_frame_at_a_cue() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("subtitle-burn.mp4", 4, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/subtitle-burn.mp4").await;
    let caption = caption_file(
        "subtitle-burn",
        "srt",
        "1\n00:00:00,500 --> 00:00:03,500\nBURNED CAPTION\n",
    );
    let captions = upload_input(&store, &caption, "inputs/subtitle-burn.srt").await;
    let repository = db.repository();
    let spec = SubtitleSpec::new(
        input,
        Some(captions.into()),
        Some(CaptionFormat::Srt),
        SubtitleMode::Burn,
    )
    .unwrap();
    let job = repository
        .create(single_input_job(OpSpec::Subtitle(spec)))
        .await
        .unwrap();
    let handle = start(base_config(&db, store.clone(), "worker-subtitle-burn"));
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let OpResult::Subtitle(result) = record.result.expect("result recorded") else {
        panic!("expected a subtitle result");
    };
    let MediaRef::Object(output) = result.output() else {
        panic!("expected an object output");
    };
    let path = scratch_dir("subtitle").join(format!("{}-burn.mp4", job.id));
    write_object(&store, output, &path).await;
    let source_frame = extract_frame(&source, "subtitle-burn-source.jpg");
    let output_frame = extract_frame(&path, "subtitle-burn-output.jpg");
    assert_ne!(
        std::fs::read(&source_frame).unwrap(),
        std::fs::read(&output_frame).unwrap(),
        "the cue left the frame unchanged"
    );
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_muxed_subtitle_is_reported_by_the_probe() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("subtitle-mux.mp4", 3, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/subtitle-mux.mp4").await;
    let caption = caption_file(
        "subtitle-mux",
        "vtt",
        "WEBVTT\n\n00:00:00.500 --> 00:00:02.500\nMUXED CAPTION\n",
    );
    let captions = upload_input(&store, &caption, "inputs/subtitle-mux.vtt").await;
    let repository = db.repository();
    let spec = SubtitleSpec::new(
        input,
        Some(captions.into()),
        Some(CaptionFormat::WebVtt),
        SubtitleMode::Mux,
    )
    .unwrap();
    let job = repository
        .create(single_input_job(OpSpec::Subtitle(spec)))
        .await
        .unwrap();
    let handle = start(base_config(&db, store.clone(), "worker-subtitle-mux"));
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let OpResult::Subtitle(result) = record.result.expect("result recorded") else {
        panic!("expected a subtitle result");
    };
    let MediaRef::Object(output) = result.output() else {
        panic!("expected an object output");
    };
    let path = scratch_dir("subtitle").join(format!("{}-mux.mp4", job.id));
    write_object(&store, output, &path).await;
    let info = Prober::default()
        .probe(&path.to_string_lossy(), &ProbeLimits::default())
        .await
        .expect("the muxed output probes");
    assert_eq!(info.subtitle.len(), 1, "no subtitle stream in {info:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stripped_subtitle_leaves_no_track() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture_with_subtitles("subtitle-strip.mp4", 3);
    let input = upload_input(&store, &source, "inputs/subtitle-strip.mp4").await;
    let repository = db.repository();
    let spec = SubtitleSpec::new(input, None, None, SubtitleMode::Strip).unwrap();
    let job = repository
        .create(single_input_job(OpSpec::Subtitle(spec)))
        .await
        .unwrap();
    let handle = start(base_config(&db, store.clone(), "worker-subtitle-strip"));
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let OpResult::Subtitle(result) = record.result.expect("result recorded") else {
        panic!("expected a subtitle result");
    };
    let MediaRef::Object(output) = result.output() else {
        panic!("expected an object output");
    };
    let path = scratch_dir("subtitle").join(format!("{}-strip.mp4", job.id));
    write_object(&store, output, &path).await;
    let info = Prober::default()
        .probe(&path.to_string_lossy(), &ProbeLimits::default())
        .await
        .expect("the stripped output probes");
    assert!(
        info.subtitle.is_empty(),
        "a subtitle track survived: {info:?}"
    );
    db.dispose().await;
}

fn caption_file(name: &str, extension: &str, text: &str) -> PathBuf {
    let path = scratch_dir("fixtures").join(format!("{name}.{extension}"));
    std::fs::write(&path, text).expect("caption file");
    path
}

async fn write_object(store: &S3ObjectStore, object: &ObjectRef, path: &Path) {
    let bytes = collect_body(store.get(object).await.expect("the output is readable"))
        .await
        .expect("the output body is read");
    std::fs::write(path, bytes).expect("the output is staged");
}

fn extract_frame(input: &Path, name: &str) -> PathBuf {
    let output = scratch_dir("subtitle").join(name);
    let status = Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error", "-ss", "1", "-i"])
        .arg(input)
        .args(["-frames:v", "1", "-f", "image2"])
        .arg(&output)
        .status()
        .expect("ffmpeg must be runnable");
    assert!(status.success(), "frame extraction failed");
    output
}

fn fixture_with_subtitles(name: &str, seconds: u32) -> PathBuf {
    let video = fixture(name, seconds, 24, "mp4");
    let captions = caption_file(
        &format!("{name}-text"),
        "srt",
        "1\n00:00:00,500 --> 00:00:02,500\nSTRIP ME\n",
    );
    let output_path = scratch_dir("fixtures").join(format!(
        "{}-with-subtitles.mp4",
        name.trim_end_matches(".mp4")
    ));
    let status = Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error", "-i"])
        .arg(&video)
        .args(["-i"])
        .arg(&captions)
        .args([
            "-map",
            "0",
            "-map",
            "1:0",
            "-c",
            "copy",
            "-c:s",
            "mov_text",
            "-movflags",
            "+faststart",
            "-f",
            "mp4",
        ])
        .arg(&output_path)
        .status()
        .expect("ffmpeg must be runnable");
    assert!(status.success(), "subtitle fixture generation failed");
    output_path
}

#[tokio::test(flavor = "multi_thread")]
async fn a_brighter_grade_moves_the_mean_luma_and_lands_a_scope() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("grade-bright.mp4", 4, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/grade-bright.mp4").await;
    let repository = db.repository();
    let adjustments = GradeAdjustments::new(0.3, 1.0, 1.0, 1.0).unwrap();
    let spec = GradeSpec::new(
        input,
        adjustments,
        None,
        Some(GradeScope::new(160, 120).unwrap()),
    )
    .unwrap();
    let job = repository
        .create(single_input_job(OpSpec::Grade(spec)))
        .await
        .unwrap();
    let handle = start(base_config(&db, store.clone(), "worker-grade-bright"));
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let OpResult::Grade(result) = record.result.expect("result recorded") else {
        panic!("expected a grade result");
    };
    let MediaRef::Object(output) = result.output() else {
        panic!("expected an object output");
    };
    let path = scratch_dir("grade").join(format!("{}-graded.mp4", job.id));
    write_object(&store, output, &path).await;
    let source_frame = extract_frame(&source, "grade-source.jpg");
    let graded_frame = extract_frame(&path, "grade-output.jpg");
    assert!(
        mean_luma(&graded_frame) > mean_luma(&source_frame),
        "the grade did not brighten the frame"
    );
    let MediaRef::Object(scope) = result.scope().expect("a scope was requested") else {
        panic!("expected a scope object");
    };
    assert_eq!(scope.key(), format!("jobs/{}/scope.jpg", job.id));
    let scope_path = scratch_dir("grade").join(format!("{}-scope.jpg", job.id));
    write_object(&store, scope, &scope_path).await;
    let still = Prober::default()
        .still(&scope_path.to_string_lossy(), &ProbeLimits::default())
        .await
        .expect("the scope probes");
    assert_eq!((still.width, still.height), (160, 120));
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_graded_job_answers_its_scope_data_over_the_graded_frame() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("grade-scope.mp4", 4, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/grade-scope.mp4").await;
    let repository = db.repository();
    let adjustments = GradeAdjustments::new(0.2, 1.1, 1.0, 1.0).unwrap();
    let scope = GradeScope::new(160, 120)
        .unwrap()
        .with_mode(ScopeMode::Parade)
        .with_output(ScopeOutput::Data)
        .with_grid(64, 256);
    let spec = GradeSpec::new(input, adjustments, None, Some(scope)).unwrap();
    let job = repository
        .create(single_input_job(OpSpec::Grade(spec)))
        .await
        .unwrap();
    let handle = start(base_config(&db, store.clone(), "worker-grade-scope"));
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let OpResult::Grade(result) = record.result.expect("result recorded") else {
        panic!("expected a grade result");
    };
    let MediaRef::Object(output) = result.output() else {
        panic!("expected an object output");
    };
    assert_eq!(output.key(), format!("jobs/{}/output.mp4", job.id));
    assert!(result.scope().is_none(), "a data scope lands no image");
    let data = result.data().expect("the scope carries data");
    assert_eq!(data.mode(), ScopeMode::Parade);
    assert_eq!(data.components(), 3);
    assert_eq!(data.columns(), 64);
    assert_eq!(data.bins(), 256);

    let path = scratch_dir("grade").join(format!("{}-scoped.mp4", job.id));
    write_object(&store, output, &path).await;
    let info = Prober::default()
        .probe(&path.to_string_lossy(), &ProbeLimits::default())
        .await
        .expect("the graded output probes");
    let stream = info.video.first().expect("a video stream");
    let pixels = u64::from(stream.width) * u64::from(stream.height);
    assert_eq!(
        data.total(),
        3 * pixels,
        "every pixel of the frame is counted in every panel"
    );

    let tool = tool_channel_means(&path);
    for (component, name) in ["red", "green", "blue"].into_iter().enumerate() {
        let measured = mean_of(data.panel(component as u32), data.bins());
        assert!(
            (measured - tool[component]).abs() <= 1.0,
            "the scope read a mean {name} of {measured}, the tool read {}",
            tool[component]
        );
    }
    db.dispose().await;
}

fn mean_of(panel: &[u32], bins: u32) -> f64 {
    let counted: u64 = panel.iter().map(|count| u64::from(*count)).sum();
    assert!(counted > 0, "the panel counts nothing");
    panel
        .iter()
        .enumerate()
        .map(|(index, count)| ((index as u32) % bins) as f64 * f64::from(*count))
        .sum::<f64>()
        / counted as f64
}

fn tool_channel_means(path: &Path) -> [f64; 3] {
    let output = Command::new("convert")
        .arg(format!("{}[0]", path.to_string_lossy()))
        .args(["-depth", "8", "-format"])
        .arg("%[fx:mean.r*255],%[fx:mean.g*255],%[fx:mean.b*255]")
        .arg("info:")
        .output()
        .expect("ImageMagick must be runnable");
    assert!(output.status.success(), "the tool read no frame");
    let text = String::from_utf8_lossy(&output.stdout);
    let means: Vec<f64> = text
        .trim()
        .split(',')
        .map(|value| value.parse().expect("a channel mean is a number"))
        .collect();
    assert_eq!(means.len(), 3, "the tool read three channels: {text}");
    [means[0], means[1], means[2]]
}

#[tokio::test(flavor = "multi_thread")]
async fn a_valid_lookup_table_is_fetched_and_applied() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("grade-lut.mp4", 3, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/grade-lut.mp4").await;
    let table = caption_file(
        "grade-lut",
        "cube",
        "LUT_3D_SIZE 2\n0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n",
    );
    let lut = upload_input(&store, &table, "inputs/grade-lut.cube").await;
    let repository = db.repository();
    let spec = GradeSpec::new(input, GradeAdjustments::default(), Some(lut.into()), None).unwrap();
    let job = repository
        .create(single_input_job(OpSpec::Grade(spec)))
        .await
        .unwrap();
    let handle = start(base_config(&db, store.clone(), "worker-grade-lut"));
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let OpResult::Grade(result) = record.result.expect("result recorded") else {
        panic!("expected a grade result");
    };
    let MediaRef::Object(output) = result.output() else {
        panic!("expected an object output");
    };
    assert!(store.head(output).await.expect("output exists") > 0);
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lookup_table_that_is_not_a_table_is_refused_by_name() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("grade-bad-lut.mp4", 3, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/grade-bad-lut.mp4").await;
    let table = caption_file("grade-bad-lut", "cube", "this is not a cube table\n");
    let lut = upload_input(&store, &table, "inputs/grade-bad-lut.cube").await;
    let repository = db.repository();
    let spec = GradeSpec::new(input, GradeAdjustments::default(), Some(lut.into()), None).unwrap();
    let job = repository
        .create(single_input_job(OpSpec::Grade(spec)))
        .await
        .unwrap();
    let handle = start(base_config(&db, store.clone(), "worker-grade-bad-lut"));
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Failed, "{:?}", record.result);
    assert_eq!(
        record.failure.expect("failure recorded").code(),
        codes::GRADE_INVALID
    );
    let key = ObjectRef::new(BUCKET, &format!("jobs/{}/output.mp4", job.id)).unwrap();
    assert!(
        store.head(&key).await.is_err(),
        "a refused table landed an output"
    );
    db.dispose().await;
}

fn mean_luma(path: &Path) -> f64 {
    let output = Command::new("convert")
        .arg(path)
        .args(["-colorspace", "Gray", "-format", "%[fx:mean]", "info:"])
        .output()
        .expect("ImageMagick must be runnable");
    assert!(output.status.success(), "mean luma failed");
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .expect("mean luma is a number")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_gain_moves_the_measured_loudness_by_the_gain() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("audio-gain.mp4", 6, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/audio-gain.mp4").await;
    let repository = db.repository();
    let spec =
        AudioSpec::new(vec![input], 6.0, Ticks::ZERO, Ticks::ZERO, None, None, None).unwrap();
    let job = repository
        .create(single_input_job(OpSpec::Audio(spec)))
        .await
        .unwrap();
    let handle = start(base_config(&db, store.clone(), "worker-audio-gain"));
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let OpResult::Audio(result) = record.result.expect("result recorded") else {
        panic!("expected an audio result");
    };
    let MediaRef::Object(output) = result.output() else {
        panic!("expected an object output");
    };
    let path = scratch_dir("audio").join(format!("{}-gain.wav", job.id));
    write_object(&store, output, &path).await;
    let before = measure_file(&source).await.integrated_lufs;
    let after = measure_file(&path).await.integrated_lufs;
    assert!(
        (after - before - 6.0).abs() < 1.5,
        "gain moved loudness from {before} to {after}"
    );
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn fades_make_the_first_and_last_windows_quieter_than_the_middle() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("audio-fade.mp4", 6, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/audio-fade.mp4").await;
    let repository = db.repository();
    let spec = AudioSpec::new(
        vec![input],
        0.0,
        Ticks::from_seconds(1.0).unwrap(),
        Ticks::from_seconds(1.0).unwrap(),
        None,
        None,
        None,
    )
    .unwrap();
    let job = repository
        .create(single_input_job(OpSpec::Audio(spec)))
        .await
        .unwrap();
    let handle = start(base_config(&db, store.clone(), "worker-audio-fade"));
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let OpResult::Audio(result) = record.result.expect("result recorded") else {
        panic!("expected an audio result");
    };
    let MediaRef::Object(output) = result.output() else {
        panic!("expected an object output");
    };
    let path = scratch_dir("audio").join(format!("{}-fade.wav", job.id));
    write_object(&store, output, &path).await;
    let first = window_mean_db(&path, 0.05, 0.4);
    let middle = window_mean_db(&path, 2.8, 0.4);
    let last = window_mean_db(&path, 5.55, 0.4);
    assert!(first < middle - 10.0, "first {first} middle {middle}");
    assert!(last < middle - 10.0, "last {last} middle {middle}");
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn two_inputs_mix_into_the_longer_duration() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let long = fixture("audio-mix-long.mp4", 5, 24, "mp4");
    let short = fixture("audio-mix-short.mp4", 3, 24, "mp4");
    let long_input = upload_input(&store, &long, "inputs/audio-mix-long.mp4").await;
    let short_input = upload_input(&store, &short, "inputs/audio-mix-short.mp4").await;
    let repository = db.repository();
    let spec = AudioSpec::new(
        vec![long_input, short_input],
        0.0,
        Ticks::ZERO,
        Ticks::ZERO,
        None,
        None,
        None,
    )
    .unwrap();
    let job = repository
        .create(single_input_job(OpSpec::Audio(spec)))
        .await
        .unwrap();
    let handle = start(base_config(&db, store.clone(), "worker-audio-mix"));
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let OpResult::Audio(result) = record.result.expect("result recorded") else {
        panic!("expected an audio result");
    };
    assert!(
        (result.duration_seconds() - 5.0).abs() < 0.3,
        "mixed duration {}",
        result.duration_seconds()
    );
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_limiter_holds_the_true_peak_at_or_below_its_ceiling() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("audio-limit.mp4", 4, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/audio-limit.mp4").await;
    let repository = db.repository();
    let spec = AudioSpec::new(
        vec![input],
        18.0,
        Ticks::ZERO,
        Ticks::ZERO,
        None,
        None,
        Some(-3.0),
    )
    .unwrap();
    let job = repository
        .create(single_input_job(OpSpec::Audio(spec)))
        .await
        .unwrap();
    let handle = start(base_config(&db, store.clone(), "worker-audio-limit"));
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let OpResult::Audio(result) = record.result.expect("result recorded") else {
        panic!("expected an audio result");
    };
    let MediaRef::Object(output) = result.output() else {
        panic!("expected an object output");
    };
    let path = scratch_dir("audio").join(format!("{}-limit.wav", job.id));
    write_object(&store, output, &path).await;
    let measured = measure_file(&path).await;
    assert!(
        measured.true_peak_db <= -2.9,
        "true peak {} dBTP exceeded the ceiling",
        measured.true_peak_db
    );
    db.dispose().await;
}

fn window_mean_db(path: &Path, start: f64, duration: f64) -> f64 {
    let output = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-nostats",
            "-ss",
            &start.to_string(),
            "-t",
            &duration.to_string(),
            "-i",
        ])
        .arg(path)
        .args(["-af", "volumedetect", "-f", "null", "-"])
        .output()
        .expect("the media tool must be runnable");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let line = stderr
        .lines()
        .find(|line| line.contains("mean_volume:"))
        .expect("volumedetect reports a mean");
    line.split("mean_volume:")
        .nth(1)
        .and_then(|rest| rest.trim().trim_end_matches(" dB").parse().ok())
        .expect("the mean is a number")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_title_lands_a_transparent_still_at_the_box_dimensions() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let repository = db.repository();
    let spec = TitleSpec::new(
        "Hello",
        320,
        120,
        32,
        Colour::parse("text", "#ffffff").unwrap(),
        None,
        ImageFormat::Png,
    )
    .unwrap();
    let job = repository
        .create(single_input_job(OpSpec::Title(spec)))
        .await
        .unwrap();
    let handle = start(base_config(&db, store.clone(), "worker-title"));
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let OpResult::Title(result) = record.result.expect("result recorded") else {
        panic!("expected a title result");
    };
    let MediaRef::Object(object) = result.output() else {
        panic!("expected an object output");
    };
    assert_eq!(object.key(), format!("jobs/{}/title.png", job.id));
    let path = scratch_dir("title").join(format!("{}-title.png", job.id));
    write_object(&store, object, &path).await;
    let (width, height, format) = identify_frame(&store, object).await;
    assert_eq!((width, height), (320, 120));
    assert_eq!(format, "PNG");
    assert_eq!(magick_format(&path, "%[opaque]"), "False");
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_graph_composes_a_title_onto_a_frame() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let base = image_fixture("title-base.png", 400, 300, "white");
    let base_ref = upload_input(&store, &base, "inputs/title-base.png").await;
    let repository = db.repository();
    let title = TitleSpec::new(
        "Hello",
        200,
        100,
        32,
        Colour::parse("text", "#000000").unwrap(),
        None,
        ImageFormat::Png,
    )
    .unwrap();
    let composite = ImageSpec::new(
        vec![
            InputSource::from(NodeRef::new("title", 0).unwrap()),
            InputSource::from(base_ref),
        ],
        ImageOp::Composite,
        0,
        0,
        ResizeMode::Fit,
        Some(ImageFormat::Jpeg),
    )
    .unwrap();
    let graph = GraphSpec::new(vec![
        GraphNode::new("title", OpSpec::Title(title)).unwrap(),
        GraphNode::new("comp", OpSpec::Image(composite)).unwrap(),
    ])
    .unwrap();
    let job = repository
        .create(single_input_job(OpSpec::Graph(graph)))
        .await
        .unwrap();
    let handle = start(base_config(&db, store.clone(), "worker-title-graph"));
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let OpResult::Graph(graph) = record.result.expect("result recorded") else {
        panic!("expected a graph result");
    };
    let node = graph
        .nodes()
        .iter()
        .find(|node| node.id().as_str() == "comp")
        .expect("the composite node recorded");
    let StepState::Result(OpResult::Image(image)) = node.state() else {
        panic!("expected the composite image result");
    };
    let MediaRef::Object(object) = &image.outputs()[0] else {
        panic!("expected an object output");
    };
    let path = scratch_dir("title").join(format!("{}-graph.jpg", job.id));
    write_object(&store, object, &path).await;
    let (width, height, format) = identify_frame(&store, object).await;
    assert_eq!((width, height), (400, 300));
    assert_eq!(format, "JPEG");
    assert!(
        mean_luma(&path) < 0.999,
        "the title was not laid over the white frame"
    );
    db.dispose().await;
}

fn magick_format(path: &Path, format: &str) -> String {
    let output = Command::new("magick")
        .arg(path)
        .arg("-format")
        .arg(format)
        .arg("info:")
        .output()
        .expect("the image tool must be runnable");
    assert!(output.status.success(), "image inspection failed");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_hardware_encoder_falls_back_and_names_software() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("encoder-fallback.mp4", 3, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/encoder-fallback.mp4").await;
    let repository = db.repository();
    let profile = OutputProfile::from_raw(30.0, 4000, 23, 128_000, 48_000, 1280, 720)
        .unwrap()
        .with_encoder(VideoEncoder::H264Nvenc, false)
        .unwrap();
    let spec = TranscodeSpec::new(input, profile).unwrap();
    let job = repository
        .create(single_input_job(OpSpec::Transcode(spec)))
        .await
        .unwrap();
    let handle = start(base_config(&db, store.clone(), "worker-encoder-fallback"));
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    let OpResult::Transcode(result) = record.result.expect("result recorded") else {
        panic!("expected a transcode result");
    };
    assert_eq!(result.encoder(), VideoEncoder::Libx264);
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_required_encoder_the_host_lacks_fails_by_name() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("encoder-required.mp4", 3, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/encoder-required.mp4").await;
    let repository = db.repository();
    let profile = OutputProfile::from_raw(30.0, 4000, 23, 128_000, 48_000, 1280, 720)
        .unwrap()
        .with_encoder(VideoEncoder::H264Qsv, true)
        .unwrap();
    let spec = TranscodeSpec::new(input, profile).unwrap();
    let job = repository
        .create(single_input_job(OpSpec::Transcode(spec)))
        .await
        .unwrap();
    let handle = start(base_config(&db, store.clone(), "worker-encoder-required"));
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Failed, "{:?}", record.result);
    let failure = record.failure.expect("failure recorded");
    assert_eq!(failure.code(), codes::ENCODER_UNAVAILABLE);
    assert!(
        failure.message().contains("h264_qsv"),
        "{}",
        failure.message()
    );
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_planted_journal_for_a_failed_job_reclaims_staged_and_landed() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("journal-failed.mp4", 2, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/journal-failed.mp4").await;
    let repository = db.repository();
    let job = repository
        .create(single_input_job(OpSpec::Probe(
            ProbeSpec::new(input).expect("a bare reference cuts nothing"),
        )))
        .await
        .unwrap();
    repository
        .claim(
            &worker_id("journal-planter"),
            std::num::NonZeroU32::new(1).unwrap(),
            Duration::from_secs(300),
        )
        .await
        .unwrap();
    repository
        .mark_failed(
            job.id,
            &worker_id("journal-planter"),
            JobFailure::new(codes::MERGE_FAILED, "planted").unwrap(),
            None,
        )
        .await
        .unwrap();
    let config = base_config(&db, store.clone(), "journal-reclaim");
    let staging = config.staging_dir.clone();
    tokio::fs::create_dir_all(staging.join("journals"))
        .await
        .unwrap();
    let staged = staging.join(format!("{}.mp4", job.id));
    tokio::fs::write(&staged, b"staged").await.unwrap();
    let landed = ObjectRef::new(BUCKET, &format!("jobs/{}/output.mp4", job.id)).unwrap();
    mm_testkit::upload_object(store.as_ref(), &source, BUCKET, landed.key()).await;
    let journal = staging.join("journals").join(format!("{}.json", job.id));
    let entry = format!(
        "{{\"job_id\":\"{}\",\"staged\":[\"{}\"],\"landed\":[{{\"bucket\":\"{}\",\"key\":\"{}\"}}]}}",
        job.id,
        staged.display(),
        BUCKET,
        landed.key()
    );
    tokio::fs::write(&journal, entry).await.unwrap();
    let handle = start(config);
    wait_for(|| async {
        !staged.exists() && store.head(&landed).await.is_err() && !journal.exists()
    })
    .await;
    handle.shutdown().await;

    assert!(!staged.exists(), "the staged file was not reclaimed");
    assert!(
        store.head(&landed).await.is_err(),
        "the landed key of a failed job survived"
    );
    assert!(!journal.exists(), "the journal entry survived");
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_planted_journal_for_a_running_job_is_left_for_its_holder() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("journal-running.mp4", 2, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/journal-running.mp4").await;
    let repository = db.repository();
    let job = repository
        .create(single_input_job(OpSpec::Probe(
            ProbeSpec::new(input).expect("a bare reference cuts nothing"),
        )))
        .await
        .unwrap();
    repository
        .claim(
            &worker_id("journal-holder"),
            std::num::NonZeroU32::new(1).unwrap(),
            Duration::from_secs(300),
        )
        .await
        .unwrap();
    let config = base_config(&db, store.clone(), "journal-observer");
    let staging = config.staging_dir.clone();
    tokio::fs::create_dir_all(staging.join("journals"))
        .await
        .unwrap();
    let staged = staging.join(format!("{}.mp4", job.id));
    tokio::fs::write(&staged, b"staged").await.unwrap();
    let landed = ObjectRef::new(BUCKET, &format!("jobs/{}/output.mp4", job.id)).unwrap();
    mm_testkit::upload_object(store.as_ref(), &source, BUCKET, landed.key()).await;
    let journal = staging.join("journals").join(format!("{}.json", job.id));
    tokio::fs::write(
        &journal,
        format!(
            "{{\"job_id\":\"{}\",\"staged\":[\"{}\"],\"landed\":[{{\"bucket\":\"{}\",\"key\":\"{}\"}}]}}",
            job.id,
            staged.display(),
            BUCKET,
            landed.key()
        ),
    )
    .await
    .unwrap();
    let handle = start(config);
    tokio::time::sleep(Duration::from_millis(800)).await;
    handle.shutdown().await;

    assert!(staged.exists(), "a running job's staged file was reclaimed");
    assert!(
        store.head(&landed).await.is_ok(),
        "a running job's landed key was deleted"
    );
    assert!(journal.exists(), "a running job's journal was removed");
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreadable_journal_is_moved_aside_not_deleted() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let config = base_config(&db, store.clone(), "journal-corrupt");
    let staging = config.staging_dir.clone();
    tokio::fs::create_dir_all(staging.join("journals"))
        .await
        .unwrap();
    let journal = staging.join("journals").join("not-json.json");
    tokio::fs::write(&journal, b"{not json").await.unwrap();
    let handle = start(config);
    wait_for(|| async { !journal.exists() }).await;
    handle.shutdown().await;

    let moved = std::fs::read_dir(staging.join("journals"))
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .find(|name| name.starts_with("not-json.corrupt-"));
    assert!(
        moved.is_some(),
        "an unreadable journal was not moved under a named file"
    );
    db.dispose().await;
}

async fn wait_for<F, Fut>(check: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the condition was not met before the deadline");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_terminal_job_removes_its_journal_and_staged_files() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("journal-success.mp4", 3, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/journal-success.mp4").await;
    let repository = db.repository();
    let job = repository
        .create(single_input_job(OpSpec::Probe(
            ProbeSpec::new(input).expect("a bare reference cuts nothing"),
        )))
        .await
        .unwrap();
    let config = base_config(&db, store.clone(), "journal-success");
    let staging = config.staging_dir.clone();
    let journal = staging.join("journals").join(format!("{}.json", job.id));
    let handle = start(config);
    let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;

    assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
    wait_for(|| async { !journal.exists() }).await;
    let staged: Vec<String> = std::fs::read_dir(&staging)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.starts_with(&job.id.to_string()))
        .collect();
    assert!(staged.is_empty(), "staged paths survived: {staged:?}");
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_transcode_copies_the_cached_output_without_an_encoder() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("cache-transcode.mp4", 4, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/cache-transcode.mp4").await;
    let repository = db.repository();
    let profile = OutputProfile::from_raw(24.0, 1200, 23, 128_000, 48_000, 0, 0).unwrap();
    let spec = TranscodeSpec::new(input.clone(), profile).unwrap();
    let first = repository
        .create(single_input_job(OpSpec::Transcode(spec.clone())))
        .await
        .unwrap();
    let config = base_config(&db, store.clone(), "worker-cache");
    let metrics = config.metrics.clone();
    let handle = start(config);
    let first_record =
        await_terminal(&repository, first.id, mm_testkit::budget::MERGE_TERMINAL).await;
    assert_eq!(
        first_record.state,
        JobState::Succeeded,
        "{:?}",
        first_record.failure
    );
    let OpResult::Transcode(first_result) = first_record.result.expect("result recorded") else {
        panic!("expected a transcode result");
    };
    let MediaRef::Object(first_output) = first_result.output() else {
        panic!("expected an object output");
    };
    assert_eq!(first_output.key(), format!("jobs/{}/output.mp4", first.id));
    assert_eq!(metrics.cache_hits.value(), 0);
    assert_eq!(metrics.cache_misses.value(), 1);

    let second = repository
        .create(single_input_job(OpSpec::Transcode(spec.clone())))
        .await
        .unwrap();
    let second_record =
        await_terminal(&repository, second.id, mm_testkit::budget::MERGE_TERMINAL).await;
    assert_eq!(
        second_record.state,
        JobState::Succeeded,
        "{:?}",
        second_record.failure
    );
    let OpResult::Transcode(second_result) = second_record.result.expect("result recorded") else {
        panic!("expected a transcode result");
    };
    let MediaRef::Object(second_output) = second_result.output() else {
        panic!("expected an object output");
    };
    assert_eq!(
        second_output.key(),
        format!("jobs/{}/output.mp4", second.id)
    );
    assert_ne!(second_output.key(), first_output.key());
    assert_eq!(second_result.encoder(), first_result.encoder());
    assert_eq!(metrics.cache_hits.value(), 1, "the second job did not hit");
    assert_eq!(
        metrics.cache_misses.value(),
        1,
        "the second job ran the encoder"
    );

    let changed = fixture("cache-transcode-changed.mp4", 4, 24, "mp4");
    let changed_input = upload_input(&store, &changed, "inputs/cache-transcode-changed.mp4").await;
    let changed_spec = TranscodeSpec::new(changed_input, profile).unwrap();
    let third = repository
        .create(single_input_job(OpSpec::Transcode(changed_spec)))
        .await
        .unwrap();
    let third_record =
        await_terminal(&repository, third.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;
    assert_eq!(
        third_record.state,
        JobState::Succeeded,
        "{:?}",
        third_record.failure
    );
    assert_eq!(metrics.cache_hits.value(), 1);
    assert_eq!(
        metrics.cache_misses.value(),
        2,
        "a changed input hit the cache"
    );
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cache_row_whose_source_is_gone_is_removed_and_the_job_runs() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("cache-stale.mp4", 3, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/cache-stale.mp4").await;
    let repository = db.repository();
    let profile = OutputProfile::from_raw(24.0, 1200, 23, 128_000, 48_000, 0, 0).unwrap();
    let spec = TranscodeSpec::new(input, profile).unwrap();
    let first = repository
        .create(single_input_job(OpSpec::Transcode(spec.clone())))
        .await
        .unwrap();
    let config = base_config(&db, store.clone(), "worker-cache-stale");
    let metrics = config.metrics.clone();
    let handle = start(config);
    let first_record =
        await_terminal(&repository, first.id, mm_testkit::budget::MERGE_TERMINAL).await;
    assert_eq!(
        first_record.state,
        JobState::Succeeded,
        "{:?}",
        first_record.failure
    );
    let OpResult::Transcode(first_result) = first_record.result.expect("result recorded") else {
        panic!("expected a transcode result");
    };
    let MediaRef::Object(first_output) = first_result.output() else {
        panic!("expected an object output");
    };
    store
        .delete(first_output)
        .await
        .expect("cached output deletes");

    let second = repository
        .create(single_input_job(OpSpec::Transcode(spec)))
        .await
        .unwrap();
    let second_record =
        await_terminal(&repository, second.id, mm_testkit::budget::MERGE_TERMINAL).await;
    handle.shutdown().await;
    assert_eq!(
        second_record.state,
        JobState::Succeeded,
        "{:?}",
        second_record.failure
    );
    assert_eq!(metrics.cache_hits.value(), 0);
    assert_eq!(
        metrics.cache_misses.value(),
        2,
        "the stale row was not counted as a miss"
    );
    let OpResult::Transcode(second_result) = second_record.result.expect("result recorded") else {
        panic!("expected a transcode result");
    };
    let MediaRef::Object(second_output) = second_result.output() else {
        panic!("expected an object output");
    };
    assert!(store.head(second_output).await.expect("output exists") > 0);
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_transcode_node_reading_another_node_hits_the_branch_cache() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("cache-branch.mp4", 4, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/cache-branch.mp4").await;
    let repository = db.repository();
    let profile = OutputProfile::from_raw(24.0, 1200, 23, 128_000, 48_000, 0, 0).unwrap();
    let first = TranscodeSpec::new(input.clone(), profile).unwrap();
    let second = TranscodeSpec::new(
        InputSource::from(NodeRef::new("source", 0).unwrap()),
        profile,
    )
    .unwrap();
    let graph = GraphSpec::new(vec![
        GraphNode::new("source", OpSpec::Transcode(first)).unwrap(),
        GraphNode::new("encode", OpSpec::Transcode(second)).unwrap(),
    ])
    .unwrap();
    let first_job = repository
        .create(single_input_job(OpSpec::Graph(graph.clone())))
        .await
        .unwrap();
    let config = base_config(&db, store.clone(), "worker-branch-cache");
    let metrics = config.metrics.clone();
    let handle = start(config);
    let first_record = await_terminal(
        &repository,
        first_job.id,
        mm_testkit::budget::MERGE_TERMINAL,
    )
    .await;
    assert_eq!(
        first_record.state,
        JobState::Succeeded,
        "{:?}",
        first_record.failure
    );
    assert_eq!(metrics.cache_hits.value(), 0);
    assert_eq!(
        metrics.cache_misses.value(),
        2,
        "a node-backed transcode was not cached"
    );

    let second_job = repository
        .create(single_input_job(OpSpec::Graph(graph)))
        .await
        .unwrap();
    let second_record = await_terminal(
        &repository,
        second_job.id,
        mm_testkit::budget::MERGE_TERMINAL,
    )
    .await;
    handle.shutdown().await;
    assert_eq!(
        second_record.state,
        JobState::Succeeded,
        "{:?}",
        second_record.failure
    );
    assert_eq!(
        metrics.cache_hits.value(),
        2,
        "the second graph did not copy both branches"
    );
    assert_eq!(
        metrics.cache_misses.value(),
        2,
        "the second graph ran an encoder"
    );
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_grade_scope_mode_changes_the_scope_object() {
    let _slot = mm_testkit::stack_slot();
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("grade-scope.mp4", 4, 24, "mp4");
    let input = upload_input(&store, &source, "inputs/grade-scope.mp4").await;
    let repository = db.repository();
    let handle = start(base_config(&db, store.clone(), "worker-scope"));
    let mut bodies = std::collections::HashMap::new();
    for mode in [
        ScopeMode::Frame,
        ScopeMode::Waveform,
        ScopeMode::Parade,
        ScopeMode::Histogram,
        ScopeMode::Vectorscope,
    ] {
        let spec = GradeSpec::new(
            input.clone(),
            GradeAdjustments::new(0.1, 1.0, 1.0, 1.0).unwrap(),
            None,
            Some(GradeScope::new(160, 120).unwrap().with_mode(mode)),
        )
        .unwrap();
        let job = repository
            .create(single_input_job(OpSpec::Grade(spec)))
            .await
            .unwrap();
        let record = await_terminal(&repository, job.id, mm_testkit::budget::MERGE_TERMINAL).await;
        assert_eq!(record.state, JobState::Succeeded, "{:?}", record.failure);
        let OpResult::Grade(result) = record.result.expect("result recorded") else {
            panic!("expected a grade result");
        };
        let MediaRef::Object(scope) = result.scope().expect("a scope was requested") else {
            panic!("expected a scope object");
        };
        let body = store.get(scope).await.expect("scope fetches");
        let bytes = collect_body(body).await.expect("scope body collects");
        if let Some(previous) = bodies.insert(bytes, mode) {
            panic!("{mode:?} and {previous:?} produced the same scope");
        }
    }
    handle.shutdown().await;
    assert_eq!(bodies.len(), 5);
    db.dispose().await;
}

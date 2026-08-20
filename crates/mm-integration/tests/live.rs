#![allow(deprecated)]
use std::env;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures_util::TryStreamExt;
use mm_domain::{
    AudioReport, BlockCode, InputReport, JobId, MediaKind, MediaRef, ObjectBody, ObjectStore,
    ObjectStoreError, OutputKey, Verdict, VerdictCode, VideoReport, WarnCode, resolve_media_ref,
    validate_concat_inputs,
};
use mm_ffmpeg::{MediaInfo, ProbeLimits, Prober};
use mm_storage::{CredentialSource, S3ObjectStore, S3Settings};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:8333";
const DEFAULT_ACCESS_KEY: &str = "mm-test-access";
const DEFAULT_SECRET_KEY: &str = "mm-test-secret";
const BUCKET: &str = "mm-it-shared";
const RELAY_IDLE: Duration = Duration::from_secs(60);

fn env_or(name: &str, fallback: &str) -> String {
    env::var(name).unwrap_or_else(|_| fallback.to_string())
}

fn endpoint() -> String {
    env_or("MM_OBJECTSTORE_ENDPOINT", DEFAULT_ENDPOINT)
}

fn authority(endpoint: &str) -> String {
    endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))
        .expect("endpoint must be an http(s) url")
        .trim_end_matches('/')
        .to_string()
}

fn resolve(authority: &str) -> SocketAddr {
    authority
        .to_socket_addrs()
        .expect("authority must resolve")
        .next()
        .expect("authority must resolve to an address")
}

fn require_stack() {
    let endpoint = endpoint();
    let address = resolve(&authority(&endpoint));
    if TcpStream::connect_timeout(&address, Duration::from_secs(2)).is_err() {
        panic!(
            "objectstore at {endpoint} is unreachable; \
             start the compose stack with `docker compose up -d` \
             before running integration tests"
        );
    }
}

async fn test_store() -> S3ObjectStore {
    require_stack();
    let store = S3ObjectStore::connect(S3Settings {
        endpoint: Some(endpoint()),
        region: Some("us-east-1".to_string()),
        force_path_style: true,
        credentials: CredentialSource::Static {
            access_key_id: env_or("MM_OBJECTSTORE_ACCESS_KEY", DEFAULT_ACCESS_KEY),
            secret_access_key: env_or("MM_OBJECTSTORE_SECRET_KEY", DEFAULT_SECRET_KEY),
        },
    })
    .await;
    store.ensure_bucket(BUCKET).await.unwrap();
    store
}

fn fixtures_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scratch/fixtures");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn fixture(name: &str, seconds: u32, fps: u32, container: &str) -> PathBuf {
    static GENERATION: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = GENERATION.lock().unwrap();
    let path = fixtures_dir().join(name);
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
        .arg("-b:v")
        .arg("2000k")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-c:a")
        .arg("aac")
        .arg("-b:a")
        .arg("128k");
    if container == "mp4" {
        command.arg("-movflags").arg("+faststart");
    }
    let status = command
        .arg("-f")
        .arg(container)
        .arg(&path)
        .output()
        .expect("ffmpeg must be runnable to generate fixtures");
    assert!(status.status.success(), "fixture generation failed: {name}");
    path
}

fn keyed_fixture(name: &str, seconds: u32, fps: u32) -> PathBuf {
    static GENERATION: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = GENERATION.lock().unwrap();
    let path = fixtures_dir().join(name);
    if path.is_file() && path.metadata().unwrap().len() > 0 {
        return path;
    }
    let mut command = Command::new("ffmpeg");
    command.arg("-y");
    for index in 0..seconds {
        let level = (index * 255 / seconds.max(1)).min(255);
        let hex = format!("{level:02x}");
        command.arg("-f").arg("lavfi").arg("-i").arg(format!(
            "color=c=0x{hex}{hex}{hex}:size=1280x720:rate={fps}:duration=1"
        ));
    }
    let sources: String = (0..seconds).map(|index| format!("[{index}:v]")).collect();
    let status = command
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg(format!(
            "sine=frequency=440:sample_rate=44100:duration={seconds}"
        ))
        .arg("-filter_complex")
        .arg(format!("{sources}concat=n={seconds}:v=1:a=0[v]"))
        .arg("-map")
        .arg("[v]")
        .arg("-map")
        .arg(format!("{seconds}:a"))
        .arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("veryfast")
        .arg("-g")
        .arg("1")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-c:a")
        .arg("aac")
        .arg("-b:a")
        .arg("128k")
        .arg("-movflags")
        .arg("+faststart")
        .arg("-f")
        .arg("mp4")
        .arg(&path)
        .output()
        .expect("ffmpeg must be runnable to generate fixtures");
    assert!(status.status.success(), "fixture generation failed: {name}");
    path
}

const FRAME_SAMPLES: usize = 16 * 16;

fn frame_signature(input: &str, at_seconds: Option<f64>) -> Vec<u8> {
    let mut command = Command::new("ffmpeg");
    command.arg("-v").arg("quiet").arg("-nostdin");
    if let Some(seconds) = at_seconds {
        command.arg("-ss").arg(format!("{seconds:.3}"));
    }
    let output = command
        .arg("-i")
        .arg(input)
        .arg("-frames:v")
        .arg("1")
        .arg("-vf")
        .arg(
            "scale=w=640:h=360:force_original_aspect_ratio=decrease,\
             pad=w=640:h=360:x=(ow-iw)/2:y=(oh-ih)/2,setsar=1,fps=24,\
             format=yuv420p,scale=16:16",
        )
        .arg("-pix_fmt")
        .arg("gray")
        .arg("-f")
        .arg("rawvideo")
        .arg("-")
        .output()
        .expect("ffmpeg must be runnable to extract a frame");
    assert!(
        output.status.success(),
        "frame extraction failed for {input}"
    );
    assert_eq!(output.stdout.len(), FRAME_SAMPLES, "a frame is 16x16 gray");
    output.stdout
}

fn frame_difference(left: &[u8], right: &[u8]) -> u32 {
    left.iter()
        .zip(right)
        .map(|(left, right)| u32::from(left.abs_diff(*right)))
        .sum()
}

fn file_body(path: &Path) -> (ObjectBody, u64) {
    let payload = std::fs::read(path).unwrap();
    let length = payload.len() as u64;
    let chunks: Vec<Result<Bytes, ObjectStoreError>> = payload
        .chunks(64 * 1024)
        .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
        .collect();
    (Box::pin(futures_util::stream::iter(chunks)), length)
}

async fn upload_and_presign(store: &S3ObjectStore, path: &Path, artifact: &str) -> String {
    let job_id = JobId::generate();
    let key = OutputKey::new(&job_id, artifact).unwrap();
    let target = key.clone().into_object(BUCKET).unwrap();
    let (body, length) = file_body(path);
    store.put(&target, body, length).await.unwrap();
    let media = MediaRef::object_uri(&format!("s3://{BUCKET}/{key}")).unwrap();
    let now = SystemTime::now();
    resolve_media_ref(store, &media, now + Duration::from_secs(300), now)
        .await
        .unwrap()
}

async fn rewrite_heads(
    client: tokio::net::tcp::OwnedReadHalf,
    mut upstream: tokio::net::tcp::OwnedWriteHalf,
    upstream_authority: String,
) {
    let mut reader = BufReader::new(client);
    loop {
        let mut head = Vec::new();
        loop {
            let mut line = String::new();
            let read = tokio::time::timeout(RELAY_IDLE, reader.read_line(&mut line)).await;
            let Ok(Ok(count)) = read else { return };
            if count == 0 {
                return;
            }
            if line.to_ascii_lowercase().starts_with("host:") {
                line = format!("Host: {upstream_authority}\r\n");
            }
            let done = line == "\r\n";
            head.extend_from_slice(line.as_bytes());
            if done {
                break;
            }
        }
        if upstream.write_all(&head).await.is_err() {
            return;
        }
    }
}

async fn count_responses(
    mut upstream: tokio::net::tcp::OwnedReadHalf,
    mut client: tokio::net::tcp::OwnedWriteHalf,
    counter: Arc<AtomicU64>,
) {
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = tokio::time::timeout(RELAY_IDLE, upstream.read(&mut buffer)).await;
        let Ok(Ok(count)) = read else { return };
        if count == 0 {
            return;
        }
        counter.fetch_add(count as u64, Ordering::Relaxed);
        if client.write_all(&buffer[..count]).await.is_err() {
            return;
        }
    }
}

async fn counting_relay(upstream_authority: String) -> (SocketAddr, Arc<AtomicU64>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let counter = Arc::new(AtomicU64::new(0));
    let counted = counter.clone();
    tokio::spawn(async move {
        loop {
            let Ok((client, _)) = listener.accept().await else {
                return;
            };
            let upstream_authority = upstream_authority.clone();
            let counter = counted.clone();
            tokio::spawn(async move {
                let Ok(upstream) = tokio::net::TcpStream::connect(&upstream_authority).await else {
                    return;
                };
                let (client_read, client_write) = client.into_split();
                let (upstream_read, upstream_write) = upstream.into_split();
                tokio::join!(
                    rewrite_heads(client_read, upstream_write, upstream_authority),
                    count_responses(upstream_read, client_write, counter),
                );
            });
        }
    });
    (address, counter)
}

fn to_report(info: &MediaInfo) -> InputReport {
    InputReport {
        container: info.container.clone(),
        duration: info.duration,
        size_bytes: info.size_bytes,
        drm: info.drm,
        kind: MediaKind::Video,
        video: info
            .video
            .iter()
            .map(|video| VideoReport {
                codec: video.codec.clone(),
                width: video.width,
                height: video.height,
                avg_fps: video.avg_fps,
                real_fps: video.real_fps,
                rotation: video.rotation,
                sample_aspect_ratio: video.sample_aspect_ratio,
                interlaced: video.interlaced,
            })
            .collect(),
        audio: info
            .audio
            .iter()
            .map(|audio| AudioReport {
                codec: audio.codec.clone(),
                channels: audio.channels,
                sample_rate: audio.sample_rate,
                loudness_lufs: None,
            })
            .collect(),
    }
}

fn probe_limits() -> ProbeLimits {
    ProbeLimits {
        probe_size_bytes: 1024 * 1024,
        analyze_duration: Duration::from_secs(2),
        timeout: Duration::from_secs(30),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_presigned_fixture_probes_without_a_full_download() {
    let store = test_store().await;
    let path = fixture("main-30fps.mp4", 90, 30, "mp4");
    let file_size = path.metadata().unwrap().len();
    assert!(file_size > 15_000_000, "fixture too small: {file_size}");

    let url = upload_and_presign(&store, &path, "main-30fps.mp4").await;
    let upstream_authority = authority(&endpoint());
    let (relay_address, counter) = counting_relay(upstream_authority.clone()).await;
    let relayed_url = url.replace(&upstream_authority, &relay_address.to_string());
    assert_ne!(relayed_url, url);

    let info = Prober::default()
        .probe(&relayed_url, &probe_limits())
        .await
        .unwrap();

    assert!(info.container.split(',').any(|token| token == "mp4"));
    assert!((info.duration.as_secs_f64() - 90.0).abs() < 0.6);
    assert_eq!(info.video.len(), 1);
    assert_eq!(info.video[0].codec, "h264");
    assert_eq!(info.video[0].width, 1280);
    assert_eq!(info.video[0].height, 720);
    assert!((info.video[0].avg_fps.unwrap() - 30.0).abs() < 0.1);
    assert_eq!(info.audio.len(), 1);
    assert_eq!(info.audio[0].codec, "aac");
    assert_eq!(info.audio[0].channels, 1);
    assert_eq!(info.audio[0].sample_rate, Some(44_100));

    let transferred = counter.load(Ordering::Relaxed);
    assert!(transferred > 0);
    assert!(
        transferred < file_size / 2,
        "probe transferred {transferred} of {file_size} bytes"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_foreign_container_blocks_live() {
    let store = test_store().await;
    let good = fixture("main-30fps.mp4", 90, 30, "mp4");
    let bad = fixture("bad-container.mkv", 3, 30, "matroska");

    let good_url = upload_and_presign(&store, &good, "main-30fps.mp4").await;
    let bad_url = upload_and_presign(&store, &bad, "bad-container.mkv").await;

    let prober = Prober::default();
    let bad_info = prober.probe(&bad_url, &probe_limits()).await.unwrap();
    let good_info = prober.probe(&good_url, &probe_limits()).await.unwrap();

    let verdicts = validate_concat_inputs(&[to_report(&bad_info), to_report(&good_info)]);
    assert!(verdicts.contains(&Verdict {
        input: Some(0),
        code: VerdictCode::Block(BlockCode::UnsupportedContainer),
    }));
}

#[tokio::test(flavor = "multi_thread")]
async fn frame_rate_drift_warns_live() {
    let store = test_store().await;
    let main = fixture("main-30fps.mp4", 90, 30, "mp4");
    let slow = fixture("alt-25fps.mp4", 6, 25, "mp4");

    let main_url = upload_and_presign(&store, &main, "main-30fps.mp4").await;
    let slow_url = upload_and_presign(&store, &slow, "alt-25fps.mp4").await;

    let prober = Prober::default();
    let main_info = prober.probe(&main_url, &probe_limits()).await.unwrap();
    let slow_info = prober.probe(&slow_url, &probe_limits()).await.unwrap();

    let verdicts = validate_concat_inputs(&[to_report(&main_info), to_report(&slow_info)]);
    assert!(verdicts.contains(&Verdict {
        input: None,
        code: VerdictCode::Warn(WarnCode::FpsDrift),
    }));
    assert!(
        verdicts
            .iter()
            .all(|verdict| !matches!(verdict.code, VerdictCode::Block(_))),
        "unexpected blocks: {verdicts:?}"
    );
}

const ADMIN_URL_DEFAULT: &str = "postgres://mm:mm@127.0.0.1:5433/mm";

fn admin_url() -> String {
    env_or("MM_DATABASE_URL", ADMIN_URL_DEFAULT)
}

async fn fresh_pool() -> (sqlx::PgPool, sqlx::PgPool, String, String) {
    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url())
        .await
        .expect("admin connection");
    let name = format!("mm_test_{}", uuid::Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&admin)
        .await
        .expect("create test database");
    let url = format!(
        "{}/{name}",
        admin_url()
            .rsplit_once('/')
            .expect("database url has a path")
            .0
    );
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await
        .expect("test database connection");
    mm_jobstore::MIGRATOR
        .run(&pool)
        .await
        .expect("migrations apply");
    (admin, pool, url, name)
}

async fn drop_db(admin: &sqlx::PgPool, pool: sqlx::PgPool, name: &str) {
    pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE {name} WITH (FORCE)"
    )))
    .execute(admin)
    .await
    .expect("drop test database");
}

async fn upload_object(store: &S3ObjectStore, path: &Path, key: &str) -> MediaRef {
    mm_testkit::upload_object(store, path, BUCKET, key).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_created_job_merges_end_to_end_through_server_and_worker() {
    let _slot = mm_testkit::stack_slot();
    let (admin, pool, url, name) = fresh_pool().await;
    let store = Arc::new(test_store().await);
    let repository: Arc<dyn mm_domain::JobRepository> =
        Arc::new(mm_jobstore::PgJobRepository::new(pool.clone()));

    let first = fixture("e2e-a-6s.mp4", 6, 30, "mp4");
    let second = fixture("e2e-b-4s.mp4", 4, 30, "mp4");
    let inputs = vec![
        upload_object(&store, &first, "inputs/e2e-a-6s.mp4").await,
        upload_object(&store, &second, "inputs/e2e-b-4s.mp4").await,
    ];

    let server_config = mm_server::ServerConfig::new(
        repository.clone(),
        store.clone(),
        BUCKET.to_string(),
        Prober::default(),
    )
    .expect("valid server config");
    let server = mm_server::start(server_config)
        .await
        .expect("server starts");

    let staging = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scratch/e2e-staging");
    let mut worker_config = mm_worker::WorkerConfig::new(
        repository.clone(),
        store.clone(),
        BUCKET.to_string(),
        std::str::FromStr::from_str("worker-e2e").unwrap(),
        staging,
    );
    worker_config.reconcile_interval = Duration::from_secs(30);
    worker_config.notifications = Some(Arc::new(
        mm_jobstore::PgJobListener::connect(&url)
            .await
            .expect("listener connects"),
    ));
    let worker = mm_worker::start(worker_config);
    tokio::time::sleep(Duration::from_millis(400)).await;

    let channel =
        tonic::transport::Endpoint::from_shared(format!("http://{}", server.local_addr()))
            .expect("valid endpoint")
            .connect_timeout(Duration::from_secs(2))
            .timeout(mm_testkit::budget::RPC)
            .connect()
            .await
            .expect("dial announced address");
    let mut client = mm_proto::v1::media_job_service_client::MediaJobServiceClient::new(channel);

    let spec = mm_proto::v1::OpSpec::from(&mm_domain::OpSpec::Concat(
        mm_domain::ConcatSpec::new(inputs, None).expect("valid concat spec"),
    ));
    let created_at = std::time::Instant::now();
    let created = client
        .create_job(mm_proto::v1::CreateJobRequest {
            user_id: "owner-e2e".to_string(),
            idempotency_key: "req-e2e".to_string(),
            spec: Some(spec),
            callback: None,
        })
        .await
        .expect("create succeeds")
        .into_inner();
    assert!(created.created);
    let job_id = created.job_id;

    let mut left_pending_within = None;
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let final_job = loop {
        let job = client
            .get_job(mm_proto::v1::GetJobRequest {
                user_id: "owner-e2e".to_string(),
                job_id: job_id.clone(),
            })
            .await
            .expect("get succeeds")
            .into_inner()
            .job
            .expect("job present");
        if left_pending_within.is_none() && job.state() != mm_proto::v1::JobState::Pending {
            left_pending_within = Some(created_at.elapsed());
        }
        match job.state() {
            mm_proto::v1::JobState::Succeeded => break job,
            mm_proto::v1::JobState::Failed | mm_proto::v1::JobState::Cancelled => {
                panic!("job ended in {:?}", job.state());
            }
            _ => {}
        }
        assert!(std::time::Instant::now() < deadline, "job never succeeded");
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    let picked_up = left_pending_within.expect("job left pending");
    assert!(
        picked_up < mm_testkit::budget::PICKUP,
        "pickup took {picked_up:?}, outside the notify budget and \
         approaching the half-minute tick"
    );
    assert_eq!(final_job.progress_percent, 100);

    let result = mm_domain::OpResult::try_from(final_job.result.expect("result present"))
        .expect("result decodes");
    let mm_domain::OpResult::Concat(timed) = result else {
        panic!("expected a concat result");
    };
    assert!(
        (timed.duration_seconds() - 10.0).abs() < 0.6,
        "duration {}",
        timed.duration_seconds()
    );
    let MediaRef::Object(object) = timed.output().clone() else {
        panic!("expected an object output");
    };
    assert_eq!(object.key(), format!("jobs/{job_id}/output.mp4"));
    assert!(store.head(&object).await.expect("output exists") > 0);

    let output_url = resolve_media_ref(
        store.as_ref(),
        &MediaRef::Object(object),
        SystemTime::now() + Duration::from_secs(300),
        SystemTime::now(),
    )
    .await
    .expect("output presigns");
    let info = Prober::default()
        .probe(&output_url, &probe_limits())
        .await
        .expect("output probes");
    assert!(info.container.split(',').any(|token| token == "mp4"));
    assert!((info.duration.as_secs_f64() - 10.0).abs() < 0.6);
    assert_eq!(info.video[0].codec, "h264");
    assert_eq!(info.audio[0].codec, "aac");

    worker.shutdown().await;
    server.shutdown().await;
    drop_db(&admin, pool, &name).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_merge_with_a_callback_delivers_one_decoded_queue_message() {
    let _slot = mm_testkit::stack_slot();
    let (admin, pool, url, name) = fresh_pool().await;
    let store = Arc::new(test_store().await);
    let repository: Arc<dyn mm_domain::JobRepository> =
        Arc::new(mm_jobstore::PgJobRepository::new(pool.clone()));

    let queue_endpoint = env_or("MM_QUEUE_ENDPOINT", "http://127.0.0.1:9324");
    let address = resolve(&authority(&queue_endpoint));
    assert!(
        TcpStream::connect_timeout(&address, Duration::from_secs(2)).is_ok(),
        "queue at {queue_endpoint} is unreachable; \
         start the compose stack with `docker compose up -d`"
    );
    let publisher = Arc::new(
        mm_storage::SqsCallbackPublisher::connect(mm_storage::SqsSettings {
            endpoint: Some(queue_endpoint),
            region: Some("local".to_string()),
            credentials: CredentialSource::Static {
                access_key_id: "queue-access".to_string(),
                secret_access_key: "queue-secret".to_string(),
            },
        })
        .await,
    );
    let queue = publisher
        .ensure_queue(&format!("mm-it-cb-{}", uuid::Uuid::now_v7().simple()))
        .await
        .expect("queue exists");

    let first = fixture("e2e-a-6s.mp4", 6, 30, "mp4");
    let second = fixture("e2e-b-4s.mp4", 4, 30, "mp4");
    let inputs = vec![
        upload_object(&store, &first, "inputs/cb-a-6s.mp4").await,
        upload_object(&store, &second, "inputs/cb-b-4s.mp4").await,
    ];

    let server_config = mm_server::ServerConfig::new(
        repository.clone(),
        store.clone(),
        BUCKET.to_string(),
        Prober::default(),
    )
    .expect("valid server config");
    let server = mm_server::start(server_config)
        .await
        .expect("server starts");

    let staging = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scratch/e2e-staging");
    let mut worker_config = mm_worker::WorkerConfig::new(
        repository.clone(),
        store.clone(),
        BUCKET.to_string(),
        std::str::FromStr::from_str("worker-cb").unwrap(),
        staging,
    );
    worker_config.notifications = Some(Arc::new(
        mm_jobstore::PgJobListener::connect(&url)
            .await
            .expect("listener connects"),
    ));
    worker_config.callbacks = Some(publisher.clone());
    let worker = mm_worker::start(worker_config);
    tokio::time::sleep(Duration::from_millis(400)).await;

    let channel =
        tonic::transport::Endpoint::from_shared(format!("http://{}", server.local_addr()))
            .expect("valid endpoint")
            .connect_timeout(Duration::from_secs(2))
            .timeout(mm_testkit::budget::RPC)
            .connect()
            .await
            .expect("dial announced address");
    let mut client = mm_proto::v1::media_job_service_client::MediaJobServiceClient::new(channel);

    let spec = mm_proto::v1::OpSpec::from(&mm_domain::OpSpec::Concat(
        mm_domain::ConcatSpec::new(inputs, None).expect("valid concat spec"),
    ));
    let created = client
        .create_job(mm_proto::v1::CreateJobRequest {
            user_id: "owner-cb".to_string(),
            idempotency_key: "req-cb".to_string(),
            spec: Some(spec),
            callback: Some(mm_proto::v1::Callback {
                queue_url: queue.as_str().to_string(),
            }),
        })
        .await
        .expect("create succeeds")
        .into_inner();
    let job_id = created.job_id;

    let settle = std::time::Instant::now() + Duration::from_secs(120);
    let bodies = loop {
        let received = tokio::time::timeout(
            Duration::from_secs(30),
            publisher.receive(&queue, Duration::from_secs(2)),
        )
        .await
        .expect("receive answers in time")
        .expect("receive succeeds");
        if !received.is_empty() {
            break received;
        }
        assert!(
            std::time::Instant::now() < settle,
            "no callback message arrived"
        );
    };
    assert_eq!(bodies.len(), 1, "exactly one callback message");
    let leftover = tokio::time::timeout(
        Duration::from_secs(30),
        publisher.receive(&queue, Duration::from_secs(1)),
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
    let callback = <mm_proto::v1::JobCallback as prost::Message>::decode(decoded.as_slice())
        .expect("message is a job callback");
    let outcome = callback.outcome.expect("the callback carries the outcome");
    let stitch = callback
        .stitch
        .expect("the callback carries the reference half");
    assert_eq!(outcome.job_id, job_id);
    assert_eq!(outcome.request_id, "req-cb");
    assert_eq!(outcome.state, mm_proto::v1::JobState::Succeeded as i32);
    assert_eq!(outcome.error_code, "");
    assert!(outcome.file_errors.is_empty());
    assert_eq!(stitch.status, mm_proto::v1::StitchStatus::Completed as i32);
    assert_eq!(stitch.request_id, "req-cb");
    assert_eq!(stitch.error_code, None);
    assert!(stitch.file_errors.is_empty());
    let result =
        mm_domain::OpResult::try_from(outcome.result.expect("result present")).expect("decodes");
    let mm_domain::OpResult::Concat(timed) = result else {
        panic!("expected a concat result");
    };
    let MediaRef::Object(object) = timed.output().clone() else {
        panic!("expected an object output");
    };
    assert_eq!(object.key(), format!("jobs/{job_id}/output.mp4"));
    assert!(store.head(&object).await.expect("output exists") > 0);

    publisher.delete_queue(&queue).await.expect("queue removed");
    worker.shutdown().await;
    server.shutdown().await;
    drop_db(&admin, pool, &name).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_watch_stream_observes_a_live_merge_to_completion() {
    let _slot = mm_testkit::stack_slot();
    let (admin, pool, url, name) = fresh_pool().await;
    let store = Arc::new(test_store().await);
    let repository: Arc<dyn mm_domain::JobRepository> =
        Arc::new(mm_jobstore::PgJobRepository::new(pool.clone()));

    let first = fixture("watch-a-15s.mp4", 15, 30, "mp4");
    let second = fixture("watch-b-15s.mp4", 15, 30, "mp4");
    let inputs = vec![
        upload_object(&store, &first, "inputs/watch-a-15s.mp4").await,
        upload_object(&store, &second, "inputs/watch-b-15s.mp4").await,
    ];

    let mut server_config = mm_server::ServerConfig::new(
        repository.clone(),
        store.clone(),
        BUCKET.to_string(),
        Prober::default(),
    )
    .expect("valid server config");
    server_config.watch_poll_interval = Duration::from_secs(600);
    server_config.job_events = Some(Arc::new(
        mm_jobstore::PgJobEventListener::connect(&url)
            .await
            .expect("events listener connects"),
    ));
    let server = mm_server::start(server_config)
        .await
        .expect("server starts");

    let channel =
        tonic::transport::Endpoint::from_shared(format!("http://{}", server.local_addr()))
            .expect("valid endpoint")
            .connect_timeout(Duration::from_secs(2))
            .connect()
            .await
            .expect("dial announced address");
    let mut client = mm_proto::v1::media_job_service_client::MediaJobServiceClient::new(channel);

    let spec = mm_proto::v1::OpSpec::from(&mm_domain::OpSpec::Concat(
        mm_domain::ConcatSpec::new(inputs, None).expect("valid concat spec"),
    ));
    let created = client
        .create_job(mm_proto::v1::CreateJobRequest {
            user_id: "owner-watch".to_string(),
            idempotency_key: "req-watch".to_string(),
            spec: Some(spec),
            callback: None,
        })
        .await
        .expect("create succeeds")
        .into_inner();

    let mut stream = client
        .watch_job(mm_proto::v1::WatchJobRequest {
            user_id: "owner-watch".to_string(),
            job_id: created.job_id.clone(),
        })
        .await
        .expect("watch starts")
        .into_inner();

    let staging = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scratch/e2e-staging");
    let mut worker_config = mm_worker::WorkerConfig::new(
        repository.clone(),
        store.clone(),
        BUCKET.to_string(),
        std::str::FromStr::from_str("worker-watch").unwrap(),
        staging,
    );
    worker_config.reconcile_interval = Duration::from_secs(30);
    worker_config.progress_interval = Duration::from_millis(500);
    worker_config.notifications = Some(Arc::new(
        mm_jobstore::PgJobListener::connect(&url)
            .await
            .expect("listener connects"),
    ));
    let worker = mm_worker::start(worker_config);

    let mut kinds = Vec::new();
    loop {
        let message = tokio::time::timeout(Duration::from_secs(60), stream.message())
            .await
            .expect("an event arrives within a minute")
            .expect("stream stays healthy");
        match message {
            Some(response) => kinds.push(
                response
                    .event
                    .expect("event present")
                    .kind
                    .expect("kind present"),
            ),
            None => break,
        }
    }

    use mm_proto::v1::job_event::Kind;
    let states: Vec<i32> = kinds
        .iter()
        .filter_map(|kind| match kind {
            Kind::State(state) => Some(*state),
            Kind::ProgressPercent(_) => None,
        })
        .collect();
    assert_eq!(
        states,
        vec![
            mm_proto::v1::JobState::Pending as i32,
            mm_proto::v1::JobState::Running as i32,
            mm_proto::v1::JobState::Succeeded as i32,
        ]
    );
    assert_eq!(
        kinds.first(),
        Some(&Kind::State(mm_proto::v1::JobState::Pending as i32))
    );
    assert_eq!(
        kinds.last(),
        Some(&Kind::State(mm_proto::v1::JobState::Succeeded as i32))
    );
    let progress: Vec<u32> = kinds
        .iter()
        .filter_map(|kind| match kind {
            Kind::ProgressPercent(value) => Some(*value),
            Kind::State(_) => None,
        })
        .collect();
    assert!(
        progress.windows(2).all(|pair| pair[0] <= pair[1]),
        "progress regressed: {progress:?}"
    );
    let distinct: std::collections::BTreeSet<u32> = progress.iter().copied().collect();
    assert!(
        distinct.len() >= 3,
        "fewer than three distinct progress values: {progress:?}"
    );
    assert_eq!(progress.last(), Some(&100));

    worker.shutdown().await;
    server.shutdown().await;
    drop_db(&admin, pool, &name).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_probe_job_returns_stream_metadata_through_the_api() {
    let (admin, pool, url, name) = fresh_pool().await;
    let store = Arc::new(test_store().await);
    let repository: Arc<dyn mm_domain::JobRepository> =
        Arc::new(mm_jobstore::PgJobRepository::new(pool.clone()));

    let source = fixture("probe-e2e-5s.mp4", 5, 30, "mp4");
    let input = upload_object(&store, &source, "inputs/probe-e2e-5s.mp4").await;

    let server_config = mm_server::ServerConfig::new(
        repository.clone(),
        store.clone(),
        BUCKET.to_string(),
        Prober::default(),
    )
    .expect("valid server config");
    let server = mm_server::start(server_config)
        .await
        .expect("server starts");

    let staging = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scratch/e2e-staging");
    let mut worker_config = mm_worker::WorkerConfig::new(
        repository.clone(),
        store.clone(),
        BUCKET.to_string(),
        std::str::FromStr::from_str("worker-probe-e2e").unwrap(),
        staging,
    );
    worker_config.reconcile_interval = Duration::from_secs(30);
    worker_config.notifications = Some(Arc::new(
        mm_jobstore::PgJobListener::connect(&url)
            .await
            .expect("listener connects"),
    ));
    let worker = mm_worker::start(worker_config);
    tokio::time::sleep(Duration::from_millis(400)).await;

    let channel =
        tonic::transport::Endpoint::from_shared(format!("http://{}", server.local_addr()))
            .expect("valid endpoint")
            .connect_timeout(Duration::from_secs(2))
            .timeout(mm_testkit::budget::RPC)
            .connect()
            .await
            .expect("dial announced address");
    let mut client = mm_proto::v1::media_job_service_client::MediaJobServiceClient::new(channel);

    let spec = mm_proto::v1::OpSpec::from(&mm_domain::OpSpec::Probe(
        mm_domain::ProbeSpec::new(input.clone()).expect("a bare reference cuts nothing"),
    ));
    let created = client
        .create_job(mm_proto::v1::CreateJobRequest {
            user_id: "owner-probe".to_string(),
            idempotency_key: "req-probe".to_string(),
            spec: Some(spec),
            callback: None,
        })
        .await
        .expect("create succeeds")
        .into_inner();

    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let final_job = loop {
        let job = client
            .get_job(mm_proto::v1::GetJobRequest {
                user_id: "owner-probe".to_string(),
                job_id: created.job_id.clone(),
            })
            .await
            .expect("get succeeds")
            .into_inner()
            .job
            .expect("job present");
        match job.state() {
            mm_proto::v1::JobState::Succeeded => break job,
            mm_proto::v1::JobState::Failed | mm_proto::v1::JobState::Cancelled => {
                panic!("probe job ended in {:?}", job.state());
            }
            _ => {}
        }
        assert!(
            std::time::Instant::now() < deadline,
            "probe job never succeeded"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    let result = mm_domain::OpResult::try_from(final_job.result.expect("result present"))
        .expect("result decodes");
    let mm_domain::OpResult::Probe(probe) = result else {
        panic!("expected a probe result");
    };
    assert!(
        (probe.duration_seconds() - 5.0).abs() < 0.5,
        "duration {}",
        probe.duration_seconds()
    );
    assert_eq!(probe.video_streams(), 1);
    assert_eq!(probe.audio_streams(), 1);
    assert!(probe.container().split(',').any(|token| token == "mp4"));

    worker.shutdown().await;
    server.shutdown().await;
    drop_db(&admin, pool, &name).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_transcode_job_streams_progress_and_lands_a_verified_output() {
    let _slot = mm_testkit::stack_slot();
    let (admin, pool, url, name) = fresh_pool().await;
    let store = Arc::new(test_store().await);
    let repository: Arc<dyn mm_domain::JobRepository> =
        Arc::new(mm_jobstore::PgJobRepository::new(pool.clone()));

    let source = fixture("transcode-e2e-30s.mp4", 30, 30, "mp4");
    let input = upload_object(&store, &source, "inputs/transcode-e2e-30s.mp4").await;

    let mut server_config = mm_server::ServerConfig::new(
        repository.clone(),
        store.clone(),
        BUCKET.to_string(),
        Prober::default(),
    )
    .expect("valid server config");
    server_config.watch_poll_interval = Duration::from_secs(600);
    server_config.job_events = Some(Arc::new(
        mm_jobstore::PgJobEventListener::connect(&url)
            .await
            .expect("events listener connects"),
    ));
    let server = mm_server::start(server_config)
        .await
        .expect("server starts");

    let channel =
        tonic::transport::Endpoint::from_shared(format!("http://{}", server.local_addr()))
            .expect("valid endpoint")
            .connect_timeout(Duration::from_secs(2))
            .connect()
            .await
            .expect("dial announced address");
    let mut client = mm_proto::v1::media_job_service_client::MediaJobServiceClient::new(channel);

    let profile =
        mm_domain::OutputProfile::from_raw(24.0, 0, 0, 0, 48_000, 640, 360).expect("valid profile");
    let spec = mm_proto::v1::OpSpec::from(&mm_domain::OpSpec::Transcode(
        mm_domain::TranscodeSpec::new(input.clone(), profile).expect("valid transcode spec"),
    ));
    let created = client
        .create_job(mm_proto::v1::CreateJobRequest {
            user_id: "owner-transcode".to_string(),
            idempotency_key: "req-transcode".to_string(),
            spec: Some(spec),
            callback: None,
        })
        .await
        .expect("create succeeds")
        .into_inner();

    let mut stream = client
        .watch_job(mm_proto::v1::WatchJobRequest {
            user_id: "owner-transcode".to_string(),
            job_id: created.job_id.clone(),
        })
        .await
        .expect("watch starts")
        .into_inner();

    let staging = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scratch/e2e-staging");
    let mut worker_config = mm_worker::WorkerConfig::new(
        repository.clone(),
        store.clone(),
        BUCKET.to_string(),
        std::str::FromStr::from_str("worker-transcode-e2e").unwrap(),
        staging,
    );
    worker_config.reconcile_interval = Duration::from_secs(30);
    worker_config.progress_interval = Duration::from_millis(500);
    worker_config.notifications = Some(Arc::new(
        mm_jobstore::PgJobListener::connect(&url)
            .await
            .expect("listener connects"),
    ));
    let worker = mm_worker::start(worker_config);

    let mut kinds = Vec::new();
    loop {
        let message = tokio::time::timeout(Duration::from_secs(60), stream.message())
            .await
            .expect("an event arrives within a minute")
            .expect("stream stays healthy");
        match message {
            Some(response) => kinds.push(
                response
                    .event
                    .expect("event present")
                    .kind
                    .expect("kind present"),
            ),
            None => break,
        }
    }

    use mm_proto::v1::job_event::Kind;
    let states: Vec<i32> = kinds
        .iter()
        .filter_map(|kind| match kind {
            Kind::State(state) => Some(*state),
            Kind::ProgressPercent(_) => None,
        })
        .collect();
    assert_eq!(
        states,
        vec![
            mm_proto::v1::JobState::Pending as i32,
            mm_proto::v1::JobState::Running as i32,
            mm_proto::v1::JobState::Succeeded as i32,
        ]
    );
    let progress: Vec<u32> = kinds
        .iter()
        .filter_map(|kind| match kind {
            Kind::ProgressPercent(value) => Some(*value),
            Kind::State(_) => None,
        })
        .collect();
    assert!(
        progress.windows(2).all(|pair| pair[0] <= pair[1]),
        "progress regressed: {progress:?}"
    );
    let distinct: std::collections::BTreeSet<u32> = progress.iter().copied().collect();
    assert!(
        distinct.len() >= 3,
        "fewer than three distinct progress values: {progress:?}"
    );
    assert_eq!(progress.last(), Some(&100));

    let final_job = client
        .get_job(mm_proto::v1::GetJobRequest {
            user_id: "owner-transcode".to_string(),
            job_id: created.job_id.clone(),
        })
        .await
        .expect("get succeeds")
        .into_inner()
        .job
        .expect("job present");
    assert_eq!(final_job.state(), mm_proto::v1::JobState::Succeeded);
    let result = mm_domain::OpResult::try_from(final_job.result.expect("result present"))
        .expect("result decodes");
    let mm_domain::OpResult::Transcode(transcoded) = result else {
        panic!("expected a transcode result");
    };
    let MediaRef::Object(object) = transcoded.output().clone() else {
        panic!("expected an object output");
    };
    assert_eq!(object.key(), format!("jobs/{}/output.mp4", created.job_id));
    assert!(store.head(&object).await.expect("output exists") > 0);

    let output_url = resolve_media_ref(
        store.as_ref(),
        &MediaRef::Object(object),
        SystemTime::now() + Duration::from_secs(300),
        SystemTime::now(),
    )
    .await
    .expect("output presigns");
    let info = Prober::default()
        .probe(&output_url, &probe_limits())
        .await
        .expect("output probes");
    assert!(info.container.split(',').any(|token| token == "mp4"));
    assert!((info.duration.as_secs_f64() - 30.0).abs() < 0.6);
    assert_eq!(info.video[0].codec, "h264");
    assert_eq!(info.video[0].width, 640);
    assert_eq!(info.video[0].height, 360);
    assert!((info.video[0].avg_fps.unwrap() - 24.0).abs() < 0.1);
    assert_eq!(info.audio[0].codec, "aac");
    assert_eq!(info.audio[0].sample_rate, Some(48_000));

    worker.shutdown().await;
    server.shutdown().await;
    drop_db(&admin, pool, &name).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_trim_job_reaches_success_through_server_and_worker() {
    let _slot = mm_testkit::stack_slot();
    let (admin, pool, url, name) = fresh_pool().await;
    let store = Arc::new(test_store().await);
    let repository: Arc<dyn mm_domain::JobRepository> =
        Arc::new(mm_jobstore::PgJobRepository::new(pool.clone()));

    let source = fixture("trim-e2e-6s.mp4", 6, 30, "mp4");
    let input = upload_object(&store, &source, "inputs/trim-e2e-6s.mp4").await;

    let mut server_config = mm_server::ServerConfig::new(
        repository.clone(),
        store.clone(),
        BUCKET.to_string(),
        Prober::default(),
    )
    .expect("valid server config");
    server_config.job_events = Some(Arc::new(
        mm_jobstore::PgJobEventListener::connect(&url)
            .await
            .expect("events listener connects"),
    ));
    let server = mm_server::start(server_config)
        .await
        .expect("server starts");

    let channel =
        tonic::transport::Endpoint::from_shared(format!("http://{}", server.local_addr()))
            .expect("valid endpoint")
            .connect_timeout(Duration::from_secs(2))
            .connect()
            .await
            .expect("dial announced address");
    let mut client = mm_proto::v1::media_job_service_client::MediaJobServiceClient::new(channel);

    let spec = mm_proto::v1::OpSpec::from(&mm_domain::OpSpec::Trim(
        mm_domain::TrimSpec::new(input, 1.0, 2.0).expect("valid trim spec"),
    ));
    let created = client
        .create_job(mm_proto::v1::CreateJobRequest {
            user_id: "owner-trim".to_string(),
            idempotency_key: "req-trim".to_string(),
            spec: Some(spec),
            callback: None,
        })
        .await
        .expect("create succeeds")
        .into_inner();
    assert!(created.created);

    let staging = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scratch/e2e-trim-staging");
    let mut worker_config = mm_worker::WorkerConfig::new(
        repository.clone(),
        store.clone(),
        BUCKET.to_string(),
        std::str::FromStr::from_str("worker-trim-e2e").unwrap(),
        staging,
    );
    worker_config.reconcile_interval = Duration::from_secs(30);
    worker_config.notifications = Some(Arc::new(
        mm_jobstore::PgJobListener::connect(&url)
            .await
            .expect("listener connects"),
    ));
    let worker = mm_worker::start(worker_config);

    let final_job = loop {
        let job = client
            .get_job(mm_proto::v1::GetJobRequest {
                user_id: "owner-trim".to_string(),
                job_id: created.job_id.clone(),
            })
            .await
            .expect("get succeeds")
            .into_inner()
            .job
            .expect("job present");
        if job.state() == mm_proto::v1::JobState::Succeeded
            || job.state() == mm_proto::v1::JobState::Failed
            || job.state() == mm_proto::v1::JobState::Cancelled
        {
            break job;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(
        final_job.state(),
        mm_proto::v1::JobState::Succeeded,
        "trim job did not succeed"
    );
    let result = mm_domain::OpResult::try_from(final_job.result.expect("result present"))
        .expect("result decodes");
    let mm_domain::OpResult::Trim(trimmed) = result else {
        panic!("expected a trim result");
    };
    let MediaRef::Object(object) = trimmed.output().clone() else {
        panic!("expected an object output");
    };
    assert_eq!(object.key(), format!("jobs/{}/output.mp4", created.job_id));
    assert!(store.head(&object).await.expect("output exists") > 0);

    let output_url = resolve_media_ref(
        store.as_ref(),
        &MediaRef::Object(object),
        SystemTime::now() + Duration::from_secs(300),
        SystemTime::now(),
    )
    .await
    .expect("output presigns");
    let info = Prober::default()
        .probe(&output_url, &probe_limits())
        .await
        .expect("output probes");
    assert!(
        (info.duration.as_secs_f64() - 2.0).abs() < 0.4,
        "the trimmed output is {}s long",
        info.duration.as_secs_f64()
    );
    assert!(info.container.split(',').any(|token| token == "mp4"));

    worker.shutdown().await;
    server.shutdown().await;
    drop_db(&admin, pool, &name).await;
}

async fn transcode_through_server_and_worker(
    owner: &str,
    key: &str,
    input: mm_domain::InputSource,
) -> String {
    let _slot = mm_testkit::stack_slot();
    let (admin, pool, url, name) = fresh_pool().await;
    let store = Arc::new(test_store().await);
    let repository: Arc<dyn mm_domain::JobRepository> =
        Arc::new(mm_jobstore::PgJobRepository::new(pool.clone()));

    let mut server_config = mm_server::ServerConfig::new(
        repository.clone(),
        store.clone(),
        BUCKET.to_string(),
        Prober::default(),
    )
    .expect("valid server config");
    server_config.job_events = Some(Arc::new(
        mm_jobstore::PgJobEventListener::connect(&url)
            .await
            .expect("events listener connects"),
    ));
    let server = mm_server::start(server_config)
        .await
        .expect("server starts");

    let channel =
        tonic::transport::Endpoint::from_shared(format!("http://{}", server.local_addr()))
            .expect("valid endpoint")
            .connect_timeout(Duration::from_secs(2))
            .connect()
            .await
            .expect("dial announced address");
    let mut client = mm_proto::v1::media_job_service_client::MediaJobServiceClient::new(channel);

    let profile =
        mm_domain::OutputProfile::from_raw(24.0, 0, 0, 0, 48_000, 640, 360).expect("valid profile");
    let spec = mm_proto::v1::OpSpec::from(&mm_domain::OpSpec::Transcode(
        mm_domain::TranscodeSpec::new(input, profile).expect("valid transcode spec"),
    ));
    let created = client
        .create_job(mm_proto::v1::CreateJobRequest {
            user_id: owner.to_string(),
            idempotency_key: key.to_string(),
            spec: Some(spec),
            callback: None,
        })
        .await
        .expect("create succeeds")
        .into_inner();

    let staging = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scratch/e2e-cut-staging");
    let mut worker_config = mm_worker::WorkerConfig::new(
        repository.clone(),
        store.clone(),
        BUCKET.to_string(),
        std::str::FromStr::from_str("worker-cut-e2e").unwrap(),
        staging,
    );
    worker_config.reconcile_interval = Duration::from_secs(30);
    worker_config.notifications = Some(Arc::new(
        mm_jobstore::PgJobListener::connect(&url)
            .await
            .expect("listener connects"),
    ));
    let worker = mm_worker::start(worker_config);

    let final_job = loop {
        let job = client
            .get_job(mm_proto::v1::GetJobRequest {
                user_id: owner.to_string(),
                job_id: created.job_id.clone(),
            })
            .await
            .expect("get succeeds")
            .into_inner()
            .job
            .expect("job present");
        if job.state() == mm_proto::v1::JobState::Succeeded
            || job.state() == mm_proto::v1::JobState::Failed
            || job.state() == mm_proto::v1::JobState::Cancelled
        {
            break job;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(
        final_job.state(),
        mm_proto::v1::JobState::Succeeded,
        "the transcode of a cut source did not succeed"
    );
    let result = mm_domain::OpResult::try_from(final_job.result.expect("result present"))
        .expect("result decodes");
    let mm_domain::OpResult::Transcode(transcoded) = result else {
        panic!("expected a transcode result");
    };
    let MediaRef::Object(object) = transcoded.output().clone() else {
        panic!("expected an object output");
    };
    assert_eq!(object.key(), format!("jobs/{}/output.mp4", created.job_id));
    assert!(store.head(&object).await.expect("output exists") > 0);

    let output_url = resolve_media_ref(
        store.as_ref(),
        &MediaRef::Object(object),
        SystemTime::now() + Duration::from_secs(300),
        SystemTime::now(),
    )
    .await
    .expect("output presigns");

    worker.shutdown().await;
    server.shutdown().await;
    drop_db(&admin, pool, &name).await;
    output_url
}

#[tokio::test(flavor = "multi_thread")]
async fn a_transcode_at_half_speed_lands_an_output_twice_as_long() {
    let _slot = mm_testkit::stack_slot();
    let store = Arc::new(test_store().await);
    let source = keyed_fixture("cuts-e2e-speed-6s.mp4", 6, 30);
    let media = upload_object(&store, &source, "inputs/cuts-e2e-speed-6s.mp4").await;

    let input = mm_domain::InputSource::from(
        mm_domain::MediaSource::new(media, None, Some(0.5), None).expect("valid source"),
    );
    let output_url = transcode_through_server_and_worker("owner-speed", "req-speed", input).await;

    let info = Prober::default()
        .probe(&output_url, &probe_limits())
        .await
        .expect("output probes");
    assert!(
        (info.duration.as_secs_f64() - 12.0).abs() < 1.0,
        "half speed produced {}s from a 6s source",
        info.duration.as_secs_f64()
    );
    assert_eq!(info.video.len(), 1);
    assert_eq!(info.audio.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_trimmed_transcode_start_lands_where_the_range_asked() {
    let _slot = mm_testkit::stack_slot();
    let store = Arc::new(test_store().await);
    let source = keyed_fixture("cuts-e2e-trim-6s.mp4", 6, 30);
    let media = upload_object(&store, &source, "inputs/cuts-e2e-trim-6s.mp4").await;

    let input = mm_domain::InputSource::from(
        mm_domain::MediaSource::new(
            media,
            Some(mm_domain::TrimRange::new(1.0, 2.0).expect("valid range")),
            None,
            None,
        )
        .expect("valid source"),
    );
    let output_url =
        transcode_through_server_and_worker("owner-trimmed", "req-trimmed", input).await;

    let info = Prober::default()
        .probe(&output_url, &probe_limits())
        .await
        .expect("output probes");
    assert!(
        (info.duration.as_secs_f64() - 2.0).abs() < 0.4,
        "the trimmed output is {}s long",
        info.duration.as_secs_f64()
    );

    let landed = frame_signature(&output_url, None);
    let candidates: Vec<(f64, Vec<u8>)> = [0.0f64, 1.0, 2.0, 3.0]
        .into_iter()
        .map(|seconds| {
            (
                seconds,
                frame_signature(source.to_str().unwrap(), Some(seconds)),
            )
        })
        .collect();
    let nearest = candidates
        .iter()
        .map(|(seconds, frame)| (*seconds, frame_difference(frame, &landed)))
        .min_by_key(|(_, difference)| *difference)
        .expect("candidates are present");
    assert_eq!(
        nearest.0, 1.0,
        "the first output frame is the source frame at {nearest:?}"
    );
    let runner_up = candidates
        .iter()
        .map(|(_, frame)| frame_difference(frame, &landed))
        .filter(|difference| *difference > nearest.1)
        .min()
        .expect("the other frames are further away");
    assert!(
        runner_up > nearest.1 * 10,
        "the nearest frame at {} is not decisively closer than the runner up at {runner_up}",
        nearest.1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_thumbnail_job_lands_every_key_through_server_and_worker() {
    let _slot = mm_testkit::stack_slot();
    let (admin, pool, url, name) = fresh_pool().await;
    let store = Arc::new(test_store().await);
    let repository: Arc<dyn mm_domain::JobRepository> =
        Arc::new(mm_jobstore::PgJobRepository::new(pool.clone()));

    let source = fixture("thumbnail-e2e-6s.mp4", 6, 30, "mp4");
    let input = upload_object(&store, &source, "inputs/thumbnail-e2e-6s.mp4").await;

    let mut server_config = mm_server::ServerConfig::new(
        repository.clone(),
        store.clone(),
        BUCKET.to_string(),
        Prober::default(),
    )
    .expect("valid server config");
    server_config.job_events = Some(Arc::new(
        mm_jobstore::PgJobEventListener::connect(&url)
            .await
            .expect("events listener connects"),
    ));
    let server = mm_server::start(server_config)
        .await
        .expect("server starts");

    let channel =
        tonic::transport::Endpoint::from_shared(format!("http://{}", server.local_addr()))
            .expect("valid endpoint")
            .connect_timeout(Duration::from_secs(2))
            .connect()
            .await
            .expect("dial announced address");
    let mut client = mm_proto::v1::media_job_service_client::MediaJobServiceClient::new(channel);

    let spec = mm_proto::v1::OpSpec::from(&mm_domain::OpSpec::Thumbnail(
        mm_domain::ThumbnailSpec::new(input, vec![0.0, 2.0], 160, 120)
            .expect("valid thumbnail spec"),
    ));
    let created = client
        .create_job(mm_proto::v1::CreateJobRequest {
            user_id: "owner-thumbnail".to_string(),
            idempotency_key: "req-thumbnail".to_string(),
            spec: Some(spec),
            callback: None,
        })
        .await
        .expect("create succeeds")
        .into_inner();
    assert!(created.created);

    let staging = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scratch/e2e-thumbnail-staging");
    let mut worker_config = mm_worker::WorkerConfig::new(
        repository.clone(),
        store.clone(),
        BUCKET.to_string(),
        std::str::FromStr::from_str("worker-thumbnail-e2e").unwrap(),
        staging,
    );
    worker_config.reconcile_interval = Duration::from_secs(30);
    worker_config.notifications = Some(Arc::new(
        mm_jobstore::PgJobListener::connect(&url)
            .await
            .expect("listener connects"),
    ));
    let worker = mm_worker::start(worker_config);

    let final_job = loop {
        let job = client
            .get_job(mm_proto::v1::GetJobRequest {
                user_id: "owner-thumbnail".to_string(),
                job_id: created.job_id.clone(),
            })
            .await
            .expect("get succeeds")
            .into_inner()
            .job
            .expect("job present");
        if job.state() != mm_proto::v1::JobState::Pending
            && job.state() != mm_proto::v1::JobState::Running
        {
            break job;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(
        final_job.state(),
        mm_proto::v1::JobState::Succeeded,
        "thumbnail job did not succeed"
    );
    let result = mm_domain::OpResult::try_from(final_job.result.expect("result present"))
        .expect("result decodes");
    let mm_domain::OpResult::Thumbnail(thumbs) = result else {
        panic!("expected a thumbnail result");
    };
    assert_eq!(thumbs.outputs().len(), 2);

    let mut sizes = Vec::new();
    for (index, output) in thumbs.outputs().iter().enumerate() {
        let MediaRef::Object(object) = output else {
            panic!("expected an object output");
        };
        assert_eq!(
            object.key(),
            format!("jobs/{}/thumb-{index}.jpg", created.job_id)
        );
        let url = resolve_media_ref(
            store.as_ref(),
            &MediaRef::Object(object.clone()),
            SystemTime::now() + Duration::from_secs(300),
            SystemTime::now(),
        )
        .await
        .expect("thumbnail presigns");
        let still = Prober::default()
            .still(&url, &probe_limits())
            .await
            .expect("the stored thumbnail reports");
        assert_eq!((still.width, still.height), (160, 120));
        sizes.push(still.size_bytes.unwrap_or_default());
    }
    assert!(
        sizes.windows(2).any(|pair| pair[0] != pair[1]),
        "both offsets produced the same frame: {sizes:?}"
    );

    worker.shutdown().await;
    server.shutdown().await;
    drop_db(&admin, pool, &name).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_image_op_reaches_success_through_server_and_worker() {
    let _slot = mm_testkit::stack_slot();
    let (admin, pool, url, name) = fresh_pool().await;
    let store = Arc::new(test_store().await);
    let repository: Arc<dyn mm_domain::JobRepository> =
        Arc::new(mm_jobstore::PgJobRepository::new(pool.clone()));

    let base_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scratch/e2e-images");
    std::fs::create_dir_all(&base_dir).unwrap();
    let source = base_dir.join("e2e-source.png");
    if !source.is_file() {
        let generated = std::process::Command::new("magick")
            .arg("-size")
            .arg("600x400")
            .arg("xc:teal")
            .arg(&source)
            .output()
            .expect("the image tool must be runnable to generate fixtures");
        assert!(generated.status.success(), "fixture generation failed");
    }
    let input = upload_object(&store, &source, "inputs/e2e-source.png").await;

    let mut server_config = mm_server::ServerConfig::new(
        repository.clone(),
        store.clone(),
        BUCKET.to_string(),
        Prober::default(),
    )
    .expect("valid server config");
    server_config.job_events = Some(Arc::new(
        mm_jobstore::PgJobEventListener::connect(&url)
            .await
            .expect("events listener connects"),
    ));
    let server = mm_server::start(server_config)
        .await
        .expect("server starts");

    let channel =
        tonic::transport::Endpoint::from_shared(format!("http://{}", server.local_addr()))
            .expect("valid endpoint")
            .connect_timeout(Duration::from_secs(2))
            .connect()
            .await
            .expect("dial announced address");
    let mut client = mm_proto::v1::media_job_service_client::MediaJobServiceClient::new(channel);

    let spec = mm_proto::v1::OpSpec::from(&mm_domain::OpSpec::Image(
        mm_domain::ImageSpec::new(
            vec![input],
            mm_domain::ImageOp::Resize,
            200,
            150,
            mm_domain::ResizeMode::Exact,
            Some(mm_domain::ImageFormat::Jpeg),
        )
        .expect("valid resize spec"),
    ));
    let created = client
        .create_job(mm_proto::v1::CreateJobRequest {
            user_id: "owner-image".to_string(),
            idempotency_key: "req-image-resize".to_string(),
            spec: Some(spec),
            callback: None,
        })
        .await
        .expect("create succeeds")
        .into_inner();
    assert!(created.created);

    let staging = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scratch/e2e-image-staging");
    let mut worker_config = mm_worker::WorkerConfig::new(
        repository.clone(),
        store.clone(),
        BUCKET.to_string(),
        std::str::FromStr::from_str("worker-image-e2e").unwrap(),
        staging,
    );
    worker_config.reconcile_interval = Duration::from_secs(30);
    worker_config.notifications = Some(Arc::new(
        mm_jobstore::PgJobListener::connect(&url)
            .await
            .expect("listener connects"),
    ));
    let worker = mm_worker::start(worker_config);

    let final_job = loop {
        let job = client
            .get_job(mm_proto::v1::GetJobRequest {
                user_id: "owner-image".to_string(),
                job_id: created.job_id.clone(),
            })
            .await
            .expect("get succeeds")
            .into_inner()
            .job
            .expect("job present");
        if job.state() != mm_proto::v1::JobState::Pending
            && job.state() != mm_proto::v1::JobState::Running
        {
            break job;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(
        final_job.state(),
        mm_proto::v1::JobState::Succeeded,
        "image job did not succeed"
    );
    let result = mm_domain::OpResult::try_from(final_job.result.expect("result present"))
        .expect("result decodes");
    let mm_domain::OpResult::Image(image) = result else {
        panic!("expected an image result");
    };
    assert_eq!(image.outputs().len(), 1);
    let MediaRef::Object(object) = &image.outputs()[0] else {
        panic!("expected an object output");
    };
    assert_eq!(object.key(), format!("jobs/{}/image.jpg", created.job_id));
    let output_url = resolve_media_ref(
        store.as_ref(),
        &MediaRef::Object(object.clone()),
        SystemTime::now() + Duration::from_secs(300),
        SystemTime::now(),
    )
    .await
    .expect("image presigns");
    let still = Prober::default()
        .still(&output_url, &probe_limits())
        .await
        .expect("the produced image reports");
    assert_eq!((still.width, still.height), (200, 150));
    assert_eq!(still.codec, "mjpeg");

    worker.shutdown().await;
    server.shutdown().await;
    drop_db(&admin, pool, &name).await;
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
async fn a_mid_merge_cancel_kills_the_tool_and_cleans_every_output() {
    let _slot = mm_testkit::stack_slot();
    let (admin, pool, url, name) = fresh_pool().await;
    let store = Arc::new(test_store().await);
    let repository: Arc<dyn mm_domain::JobRepository> =
        Arc::new(mm_jobstore::PgJobRepository::new(pool.clone()));

    let first = fixture("cancel-a-60s.mp4", 60, 30, "mp4");
    let second = fixture("cancel-b-60s.mp4", 60, 30, "mp4");
    let inputs = vec![
        upload_object(&store, &first, "inputs/cancel-a-60s.mp4").await,
        upload_object(&store, &second, "inputs/cancel-b-60s.mp4").await,
    ];

    let server_config = mm_server::ServerConfig::new(
        repository.clone(),
        store.clone(),
        BUCKET.to_string(),
        Prober::default(),
    )
    .expect("valid server config");
    let server = mm_server::start(server_config)
        .await
        .expect("server starts");

    let staging = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scratch/e2e-cancel-staging");
    let mut worker_config = mm_worker::WorkerConfig::new(
        repository.clone(),
        store.clone(),
        BUCKET.to_string(),
        std::str::FromStr::from_str("worker-cancel").unwrap(),
        staging.clone(),
    );
    worker_config.heartbeat_interval = Duration::from_secs(3600);
    worker_config.reconcile_interval = Duration::from_secs(60);
    worker_config.notifications = Some(Arc::new(
        mm_jobstore::PgJobListener::connect(&url)
            .await
            .expect("listener connects"),
    ));
    worker_config.cancellations = Some(Arc::new(
        mm_jobstore::PgCancelListener::connect(&url)
            .await
            .expect("cancel listener connects"),
    ));
    let worker = mm_worker::start(worker_config);
    tokio::time::sleep(Duration::from_millis(400)).await;

    let channel =
        tonic::transport::Endpoint::from_shared(format!("http://{}", server.local_addr()))
            .expect("valid endpoint")
            .connect_timeout(Duration::from_secs(2))
            .timeout(mm_testkit::budget::RPC)
            .connect()
            .await
            .expect("dial announced address");
    let mut client = mm_proto::v1::media_job_service_client::MediaJobServiceClient::new(channel);

    let spec = mm_proto::v1::OpSpec::from(&mm_domain::OpSpec::Concat(
        mm_domain::ConcatSpec::new(inputs, None).expect("valid concat spec"),
    ));
    let created = client
        .create_job(mm_proto::v1::CreateJobRequest {
            user_id: "owner-cancel".to_string(),
            idempotency_key: "req-cancel-e2e".to_string(),
            spec: Some(spec),
            callback: None,
        })
        .await
        .expect("create succeeds")
        .into_inner();
    let job_id = created.job_id;

    let get = |mut client: mm_proto::v1::media_job_service_client::MediaJobServiceClient<
        tonic::transport::Channel,
    >,
               job_id: String| async move {
        client
            .get_job(mm_proto::v1::GetJobRequest {
                user_id: "owner-cancel".to_string(),
                job_id,
            })
            .await
            .expect("get succeeds")
            .into_inner()
            .job
            .expect("job present")
    };

    let running_deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let job = get(client.clone(), job_id.clone()).await;
        if job.state() == mm_proto::v1::JobState::Running {
            break;
        }
        assert!(
            job.state() == mm_proto::v1::JobState::Pending,
            "job ended early in {:?}",
            job.state()
        );
        assert!(
            std::time::Instant::now() < running_deadline,
            "job never started"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let found = std::time::Instant::now() + Duration::from_secs(30);
    let pid = loop {
        if let Some(pid) = find_process(&job_id) {
            break pid;
        }
        assert!(
            std::time::Instant::now() < found,
            "merge process never appeared"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    let cancel_sent = std::time::Instant::now();
    let response = client
        .cancel_job(mm_proto::v1::CancelJobRequest {
            user_id: "owner-cancel".to_string(),
            job_id: job_id.clone(),
        })
        .await
        .expect("cancel succeeds")
        .into_inner();
    assert!(response.cancellation_accepted);

    loop {
        let job = get(client.clone(), job_id.clone()).await;
        if job.state() == mm_proto::v1::JobState::Cancelled {
            break;
        }
        assert!(
            cancel_sent.elapsed() < mm_testkit::budget::CANCEL,
            "cancel never propagated inside the budget, far below the \
             one-minute reconcile tick; job is {:?}",
            job.state()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let gone = std::time::Instant::now() + mm_testkit::budget::PROCESS_EXIT;
    while process_mentions(pid, &job_id) {
        assert!(
            std::time::Instant::now() < gone,
            "merge process survived the cancel"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let object = mm_domain::ObjectRef::new(BUCKET, &format!("jobs/{job_id}/output.mp4")).unwrap();
    assert!(
        matches!(
            store.head(&object).await,
            Err(ObjectStoreError::NotFound { .. })
        ),
        "cancelled job left an output object"
    );
    let uploads = store
        .list_multipart_uploads(BUCKET)
        .await
        .expect("multipart listing succeeds");
    assert!(
        uploads.iter().all(|key| !key.contains(&job_id)),
        "incomplete multipart upload left behind: {uploads:?}"
    );
    let leftovers: Vec<String> = std::fs::read_dir(&staging)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|file| file.contains(&job_id))
                .collect()
        })
        .unwrap_or_default();
    assert!(leftovers.is_empty(), "staging not cleaned: {leftovers:?}");

    worker.shutdown().await;
    server.shutdown().await;
    drop_db(&admin, pool, &name).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_package_job_reaches_success_through_server_and_worker() {
    let _slot = mm_testkit::stack_slot();
    let (admin, pool, url, name) = fresh_pool().await;
    let store = Arc::new(test_store().await);
    let repository: Arc<dyn mm_domain::JobRepository> =
        Arc::new(mm_jobstore::PgJobRepository::new(pool.clone()));

    let staging_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scratch/e2e-package-src");
    std::fs::create_dir_all(&staging_dir).unwrap();
    let source = staging_dir.join("packaged.mp4");
    if !source.is_file() {
        let generated = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=duration=4:size=640x360:rate=30",
                "-c:v",
                "libx264",
                "-preset",
                "veryfast",
                "-pix_fmt",
                "yuv420p",
                "-g",
                "30",
                "-keyint_min",
                "30",
                "-sc_threshold",
                "0",
                "-movflags",
                "+faststart",
            ])
            .arg(&source)
            .output()
            .expect("the media tool must be runnable to generate fixtures");
        assert!(generated.status.success(), "fixture generation failed");
    }
    let input = upload_object(&store, &source, "inputs/packaged.mp4").await;

    let mut server_config = mm_server::ServerConfig::new(
        repository.clone(),
        store.clone(),
        BUCKET.to_string(),
        Prober::default(),
    )
    .expect("valid server config");
    server_config.job_events = Some(Arc::new(
        mm_jobstore::PgJobEventListener::connect(&url)
            .await
            .expect("events listener connects"),
    ));
    let server = mm_server::start(server_config)
        .await
        .expect("server starts");

    let channel =
        tonic::transport::Endpoint::from_shared(format!("http://{}", server.local_addr()))
            .expect("valid endpoint")
            .connect_timeout(Duration::from_secs(2))
            .connect()
            .await
            .expect("dial announced address");
    let mut client = mm_proto::v1::media_job_service_client::MediaJobServiceClient::new(channel);

    let spec = mm_proto::v1::OpSpec::from(&mm_domain::OpSpec::Package(
        mm_domain::PackageSpec::new(input, 1.0).expect("valid package spec"),
    ));
    let created = client
        .create_job(mm_proto::v1::CreateJobRequest {
            user_id: "owner-package".to_string(),
            idempotency_key: "req-package".to_string(),
            spec: Some(spec),
            callback: None,
        })
        .await
        .expect("create succeeds")
        .into_inner();
    assert!(created.created);

    let staging = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scratch/e2e-package-staging");
    let mut worker_config = mm_worker::WorkerConfig::new(
        repository.clone(),
        store.clone(),
        BUCKET.to_string(),
        std::str::FromStr::from_str("worker-package-e2e").unwrap(),
        staging,
    );
    worker_config.reconcile_interval = Duration::from_secs(30);
    worker_config.notifications = Some(Arc::new(
        mm_jobstore::PgJobListener::connect(&url)
            .await
            .expect("listener connects"),
    ));
    let worker = mm_worker::start(worker_config);

    let final_job = loop {
        let job = client
            .get_job(mm_proto::v1::GetJobRequest {
                user_id: "owner-package".to_string(),
                job_id: created.job_id.clone(),
            })
            .await
            .expect("get succeeds")
            .into_inner()
            .job
            .expect("job present");
        if job.state() != mm_proto::v1::JobState::Pending
            && job.state() != mm_proto::v1::JobState::Running
        {
            break job;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(
        final_job.state(),
        mm_proto::v1::JobState::Succeeded,
        "packaging job did not succeed"
    );
    let result = mm_domain::OpResult::try_from(final_job.result.expect("result present"))
        .expect("result decodes");
    let mm_domain::OpResult::Package(packaged) = result else {
        panic!("expected a package result");
    };
    assert!(
        packaged.segments().len() >= 2,
        "segments: {:?}",
        packaged.segments()
    );
    let MediaRef::Object(playlist) = packaged.playlist() else {
        panic!("expected an object playlist");
    };
    assert_eq!(
        playlist.key(),
        format!("jobs/{}/playlist.m3u8", created.job_id)
    );
    let playlist_body = store.get(playlist).await.expect("playlist fetches");
    let playlist_text = String::from_utf8(
        playlist_body
            .try_collect::<Vec<Bytes>>()
            .await
            .expect("playlist body collects")
            .concat(),
    )
    .unwrap();
    assert!(playlist_text.starts_with("#EXTM3U"), "{playlist_text}");
    assert!(playlist_text.contains("#EXT-X-ENDLIST"));

    let init = mm_domain::ObjectRef::new(BUCKET, &format!("jobs/{}/init.mp4", created.job_id))
        .expect("valid object ref");
    let mut playable = store
        .get(&init)
        .await
        .expect("init fetches")
        .try_collect::<Vec<Bytes>>()
        .await
        .expect("init body collects")
        .concat();
    let first = match packaged.segments().first() {
        Some(MediaRef::Object(object)) => object.clone(),
        _ => panic!("expected an object segment"),
    };
    let segment = store
        .get(&first)
        .await
        .expect("segment fetches")
        .try_collect::<Vec<Bytes>>()
        .await
        .expect("segment body collects")
        .concat();
    playable.extend(segment);
    let local = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scratch/e2e-package-staging")
        .join(format!("{}-probe.mp4", created.job_id));
    std::fs::write(&local, playable).expect("the probe copy writes");
    let info = Prober::default()
        .probe(&local.to_string_lossy(), &probe_limits())
        .await
        .expect("the init plus its first segment probes");
    assert_eq!(info.video.len(), 1);

    worker.shutdown().await;
    server.shutdown().await;
    drop_db(&admin, pool, &name).await;
}

fn http_put(url: &str, body: &[u8]) -> u16 {
    let without_scheme = url.strip_prefix("http://").expect("plain http");
    let authority = without_scheme.split('/').next().expect("authority");
    let path = &without_scheme[authority.len()..];
    let address = authority
        .to_socket_addrs()
        .expect("authority resolves")
        .next()
        .expect("authority resolves");
    let mut stream =
        TcpStream::connect_timeout(&address, Duration::from_secs(10)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    let request = format!(
        "PUT {path} HTTP/1.0\r\nHost: {authority}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(request.as_bytes()).expect("write head");
    stream.write_all(body).expect("write body");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    let head_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("headers end");
    String::from_utf8_lossy(&response[..head_end])
        .lines()
        .next()
        .expect("status line")
        .split_whitespace()
        .nth(1)
        .expect("status code")
        .parse()
        .expect("status code parses")
}

#[tokio::test(flavor = "multi_thread")]
async fn an_upload_target_lands_bytes_a_job_reads_back() {
    let _slot = mm_testkit::stack_slot();
    let (admin, pool, url, name) = fresh_pool().await;
    let store = Arc::new(test_store().await);
    let repository: Arc<dyn mm_domain::JobRepository> =
        Arc::new(mm_jobstore::PgJobRepository::new(pool.clone()));

    let server_config = mm_server::ServerConfig::new(
        repository.clone(),
        store.clone(),
        BUCKET.to_string(),
        Prober::default(),
    )
    .expect("valid server config");
    let server = mm_server::start(server_config)
        .await
        .expect("server starts");
    let channel =
        tonic::transport::Endpoint::from_shared(format!("http://{}", server.local_addr()))
            .expect("valid endpoint")
            .connect_timeout(Duration::from_secs(2))
            .connect()
            .await
            .expect("dial announced address");
    let mut client = mm_proto::v1::media_job_service_client::MediaJobServiceClient::new(channel);

    let target = client
        .create_upload_target(mm_proto::v1::CreateUploadTargetRequest {
            user_id: "owner-upload".to_string(),
            expires_seconds: 120,
        })
        .await
        .expect("upload target")
        .into_inner();
    assert!(target.upload_url.contains("X-Amz-Signature="));
    assert!(target.media_id.starts_with("s3://"), "{}", target.media_id);

    let source = fixture("upload-4s.mp4", 4, 30, "mp4");
    let bytes = std::fs::read(&source).expect("fixture readable");
    assert_eq!(http_put(&target.upload_url, &bytes), 200);

    let spec = mm_proto::v1::OpSpec::from(&mm_domain::OpSpec::Probe(
        mm_domain::ProbeSpec::new(MediaRef::object_uri(&target.media_id).expect("valid media id"))
            .expect("probe"),
    ));
    let created = client
        .create_job(mm_proto::v1::CreateJobRequest {
            user_id: "owner-upload".to_string(),
            idempotency_key: "req-upload".to_string(),
            spec: Some(spec),
            callback: None,
        })
        .await
        .expect("create job")
        .into_inner();

    let staging = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scratch/e2e-upload-staging");
    let mut worker_config = mm_worker::WorkerConfig::new(
        repository.clone(),
        store.clone(),
        BUCKET.to_string(),
        std::str::FromStr::from_str("worker-upload-e2e").unwrap(),
        staging,
    );
    worker_config.reconcile_interval = Duration::from_secs(30);
    worker_config.notifications = Some(Arc::new(
        mm_jobstore::PgJobListener::connect(&url)
            .await
            .expect("listener connects"),
    ));
    let worker = mm_worker::start(worker_config);

    let final_job = loop {
        let job = client
            .get_job(mm_proto::v1::GetJobRequest {
                user_id: "owner-upload".to_string(),
                job_id: created.job_id.clone(),
            })
            .await
            .expect("get succeeds")
            .into_inner()
            .job
            .expect("job present");
        if job.state() == mm_proto::v1::JobState::Succeeded
            || job.state() == mm_proto::v1::JobState::Failed
            || job.state() == mm_proto::v1::JobState::Cancelled
        {
            break job;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(
        final_job.state(),
        mm_proto::v1::JobState::Succeeded,
        "the job did not read the uploaded object"
    );
    let result = mm_domain::OpResult::try_from(final_job.result.expect("result present"))
        .expect("result decodes");
    assert!(matches!(result, mm_domain::OpResult::Probe(_)));

    worker.shutdown().await;
    server.shutdown().await;
    drop_db(&admin, pool, &name).await;
}

use std::env;
use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use mm_domain::{
    MediaRef, ObjectBody, ObjectRef, ObjectStore, ObjectStoreError, OpResult, QueueUrl,
};
use mm_ffmpeg::{ProbeLimits, Prober};
use mm_jobstore::MIGRATOR;
use mm_proto::v1;
use mm_proto::v1::media_job_service_client::MediaJobServiceClient;
use mm_storage::{CredentialSource, S3ObjectStore, S3Settings, SqsCallbackPublisher, SqsSettings};
use prost::Message;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tonic::Code;
use tonic::transport::{Channel, Endpoint};
use uuid::Uuid;

const DEFAULT_ADMIN_URL: &str = "postgres://mm:mm@127.0.0.1:5433/mm";
const DEFAULT_STORE_ENDPOINT: &str = "http://127.0.0.1:8333";
const DEFAULT_QUEUE_ENDPOINT: &str = "http://127.0.0.1:9324";
const DEFAULT_ACCESS_KEY: &str = "mm-test-access";
const DEFAULT_SECRET_KEY: &str = "mm-test-secret";
const BUCKET: &str = "mm-parity";

fn env_or(name: &str, fallback: &str) -> String {
    env::var(name).unwrap_or_else(|_| fallback.to_string())
}

fn admin_url() -> String {
    env_or("MM_DATABASE_URL", DEFAULT_ADMIN_URL)
}

fn store_endpoint() -> String {
    env_or("MM_OBJECTSTORE_ENDPOINT", DEFAULT_STORE_ENDPOINT)
}

fn queue_endpoint() -> String {
    env_or("MM_QUEUE_ENDPOINT", DEFAULT_QUEUE_ENDPOINT)
}

fn authority(endpoint: &str) -> String {
    endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))
        .expect("endpoint must be an http(s) url")
        .trim_end_matches('/')
        .to_string()
}

fn require_reachable(authority: &str, what: &str) {
    let address = authority
        .to_socket_addrs()
        .unwrap_or_else(|_| panic!("{what} authority must resolve"))
        .next()
        .unwrap_or_else(|| panic!("{what} authority must resolve to an address"));
    if TcpStream::connect_timeout(&address, Duration::from_secs(2)).is_err() {
        panic!(
            "{what} at {authority} is unreachable; \
             start the compose stack with `docker compose up -d` \
             before running the parity suite"
        );
    }
}

fn require_stack() {
    let url = admin_url();
    let db_authority = url
        .split('@')
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .expect("database url must contain an authority");
    require_reachable(db_authority, "database");
    require_reachable(&authority(&store_endpoint()), "object store");
    require_reachable(&authority(&queue_endpoint()), "queue");
}

struct TestDb {
    admin: PgPool,
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
    let name = format!("mm_parity_{}", Uuid::now_v7().simple());
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
        .max_connections(2)
        .connect(&url)
        .await
        .expect("test database connection");
    MIGRATOR.run(&pool).await.expect("migrations apply");
    pool.close().await;
    TestDb { admin, url, name }
}

impl TestDb {
    async fn dispose(self) {
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
    store.ensure_bucket(BUCKET).await.expect("bucket exists");
    Arc::new(store)
}

async fn queue_publisher() -> Arc<SqsCallbackPublisher> {
    Arc::new(
        SqsCallbackPublisher::connect(SqsSettings {
            endpoint: Some(queue_endpoint()),
            region: Some("local".to_string()),
            credentials: CredentialSource::Static {
                access_key_id: "queue-access".to_string(),
                secret_access_key: "queue-secret".to_string(),
            },
        })
        .await,
    )
}

fn scratch_dir(part: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../scratch/parity/{part}"));
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

async fn upload_input(store: &S3ObjectStore, path: &Path, key: &str) -> String {
    let payload = tokio::fs::read(path).await.unwrap();
    let length = payload.len() as u64;
    let object = ObjectRef::new(BUCKET, key).unwrap();
    let mut attempt = 0;
    loop {
        attempt += 1;
        let chunks: Vec<Result<bytes::Bytes, ObjectStoreError>> = payload
            .chunks(64 * 1024)
            .map(|chunk| Ok(bytes::Bytes::copy_from_slice(chunk)))
            .collect();
        let body: ObjectBody = Box::pin(futures_util::stream::iter(chunks));
        match store.put(&object, body, length).await {
            Ok(()) => break,
            Err(error) if attempt < 3 => {
                eprintln!("fixture upload attempt {attempt} failed: {error}");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(error) => panic!("fixture upload failed after {attempt} attempts: {error}"),
        }
    }
    format!("s3://{BUCKET}/{key}")
}

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_daemon(database_url: &str, worker_id: &str, extra: &[(&str, &str)]) -> Daemon {
    let staging = scratch_dir("staging");
    let mut command = Command::new(env!("CARGO_BIN_EXE_mmrpcd"));
    command
        .arg("combined")
        .env("MM_BIND_ADDR", "127.0.0.1:0")
        .env("MM_DATABASE_URL", database_url)
        .env("MM_OBJECTSTORE_ENDPOINT", store_endpoint())
        .env("MM_OBJECTSTORE_BUCKET", BUCKET)
        .env(
            "MM_OBJECTSTORE_REGION",
            env_or("MM_OBJECTSTORE_REGION", "us-east-1"),
        )
        .env(
            "MM_OBJECTSTORE_ACCESS_KEY",
            env_or("MM_OBJECTSTORE_ACCESS_KEY", DEFAULT_ACCESS_KEY),
        )
        .env(
            "MM_OBJECTSTORE_SECRET_KEY",
            env_or("MM_OBJECTSTORE_SECRET_KEY", DEFAULT_SECRET_KEY),
        )
        .env("MM_QUEUE_ENDPOINT", queue_endpoint())
        .env("MM_QUEUE_REGION", "local")
        .env("MM_QUEUE_ACCESS_KEY", "queue-access")
        .env("MM_QUEUE_SECRET_KEY", "queue-secret")
        .env("MM_WORKER_ID", worker_id)
        .env("MM_WORKER_STAGING_DIR", &staging)
        .env("MM_WORKER_RECONCILE_SECS", "1")
        .env("MM_WORKER_PROGRESS_SECS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for (name, value) in extra {
        command.env(name, value);
    }
    let child = command.spawn().expect("daemon spawns");
    Daemon { child }
}

async fn announced_address(daemon: &mut Daemon) -> SocketAddr {
    let stdout = daemon.child.stdout.take().expect("stdout is piped");
    tokio::time::timeout(
        Duration::from_secs(30),
        tokio::task::spawn_blocking(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                let count = reader.read_line(&mut line).expect("announce line readable");
                assert!(count > 0, "daemon exited before announcing");
                if let Some(address) = line.trim().strip_prefix("listening ") {
                    return address.parse().expect("announced address parses");
                }
            }
        }),
    )
    .await
    .expect("daemon announces in time")
    .expect("announce reader finishes")
}

async fn unary_client(address: SocketAddr) -> MediaJobServiceClient<Channel> {
    let channel = Endpoint::from_shared(format!("http://{address}"))
        .expect("valid endpoint")
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .connect()
        .await
        .expect("dial announced address");
    MediaJobServiceClient::new(channel)
}

async fn stream_client(address: SocketAddr) -> MediaJobServiceClient<Channel> {
    let channel = Endpoint::from_shared(format!("http://{address}"))
        .expect("valid endpoint")
        .connect_timeout(Duration::from_secs(5))
        .connect()
        .await
        .expect("dial announced address");
    MediaJobServiceClient::new(channel)
}

fn object_ref(uri: &str) -> v1::MediaRef {
    v1::MediaRef {
        r#ref: Some(v1::media_ref::Ref::ObjectUri(uri.to_string())),
    }
}

fn concat_spec(inputs: &[String]) -> v1::OpSpec {
    v1::OpSpec {
        op: Some(v1::op_spec::Op::Concat(v1::ConcatSpec {
            inputs: inputs.iter().map(|uri| object_ref(uri)).collect(),
            profile: None,
        })),
    }
}

fn probe_spec(uri: &str) -> v1::OpSpec {
    v1::OpSpec {
        op: Some(v1::op_spec::Op::Probe(v1::ProbeSpec {
            input: Some(object_ref(uri)),
        })),
    }
}

async fn create_job(
    client: &mut MediaJobServiceClient<Channel>,
    user: &str,
    key: &str,
    spec: v1::OpSpec,
    callback: Option<&QueueUrl>,
) -> v1::CreateJobResponse {
    client
        .create_job(v1::CreateJobRequest {
            user_id: user.to_string(),
            idempotency_key: key.to_string(),
            spec: Some(spec),
            callback: callback.map(|queue| v1::Callback {
                queue_url: queue.as_str().to_string(),
            }),
        })
        .await
        .expect("create succeeds")
        .into_inner()
}

async fn get_job(client: &mut MediaJobServiceClient<Channel>, user: &str, job_id: &str) -> v1::Job {
    client
        .get_job(v1::GetJobRequest {
            user_id: user.to_string(),
            job_id: job_id.to_string(),
        })
        .await
        .expect("get succeeds")
        .into_inner()
        .job
        .expect("job present")
}

async fn await_job_state(
    client: &mut MediaJobServiceClient<Channel>,
    user: &str,
    job_id: &str,
    wanted: v1::JobState,
    timeout: Duration,
) -> v1::Job {
    let deadline = Instant::now() + timeout;
    loop {
        let job = get_job(client, user, job_id).await;
        if job.state() == wanted {
            return job;
        }
        let terminal = matches!(
            job.state(),
            v1::JobState::Succeeded | v1::JobState::Failed | v1::JobState::Cancelled
        );
        assert!(
            !terminal,
            "job {job_id} settled in {:?} instead of {wanted:?}",
            job.state()
        );
        assert!(
            Instant::now() < deadline,
            "job {job_id} never reached {wanted:?}; still {:?}",
            job.state()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn await_terminal(
    client: &mut MediaJobServiceClient<Channel>,
    user: &str,
    job_id: &str,
    timeout: Duration,
) -> v1::Job {
    let deadline = Instant::now() + timeout;
    loop {
        let job = get_job(client, user, job_id).await;
        if matches!(
            job.state(),
            v1::JobState::Succeeded | v1::JobState::Failed | v1::JobState::Cancelled
        ) {
            return job;
        }
        assert!(
            Instant::now() < deadline,
            "job {job_id} never settled; still {:?}",
            job.state()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn one_outcome(publisher: &SqsCallbackPublisher, queue: &QueueUrl) -> v1::JobOutcome {
    let settle = Instant::now() + Duration::from_secs(60);
    let bodies = loop {
        let received = tokio::time::timeout(
            Duration::from_secs(10),
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
        Duration::from_secs(10),
        publisher.receive(queue, Duration::from_secs(1)),
    )
    .await
    .expect("receive answers in time")
    .expect("receive succeeds");
    assert!(leftover.is_empty(), "a second callback message arrived");
    let decoded = BASE64_STANDARD
        .decode(&bodies[0])
        .expect("message is base64");
    v1::JobOutcome::decode(decoded.as_slice()).expect("message is a job outcome")
}

fn concat_output_key(job: &v1::Job) -> String {
    let result =
        OpResult::try_from(job.result.clone().expect("result present")).expect("result decodes");
    let OpResult::Concat(timed) = result else {
        panic!("expected a concat result");
    };
    let MediaRef::Object(object) = timed.output().clone() else {
        panic!("expected an object output");
    };
    object.key().to_string()
}

fn probe_limits() -> ProbeLimits {
    ProbeLimits {
        probe_size_bytes: 1024 * 1024,
        analyze_duration: Duration::from_secs(2),
        timeout: Duration::from_secs(30),
    }
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
async fn p01_idempotent_create_replays_and_races_to_one_row() {
    let db = fresh_db().await;
    let store = test_store().await;
    let first = fixture("merge-a-6s.mp4", 6, 30, "mp4");
    let second = fixture("merge-b-4s.mp4", 4, 30, "mp4");
    let inputs = vec![
        upload_input(&store, &first, "inputs/p01-a.mp4").await,
        upload_input(&store, &second, "inputs/p01-b.mp4").await,
    ];
    let mut daemon = spawn_daemon(&db.url, "parity-p01", &[]);
    let address = announced_address(&mut daemon).await;
    let mut client = unary_client(address).await;

    let created = create_job(
        &mut client,
        "owner-p01",
        "req-idem",
        concat_spec(&inputs),
        None,
    )
    .await;
    assert!(created.created, "the first create inserts");
    let replayed = create_job(
        &mut client,
        "owner-p01",
        "req-idem",
        concat_spec(&inputs),
        None,
    )
    .await;
    assert!(!replayed.created, "the replay reports created=false");
    assert_eq!(replayed.job_id, created.job_id, "the replay keeps the id");
    assert_eq!(replayed.request_id, "req-idem");

    let mut racer_a = unary_client(address).await;
    let mut racer_b = unary_client(address).await;
    let race = inputs.clone();
    let (left, right) = tokio::join!(
        create_job(
            &mut racer_a,
            "owner-p01",
            "req-race",
            concat_spec(&race),
            None
        ),
        create_job(
            &mut racer_b,
            "owner-p01",
            "req-race",
            concat_spec(&inputs),
            None
        ),
    );
    assert_eq!(left.job_id, right.job_id, "both racers see one job");
    assert_eq!(
        [left.created, right.created].iter().filter(|c| **c).count(),
        1,
        "exactly one racer inserted"
    );

    let listed = client
        .list_jobs(v1::ListJobsRequest {
            user_id: "owner-p01".to_string(),
            statuses: vec![],
            page_size: 50,
            page_token: String::new(),
        })
        .await
        .expect("list succeeds")
        .into_inner();
    assert_eq!(listed.jobs.len(), 2, "two keys made exactly two rows");

    println!(
        "parity p01 idempotent-create: pass (replay created=false kept {}; race produced one row)",
        created.job_id
    );
    drop(daemon);
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn p02_foreign_owner_get_is_not_found_like_an_absent_job() {
    let db = fresh_db().await;
    let store = test_store().await;
    let first = fixture("merge-a-6s.mp4", 6, 30, "mp4");
    let second = fixture("merge-b-4s.mp4", 4, 30, "mp4");
    let inputs = vec![
        upload_input(&store, &first, "inputs/p02-a.mp4").await,
        upload_input(&store, &second, "inputs/p02-b.mp4").await,
    ];
    let mut daemon = spawn_daemon(&db.url, "parity-p02", &[]);
    let address = announced_address(&mut daemon).await;
    let mut client = unary_client(address).await;

    let created = create_job(
        &mut client,
        "owner-p02",
        "req-owned",
        concat_spec(&inputs),
        None,
    )
    .await;

    let foreign = client
        .get_job(v1::GetJobRequest {
            user_id: "intruder-p02".to_string(),
            job_id: created.job_id.clone(),
        })
        .await
        .expect_err("a foreign owner must not see the job");
    let absent = client
        .get_job(v1::GetJobRequest {
            user_id: "intruder-p02".to_string(),
            job_id: Uuid::now_v7().to_string(),
        })
        .await
        .expect_err("an absent job must not resolve");
    assert_eq!(foreign.code(), Code::NotFound);
    assert_eq!(absent.code(), Code::NotFound);
    assert_eq!(
        foreign.message(),
        absent.message(),
        "foreign and absent answers must be indistinguishable"
    );

    let owned = get_job(&mut client, "owner-p02", &created.job_id).await;
    assert_eq!(owned.job_id, created.job_id);

    println!(
        "parity p02 foreign-owner: pass (NOT_FOUND message {:?} matches the absent case)",
        foreign.message()
    );
    drop(daemon);
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn p03_listing_pages_stably_filters_by_status_and_rejects_bad_cursors() {
    let db = fresh_db().await;
    let store = test_store().await;
    let media = fixture("merge-a-6s.mp4", 6, 30, "mp4");
    let input = upload_input(&store, &media, "inputs/p03.mp4").await;
    let mut daemon = spawn_daemon(&db.url, "parity-p03", &[]);
    let address = announced_address(&mut daemon).await;
    let mut client = unary_client(address).await;

    let mut created_order = Vec::new();
    for index in 0..7 {
        let created = create_job(
            &mut client,
            "owner-p03",
            &format!("req-list-{index}"),
            probe_spec(&input),
            None,
        )
        .await;
        created_order.push(created.job_id);
    }
    for job_id in &created_order {
        let settled =
            await_terminal(&mut client, "owner-p03", job_id, Duration::from_secs(30)).await;
        assert_eq!(settled.state(), v1::JobState::Failed);
    }

    let mut pages = Vec::new();
    let mut token = String::new();
    loop {
        let page = client
            .list_jobs(v1::ListJobsRequest {
                user_id: "owner-p03".to_string(),
                statuses: vec![],
                page_size: 3,
                page_token: token.clone(),
            })
            .await
            .expect("list succeeds")
            .into_inner();
        pages.push(
            page.jobs
                .iter()
                .map(|job| job.job_id.clone())
                .collect::<Vec<_>>(),
        );
        if page.next_page_token.is_empty() {
            break;
        }
        token = page.next_page_token;
    }
    assert_eq!(
        pages.iter().map(Vec::len).collect::<Vec<_>>(),
        vec![3, 3, 1],
        "7 jobs page as 3+3+1"
    );
    let paged: Vec<String> = pages.concat();
    let mut newest_first = created_order.clone();
    newest_first.reverse();
    assert_eq!(
        paged, newest_first,
        "keyset order is creation order reversed"
    );

    let repeat = client
        .list_jobs(v1::ListJobsRequest {
            user_id: "owner-p03".to_string(),
            statuses: vec![],
            page_size: 3,
            page_token: String::new(),
        })
        .await
        .expect("list succeeds")
        .into_inner();
    assert_eq!(
        repeat
            .jobs
            .iter()
            .map(|job| job.job_id.clone())
            .collect::<Vec<_>>(),
        pages[0],
        "the first page repeats stably"
    );

    let failed = client
        .list_jobs(v1::ListJobsRequest {
            user_id: "owner-p03".to_string(),
            statuses: vec![v1::JobState::Failed as i32],
            page_size: 50,
            page_token: String::new(),
        })
        .await
        .expect("list succeeds")
        .into_inner();
    assert_eq!(failed.jobs.len(), 7, "the FAILED filter matches every job");
    let succeeded = client
        .list_jobs(v1::ListJobsRequest {
            user_id: "owner-p03".to_string(),
            statuses: vec![v1::JobState::Succeeded as i32],
            page_size: 50,
            page_token: String::new(),
        })
        .await
        .expect("list succeeds")
        .into_inner();
    assert!(
        succeeded.jobs.is_empty(),
        "the SUCCEEDED filter matches none"
    );

    let bad_cursor = client
        .list_jobs(v1::ListJobsRequest {
            user_id: "owner-p03".to_string(),
            statuses: vec![],
            page_size: 3,
            page_token: "not-a-cursor!!".to_string(),
        })
        .await
        .expect_err("a malformed cursor must be rejected");
    assert_eq!(bad_cursor.code(), Code::InvalidArgument);

    println!(
        "parity p03 listing: pass (pages 3+3+1 stable newest-first; FAILED filter 7/7; \
         bad cursor INVALID_ARGUMENT)"
    );
    drop(daemon);
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn p04_a_blocked_container_fails_with_the_typed_code_in_file_errors() {
    let db = fresh_db().await;
    let store = test_store().await;
    let foreign = fixture("blocked.mkv", 2, 30, "matroska");
    let good = fixture("merge-b-4s.mp4", 4, 30, "mp4");
    let inputs = vec![
        upload_input(&store, &foreign, "inputs/p04-blocked.mkv").await,
        upload_input(&store, &good, "inputs/p04-good.mp4").await,
    ];
    let mut daemon = spawn_daemon(&db.url, "parity-p04", &[]);
    let address = announced_address(&mut daemon).await;
    let mut client = unary_client(address).await;

    let created = create_job(
        &mut client,
        "owner-p04",
        "req-block",
        concat_spec(&inputs),
        None,
    )
    .await;
    let settled = await_terminal(
        &mut client,
        "owner-p04",
        &created.job_id,
        Duration::from_secs(60),
    )
    .await;
    assert_eq!(settled.state(), v1::JobState::Failed);
    assert!(settled.result.is_none(), "a blocked job has no result");
    assert!(!settled.file_errors.is_empty(), "file errors are reported");
    assert_eq!(settled.file_errors[0].code, "UnsupportedContainer");
    assert_eq!(
        settled.file_errors[0].media_ref,
        Some(object_ref(&inputs[0])),
        "the error names the offending input"
    );

    println!(
        "parity p04 block-verdict: pass (job {} FAILED with \
         file_errors[0]=UnsupportedContainer on input 0)",
        created.job_id
    );
    drop(daemon);
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn p05_a_warn_drift_input_still_merges() {
    let db = fresh_db().await;
    let store = test_store().await;
    let main = fixture("merge-a-6s.mp4", 6, 30, "mp4");
    let drifting = fixture("drift-25fps-4s.mp4", 4, 25, "mp4");
    let inputs = vec![
        upload_input(&store, &main, "inputs/p05-main.mp4").await,
        upload_input(&store, &drifting, "inputs/p05-drift.mp4").await,
    ];
    let mut daemon = spawn_daemon(&db.url, "parity-p05", &[]);
    let address = announced_address(&mut daemon).await;
    let mut client = unary_client(address).await;

    let created = create_job(
        &mut client,
        "owner-p05",
        "req-warn",
        concat_spec(&inputs),
        None,
    )
    .await;
    let settled = await_terminal(
        &mut client,
        "owner-p05",
        &created.job_id,
        Duration::from_secs(120),
    )
    .await;
    assert_eq!(
        settled.state(),
        v1::JobState::Succeeded,
        "an fps drift warns without blocking"
    );
    assert!(settled.file_errors.is_empty());
    assert_eq!(
        concat_output_key(&settled),
        format!("jobs/{}/output.mp4", created.job_id)
    );

    println!(
        "parity p05 warn-drift: pass (25fps second input merged; job {} SUCCEEDED)",
        created.job_id
    );
    drop(daemon);
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn p06_a_full_merge_lands_a_probe_valid_faststart_output() {
    let db = fresh_db().await;
    let store = test_store().await;
    let first = fixture("merge-a-6s.mp4", 6, 30, "mp4");
    let second = fixture("merge-b-4s.mp4", 4, 30, "mp4");
    let inputs = vec![
        upload_input(&store, &first, "inputs/p06-a.mp4").await,
        upload_input(&store, &second, "inputs/p06-b.mp4").await,
    ];
    let mut daemon = spawn_daemon(&db.url, "parity-p06", &[]);
    let address = announced_address(&mut daemon).await;
    let mut client = unary_client(address).await;

    let created = create_job(
        &mut client,
        "owner-p06",
        "req-merge",
        concat_spec(&inputs),
        None,
    )
    .await;
    let settled = await_terminal(
        &mut client,
        "owner-p06",
        &created.job_id,
        Duration::from_secs(120),
    )
    .await;
    assert_eq!(settled.state(), v1::JobState::Succeeded);
    assert_eq!(settled.progress_percent, 100);

    let key = concat_output_key(&settled);
    assert_eq!(key, format!("jobs/{}/output.mp4", created.job_id));
    let object = ObjectRef::new(BUCKET, &key).unwrap();
    let size = store.head(&object).await.expect("output exists");
    assert!(size > 0);

    let body = store.get(&object).await.expect("output downloads");
    let bytes = mm_domain::collect_body(body).await.expect("body collects");
    let moov = bytes.windows(4).position(|window| window == b"moov");
    let mdat = bytes.windows(4).position(|window| window == b"mdat");
    let (moov, mdat) = (moov.expect("moov present"), mdat.expect("mdat present"));
    assert!(moov < mdat, "faststart places moov before mdat");

    let url = mm_domain::resolve_media_ref(
        store.as_ref(),
        &MediaRef::object_uri(&format!("s3://{BUCKET}/{key}")).unwrap(),
        SystemTime::now() + Duration::from_secs(300),
        SystemTime::now(),
    )
    .await
    .expect("output presigns");
    let info = Prober::default()
        .probe(&url, &probe_limits())
        .await
        .expect("output probes");
    assert!(info.container.split(',').any(|token| token == "mp4"));
    assert_eq!(info.video[0].codec, "h264");
    assert_eq!(info.audio[0].codec, "aac");
    assert!(
        (info.duration.as_secs_f64() - 10.0).abs() < 0.6,
        "duration {} is not the 6s+4s sum",
        info.duration.as_secs_f64()
    );

    println!(
        "parity p06 full-merge: pass (jobs/{}/output.mp4, {size} bytes, h264/aac mp4, \
         moov@{moov}<mdat@{mdat}, duration {:.2}s)",
        created.job_id,
        info.duration.as_secs_f64()
    );
    drop(daemon);
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn p07_a_watch_stream_runs_snapshot_to_terminal_and_closes() {
    let db = fresh_db().await;
    let store = test_store().await;
    let first = fixture("watch-a-30s.mp4", 30, 30, "mp4");
    let second = fixture("watch-b-30s.mp4", 30, 30, "mp4");
    let inputs = vec![
        upload_input(&store, &first, "inputs/p07-a.mp4").await,
        upload_input(&store, &second, "inputs/p07-b.mp4").await,
    ];
    let mut daemon = spawn_daemon(&db.url, "parity-p07", &[]);
    let address = announced_address(&mut daemon).await;
    let mut client = unary_client(address).await;
    let mut watcher = stream_client(address).await;

    let created = create_job(
        &mut client,
        "owner-p07",
        "req-watch",
        concat_spec(&inputs),
        None,
    )
    .await;
    let mut stream = watcher
        .watch_job(v1::WatchJobRequest {
            user_id: "owner-p07".to_string(),
            job_id: created.job_id.clone(),
        })
        .await
        .expect("watch starts")
        .into_inner();

    let mut kinds = Vec::new();
    loop {
        let message = tokio::time::timeout(Duration::from_secs(120), stream.message())
            .await
            .expect("an event arrives in time")
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

    use v1::job_event::Kind;
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
            v1::JobState::Pending as i32,
            v1::JobState::Running as i32,
            v1::JobState::Succeeded as i32,
        ],
        "states run snapshot to terminal"
    );
    assert_eq!(
        kinds.first(),
        Some(&Kind::State(v1::JobState::Pending as i32)),
        "the snapshot leads"
    );
    assert_eq!(
        kinds.last(),
        Some(&Kind::State(v1::JobState::Succeeded as i32)),
        "the terminal state closes the stream"
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

    println!(
        "parity p07 watch: pass (PENDING->RUNNING->SUCCEEDED, {} distinct progress values, \
         stream closed)",
        distinct.len()
    );
    drop(daemon);
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn p08_a_mid_merge_cancel_leaves_no_output_and_no_process() {
    let db = fresh_db().await;
    let store = test_store().await;
    let first = fixture("cancel-a-60s.mp4", 60, 30, "mp4");
    let second = fixture("cancel-b-60s.mp4", 60, 30, "mp4");
    let inputs = vec![
        upload_input(&store, &first, "inputs/p08-a.mp4").await,
        upload_input(&store, &second, "inputs/p08-b.mp4").await,
    ];
    let mut daemon = spawn_daemon(&db.url, "parity-p08", &[]);
    let address = announced_address(&mut daemon).await;
    let mut client = unary_client(address).await;

    let created = create_job(
        &mut client,
        "owner-p08",
        "req-cancel",
        concat_spec(&inputs),
        None,
    )
    .await;
    await_job_state(
        &mut client,
        "owner-p08",
        &created.job_id,
        v1::JobState::Running,
        Duration::from_secs(60),
    )
    .await;
    let found = Instant::now() + Duration::from_secs(30);
    let pid = loop {
        if let Some(pid) = find_process(&created.job_id) {
            break pid;
        }
        assert!(Instant::now() < found, "merge process never appeared");
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    let cancel_sent = Instant::now();
    let response = client
        .cancel_job(v1::CancelJobRequest {
            user_id: "owner-p08".to_string(),
            job_id: created.job_id.clone(),
        })
        .await
        .expect("cancel succeeds")
        .into_inner();
    assert!(response.cancellation_accepted);

    loop {
        let job = get_job(&mut client, "owner-p08", &created.job_id).await;
        if job.state() == v1::JobState::Cancelled {
            break;
        }
        assert!(
            cancel_sent.elapsed() < Duration::from_secs(15),
            "cancel never propagated; job is {:?}",
            job.state()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let propagated = cancel_sent.elapsed();

    let gone = Instant::now() + Duration::from_secs(5);
    while process_mentions(pid, &created.job_id) {
        assert!(Instant::now() < gone, "merge process survived the cancel");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let object = ObjectRef::new(BUCKET, &format!("jobs/{}/output.mp4", created.job_id)).unwrap();
    assert!(
        matches!(
            store.head(&object).await,
            Err(ObjectStoreError::NotFound { .. })
        ),
        "a cancelled job must leave no output object"
    );

    println!(
        "parity p08 cancel: pass (CANCELLED in {propagated:?}, pid {pid} gone, no output object)"
    );
    drop(daemon);
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn p09_a_succeeded_job_delivers_one_matching_callback_outcome() {
    let db = fresh_db().await;
    let store = test_store().await;
    let publisher = queue_publisher().await;
    let queue = publisher
        .ensure_queue(&format!("mm-parity-ok-{}", Uuid::now_v7().simple()))
        .await
        .expect("queue exists");
    let first = fixture("merge-a-6s.mp4", 6, 30, "mp4");
    let second = fixture("merge-b-4s.mp4", 4, 30, "mp4");
    let inputs = vec![
        upload_input(&store, &first, "inputs/p09-a.mp4").await,
        upload_input(&store, &second, "inputs/p09-b.mp4").await,
    ];
    let mut daemon = spawn_daemon(&db.url, "parity-p09", &[]);
    let address = announced_address(&mut daemon).await;
    let mut client = unary_client(address).await;

    let created = create_job(
        &mut client,
        "owner-p09",
        "req-cb-ok",
        concat_spec(&inputs),
        Some(&queue),
    )
    .await;

    let outcome = one_outcome(&publisher, &queue).await;
    assert_eq!(outcome.job_id, created.job_id);
    assert_eq!(outcome.request_id, "req-cb-ok");
    assert_eq!(outcome.state, v1::JobState::Succeeded as i32);
    assert_eq!(outcome.error_code, "");
    assert!(outcome.file_errors.is_empty());
    let result = OpResult::try_from(outcome.result.expect("result present")).expect("decodes");
    let OpResult::Concat(timed) = result else {
        panic!("expected a concat result");
    };
    let MediaRef::Object(object) = timed.output().clone() else {
        panic!("expected an object output");
    };
    assert_eq!(object.key(), format!("jobs/{}/output.mp4", created.job_id));
    assert!(store.head(&object).await.expect("output exists") > 0);

    println!(
        "parity p09 callback-success: pass (one outcome, request req-cb-ok, result key {})",
        object.key()
    );
    publisher.delete_queue(&queue).await.expect("queue removed");
    drop(daemon);
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn p10_a_blocked_job_delivers_its_error_and_file_errors_by_callback() {
    let db = fresh_db().await;
    let store = test_store().await;
    let publisher = queue_publisher().await;
    let queue = publisher
        .ensure_queue(&format!("mm-parity-bl-{}", Uuid::now_v7().simple()))
        .await
        .expect("queue exists");
    let foreign = fixture("blocked.mkv", 2, 30, "matroska");
    let good = fixture("merge-b-4s.mp4", 4, 30, "mp4");
    let inputs = vec![
        upload_input(&store, &foreign, "inputs/p10-blocked.mkv").await,
        upload_input(&store, &good, "inputs/p10-good.mp4").await,
    ];
    let mut daemon = spawn_daemon(&db.url, "parity-p10", &[]);
    let address = announced_address(&mut daemon).await;
    let mut client = unary_client(address).await;

    let created = create_job(
        &mut client,
        "owner-p10",
        "req-cb-block",
        concat_spec(&inputs),
        Some(&queue),
    )
    .await;

    let outcome = one_outcome(&publisher, &queue).await;
    assert_eq!(outcome.job_id, created.job_id);
    assert_eq!(outcome.request_id, "req-cb-block");
    assert_eq!(outcome.state, v1::JobState::Failed as i32);
    assert_eq!(outcome.error_code, "validation_blocked");
    assert!(outcome.result.is_none());
    assert!(!outcome.file_errors.is_empty());
    assert_eq!(outcome.file_errors[0].code, "UnsupportedContainer");
    assert_eq!(
        outcome.file_errors[0].media_ref,
        Some(object_ref(&inputs[0]))
    );

    println!(
        "parity p10 callback-block: pass (one outcome, validation_blocked, \
         file_errors[0]=UnsupportedContainer)"
    );
    publisher.delete_queue(&queue).await.expect("queue removed");
    drop(daemon);
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn p11_a_job_cancelled_while_pending_delivers_one_callback() {
    let db = fresh_db().await;
    let store = test_store().await;
    let publisher = queue_publisher().await;
    let queue = publisher
        .ensure_queue(&format!("mm-parity-pc-{}", Uuid::now_v7().simple()))
        .await
        .expect("queue exists");
    let first = fixture("cancel-a-60s.mp4", 60, 30, "mp4");
    let second = fixture("cancel-b-60s.mp4", 60, 30, "mp4");
    let long_inputs = vec![
        upload_input(&store, &first, "inputs/p11-a.mp4").await,
        upload_input(&store, &second, "inputs/p11-b.mp4").await,
    ];
    let mut daemon = spawn_daemon(&db.url, "parity-p11", &[("MM_WORKER_MAX_CONCURRENT", "1")]);
    let address = announced_address(&mut daemon).await;
    let mut client = unary_client(address).await;

    let blocker = create_job(
        &mut client,
        "owner-p11",
        "req-blocker",
        concat_spec(&long_inputs),
        None,
    )
    .await;
    await_job_state(
        &mut client,
        "owner-p11",
        &blocker.job_id,
        v1::JobState::Running,
        Duration::from_secs(60),
    )
    .await;

    let parked = create_job(
        &mut client,
        "owner-p11",
        "req-parked",
        concat_spec(&long_inputs),
        Some(&queue),
    )
    .await;
    let pending = get_job(&mut client, "owner-p11", &parked.job_id).await;
    assert_eq!(
        pending.state(),
        v1::JobState::Pending,
        "the single-slot worker leaves the second job pending"
    );

    let response = client
        .cancel_job(v1::CancelJobRequest {
            user_id: "owner-p11".to_string(),
            job_id: parked.job_id.clone(),
        })
        .await
        .expect("cancel succeeds")
        .into_inner();
    assert!(response.cancellation_accepted);
    assert_eq!(
        response.job.expect("job present").state(),
        v1::JobState::Cancelled,
        "a pending cancel lands immediately"
    );

    let outcome = one_outcome(&publisher, &queue).await;
    assert_eq!(outcome.job_id, parked.job_id);
    assert_eq!(outcome.request_id, "req-parked");
    assert_eq!(outcome.state, v1::JobState::Cancelled as i32);
    assert_eq!(outcome.error_code, "");
    assert!(outcome.result.is_none());

    println!(
        "parity p11 pending-cancel-callback: pass (job {} cancelled before any worker \
         touched it; one CANCELLED outcome delivered)",
        parked.job_id
    );
    publisher.delete_queue(&queue).await.expect("queue removed");
    drop(daemon);
    db.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn p12_a_deadline_reaped_job_delivers_one_timeout_callback() {
    let db = fresh_db().await;
    let store = test_store().await;
    let publisher = queue_publisher().await;
    let queue = publisher
        .ensure_queue(&format!("mm-parity-rc-{}", Uuid::now_v7().simple()))
        .await
        .expect("queue exists");
    let first = fixture("cancel-a-60s.mp4", 60, 30, "mp4");
    let second = fixture("cancel-b-60s.mp4", 60, 30, "mp4");
    let long_inputs = vec![
        upload_input(&store, &first, "inputs/p12-a.mp4").await,
        upload_input(&store, &second, "inputs/p12-b.mp4").await,
    ];
    let mut daemon = spawn_daemon(&db.url, "parity-p12", &[("MM_WORKER_MAX_CONCURRENT", "1")]);
    let address = announced_address(&mut daemon).await;
    let mut client = unary_client(address).await;

    let blocker = create_job(
        &mut client,
        "owner-p12",
        "req-blocker",
        concat_spec(&long_inputs),
        None,
    )
    .await;
    await_job_state(
        &mut client,
        "owner-p12",
        &blocker.job_id,
        v1::JobState::Running,
        Duration::from_secs(60),
    )
    .await;

    let parked = create_job(
        &mut client,
        "owner-p12",
        "req-reaped",
        concat_spec(&long_inputs),
        Some(&queue),
    )
    .await;
    let pending = get_job(&mut client, "owner-p12", &parked.job_id).await;
    assert_eq!(pending.state(), v1::JobState::Pending);

    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&db.url)
        .await
        .expect("test database connection");
    sqlx::query("UPDATE jobs SET deadline_at = now() WHERE id = $1::uuid")
        .bind(&parked.job_id)
        .execute(&pool)
        .await
        .expect("deadline shrinks");
    pool.close().await;

    let settled = await_terminal(
        &mut client,
        "owner-p12",
        &parked.job_id,
        Duration::from_secs(30),
    )
    .await;
    assert_eq!(
        settled.state(),
        v1::JobState::Failed,
        "the reaper fails an overdue pending job"
    );

    let outcome = one_outcome(&publisher, &queue).await;
    assert_eq!(outcome.job_id, parked.job_id);
    assert_eq!(outcome.request_id, "req-reaped");
    assert_eq!(outcome.state, v1::JobState::Failed as i32);
    assert_eq!(outcome.error_code, "timeout");
    assert!(outcome.result.is_none());

    println!(
        "parity p12 reaped-callback: pass (job {} reaped while pending; one FAILED/timeout \
         outcome delivered)",
        parked.job_id
    );
    publisher.delete_queue(&queue).await.expect("queue removed");
    drop(daemon);
    db.dispose().await;
}

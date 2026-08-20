#![allow(deprecated)]
use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mm_domain::{MediaRef, OpSpec, ProbeSpec};
use mm_jobstore::MIGRATOR;
use mm_proto::v1;
use mm_proto::v1::media_job_service_client::MediaJobServiceClient;
use mm_storage::{CredentialSource, S3ObjectStore, S3Settings};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tonic::transport::Endpoint;
use uuid::Uuid;

const DEFAULT_ADMIN_URL: &str = "postgres://mm:mm@127.0.0.1:5433/mm";
const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:8333";
const DEFAULT_ACCESS_KEY: &str = "mm-test-access";
const DEFAULT_SECRET_KEY: &str = "mm-test-secret";
const BUCKET: &str = "mm-observability-it";
const SCRAPE_TIMEOUT: Duration = Duration::from_secs(5);

fn env_or(name: &str, fallback: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| fallback.to_string())
}

fn admin_url() -> String {
    env_or("MM_DATABASE_URL", DEFAULT_ADMIN_URL)
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
             before running the observability tests"
        );
    }
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

async fn test_store() -> S3ObjectStore {
    let store = S3ObjectStore::connect(S3Settings {
        endpoint: Some(env_or("MM_OBJECTSTORE_ENDPOINT", DEFAULT_ENDPOINT)),
        region: Some("us-east-1".to_string()),
        force_path_style: true,
        credentials: CredentialSource::Static {
            access_key_id: env_or("MM_OBJECTSTORE_ACCESS_KEY", DEFAULT_ACCESS_KEY),
            secret_access_key: env_or("MM_OBJECTSTORE_SECRET_KEY", DEFAULT_SECRET_KEY),
        },
    })
    .await;
    store.ensure_bucket(BUCKET).await.expect("bucket exists");
    store
}

struct Daemon {
    child: Child,
    lines: Arc<std::sync::Mutex<Vec<String>>>,
    announced: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Daemon {
    fn records(&self) -> Vec<String> {
        self.lines
            .lock()
            .map(|lines| lines.clone())
            .unwrap_or_default()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_lines<R: std::io::Read + Send + 'static>(reader: R) -> Arc<std::sync::Mutex<Vec<String>>> {
    let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&lines);
    std::thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        while let Ok(count) = reader.read_line(&mut line) {
            if count == 0 {
                break;
            }
            if let Ok(mut sink) = sink.lock() {
                sink.push(line.trim_end().to_string());
            }
            line.clear();
        }
    });
    lines
}

fn spawn_combined(database_url: &str, interval: u64) -> Daemon {
    let staging =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scratch/s16/staging");
    let mut child = Command::new(env!("CARGO_BIN_EXE_mmrpcd"))
        .arg("combined")
        .env("MM_BIND_ADDR", "127.0.0.1:0")
        .env("MM_DATABASE_URL", database_url)
        .env(
            "MM_OBJECTSTORE_ENDPOINT",
            env_or("MM_OBJECTSTORE_ENDPOINT", DEFAULT_ENDPOINT),
        )
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
        .env("MM_WORKER_ID", "observability-worker")
        .env("MM_WORKER_STAGING_DIR", staging)
        .env("MM_WORKER_RECONCILE_SECS", "30")
        .env("MM_METRICS_INTERVAL_SECS", interval.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("daemon spawns");
    let stderr = child.stderr.take().expect("stderr is piped");
    let stdout = child.stdout.take().expect("stdout is piped");
    let lines = read_lines(stderr);
    let announced = read_lines(stdout);
    Daemon {
        child,
        lines,
        announced,
    }
}

async fn announced(daemon: &Daemon, prefix: &str) -> SocketAddr {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let seen = daemon
            .announced
            .lock()
            .map(|lines| lines.clone())
            .unwrap_or_default();
        if let Some(address) = seen.iter().find_map(|line| line.strip_prefix(prefix)) {
            return address.parse().expect("announced address parses");
        }
        assert!(
            Instant::now() < deadline,
            "the daemon never announced {prefix}{seen:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn scrape(address: SocketAddr) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::time::timeout(SCRAPE_TIMEOUT, tokio::net::TcpStream::connect(address))
        .await
        .expect("the metrics endpoint answers within the connect budget")
        .expect("the metrics endpoint accepts a connection");
    tokio::time::timeout(
        SCRAPE_TIMEOUT,
        stream.write_all(b"GET /metrics HTTP/1.1\r\nhost: local\r\nconnection: close\r\n\r\n"),
    )
    .await
    .expect("the scrape request is written within the budget")
    .expect("the scrape request is written");
    let mut body = Vec::new();
    tokio::time::timeout(SCRAPE_TIMEOUT, stream.read_to_end(&mut body))
        .await
        .expect("the scrape answer arrives within the read budget")
        .expect("the scrape answer is read");
    String::from_utf8_lossy(&body).to_string()
}

fn series_value(body: &str, series: &str) -> Option<u64> {
    body.lines()
        .find(|line| line.starts_with(&format!("{series} ")))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
}

async fn wait_for_series(address: SocketAddr, series: &str, wanted: bool) -> (u64, String) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let body = scrape(address).await;
        let observed = series_value(&body, series).unwrap_or_default();
        if (observed > 0) == wanted {
            return (observed, body);
        }
        assert!(
            Instant::now() < deadline,
            "{series} never became nonzero: {body}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn one_job_is_traced_across_the_server_and_the_worker_and_its_series_are_scrapeable() {
    let db = fresh_db().await;
    let _store = test_store().await;
    let daemon = spawn_combined(&db.url, 1);
    let address = announced(&daemon, "listening ").await;
    let metrics = announced(&daemon, "metrics ").await;
    assert_ne!(metrics.port(), 0);
    let channel = Endpoint::from_shared(format!("http://{address}"))
        .expect("valid endpoint")
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(10))
        .connect()
        .await
        .expect("dial announced address");
    let mut client = MediaJobServiceClient::new(channel);

    let trace = Uuid::now_v7().to_string();
    let uri = format!("s3://{BUCKET}/inputs/observability.mp4");
    let mut request = tonic::Request::new(v1::CreateJobRequest {
        user_id: "owner-1".to_string(),
        idempotency_key: Uuid::now_v7().to_string(),
        spec: Some(v1::OpSpec::from(&OpSpec::Probe(ProbeSpec::new(
            MediaRef::object_uri(&uri).expect("valid ref"),
        )))),
        callback: None,
    });
    request
        .metadata_mut()
        .insert("x-trace-id", trace.parse().expect("metadata value"));
    let created = client
        .create_job(request)
        .await
        .expect("create succeeds")
        .into_inner();

    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let job = client
            .get_job(v1::GetJobRequest {
                user_id: "owner-1".to_string(),
                job_id: created.job_id.clone(),
            })
            .await
            .expect("get succeeds")
            .into_inner()
            .job
            .expect("job present");
        if job.state() != v1::JobState::Pending {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the job never left the queue: {:?}",
            job.state()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let (started, body) = wait_for_series(metrics, "mm_jobs_started_total", true).await;
    assert!(started >= 1, "no job was counted as started: {body}");
    let (finished, body) = wait_for_series(metrics, "mm_jobs_finished_total", true).await;
    assert!(finished >= 1, "no job was counted as finished: {body}");
    let (claimed, body) = wait_for_series(metrics, "mm_claim_latency_seconds_count", true).await;
    assert!(claimed >= 1, "claim latency was never recorded: {body}");
    let (duration, body) = wait_for_series(metrics, "mm_job_duration_seconds_count", true).await;
    assert!(duration >= 1, "job duration was never recorded: {body}");

    let records = daemon.records();
    let server_side: Vec<&String> = records
        .iter()
        .filter(|line| {
            line.contains(&format!("trace={trace}")) && line.contains("component=server")
        })
        .collect();
    let worker_side: Vec<&String> = records
        .iter()
        .filter(|line| {
            line.contains(&format!("trace={trace}")) && line.contains("component=worker")
        })
        .collect();
    assert!(
        !server_side.is_empty(),
        "no server record carried the trace: {records:?}"
    );
    assert!(
        !worker_side.is_empty(),
        "no worker record carried the trace: {records:?}"
    );
    assert!(
        worker_side
            .iter()
            .any(|line| line.contains(&format!("job={}", created.job_id))),
        "the worker record does not name the job: {worker_side:?}"
    );

    drop(daemon);
    db.dispose().await;
}

#[tokio::test]
async fn the_scaler_inputs_are_both_nonzero_under_a_synthetic_backlog() {
    let db = fresh_db().await;
    let _store = test_store().await;
    let daemon = spawn_combined(&db.url, 1);
    let address = announced(&daemon, "listening ").await;
    let metrics = announced(&daemon, "metrics ").await;
    let channel = Endpoint::from_shared(format!("http://{address}"))
        .expect("valid endpoint")
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(10))
        .connect()
        .await
        .expect("dial announced address");
    let mut client = MediaJobServiceClient::new(channel);

    let uri = format!("s3://{BUCKET}/inputs/backlog.mp4");
    for _ in 0..3 {
        client
            .create_job(v1::CreateJobRequest {
                user_id: "owner-1".to_string(),
                idempotency_key: Uuid::now_v7().to_string(),
                spec: Some(v1::OpSpec::from(&OpSpec::Probe(ProbeSpec::new(
                    MediaRef::object_uri(&uri).expect("valid ref"),
                )))),
                callback: None,
            })
            .await
            .expect("create succeeds");
    }

    let (pending, body) = wait_for_series(metrics, "mm_jobs_pending", true).await;
    assert!(pending >= 1, "the backlog did not show as pending: {body}");
    let (claimed, body) = wait_for_series(metrics, "mm_claim_latency_seconds_count", true).await;
    assert!(claimed >= 1, "claim lag was never recorded: {body}");
    assert!(
        body.contains(r#"mm_claim_latency_seconds_bucket{le="+Inf"} "#),
        "the claim lag histogram has no series: {body}"
    );
    assert!(
        body.contains("mm_jobs_pending ") && body.contains("mm_jobs_started_total "),
        "a documented scaler input is missing from the scrape: {body}"
    );

    drop(daemon);
    db.dispose().await;
}

use std::env;
use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use mm_domain::{
    MediaRef, ObjectBody, ObjectRef, ObjectStore, ObjectStoreError, OpSpec, ProbeSpec,
};
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
const BUCKET: &str = "mm-server-it";

fn env_or(name: &str, fallback: &str) -> String {
    env::var(name).unwrap_or_else(|_| fallback.to_string())
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
             before running mmrpcd integration tests"
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

fn fixture(name: &str, seconds: u32) -> PathBuf {
    static GENERATION: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = GENERATION.lock().unwrap();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scratch/s12/fixtures");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    if path.is_file() && path.metadata().unwrap().len() > 0 {
        return path;
    }
    let output = Command::new("ffmpeg")
        .arg("-y")
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg(format!("testsrc2=duration={seconds}:size=640x360:rate=24"))
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
        .arg("128k")
        .arg("-movflags")
        .arg("+faststart")
        .arg("-f")
        .arg("mp4")
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

fn spawn_combined(database_url: &str) -> Daemon {
    let staging = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scratch/s08/staging");
    let child = Command::new(env!("CARGO_BIN_EXE_mmrpcd"))
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
        .env("MM_WORKER_ID", "combined-worker")
        .env("MM_WORKER_STAGING_DIR", staging)
        .env("MM_WORKER_RECONCILE_SECS", "30")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("daemon spawns");
    Daemon { child }
}

async fn announced_address(daemon: &mut Daemon) -> SocketAddr {
    let stdout = daemon.child.stdout.take().expect("stdout is piped");
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
    })
    .await
    .expect("announce reader finishes")
}

#[tokio::test]
async fn the_combined_daemon_serves_and_works_jobs() {
    let db = fresh_db().await;
    let store = test_store().await;
    let source = fixture("combined-probe-3s.mp4", 3);
    let uri = upload_input(&store, &source, "inputs/combined-probe-3s.mp4").await;
    let mut daemon = spawn_combined(&db.url);
    let address = announced_address(&mut daemon).await;

    let channel = Endpoint::from_shared(format!("http://{address}"))
        .expect("valid endpoint")
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .connect()
        .await
        .expect("dial announced address");
    let mut client = MediaJobServiceClient::new(channel);

    let spec = v1::OpSpec::from(&OpSpec::Probe(ProbeSpec::new(
        MediaRef::object_uri(&uri).expect("valid ref"),
    )));
    let created = client
        .create_job(v1::CreateJobRequest {
            user_id: "owner-1".to_string(),
            idempotency_key: "req-combined".to_string(),
            spec: Some(spec),
            callback: None,
        })
        .await
        .expect("create succeeds")
        .into_inner();
    let job_id = created.job_id;

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let job = client
            .get_job(v1::GetJobRequest {
                user_id: "owner-1".to_string(),
                job_id: job_id.clone(),
            })
            .await
            .expect("get succeeds")
            .into_inner()
            .job
            .expect("job present");
        if job.state() == v1::JobState::Succeeded {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the combined daemon never worked the job: {:?}",
            job.state()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(daemon);
    db.dispose().await;
}

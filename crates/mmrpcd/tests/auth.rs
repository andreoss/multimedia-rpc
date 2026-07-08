#![allow(deprecated)]
use std::env;
use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use mm_auth::testing::Issuer;
use mm_domain::{MediaRef, OpSpec, ProbeSpec};
use mm_jobstore::MIGRATOR;
use mm_proto::v1;
use mm_proto::v1::media_job_service_client::MediaJobServiceClient;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tonic::transport::Endpoint;
use tonic::{Code, Request};
use uuid::Uuid;

const DEFAULT_ADMIN_URL: &str = "postgres://mm:mm@127.0.0.1:5433/mm";
const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:8333";
const DEFAULT_ACCESS_KEY: &str = "mm-test-access";
const DEFAULT_SECRET_KEY: &str = "mm-test-secret";
const BUCKET: &str = "mm-auth-it";

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
             before running the auth tests"
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

struct Daemon {
    child: Child,
    output: Arc<std::sync::Mutex<String>>,
}

impl Daemon {
    fn output(&self) -> String {
        self.output
            .lock()
            .map(|text| text.clone())
            .unwrap_or_default()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn publish_key(document: &str) -> String {
    let dir = mm_testkit::scratch_path("auth");
    let path = dir.join("published.json");
    std::fs::write(&path, document).expect("key document written");
    path.to_string_lossy().to_string()
}

fn spawn_server(database_url: &str, jwks_file: &str) -> Daemon {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mmrpcd"))
        .arg("server")
        .env("MM_BIND_ADDR", "127.0.0.1:0")
        .env("MM_DATABASE_URL", database_url)
        .env(
            "MM_OBJECTSTORE_ENDPOINT",
            env_or("MM_OBJECTSTORE_ENDPOINT", DEFAULT_ENDPOINT),
        )
        .env("MM_OBJECTSTORE_BUCKET", BUCKET)
        .env(
            "MM_OBJECTSTORE_ACCESS_KEY",
            env_or("MM_OBJECTSTORE_ACCESS_KEY", DEFAULT_ACCESS_KEY),
        )
        .env(
            "MM_OBJECTSTORE_SECRET_KEY",
            env_or("MM_OBJECTSTORE_SECRET_KEY", DEFAULT_SECRET_KEY),
        )
        .env(
            "MM_OBJECTSTORE_REGION",
            env_or("MM_OBJECTSTORE_REGION", "us-east-1"),
        )
        .env("MM_AUTH_JWKS_FILE", jwks_file)
        .env("MM_AUTH_ISSUER", "test-issuer")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("daemon spawns");
    let stderr = child.stderr.take().expect("stderr is piped");
    let output = Arc::new(std::sync::Mutex::new(String::new()));
    let sink = Arc::clone(&output);
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if let Ok(mut sink) = sink.lock() {
                        sink.push_str(&line);
                    }
                }
            }
        }
    });
    Daemon { child, output }
}

async fn announced_address(daemon: &mut Daemon) -> SocketAddr {
    let stdout = daemon.child.stdout.take().expect("stdout is piped");
    tokio::task::spawn_blocking(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
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

fn probe_spec(uri: &str) -> v1::OpSpec {
    v1::OpSpec::from(&OpSpec::Probe(
        ProbeSpec::new(MediaRef::object_uri(uri).expect("valid ref"))
            .expect("a bare reference cuts nothing"),
    ))
}

fn live_token(issuer: &Issuer, subject: &str) -> String {
    issuer.token(subject, SystemTime::now(), Duration::from_secs(300))
}

#[tokio::test]
async fn a_job_is_scoped_by_the_token_subject_and_the_body_field_is_ignored() {
    let db = fresh_db().await;
    let issuer = Issuer::seeded("key-1", 7);
    let jwks_file = publish_key(&issuer.jwks());
    let mut daemon = spawn_server(&db.url, &jwks_file);
    let address = announced_address(&mut daemon).await;
    let channel = Endpoint::from_shared(format!("http://{address}"))
        .expect("valid endpoint")
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(10))
        .connect()
        .await
        .expect("dial announced address");
    let mut client = MediaJobServiceClient::new(channel);

    let spec = probe_spec("s3://mm-auth-it/inputs/probe.mp4");
    let mut request = Request::new(v1::CreateJobRequest {
        user_id: "body-owner".to_string(),
        idempotency_key: Uuid::now_v7().to_string(),
        spec: Some(spec),
        callback: None,
    });
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", live_token(&issuer, "subject-owner"))
            .parse()
            .expect("metadata value"),
    );
    let created = client
        .create_job(request)
        .await
        .expect("the token is accepted")
        .into_inner();
    assert!(created.created);

    let owner_view = get_job(
        &mut client,
        live_token(&issuer, "subject-owner"),
        &created.job_id,
    )
    .await
    .expect("the owner reads its own job")
    .into_inner()
    .job
    .expect("job present");
    assert_eq!(owner_view.user_id, "subject-owner");

    let other = get_job(
        &mut client,
        live_token(&issuer, "other-owner"),
        &created.job_id,
    )
    .await
    .expect_err("another subject reads the job");
    assert_eq!(other.code(), Code::NotFound);

    drop(daemon);
    db.dispose().await;
}

#[tokio::test]
async fn a_server_with_a_published_key_refuses_every_unverified_call() {
    let db = fresh_db().await;
    let issuer = Issuer::seeded("key-1", 7);
    let other = Issuer::seeded("key-2", 7);
    let jwks_file = publish_key(&issuer.jwks());
    let mut daemon = spawn_server(&db.url, &jwks_file);
    let address = announced_address(&mut daemon).await;
    let channel = Endpoint::from_shared(format!("http://{address}"))
        .expect("valid endpoint")
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(10))
        .connect()
        .await
        .expect("dial announced address");
    let mut client = MediaJobServiceClient::new(channel);

    let expired = issuer.token(
        "subject-owner",
        SystemTime::now() - Duration::from_secs(3600),
        Duration::from_secs(60),
    );
    let future = issuer.token(
        "subject-owner",
        SystemTime::now() + Duration::from_secs(3600),
        Duration::from_secs(300),
    );
    let symmetric =
        issuer.symmetric_token("subject-owner", SystemTime::now(), Duration::from_secs(300));
    let refused = [
        None,
        Some(String::new()),
        Some(mint_from(&other, "subject-owner")),
        Some(symmetric),
        Some(expired),
        Some(future),
        Some(format!("Basic {}", live_token(&issuer, "subject-owner"))),
    ];

    for token in refused {
        let mut request = Request::new(v1::ListJobsRequest {
            user_id: "body-owner".to_string(),
            ..Default::default()
        });
        if let Some(token) = token {
            request.metadata_mut().insert(
                "authorization",
                format!("Bearer {token}").parse().expect("metadata value"),
            );
        }
        let status = client
            .list_jobs(request)
            .await
            .map(|response| response.into_inner())
            .expect_err("an unverified call is answered")
            .code();
        if status != Code::Unauthenticated {
            panic!(
                "an unverified call answered {status:?}\ndaemon output:\n{}",
                daemon.output()
            );
        }
    }

    drop(daemon);
    db.dispose().await;
}

fn mint_from(issuer: &Issuer, subject: &str) -> String {
    issuer.token(subject, SystemTime::now(), Duration::from_secs(300))
}

async fn get_job(
    client: &mut MediaJobServiceClient<tonic::transport::Channel>,
    token: String,
    job_id: &str,
) -> Result<tonic::Response<v1::GetJobResponse>, tonic::Status> {
    let mut request = Request::new(v1::GetJobRequest {
        user_id: "body-owner".to_string(),
        job_id: job_id.to_string(),
    });
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {token}").parse().expect("metadata value"),
    );
    client.get_job(request).await
}

use std::env;
use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use mm_jobstore::MIGRATOR;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tonic::transport::Endpoint;
use tonic_health::pb::HealthCheckRequest;
use tonic_health::pb::health_check_response::ServingStatus;
use tonic_health::pb::health_client::HealthClient;
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

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_server(database_url: &str) -> Daemon {
    let child = Command::new(env!("CARGO_BIN_EXE_mmrpcd"))
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
        let mut line = String::new();
        reader.read_line(&mut line).expect("announce line readable");
        line.trim()
            .strip_prefix("listening ")
            .unwrap_or_else(|| panic!("unexpected announce line: {line:?}"))
            .parse()
            .expect("announced address parses")
    })
    .await
    .expect("announce reader finishes")
}

#[tokio::test]
async fn the_daemon_announces_its_address_and_reports_health() {
    let db = fresh_db().await;
    let mut daemon = spawn_server(&db.url);
    let address = announced_address(&mut daemon).await;
    assert_ne!(address.port(), 0);
    let channel = Endpoint::from_shared(format!("http://{address}"))
        .expect("valid endpoint")
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(2))
        .connect()
        .await
        .expect("dial announced address");
    let mut client = HealthClient::new(channel);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let observed = client
            .check(HealthCheckRequest {
                service: String::new(),
            })
            .await
            .map(|response| response.into_inner().status())
            .map_err(|status| status.code());
        if observed == Ok(ServingStatus::Serving) {
            break;
        }
        if Instant::now() > deadline {
            panic!("daemon never became serving: {observed:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    drop(daemon);
    db.dispose().await;
}

#[test]
fn a_missing_database_url_fails_fast() {
    let output = Command::new(env!("CARGO_BIN_EXE_mmrpcd"))
        .arg("server")
        .env_remove("MM_DATABASE_URL")
        .env("MM_OBJECTSTORE_ENDPOINT", DEFAULT_ENDPOINT)
        .env("MM_OBJECTSTORE_BUCKET", BUCKET)
        .stdin(Stdio::null())
        .output()
        .expect("daemon runs");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("MM_DATABASE_URL"));
}

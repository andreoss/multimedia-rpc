use std::env;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mm_domain::{JobRepository, ObjectStore};
use mm_ffmpeg::Prober;
use mm_jobstore::{MIGRATOR, PgJobRepository};
use mm_server::{ServerConfig, ServerHandle};
use mm_storage::{CredentialSource, S3ObjectStore, S3Settings};
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
const CLOSED_ENDPOINT: &str = "http://127.0.0.1:1";

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
             before running mm-server integration tests"
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
    TestDb { admin, pool, name }
}

impl TestDb {
    fn repository(&self) -> Arc<dyn JobRepository> {
        Arc::new(PgJobRepository::new(self.pool.clone()))
    }

    async fn drop_hard(&self) {
        self.pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE {} WITH (FORCE)",
            self.name
        )))
        .execute(&self.admin)
        .await
        .expect("drop test database");
    }

    async fn dispose(self) {
        self.drop_hard().await;
    }
}

async fn store_at(endpoint: &str) -> Arc<dyn ObjectStore> {
    Arc::new(
        S3ObjectStore::connect(S3Settings {
            endpoint: Some(endpoint.to_string()),
            region: Some("us-east-1".to_string()),
            force_path_style: true,
            credentials: CredentialSource::Static {
                access_key_id: env_or("MM_OBJECTSTORE_ACCESS_KEY", DEFAULT_ACCESS_KEY),
                secret_access_key: env_or("MM_OBJECTSTORE_SECRET_KEY", DEFAULT_SECRET_KEY),
            },
        })
        .await,
    )
}

fn test_config(
    repository: Arc<dyn JobRepository>,
    store: Arc<dyn ObjectStore>,
    prober: Prober,
) -> ServerConfig {
    let mut config = ServerConfig::new(repository, store, BUCKET.to_string(), prober)
        .expect("valid configuration");
    config.health_interval = Duration::from_millis(100);
    config.check_timeout = Duration::from_secs(2);
    config
}

async fn start_server(config: ServerConfig) -> ServerHandle {
    mm_server::start(config).await.expect("server starts")
}

async fn health_client(handle: &ServerHandle) -> HealthClient<tonic::transport::Channel> {
    let channel = Endpoint::from_shared(format!("http://{}", handle.local_addr()))
        .expect("valid endpoint")
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(2))
        .connect()
        .await
        .expect("dial announced address");
    HealthClient::new(channel)
}

async fn await_status(
    client: &mut HealthClient<tonic::transport::Channel>,
    service: &str,
    wanted: ServingStatus,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let observed = client
            .check(HealthCheckRequest {
                service: service.to_string(),
            })
            .await
            .map(|response| response.into_inner().status())
            .map_err(|status| status.code());
        if observed == Ok(wanted) {
            return;
        }
        if Instant::now() > deadline {
            panic!("health status for {service:?} never became {wanted:?}: {observed:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn the_server_announces_a_port_zero_address_that_serves_health() {
    let db = fresh_db().await;
    let store = store_at(&store_endpoint()).await;
    let handle = start_server(test_config(db.repository(), store, Prober::default())).await;
    assert_ne!(handle.local_addr().port(), 0);
    let mut client = health_client(&handle).await;
    await_status(&mut client, "", ServingStatus::Serving).await;
    await_status(
        &mut client,
        "mmrpc.v1.MediaJobService",
        ServingStatus::Serving,
    )
    .await;
    handle.shutdown().await;
    db.dispose().await;
}

#[tokio::test]
async fn an_unreachable_object_store_reports_not_serving() {
    let db = fresh_db().await;
    let store = store_at(CLOSED_ENDPOINT).await;
    let handle = start_server(test_config(db.repository(), store, Prober::default())).await;
    let mut client = health_client(&handle).await;
    await_status(&mut client, "", ServingStatus::NotServing).await;
    handle.shutdown().await;
    db.dispose().await;
}

#[tokio::test]
async fn a_missing_probe_binary_reports_not_serving() {
    let db = fresh_db().await;
    let store = store_at(&store_endpoint()).await;
    let prober = Prober::with_binary("mm-no-such-probe-binary");
    let handle = start_server(test_config(db.repository(), store, prober)).await;
    let mut client = health_client(&handle).await;
    await_status(&mut client, "", ServingStatus::NotServing).await;
    handle.shutdown().await;
    db.dispose().await;
}

#[tokio::test]
async fn a_dropped_database_flips_health_to_not_serving() {
    let db = fresh_db().await;
    let store = store_at(&store_endpoint()).await;
    let handle = start_server(test_config(db.repository(), store, Prober::default())).await;
    let mut client = health_client(&handle).await;
    await_status(&mut client, "", ServingStatus::Serving).await;
    db.drop_hard().await;
    await_status(&mut client, "", ServingStatus::NotServing).await;
    handle.shutdown().await;
}

#[tokio::test]
async fn an_empty_bucket_name_is_rejected_at_configuration() {
    let db = fresh_db().await;
    let store = store_at(&store_endpoint()).await;
    let result = ServerConfig::new(db.repository(), store, String::new(), Prober::default());
    assert!(result.is_err());
    db.dispose().await;
}

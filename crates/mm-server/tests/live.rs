use std::env;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mm_domain::{ConcatSpec, JobRepository, MediaRef, ObjectStore, OpSpec};
use mm_ffmpeg::Prober;
use mm_jobstore::{MIGRATOR, PgJobRepository};
use mm_proto::v1;
use mm_proto::v1::media_job_service_client::MediaJobServiceClient;
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

async fn job_client(handle: &ServerHandle) -> MediaJobServiceClient<tonic::transport::Channel> {
    let channel = Endpoint::from_shared(format!("http://{}", handle.local_addr()))
        .expect("valid endpoint")
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .connect()
        .await
        .expect("dial announced address");
    MediaJobServiceClient::new(channel)
}

fn concat_spec() -> v1::OpSpec {
    v1::OpSpec::from(&OpSpec::Concat(
        ConcatSpec::new(
            vec![
                MediaRef::object_uri("s3://media/in/a.mp4").expect("valid ref"),
                MediaRef::object_uri("s3://media/in/b.mp4").expect("valid ref"),
            ],
            None,
        )
        .expect("valid concat spec"),
    ))
}

fn create_request(user: &str, key: &str) -> v1::CreateJobRequest {
    v1::CreateJobRequest {
        user_id: user.to_string(),
        idempotency_key: key.to_string(),
        spec: Some(concat_spec()),
        callback: None,
    }
}

async fn healthy_server() -> (TestDb, ServerHandle) {
    let db = fresh_db().await;
    let store = store_at(&store_endpoint()).await;
    let handle = start_server(test_config(db.repository(), store, Prober::default())).await;
    (db, handle)
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

#[tokio::test]
async fn create_job_persists_a_pending_job_and_replays_with_created_false() {
    let (db, handle) = healthy_server().await;
    let mut client = job_client(&handle).await;
    let created = client
        .create_job(create_request("owner-1", "req-1"))
        .await
        .expect("create succeeds")
        .into_inner();
    assert!(created.created);
    assert_eq!(created.request_id, "req-1");
    let id: mm_domain::JobId = created.job_id.parse().expect("job id parses");
    let record = db
        .repository()
        .get(id)
        .await
        .expect("get succeeds")
        .expect("job persisted");
    assert_eq!(record.state, mm_domain::JobState::Pending);
    let replayed = client
        .create_job(create_request("owner-1", "req-1"))
        .await
        .expect("replay succeeds")
        .into_inner();
    assert!(!replayed.created);
    assert_eq!(replayed.job_id, created.job_id);
    assert_eq!(replayed.request_id, "req-1");
    handle.shutdown().await;
    db.dispose().await;
}

#[tokio::test]
async fn two_concurrent_duplicate_creates_yield_one_job() {
    let (db, handle) = healthy_server().await;
    let mut first = job_client(&handle).await;
    let mut second = job_client(&handle).await;
    let (a, b) = tokio::join!(
        first.create_job(create_request("owner-1", "req-race")),
        second.create_job(create_request("owner-1", "req-race"))
    );
    let a = a.expect("first create succeeds").into_inner();
    let b = b.expect("second create succeeds").into_inner();
    assert_eq!(a.job_id, b.job_id);
    assert_eq!(
        [a.created, b.created].iter().filter(|flag| **flag).count(),
        1
    );
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
        .fetch_one(&db.pool)
        .await
        .expect("count succeeds");
    assert_eq!(total, 1);
    handle.shutdown().await;
    db.dispose().await;
}

#[tokio::test]
async fn an_invalid_spec_is_rejected_with_the_typed_message() {
    let (db, handle) = healthy_server().await;
    let mut client = job_client(&handle).await;
    let mut request = create_request("owner-1", "req-1");
    request.spec = Some(v1::OpSpec {
        op: Some(v1::op_spec::Op::Concat(v1::ConcatSpec {
            inputs: vec![v1::MediaRef {
                r#ref: Some(v1::media_ref::Ref::ObjectUri(
                    "s3://media/in/only.mp4".to_string(),
                )),
            }],
            profile: None,
        })),
    });
    let status = client
        .create_job(request)
        .await
        .expect_err("invalid spec rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("concat"));
    let missing = client
        .create_job(v1::CreateJobRequest {
            spec: None,
            ..create_request("owner-1", "req-2")
        })
        .await
        .expect_err("missing spec rejected");
    assert_eq!(missing.code(), tonic::Code::InvalidArgument);
    handle.shutdown().await;
    db.dispose().await;
}

#[tokio::test]
async fn malformed_identifiers_are_invalid_argument() {
    let (db, handle) = healthy_server().await;
    let mut client = job_client(&handle).await;
    let bad_user = client
        .create_job(create_request("owner one", "req-1"))
        .await
        .expect_err("malformed user rejected");
    assert_eq!(bad_user.code(), tonic::Code::InvalidArgument);
    assert!(bad_user.message().contains("user_id"));
    let bad_key = client
        .create_job(create_request("owner-1", ""))
        .await
        .expect_err("empty key rejected");
    assert_eq!(bad_key.code(), tonic::Code::InvalidArgument);
    assert!(bad_key.message().contains("idempotency_key"));
    handle.shutdown().await;
    db.dispose().await;
}

#[tokio::test]
async fn a_foreign_owner_replay_is_already_exists() {
    let (db, handle) = healthy_server().await;
    let mut client = job_client(&handle).await;
    client
        .create_job(create_request("owner-1", "req-shared"))
        .await
        .expect("create succeeds");
    let status = client
        .create_job(create_request("owner-2", "req-shared"))
        .await
        .expect_err("foreign replay rejected");
    assert_eq!(status.code(), tonic::Code::AlreadyExists);
    handle.shutdown().await;
    db.dispose().await;
}

#[tokio::test]
async fn watch_job_is_unimplemented() {
    let (db, handle) = healthy_server().await;
    let mut client = job_client(&handle).await;
    let status = client
        .watch_job(v1::WatchJobRequest {
            user_id: "owner-1".to_string(),
            job_id: mm_domain::JobId::generate().to_string(),
        })
        .await
        .expect_err("watch is unimplemented");
    assert_eq!(status.code(), tonic::Code::Unimplemented);
    handle.shutdown().await;
    db.dispose().await;
}

#[tokio::test]
async fn get_job_returns_the_owned_job_with_its_spec() {
    let (db, handle) = healthy_server().await;
    let mut client = job_client(&handle).await;
    let created = client
        .create_job(create_request("owner-1", "req-1"))
        .await
        .expect("create succeeds")
        .into_inner();
    let fetched = client
        .get_job(v1::GetJobRequest {
            user_id: "owner-1".to_string(),
            job_id: created.job_id.clone(),
        })
        .await
        .expect("get succeeds")
        .into_inner()
        .job
        .expect("job present");
    assert_eq!(fetched.job_id, created.job_id);
    assert_eq!(fetched.user_id, "owner-1");
    assert_eq!(fetched.request_id, "req-1");
    assert_eq!(fetched.state, v1::JobState::Pending as i32);
    assert_eq!(fetched.spec, Some(concat_spec()));
    assert_eq!(fetched.progress_percent, 0);
    assert_eq!(fetched.result, None);
    handle.shutdown().await;
    db.dispose().await;
}

#[tokio::test]
async fn a_foreign_owner_cannot_distinguish_a_job_from_an_absent_one() {
    let (db, handle) = healthy_server().await;
    let mut client = job_client(&handle).await;
    let created = client
        .create_job(create_request("owner-1", "req-1"))
        .await
        .expect("create succeeds")
        .into_inner();
    let foreign = client
        .get_job(v1::GetJobRequest {
            user_id: "owner-2".to_string(),
            job_id: created.job_id.clone(),
        })
        .await
        .expect_err("foreign owner sees nothing");
    let absent = client
        .get_job(v1::GetJobRequest {
            user_id: "owner-2".to_string(),
            job_id: mm_domain::JobId::generate().to_string(),
        })
        .await
        .expect_err("absent job is not found");
    assert_eq!(foreign.code(), tonic::Code::NotFound);
    assert_eq!(absent.code(), tonic::Code::NotFound);
    assert_eq!(foreign.message(), absent.message());
    handle.shutdown().await;
    db.dispose().await;
}

#[tokio::test]
async fn a_malformed_job_id_is_invalid_argument_on_get() {
    let (db, handle) = healthy_server().await;
    let mut client = job_client(&handle).await;
    let status = client
        .get_job(v1::GetJobRequest {
            user_id: "owner-1".to_string(),
            job_id: "not-a-job".to_string(),
        })
        .await
        .expect_err("malformed id rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("job_id"));
    handle.shutdown().await;
    db.dispose().await;
}

async fn create_job_id(
    client: &mut MediaJobServiceClient<tonic::transport::Channel>,
    user: &str,
    key: &str,
) -> String {
    client
        .create_job(create_request(user, key))
        .await
        .expect("create succeeds")
        .into_inner()
        .job_id
}

async fn cancel(
    client: &mut MediaJobServiceClient<tonic::transport::Channel>,
    user: &str,
    job_id: &str,
) -> v1::CancelJobResponse {
    client
        .cancel_job(v1::CancelJobRequest {
            user_id: user.to_string(),
            job_id: job_id.to_string(),
        })
        .await
        .expect("cancel succeeds")
        .into_inner()
}

#[tokio::test]
async fn cancelling_a_pending_job_is_immediate() {
    let (db, handle) = healthy_server().await;
    let mut client = job_client(&handle).await;
    let job_id = create_job_id(&mut client, "owner-1", "req-1").await;
    let response = cancel(&mut client, "owner-1", &job_id).await;
    assert!(response.cancellation_accepted);
    let job = response.job.expect("job present");
    assert_eq!(job.state, v1::JobState::Cancelled as i32);
    handle.shutdown().await;
    db.dispose().await;
}

#[tokio::test]
async fn cancelling_a_running_job_requests_cancellation() {
    let (db, handle) = healthy_server().await;
    let mut client = job_client(&handle).await;
    let job_id = create_job_id(&mut client, "owner-1", "req-1").await;
    let repo = db.repository();
    let worker: mm_domain::WorkerId = "worker-a".parse().expect("valid worker id");
    let claimed = repo
        .claim(
            &worker,
            std::num::NonZeroU32::new(1).expect("nonzero"),
            Duration::from_secs(60),
        )
        .await
        .expect("claim succeeds");
    assert_eq!(claimed.len(), 1);
    let response = cancel(&mut client, "owner-1", &job_id).await;
    assert!(response.cancellation_accepted);
    let job = response.job.expect("job present");
    assert_eq!(job.state, v1::JobState::Running as i32);
    let record = repo
        .get(job_id.parse().expect("job id parses"))
        .await
        .expect("get succeeds")
        .expect("job exists");
    assert!(record.cancel_requested);
    assert_eq!(record.state, mm_domain::JobState::Running);
    handle.shutdown().await;
    db.dispose().await;
}

#[tokio::test]
async fn cancelling_a_terminal_job_returns_it_unchanged() {
    let (db, handle) = healthy_server().await;
    let mut client = job_client(&handle).await;
    let cancelled_id = create_job_id(&mut client, "owner-1", "req-1").await;
    cancel(&mut client, "owner-1", &cancelled_id).await;
    let again = cancel(&mut client, "owner-1", &cancelled_id).await;
    assert!(!again.cancellation_accepted);
    assert_eq!(
        again.job.expect("job present").state,
        v1::JobState::Cancelled as i32
    );

    let succeeded_id = create_job_id(&mut client, "owner-1", "req-2").await;
    let repo = db.repository();
    let worker: mm_domain::WorkerId = "worker-a".parse().expect("valid worker id");
    repo.claim(
        &worker,
        std::num::NonZeroU32::new(1).expect("nonzero"),
        Duration::from_secs(60),
    )
    .await
    .expect("claim succeeds");
    let result = mm_domain::OpResult::Concat(
        mm_domain::TimedOutput::new(
            MediaRef::object_uri("s3://media/jobs/x/out.mp4").expect("valid ref"),
            10.0,
        )
        .expect("valid result"),
    );
    repo.mark_succeeded(
        succeeded_id.parse().expect("job id parses"),
        &worker,
        result,
    )
    .await
    .expect("succeed succeeds");
    let response = cancel(&mut client, "owner-1", &succeeded_id).await;
    assert!(!response.cancellation_accepted);
    assert_eq!(
        response.job.expect("job present").state,
        v1::JobState::Succeeded as i32
    );
    handle.shutdown().await;
    db.dispose().await;
}

#[tokio::test]
async fn cancel_is_owner_scoped() {
    let (db, handle) = healthy_server().await;
    let mut client = job_client(&handle).await;
    let job_id = create_job_id(&mut client, "owner-1", "req-1").await;
    let status = client
        .cancel_job(v1::CancelJobRequest {
            user_id: "owner-2".to_string(),
            job_id,
        })
        .await
        .expect_err("foreign cancel is not found");
    assert_eq!(status.code(), tonic::Code::NotFound);
    handle.shutdown().await;
    db.dispose().await;
}

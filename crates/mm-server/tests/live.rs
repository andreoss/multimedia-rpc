#![allow(deprecated)]
use std::env;
use std::net::{TcpStream, ToSocketAddrs};
use std::num::NonZeroU32;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mm_domain::{
    ConcatSpec, InputSource, JobId, JobRepository, MediaRef, ObjectBody, ObjectRef, ObjectStore,
    ObjectStoreError, OpSpec, OutputProfile, ParameterName, ProgressPercent, TraceId,
    TranscodeSpec, WorkerId, body_from_bytes,
};
use mm_ffmpeg::Prober;
use mm_jobstore::{MIGRATOR, PgJobEventListener, PgJobRepository};
use mm_proto::v1;
use mm_proto::v1::media_job_service_client::MediaJobServiceClient;
use mm_proto::vm::v1 as vm;
use mm_server::{ServerConfig, ServerHandle};
use mm_storage::{CredentialSource, S3ObjectStore, S3Settings};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tonic::Request;
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

struct StallingStore {
    inner: Arc<dyn ObjectStore>,
    stall: Duration,
}

#[async_trait::async_trait]
impl ObjectStore for StallingStore {
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
        self.inner.put_multipart(object, body).await
    }

    async fn head(&self, object: &ObjectRef) -> Result<u64, ObjectStoreError> {
        tokio::time::sleep(self.stall).await;
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
            ]
            .into_iter()
            .map(mm_domain::InputSource::from)
            .collect(),
            None,
        )
        .expect("valid concat spec"),
    ))
}

fn cut_transcode_spec(
    trim: Option<v1::TrimRange>,
    speed: Option<f64>,
    volume: Option<f64>,
) -> v1::OpSpec {
    v1::OpSpec {
        op: Some(v1::op_spec::Op::Transcode(v1::TranscodeSpec {
            input: Some(v1::InputSource {
                source: Some(v1::input_source::Source::Media(v1::MediaSource {
                    media: Some(v1::MediaRef::from(
                        &MediaRef::object_uri("s3://media/in/a.mp4").expect("valid ref"),
                    )),
                    trim,
                    speed,
                    volume,
                })),
            }),
            profile: Some(v1::OutputProfile {
                frame_rate: 24.0,
                ..Default::default()
            }),
        })),
    }
}

fn create_request(user: &str, key: &str) -> v1::CreateJobRequest {
    v1::CreateJobRequest {
        user_id: user.to_string(),
        idempotency_key: key.to_string(),
        spec: Some(concat_spec()),
        callback: None,
    }
}

async fn provision_media() {
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
    store.ensure_bucket("media").await.expect("media bucket");
    for key in ["in/a.mp4", "in/b.mp4"] {
        let object = ObjectRef::new("media", key).expect("valid input ref");
        store
            .put(&object, body_from_bytes(vec![0u8; 8]), 8)
            .await
            .expect("input object lands");
    }
}

async fn healthy_server() -> (TestDb, ServerHandle) {
    let db = fresh_db().await;
    provision_media().await;
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
async fn an_answer_given_before_the_first_check_is_not_serving() {
    let db = fresh_db().await;
    let store = Arc::new(StallingStore {
        inner: store_at(&store_endpoint()).await,
        stall: Duration::from_secs(30),
    });
    let handle = start_server(test_config(db.repository(), store, Prober::default())).await;
    let mut client = health_client(&handle).await;
    let observed = client
        .check(HealthCheckRequest {
            service: String::new(),
        })
        .await
        .map(|response| response.into_inner().status())
        .map_err(|status| status.code());
    assert_eq!(observed, Ok(ServingStatus::NotServing));
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
            inputs: vec![v1::InputSource::from(&mm_domain::InputSource::from(
                MediaRef::object_uri("s3://media/in/only.mp4").expect("valid ref"),
            ))],
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
async fn trim_and_thumbnail_specs_are_accepted_at_create() {
    let (db, handle) = healthy_server().await;
    let mut client = job_client(&handle).await;
    let input = MediaRef::object_uri("s3://media/in/a.mp4").expect("valid ref");
    let specs = vec![
        v1::OpSpec::from(&OpSpec::Trim(
            mm_domain::TrimSpec::new(input.clone(), 1.0, 2.0).expect("valid trim spec"),
        )),
        v1::OpSpec::from(&OpSpec::Thumbnail(
            mm_domain::ThumbnailSpec::new(input, vec![1.0], 320, 180)
                .expect("valid thumbnail spec"),
        )),
    ];
    for (index, spec) in specs.into_iter().enumerate() {
        let mut request = create_request("owner-1", &format!("req-multimedia-{index}"));
        request.spec = Some(spec);

        let created = client
            .create_job(request)
            .await
            .expect("the spec is accepted")
            .into_inner();

        assert!(created.created);
    }
    handle.shutdown().await;
    db.dispose().await;
}

#[tokio::test]
async fn probe_and_transcode_specs_are_accepted_at_create() {
    let (db, handle) = healthy_server().await;
    let mut client = job_client(&handle).await;
    let input = MediaRef::object_uri("s3://media/in/a.mp4").expect("valid ref");
    let profile =
        mm_domain::OutputProfile::from_raw(24.0, 0, 0, 0, 48_000, 640, 360).expect("valid profile");
    let specs = vec![
        v1::OpSpec::from(&OpSpec::Probe(
            mm_domain::ProbeSpec::new(input.clone()).expect("a bare reference cuts nothing"),
        )),
        v1::OpSpec::from(&OpSpec::Transcode(
            mm_domain::TranscodeSpec::new(input.clone(), profile).expect("valid transcode spec"),
        )),
    ];
    for (index, spec) in specs.into_iter().enumerate() {
        let mut request = create_request("owner-1", &format!("req-supported-{index}"));
        request.spec = Some(spec);
        let created = client
            .create_job(request)
            .await
            .expect("create succeeds")
            .into_inner();
        assert!(created.created);
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
        assert_eq!(job.state(), v1::JobState::Pending);
    }
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

async fn watch_client(handle: &ServerHandle) -> MediaJobServiceClient<tonic::transport::Channel> {
    let channel = Endpoint::from_shared(format!("http://{}", handle.local_addr()))
        .expect("valid endpoint")
        .connect_timeout(Duration::from_secs(2))
        .connect()
        .await
        .expect("dial announced address");
    MediaJobServiceClient::new(channel)
}

async fn next_event(
    stream: &mut tonic::codec::Streaming<v1::WatchJobResponse>,
) -> Option<v1::job_event::Kind> {
    tokio::time::timeout(mm_testkit::budget::EVENT_ARRIVAL, stream.message())
        .await
        .expect("an event arrives within the budget")
        .expect("stream stays healthy")
        .map(|response| {
            response
                .event
                .expect("event present")
                .kind
                .expect("kind present")
        })
}

fn state_kind(state: v1::JobState) -> v1::job_event::Kind {
    v1::job_event::Kind::State(state as i32)
}

fn progress_kind(value: u32) -> v1::job_event::Kind {
    v1::job_event::Kind::ProgressPercent(value)
}

fn concat_result() -> mm_domain::OpResult {
    mm_domain::OpResult::Concat(
        mm_domain::TimedOutput::new(
            MediaRef::object_uri("s3://media/jobs/x/out.mp4").expect("valid ref"),
            10.0,
        )
        .expect("valid result"),
    )
}

#[tokio::test]
async fn a_callback_is_persisted_and_echoed_on_reads() {
    let (db, handle) = healthy_server().await;
    let mut client = job_client(&handle).await;
    let mut request = create_request("owner-1", "req-callback-echo");
    request.callback = Some(v1::Callback {
        queue_url: "http://127.0.0.1:9324/queue/results".to_string(),
    });
    let created = client
        .create_job(request)
        .await
        .expect("create succeeds")
        .into_inner();
    let fetched = client
        .get_job(v1::GetJobRequest {
            user_id: "owner-1".to_string(),
            job_id: created.job_id,
        })
        .await
        .expect("get succeeds")
        .into_inner()
        .job
        .expect("job present");
    assert_eq!(
        fetched.callback,
        Some(v1::Callback {
            queue_url: "http://127.0.0.1:9324/queue/results".to_string(),
        })
    );
    handle.shutdown().await;
    db.dispose().await;
}

#[tokio::test]
async fn a_malformed_callback_queue_url_is_rejected() {
    let (db, handle) = healthy_server().await;
    let mut client = job_client(&handle).await;
    let mut request = create_request("owner-1", "req-callback-bad");
    request.callback = Some(v1::Callback {
        queue_url: "not-a-queue".to_string(),
    });
    let status = client
        .create_job(request)
        .await
        .expect_err("malformed callback rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    handle.shutdown().await;
    db.dispose().await;
}

#[tokio::test]
async fn watching_a_foreign_job_answers_not_found_before_streaming() {
    let (db, handle) = healthy_server().await;
    let mut client = job_client(&handle).await;
    let created = client
        .create_job(create_request("owner-1", "req-watch-foreign"))
        .await
        .expect("create succeeds")
        .into_inner();
    let status = client
        .watch_job(v1::WatchJobRequest {
            user_id: "owner-2".to_string(),
            job_id: created.job_id,
        })
        .await
        .expect_err("foreign watch rejected");
    assert_eq!(status.code(), tonic::Code::NotFound);
    let absent = client
        .watch_job(v1::WatchJobRequest {
            user_id: "owner-2".to_string(),
            job_id: mm_domain::JobId::generate().to_string(),
        })
        .await
        .expect_err("absent watch rejected");
    assert_eq!(absent.message(), status.message());
    handle.shutdown().await;
    db.dispose().await;
}

#[tokio::test]
async fn watching_a_terminal_job_yields_the_terminal_snapshot_and_closes() {
    let (db, handle) = healthy_server().await;
    let repository = db.repository();
    let mut client = watch_client(&handle).await;
    let created = client
        .create_job(create_request("owner-1", "req-watch-terminal"))
        .await
        .expect("create succeeds")
        .into_inner();
    let id = mm_domain::JobId::from_str(&created.job_id).expect("job id");
    repository.mark_cancelled(id).await.expect("job cancelled");
    let mut stream = client
        .watch_job(v1::WatchJobRequest {
            user_id: "owner-1".to_string(),
            job_id: created.job_id,
        })
        .await
        .expect("watch starts")
        .into_inner();
    assert_eq!(
        next_event(&mut stream).await,
        Some(state_kind(v1::JobState::Cancelled))
    );
    assert_eq!(next_event(&mut stream).await, None);
    handle.shutdown().await;
    db.dispose().await;
}

async fn watch_follows_to_terminal(db: &TestDb, handle: &ServerHandle) {
    let repository = db.repository();
    let mut client = watch_client(handle).await;
    let created = client
        .create_job(create_request("owner-1", "req-watch-follow"))
        .await
        .expect("create succeeds")
        .into_inner();
    let id = mm_domain::JobId::from_str(&created.job_id).expect("job id");
    let mut stream = client
        .watch_job(v1::WatchJobRequest {
            user_id: "owner-1".to_string(),
            job_id: created.job_id,
        })
        .await
        .expect("watch starts")
        .into_inner();
    assert_eq!(
        next_event(&mut stream).await,
        Some(state_kind(v1::JobState::Pending))
    );

    let claimant = WorkerId::from_str("w-watch").expect("worker id");
    let claimed = repository
        .claim(
            &claimant,
            NonZeroU32::new(1).expect("limit"),
            Duration::from_secs(30),
        )
        .await
        .expect("claim succeeds");
    assert_eq!(claimed.len(), 1);
    assert_eq!(
        next_event(&mut stream).await,
        Some(state_kind(v1::JobState::Running))
    );

    for value in [25u8, 50, 75] {
        repository
            .set_progress(
                id,
                &claimant,
                ProgressPercent::new(value).expect("progress"),
            )
            .await
            .expect("progress persists");
        assert_eq!(
            next_event(&mut stream).await,
            Some(progress_kind(u32::from(value)))
        );
    }

    repository
        .mark_succeeded(id, &claimant, concat_result())
        .await
        .expect("job succeeds");
    assert_eq!(next_event(&mut stream).await, Some(progress_kind(100)));
    assert_eq!(
        next_event(&mut stream).await,
        Some(state_kind(v1::JobState::Succeeded))
    );
    assert_eq!(next_event(&mut stream).await, None);
}

#[tokio::test]
async fn a_watch_stream_follows_notified_changes_to_the_terminal_state() {
    let db = fresh_db().await;
    let store = store_at(&store_endpoint()).await;
    let mut config = test_config(db.repository(), store, Prober::default());
    config.watch_poll_interval = Duration::from_secs(600);
    config.job_events = Some(Arc::new(
        PgJobEventListener::connect(&db.url)
            .await
            .expect("events listener connects"),
    ));
    let handle = start_server(config).await;
    watch_follows_to_terminal(&db, &handle).await;
    handle.shutdown().await;
    db.dispose().await;
}

#[tokio::test]
async fn a_watch_stream_follows_changes_by_polling_without_a_listener() {
    let db = fresh_db().await;
    let store = store_at(&store_endpoint()).await;
    let mut config = test_config(db.repository(), store, Prober::default());
    config.watch_poll_interval = Duration::from_millis(100);
    let handle = start_server(config).await;
    watch_follows_to_terminal(&db, &handle).await;
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

async fn list(
    client: &mut MediaJobServiceClient<tonic::transport::Channel>,
    user: &str,
    statuses: Vec<i32>,
    page_size: u32,
    page_token: &str,
) -> v1::ListJobsResponse {
    client
        .list_jobs(v1::ListJobsRequest {
            user_id: user.to_string(),
            statuses,
            page_size,
            page_token: page_token.to_string(),
        })
        .await
        .expect("list succeeds")
        .into_inner()
}

#[tokio::test]
async fn list_jobs_pages_stably_newest_first_for_the_owner_only() {
    let (db, handle) = healthy_server().await;
    let mut client = job_client(&handle).await;
    let mut created = Vec::new();
    for index in 0..7 {
        created.push(create_job_id(&mut client, "owner-1", &format!("req-{index}")).await);
    }
    create_job_id(&mut client, "owner-2", "req-foreign").await;
    created.reverse();
    let mut seen = Vec::new();
    let mut token = String::new();
    let mut pages = 0;
    loop {
        let page = list(&mut client, "owner-1", vec![], 3, &token).await;
        assert!(page.jobs.len() <= 3);
        seen.extend(page.jobs.iter().map(|job| job.job_id.clone()));
        pages += 1;
        if page.next_page_token.is_empty() {
            break;
        }
        token = page.next_page_token;
    }
    assert!(pages >= 3);
    assert_eq!(seen, created);
    let repeat = list(&mut client, "owner-1", vec![], 3, "").await;
    assert_eq!(
        repeat
            .jobs
            .iter()
            .map(|job| job.job_id.clone())
            .collect::<Vec<_>>(),
        created[..3]
    );
    handle.shutdown().await;
    db.dispose().await;
}

#[tokio::test]
async fn list_jobs_filters_by_status() {
    let (db, handle) = healthy_server().await;
    let mut client = job_client(&handle).await;
    let kept = create_job_id(&mut client, "owner-1", "req-1").await;
    let dropped = create_job_id(&mut client, "owner-1", "req-2").await;
    cancel(&mut client, "owner-1", &dropped).await;
    let cancelled = list(
        &mut client,
        "owner-1",
        vec![v1::JobState::Cancelled as i32],
        0,
        "",
    )
    .await;
    assert_eq!(
        cancelled
            .jobs
            .iter()
            .map(|job| job.job_id.clone())
            .collect::<Vec<_>>(),
        vec![dropped]
    );
    let pending = list(
        &mut client,
        "owner-1",
        vec![v1::JobState::Pending as i32],
        0,
        "",
    )
    .await;
    assert_eq!(
        pending
            .jobs
            .iter()
            .map(|job| job.job_id.clone())
            .collect::<Vec<_>>(),
        vec![kept]
    );
    handle.shutdown().await;
    db.dispose().await;
}

#[tokio::test]
async fn list_jobs_defaults_and_caps_the_page_size() {
    let (db, handle) = healthy_server().await;
    let repo = db.repository();
    for index in 0..101 {
        repo.create(mm_domain::NewJob::new(
            mm_domain::JobId::generate(),
            "owner-1".parse().expect("valid user id"),
            format!("req-{index}").parse().expect("valid request id"),
            OpSpec::try_from(concat_spec()).expect("valid spec"),
            None,
        ))
        .await
        .expect("create succeeds");
    }
    let mut client = job_client(&handle).await;
    let defaulted = list(&mut client, "owner-1", vec![], 0, "").await;
    assert_eq!(defaulted.jobs.len(), 20);
    assert!(!defaulted.next_page_token.is_empty());
    let capped = list(&mut client, "owner-1", vec![], 1000, "").await;
    assert_eq!(capped.jobs.len(), 100);
    assert!(!capped.next_page_token.is_empty());
    let rest = list(
        &mut client,
        "owner-1",
        vec![],
        1000,
        &capped.next_page_token,
    )
    .await;
    assert_eq!(rest.jobs.len(), 1);
    assert!(rest.next_page_token.is_empty());
    handle.shutdown().await;
    db.dispose().await;
}

#[tokio::test]
async fn invalid_list_arguments_are_rejected() {
    let (db, handle) = healthy_server().await;
    let mut client = job_client(&handle).await;
    for token in ["not a token!", "AAAA", "####"] {
        let status = client
            .list_jobs(v1::ListJobsRequest {
                user_id: "owner-1".to_string(),
                statuses: vec![],
                page_size: 0,
                page_token: token.to_string(),
            })
            .await
            .expect_err("bad cursor rejected");
        assert_eq!(status.code(), tonic::Code::InvalidArgument, "{token:?}");
        assert!(status.message().contains("page token"));
    }
    let unknown = client
        .list_jobs(v1::ListJobsRequest {
            user_id: "owner-1".to_string(),
            statuses: vec![v1::JobState::Unspecified as i32, 99],
            page_size: 0,
            page_token: String::new(),
        })
        .await
        .expect_err("unknown status rejected");
    assert_eq!(unknown.code(), tonic::Code::InvalidArgument);
    handle.shutdown().await;
    db.dispose().await;
}

#[tokio::test]
async fn a_job_carries_the_trace_the_caller_offered() {
    let (db, handle) = healthy_server().await;
    let mut client = job_client(&handle).await;
    let trace = TraceId::generate();
    let mut request = Request::new(create_request("owner-trace", "req-trace-1"));
    request.metadata_mut().insert(
        "x-trace-id",
        trace.to_string().parse().expect("metadata value"),
    );
    let created = client
        .create_job(request)
        .await
        .expect("create succeeds")
        .into_inner();
    let record = db
        .repository()
        .get(JobId::from_str(&created.job_id).expect("job id parses"))
        .await
        .expect("read succeeds")
        .expect("job exists");
    assert_eq!(record.trace_id, Some(trace));
    handle.shutdown().await;
    db.dispose().await;
}

#[tokio::test]
async fn a_call_without_a_trace_gets_one_of_its_own() {
    let (db, handle) = healthy_server().await;
    let mut client = job_client(&handle).await;
    let created = client
        .create_job(create_request("owner-trace", "req-trace-2"))
        .await
        .expect("create succeeds")
        .into_inner();
    let record = db
        .repository()
        .get(JobId::from_str(&created.job_id).expect("job id parses"))
        .await
        .expect("read succeeds")
        .expect("job exists");
    assert!(
        record.trace_id.is_some(),
        "a generated trace must be stored"
    );
    handle.shutdown().await;
    db.dispose().await;
}

#[tokio::test]
async fn an_unusable_trace_header_is_replaced_rather_than_refused() {
    let (db, handle) = healthy_server().await;
    let mut client = job_client(&handle).await;
    let mut request = Request::new(create_request("owner-trace", "req-trace-3"));
    request
        .metadata_mut()
        .insert("x-trace-id", "not-a-uuid".parse().expect("metadata value"));
    let created = client
        .create_job(request)
        .await
        .expect("create succeeds")
        .into_inner();
    let record = db
        .repository()
        .get(JobId::from_str(&created.job_id).expect("job id parses"))
        .await
        .expect("read succeeds")
        .expect("job exists");
    assert!(record.trace_id.is_some());
    assert_ne!(
        record.trace_id,
        TraceId::from_str("00000000-0000-0000-0000-000000000000").ok()
    );
    handle.shutdown().await;
    db.dispose().await;
}

#[tokio::test]
async fn a_rejected_cut_is_invalid_argument_naming_the_field() {
    let (db, handle) = healthy_server().await;
    let mut client = job_client(&handle).await;

    let refused = [
        (
            Some(v1::TrimRange {
                start_seconds: -1.0,
                duration_seconds: 2.0,
            }),
            None,
            None,
            "trim start_seconds",
        ),
        (
            Some(v1::TrimRange {
                start_seconds: 0.0,
                duration_seconds: 0.0,
            }),
            None,
            None,
            "trim duration_seconds",
        ),
        (None, Some(0.0), None, "speed"),
        (None, Some(-1.0), None, "speed"),
        (None, None, Some(-0.5), "volume"),
    ];
    for (position, (trim, speed, volume, field)) in refused.into_iter().enumerate() {
        let mut request = create_request("owner-1", &format!("req-cut-{position}"));
        request.spec = Some(cut_transcode_spec(trim, speed, volume));
        let status = client
            .create_job(request)
            .await
            .expect_err("a rejected cut is refused at submission");
        assert_eq!(status.code(), tonic::Code::InvalidArgument, "{field}");
        assert!(
            status.message().contains(field),
            "{:?} does not name {field}",
            status.message()
        );
    }

    handle.shutdown().await;
    db.dispose().await;
}

#[tokio::test]
async fn a_cut_an_op_cannot_apply_is_invalid_argument_naming_the_op_and_the_field() {
    let (db, handle) = healthy_server().await;
    let mut client = job_client(&handle).await;
    let mut request = create_request("owner-1", "req-cut-unsupported");
    request.spec = Some(v1::OpSpec {
        op: Some(v1::op_spec::Op::Probe(v1::ProbeSpec {
            input: Some(v1::InputSource {
                source: Some(v1::input_source::Source::Media(v1::MediaSource {
                    media: Some(v1::MediaRef::from(
                        &MediaRef::object_uri("s3://media/in/a.mp4").expect("valid ref"),
                    )),
                    trim: Some(v1::TrimRange {
                        start_seconds: 0.0,
                        duration_seconds: 1.0,
                    }),
                    speed: None,
                    volume: None,
                })),
            }),
        })),
    });

    let status = client
        .create_job(request)
        .await
        .expect_err("probe cannot apply a cut");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("probe"), "{:?}", status.message());
    assert!(status.message().contains("trim"), "{:?}", status.message());

    handle.shutdown().await;
    db.dispose().await;
}

#[tokio::test]
async fn a_source_carrying_trim_speed_and_volume_is_accepted() {
    let (db, handle) = healthy_server().await;
    let mut client = job_client(&handle).await;
    let mut request = create_request("owner-1", "req-cut-valid");
    request.spec = Some(cut_transcode_spec(
        Some(v1::TrimRange {
            start_seconds: 1.0,
            duration_seconds: 2.0,
        }),
        Some(0.5),
        Some(0.5),
    ));

    let created = client
        .create_job(request)
        .await
        .expect("a well formed source is accepted")
        .into_inner();
    assert!(created.created);

    handle.shutdown().await;
    db.dispose().await;
}

#[tokio::test]
async fn a_node_reference_is_unimplemented_at_submission() {
    let (db, handle) = healthy_server().await;
    let mut client = job_client(&handle).await;
    let mut request = create_request("owner-1", "req-node-ref");
    request.spec = Some(v1::OpSpec {
        op: Some(v1::op_spec::Op::Probe(v1::ProbeSpec {
            input: Some(v1::InputSource {
                source: Some(v1::input_source::Source::Node(v1::NodeRef {
                    id: "join".to_string(),
                    output: 0,
                })),
            }),
        })),
    });
    let status = client
        .create_job(request)
        .await
        .expect_err("a node reference has no producer yet");
    assert_eq!(status.code(), tonic::Code::Unimplemented);
    assert!(status.message().contains("join"), "{:?}", status.message());

    handle.shutdown().await;
    db.dispose().await;
}

fn template_value(uri: &str) -> v1::ParameterBinding {
    let media = MediaRef::object_uri(uri).expect("valid uri");
    v1::ParameterBinding {
        name: "source".to_string(),
        value: Some(v1::ParameterValue {
            value: Some(v1::parameter_value::Value::Uri(v1::MediaRef::from(&media))),
        }),
    }
}

#[tokio::test]
async fn a_template_renders_into_jobs_and_stays_owner_scoped() {
    let (db, handle) = healthy_server().await;
    let mut client = job_client(&handle).await;

    let source = ParameterName::from_str("source").expect("valid parameter name");
    let spec = mm_proto::v1::OpSpec::from(&OpSpec::Transcode(
        TranscodeSpec::new(
            InputSource::from(source),
            OutputProfile::from_raw(24.0, 0, 0, 0, 48_000, 640, 360).expect("valid profile"),
        )
        .expect("valid transcode spec"),
    ));
    let created = client
        .create_template(v1::CreateTemplateRequest {
            user_id: "owner-1".to_string(),
            name: "recipe".to_string(),
            spec: Some(spec),
            parameters: vec![v1::ParameterDeclaration {
                name: "source".to_string(),
                r#type: v1::ParameterType::Uri as i32,
            }],
        })
        .await
        .expect("create template")
        .into_inner();
    let template = created.template.expect("template present");
    let template_id = template.template_id.clone();

    let status = client
        .get_template(v1::GetTemplateRequest {
            user_id: "owner-2".to_string(),
            template_id: template_id.clone(),
        })
        .await
        .expect_err("another owner reads NOT_FOUND");
    assert_eq!(status.code(), tonic::Code::NotFound);

    let first = client
        .render_template(v1::RenderTemplateRequest {
            user_id: "owner-1".to_string(),
            template_id: template_id.clone(),
            idempotency_key: "render-1".to_string(),
            values: vec![template_value("s3://media/one.mp4")],
            callback: None,
        })
        .await
        .expect("first render")
        .into_inner();
    let second = client
        .render_template(v1::RenderTemplateRequest {
            user_id: "owner-1".to_string(),
            template_id: template_id.clone(),
            idempotency_key: "render-2".to_string(),
            values: vec![template_value("s3://media/two.mp4")],
            callback: None,
        })
        .await
        .expect("second render")
        .into_inner();
    assert_ne!(first.job_id, second.job_id);

    let replay = client
        .render_template(v1::RenderTemplateRequest {
            user_id: "owner-1".to_string(),
            template_id: template_id.clone(),
            idempotency_key: "render-1".to_string(),
            values: vec![template_value("s3://media/one.mp4")],
            callback: None,
        })
        .await
        .expect("replay")
        .into_inner();
    assert_eq!(replay.job_id, first.job_id);
    assert!(!replay.created);

    let wrong = v1::ParameterBinding {
        name: "source".to_string(),
        value: Some(v1::ParameterValue {
            value: Some(v1::parameter_value::Value::Text("not a uri".to_string())),
        }),
    };
    let status = client
        .render_template(v1::RenderTemplateRequest {
            user_id: "owner-1".to_string(),
            template_id: template_id.clone(),
            idempotency_key: "render-3".to_string(),
            values: vec![wrong],
            callback: None,
        })
        .await
        .expect_err("a wrong type is refused");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("source"),
        "{:?}",
        status.message()
    );

    let listed = client
        .list_templates(v1::ListTemplatesRequest {
            user_id: "owner-1".to_string(),
        })
        .await
        .expect("list templates")
        .into_inner();
    assert_eq!(listed.templates.len(), 1);

    let deleted = client
        .delete_template(v1::DeleteTemplateRequest {
            user_id: "owner-1".to_string(),
            template_id: template_id.clone(),
        })
        .await
        .expect("delete template")
        .into_inner();
    assert!(deleted.deleted);
    let status = client
        .delete_template(v1::DeleteTemplateRequest {
            user_id: "owner-2".to_string(),
            template_id,
        })
        .await
        .expect_err("another owner deletes NOT_FOUND");
    assert_eq!(status.code(), tonic::Code::NotFound);

    handle.shutdown().await;
    db.dispose().await;
}

async fn merge_client(
    handle: &ServerHandle,
) -> mm_proto::vm::v1::video_merge_service_client::VideoMergeServiceClient<tonic::transport::Channel>
{
    let channel = Endpoint::from_shared(format!("http://{}", handle.local_addr()))
        .expect("valid endpoint")
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .connect()
        .await
        .expect("dial announced address");
    mm_proto::vm::v1::video_merge_service_client::VideoMergeServiceClient::new(channel)
}

fn merge_job(order: Vec<(i32, &str)>) -> vm::VideoMergeJob {
    vm::VideoMergeJob {
        job_id: String::new(),
        request_id: "req-merge-1".to_string(),
        requester: vm::Requester::ChatMessage as i32,
        status: None,
        input_files: order
            .into_iter()
            .map(|(sequence_order, media_id)| vm::InputFile {
                sequence_order,
                media_id: media_id.to_string(),
            })
            .collect(),
        created_at_millis: 0,
        error_code: None,
        file_errors: Vec::new(),
        output: None,
        r2_uri: None,
        user_id: "owner-merge".to_string(),
    }
}

#[tokio::test]
async fn the_reference_facade_creates_reads_and_lists_a_job() {
    let (_db, handle) = healthy_server().await;
    let mut client = merge_client(&handle).await;

    let created = client
        .create_job(vm::CreateJobRequest {
            job: Some(merge_job(vec![
                (1, "s3://media/in/b.mp4"),
                (0, "s3://media/in/a.mp4"),
            ])),
            idempotency_key: "req-merge-1".to_string(),
            callback: None,
        })
        .await
        .expect("create succeeds")
        .into_inner();

    assert!(created.created, "the first call inserted the job");
    assert_eq!(created.request_id, "req-merge-1");
    assert!(
        created.created_at_millis > 0,
        "the reference answers created_at"
    );

    let read = client
        .get_job(vm::GetJobRequest {
            job_id: created.job_id.clone(),
            user_id: "owner-merge".to_string(),
        })
        .await
        .expect("get succeeds")
        .into_inner()
        .job
        .expect("the reference answers the job");

    assert_eq!(read.request_id, "req-merge-1");
    assert_eq!(read.requester, vm::Requester::ChatMessage as i32);
    assert_eq!(
        read.status, None,
        "a job that has not settled answers no status"
    );
    assert_eq!(
        read.input_files
            .iter()
            .map(|file| file.sequence_order)
            .collect::<Vec<_>>(),
        vec![0, 1],
        "the stored order is the one the caller named"
    );
    assert_eq!(read.input_files[0].media_id, "s3://media/in/a.mp4");

    let page = client
        .list_user_jobs(vm::ListUserJobsRequest {
            user_id: "owner-merge".to_string(),
            page_size: 10,
            ..Default::default()
        })
        .await
        .expect("list succeeds")
        .into_inner();
    assert_eq!(page.jobs.len(), 1);
    assert_eq!(page.jobs[0].job_id, created.job_id);
    assert_eq!(page.next_page_token, "");

    handle.shutdown().await;
}

#[tokio::test]
async fn the_reference_facade_replays_a_known_request_id() {
    let (_db, handle) = healthy_server().await;
    let mut client = merge_client(&handle).await;

    let message = vm::CreateJobRequest {
        job: Some(merge_job(vec![
            (0, "s3://media/in/a.mp4"),
            (1, "s3://media/in/b.mp4"),
        ])),
        idempotency_key: "req-merge-replay".to_string(),
        callback: None,
    };
    let first = client
        .create_job(message.clone())
        .await
        .expect("create succeeds")
        .into_inner();
    let second = client
        .create_job(message)
        .await
        .expect("replay succeeds")
        .into_inner();

    assert!(first.created);
    assert!(!second.created, "a replay answers created=false");
    assert_eq!(first.job_id, second.job_id);

    handle.shutdown().await;
}

#[tokio::test]
async fn a_missing_object_is_refused_before_the_job_is_accepted() {
    let (_db, handle) = healthy_server().await;
    let mut client = merge_client(&handle).await;

    let answer = client
        .create_job(vm::CreateJobRequest {
            job: Some(merge_job(vec![
                (0, "s3://media/in/a.mp4"),
                (1, "s3://media/in/missing.mp4"),
            ])),
            idempotency_key: "req-merge-missing".to_string(),
            callback: None,
        })
        .await
        .expect_err("an absent input is refused before the job is accepted");

    assert_eq!(answer.code(), tonic::Code::FailedPrecondition);
    assert!(
        answer.message().contains("missing.mp4"),
        "{}",
        answer.message()
    );

    let page = client
        .list_user_jobs(vm::ListUserJobsRequest {
            user_id: "owner-merge".to_string(),
            page_size: 10,
            ..Default::default()
        })
        .await
        .expect("list succeeds")
        .into_inner();
    assert!(
        page.jobs.is_empty(),
        "a job naming an absent input never reaches PENDING"
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn a_repeated_sequence_order_is_refused_by_the_facade() {
    let (_db, handle) = healthy_server().await;
    let mut client = merge_client(&handle).await;

    let answer = client
        .create_job(vm::CreateJobRequest {
            job: Some(merge_job(vec![
                (1, "s3://media/in/a.mp4"),
                (1, "s3://media/in/b.mp4"),
            ])),
            idempotency_key: "req-merge-dup".to_string(),
            callback: None,
        })
        .await
        .expect_err("a repeated sequence order is refused");

    assert_eq!(answer.code(), tonic::Code::InvalidArgument);
    assert!(
        answer.message().contains("sequence order"),
        "{}",
        answer.message()
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn a_foreign_or_absent_job_is_not_found_on_the_facade() {
    let (_db, handle) = healthy_server().await;
    let mut client = merge_client(&handle).await;

    let absent = client
        .get_job(vm::GetJobRequest {
            job_id: mm_domain::JobId::generate().to_string(),
            user_id: "owner-merge".to_string(),
        })
        .await
        .expect_err("an absent job is not found");
    assert_eq!(absent.code(), tonic::Code::NotFound);

    let created = client
        .create_job(vm::CreateJobRequest {
            job: Some(vm::VideoMergeJob {
                request_id: "req-foreign".to_string(),
                user_id: "owner-merge".to_string(),
                ..merge_job(vec![(0, "s3://media/in/a.mp4"), (1, "s3://media/in/b.mp4")])
            }),
            idempotency_key: "req-foreign".to_string(),
            callback: None,
        })
        .await
        .expect("create succeeds")
        .into_inner();

    let foreign = client
        .get_job(vm::GetJobRequest {
            job_id: created.job_id.clone(),
            user_id: "owner-other".to_string(),
        })
        .await
        .expect_err("another owner's job reads as missing");
    assert_eq!(
        foreign.code(),
        tonic::Code::NotFound,
        "a foreign job is reported the same way an absent one is"
    );

    let foreign_list = client
        .list_user_jobs(vm::ListUserJobsRequest {
            user_id: "owner-other".to_string(),
            page_size: 10,
            ..Default::default()
        })
        .await
        .expect("list succeeds")
        .into_inner();
    assert!(foreign_list.jobs.is_empty(), "another owner lists nothing");

    let cancelled = client
        .cancel_job(vm::CancelJobRequest {
            job_id: created.job_id.clone(),
            user_id: "owner-other".to_string(),
        })
        .await
        .expect_err("another owner's job cannot be cancelled");
    assert_eq!(cancelled.code(), tonic::Code::NotFound);

    handle.shutdown().await;
}

#[tokio::test]
async fn the_reference_facade_answers_the_output_profile_it_accepted() {
    let (_db, handle) = healthy_server().await;
    let mut client = merge_client(&handle).await;

    let created = client
        .create_job(vm::CreateJobRequest {
            job: Some(vm::VideoMergeJob {
                request_id: "req-profile".to_string(),
                output: Some(vm::OutputProfile {
                    frame_rate: Some(30),
                    video_max_bitrate: Some(8_000_000),
                    video_qvbr_quality: None,
                    audio_bitrate: None,
                    audio_sample_rate: None,
                }),
                ..merge_job(vec![(0, "s3://media/in/a.mp4"), (1, "s3://media/in/b.mp4")])
            }),
            idempotency_key: "req-profile".to_string(),
            callback: None,
        })
        .await
        .expect("create succeeds")
        .into_inner();

    let read = client
        .get_job(vm::GetJobRequest {
            job_id: created.job_id,
            user_id: "owner-merge".to_string(),
        })
        .await
        .expect("get succeeds")
        .into_inner()
        .job
        .expect("the reference answers the job");

    let output = read
        .output
        .expect("the reference answers the output profile");
    assert_eq!(output.frame_rate, Some(30));
    assert_eq!(output.video_max_bitrate, Some(8_000_000));
    assert_eq!(output.audio_bitrate, None, "an unset field stays unset");

    handle.shutdown().await;
}

#[tokio::test]
async fn a_requester_filter_matches_the_requester_type() {
    let (_db, handle) = healthy_server().await;
    let mut client = merge_client(&handle).await;

    let created = client
        .create_job(vm::CreateJobRequest {
            job: Some(vm::VideoMergeJob {
                job_id: String::new(),
                request_id: "req-filter".to_string(),
                requester: vm::Requester::ChatMessage as i32,
                user_id: "owner-merge".to_string(),
                status: None,
                input_files: vec![
                    vm::InputFile {
                        sequence_order: 0,
                        media_id: "s3://media/in/a.mp4".to_string(),
                    },
                    vm::InputFile {
                        sequence_order: 1,
                        media_id: "s3://media/in/b.mp4".to_string(),
                    },
                ],
                created_at_millis: 0,
                error_code: Some(String::new()),
                file_errors: vec![],
                output: None,
                r2_uri: None,
            }),
            idempotency_key: "idem-filter".to_string(),
            callback: None,
        })
        .await
        .expect("create succeeds")
        .into_inner();
    assert!(created.created);

    let filtered = client
        .list_user_jobs(vm::ListUserJobsRequest {
            user_id: "owner-merge".to_string(),
            page_size: 10,
            requester: Some(vm::Requester::ChatMessage as i32),
            ..Default::default()
        })
        .await
        .expect("list with matching requester succeeds")
        .into_inner();
    assert_eq!(filtered.jobs.len(), 1);
    assert_eq!(filtered.jobs[0].job_id, created.job_id);

    let not_matching = client
        .list_user_jobs(vm::ListUserJobsRequest {
            user_id: "owner-merge".to_string(),
            page_size: 10,
            requester: Some(vm::Requester::Studio as i32),
            ..Default::default()
        })
        .await
        .expect("list with non-matching requester succeeds")
        .into_inner();
    assert_eq!(not_matching.jobs.len(), 0);

    handle.shutdown().await;
}

#[tokio::test]
async fn a_status_filter_the_reference_cannot_express_is_refused() {
    let (_db, handle) = healthy_server().await;
    let mut client = merge_client(&handle).await;

    let answer = client
        .list_user_jobs(vm::ListUserJobsRequest {
            user_id: "owner-merge".to_string(),
            page_size: 10,
            statuses: vec![v1::StitchStatus::Unspecified as i32],
            ..Default::default()
        })
        .await
        .expect_err("an unsettled status is not a filter");

    assert_eq!(answer.code(), tonic::Code::InvalidArgument);

    handle.shutdown().await;
}

#[tokio::test]
async fn the_reference_facade_cancels_a_job_and_answers_its_status() {
    let (_db, handle) = healthy_server().await;
    let mut client = merge_client(&handle).await;

    let created = client
        .create_job(vm::CreateJobRequest {
            job: Some(vm::VideoMergeJob {
                request_id: "req-cancel".to_string(),
                ..merge_job(vec![(0, "s3://media/in/a.mp4"), (1, "s3://media/in/b.mp4")])
            }),
            idempotency_key: "req-cancel".to_string(),
            callback: None,
        })
        .await
        .expect("create succeeds")
        .into_inner();

    let cancelled = client
        .cancel_job(vm::CancelJobRequest {
            job_id: created.job_id.clone(),
            user_id: "owner-merge".to_string(),
        })
        .await
        .expect("cancel succeeds")
        .into_inner();

    assert!(cancelled.cancellation_accepted);
    assert_eq!(cancelled.job_id, created.job_id);
    assert_eq!(
        cancelled.status,
        v1::StitchStatus::Unspecified as i32,
        "a job that has not settled answers no status"
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn a_page_token_from_the_facade_pages_the_list() {
    let (_db, handle) = healthy_server().await;
    let mut client = merge_client(&handle).await;

    for index in 0..3 {
        client
            .create_job(vm::CreateJobRequest {
                job: Some(vm::VideoMergeJob {
                    request_id: format!("req-page-{index}"),
                    user_id: "owner-merge".to_string(),
                    ..merge_job(vec![(0, "s3://media/in/a.mp4"), (1, "s3://media/in/b.mp4")])
                }),
                idempotency_key: format!("req-page-{index}"),
                callback: None,
            })
            .await
            .expect("create succeeds");
    }

    let first = client
        .list_user_jobs(vm::ListUserJobsRequest {
            user_id: "owner-merge".to_string(),
            page_size: 2,
            ..Default::default()
        })
        .await
        .expect("list succeeds")
        .into_inner();
    assert_eq!(first.jobs.len(), 2);
    assert_ne!(first.next_page_token, "", "a full page answers a token");

    let second = client
        .list_user_jobs(vm::ListUserJobsRequest {
            user_id: "owner-merge".to_string(),
            page_size: 2,
            page_token: first.next_page_token.clone(),
            ..Default::default()
        })
        .await
        .expect("list succeeds")
        .into_inner();
    assert_eq!(second.jobs.len(), 1, "the second page holds the remainder");

    handle.shutdown().await;
}

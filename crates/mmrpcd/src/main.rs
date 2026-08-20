use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use mm_auth::Settings;
use mm_domain::{JobRepository, JobStoreError, WorkerId};
use mm_ffmpeg::Prober;
use mm_jobstore::{PgCancelListener, PgJobEventListener, PgJobListener, PgJobRepository};
use mm_observability::{Component, record};
use mm_observability::{Metrics, Registry, Scrape};
use mm_server::{Auth, ServerConfig};
use mm_storage::{CredentialSource, S3ObjectStore, S3Settings, SqsCallbackPublisher, SqsSettings};
use mm_worker::{DiskBudget, RetryPolicy, WorkerConfig, WorkerHandle};
use sqlx::postgres::PgPoolOptions;
use tokio::task::JoinHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Server,
    Worker,
    Combined,
}

fn parse_role(args: &[String]) -> Result<Role, String> {
    match args {
        [role] if role == "server" => Ok(Role::Server),
        [role] if role == "worker" => Ok(Role::Worker),
        [role] if role == "combined" => Ok(Role::Combined),
        _ => Err(String::from("usage: mmrpcd <server|worker|combined>")),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StackEnv {
    database_url: String,
    store_endpoint: String,
    store_bucket: String,
    store_region: Option<String>,
    store_credentials: Option<(String, String)>,
    migrate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerEnv {
    bind: SocketAddr,
    stack: StackEnv,
    notify: bool,
    auth: AuthSettings,
    observability: Observability,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkerEnv {
    stack: StackEnv,
    worker_id: String,
    staging_dir: PathBuf,
    max_concurrent: u32,
    claim_batch: u32,
    ladder_concurrency: u32,
    lease: Duration,
    heartbeat_interval: Duration,
    reconcile_interval: Duration,
    progress_interval: Duration,
    default_deadline: Duration,
    disk_budget_bytes: Option<u64>,
    retry_attempts: u32,
    backoff_base: Duration,
    backoff_cap: Duration,
    notify: bool,
    queue_endpoint: Option<String>,
    queue_region: Option<String>,
    queue_credentials: Option<(String, String)>,
    observability: Observability,
}

fn stack_env(get: &dyn Fn(&str) -> Option<String>) -> Result<StackEnv, String> {
    let required = |name: &str| get(name).ok_or_else(|| format!("{name} is required"));
    let database_url = required("MM_DATABASE_URL")?;
    let store_endpoint = required("MM_OBJECTSTORE_ENDPOINT")?;
    let store_bucket = required("MM_OBJECTSTORE_BUCKET")?;
    let store_credentials = match (
        get("MM_OBJECTSTORE_ACCESS_KEY"),
        get("MM_OBJECTSTORE_SECRET_KEY"),
    ) {
        (Some(access), Some(secret)) => Some((access, secret)),
        (None, None) => None,
        _ => {
            return Err(String::from(
                "MM_OBJECTSTORE_ACCESS_KEY and MM_OBJECTSTORE_SECRET_KEY \
                 must be set together",
            ));
        }
    };
    Ok(StackEnv {
        database_url,
        store_endpoint,
        store_bucket,
        store_region: get("MM_OBJECTSTORE_REGION"),
        store_credentials,
        migrate: migrate_var(get)?,
    })
}

fn notify_var(get: &dyn Fn(&str) -> Option<String>, name: &str) -> Result<bool, String> {
    match get(name).as_deref() {
        None | Some("on") => Ok(true),
        Some("off") => Ok(false),
        Some(other) => Err(format!("{name} must be on or off: {other}")),
    }
}

fn migrate_var(get: &dyn Fn(&str) -> Option<String>) -> Result<bool, String> {
    match get("MM_MIGRATE").as_deref() {
        None | Some("off") => Ok(false),
        Some("on") => Ok(true),
        Some(other) => Err(format!("MM_MIGRATE must be on or off: {other}")),
    }
}

fn server_env(get: &dyn Fn(&str) -> Option<String>) -> Result<ServerEnv, String> {
    let raw_bind = get("MM_BIND_ADDR").unwrap_or_else(|| String::from("127.0.0.1:0"));
    let bind = raw_bind
        .parse()
        .map_err(|_| format!("MM_BIND_ADDR is not a socket address: {raw_bind}"))?;
    Ok(ServerEnv {
        bind,
        stack: stack_env(get)?,
        notify: notify_var(get, "MM_SERVER_NOTIFY")?,
        auth: auth_settings(get)?,
        observability: observability(get)?,
    })
}

fn positive_var(
    get: &dyn Fn(&str) -> Option<String>,
    name: &str,
    default: u32,
) -> Result<u32, String> {
    match get(name) {
        None => Ok(default),
        Some(raw) => match raw.parse::<u32>() {
            Ok(value) if value > 0 => Ok(value),
            _ => Err(format!("{name} is not a positive number: {raw}")),
        },
    }
}

fn seconds_var(
    get: &dyn Fn(&str) -> Option<String>,
    name: &str,
    default: u64,
) -> Result<Duration, String> {
    match get(name) {
        None => Ok(Duration::from_secs(default)),
        Some(raw) => raw
            .parse::<u64>()
            .map(Duration::from_secs)
            .map_err(|_| format!("{name} is not a number of seconds: {raw}")),
    }
}

fn millis_var(
    get: &dyn Fn(&str) -> Option<String>,
    name: &str,
    default: u64,
) -> Result<Duration, String> {
    match get(name) {
        None => Ok(Duration::from_millis(default)),
        Some(raw) => raw
            .parse::<u64>()
            .map(Duration::from_millis)
            .map_err(|_| format!("{name} is not a number of milliseconds: {raw}")),
    }
}

fn worker_env(get: &dyn Fn(&str) -> Option<String>) -> Result<WorkerEnv, String> {
    let stack = stack_env(get)?;
    let worker_id = get("MM_WORKER_ID").unwrap_or_else(|| format!("worker-{}", std::process::id()));
    let staging_dir = PathBuf::from(
        get("MM_WORKER_STAGING_DIR").unwrap_or_else(|| String::from("scratch/worker")),
    );
    let disk_budget_bytes =
        match get("MM_WORKER_DISK_BUDGET_BYTES") {
            None => None,
            Some(raw) => Some(raw.parse::<u64>().map_err(|_| {
                format!("MM_WORKER_DISK_BUDGET_BYTES is not a number of bytes: {raw}")
            })?),
        };
    let notify = notify_var(get, "MM_WORKER_NOTIFY")?;
    let queue_credentials = match (get("MM_QUEUE_ACCESS_KEY"), get("MM_QUEUE_SECRET_KEY")) {
        (Some(access), Some(secret)) => Some((access, secret)),
        (None, None) => None,
        _ => {
            return Err(String::from(
                "MM_QUEUE_ACCESS_KEY and MM_QUEUE_SECRET_KEY must be set together",
            ));
        }
    };
    Ok(WorkerEnv {
        stack,
        worker_id,
        staging_dir,
        max_concurrent: positive_var(get, "MM_WORKER_MAX_CONCURRENT", 4)?,
        claim_batch: positive_var(get, "MM_WORKER_CLAIM_BATCH", 4)?,
        ladder_concurrency: positive_var(get, "MM_WORKER_LADDER_CONCURRENCY", 1)?,
        lease: seconds_var(get, "MM_WORKER_LEASE_SECS", 30)?,
        heartbeat_interval: seconds_var(get, "MM_WORKER_HEARTBEAT_SECS", 10)?,
        reconcile_interval: seconds_var(get, "MM_WORKER_RECONCILE_SECS", 30)?,
        progress_interval: seconds_var(get, "MM_WORKER_PROGRESS_SECS", 2)?,
        default_deadline: seconds_var(get, "MM_WORKER_DEADLINE_SECS", 3600)?,
        disk_budget_bytes,
        retry_attempts: positive_var(get, "MM_WORKER_RETRY_ATTEMPTS", 3)?,
        backoff_base: millis_var(get, "MM_WORKER_BACKOFF_BASE_MS", 1000)?,
        backoff_cap: millis_var(get, "MM_WORKER_BACKOFF_CAP_MS", 60000)?,
        notify,
        queue_endpoint: get("MM_QUEUE_ENDPOINT"),
        queue_region: get("MM_QUEUE_REGION"),
        queue_credentials,
        observability: observability(get)?,
    })
}

async fn connect_stack(
    stack: &StackEnv,
) -> Result<(Arc<PgJobRepository>, Arc<S3ObjectStore>), String> {
    let pool = PgPoolOptions::new()
        .connect(&stack.database_url)
        .await
        .map_err(|error| format!("database connection failed: {error}"))?;
    if stack.migrate {
        mm_jobstore::MIGRATOR
            .run(&pool)
            .await
            .map_err(|error| format!("migrations failed: {error}"))?;
    }
    let repository = Arc::new(PgJobRepository::new(pool));
    let credentials = match stack.store_credentials.clone() {
        Some((access_key_id, secret_access_key)) => CredentialSource::Static {
            access_key_id,
            secret_access_key,
        },
        None => CredentialSource::DefaultChain,
    };
    let store = Arc::new(
        S3ObjectStore::connect(S3Settings {
            endpoint: Some(stack.store_endpoint.clone()),
            region: stack.store_region.clone(),
            force_path_style: true,
            credentials,
        })
        .await,
    );
    Ok((repository, store))
}

struct Telemetry {
    metrics: Metrics,
    scrape: Scrape,
    sampler: JoinHandle<()>,
}

async fn start_telemetry(
    repository: Arc<PgJobRepository>,
    settings: &Observability,
) -> Result<Telemetry, String> {
    let (registry, metrics) = Registry::new();
    let registry = Arc::new(registry);
    let scrape = mm_observability::serve(registry, settings.bind)
        .await
        .map_err(|error| format!("the metrics endpoint failed: {error}"))?;
    println!("metrics {}", scrape.local_addr());
    let sampler = {
        let interval = settings.interval;
        let sample_timeout = settings.sample_timeout;
        tokio::spawn(sample_queue_depth(
            repository,
            metrics.clone(),
            interval,
            sample_timeout,
        ))
    };
    Ok(Telemetry {
        metrics,
        scrape,
        sampler,
    })
}

#[async_trait::async_trait]
trait QueueDepth: Send + Sync {
    async fn pending_count(&self) -> Result<u64, JobStoreError>;
}

#[async_trait::async_trait]
impl QueueDepth for PgJobRepository {
    async fn pending_count(&self) -> Result<u64, JobStoreError> {
        JobRepository::pending_count(self).await
    }
}

async fn sample_once(
    repository: &dyn QueueDepth,
    metrics: &Metrics,
    bound: Duration,
) -> Result<(), String> {
    let pending = match tokio::time::timeout(bound, repository.pending_count()).await {
        Ok(Ok(pending)) => {
            metrics.jobs_pending.set(pending);
            return Ok(());
        }
        Ok(Err(error)) => error.to_string(),
        Err(_) => format!("the job store did not answer within {bound:?}"),
    };
    metrics.queue_depth_errors.add(1);
    record(
        Component::Daemon,
        None,
        "queue.depth.unavailable",
        &[
            ("dependency", "job store".to_string()),
            ("error", pending.clone()),
        ],
    );
    Err(pending)
}

async fn sample_queue_depth(
    repository: Arc<PgJobRepository>,
    metrics: Metrics,
    interval: Duration,
    sample_timeout: Duration,
) {
    loop {
        let _ = sample_once(repository.as_ref(), &metrics, sample_timeout).await;
        tokio::time::sleep(interval).await;
    }
}

async fn start_server(
    env: &ServerEnv,
    repository: Arc<PgJobRepository>,
    store: Arc<S3ObjectStore>,
    metrics: Metrics,
) -> Result<mm_server::ServerHandle, String> {
    let auth = build_auth(&env.auth)?;
    let mut config = ServerConfig::new(
        repository,
        store,
        env.stack.store_bucket.clone(),
        Prober::default(),
    )
    .map_err(|error| error.to_string())?;
    config.bind = env.bind;
    config.auth = auth;
    config.metrics = metrics;
    if env.notify {
        let events = PgJobEventListener::connect(&env.stack.database_url)
            .await
            .map_err(|error| format!("job events listener failed: {error}"))?;
        config.job_events = Some(Arc::new(events));
    }
    mm_server::start(config)
        .await
        .map_err(|error| error.to_string())
}

async fn start_worker(
    env: &WorkerEnv,
    repository: Arc<PgJobRepository>,
    store: Arc<S3ObjectStore>,
    metrics: Metrics,
) -> Result<WorkerHandle, String> {
    let worker_id = WorkerId::from_str(&env.worker_id)
        .map_err(|error| format!("MM_WORKER_ID is invalid: {error}"))?;
    let mut config = WorkerConfig::new(
        repository,
        store,
        env.stack.store_bucket.clone(),
        worker_id,
        env.staging_dir.clone(),
    );
    config.max_concurrent = env.max_concurrent as usize;
    config.claim_batch =
        NonZeroU32::new(env.claim_batch).ok_or("MM_WORKER_CLAIM_BATCH must be positive")?;
    config.ladder_concurrency = env.ladder_concurrency as usize;
    config.lease = env.lease;
    config.heartbeat_interval = env.heartbeat_interval;
    config.reconcile_interval = env.reconcile_interval;
    config.progress_interval = env.progress_interval;
    config.default_deadline = env.default_deadline;
    if let Some(bytes) = env.disk_budget_bytes {
        config.disk_budget = DiskBudget::Fixed(bytes);
    }
    config.retry = RetryPolicy {
        max_attempts: env.retry_attempts,
        base: env.backoff_base,
        cap: env.backoff_cap,
    };
    config.metrics = metrics;
    if env.notify {
        let listener = PgJobListener::connect(&env.stack.database_url)
            .await
            .map_err(|error| format!("notification listener failed: {error}"))?;
        config.notifications = Some(Arc::new(listener));
        let cancels = PgCancelListener::connect(&env.stack.database_url)
            .await
            .map_err(|error| format!("cancel listener failed: {error}"))?;
        config.cancellations = Some(Arc::new(cancels));
    }
    if let Some(endpoint) = env.queue_endpoint.clone() {
        let credentials = match env.queue_credentials.clone() {
            Some((access_key_id, secret_access_key)) => CredentialSource::Static {
                access_key_id,
                secret_access_key,
            },
            None => CredentialSource::DefaultChain,
        };
        let publisher = SqsCallbackPublisher::connect(SqsSettings {
            endpoint: Some(endpoint),
            region: env.queue_region.clone(),
            credentials,
        })
        .await;
        config.callbacks = Some(Arc::new(publisher));
    }
    let handle = mm_worker::start(config);
    println!("worker {} ready", env.worker_id);
    Ok(handle)
}

async fn serve(env: ServerEnv) -> Result<(), String> {
    let (repository, store) = connect_stack(&env.stack).await?;
    let telemetry = start_telemetry(repository.clone(), &env.observability).await?;
    let handle = start_server(&env, repository, store, telemetry.metrics.clone()).await?;
    wait_with_telemetry(handle.wait(), telemetry).await
}

async fn work(env: WorkerEnv) -> Result<(), String> {
    let (repository, store) = connect_stack(&env.stack).await?;
    let telemetry = start_telemetry(repository.clone(), &env.observability).await?;
    let handle = start_worker(&env, repository, store, telemetry.metrics).await?;
    handle.wait().await;
    telemetry.scrape.shutdown().await;
    telemetry.sampler.abort();
    Ok(())
}

async fn combine(server: ServerEnv, worker: WorkerEnv) -> Result<(), String> {
    let (repository, store) = connect_stack(&server.stack).await?;
    let telemetry = start_telemetry(repository.clone(), &server.observability).await?;
    let worker_handle = start_worker(
        &worker,
        repository.clone(),
        store.clone(),
        telemetry.metrics.clone(),
    )
    .await?;
    let handle = start_server(&server, repository, store, telemetry.metrics.clone()).await?;
    let outcome = wait_with_telemetry(handle.wait(), telemetry).await;
    worker_handle.abort();
    outcome
}

async fn wait_with_telemetry(
    served: impl std::future::Future<Output = Result<(), mm_server::TransportError>>,
    telemetry: Telemetry,
) -> Result<(), String> {
    let outcome = served.await.map_err(|error| error.to_string());
    telemetry.scrape.shutdown().await;
    telemetry.sampler.abort();
    outcome
}

fn run<E, F>(
    parse: impl Fn(&dyn Fn(&str) -> Option<String>) -> Result<E, String>,
    drive: impl FnOnce(E) -> F,
) -> ExitCode
where
    F: std::future::Future<Output = Result<(), String>>,
{
    let env = match parse(&|name| std::env::var(name).ok()) {
        Ok(env) => env,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime builds");
    match runtime.block_on(drive(env)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Observability {
    bind: SocketAddr,
    interval: Duration,
    sample_timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AuthSettings {
    jwks_file: Option<PathBuf>,
    issuer: Option<String>,
    leeway: Duration,
}

impl Default for AuthSettings {
    fn default() -> Self {
        AuthSettings {
            jwks_file: None,
            issuer: None,
            leeway: Duration::from_secs(0),
        }
    }
}

fn observability(get: &dyn Fn(&str) -> Option<String>) -> Result<Observability, String> {
    let raw_bind = get("MM_METRICS_BIND_ADDR").unwrap_or_else(|| String::from("127.0.0.1:0"));
    let bind = raw_bind
        .parse()
        .map_err(|_| format!("MM_METRICS_BIND_ADDR is not a socket address: {raw_bind}"))?;
    Ok(Observability {
        bind,
        interval: seconds_var(get, "MM_METRICS_INTERVAL_SECS", 5)?,
        sample_timeout: seconds_var(get, "MM_METRICS_SAMPLE_TIMEOUT_SECS", 5)?,
    })
}

fn auth_settings(get: &dyn Fn(&str) -> Option<String>) -> Result<AuthSettings, String> {
    let jwks_file = get("MM_AUTH_JWKS_FILE").map(PathBuf::from);
    let issuer = get("MM_AUTH_ISSUER");
    let leeway = Duration::from_secs(match get("MM_AUTH_LEEWAY_SECS") {
        None => 0,
        Some(raw) => raw
            .parse::<u64>()
            .map_err(|_| format!("MM_AUTH_LEEWAY_SECS is not a number of seconds: {raw}"))?,
    });
    if jwks_file.is_none() && (issuer.is_some() || leeway > Duration::from_secs(0)) {
        return Err(String::from(
            "MM_AUTH_JWKS_FILE must be set when MM_AUTH_ISSUER or MM_AUTH_LEEWAY_SECS is",
        ));
    }
    Ok(AuthSettings {
        jwks_file,
        issuer,
        leeway,
    })
}

fn build_auth(settings: &AuthSettings) -> Result<Auth, String> {
    let Some(path) = &settings.jwks_file else {
        return Ok(Auth::open());
    };
    let document = std::fs::read_to_string(path)
        .map_err(|error| format!("{} could not be read: {error}", path.display()))?;
    let mut verifier_settings = Settings::none().with_leeway(settings.leeway);
    if let Some(issuer) = &settings.issuer {
        verifier_settings = verifier_settings.issued_by(issuer);
    }
    Auth::from_settings(Some(&document), verifier_settings)
        .map_err(|error| format!("{} is not a usable key document: {error}", path.display()))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_role(&args) {
        Ok(Role::Server) => run(server_env, serve),
        Ok(Role::Worker) => run(worker_env, work),
        Ok(Role::Combined) => run(
            |get| Ok((server_env(get)?, worker_env(get)?)),
            |(server, worker)| combine(server, worker),
        ),
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AuthSettings, QueueDepth, Role, ServerEnv, WorkerEnv, build_auth, parse_role, sample_once,
        server_env, worker_env,
    };
    use mm_domain::JobStoreError;
    use mm_observability::Registry;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    fn env_from(map: &HashMap<String, String>) -> Result<ServerEnv, String> {
        server_env(&|name| map.get(name).cloned())
    }

    fn worker_from(map: &HashMap<String, String>) -> Result<WorkerEnv, String> {
        worker_env(&|name| map.get(name).cloned())
    }

    fn complete() -> HashMap<String, String> {
        vars(&[
            ("MM_DATABASE_URL", "postgres://u:p@h:5/db"),
            ("MM_OBJECTSTORE_ENDPOINT", "http://h:1"),
            ("MM_OBJECTSTORE_BUCKET", "media"),
        ])
    }

    #[test]
    fn server_role_parses() {
        assert_eq!(parse_role(&args(&["server"])), Ok(Role::Server));
    }

    #[test]
    fn worker_role_parses() {
        assert_eq!(parse_role(&args(&["worker"])), Ok(Role::Worker));
    }

    #[test]
    fn combined_role_parses() {
        assert_eq!(parse_role(&args(&["combined"])), Ok(Role::Combined));
    }

    #[test]
    fn unknown_role_rejected() {
        assert!(parse_role(&args(&["observer"])).is_err());
    }

    #[test]
    fn missing_role_rejected() {
        assert!(parse_role(&args(&[])).is_err());
    }

    #[test]
    fn extra_arguments_rejected() {
        assert!(parse_role(&args(&["server", "worker"])).is_err());
    }

    #[test]
    fn a_complete_environment_parses_with_defaults() {
        let env = env_from(&complete()).expect("environment parses");
        assert_eq!(env.bind.to_string(), "127.0.0.1:0");
        assert!(env.notify);
        assert_eq!(env.stack.database_url, "postgres://u:p@h:5/db");
        assert_eq!(env.stack.store_endpoint, "http://h:1");
        assert_eq!(env.stack.store_bucket, "media");
        assert_eq!(env.stack.store_region, None);
        assert_eq!(env.stack.store_credentials, None);
        assert!(!env.stack.migrate);
    }

    #[test]
    fn migrations_are_applied_only_when_the_setting_asks_for_it() {
        let mut map = complete();
        map.insert("MM_MIGRATE".into(), "on".into());
        assert!(env_from(&map).expect("environment parses").stack.migrate);
        assert!(worker_from(&map).expect("environment parses").stack.migrate);
        map.insert("MM_MIGRATE".into(), "off".into());
        assert!(!env_from(&map).expect("environment parses").stack.migrate);
        map.insert("MM_MIGRATE".into(), "sometimes".into());
        assert!(env_from(&map).unwrap_err().contains("MM_MIGRATE"));
    }

    #[test]
    fn the_bind_address_is_configurable() {
        let mut map = complete();
        map.insert("MM_BIND_ADDR".into(), "0.0.0.0:7000".into());
        let env = env_from(&map).expect("environment parses");
        assert_eq!(env.bind.to_string(), "0.0.0.0:7000");
    }

    #[test]
    fn a_malformed_bind_address_is_rejected() {
        let mut map = complete();
        map.insert("MM_BIND_ADDR".into(), "nowhere".into());
        assert!(env_from(&map).unwrap_err().contains("MM_BIND_ADDR"));
    }

    #[test]
    fn each_required_variable_is_reported_when_missing() {
        for name in [
            "MM_DATABASE_URL",
            "MM_OBJECTSTORE_ENDPOINT",
            "MM_OBJECTSTORE_BUCKET",
        ] {
            let mut map = complete();
            map.remove(name);
            assert!(env_from(&map).unwrap_err().contains(name));
            assert!(worker_from(&map).unwrap_err().contains(name));
        }
    }

    #[test]
    fn static_credentials_require_both_halves() {
        let mut map = complete();
        map.insert("MM_OBJECTSTORE_ACCESS_KEY".into(), "a".into());
        assert!(env_from(&map).is_err());
        map.insert("MM_OBJECTSTORE_SECRET_KEY".into(), "s".into());
        let env = env_from(&map).expect("environment parses");
        assert_eq!(env.stack.store_credentials, Some(("a".into(), "s".into())));
    }

    #[test]
    fn the_region_passes_through() {
        let mut map = complete();
        map.insert("MM_OBJECTSTORE_REGION".into(), "r1".into());
        let env = env_from(&map).expect("environment parses");
        assert_eq!(env.stack.store_region, Some("r1".into()));
    }

    #[test]
    fn a_worker_environment_parses_with_defaults() {
        let env = worker_from(&complete()).expect("environment parses");
        assert!(env.worker_id.starts_with("worker-"));
        assert_eq!(env.staging_dir.to_string_lossy(), "scratch/worker");
        assert_eq!(env.max_concurrent, 4);
        assert_eq!(env.claim_batch, 4);
        assert_eq!(env.ladder_concurrency, 1);
        assert_eq!(env.lease, Duration::from_secs(30));
        assert_eq!(env.heartbeat_interval, Duration::from_secs(10));
        assert_eq!(env.reconcile_interval, Duration::from_secs(30));
        assert_eq!(env.progress_interval, Duration::from_secs(2));
        assert_eq!(env.default_deadline, Duration::from_secs(3600));
        assert_eq!(env.disk_budget_bytes, None);
        assert_eq!(env.retry_attempts, 3);
        assert_eq!(env.backoff_base, Duration::from_millis(1000));
        assert_eq!(env.backoff_cap, Duration::from_millis(60000));
        assert!(env.notify);
    }

    #[test]
    fn worker_settings_are_configurable() {
        let mut map = complete();
        map.insert("MM_WORKER_ID".into(), "w-7".into());
        map.insert("MM_WORKER_STAGING_DIR".into(), "scratch/stage".into());
        map.insert("MM_WORKER_MAX_CONCURRENT".into(), "2".into());
        map.insert("MM_WORKER_CLAIM_BATCH".into(), "8".into());
        map.insert("MM_WORKER_LEASE_SECS".into(), "15".into());
        map.insert("MM_WORKER_HEARTBEAT_SECS".into(), "5".into());
        map.insert("MM_WORKER_RECONCILE_SECS".into(), "7".into());
        map.insert("MM_WORKER_PROGRESS_SECS".into(), "1".into());
        map.insert("MM_WORKER_DEADLINE_SECS".into(), "600".into());
        map.insert("MM_WORKER_DISK_BUDGET_BYTES".into(), "1048576".into());
        map.insert("MM_WORKER_RETRY_ATTEMPTS".into(), "5".into());
        map.insert("MM_WORKER_BACKOFF_BASE_MS".into(), "250".into());
        map.insert("MM_WORKER_BACKOFF_CAP_MS".into(), "10000".into());
        map.insert("MM_WORKER_NOTIFY".into(), "off".into());
        let env = worker_from(&map).expect("environment parses");
        assert_eq!(env.worker_id, "w-7");
        assert_eq!(env.staging_dir.to_string_lossy(), "scratch/stage");
        assert_eq!(env.max_concurrent, 2);
        assert_eq!(env.claim_batch, 8);
        assert_eq!(env.lease, Duration::from_secs(15));
        assert_eq!(env.heartbeat_interval, Duration::from_secs(5));
        assert_eq!(env.reconcile_interval, Duration::from_secs(7));
        assert_eq!(env.progress_interval, Duration::from_secs(1));
        assert_eq!(env.default_deadline, Duration::from_secs(600));
        assert_eq!(env.disk_budget_bytes, Some(1_048_576));
        assert_eq!(env.retry_attempts, 5);
        assert_eq!(env.backoff_base, Duration::from_millis(250));
        assert_eq!(env.backoff_cap, Duration::from_millis(10000));
        assert!(!env.notify);
    }

    #[test]
    fn malformed_worker_numbers_are_rejected() {
        for name in [
            "MM_WORKER_MAX_CONCURRENT",
            "MM_WORKER_CLAIM_BATCH",
            "MM_WORKER_LEASE_SECS",
            "MM_WORKER_DISK_BUDGET_BYTES",
            "MM_WORKER_RETRY_ATTEMPTS",
            "MM_WORKER_BACKOFF_BASE_MS",
            "MM_WORKER_BACKOFF_CAP_MS",
        ] {
            let mut map = complete();
            map.insert(name.into(), "many".into());
            assert!(worker_from(&map).unwrap_err().contains(name), "{name}");
        }
    }

    #[test]
    fn zero_concurrency_is_rejected() {
        let mut map = complete();
        map.insert("MM_WORKER_MAX_CONCURRENT".into(), "0".into());
        assert!(worker_from(&map).is_err());
    }

    #[test]
    fn an_unknown_notify_value_is_rejected() {
        let mut map = complete();
        map.insert("MM_WORKER_NOTIFY".into(), "maybe".into());
        assert!(worker_from(&map).unwrap_err().contains("MM_WORKER_NOTIFY"));
    }

    #[test]
    fn the_queue_settings_default_to_absent() {
        let env = worker_from(&complete()).expect("environment parses");
        assert_eq!(env.queue_endpoint, None);
        assert_eq!(env.queue_region, None);
        assert_eq!(env.queue_credentials, None);
    }

    #[test]
    fn the_queue_settings_are_configurable() {
        let mut map = complete();
        map.insert("MM_QUEUE_ENDPOINT".into(), "http://h:9324".into());
        map.insert("MM_QUEUE_REGION".into(), "local".into());
        map.insert("MM_QUEUE_ACCESS_KEY".into(), "qa".into());
        map.insert("MM_QUEUE_SECRET_KEY".into(), "qs".into());
        let env = worker_from(&map).expect("environment parses");
        assert_eq!(env.queue_endpoint, Some("http://h:9324".into()));
        assert_eq!(env.queue_region, Some("local".into()));
        assert_eq!(env.queue_credentials, Some(("qa".into(), "qs".into())));
    }

    #[test]
    fn queue_credentials_require_both_halves() {
        let mut map = complete();
        map.insert("MM_QUEUE_ACCESS_KEY".into(), "qa".into());
        assert!(worker_from(&map).unwrap_err().contains("MM_QUEUE"));
    }

    #[test]
    fn the_server_notify_setting_parses_and_rejects_unknown_values() {
        let mut map = complete();
        map.insert("MM_SERVER_NOTIFY".into(), "off".into());
        let env = env_from(&map).expect("environment parses");
        assert!(!env.notify);
        map.insert("MM_SERVER_NOTIFY".into(), "maybe".into());
        assert!(env_from(&map).unwrap_err().contains("MM_SERVER_NOTIFY"));
    }

    #[test]
    fn a_depth_sample_that_never_returns_is_bounded_and_counted() {
        let (registry, metrics) = Registry::new();
        metrics.jobs_pending.set(11);
        let depth = Hanging;
        let outcome = block_on(sample_once(&depth, &metrics, Duration::from_millis(50)));
        assert_eq!(
            outcome,
            Err("the job store did not answer within 50ms".to_string())
        );
        assert_eq!(metrics.jobs_pending.value(), 11);
        assert_eq!(metrics.queue_depth_errors.value(), 1);
        assert!(registry.render().contains("mm_queue_depth_errors_total 1"));
    }

    #[test]
    fn a_sample_timeout_is_configurable_and_defaults_to_five_seconds() {
        let env = env_from(&complete()).expect("environment parses");
        assert_eq!(env.observability.sample_timeout, Duration::from_secs(5));
        let mut map = complete();
        map.insert("MM_METRICS_SAMPLE_TIMEOUT_SECS".into(), "2".into());
        let env = env_from(&map).expect("environment parses");
        assert_eq!(env.observability.sample_timeout, Duration::from_secs(2));
        let worker = worker_from(&map).expect("environment parses");
        assert_eq!(worker.observability.sample_timeout, Duration::from_secs(2));
        map.insert("MM_METRICS_SAMPLE_TIMEOUT_SECS".into(), "later".into());
        assert!(
            env_from(&map)
                .unwrap_err()
                .contains("MM_METRICS_SAMPLE_TIMEOUT_SECS")
        );
    }

    struct Hanging;

    #[async_trait::async_trait]
    impl QueueDepth for Hanging {
        async fn pending_count(&self) -> Result<u64, JobStoreError> {
            std::future::pending().await
        }
    }

    #[test]
    fn a_failed_depth_sample_counts_the_failure_and_keeps_the_last_value() {
        let (registry, metrics) = Registry::new();
        metrics.jobs_pending.set(3);
        let depth = Failing(Err("connection refused".to_string()));
        let outcome = block_on(sample_once(&depth, &metrics, Duration::from_secs(5)));
        assert_eq!(
            outcome,
            Err("job store backend error: connection refused".to_string())
        );
        assert_eq!(metrics.jobs_pending.value(), 3);
        assert_eq!(metrics.queue_depth_errors.value(), 1);
        assert!(registry.render().contains("mm_queue_depth_errors_total 1"));
    }

    #[test]
    fn a_depth_sample_that_answers_publishes_the_depth() {
        let (registry, metrics) = Registry::new();
        let depth = Failing(Ok(7));
        assert_eq!(
            block_on(sample_once(&depth, &metrics, Duration::from_secs(5))),
            Ok(())
        );
        assert_eq!(metrics.jobs_pending.value(), 7);
        assert_eq!(metrics.queue_depth_errors.value(), 0);
        assert!(registry.render().contains("mm_jobs_pending 7"));
    }

    struct Failing(Result<u64, String>);

    #[async_trait::async_trait]
    impl QueueDepth for Failing {
        async fn pending_count(&self) -> Result<u64, mm_domain::JobStoreError> {
            match &self.0 {
                Ok(count) => Ok(*count),
                Err(message) => Err(mm_domain::JobStoreError::Backend(message.clone())),
            }
        }
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime builds")
            .block_on(future)
    }

    #[test]
    fn the_observability_settings_default_to_a_loopback_port_and_a_five_second_tick() {
        let env = env_from(&complete()).expect("environment parses");
        assert_eq!(env.observability.bind.to_string(), "127.0.0.1:0");
        assert_eq!(env.observability.interval, Duration::from_secs(5));
        let worker = worker_from(&complete()).expect("environment parses");
        assert_eq!(worker.observability.bind.to_string(), "127.0.0.1:0");
        assert_eq!(worker.observability.interval, Duration::from_secs(5));
    }

    #[test]
    fn the_observability_settings_are_configurable() {
        let mut map = complete();
        map.insert("MM_METRICS_BIND_ADDR".into(), "0.0.0.0:9100".into());
        map.insert("MM_METRICS_INTERVAL_SECS".into(), "2".into());
        let env = env_from(&map).expect("environment parses");
        assert_eq!(env.observability.bind.to_string(), "0.0.0.0:9100");
        assert_eq!(env.observability.interval, Duration::from_secs(2));
        let worker = worker_from(&map).expect("environment parses");
        assert_eq!(worker.observability.bind.to_string(), "0.0.0.0:9100");
        assert_eq!(worker.observability.interval, Duration::from_secs(2));
    }

    #[test]
    fn a_malformed_metrics_setting_is_rejected_by_both_roles() {
        let mut map = complete();
        map.insert("MM_METRICS_BIND_ADDR".into(), "nowhere".into());
        assert!(env_from(&map).unwrap_err().contains("MM_METRICS_BIND_ADDR"));
        assert!(
            worker_from(&map)
                .unwrap_err()
                .contains("MM_METRICS_BIND_ADDR")
        );
        let mut map = complete();
        map.insert("MM_METRICS_INTERVAL_SECS".into(), "soon".into());
        assert!(
            env_from(&map)
                .unwrap_err()
                .contains("MM_METRICS_INTERVAL_SECS")
        );
        assert!(
            worker_from(&map)
                .unwrap_err()
                .contains("MM_METRICS_INTERVAL_SECS")
        );
    }

    #[test]
    fn the_auth_settings_default_to_no_published_key() {
        let env = env_from(&complete()).expect("environment parses");
        assert_eq!(env.auth, AuthSettings::default());
        assert_eq!(env.auth.jwks_file, None);
        assert_eq!(env.auth.issuer, None);
        assert_eq!(env.auth.leeway, Duration::from_secs(0));
    }

    #[test]
    fn the_auth_settings_are_configurable() {
        let mut map = complete();
        map.insert("MM_AUTH_JWKS_FILE".into(), "scratch/jwks.json".into());
        map.insert("MM_AUTH_ISSUER".into(), "https://issuer.example".into());
        map.insert("MM_AUTH_LEEWAY_SECS".into(), "45".into());
        let env = env_from(&map).expect("environment parses");
        assert_eq!(env.auth.jwks_file, Some(PathBuf::from("scratch/jwks.json")));
        assert_eq!(env.auth.issuer.as_deref(), Some("https://issuer.example"));
        assert_eq!(env.auth.leeway, Duration::from_secs(45));
    }

    #[test]
    fn a_malformed_auth_leeway_is_rejected() {
        let mut map = complete();
        map.insert("MM_AUTH_JWKS_FILE".into(), "scratch/jwks.json".into());
        map.insert("MM_AUTH_LEEWAY_SECS".into(), "later".into());
        assert!(env_from(&map).unwrap_err().contains("MM_AUTH_LEEWAY_SECS"));
    }

    #[test]
    fn the_issuer_without_a_published_key_is_rejected() {
        let mut map = complete();
        map.insert("MM_AUTH_ISSUER".into(), "https://issuer.example".into());
        assert!(env_from(&map).unwrap_err().contains("MM_AUTH_JWKS_FILE"));
        let mut map = complete();
        map.insert("MM_AUTH_LEEWAY_SECS".into(), "10".into());
        assert!(env_from(&map).unwrap_err().contains("MM_AUTH_JWKS_FILE"));
    }

    #[test]
    fn a_published_key_document_is_read_into_a_verifier() {
        let issuer = mm_auth::testing::Issuer::seeded("key-1", 7);
        let path = mm_testkit::scratch_path("auth").join("jwks.json");
        std::fs::write(&path, issuer.jwks()).expect("key document written");
        let settings = AuthSettings {
            jwks_file: Some(path.clone()),
            issuer: Some("test-issuer".to_string()),
            leeway: Duration::from_secs(5),
        };
        let auth = build_auth(&settings).expect("the published key document loads");
        assert!(auth.verifies_tokens());
        let token = issuer.token("owner-1", SystemTime::now(), Duration::from_secs(300));
        assert!(auth.verify(&token).is_ok());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_missing_or_unreadable_key_document_fails_the_daemon() {
        let missing = AuthSettings {
            jwks_file: Some(PathBuf::from("scratch/auth/absent.json")),
            ..AuthSettings::default()
        };
        assert!(build_auth(&missing).is_err());
        let path = mm_testkit::scratch_path("auth").join("broken.json");
        std::fs::write(&path, "not json").expect("key document written");
        let broken = AuthSettings {
            jwks_file: Some(path.clone()),
            ..AuthSettings::default()
        };
        assert!(build_auth(&broken).is_err());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_published_shared_secret_is_refused_before_the_server_binds() {
        let path = mm_testkit::scratch_path("auth").join("oct.json");
        std::fs::write(
            &path,
            r#"{"keys":[{"kty":"oct","kid":"key-1","alg":"HS256","k":"c2hhcmVkLXNlY3JldA"}]}"#,
        )
        .expect("key document written");
        let settings = AuthSettings {
            jwks_file: Some(path.clone()),
            ..AuthSettings::default()
        };
        assert!(build_auth(&settings).is_err());
        std::fs::remove_file(&path).ok();
    }
}

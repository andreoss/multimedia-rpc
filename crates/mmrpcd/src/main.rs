use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use mm_domain::WorkerId;
use mm_ffmpeg::Prober;
use mm_jobstore::{PgJobListener, PgJobRepository};
use mm_server::ServerConfig;
use mm_storage::{CredentialSource, S3ObjectStore, S3Settings};
use mm_worker::{DiskBudget, WorkerConfig, WorkerHandle};
use sqlx::postgres::PgPoolOptions;

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerEnv {
    bind: SocketAddr,
    stack: StackEnv,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkerEnv {
    stack: StackEnv,
    worker_id: String,
    staging_dir: PathBuf,
    max_concurrent: u32,
    claim_batch: u32,
    lease: Duration,
    heartbeat_interval: Duration,
    reconcile_interval: Duration,
    progress_interval: Duration,
    default_deadline: Duration,
    disk_budget_bytes: Option<u64>,
    notify: bool,
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
    })
}

fn server_env(get: &dyn Fn(&str) -> Option<String>) -> Result<ServerEnv, String> {
    let raw_bind = get("MM_BIND_ADDR").unwrap_or_else(|| String::from("127.0.0.1:0"));
    let bind = raw_bind
        .parse()
        .map_err(|_| format!("MM_BIND_ADDR is not a socket address: {raw_bind}"))?;
    Ok(ServerEnv {
        bind,
        stack: stack_env(get)?,
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
    let notify = match get("MM_WORKER_NOTIFY").as_deref() {
        None | Some("on") => true,
        Some("off") => false,
        Some(other) => return Err(format!("MM_WORKER_NOTIFY must be on or off: {other}")),
    };
    Ok(WorkerEnv {
        stack,
        worker_id,
        staging_dir,
        max_concurrent: positive_var(get, "MM_WORKER_MAX_CONCURRENT", 4)?,
        claim_batch: positive_var(get, "MM_WORKER_CLAIM_BATCH", 4)?,
        lease: seconds_var(get, "MM_WORKER_LEASE_SECS", 30)?,
        heartbeat_interval: seconds_var(get, "MM_WORKER_HEARTBEAT_SECS", 10)?,
        reconcile_interval: seconds_var(get, "MM_WORKER_RECONCILE_SECS", 30)?,
        progress_interval: seconds_var(get, "MM_WORKER_PROGRESS_SECS", 2)?,
        default_deadline: seconds_var(get, "MM_WORKER_DEADLINE_SECS", 3600)?,
        disk_budget_bytes,
        notify,
    })
}

async fn connect_stack(
    stack: &StackEnv,
) -> Result<(Arc<PgJobRepository>, Arc<S3ObjectStore>), String> {
    let pool = PgPoolOptions::new()
        .connect(&stack.database_url)
        .await
        .map_err(|error| format!("database connection failed: {error}"))?;
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

async fn start_server(env: &ServerEnv) -> Result<mm_server::ServerHandle, String> {
    let (repository, store) = connect_stack(&env.stack).await?;
    let mut config = ServerConfig::new(
        repository,
        store,
        env.stack.store_bucket.clone(),
        Prober::default(),
    )
    .map_err(|error| error.to_string())?;
    config.bind = env.bind;
    mm_server::start(config)
        .await
        .map_err(|error| error.to_string())
}

async fn start_worker(env: &WorkerEnv) -> Result<WorkerHandle, String> {
    let (repository, store) = connect_stack(&env.stack).await?;
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
    config.lease = env.lease;
    config.heartbeat_interval = env.heartbeat_interval;
    config.reconcile_interval = env.reconcile_interval;
    config.progress_interval = env.progress_interval;
    config.default_deadline = env.default_deadline;
    if let Some(bytes) = env.disk_budget_bytes {
        config.disk_budget = DiskBudget::Fixed(bytes);
    }
    if env.notify {
        let listener = PgJobListener::connect(&env.stack.database_url)
            .await
            .map_err(|error| format!("notification listener failed: {error}"))?;
        config.notifications = Some(Arc::new(listener));
    }
    let handle = mm_worker::start(config);
    println!("worker {} ready", env.worker_id);
    Ok(handle)
}

async fn serve(env: ServerEnv) -> Result<(), String> {
    let handle = start_server(&env).await?;
    handle.wait().await.map_err(|error| error.to_string())
}

async fn work(env: WorkerEnv) -> Result<(), String> {
    let handle = start_worker(&env).await?;
    handle.wait().await;
    Ok(())
}

async fn combine(server: ServerEnv, worker: WorkerEnv) -> Result<(), String> {
    let _worker_handle = start_worker(&worker).await?;
    serve(server).await
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
    use super::{Role, ServerEnv, WorkerEnv, parse_role, server_env, worker_env};
    use std::collections::HashMap;
    use std::time::Duration;

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
        assert_eq!(env.stack.database_url, "postgres://u:p@h:5/db");
        assert_eq!(env.stack.store_endpoint, "http://h:1");
        assert_eq!(env.stack.store_bucket, "media");
        assert_eq!(env.stack.store_region, None);
        assert_eq!(env.stack.store_credentials, None);
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
        assert_eq!(env.lease, Duration::from_secs(30));
        assert_eq!(env.heartbeat_interval, Duration::from_secs(10));
        assert_eq!(env.reconcile_interval, Duration::from_secs(30));
        assert_eq!(env.progress_interval, Duration::from_secs(2));
        assert_eq!(env.default_deadline, Duration::from_secs(3600));
        assert_eq!(env.disk_budget_bytes, None);
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
        assert!(!env.notify);
    }

    #[test]
    fn malformed_worker_numbers_are_rejected() {
        for name in [
            "MM_WORKER_MAX_CONCURRENT",
            "MM_WORKER_CLAIM_BATCH",
            "MM_WORKER_LEASE_SECS",
            "MM_WORKER_DISK_BUDGET_BYTES",
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
}

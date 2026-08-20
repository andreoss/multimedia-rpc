use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use mm_ffmpeg::Prober;
use mm_jobstore::PgJobRepository;
use mm_server::ServerConfig;
use mm_storage::{CredentialSource, S3ObjectStore, S3Settings};
use sqlx::postgres::PgPoolOptions;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Server,
    Worker,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Role::Server => "server",
            Role::Worker => "worker",
        }
    }
}

fn parse_role(args: &[String]) -> Result<Role, String> {
    match args {
        [role] if role == "server" => Ok(Role::Server),
        [role] if role == "worker" => Ok(Role::Worker),
        _ => Err(String::from("usage: mmrpcd <server|worker>")),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerEnv {
    bind: SocketAddr,
    database_url: String,
    store_endpoint: String,
    store_bucket: String,
    store_region: Option<String>,
    store_credentials: Option<(String, String)>,
}

fn server_env(get: &dyn Fn(&str) -> Option<String>) -> Result<ServerEnv, String> {
    let raw_bind = get("MM_BIND_ADDR").unwrap_or_else(|| String::from("127.0.0.1:0"));
    let bind = raw_bind
        .parse()
        .map_err(|_| format!("MM_BIND_ADDR is not a socket address: {raw_bind}"))?;
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
    Ok(ServerEnv {
        bind,
        database_url,
        store_endpoint,
        store_bucket,
        store_region: get("MM_OBJECTSTORE_REGION"),
        store_credentials,
    })
}

async fn serve(env: ServerEnv) -> Result<(), String> {
    let pool = PgPoolOptions::new()
        .connect(&env.database_url)
        .await
        .map_err(|error| format!("database connection failed: {error}"))?;
    let repository = Arc::new(PgJobRepository::new(pool));
    let credentials = match env.store_credentials {
        Some((access_key_id, secret_access_key)) => CredentialSource::Static {
            access_key_id,
            secret_access_key,
        },
        None => CredentialSource::DefaultChain,
    };
    let store = Arc::new(
        S3ObjectStore::connect(S3Settings {
            endpoint: Some(env.store_endpoint),
            region: env.store_region,
            force_path_style: true,
            credentials,
        })
        .await,
    );
    let mut config = ServerConfig::new(repository, store, env.store_bucket, Prober::default())
        .map_err(|error| error.to_string())?;
    config.bind = env.bind;
    let handle = mm_server::start(config)
        .await
        .map_err(|error| error.to_string())?;
    handle.wait().await.map_err(|error| error.to_string())
}

fn run_server() -> ExitCode {
    let env = match server_env(&|name| std::env::var(name).ok()) {
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
    match runtime.block_on(serve(env)) {
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
        Ok(Role::Server) => run_server(),
        Ok(role) => {
            println!("{}", role.name());
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Role, ServerEnv, parse_role, server_env};
    use std::collections::HashMap;

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
    fn role_names_render() {
        assert_eq!(Role::Server.name(), "server");
        assert_eq!(Role::Worker.name(), "worker");
    }

    #[test]
    fn a_complete_environment_parses_with_defaults() {
        let env = env_from(&complete()).expect("environment parses");
        assert_eq!(env.bind.to_string(), "127.0.0.1:0");
        assert_eq!(env.database_url, "postgres://u:p@h:5/db");
        assert_eq!(env.store_endpoint, "http://h:1");
        assert_eq!(env.store_bucket, "media");
        assert_eq!(env.store_region, None);
        assert_eq!(env.store_credentials, None);
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
        }
    }

    #[test]
    fn static_credentials_require_both_halves() {
        let mut map = complete();
        map.insert("MM_OBJECTSTORE_ACCESS_KEY".into(), "a".into());
        assert!(env_from(&map).is_err());
        map.insert("MM_OBJECTSTORE_SECRET_KEY".into(), "s".into());
        let env = env_from(&map).expect("environment parses");
        assert_eq!(env.store_credentials, Some(("a".into(), "s".into())));
    }

    #[test]
    fn the_region_passes_through() {
        let mut map = complete();
        map.insert("MM_OBJECTSTORE_REGION".into(), "r1".into());
        let env = env_from(&map).expect("environment parses");
        assert_eq!(env.store_region, Some("r1".into()));
    }
}

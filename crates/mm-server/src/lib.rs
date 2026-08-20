mod health;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use mm_domain::{JobRepository, MediaRefError, ObjectRef, ObjectStore};
use mm_ffmpeg::Prober;
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

use health::Dependencies;

pub const SERVICE_NAME: &str = "mmrpc.v1.MediaJobService";

const DEFAULT_BIND: ([u8; 4], u16) = ([127, 0, 0, 1], 0);

pub struct ServerConfig {
    pub bind: SocketAddr,
    pub health_interval: Duration,
    pub check_timeout: Duration,
    repository: Arc<dyn JobRepository>,
    store: Arc<dyn ObjectStore>,
    store_probe: ObjectRef,
    prober: Prober,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("object store bucket is invalid: {0}")]
    Bucket(#[from] MediaRefError),
}

#[derive(Debug, Error)]
pub enum StartError {
    #[error("server failed to bind: {0}")]
    Bind(#[from] std::io::Error),
}

impl ServerConfig {
    pub fn new(
        repository: Arc<dyn JobRepository>,
        store: Arc<dyn ObjectStore>,
        store_bucket: String,
        prober: Prober,
    ) -> Result<Self, ConfigError> {
        let store_probe = ObjectRef::new(&store_bucket, ".mm-health-probe")?;
        Ok(Self {
            bind: SocketAddr::from(DEFAULT_BIND),
            health_interval: Duration::from_secs(1),
            check_timeout: Duration::from_secs(2),
            repository,
            store,
            store_probe,
            prober,
        })
    }
}

pub struct ServerHandle {
    local_addr: SocketAddr,
    shutdown: oneshot::Sender<()>,
    server: JoinHandle<Result<(), tonic::transport::Error>>,
    health: JoinHandle<()>,
}

impl ServerHandle {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub async fn wait(self) -> Result<(), tonic::transport::Error> {
        let served = self.server.await.expect("server task finishes");
        self.health.abort();
        served
    }

    pub async fn shutdown(self) {
        self.health.abort();
        let _ = self.shutdown.send(());
        let _ = self.server.await;
    }
}

pub async fn start(config: ServerConfig) -> Result<ServerHandle, StartError> {
    let listener = TcpListener::bind(config.bind).await?;
    let local_addr = listener.local_addr()?;
    let (reporter, health_service) = tonic_health::server::health_reporter();
    let dependencies = Dependencies::new(
        config.repository,
        config.store,
        config.store_probe,
        config.prober,
        config.check_timeout,
    );
    let health = tokio::spawn(health::watch(
        reporter,
        dependencies,
        config.health_interval,
    ));
    let (shutdown, shutdown_rx) = oneshot::channel::<()>();
    let router = Server::builder().add_service(health_service);
    let server = tokio::spawn(router.serve_with_incoming_shutdown(
        TcpListenerStream::new(listener),
        async move {
            let _ = shutdown_rx.await;
        },
    ));
    println!("listening {local_addr}");
    Ok(ServerHandle {
        local_addr,
        shutdown,
        server,
        health,
    })
}

mod cursor;
mod health;
mod interceptor;
mod service;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use mm_domain::{JobEventNotifications, JobRepository, MediaRefError, ObjectRef, ObjectStore};
use mm_ffmpeg::Prober;
use mm_proto::v1::media_job_service_server::MediaJobServiceServer;
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::server::NamedService;
use tonic::transport::Server;

use health::Dependencies;
use service::{MediaJobs, WatchHub};

pub use interceptor::{Auth, Interceptor, Owner};

pub const SERVICE_NAME: &str = <MediaJobServiceServer<MediaJobs> as NamedService>::NAME;

const DEFAULT_BIND: ([u8; 4], u16) = ([127, 0, 0, 1], 0);

pub struct ServerConfig {
    pub bind: SocketAddr,
    pub health_interval: Duration,
    pub check_timeout: Duration,
    pub watch_poll_interval: Duration,
    pub job_events: Option<Arc<dyn JobEventNotifications>>,
    pub auth: Auth,
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
            watch_poll_interval: Duration::from_secs(1),
            job_events: None,
            auth: Auth::open(),
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
    watch_feed: Option<JoinHandle<()>>,
}

impl ServerHandle {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    fn stop_watch_feed(&self) {
        if let Some(feed) = &self.watch_feed {
            feed.abort();
        }
    }

    pub async fn wait(self) -> Result<(), tonic::transport::Error> {
        let served = self.server.await.expect("server task finishes");
        self.health.abort();
        served
    }

    pub async fn shutdown(self) {
        self.health.abort();
        self.stop_watch_feed();
        let _ = self.shutdown.send(());
        let _ = self.server.await;
    }
}

pub async fn start(config: ServerConfig) -> Result<ServerHandle, StartError> {
    let listener = TcpListener::bind(config.bind).await?;
    let local_addr = listener.local_addr()?;
    let (reporter, health_service) = tonic_health::server::health_reporter();
    let dependencies = Dependencies::new(
        config.repository.clone(),
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
    let (watch, watch_feed) = WatchHub::start(config.job_events, config.watch_poll_interval);
    let router =
        Server::builder()
            .add_service(health_service)
            .add_service(MediaJobServiceServer::new(Interceptor::new(
                config.auth,
                MediaJobs::new(config.repository, watch),
            )));
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
        watch_feed,
    })
}

mod budget;
mod packer;
mod pipeline;
mod report;
mod retry;

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mm_domain::{
    CallbackPublisher, CancelNotifications, JobNotifications, JobRepository, ObjectStore, WorkerId,
};
use mm_ffmpeg::{ProbeLimits, Prober, Runner};
use mm_magick::{Tool as ImageTool, ToolLimits as ImageLimits};
use mm_observability::{Metrics, Registry};
use tokio::sync::{Notify, Semaphore, watch};
use tokio::task::{JoinHandle, JoinSet};

pub use budget::{DiskBudget, admits};
pub use packer::{GstRungPacker, LadderPackError, LadderPacked, RungPacker, packer_name};
pub use pipeline::codes;
pub use report::{blocks, input_report, render_verdicts, still_report, target_profile, warnings};
pub use retry::{RetryPolicy, is_transient};

pub struct WorkerConfig {
    pub repository: Arc<dyn JobRepository>,
    pub store: Arc<dyn ObjectStore>,
    pub bucket: String,
    pub worker_id: WorkerId,
    pub staging_dir: PathBuf,
    pub prober: Prober,
    pub runner: Runner,
    pub probe_limits: ProbeLimits,
    pub image_tool: ImageTool,
    pub image_limits: ImageLimits,
    pub max_concurrent: usize,
    pub claim_batch: NonZeroU32,
    pub lease: Duration,
    pub heartbeat_interval: Duration,
    pub reconcile_interval: Duration,
    pub progress_interval: Duration,
    pub default_deadline: Duration,
    pub stderr_tail_bytes: usize,
    pub disk_budget: DiskBudget,
    pub release_retry: Duration,
    pub retry: RetryPolicy,
    pub notifications: Option<Arc<dyn JobNotifications>>,
    pub cancellations: Option<Arc<dyn CancelNotifications>>,
    pub callbacks: Option<Arc<dyn CallbackPublisher>>,
    pub metrics: Metrics,
    pub ladder_packer: Option<Arc<dyn RungPacker>>,
    reserved: std::sync::Mutex<u64>,
}

impl WorkerConfig {
    pub fn new(
        repository: Arc<dyn JobRepository>,
        store: Arc<dyn ObjectStore>,
        bucket: String,
        worker_id: WorkerId,
        staging_dir: PathBuf,
    ) -> Self {
        Self {
            repository,
            store,
            bucket,
            worker_id,
            staging_dir,
            prober: Prober::default(),
            runner: Runner::default(),
            probe_limits: ProbeLimits::default(),
            image_tool: ImageTool::default(),
            image_limits: ImageLimits::default(),
            max_concurrent: 4,
            claim_batch: NonZeroU32::new(4).expect("positive batch"),
            lease: Duration::from_secs(30),
            heartbeat_interval: Duration::from_secs(10),
            reconcile_interval: Duration::from_secs(30),
            progress_interval: Duration::from_secs(2),
            default_deadline: Duration::from_secs(3600),
            stderr_tail_bytes: 4096,
            disk_budget: DiskBudget::Probe,
            release_retry: Duration::from_secs(30),
            retry: RetryPolicy::default(),
            notifications: None,
            cancellations: None,
            callbacks: None,
            metrics: Registry::new().1,
            ladder_packer: None,
            reserved: std::sync::Mutex::new(0),
        }
    }

    fn ledger(&self) -> &std::sync::Mutex<u64> {
        &self.reserved
    }
}

pub struct WorkerHandle {
    shutdown: watch::Sender<bool>,
    main: JoinHandle<()>,
}

impl WorkerHandle {
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        let _ = self.main.await;
    }

    pub fn abort(self) {
        self.main.abort();
    }

    pub async fn wait(self) {
        let _ = self.main.await;
    }
}

pub fn start(config: WorkerConfig) -> WorkerHandle {
    let (shutdown, shutdown_rx) = watch::channel(false);
    let main = tokio::spawn(run_loop(Arc::new(config), shutdown_rx));
    WorkerHandle { shutdown, main }
}

async fn run_loop(ctx: Arc<WorkerConfig>, mut shutdown: watch::Receiver<bool>) {
    let semaphore = Arc::new(Semaphore::new(ctx.max_concurrent));
    let wake = Arc::new(Notify::new());
    let registry = Arc::new(pipeline::CancelRegistry::new());
    let mut jobs = JoinSet::new();
    let watcher = ctx.cancellations.clone().map(|listener| {
        let registry = registry.clone();
        tokio::spawn(async move {
            loop {
                if let Ok(id) = listener.cancelled().await {
                    registry.cancel(id);
                }
            }
        })
    });
    loop {
        let _ = ctx.repository.requeue_expired().await;
        let _ = ctx.repository.fail_overdue().await;
        publish_unpublished(&ctx).await;
        claim_available(&ctx, &semaphore, &wake, &registry, &mut jobs).await;
        while jobs.try_join_next().is_some() {}
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            _ = wake.notified() => {}
            _ = job_ready(ctx.notifications.as_deref()) => {}
            _ = tokio::time::sleep(ctx.reconcile_interval) => {}
        }
    }
    while jobs.join_next().await.is_some() {}
    if let Some(watcher) = watcher {
        watcher.abort();
    }
}

async fn publish_unpublished(ctx: &Arc<WorkerConfig>) {
    if ctx.callbacks.is_none() {
        return;
    }
    let Ok(records) = ctx.repository.unpublished_callbacks(ctx.claim_batch).await else {
        return;
    };
    for record in records {
        pipeline::publish_outcome(ctx, &record).await;
    }
}

async fn job_ready(notifications: Option<&dyn JobNotifications>) {
    match notifications {
        Some(notifications) => {
            if notifications.notified().await.is_err() {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
        None => std::future::pending().await,
    }
}

async fn claim_available(
    ctx: &Arc<WorkerConfig>,
    semaphore: &Arc<Semaphore>,
    wake: &Arc<Notify>,
    registry: &Arc<pipeline::CancelRegistry>,
    jobs: &mut JoinSet<()>,
) {
    loop {
        let permits = semaphore.available_permits();
        if permits == 0 {
            return;
        }
        let want = (permits as u32).min(ctx.claim_batch.get());
        let batch = NonZeroU32::new(want).expect("positive claim size");
        let claimed = match ctx.repository.claim(&ctx.worker_id, batch, ctx.lease).await {
            Ok(claimed) => claimed,
            Err(_) => return,
        };
        let drained = (claimed.len() as u32) < want;
        for job in claimed {
            let permit = semaphore
                .clone()
                .try_acquire_owned()
                .expect("claim size never exceeds available permits");
            let ctx = ctx.clone();
            let wake = wake.clone();
            let registry = registry.clone();
            jobs.spawn(async move {
                let _permit = permit;
                pipeline::execute(&ctx, job, &registry).await;
                wake.notify_one();
            });
        }
        if drained {
            return;
        }
    }
}

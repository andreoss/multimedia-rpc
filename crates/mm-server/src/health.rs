use std::sync::Arc;
use std::time::Duration;

use mm_domain::{JobId, JobRepository, ObjectRef, ObjectStore, ObjectStoreError};
use mm_ffmpeg::Prober;
use tonic_health::ServingStatus;
use tonic_health::server::HealthReporter;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dependency {
    JobStore,
    ObjectStore,
    ProbeBinary,
}

impl Dependency {
    fn name(self) -> &'static str {
        match self {
            Dependency::JobStore => "job store",
            Dependency::ObjectStore => "object store",
            Dependency::ProbeBinary => "probe binary",
        }
    }
}

pub(crate) struct Dependencies {
    repository: Arc<dyn JobRepository>,
    store: Arc<dyn ObjectStore>,
    store_probe: ObjectRef,
    prober: Prober,
    timeout: Duration,
}

impl Dependencies {
    pub(crate) fn new(
        repository: Arc<dyn JobRepository>,
        store: Arc<dyn ObjectStore>,
        store_probe: ObjectRef,
        prober: Prober,
        timeout: Duration,
    ) -> Self {
        Self {
            repository,
            store,
            store_probe,
            prober,
            timeout,
        }
    }

    async fn job_store_healthy(&self) -> bool {
        matches!(
            tokio::time::timeout(self.timeout, self.repository.get(JobId::generate())).await,
            Ok(Ok(_))
        )
    }

    async fn object_store_healthy(&self) -> bool {
        matches!(
            tokio::time::timeout(self.timeout, self.store.head(&self.store_probe)).await,
            Ok(Ok(_)) | Ok(Err(ObjectStoreError::NotFound { .. }))
        )
    }

    async fn prober_healthy(&self) -> bool {
        self.prober.version(self.timeout).await.is_ok()
    }

    pub(crate) async fn unhealthy(&self) -> Option<Dependency> {
        if !self.job_store_healthy().await {
            return Some(Dependency::JobStore);
        }
        if !self.object_store_healthy().await {
            return Some(Dependency::ObjectStore);
        }
        if !self.prober_healthy().await {
            return Some(Dependency::ProbeBinary);
        }
        None
    }
}

fn log_line(message: std::fmt::Arguments<'_>) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "{message}");
}

async fn report(reporter: &HealthReporter, status: ServingStatus) {
    reporter.set_service_status("", status).await;
    reporter
        .set_service_status(crate::SERVICE_NAME, status)
        .await;
}

fn status_of(unhealthy: Option<Dependency>) -> ServingStatus {
    if unhealthy.is_some() {
        ServingStatus::NotServing
    } else {
        ServingStatus::Serving
    }
}

pub(crate) async fn watch(
    reporter: HealthReporter,
    dependencies: Dependencies,
    interval: Duration,
) {
    report(&reporter, ServingStatus::NotServing).await;
    loop {
        let unhealthy = dependencies.unhealthy().await;
        if let Some(dependency) = unhealthy {
            log_line(format_args!(
                "health: the {} is not answering",
                dependency.name()
            ));
        }
        report(&reporter, status_of(unhealthy)).await;
        tokio::time::sleep(interval).await;
    }
}

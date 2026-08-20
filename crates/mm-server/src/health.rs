use std::sync::Arc;
use std::time::Duration;

use mm_domain::{JobId, JobRepository, ObjectRef, ObjectStore, ObjectStoreError};
use mm_ffmpeg::Prober;
use tonic_health::ServingStatus;
use tonic_health::server::HealthReporter;

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

    pub(crate) async fn healthy(&self) -> bool {
        self.job_store_healthy().await
            && self.object_store_healthy().await
            && self.prober_healthy().await
    }
}

pub(crate) async fn watch(
    reporter: HealthReporter,
    dependencies: Dependencies,
    interval: Duration,
) {
    loop {
        let status = if dependencies.healthy().await {
            ServingStatus::Serving
        } else {
            ServingStatus::NotServing
        };
        reporter.set_service_status("", status).await;
        reporter
            .set_service_status(crate::SERVICE_NAME, status)
            .await;
        tokio::time::sleep(interval).await;
    }
}

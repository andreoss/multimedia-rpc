use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use mm_domain::{MediaRef, ObjectBody, ObjectRef, ObjectStore, ObjectStoreError, collect_body};

struct FlakyStore {
    puts: AtomicU32,
    failures_before_success: u32,
    stored: tokio::sync::Mutex<Vec<(String, Vec<u8>, u64)>>,
}

impl FlakyStore {
    fn failing(failures_before_success: u32) -> Self {
        Self {
            puts: AtomicU32::new(0),
            failures_before_success,
            stored: tokio::sync::Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl ObjectStore for FlakyStore {
    async fn get(&self, _object: &ObjectRef) -> Result<ObjectBody, ObjectStoreError> {
        Err(ObjectStoreError::Backend {
            message: "unused".to_string(),
        })
    }

    async fn put(
        &self,
        object: &ObjectRef,
        body: ObjectBody,
        length: u64,
    ) -> Result<(), ObjectStoreError> {
        let attempt = self.puts.fetch_add(1, Ordering::SeqCst) + 1;
        let payload = collect_body(body).await?;
        if attempt <= self.failures_before_success {
            return Err(ObjectStoreError::Backend {
                message: format!("injected failure {attempt}"),
            });
        }
        self.stored
            .lock()
            .await
            .push((object.key().to_string(), payload.to_vec(), length));
        Ok(())
    }

    async fn put_multipart(
        &self,
        _object: &ObjectRef,
        _body: ObjectBody,
    ) -> Result<(), ObjectStoreError> {
        Err(ObjectStoreError::Backend {
            message: "unused".to_string(),
        })
    }

    async fn head(&self, _object: &ObjectRef) -> Result<u64, ObjectStoreError> {
        Err(ObjectStoreError::Backend {
            message: "unused".to_string(),
        })
    }

    async fn delete(&self, _object: &ObjectRef) -> Result<(), ObjectStoreError> {
        Err(ObjectStoreError::Backend {
            message: "unused".to_string(),
        })
    }

    async fn presign_get(
        &self,
        _object: &ObjectRef,
        _expires_in: Duration,
    ) -> Result<String, ObjectStoreError> {
        Err(ObjectStoreError::Backend {
            message: "unused".to_string(),
        })
    }
}

fn payload_file(name: &str, bytes: &[u8]) -> PathBuf {
    let dir = mm_testkit::scratch_path("testkit/uploads");
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("payload written");
    path
}

#[tokio::test]
async fn an_upload_survives_two_transient_put_failures() {
    let store = FlakyStore::failing(2);
    let payload: Vec<u8> = (0u32..200_000).map(|value| value as u8).collect();
    let path = payload_file("retry.bin", &payload);

    let media = mm_testkit::upload_object(&store, &path, "bucket-a", "inputs/retry.bin").await;

    assert_eq!(store.puts.load(Ordering::SeqCst), 3);
    let stored = store.stored.lock().await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].0, "inputs/retry.bin");
    assert_eq!(stored[0].1, payload);
    assert_eq!(stored[0].2, payload.len() as u64);
    assert_eq!(
        media,
        MediaRef::object_uri("s3://bucket-a/inputs/retry.bin").expect("valid ref")
    );
}

#[tokio::test]
async fn a_first_try_upload_puts_exactly_once() {
    let store = FlakyStore::failing(0);
    let path = payload_file("clean.bin", b"payload");

    mm_testkit::upload_object(&store, &path, "bucket-b", "inputs/clean.bin").await;

    assert_eq!(store.puts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
#[should_panic(expected = "fixture upload failed after 3 attempts")]
async fn a_persistent_outage_panics_after_three_attempts() {
    let store = FlakyStore::failing(u32::MAX);
    let path = payload_file("doomed.bin", b"payload");

    mm_testkit::upload_object(&store, &path, "bucket-c", "inputs/doomed.bin").await;
}

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use mm_domain::{
    JobId, JobPrefix, JobPrefixError, ObjectBody, ObjectRef, ObjectStore, ObjectStoreError,
    OutputKey, OutputKeyError, body_from_bytes, collect_body,
};

struct MemoryStore {
    objects: Mutex<HashMap<String, Vec<u8>>>,
}

impl MemoryStore {
    fn new() -> Self {
        Self {
            objects: Mutex::new(HashMap::new()),
        }
    }

    fn address(object: &ObjectRef) -> String {
        format!("{}/{}", object.bucket(), object.key())
    }
}

#[async_trait::async_trait]
impl ObjectStore for MemoryStore {
    async fn get(&self, object: &ObjectRef) -> Result<ObjectBody, ObjectStoreError> {
        let objects = self.objects.lock().unwrap();
        let bytes = objects
            .get(&Self::address(object))
            .ok_or_else(|| ObjectStoreError::NotFound {
                bucket: object.bucket().to_string(),
                key: object.key().to_string(),
            })?
            .clone();
        Ok(body_from_bytes(bytes))
    }

    async fn put(
        &self,
        object: &ObjectRef,
        body: ObjectBody,
        length: u64,
    ) -> Result<(), ObjectStoreError> {
        let bytes = collect_body(body).await?;
        if bytes.len() as u64 != length {
            return Err(ObjectStoreError::Backend {
                message: "length mismatch".to_string(),
            });
        }
        self.objects
            .lock()
            .unwrap()
            .insert(Self::address(object), bytes);
        Ok(())
    }

    async fn put_multipart(
        &self,
        object: &ObjectRef,
        body: ObjectBody,
    ) -> Result<(), ObjectStoreError> {
        let bytes = collect_body(body).await?;
        self.objects
            .lock()
            .unwrap()
            .insert(Self::address(object), bytes);
        Ok(())
    }

    async fn head(&self, object: &ObjectRef) -> Result<u64, ObjectStoreError> {
        let objects = self.objects.lock().unwrap();
        objects
            .get(&Self::address(object))
            .map(|bytes| bytes.len() as u64)
            .ok_or_else(|| ObjectStoreError::NotFound {
                bucket: object.bucket().to_string(),
                key: object.key().to_string(),
            })
    }

    async fn delete(&self, object: &ObjectRef) -> Result<(), ObjectStoreError> {
        self.objects.lock().unwrap().remove(&Self::address(object));
        Ok(())
    }

    async fn presign_get(
        &self,
        object: &ObjectRef,
        expires_in: Duration,
    ) -> Result<String, ObjectStoreError> {
        Ok(format!(
            "http://store.local/{}?expires={}",
            Self::address(object),
            expires_in.as_secs()
        ))
    }
}

fn object(bucket: &str, key: &str) -> ObjectRef {
    ObjectRef::new(bucket, key).unwrap()
}

#[tokio::test]
async fn port_round_trips_through_a_dyn_store() {
    let store: Box<dyn ObjectStore> = Box::new(MemoryStore::new());
    let target = object("media", "jobs/a/out.bin");
    let payload = vec![7u8; 1024];

    store
        .put(&target, body_from_bytes(payload.clone()), 1024)
        .await
        .unwrap();
    assert_eq!(store.head(&target).await.unwrap(), 1024);

    let read = collect_body(store.get(&target).await.unwrap())
        .await
        .unwrap();
    assert_eq!(read, payload);

    store.delete(&target).await.unwrap();
    let missing = store.head(&target).await.unwrap_err();
    assert!(matches!(
        missing,
        ObjectStoreError::NotFound { bucket, key } if bucket == "media" && key == "jobs/a/out.bin"
    ));
}

#[tokio::test]
async fn multipart_put_stores_the_body() {
    let store = MemoryStore::new();
    let target = object("media", "big.bin");
    store
        .put_multipart(&target, body_from_bytes(vec![1u8; 64]))
        .await
        .unwrap();
    assert_eq!(store.head(&target).await.unwrap(), 64);
}

#[tokio::test]
async fn presign_carries_the_expiry() {
    let store = MemoryStore::new();
    let target = object("media", "clip.mp4");
    let url = store
        .presign_get(&target, Duration::from_secs(90))
        .await
        .unwrap();
    assert!(url.contains("media/clip.mp4"));
    assert!(url.contains("expires=90"));
}

#[tokio::test]
async fn body_helpers_round_trip() {
    let collected = collect_body(body_from_bytes(b"abc".to_vec()))
        .await
        .unwrap();
    assert_eq!(collected, b"abc");
}

#[test]
fn object_ref_rejects_empty_parts() {
    assert!(ObjectRef::new("", "key").is_err());
    assert!(ObjectRef::new("bucket", "").is_err());
    let valid = ObjectRef::new("bucket", "a/b.bin").unwrap();
    assert_eq!(valid.bucket(), "bucket");
    assert_eq!(valid.key(), "a/b.bin");
}

#[test]
fn output_key_is_job_scoped() {
    let job_id = JobId::generate();
    let key = OutputKey::new(&job_id, "out.mp4").unwrap();
    assert_eq!(key.as_str(), format!("jobs/{job_id}/out.mp4"));
    assert_eq!(key.to_string(), key.as_str());
}

#[test]
fn output_key_allows_nested_artifacts() {
    let job_id = JobId::generate();
    let key = OutputKey::new(&job_id, "thumbs/0001.png").unwrap();
    assert_eq!(key.as_str(), format!("jobs/{job_id}/thumbs/0001.png"));
}

#[test]
fn output_key_rejects_bad_artifacts() {
    let job_id = JobId::generate();
    assert!(matches!(
        OutputKey::new(&job_id, ""),
        Err(OutputKeyError::Empty)
    ));
    assert!(matches!(
        OutputKey::new(&job_id, "/out.mp4"),
        Err(OutputKeyError::EmptySegment)
    ));
    assert!(matches!(
        OutputKey::new(&job_id, "out.mp4/"),
        Err(OutputKeyError::EmptySegment)
    ));
    assert!(matches!(
        OutputKey::new(&job_id, "a//b"),
        Err(OutputKeyError::EmptySegment)
    ));
    assert!(matches!(
        OutputKey::new(&job_id, "../escape"),
        Err(OutputKeyError::ReservedSegment { segment }) if segment == ".."
    ));
    assert!(matches!(
        OutputKey::new(&job_id, "a/./b"),
        Err(OutputKeyError::ReservedSegment { segment }) if segment == "."
    ));
    assert!(matches!(
        OutputKey::new(&job_id, "out put.mp4"),
        Err(OutputKeyError::ForbiddenCharacter { character: ' ' })
    ));
}

#[test]
fn output_key_converts_to_an_object_ref() {
    let job_id = JobId::generate();
    let key = OutputKey::new(&job_id, "out.mp4").unwrap();
    let object = key.clone().into_object("outputs").unwrap();
    assert_eq!(object.bucket(), "outputs");
    assert_eq!(object.key(), key.as_str());
    assert!(key.into_object("").is_err());
}

#[test]
fn store_errors_describe_themselves() {
    let not_found = ObjectStoreError::NotFound {
        bucket: "b".to_string(),
        key: "k".to_string(),
    };
    assert_eq!(not_found.to_string(), "object b/k not found");
    let backend = ObjectStoreError::Backend {
        message: "boom".to_string(),
    };
    assert_eq!(backend.to_string(), "object store backend error: boom");
}

#[test]
fn a_job_prefix_is_scoped_to_one_job() {
    let job = JobId::generate();
    let prefix = JobPrefix::new("mm-bucket", &job).unwrap();
    let key = OutputKey::new(&job, "renditions/0/seg00000.m4s").unwrap();
    assert!(
        key.as_str().starts_with(prefix.prefix()),
        "{key} is not under {prefix}"
    );
    let other = OutputKey::new(&JobId::generate(), "output.mp4").unwrap();
    assert!(
        !other.as_str().starts_with(prefix.prefix()),
        "a prefix that reaches another job delete is not a job prefix"
    );
}

#[test]
fn a_job_prefix_needs_a_bucket() {
    let job = JobId::generate();
    assert!(matches!(
        JobPrefix::new("", &job),
        Err(JobPrefixError::EmptyBucket)
    ));
}

#[tokio::test]
async fn a_store_that_has_not_implemented_the_prefix_delete_refuses_by_name() {
    let store = MemoryStore {
        objects: Mutex::new(HashMap::new()),
    };
    let prefix = JobPrefix::new("mm-bucket", &JobId::generate()).unwrap();
    match store.delete_job_prefix(&prefix).await {
        Err(ObjectStoreError::Backend { message }) => assert!(
            message.contains("mm-bucket"),
            "the refusal must name the bucket it would have swept: {message}"
        ),
        other => panic!("a store that cannot sweep a prefix must say so, not answer {other:?}"),
    }
}

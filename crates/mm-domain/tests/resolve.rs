use std::time::{Duration, SystemTime};

use mm_domain::{
    MediaRef, ObjectBody, ObjectRef, ObjectStore, ObjectStoreError, ResolveError, resolve_media_ref,
};

struct PresignStore;

#[async_trait::async_trait]
impl ObjectStore for PresignStore {
    async fn get(&self, _object: &ObjectRef) -> Result<ObjectBody, ObjectStoreError> {
        unreachable!()
    }

    async fn put(
        &self,
        _object: &ObjectRef,
        _body: ObjectBody,
        _length: u64,
    ) -> Result<(), ObjectStoreError> {
        unreachable!()
    }

    async fn put_multipart(
        &self,
        _object: &ObjectRef,
        _body: ObjectBody,
    ) -> Result<(), ObjectStoreError> {
        unreachable!()
    }

    async fn head(&self, _object: &ObjectRef) -> Result<u64, ObjectStoreError> {
        unreachable!()
    }

    async fn delete(&self, _object: &ObjectRef) -> Result<(), ObjectStoreError> {
        unreachable!()
    }

    async fn presign_get(
        &self,
        object: &ObjectRef,
        expires_in: Duration,
    ) -> Result<String, ObjectStoreError> {
        Ok(format!(
            "http://signed.local/{}/{}?expires={}",
            object.bucket(),
            object.key(),
            expires_in.as_secs()
        ))
    }
}

struct FailingStore;

#[async_trait::async_trait]
impl ObjectStore for FailingStore {
    async fn get(&self, _object: &ObjectRef) -> Result<ObjectBody, ObjectStoreError> {
        unreachable!()
    }

    async fn put(
        &self,
        _object: &ObjectRef,
        _body: ObjectBody,
        _length: u64,
    ) -> Result<(), ObjectStoreError> {
        unreachable!()
    }

    async fn put_multipart(
        &self,
        _object: &ObjectRef,
        _body: ObjectBody,
    ) -> Result<(), ObjectStoreError> {
        unreachable!()
    }

    async fn head(&self, _object: &ObjectRef) -> Result<u64, ObjectStoreError> {
        unreachable!()
    }

    async fn delete(&self, _object: &ObjectRef) -> Result<(), ObjectStoreError> {
        unreachable!()
    }

    async fn presign_get(
        &self,
        _object: &ObjectRef,
        _expires_in: Duration,
    ) -> Result<String, ObjectStoreError> {
        Err(ObjectStoreError::Backend {
            message: "signer offline".to_string(),
        })
    }
}

#[tokio::test]
async fn http_refs_pass_through_unchanged() {
    let media = MediaRef::http_url("https://cdn.example/a.mp4").unwrap();
    let now = SystemTime::now();
    let resolved = resolve_media_ref(&PresignStore, &media, now + Duration::from_secs(60), now)
        .await
        .unwrap();
    assert_eq!(resolved, "https://cdn.example/a.mp4");
}

#[tokio::test]
async fn object_refs_presign_with_expiry_sized_to_the_deadline() {
    let media = MediaRef::object_uri("s3://media/in.mp4").unwrap();
    let now = SystemTime::now();
    let resolved = resolve_media_ref(&PresignStore, &media, now + Duration::from_secs(300), now)
        .await
        .unwrap();
    assert_eq!(resolved, "http://signed.local/media/in.mp4?expires=300");
}

#[tokio::test]
async fn a_passed_deadline_is_rejected() {
    let media = MediaRef::object_uri("s3://media/in.mp4").unwrap();
    let now = SystemTime::now();
    let at_deadline = resolve_media_ref(&PresignStore, &media, now, now).await;
    assert!(matches!(at_deadline, Err(ResolveError::DeadlinePassed)));
    let past_deadline =
        resolve_media_ref(&PresignStore, &media, now - Duration::from_secs(1), now).await;
    assert!(matches!(past_deadline, Err(ResolveError::DeadlinePassed)));
}

#[tokio::test]
async fn a_passed_deadline_does_not_matter_for_http_refs() {
    let media = MediaRef::http_url("http://cdn.example/b.mp4").unwrap();
    let now = SystemTime::now();
    let resolved = resolve_media_ref(&PresignStore, &media, now - Duration::from_secs(1), now)
        .await
        .unwrap();
    assert_eq!(resolved, "http://cdn.example/b.mp4");
}

#[tokio::test]
async fn store_failures_propagate() {
    let media = MediaRef::object_uri("s3://media/in.mp4").unwrap();
    let now = SystemTime::now();
    let outcome =
        resolve_media_ref(&FailingStore, &media, now + Duration::from_secs(60), now).await;
    match outcome {
        Err(ResolveError::Store(ObjectStoreError::Backend { message })) => {
            assert_eq!(message, "signer offline");
        }
        other => panic!("expected a store error, got {other:?}"),
    }
}

#[tokio::test]
async fn resolve_errors_describe_themselves() {
    assert_eq!(
        ResolveError::DeadlinePassed.to_string(),
        "job deadline has already passed"
    );
    let wrapped = ResolveError::Store(ObjectStoreError::Backend {
        message: "boom".to_string(),
    });
    assert_eq!(wrapped.to_string(), "object store backend error: boom");
}

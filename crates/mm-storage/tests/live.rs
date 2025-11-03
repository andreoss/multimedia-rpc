use std::env;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use bytes::Bytes;
use mm_domain::{
    JobId, ObjectBody, ObjectRef, ObjectStore, ObjectStoreError, OutputKey, collect_body,
};
use mm_storage::{CredentialSource, S3ObjectStore, S3Settings};

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:8333";
const DEFAULT_ACCESS_KEY: &str = "mm-test-access";
const DEFAULT_SECRET_KEY: &str = "mm-test-secret";

fn env_or(name: &str, fallback: &str) -> String {
    env::var(name).unwrap_or_else(|_| fallback.to_string())
}

fn endpoint() -> String {
    env_or("MM_OBJECTSTORE_ENDPOINT", DEFAULT_ENDPOINT)
}

fn host_port(endpoint: &str) -> SocketAddr {
    let authority = endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))
        .expect("endpoint must be an http(s) url")
        .trim_end_matches('/');
    authority
        .to_socket_addrs()
        .expect("endpoint authority must resolve")
        .next()
        .expect("endpoint authority must resolve to an address")
}

fn require_stack() {
    let endpoint = endpoint();
    let address = host_port(&endpoint);
    if TcpStream::connect_timeout(&address, Duration::from_secs(2)).is_err() {
        panic!(
            "objectstore at {endpoint} is unreachable; \
             start the compose stack with `docker compose up -d` \
             before running mm-storage integration tests"
        );
    }
}

fn credentials() -> CredentialSource {
    CredentialSource::Static {
        access_key_id: env_or("MM_OBJECTSTORE_ACCESS_KEY", DEFAULT_ACCESS_KEY),
        secret_access_key: env_or("MM_OBJECTSTORE_SECRET_KEY", DEFAULT_SECRET_KEY),
    }
}

fn settings(credentials: CredentialSource) -> S3Settings {
    S3Settings {
        endpoint: Some(endpoint()),
        region: Some("us-east-1".to_string()),
        force_path_style: true,
        credentials,
    }
}

async fn test_store() -> S3ObjectStore {
    require_stack();
    S3ObjectStore::connect(settings(credentials())).await
}

fn deterministic_payload(length: usize) -> Vec<u8> {
    let mut state = 0x2545f4914f6cdd1du64;
    let mut payload = Vec::with_capacity(length);
    while payload.len() < length {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        payload.extend_from_slice(&state.to_le_bytes());
    }
    payload.truncate(length);
    payload
}

fn chunked_body(payload: &[u8], chunk_size: usize) -> ObjectBody {
    let chunks: Vec<Result<Bytes, ObjectStoreError>> = payload
        .chunks(chunk_size)
        .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
        .collect();
    Box::pin(futures_util::stream::iter(chunks))
}

fn fresh_bucket() -> String {
    format!("mm-it-{}", JobId::generate())
}

#[tokio::test]
async fn multipart_round_trip_lands_under_the_job_scope() {
    let store = test_store().await;
    let bucket = fresh_bucket();
    store.ensure_bucket(&bucket).await.unwrap();

    let job_id = JobId::generate();
    let key = OutputKey::new(&job_id, "out.bin").unwrap();
    assert!(key.as_str().starts_with(&format!("jobs/{job_id}/")));
    let target = key.into_object(&bucket).unwrap();

    let payload = deterministic_payload(12 * 1024 * 1024);
    store
        .put_multipart(&target, chunked_body(&payload, 1024 * 1024))
        .await
        .unwrap();

    assert_eq!(store.head(&target).await.unwrap(), payload.len() as u64);
    let read = collect_body(store.get(&target).await.unwrap())
        .await
        .unwrap();
    assert_eq!(read.len(), payload.len());
    assert!(read == payload);

    store.delete(&target).await.unwrap();
    assert!(matches!(
        store.head(&target).await.unwrap_err(),
        ObjectStoreError::NotFound { .. }
    ));
    store.delete_bucket(&bucket).await.unwrap();
}

#[tokio::test]
async fn streaming_put_round_trips() {
    let store = test_store().await;
    let bucket = fresh_bucket();
    store.ensure_bucket(&bucket).await.unwrap();

    let target = ObjectRef::new(&bucket, "small.bin").unwrap();
    let payload = deterministic_payload(64 * 1024);
    store
        .put(
            &target,
            chunked_body(&payload, 8 * 1024),
            payload.len() as u64,
        )
        .await
        .unwrap();

    let read = collect_body(store.get(&target).await.unwrap())
        .await
        .unwrap();
    assert!(read == payload);

    store.delete(&target).await.unwrap();
    store.delete_bucket(&bucket).await.unwrap();
}

#[tokio::test]
async fn get_of_a_missing_object_is_not_found() {
    let store = test_store().await;
    let bucket = fresh_bucket();
    store.ensure_bucket(&bucket).await.unwrap();

    let target = ObjectRef::new(&bucket, "absent.bin").unwrap();
    match store.get(&target).await {
        Err(ObjectStoreError::NotFound { .. }) => {}
        Err(other) => panic!("expected NotFound, got {other}"),
        Ok(_) => panic!("expected NotFound, got a body"),
    }
    store.delete_bucket(&bucket).await.unwrap();
}

#[tokio::test]
async fn wrong_credentials_are_rejected() {
    require_stack();
    let impostor = S3ObjectStore::connect(settings(CredentialSource::Static {
        access_key_id: "mm-test-access".to_string(),
        secret_access_key: "not-the-secret".to_string(),
    }))
    .await;
    let result = impostor.ensure_bucket(&fresh_bucket()).await;
    assert!(matches!(result, Err(ObjectStoreError::Backend { .. })));
}

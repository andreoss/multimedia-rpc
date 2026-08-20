use std::env;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use mm_domain::{
    JobId, MediaRef, ObjectBody, ObjectRef, ObjectStore, ObjectStoreError, OutputKey, collect_body,
    resolve_media_ref,
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

async fn shared_bucket(store: &S3ObjectStore) -> String {
    let bucket = "mm-it-shared".to_string();
    store.ensure_bucket(&bucket).await.unwrap();
    bucket
}

#[tokio::test]
async fn multipart_round_trip_lands_under_the_job_scope() {
    let store = test_store().await;
    let bucket = shared_bucket(&store).await;

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
}

#[tokio::test]
async fn streaming_put_round_trips() {
    let store = test_store().await;
    let bucket = shared_bucket(&store).await;

    let target = ObjectRef::new(&bucket, &format!("{}/small.bin", JobId::generate())).unwrap();
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
}

#[tokio::test]
async fn get_of_a_missing_object_is_not_found() {
    let store = test_store().await;
    let bucket = shared_bucket(&store).await;

    let target = ObjectRef::new(&bucket, &format!("{}/absent.bin", JobId::generate())).unwrap();
    match store.get(&target).await {
        Err(ObjectStoreError::NotFound { .. }) => {}
        Err(other) => panic!("expected NotFound, got {other}"),
        Ok(_) => panic!("expected NotFound, got a body"),
    }
}

#[tokio::test]
async fn bucket_lifecycle_round_trips() {
    let store = test_store().await;
    let bucket = "mm-storage-it".to_string();
    store.ensure_bucket(&bucket).await.unwrap();
    store.ensure_bucket(&bucket).await.unwrap();
    store.delete_bucket(&bucket).await.unwrap();
}

fn http_get_once(url: &str, read_timeout: Duration) -> (u16, Vec<u8>) {
    let without_scheme = url
        .strip_prefix("http://")
        .expect("presigned url must be plain http against the local store");
    let (authority, target) = without_scheme
        .split_once('/')
        .expect("presigned url must carry a path");
    let address = authority
        .to_socket_addrs()
        .expect("authority must resolve")
        .next()
        .expect("authority must resolve to an address");
    let mut connection =
        TcpStream::connect_timeout(&address, Duration::from_secs(2)).expect("connect");
    connection
        .set_read_timeout(Some(read_timeout))
        .expect("read timeout");
    connection
        .write_all(
            format!("GET /{target} HTTP/1.0\r\nHost: {authority}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .expect("request");
    let mut raw = Vec::new();
    connection.read_to_end(&mut raw).expect("response");
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("header separator");
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .expect("status code")
        .parse()
        .expect("numeric status");
    (status, raw[split + 4..].to_vec())
}

#[tokio::test]
async fn presigned_object_is_fetchable_over_plain_http() {
    let store = test_store().await;
    let bucket = shared_bucket(&store).await;

    let job_id = JobId::generate();
    let key = OutputKey::new(&job_id, "clip.bin").unwrap();
    let target = key.clone().into_object(&bucket).unwrap();
    let payload = deterministic_payload(32 * 1024);
    store
        .put(
            &target,
            chunked_body(&payload, 8 * 1024),
            payload.len() as u64,
        )
        .await
        .unwrap();

    let media = MediaRef::object_uri(&format!("s3://{bucket}/{key}")).unwrap();
    let now = SystemTime::now();
    let url = resolve_media_ref(&store, &media, now + Duration::from_secs(120), now)
        .await
        .unwrap();
    assert!(url.starts_with(&format!("{}/{bucket}/{key}?", endpoint())));
    assert!(url.contains("X-Amz-Expires=120"));

    let (status, body) = http_get_once(&url, Duration::from_secs(10));
    assert_eq!(status, 200);
    assert!(body == payload);

    let (anonymous_status, _) = http_get_once(
        &format!("{}/{bucket}/{key}", endpoint()),
        Duration::from_secs(10),
    );
    assert_eq!(anonymous_status, 403);

    store.delete(&target).await.unwrap();
}

#[tokio::test]
async fn wrong_credentials_are_rejected() {
    require_stack();
    let impostor = S3ObjectStore::connect(settings(CredentialSource::Static {
        access_key_id: "mm-test-access".to_string(),
        secret_access_key: "not-the-secret".to_string(),
    }))
    .await;
    let result = impostor.ensure_bucket("mm-storage-it").await;
    assert!(matches!(result, Err(ObjectStoreError::Backend { .. })));
}

fn http_put_once(url: &str, body: &[u8], timeout: Duration) -> u16 {
    let without_scheme = url.strip_prefix("http://").expect("plain http");
    let authority = without_scheme.split('/').next().expect("authority");
    let path = &without_scheme[authority.len()..];
    let address = authority
        .to_socket_addrs()
        .expect("authority resolves")
        .next()
        .expect("authority resolves");
    let mut stream = TcpStream::connect_timeout(&address, timeout).expect("connect");
    stream
        .set_read_timeout(Some(timeout))
        .expect("read timeout");
    let request = format!(
        "PUT {path} HTTP/1.0\r\nHost: {authority}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(request.as_bytes()).expect("write head");
    stream.write_all(body).expect("write body");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    let head_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("headers end");
    let head = String::from_utf8_lossy(&response[..head_end]);
    head.lines()
        .next()
        .expect("status line")
        .split_whitespace()
        .nth(1)
        .expect("status code")
        .parse()
        .expect("status code parses")
}

#[tokio::test]
async fn a_presigned_put_accepts_a_client_body_without_the_service() {
    let store = test_store().await;
    let bucket = shared_bucket(&store).await;

    let key = OutputKey::new(&JobId::generate(), "upload.bin").unwrap();
    let target = key.clone().into_object(&bucket).unwrap();
    let payload = deterministic_payload(64 * 1024);

    let url = store
        .presign_put(&target, Duration::from_secs(120))
        .await
        .unwrap();
    assert!(url.contains("X-Amz-Signature="), "the put is not signed");

    assert_eq!(http_put_once(&url, &payload, Duration::from_secs(10)), 200);

    let read = collect_body(store.get(&target).await.unwrap())
        .await
        .unwrap();
    assert!(read == payload, "the object does not hold the put body");
    store.delete(&target).await.unwrap();
}

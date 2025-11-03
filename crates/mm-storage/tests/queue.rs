use std::env;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use mm_domain::{CallbackError, CallbackPublisher, QueueUrl};
use mm_storage::{CredentialSource, SqsCallbackPublisher, SqsSettings};
use uuid::Uuid;

const DEFAULT_QUEUE_ENDPOINT: &str = "http://127.0.0.1:9324";

fn env_or(name: &str, fallback: &str) -> String {
    env::var(name).unwrap_or_else(|_| fallback.to_string())
}

fn queue_endpoint() -> String {
    env_or("MM_QUEUE_ENDPOINT", DEFAULT_QUEUE_ENDPOINT)
}

fn require_queue() {
    let endpoint = queue_endpoint();
    let authority = endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))
        .expect("endpoint must be an http(s) url")
        .trim_end_matches('/')
        .to_string();
    let address = authority
        .to_socket_addrs()
        .expect("queue authority must resolve")
        .next()
        .expect("queue authority must resolve to an address");
    if TcpStream::connect_timeout(&address, Duration::from_secs(2)).is_err() {
        panic!(
            "queue at {endpoint} is unreachable; \
             start the compose stack with `docker compose up -d` \
             before running mm-storage queue tests"
        );
    }
}

async fn publisher() -> SqsCallbackPublisher {
    require_queue();
    SqsCallbackPublisher::connect(SqsSettings {
        endpoint: Some(queue_endpoint()),
        region: Some("local".to_string()),
        credentials: CredentialSource::Static {
            access_key_id: "queue-access".to_string(),
            secret_access_key: "queue-secret".to_string(),
        },
    })
    .await
}

#[tokio::test]
async fn a_published_message_round_trips_through_a_real_queue() {
    let publisher = publisher().await;
    let queue = publisher
        .ensure_queue(&format!("mm-storage-it-{}", Uuid::now_v7().simple()))
        .await
        .expect("queue exists");
    let body = format!("payload-{}", Uuid::now_v7().simple());
    publisher.publish(&queue, &body).await.expect("publish");
    let received = tokio::time::timeout(
        Duration::from_secs(10),
        publisher.receive(&queue, Duration::from_secs(5)),
    )
    .await
    .expect("receive answers in time")
    .expect("receive succeeds");
    assert_eq!(received, vec![body]);
    publisher.delete_queue(&queue).await.expect("queue removed");
}

#[tokio::test]
async fn ensure_queue_is_idempotent() {
    let publisher = publisher().await;
    let name = format!("mm-storage-it-{}", Uuid::now_v7().simple());
    let first = publisher.ensure_queue(&name).await.expect("queue exists");
    let second = publisher
        .ensure_queue(&name)
        .await
        .expect("queue still exists");
    assert_eq!(first, second);
    publisher.delete_queue(&first).await.expect("queue removed");
}

#[tokio::test]
async fn publishing_to_a_missing_queue_is_a_typed_backend_error() {
    let publisher = publisher().await;
    let queue = QueueUrl::new(&format!(
        "{}/queue/mm-storage-it-absent-{}",
        queue_endpoint(),
        Uuid::now_v7().simple()
    ))
    .expect("valid url");
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        publisher.publish(&queue, "payload"),
    )
    .await
    .expect("publish answers in time");
    assert!(matches!(outcome, Err(CallbackError::Backend { .. })));
}

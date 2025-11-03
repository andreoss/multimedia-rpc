use std::env;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use mm_domain::{
    JobId, MediaRef, ObjectBody, ObjectStore, ObjectStoreError, OutputKey, resolve_media_ref,
};
use mm_ffmpeg::{ProbeLimits, Prober};
use mm_storage::{CredentialSource, S3ObjectStore, S3Settings};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:8333";
const DEFAULT_ACCESS_KEY: &str = "mm-test-access";
const DEFAULT_SECRET_KEY: &str = "mm-test-secret";
const BUCKET: &str = "mm-it-shared";
const RELAY_IDLE: Duration = Duration::from_secs(60);

fn env_or(name: &str, fallback: &str) -> String {
    env::var(name).unwrap_or_else(|_| fallback.to_string())
}

fn endpoint() -> String {
    env_or("MM_OBJECTSTORE_ENDPOINT", DEFAULT_ENDPOINT)
}

fn authority(endpoint: &str) -> String {
    endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))
        .expect("endpoint must be an http(s) url")
        .trim_end_matches('/')
        .to_string()
}

fn resolve(authority: &str) -> SocketAddr {
    authority
        .to_socket_addrs()
        .expect("authority must resolve")
        .next()
        .expect("authority must resolve to an address")
}

fn require_stack() {
    let endpoint = endpoint();
    let address = resolve(&authority(&endpoint));
    if TcpStream::connect_timeout(&address, Duration::from_secs(2)).is_err() {
        panic!(
            "objectstore at {endpoint} is unreachable; \
             start the compose stack with `docker compose up -d` \
             before running integration tests"
        );
    }
}

async fn test_store() -> S3ObjectStore {
    require_stack();
    let store = S3ObjectStore::connect(S3Settings {
        endpoint: Some(endpoint()),
        region: Some("us-east-1".to_string()),
        force_path_style: true,
        credentials: CredentialSource::Static {
            access_key_id: env_or("MM_OBJECTSTORE_ACCESS_KEY", DEFAULT_ACCESS_KEY),
            secret_access_key: env_or("MM_OBJECTSTORE_SECRET_KEY", DEFAULT_SECRET_KEY),
        },
    })
    .await;
    store.ensure_bucket(BUCKET).await.unwrap();
    store
}

fn fixtures_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scratch/fixtures");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn fixture(name: &str, seconds: u32, fps: u32, container: &str) -> PathBuf {
    static GENERATION: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = GENERATION.lock().unwrap();
    let path = fixtures_dir().join(name);
    if path.is_file() && path.metadata().unwrap().len() > 0 {
        return path;
    }
    let mut command = Command::new("ffmpeg");
    command
        .arg("-y")
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg(format!(
            "testsrc2=duration={seconds}:size=1280x720:rate={fps}"
        ))
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg(format!(
            "sine=frequency=440:sample_rate=44100:duration={seconds}"
        ))
        .arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("veryfast")
        .arg("-b:v")
        .arg("2000k")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-c:a")
        .arg("aac")
        .arg("-b:a")
        .arg("128k");
    if container == "mp4" {
        command.arg("-movflags").arg("+faststart");
    }
    let status = command
        .arg("-f")
        .arg(container)
        .arg(&path)
        .output()
        .expect("ffmpeg must be runnable to generate fixtures");
    assert!(status.status.success(), "fixture generation failed: {name}");
    path
}

fn file_body(path: &Path) -> (ObjectBody, u64) {
    let payload = std::fs::read(path).unwrap();
    let length = payload.len() as u64;
    let chunks: Vec<Result<Bytes, ObjectStoreError>> = payload
        .chunks(64 * 1024)
        .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
        .collect();
    (Box::pin(futures_util::stream::iter(chunks)), length)
}

async fn upload_and_presign(store: &S3ObjectStore, path: &Path, artifact: &str) -> String {
    let job_id = JobId::generate();
    let key = OutputKey::new(&job_id, artifact).unwrap();
    let target = key.clone().into_object(BUCKET).unwrap();
    let (body, length) = file_body(path);
    store.put(&target, body, length).await.unwrap();
    let media = MediaRef::object_uri(&format!("s3://{BUCKET}/{key}")).unwrap();
    let now = SystemTime::now();
    resolve_media_ref(store, &media, now + Duration::from_secs(300), now)
        .await
        .unwrap()
}

async fn rewrite_heads(
    client: tokio::net::tcp::OwnedReadHalf,
    mut upstream: tokio::net::tcp::OwnedWriteHalf,
    upstream_authority: String,
) {
    let mut reader = BufReader::new(client);
    loop {
        let mut head = Vec::new();
        loop {
            let mut line = String::new();
            let read = tokio::time::timeout(RELAY_IDLE, reader.read_line(&mut line)).await;
            let Ok(Ok(count)) = read else { return };
            if count == 0 {
                return;
            }
            if line.to_ascii_lowercase().starts_with("host:") {
                line = format!("Host: {upstream_authority}\r\n");
            }
            let done = line == "\r\n";
            head.extend_from_slice(line.as_bytes());
            if done {
                break;
            }
        }
        if upstream.write_all(&head).await.is_err() {
            return;
        }
    }
}

async fn count_responses(
    mut upstream: tokio::net::tcp::OwnedReadHalf,
    mut client: tokio::net::tcp::OwnedWriteHalf,
    counter: Arc<AtomicU64>,
) {
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = tokio::time::timeout(RELAY_IDLE, upstream.read(&mut buffer)).await;
        let Ok(Ok(count)) = read else { return };
        if count == 0 {
            return;
        }
        counter.fetch_add(count as u64, Ordering::Relaxed);
        if client.write_all(&buffer[..count]).await.is_err() {
            return;
        }
    }
}

async fn counting_relay(upstream_authority: String) -> (SocketAddr, Arc<AtomicU64>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let counter = Arc::new(AtomicU64::new(0));
    let counted = counter.clone();
    tokio::spawn(async move {
        loop {
            let Ok((client, _)) = listener.accept().await else {
                return;
            };
            let upstream_authority = upstream_authority.clone();
            let counter = counted.clone();
            tokio::spawn(async move {
                let Ok(upstream) = tokio::net::TcpStream::connect(&upstream_authority).await else {
                    return;
                };
                let (client_read, client_write) = client.into_split();
                let (upstream_read, upstream_write) = upstream.into_split();
                tokio::join!(
                    rewrite_heads(client_read, upstream_write, upstream_authority),
                    count_responses(upstream_read, client_write, counter),
                );
            });
        }
    });
    (address, counter)
}

fn probe_limits() -> ProbeLimits {
    ProbeLimits {
        probe_size_bytes: 1024 * 1024,
        analyze_duration: Duration::from_secs(2),
        timeout: Duration::from_secs(30),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_presigned_fixture_probes_without_a_full_download() {
    let store = test_store().await;
    let path = fixture("main-30fps.mp4", 90, 30, "mp4");
    let file_size = path.metadata().unwrap().len();
    assert!(file_size > 15_000_000, "fixture too small: {file_size}");

    let url = upload_and_presign(&store, &path, "main-30fps.mp4").await;
    let upstream_authority = authority(&endpoint());
    let (relay_address, counter) = counting_relay(upstream_authority.clone()).await;
    let relayed_url = url.replace(&upstream_authority, &relay_address.to_string());
    assert_ne!(relayed_url, url);

    let info = Prober::default()
        .probe(&relayed_url, &probe_limits())
        .await
        .unwrap();

    assert!(info.container.split(',').any(|token| token == "mp4"));
    assert!((info.duration.as_secs_f64() - 90.0).abs() < 0.6);
    assert_eq!(info.video.len(), 1);
    assert_eq!(info.video[0].codec, "h264");
    assert_eq!(info.video[0].width, 1280);
    assert_eq!(info.video[0].height, 720);
    assert!((info.video[0].avg_fps.unwrap() - 30.0).abs() < 0.1);
    assert_eq!(info.audio.len(), 1);
    assert_eq!(info.audio[0].codec, "aac");
    assert_eq!(info.audio[0].channels, 1);
    assert_eq!(info.audio[0].sample_rate, Some(44_100));

    let transferred = counter.load(Ordering::Relaxed);
    assert!(transferred > 0);
    assert!(
        transferred < file_size / 2,
        "probe transferred {transferred} of {file_size} bytes"
    );
}

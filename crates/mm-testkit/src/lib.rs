use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use mm_domain::{MediaRef, ObjectBody, ObjectRef, ObjectStore, ObjectStoreError};

pub const STACK_SLOTS: u32 = 3;
const UPLOAD_ATTEMPTS: u32 = 3;
const UPLOAD_CHUNK_BYTES: usize = 64 * 1024;
const UPLOAD_RETRY_PAUSE: Duration = Duration::from_millis(500);
const SLOT_POLL: Duration = Duration::from_millis(100);

pub mod budget {
    use std::time::Duration;

    pub const PICKUP: Duration = Duration::from_secs(10);
    pub const TICK_PICKUP: Duration = Duration::from_secs(15);
    pub const CANCEL: Duration = Duration::from_secs(15);
    pub const PROCESS_EXIT: Duration = Duration::from_secs(15);
    pub const QUEUE_RECEIVE: Duration = Duration::from_secs(30);
    pub const QUEUE_SETTLE: Duration = Duration::from_secs(600);
    pub const EVENT_ARRIVAL: Duration = Duration::from_secs(30);
    pub const NOTIFY: Duration = Duration::from_secs(5);
    pub const LISTENER_RECOVERY: Duration = Duration::from_secs(10);
    pub const MERGE_TERMINAL: Duration = Duration::from_secs(600);
    pub const RPC: Duration = Duration::from_secs(30);
}

pub fn scratch_path(part: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../scratch/{part}"));
    std::fs::create_dir_all(&dir).expect("scratch directory exists");
    dir
}

pub struct StackSlot {
    _lock: File,
}

pub fn stack_slot() -> StackSlot {
    let dir = scratch_path("locks");
    loop {
        for index in 0..STACK_SLOTS {
            let path = dir.join(format!("stack-{index}.lock"));
            if let Some(lock) = try_lock(&path) {
                return StackSlot { _lock: lock };
            }
        }
        thread::sleep(SLOT_POLL);
    }
}

fn try_lock(path: &Path) -> Option<File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("lock file opens");
    let outcome = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    (outcome == 0).then_some(file)
}

pub async fn upload_object(
    store: &dyn ObjectStore,
    path: &Path,
    bucket: &str,
    key: &str,
) -> MediaRef {
    let payload = tokio::fs::read(path).await.expect("fixture readable");
    let length = payload.len() as u64;
    let object = ObjectRef::new(bucket, key).expect("valid object ref");
    let mut attempt = 0;
    loop {
        attempt += 1;
        let chunks: Vec<Result<bytes::Bytes, ObjectStoreError>> = payload
            .chunks(UPLOAD_CHUNK_BYTES)
            .map(|chunk| Ok(bytes::Bytes::copy_from_slice(chunk)))
            .collect();
        let body: ObjectBody = Box::pin(futures_util::stream::iter(chunks));
        match store.put(&object, body, length).await {
            Ok(()) => break,
            Err(error) if attempt < UPLOAD_ATTEMPTS => {
                eprintln!("fixture upload attempt {attempt} failed: {error}");
                tokio::time::sleep(UPLOAD_RETRY_PAUSE).await;
            }
            Err(error) => panic!("fixture upload failed after {attempt} attempts: {error}"),
        }
    }
    MediaRef::object_uri(&format!("s3://{bucket}/{key}")).expect("valid media ref")
}

pub fn assert_golden(path: &Path, actual: &str) {
    if std::env::var("UPDATE_GOLDEN").is_ok_and(|value| value == "1") {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("golden directory exists");
        }
        std::fs::write(path, actual).expect("golden file writes");
        return;
    }
    let expected = std::fs::read_to_string(path).unwrap_or_else(|error| {
        panic!(
            "golden {} cannot be read: {error}; rerun with UPDATE_GOLDEN=1 to write it",
            path.display()
        )
    });
    assert_eq!(expected, actual, "golden {} differs", path.display());
}

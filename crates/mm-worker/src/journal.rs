use std::io;
use std::path::{Path, PathBuf};

use mm_domain::{JobId, JobState, ObjectRef};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::WorkerConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LandedKey {
    bucket: String,
    key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    job_id: String,
    staged: Vec<String>,
    landed: Vec<LandedKey>,
}

static JOURNAL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn directory(staging: &Path) -> PathBuf {
    staging.join("journals")
}

fn journal_path(staging: &Path, id: JobId) -> PathBuf {
    directory(staging).join(format!("{id}.json"))
}

fn temporary_path(path: &Path) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    path.with_extension(format!("tmp-{stamp}"))
}

async fn write_atomic(path: &Path, entry: &Entry) -> io::Result<()> {
    let text = serde_json::to_vec(entry).map_err(io::Error::other)?;
    let temporary = temporary_path(path);
    tokio::fs::write(&temporary, text).await?;
    tokio::fs::rename(&temporary, path).await
}

async fn read(path: &Path) -> io::Result<Option<Entry>> {
    match tokio::fs::read(path).await {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(io::Error::other),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub async fn start(
    ctx: &std::sync::Arc<WorkerConfig>,
    id: JobId,
) -> io::Result<watch::Sender<bool>> {
    let directory = directory(&ctx.staging_dir);
    tokio::fs::create_dir_all(&directory).await?;
    let entry = Entry {
        job_id: id.to_string(),
        staged: Vec::new(),
        landed: Vec::new(),
    };
    let path = journal_path(&ctx.staging_dir, id);
    let _guard = JOURNAL_LOCK.lock().await;
    write_atomic(&path, &entry).await?;
    let (stop, mut stopped) = watch::channel(false);
    let ctx = ctx.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(200));
        loop {
            tokio::select! {
                changed = stopped.changed() => {
                    if changed.is_err() || *stopped.borrow() {
                        break;
                    }
                }
                _ = ticker.tick() => {
                    let _ = record_staged_scan(&ctx, id).await;
                }
            }
        }
    });
    Ok(stop)
}

pub async fn finish(ctx: &WorkerConfig, id: JobId, state: Option<JobState>) {
    match state {
        Some(JobState::Succeeded) => {
            remove_staged(&ctx.staging_dir, id);
            remove_journal(&ctx.staging_dir, id).await;
        }
        Some(JobState::Failed) | Some(JobState::Cancelled) => {
            let path = journal_path(&ctx.staging_dir, id);
            if let Ok(Some(entry)) = read(&path).await {
                for landed in &entry.landed {
                    if let Ok(object) = ObjectRef::new(&landed.bucket, &landed.key) {
                        let _ = ctx.store.delete(&object).await;
                    }
                }
            }
            remove_staged(&ctx.staging_dir, id);
            remove_journal(&ctx.staging_dir, id).await;
        }
        Some(JobState::Pending) | Some(JobState::Running) | None => {}
    }
}

pub async fn record_staged_scan(ctx: &WorkerConfig, id: JobId) -> io::Result<()> {
    let prefix = id.to_string();
    let mut staged = Vec::new();
    collect_staged(&ctx.staging_dir, &prefix, &mut staged);
    staged.sort();
    let path = journal_path(&ctx.staging_dir, id);
    let _guard = JOURNAL_LOCK.lock().await;
    let Some(mut entry) = read(&path).await? else {
        return Ok(());
    };
    if entry.staged != staged {
        entry.staged = staged;
        write_atomic(&path, &entry).await?;
    }
    Ok(())
}

pub async fn record_landed(ctx: &WorkerConfig, id: JobId, objects: &[ObjectRef]) -> io::Result<()> {
    let path = journal_path(&ctx.staging_dir, id);
    let _guard = JOURNAL_LOCK.lock().await;
    let Some(mut entry) = read(&path).await? else {
        return Ok(());
    };
    for object in objects {
        let key = LandedKey {
            bucket: object.bucket().to_string(),
            key: object.key().to_string(),
        };
        if !entry
            .landed
            .iter()
            .any(|landed| landed.bucket == key.bucket && landed.key == key.key)
        {
            entry.landed.push(key);
        }
    }
    write_atomic(&path, &entry).await
}

pub async fn reconcile(ctx: &WorkerConfig) {
    let directory = directory(&ctx.staging_dir);
    let Ok(mut entries) = tokio::fs::read_dir(&directory).await else {
        return;
    };
    while let Ok(Some(item)) = entries.next_entry().await {
        let path = item.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let entry = match read(&path).await {
            Ok(Some(entry)) => entry,
            Ok(None) => continue,
            Err(error) => {
                let aside = path.with_extension(format!("corrupt-{}", now_stamp()));
                let _ = tokio::fs::rename(&path, &aside).await;
                eprintln!(
                    "staging journal {} could not be read: {error}; moved to {}",
                    path.display(),
                    aside.display()
                );
                continue;
            }
        };
        let Ok(id) = entry.job_id.parse::<JobId>() else {
            let aside = path.with_extension(format!("invalid-{}", now_stamp()));
            let _ = tokio::fs::rename(&path, &aside).await;
            continue;
        };
        let row = ctx.repository.get(id).await.ok().flatten();
        let state = row.as_ref().map(|record| record.state);
        match state {
            Some(JobState::Running) => {}
            Some(JobState::Succeeded) => {
                remove_staged(&ctx.staging_dir, id);
                remove_journal(&ctx.staging_dir, id).await;
            }
            Some(JobState::Failed) | Some(JobState::Cancelled) => {
                for landed in &entry.landed {
                    if let Ok(object) = ObjectRef::new(&landed.bucket, &landed.key) {
                        let _ = ctx.store.delete(&object).await;
                    }
                }
                remove_staged(&ctx.staging_dir, id);
                remove_journal(&ctx.staging_dir, id).await;
            }
            Some(JobState::Pending) => {
                remove_staged(&ctx.staging_dir, id);
                remove_journal(&ctx.staging_dir, id).await;
            }
            None => {
                remove_staged(&ctx.staging_dir, id);
                remove_journal(&ctx.staging_dir, id).await;
            }
        }
    }
}

async fn remove_journal(staging: &Path, id: JobId) {
    let path = journal_path(staging, id);
    let _guard = JOURNAL_LOCK.lock().await;
    let _ = tokio::fs::remove_file(path).await;
}

fn remove_staged(staging: &Path, id: JobId) {
    let prefix = id.to_string();
    let Ok(entries) = std::fs::read_dir(staging) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(&prefix) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            let _ = std::fs::remove_dir_all(&path);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
}

fn collect_staged(root: &Path, prefix: &str, found: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_name().to_string_lossy().starts_with(prefix) {
            continue;
        }
        if path.is_dir() {
            collect_staged(&path, prefix, found);
        } else {
            found.push(path.to_string_lossy().to_string());
        }
    }
}

fn now_stamp() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

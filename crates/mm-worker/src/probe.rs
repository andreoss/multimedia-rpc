use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use mm_ffmpeg::MediaInfo;

#[derive(Default)]
pub struct ProbeCache {
    entries: Mutex<HashMap<String, MediaInfo>>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl ProbeCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: &str) -> Option<MediaInfo> {
        let found = self
            .entries
            .lock()
            .expect("probe cache lock is not poisoned")
            .get(key)
            .cloned();
        match found {
            Some(info) => {
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some(info)
            }
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    pub fn insert(&self, key: String, info: MediaInfo) {
        self.entries
            .lock()
            .expect("probe cache lock is not poisoned")
            .insert(key, info);
    }

    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }
}

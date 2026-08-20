use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::time::Duration;

use crate::pipeline::codes;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base: Duration,
    pub cap: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base: Duration::from_secs(1),
            cap: Duration::from_secs(60),
        }
    }
}

impl RetryPolicy {
    pub fn delay(&self, attempts: u32) -> Duration {
        let doubled = self
            .base
            .saturating_mul(2u32.saturating_pow(attempts.min(31)));
        jittered(doubled.min(self.cap))
    }
}

fn jittered(delay: Duration) -> Duration {
    let quarter = (delay / 4).as_nanos();
    if quarter == 0 {
        return delay;
    }
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(quarter as u64);
    let shaved = u128::from(hasher.finish()) % (quarter + 1);
    delay.saturating_sub(Duration::from_nanos(shaved as u64))
}

pub fn is_transient(code: &str) -> bool {
    matches!(
        code,
        codes::INPUT_UNRESOLVABLE
            | codes::PROBE_FAILED
            | codes::MERGE_LAUNCH_FAILED
            | codes::MERGE_FAILED
            | codes::OUTPUT_INVALID
            | codes::OUTPUT_UPLOAD_FAILED
    )
}

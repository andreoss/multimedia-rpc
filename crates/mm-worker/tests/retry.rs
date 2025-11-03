use std::collections::HashSet;
use std::time::Duration;

use mm_worker::{RetryPolicy, codes, is_transient};

fn policy(base_secs: u64, cap_secs: u64) -> RetryPolicy {
    RetryPolicy {
        max_attempts: 3,
        base: Duration::from_secs(base_secs),
        cap: Duration::from_secs(cap_secs),
    }
}

#[test]
fn the_default_policy_allows_three_attempts() {
    let policy = RetryPolicy::default();
    assert_eq!(policy.max_attempts, 3);
    assert!(policy.base > Duration::ZERO);
    assert!(policy.cap >= policy.base);
}

#[test]
fn delays_grow_exponentially_with_bounded_jitter() {
    let policy = policy(1, 600);
    for attempts in 1..=4u32 {
        let full = Duration::from_secs(1) * 2u32.pow(attempts);
        for _ in 0..50 {
            let delay = policy.delay(attempts);
            assert!(delay >= full * 3 / 4, "attempt {attempts}: {delay:?}");
            assert!(delay <= full, "attempt {attempts}: {delay:?}");
        }
    }
}

#[test]
fn consecutive_delays_never_overlap() {
    let policy = policy(1, 600);
    for _ in 0..50 {
        assert!(policy.delay(2) > policy.delay(1));
        assert!(policy.delay(3) > policy.delay(2));
    }
}

#[test]
fn delays_are_capped() {
    let policy = policy(1, 5);
    for attempts in [3u32, 10, 31, u32::MAX] {
        let delay = policy.delay(attempts);
        assert!(delay <= Duration::from_secs(5), "{delay:?}");
        assert!(delay >= Duration::from_secs(5) * 3 / 4, "{delay:?}");
    }
}

#[test]
fn delays_vary_between_samples() {
    let policy = policy(10, 600);
    let samples: HashSet<u128> = (0..50).map(|_| policy.delay(1).as_nanos()).collect();
    assert!(samples.len() > 1, "jitter produced identical delays");
}

#[test]
fn a_zero_base_yields_immediate_retries() {
    let policy = policy(0, 600);
    assert_eq!(policy.delay(1), Duration::ZERO);
}

#[test]
fn transient_codes_are_classified_for_retry() {
    for code in [
        codes::INPUT_UNRESOLVABLE,
        codes::PROBE_FAILED,
        codes::MERGE_LAUNCH_FAILED,
        codes::MERGE_FAILED,
        codes::OUTPUT_INVALID,
        codes::OUTPUT_UPLOAD_FAILED,
    ] {
        assert!(is_transient(code), "{code}");
    }
}

#[test]
fn permanent_codes_never_retry() {
    for code in [
        codes::UNSUPPORTED_OPERATION,
        codes::VALIDATION_BLOCKED,
        codes::DEADLINE_EXCEEDED,
        "anything_unknown",
    ] {
        assert!(!is_transient(code), "{code}");
    }
}

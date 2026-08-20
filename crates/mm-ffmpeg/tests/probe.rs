use std::time::{Duration, Instant};

use mm_ffmpeg::{ProbeError, ProbeLimits, Prober};
use tokio::net::TcpListener;

fn short_limits() -> ProbeLimits {
    ProbeLimits {
        probe_size_bytes: 64 * 1024,
        analyze_duration: Duration::from_millis(500),
        timeout: Duration::from_secs(2),
    }
}

#[tokio::test]
async fn a_stalled_input_is_killed_at_the_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let hold = tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            let (connection, _) = listener.accept().await.unwrap();
            held.push(connection);
        }
    });

    let started = Instant::now();
    let result = Prober::default()
        .probe(&format!("http://{address}/stalled.mp4"), &short_limits())
        .await;
    hold.abort();

    assert!(matches!(result, Err(ProbeError::TimedOut { .. })));
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[tokio::test]
async fn a_missing_input_is_a_typed_failure() {
    let result = Prober::default()
        .probe("surely-absent-fixture.mp4", &short_limits())
        .await;
    assert!(matches!(result, Err(ProbeError::Failed { .. })));
}

#[tokio::test]
async fn a_missing_binary_is_a_launch_error() {
    let result = Prober::with_binary("mm-no-such-probe-binary")
        .probe("anything.mp4", &short_limits())
        .await;
    assert!(matches!(result, Err(ProbeError::Launch { .. })));
}

#[tokio::test]
async fn a_present_binary_reports_its_version() {
    let version = Prober::default()
        .version(Duration::from_secs(5))
        .await
        .expect("probe binary present");
    assert!(!version.is_empty());
    assert!(version.contains("version"));
}

#[tokio::test]
async fn a_missing_binary_has_no_version() {
    let result = Prober::with_binary("mm-no-such-probe-binary")
        .version(Duration::from_secs(5))
        .await;
    assert!(matches!(result, Err(ProbeError::Launch { .. })));
}

#[test]
fn default_limits_cap_the_probe() {
    let limits = ProbeLimits::default();
    assert!(limits.probe_size_bytes <= 8 * 1024 * 1024);
    assert!(limits.analyze_duration <= Duration::from_secs(10));
    assert!(limits.timeout <= Duration::from_secs(60));
}

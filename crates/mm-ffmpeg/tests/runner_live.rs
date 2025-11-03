mod common;

use std::time::Duration;

use mm_ffmpeg::{
    ProgressUpdate, RunError, RunLimits, RunOutcome, Runner, TargetProfile, build_concat_args,
};

const TEST_CAP: Duration = Duration::from_secs(120);

fn pid_is_gone(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == -1 && *libc::__errno_location() == libc::ESRCH }
}

fn merge_args(first: &str, second: &str, output: &str) -> Vec<String> {
    build_concat_args(
        &[first.to_string(), second.to_string()],
        &TargetProfile::default(),
        Some((1280, 720)),
        output,
    )
    .unwrap()
}

fn limits(total: Duration, deadline: Duration) -> RunLimits {
    RunLimits {
        total_duration: total,
        deadline,
        stderr_tail_bytes: 8 * 1024,
    }
}

async fn collect_progress(
    mut receiver: tokio::sync::mpsc::Receiver<ProgressUpdate>,
) -> Vec<ProgressUpdate> {
    let mut updates = Vec::new();
    while let Some(update) = receiver.recv().await {
        updates.push(update);
    }
    updates
}

#[tokio::test(flavor = "multi_thread")]
async fn a_real_merge_reports_monotonic_progress_ending_complete() {
    let body = async {
        let first = common::fixture("merge-a-30fps-720p.mp4", 6, 30, 1280, 720);
        let second = common::fixture("merge-b-25fps-480p.mp4", 4, 25, 640, 480);
        let output = common::scratch_dir().join("merge-supervised.mp4");
        let args = merge_args(
            first.to_str().unwrap(),
            second.to_str().unwrap(),
            output.to_str().unwrap(),
        );

        let mut run = Runner::default()
            .start(
                &args,
                &limits(Duration::from_secs(10), Duration::from_secs(90)),
            )
            .unwrap();
        let progress = tokio::spawn(collect_progress(run.take_progress()));
        let outcome = run.wait().await.unwrap();
        assert!(matches!(outcome, RunOutcome::Succeeded));

        let updates = progress.await.unwrap();
        assert!(!updates.is_empty());
        for pair in updates.windows(2) {
            assert!(pair[1].percent >= pair[0].percent);
        }
        assert!(updates[0].percent >= 0.0);
        let last = updates.last().unwrap();
        assert!((last.percent - 100.0).abs() < f64::EPSILON);
        assert!(last.complete);
        assert!(output.is_file());
    };
    tokio::time::timeout(TEST_CAP, body).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_running_merge_kills_the_process_and_reports_cancelled() {
    let body = async {
        let first = common::fixture("merge-long-a.mp4", 30, 30, 1280, 720);
        let second = common::fixture("merge-long-b.mp4", 30, 25, 1280, 720);
        let output = common::scratch_dir().join("merge-cancelled.mp4");
        let args = merge_args(
            first.to_str().unwrap(),
            second.to_str().unwrap(),
            output.to_str().unwrap(),
        );

        let mut run = Runner::default()
            .start(
                &args,
                &limits(Duration::from_secs(60), Duration::from_secs(90)),
            )
            .unwrap();
        let pid = run.pid();
        let cancel = run.cancel_handle();
        let mut progress = run.take_progress();
        let canceller = tokio::spawn(async move {
            progress.recv().await.expect("one progress update");
            cancel.cancel();
            while progress.recv().await.is_some() {}
        });
        let outcome = run.wait().await.unwrap();
        canceller.await.unwrap();
        assert!(matches!(outcome, RunOutcome::Cancelled));
        assert!(pid_is_gone(pid), "pid {pid} is still alive");
    };
    tokio::time::timeout(TEST_CAP, body).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tiny_deadline_kills_the_merge_and_reports_the_deadline() {
    let body = async {
        let first = common::fixture("merge-long-a.mp4", 30, 30, 1280, 720);
        let second = common::fixture("merge-long-b.mp4", 30, 25, 1280, 720);
        let output = common::scratch_dir().join("merge-deadline.mp4");
        let args = merge_args(
            first.to_str().unwrap(),
            second.to_str().unwrap(),
            output.to_str().unwrap(),
        );

        let mut run = Runner::default()
            .start(
                &args,
                &limits(Duration::from_secs(60), Duration::from_millis(300)),
            )
            .unwrap();
        let pid = run.pid();
        drop(run.take_progress());
        let outcome = run.wait().await.unwrap();
        assert!(matches!(outcome, RunOutcome::DeadlineExceeded { .. }));
        assert!(pid_is_gone(pid), "pid {pid} is still alive");
    };
    tokio::time::timeout(TEST_CAP, body).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_nonzero_exit_maps_to_failed_with_a_bounded_stderr_tail() {
    let body = async {
        let output = common::scratch_dir().join("merge-failed.mp4");
        let args = merge_args(
            "/nonexistent/one.mp4",
            "/nonexistent/two.mp4",
            output.to_str().unwrap(),
        );
        let mut run = Runner::default()
            .start(
                &args,
                &RunLimits {
                    total_duration: Duration::from_secs(10),
                    deadline: Duration::from_secs(30),
                    stderr_tail_bytes: 64,
                },
            )
            .unwrap();
        drop(run.take_progress());
        let outcome = run.wait().await.unwrap();
        match outcome {
            RunOutcome::Failed {
                status,
                stderr_tail,
            } => {
                assert!(status.is_some_and(|code| code != 0));
                assert!(!stderr_tail.is_empty());
                assert!(stderr_tail.len() <= 64);
            }
            other => panic!("expected failure, got {other:?}"),
        }
    };
    tokio::time::timeout(TEST_CAP, body).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_binary_is_a_typed_launch_error() {
    let error = Runner::with_binary("/nonexistent/media-tool")
        .start(
            &["-version".to_string()],
            &limits(Duration::from_secs(1), Duration::from_secs(1)),
        )
        .unwrap_err();
    assert!(matches!(error, RunError::Launch { .. }));
}

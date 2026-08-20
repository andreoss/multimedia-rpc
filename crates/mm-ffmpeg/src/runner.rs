use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::progress::{ProgressParser, ProgressUpdate};

const PROGRESS_CHANNEL_CAPACITY: usize = 64;
const STDERR_CHUNK_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct RunLimits {
    pub total_duration: Duration,
    pub deadline: Duration,
    pub stderr_tail_bytes: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("tool failed to launch: {source}")]
    Launch { source: std::io::Error },
    #[error("tool could not be awaited: {source}")]
    Wait { source: std::io::Error },
    #[error("the encoder list could not be read")]
    Encoders,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RunOutcome {
    Succeeded,
    Cancelled,
    DeadlineExceeded {
        after: Duration,
    },
    Failed {
        status: Option<i32>,
        stderr_tail: String,
    },
}

#[derive(Debug, Clone)]
pub struct CancelHandle {
    sender: watch::Sender<bool>,
}

impl CancelHandle {
    pub fn cancel(&self) {
        let _ = self.sender.send(true);
    }
}

#[derive(Debug, Clone)]
pub struct Runner {
    binary: PathBuf,
}

impl Default for Runner {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("ffmpeg"),
        }
    }
}

impl Runner {
    pub fn with_binary(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
        }
    }

    pub async fn available_encoders(&self, timeout: Duration) -> Result<Vec<String>, RunError> {
        let mut command = Command::new(&self.binary);
        command
            .args(["-hide_banner", "-encoders"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = tokio::time::timeout(timeout, command.output())
            .await
            .map_err(|_| RunError::Encoders)?
            .map_err(|source| RunError::Launch { source })?;
        if !output.status.success() {
            return Err(RunError::Encoders);
        }
        Ok(parse_encoders(&String::from_utf8_lossy(&output.stdout)))
    }

    pub fn start(&self, args: &[String], limits: &RunLimits) -> Result<MergeRun, RunError> {
        let mut child = Command::new(&self.binary)
            .args(["-progress", "pipe:1", "-nostats", "-v", "error"])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| RunError::Launch { source })?;
        let pid = child.id().expect("a freshly spawned child has a pid");
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");

        let (progress_sender, progress_receiver) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let parser = ProgressParser::new(limits.total_duration);
        let progress_task = tokio::spawn(stream_progress(stdout, parser, progress_sender));
        let stderr_task = tokio::spawn(capture_tail(stderr, limits.stderr_tail_bytes));
        let (cancel_sender, cancel_receiver) = watch::channel(false);

        Ok(MergeRun {
            child,
            pid,
            deadline: limits.deadline,
            progress: Some(progress_receiver),
            cancel: CancelHandle {
                sender: cancel_sender,
            },
            cancelled: cancel_receiver,
            progress_task,
            stderr_task,
        })
    }
}

#[derive(Debug)]
pub struct MergeRun {
    child: Child,
    pid: u32,
    deadline: Duration,
    progress: Option<mpsc::Receiver<ProgressUpdate>>,
    cancel: CancelHandle,
    cancelled: watch::Receiver<bool>,
    progress_task: JoinHandle<()>,
    stderr_task: JoinHandle<Vec<u8>>,
}

impl MergeRun {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn cancel_handle(&self) -> CancelHandle {
        self.cancel.clone()
    }

    pub fn take_progress(&mut self) -> mpsc::Receiver<ProgressUpdate> {
        self.progress.take().expect("progress was already taken")
    }

    pub async fn wait(self) -> Result<RunOutcome, RunError> {
        self.wait_inner().await.map(|(outcome, _)| outcome)
    }

    pub async fn wait_with_stderr(self) -> Result<(RunOutcome, String), RunError> {
        self.wait_inner().await
    }

    async fn wait_inner(self) -> Result<(RunOutcome, String), RunError> {
        let MergeRun {
            mut child,
            pid,
            deadline,
            progress,
            cancel,
            mut cancelled,
            progress_task,
            stderr_task,
        } = self;
        drop(progress);

        let cancel_requested = async move {
            if cancelled.wait_for(|flag| *flag).await.is_err() {
                std::future::pending::<()>().await;
            }
        };

        let interrupt = tokio::select! {
            waited = child.wait() => Interrupt::Exited(waited),
            () = cancel_requested => Interrupt::Cancelled,
            () = tokio::time::sleep(deadline) => Interrupt::DeadlineExceeded,
        };

        let (outcome, stderr) = match interrupt {
            Interrupt::Exited(waited) => {
                let status = waited.map_err(|source| RunError::Wait { source })?;
                let tail =
                    String::from_utf8_lossy(&stderr_task.await.unwrap_or_default()).into_owned();
                let outcome = if status.success() {
                    RunOutcome::Succeeded
                } else {
                    RunOutcome::Failed {
                        status: status.code(),
                        stderr_tail: tail.clone(),
                    }
                };
                (outcome, tail)
            }
            Interrupt::Cancelled => {
                kill_group(pid);
                let _ = child.wait().await;
                let tail =
                    String::from_utf8_lossy(&stderr_task.await.unwrap_or_default()).into_owned();
                (RunOutcome::Cancelled, tail)
            }
            Interrupt::DeadlineExceeded => {
                kill_group(pid);
                let _ = child.wait().await;
                let tail =
                    String::from_utf8_lossy(&stderr_task.await.unwrap_or_default()).into_owned();
                (RunOutcome::DeadlineExceeded { after: deadline }, tail)
            }
        };
        let _ = progress_task.await;
        drop(cancel);
        Ok((outcome, stderr))
    }
}

enum Interrupt {
    Exited(std::io::Result<std::process::ExitStatus>),
    Cancelled,
    DeadlineExceeded,
}

fn kill_group(pid: u32) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

async fn stream_progress(
    stdout: ChildStdout,
    mut parser: ProgressParser,
    sender: mpsc::Sender<ProgressUpdate>,
) {
    let mut lines = BufReader::new(stdout).lines();
    let mut open = true;
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(update) = parser.feed_line(&line)
            && open
            && sender.send(update).await.is_err()
        {
            open = false;
        }
    }
}

async fn capture_tail(mut stderr: ChildStderr, cap: usize) -> Vec<u8> {
    let mut tail = Vec::new();
    let mut buffer = [0u8; STDERR_CHUNK_BYTES];
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                tail.extend_from_slice(&buffer[..count]);
                if tail.len() > cap {
                    let excess = tail.len() - cap;
                    tail.drain(..excess);
                }
            }
        }
    }
    tail
}

pub fn parse_encoders(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| line.starts_with(" V") || line.starts_with(" A"))
        .filter_map(|line| line.split_whitespace().nth(1))
        .filter(|name| *name != "=")
        .map(str::to_string)
        .collect()
}

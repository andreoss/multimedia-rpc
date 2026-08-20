use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::args::IDENTIFY_FORMAT;
use crate::args::identify_args;
use crate::parse::{ImageInfo, parse_identify_output};

const MAX_OUTPUT_BYTES: u64 = 1024 * 1024;
const STDERR_TAIL_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct ToolLimits {
    pub output_bytes: u64,
    pub timeout: Duration,
    pub stderr_tail_bytes: usize,
}

impl Default for ToolLimits {
    fn default() -> Self {
        Self {
            output_bytes: MAX_OUTPUT_BYTES,
            timeout: Duration::from_secs(60),
            stderr_tail_bytes: STDERR_TAIL_BYTES,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("image tool failed to launch: {source}")]
    Launch { source: std::io::Error },
    #[error("image tool timed out after {after:?}")]
    TimedOut { after: Duration },
    #[error("image tool exited with status {status:?}: {stderr_tail}")]
    Failed {
        status: Option<i32>,
        stderr_tail: String,
    },
    #[error("image tool output could not be read: {source}")]
    Output { source: std::io::Error },
    #[error(transparent)]
    Parse(#[from] crate::parse::ParseError),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Succeeded,
    Failed {
        status: Option<i32>,
        stderr_tail: String,
    },
}

#[derive(Debug, Clone)]
pub struct Tool {
    binary: PathBuf,
}

impl Default for Tool {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("magick"),
        }
    }
}

impl Tool {
    pub fn with_binary(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
        }
    }

    pub async fn version(&self, timeout: Duration) -> Result<String, ToolError> {
        let text = self.capture(&["-version".to_string()], timeout).await?;
        Ok(text.lines().next().unwrap_or_default().trim().to_string())
    }

    pub async fn identify(&self, input: &str, limits: &ToolLimits) -> Result<ImageInfo, ToolError> {
        let text = self
            .capture(&identify_args(IDENTIFY_FORMAT, input), limits.timeout)
            .await?;
        Ok(parse_identify_output(&text)?)
    }

    pub async fn run(&self, args: &[String], limits: &ToolLimits) -> Result<Outcome, ToolError> {
        self.capture(args, limits.timeout).await?;
        Ok(Outcome::Succeeded)
    }

    async fn capture(&self, args: &[String], timeout: Duration) -> Result<String, ToolError> {
        let mut child = Command::new(&self.binary)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| ToolError::Launch { source })?;

        let mut stdout = child.stdout.take().expect("stdout is piped");
        let mut stderr = child.stderr.take().expect("stderr is piped");
        let bounded_out = async {
            let mut buffer = Vec::new();
            let mut reader = (&mut stdout).take(MAX_OUTPUT_BYTES);
            reader.read_to_end(&mut buffer).await.map(|_| buffer)
        };
        let bounded_err = async {
            let mut buffer = Vec::new();
            let mut reader = (&mut stderr).take(MAX_OUTPUT_BYTES);
            reader.read_to_end(&mut buffer).await.map(|_| buffer)
        };

        let outcome = tokio::time::timeout(timeout, async {
            tokio::join!(bounded_out, bounded_err, child.wait())
        })
        .await;

        let (drained, errors, waited) = match outcome {
            Ok(result) => result,
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(ToolError::TimedOut { after: timeout });
            }
        };

        let status = waited.map_err(|source| ToolError::Output { source })?;
        let tail = String::from_utf8_lossy(&errors.map_err(|source| ToolError::Output { source })?)
            .into_owned();
        if !status.success() {
            return Err(ToolError::Failed {
                status: status.code(),
                stderr_tail: tail,
            });
        }
        let output = drained.map_err(|source| ToolError::Output { source })?;
        Ok(String::from_utf8_lossy(&output).into_owned())
    }
}

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::parse::{
    MediaInfo, ParseError, StillInfo, parse_keyframe_times, parse_media_info, parse_still_info,
};

const MAX_OUTPUT_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct ProbeLimits {
    pub probe_size_bytes: u64,
    pub analyze_duration: Duration,
    pub timeout: Duration,
}

impl Default for ProbeLimits {
    fn default() -> Self {
        Self {
            probe_size_bytes: 1024 * 1024,
            analyze_duration: Duration::from_secs(2),
            timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("probe binary failed to launch: {source}")]
    Launch { source: std::io::Error },
    #[error("probe timed out after {after:?}")]
    TimedOut { after: Duration },
    #[error("probe exited with status {status:?}")]
    Failed { status: Option<i32> },
    #[error("probe output could not be read: {source}")]
    Output { source: std::io::Error },
    #[error(transparent)]
    Parse(#[from] ParseError),
}

#[derive(Debug, Clone)]
pub struct Prober {
    binary: PathBuf,
}

impl Default for Prober {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("ffprobe"),
        }
    }
}

impl Prober {
    pub fn with_binary(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
        }
    }

    pub async fn version(&self, timeout: Duration) -> Result<String, ProbeError> {
        let limits = ProbeLimits {
            timeout,
            ..ProbeLimits::default()
        };
        let text = self.capture(&["-version".to_string()], &limits).await?;
        Ok(text.lines().next().unwrap_or_default().trim().to_string())
    }

    pub async fn probe(&self, input: &str, limits: &ProbeLimits) -> Result<MediaInfo, ProbeError> {
        let text = self.capture(&stream_args(limits, input), limits).await?;
        Ok(parse_media_info(&text)?)
    }

    pub async fn subtitle_streams(
        &self,
        input: &str,
        limits: &ProbeLimits,
    ) -> Result<Vec<crate::parse::SubtitleStream>, ProbeError> {
        let text = self.capture(&stream_args(limits, input), limits).await?;
        Ok(crate::parse::parse_subtitle_streams(&text)?)
    }

    pub async fn still(&self, input: &str, limits: &ProbeLimits) -> Result<StillInfo, ProbeError> {
        let mut args = vec![
            "-v".to_string(),
            "quiet".to_string(),
            "-print_format".to_string(),
            "json".to_string(),
            "-show_format".to_string(),
            "-show_streams".to_string(),
        ];
        args.extend(limits_args(limits));
        args.push(input.to_string());
        let text = self.capture(&args, limits).await?;
        Ok(parse_still_info(&text)?)
    }

    pub async fn keyframes(
        &self,
        input: &str,
        limits: &ProbeLimits,
    ) -> Result<Vec<f64>, ProbeError> {
        let mut args = vec![
            "-v".to_string(),
            "quiet".to_string(),
            "-print_format".to_string(),
            "csv=p=0".to_string(),
            "-select_streams".to_string(),
            "v:0".to_string(),
            "-skip_frame".to_string(),
            "nokey".to_string(),
            "-show_frames".to_string(),
            "-show_entries".to_string(),
            "frame=best_effort_timestamp_time".to_string(),
        ];
        args.extend(limits_args(limits));
        args.push(input.to_string());
        let text = self.capture(&args, limits).await?;
        Ok(parse_keyframe_times(&text))
    }

    async fn capture(&self, args: &[String], limits: &ProbeLimits) -> Result<String, ProbeError> {
        let mut child = Command::new(&self.binary)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| ProbeError::Launch { source })?;

        let mut stdout = child.stdout.take().expect("stdout is piped");
        let bounded = async {
            let mut buffer = Vec::new();
            let mut reader = (&mut stdout).take(MAX_OUTPUT_BYTES);
            reader.read_to_end(&mut buffer).await.map(|_| buffer)
        };

        let outcome = tokio::time::timeout(limits.timeout, async {
            tokio::join!(bounded, child.wait())
        })
        .await;

        let (drained, waited) = match outcome {
            Ok(result) => result,
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(ProbeError::TimedOut {
                    after: limits.timeout,
                });
            }
        };

        let status = waited.map_err(|source| ProbeError::Output { source })?;
        if !status.success() {
            return Err(ProbeError::Failed {
                status: status.code(),
            });
        }
        let output = drained.map_err(|source| ProbeError::Output { source })?;
        Ok(String::from_utf8_lossy(&output).into_owned())
    }
}

fn limits_args(limits: &ProbeLimits) -> Vec<String> {
    vec![
        "-probesize".to_string(),
        limits.probe_size_bytes.to_string(),
        "-analyzeduration".to_string(),
        limits.analyze_duration.as_micros().to_string(),
    ]
}

fn stream_args(limits: &ProbeLimits, input: &str) -> Vec<String> {
    let mut args = vec![
        "-v".to_string(),
        "quiet".to_string(),
        "-print_format".to_string(),
        "json".to_string(),
        "-show_format".to_string(),
        "-show_streams".to_string(),
    ];
    args.extend(limits_args(limits));
    args.push(input.to_string());
    args
}

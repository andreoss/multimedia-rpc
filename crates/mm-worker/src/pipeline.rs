use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use bytes::Bytes;
use mm_domain::{
    ConcatSpec, InputReport, JobFailure, JobId, JobRecord, JobRepository, JobStoreError, MediaRef,
    ObjectBody, ObjectRef, ObjectStoreError, OpKind, OpResult, OpSpec, OutputKey, ProbeResult,
    ProbeSpec, ProgressPercent, TimedOutput, TranscodeResult, TranscodeSpec, UnsupportedOp,
    WorkerId, resolve_media_ref, validate_inputs,
};
use mm_ffmpeg::{
    MediaInfo, RunLimits, RunOutcome, ScaleMode, TranscodeTarget, build_concat_args,
    build_transcode_args,
};
use prost::Message;
use tokio::io::AsyncReadExt;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::WorkerConfig;
use crate::report::{blocks, input_report, render_verdicts, target_profile, warnings};

pub mod codes {
    pub const UNSUPPORTED_OPERATION: &str = "unsupported_operation";
    pub const INPUT_UNRESOLVABLE: &str = "input_unresolvable";
    pub const PROBE_FAILED: &str = "probe_failed";
    pub const VALIDATION_BLOCKED: &str = "validation_blocked";
    pub const TIMEOUT: &str = "timeout";
    pub const MERGE_LAUNCH_FAILED: &str = "merge_launch_failed";
    pub const MERGE_FAILED: &str = "merge_failed";
    pub const OUTPUT_INVALID: &str = "output_invalid";
    pub const OUTPUT_UPLOAD_FAILED: &str = "output_upload_failed";
}

const UPLOAD_CHUNK_BYTES: usize = 1024 * 1024;

enum Finish {
    Cancelled,
    LeaseLost,
    Released,
    TimedOut,
    Failed(JobFailure),
}

struct Reservation<'a> {
    ledger: &'a std::sync::Mutex<u64>,
    amount: u64,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        let mut reserved = self.ledger.lock().expect("ledger lock");
        *reserved = reserved.saturating_sub(self.amount);
    }
}

fn admit(ctx: &WorkerConfig, estimate: u64) -> Result<Option<Reservation<'_>>, u64> {
    let Some(available) = ctx.disk_budget.available(&ctx.staging_dir) else {
        return Ok(None);
    };
    let mut reserved = ctx.ledger().lock().expect("ledger lock");
    if !crate::budget::admits(estimate, available, *reserved) {
        return Err(available);
    }
    *reserved = reserved.saturating_add(estimate);
    Ok(Some(Reservation {
        ledger: ctx.ledger(),
        amount: estimate,
    }))
}

async fn estimate_bytes(ctx: &WorkerConfig, inputs: &[MediaRef], infos: &[MediaInfo]) -> u64 {
    let mut total = 0u64;
    for (input, info) in inputs.iter().zip(infos) {
        let size = match info.size_bytes {
            Some(size) => size,
            None => match input {
                MediaRef::Object(object) => ctx.store.head(object).await.unwrap_or(0),
                MediaRef::Http(_) => 0,
            },
        };
        total = total.saturating_add(size);
    }
    total
}

fn failed(code: &str, message: impl ToString) -> Finish {
    Finish::Failed(
        JobFailure::new(code, &message.to_string()).expect("worker failure codes are valid"),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signal {
    Run,
    Cancel,
    LeaseLost,
    Deadline,
}

pub(crate) struct CancelRegistry {
    entries: std::sync::Mutex<std::collections::HashMap<JobId, watch::Sender<Signal>>>,
}

impl CancelRegistry {
    pub(crate) fn new() -> Self {
        Self {
            entries: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn register(&self, id: JobId, sender: watch::Sender<Signal>) {
        self.entries
            .lock()
            .expect("registry lock")
            .insert(id, sender);
    }

    fn deregister(&self, id: JobId) {
        self.entries.lock().expect("registry lock").remove(&id);
    }

    pub(crate) fn cancel(&self, id: JobId) {
        if let Some(sender) = self.entries.lock().expect("registry lock").get(&id) {
            let _ = sender.send(Signal::Cancel);
        }
    }
}

struct Registered<'a> {
    registry: &'a CancelRegistry,
    id: JobId,
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        self.registry.deregister(self.id);
    }
}

struct Supervisor {
    task: JoinHandle<()>,
}

impl Supervisor {
    fn start(
        repository: Arc<dyn JobRepository>,
        id: JobId,
        worker: WorkerId,
        interval: Duration,
        lease: Duration,
        deadline_at: SystemTime,
        sender: watch::Sender<Signal>,
    ) -> Self {
        let task = tokio::spawn(async move {
            if let Ok(Some(record)) = repository.get(id).await
                && record.cancel_requested
            {
                let _ = sender.send(Signal::Cancel);
                return;
            }
            loop {
                let remaining = deadline_at
                    .duration_since(SystemTime::now())
                    .unwrap_or(Duration::ZERO);
                if remaining.is_zero() {
                    let _ = sender.send(Signal::Deadline);
                    return;
                }
                tokio::time::sleep(interval.min(remaining)).await;
                if SystemTime::now() >= deadline_at {
                    let _ = sender.send(Signal::Deadline);
                    return;
                }
                match repository.heartbeat(id, &worker, lease).await {
                    Ok(_) => {}
                    Err(JobStoreError::Backend(_)) => continue,
                    Err(_) => {
                        let _ = sender.send(Signal::LeaseLost);
                        return;
                    }
                }
                match repository.get(id).await {
                    Ok(Some(record)) if record.cancel_requested => {
                        let _ = sender.send(Signal::Cancel);
                        return;
                    }
                    _ => {}
                }
            }
        });
        Self { task }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn check(signal: &watch::Receiver<Signal>) -> Result<(), Finish> {
    match *signal.borrow() {
        Signal::Run => Ok(()),
        Signal::Cancel => Err(Finish::Cancelled),
        Signal::LeaseLost => Err(Finish::LeaseLost),
        Signal::Deadline => Err(Finish::TimedOut),
    }
}

fn log_line(message: std::fmt::Arguments<'_>) {
    use std::io::Write;
    let _ = writeln!(std::io::stdout(), "{message}");
}

fn timeout_failure() -> JobFailure {
    JobFailure::new(codes::TIMEOUT, "job deadline passed").expect("worker failure codes are valid")
}

pub(crate) async fn publish_outcome(ctx: &WorkerConfig, record: &JobRecord) {
    let Some(publisher) = &ctx.callbacks else {
        return;
    };
    let Some(queue) = &record.callback else {
        return;
    };
    if !matches!(ctx.repository.claim_callback(record.id).await, Ok(true)) {
        return;
    }
    let body = BASE64_STANDARD.encode(mm_proto::v1::JobOutcome::from(record).encode_to_vec());
    let mut attempt = 0;
    loop {
        attempt += 1;
        match publisher.publish(queue, &body).await {
            Ok(()) => return,
            Err(error) if attempt < ctx.retry.max_attempts => {
                log_line(format_args!(
                    "job {} callback attempt {attempt} failed: {error}",
                    record.id
                ));
                tokio::time::sleep(ctx.retry.delay(attempt)).await;
            }
            Err(error) => {
                log_line(format_args!(
                    "job {} callback abandoned after {attempt} attempts: {error}",
                    record.id
                ));
                return;
            }
        }
    }
}

pub async fn execute(ctx: &Arc<WorkerConfig>, job: JobRecord, registry: &CancelRegistry) {
    if job.cancel_requested {
        if let Ok(record) = ctx.repository.mark_cancelled(job.id).await {
            publish_outcome(ctx, &record).await;
        }
        return;
    }
    let deadline_at = job
        .deadline_at
        .unwrap_or_else(|| SystemTime::now() + ctx.default_deadline);
    if SystemTime::now() >= deadline_at {
        if let Ok(record) = ctx
            .repository
            .mark_failed(job.id, &ctx.worker_id, timeout_failure())
            .await
        {
            publish_outcome(ctx, &record).await;
        }
        return;
    }
    let (sender, mut signal) = watch::channel(Signal::Run);
    registry.register(job.id, sender.clone());
    let registered = Registered {
        registry,
        id: job.id,
    };
    let supervisor = Supervisor::start(
        ctx.repository.clone(),
        job.id,
        ctx.worker_id.clone(),
        ctx.heartbeat_interval,
        ctx.lease,
        deadline_at,
        sender,
    );
    let outcome = pipeline(ctx, &job, deadline_at, &mut signal).await;
    drop(supervisor);
    drop(registered);
    match outcome {
        Ok(result) => {
            if let Ok(record) = ctx
                .repository
                .mark_succeeded(job.id, &ctx.worker_id, result)
                .await
            {
                publish_outcome(ctx, &record).await;
            }
        }
        Err(Finish::Cancelled) => {
            if let Ok(record) = ctx.repository.mark_cancelled(job.id).await {
                publish_outcome(ctx, &record).await;
            }
        }
        Err(Finish::LeaseLost) | Err(Finish::Released) => {}
        Err(Finish::TimedOut) => {
            if let Ok(record) = ctx
                .repository
                .mark_failed(job.id, &ctx.worker_id, timeout_failure())
                .await
            {
                publish_outcome(ctx, &record).await;
            }
        }
        Err(Finish::Failed(failure)) => {
            if crate::retry::is_transient(failure.code()) && job.attempts < ctx.retry.max_attempts {
                let delay = ctx.retry.delay(job.attempts);
                let _ = ctx
                    .repository
                    .retry(job.id, &ctx.worker_id, failure, delay)
                    .await;
            } else if let Ok(record) = ctx
                .repository
                .mark_failed(job.id, &ctx.worker_id, failure)
                .await
            {
                publish_outcome(ctx, &record).await;
            }
        }
    }
}

async fn pipeline(
    ctx: &Arc<WorkerConfig>,
    job: &JobRecord,
    deadline_at: SystemTime,
    signal: &mut watch::Receiver<Signal>,
) -> Result<OpResult, Finish> {
    match &job.spec {
        OpSpec::Concat(spec) => concat_pipeline(ctx, job, spec, deadline_at, signal).await,
        OpSpec::Probe(spec) => probe_pipeline(ctx, spec, deadline_at, signal).await,
        OpSpec::Transcode(spec) => transcode_pipeline(ctx, job, spec, deadline_at, signal).await,
        unsupported => Err(failed(
            codes::UNSUPPORTED_OPERATION,
            UnsupportedOp {
                kind: unsupported.kind(),
            },
        )),
    }
}

async fn resolve_input(
    ctx: &WorkerConfig,
    input: &MediaRef,
    deadline_at: SystemTime,
    index: usize,
) -> Result<String, Finish> {
    resolve_media_ref(ctx.store.as_ref(), input, deadline_at, SystemTime::now())
        .await
        .map_err(|error| failed(codes::INPUT_UNRESOLVABLE, format!("input {index}: {error}")))
}

async fn probe_input(ctx: &WorkerConfig, url: &str, index: usize) -> Result<MediaInfo, Finish> {
    ctx.prober
        .probe(url, &ctx.probe_limits)
        .await
        .map_err(|error| failed(codes::PROBE_FAILED, format!("input {index}: {error}")))
}

fn gate_verdicts(id: JobId, kind: OpKind, reports: &[InputReport]) -> Result<(), Finish> {
    let verdicts = validate_inputs(kind, reports);
    let blocking = blocks(&verdicts);
    if !blocking.is_empty() {
        return Err(failed(
            codes::VALIDATION_BLOCKED,
            render_verdicts(&blocking),
        ));
    }
    let warned = warnings(&verdicts);
    if !warned.is_empty() {
        log_line(format_args!(
            "job {id} warnings: {}",
            render_verdicts(&warned)
        ));
    }
    Ok(())
}

async fn admit_or_release<'a>(
    ctx: &'a Arc<WorkerConfig>,
    id: JobId,
    estimate: u64,
) -> Result<Option<Reservation<'a>>, Finish> {
    tokio::fs::create_dir_all(&ctx.staging_dir)
        .await
        .map_err(|error| failed(codes::MERGE_LAUNCH_FAILED, error))?;
    match admit(ctx, estimate) {
        Ok(reservation) => Ok(reservation),
        Err(available) => {
            log_line(format_args!(
                "job {id} released: estimated {estimate} bytes exceed the \
                 available {available}"
            ));
            let _ = ctx
                .repository
                .release(id, &ctx.worker_id, ctx.release_retry)
                .await;
            Err(Finish::Released)
        }
    }
}

fn remaining_limits(
    ctx: &WorkerConfig,
    total: Duration,
    deadline_at: SystemTime,
) -> Result<RunLimits, Finish> {
    let remaining = deadline_at
        .duration_since(SystemTime::now())
        .unwrap_or(Duration::ZERO);
    if remaining.is_zero() {
        return Err(Finish::TimedOut);
    }
    Ok(RunLimits {
        total_duration: total,
        deadline: remaining,
        stderr_tail_bytes: ctx.stderr_tail_bytes,
    })
}

fn stream_count(streams: usize) -> u32 {
    u32::try_from(streams).unwrap_or(u32::MAX)
}

async fn probe_pipeline(
    ctx: &Arc<WorkerConfig>,
    spec: &ProbeSpec,
    deadline_at: SystemTime,
    signal: &mut watch::Receiver<Signal>,
) -> Result<OpResult, Finish> {
    check(signal)?;
    let url = resolve_input(ctx, spec.input(), deadline_at, 0).await?;
    check(signal)?;
    let info = probe_input(ctx, &url, 0).await?;
    check(signal)?;
    let result = ProbeResult::new(
        info.duration.as_secs_f64(),
        stream_count(info.video.len()),
        stream_count(info.audio.len()),
        info.container.clone(),
    )
    .map_err(|error| failed(codes::PROBE_FAILED, error))?;
    Ok(OpResult::Probe(result))
}

async fn transcode_pipeline(
    ctx: &Arc<WorkerConfig>,
    job: &JobRecord,
    spec: &TranscodeSpec,
    deadline_at: SystemTime,
    signal: &mut watch::Receiver<Signal>,
) -> Result<OpResult, Finish> {
    check(signal)?;
    let url = resolve_input(ctx, spec.input(), deadline_at, 0).await?;
    check(signal)?;
    let info = probe_input(ctx, &url, 0).await?;
    gate_verdicts(job.id, OpKind::Transcode, &[input_report(&info)])?;

    let inputs = [spec.input().clone()];
    let infos = [info];
    let estimate = estimate_bytes(ctx, &inputs, &infos).await;
    let _reservation = admit_or_release(ctx, job.id, estimate).await?;

    let target = TranscodeTarget {
        scale_mode: ScaleMode::PadToFit,
        profile: target_profile(Some(spec.profile())),
        ..TranscodeTarget::default()
    };
    let output_path = ctx.staging_dir.join(format!("{}.mp4", job.id));
    let args = build_transcode_args(&url, &target, &output_path.to_string_lossy())
        .map_err(|error| failed(codes::MERGE_LAUNCH_FAILED, error))?;
    let limits = remaining_limits(ctx, infos[0].duration, deadline_at)?;

    check(signal)?;
    let transcoded = run_and_upload(
        ctx,
        job.id,
        &args,
        &limits,
        &output_path,
        signal,
        OutputShape::Transcode,
    )
    .await;
    let _ = tokio::fs::remove_file(&output_path).await;
    transcoded
}

async fn concat_pipeline(
    ctx: &Arc<WorkerConfig>,
    job: &JobRecord,
    spec: &ConcatSpec,
    deadline_at: SystemTime,
    signal: &mut watch::Receiver<Signal>,
) -> Result<OpResult, Finish> {
    let mut urls = Vec::with_capacity(spec.inputs().len());
    for (index, input) in spec.inputs().iter().enumerate() {
        check(signal)?;
        urls.push(resolve_input(ctx, input, deadline_at, index).await?);
    }

    let mut infos: Vec<MediaInfo> = Vec::with_capacity(urls.len());
    for (index, url) in urls.iter().enumerate() {
        check(signal)?;
        infos.push(probe_input(ctx, url, index).await?);
    }

    let reports: Vec<_> = infos.iter().map(input_report).collect();
    gate_verdicts(job.id, OpKind::Concat, &reports)?;

    let estimate = estimate_bytes(ctx, spec.inputs(), &infos).await;
    let _reservation = admit_or_release(ctx, job.id, estimate).await?;

    let total: Duration = infos.iter().map(|info| info.duration).sum();
    let first_resolution = infos[0]
        .video
        .first()
        .map(|video| (video.width, video.height));
    let profile = target_profile(spec.profile());
    let output_path = ctx.staging_dir.join(format!("{}.mp4", job.id));
    let args = build_concat_args(
        &urls,
        &profile,
        first_resolution,
        &output_path.to_string_lossy(),
    )
    .map_err(|error| failed(codes::MERGE_LAUNCH_FAILED, error))?;
    let limits = remaining_limits(ctx, total, deadline_at)?;

    check(signal)?;
    let merged = run_and_upload(
        ctx,
        job.id,
        &args,
        &limits,
        &output_path,
        signal,
        OutputShape::Concat,
    )
    .await;
    let _ = tokio::fs::remove_file(&output_path).await;
    merged
}

enum OutputShape {
    Concat,
    Transcode,
}

async fn run_and_upload(
    ctx: &Arc<WorkerConfig>,
    id: JobId,
    args: &[String],
    limits: &RunLimits,
    output_path: &PathBuf,
    signal: &mut watch::Receiver<Signal>,
    shape: OutputShape,
) -> Result<OpResult, Finish> {
    let mut run = ctx
        .runner
        .start(args, limits)
        .map_err(|error| failed(codes::MERGE_LAUNCH_FAILED, error))?;
    let cancel = run.cancel_handle();
    let mut progress = run.take_progress();
    let wait = run.wait();
    tokio::pin!(wait);

    let mut progress_open = true;
    let mut signal_open = true;
    let mut interrupted: Option<Finish> = None;
    let mut last_persisted = Instant::now() - ctx.progress_interval;
    let outcome = loop {
        tokio::select! {
            waited = &mut wait => {
                break waited.map_err(|error| failed(codes::MERGE_FAILED, error))?;
            }
            update = progress.recv(), if progress_open => {
                match update {
                    Some(update) => {
                        if last_persisted.elapsed() >= ctx.progress_interval || update.complete {
                            let percent = ProgressPercent::new(update.percent as u8)
                                .unwrap_or(ProgressPercent::COMPLETE);
                            let _ = ctx
                                .repository
                                .set_progress(id, &ctx.worker_id, percent)
                                .await;
                            last_persisted = Instant::now();
                        }
                    }
                    None => progress_open = false,
                }
            }
            changed = signal.changed(), if signal_open => {
                if changed.is_err() {
                    signal_open = false;
                    continue;
                }
                match *signal.borrow() {
                    Signal::Run => {}
                    Signal::Cancel => {
                        interrupted = Some(Finish::Cancelled);
                        cancel.cancel();
                    }
                    Signal::LeaseLost => {
                        interrupted = Some(Finish::LeaseLost);
                        cancel.cancel();
                    }
                    Signal::Deadline => {
                        interrupted = Some(Finish::TimedOut);
                        cancel.cancel();
                    }
                }
            }
        }
    };

    match outcome {
        RunOutcome::Succeeded => {}
        RunOutcome::Cancelled => {
            return Err(interrupted.unwrap_or(Finish::Cancelled));
        }
        RunOutcome::DeadlineExceeded { after } => {
            return Err(failed(
                codes::TIMEOUT,
                format!("merge exceeded the deadline after {after:?}"),
            ));
        }
        RunOutcome::Failed {
            status,
            stderr_tail,
        } => {
            return Err(failed(
                codes::MERGE_FAILED,
                format!("exit status {status:?}: {stderr_tail}"),
            ));
        }
    }

    check(signal)?;
    let info = ctx
        .prober
        .probe(&output_path.to_string_lossy(), &ctx.probe_limits)
        .await
        .map_err(|error| failed(codes::OUTPUT_INVALID, error))?;
    let object = output_object(ctx, id).map_err(|error| failed(codes::OUTPUT_INVALID, error))?;
    let file = tokio::fs::File::open(output_path)
        .await
        .map_err(|error| failed(codes::OUTPUT_UPLOAD_FAILED, error))?;
    ctx.store
        .put_multipart(&object, file_body(file))
        .await
        .map_err(|error| failed(codes::OUTPUT_UPLOAD_FAILED, error))?;
    if let Err(finish) = check(signal) {
        let _ = ctx.store.delete(&object).await;
        return Err(finish);
    }

    match shape {
        OutputShape::Concat => {
            let timed = TimedOutput::new(MediaRef::Object(object), info.duration.as_secs_f64())
                .map_err(|error| failed(codes::OUTPUT_INVALID, error))?;
            Ok(OpResult::Concat(timed))
        }
        OutputShape::Transcode => Ok(OpResult::Transcode(TranscodeResult::new(MediaRef::Object(
            object,
        )))),
    }
}

fn output_object(ctx: &WorkerConfig, id: JobId) -> Result<ObjectRef, String> {
    OutputKey::new(&id, "output.mp4")
        .map_err(|error| error.to_string())?
        .into_object(&ctx.bucket)
        .map_err(|error| error.to_string())
}

fn file_body(file: tokio::fs::File) -> ObjectBody {
    Box::pin(futures_util::stream::unfold(file, |mut file| async move {
        let mut buffer = vec![0u8; UPLOAD_CHUNK_BYTES];
        match file.read(&mut buffer).await {
            Ok(0) => None,
            Ok(count) => {
                buffer.truncate(count);
                Some((Ok(Bytes::from(buffer)), file))
            }
            Err(error) => Some((
                Err(ObjectStoreError::Backend {
                    message: error.to_string(),
                }),
                file,
            )),
        }
    }))
}

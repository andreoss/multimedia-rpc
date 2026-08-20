use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use bytes::Bytes;
use mm_domain::{
    ConcatSpec, ImageFormat as DomainImageFormat, ImageOp, ImageResult, ImageSpec, InputFailure,
    InputReport, InputSource, JobFailure, JobId, JobRecord, JobRepository, JobState, JobStoreError,
    MediaRef, ObjectBody, ObjectRef, ObjectStore, ObjectStoreError, OpKind, OpResult, OpSpec,
    OutputKey, PackageResult, PackageSpec, ProbeResult, ProbeSpec, ProgressPercent,
    ResizeMode as DomainResizeMode, ThumbnailResult, ThumbnailSpec, TimedOutput, TranscodeResult,
    TranscodeSpec, TrimSpec, WorkerId, resolve_media_ref, validate_inputs,
};
use mm_ffmpeg::{
    ImageFormat, MediaInfo, RunLimits, RunOutcome, ScaleMode, SourceCut, StillInfo,
    ThumbnailTarget, TranscodeTarget, TrimStrategy, TrimTarget, build_concat_args,
    build_thumbnail_args, build_transcode_args, build_trim_args,
};
use mm_gst::Written as GstWritten;
use mm_gst::{
    Element as GstElement, Link as GstLink, PackageOutcome as GstOutcome,
    PackageRequest as GstRequest, PipelineDescription as GstDescription, Property as GstProperty,
};
use mm_magick::{
    ImageFormat as ToolImageFormat, ResizeMode as ToolResizeMode, ToolError as ImageToolError,
    composite_args, convert_args, resize_args,
};
use mm_observability::{Component, record};
use prost::Message;
use tokio::io::AsyncReadExt;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::WorkerConfig;
use crate::report::{
    blocks, input_report, render_verdicts, still_report, target_profile, warnings,
};

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
    pub const IMAGE_LAUNCH_FAILED: &str = "image_launch_failed";
    pub const IMAGE_FAILED: &str = "image_failed";
    pub const PACKAGE_LAUNCH_FAILED: &str = "package_launch_failed";
    pub const PACKAGE_FAILED: &str = "package_failed";
}

const UPLOAD_CHUNK_BYTES: usize = 1024 * 1024;
const THUMBNAIL_RUN_SECONDS: u32 = 5;
const IMAGE_RUN_SECONDS: std::time::Duration = std::time::Duration::from_secs(120);
const PACKAGE_RUN_SECONDS: std::time::Duration = std::time::Duration::from_secs(600);
const SEGMENT_ESTIMATE_BYTES: u64 = 4 * 1024 * 1024;

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

async fn estimate_bytes(ctx: &WorkerConfig, inputs: &[InputSource], infos: &[MediaInfo]) -> u64 {
    let mut total = 0u64;
    for (input, info) in inputs.iter().zip(infos) {
        let size = match info.size_bytes {
            Some(size) => size,
            None => match input.media().map(|source| source.media()) {
                Some(MediaRef::Object(object)) => ctx.store.head(object).await.unwrap_or(0),
                Some(MediaRef::Http(_)) | None => 0,
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
    let trace = job.trace_id;
    ctx.metrics.jobs_started.add(1);
    ctx.metrics
        .claim_latency
        .observe(seconds_since(job.created_at));
    record(
        Component::Worker,
        trace.as_ref(),
        "job.claimed",
        &[
            ("job", job.id.to_string()),
            ("worker", ctx.worker_id.as_str().to_string()),
            (
                "claim_latency_seconds",
                format!("{:.3}", seconds_since(job.created_at)),
            ),
        ],
    );
    let outcome = run_job(ctx, &job, registry).await;
    ctx.metrics.jobs_finished.add(1);
    ctx.metrics
        .job_duration
        .observe(seconds_since(job.created_at));
    let state = match &outcome {
        Some(JobState::Succeeded) => "SUCCEEDED",
        Some(JobState::Failed) => "FAILED",
        Some(JobState::Cancelled) => "CANCELLED",
        _ => "RELEASED",
    };
    record(
        Component::Worker,
        trace.as_ref(),
        "job.finished",
        &[
            ("job", job.id.to_string()),
            ("state", state.to_string()),
            (
                "duration_seconds",
                format!("{:.3}", seconds_since(job.created_at)),
            ),
        ],
    );
}

fn seconds_since(at: SystemTime) -> f64 {
    SystemTime::now()
        .duration_since(at)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or_default()
}

async fn run_job(
    ctx: &Arc<WorkerConfig>,
    job: &JobRecord,
    registry: &CancelRegistry,
) -> Option<JobState> {
    if job.cancel_requested {
        if let Ok(record) = ctx.repository.mark_cancelled(job.id).await {
            publish_outcome(ctx, &record).await;
            return Some(record.state);
        }
        return None;
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
            return Some(record.state);
        }
        return None;
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
    let outcome = pipeline(ctx, job, deadline_at, &mut signal).await;
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
                return Some(record.state);
            }
            None
        }
        Err(Finish::Cancelled) => {
            if let Ok(record) = ctx.repository.mark_cancelled(job.id).await {
                publish_outcome(ctx, &record).await;
                return Some(record.state);
            }
            None
        }
        Err(Finish::LeaseLost) | Err(Finish::Released) => None,
        Err(Finish::TimedOut) => {
            if let Ok(record) = ctx
                .repository
                .mark_failed(job.id, &ctx.worker_id, timeout_failure())
                .await
            {
                publish_outcome(ctx, &record).await;
                return Some(record.state);
            }
            None
        }
        Err(Finish::Failed(failure)) => {
            if crate::retry::is_transient(failure.code()) && job.attempts < ctx.retry.max_attempts {
                let delay = ctx.retry.delay(job.attempts);
                let _ = ctx
                    .repository
                    .retry(job.id, &ctx.worker_id, failure, delay)
                    .await;
                None
            } else if let Ok(record) = ctx
                .repository
                .mark_failed(job.id, &ctx.worker_id, failure)
                .await
            {
                publish_outcome(ctx, &record).await;
                Some(record.state)
            } else {
                None
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
        OpSpec::Trim(spec) => trim_pipeline(ctx, job, spec, deadline_at, signal).await,
        OpSpec::Thumbnail(spec) => thumbnail_pipeline(ctx, job, spec, deadline_at, signal).await,
        OpSpec::Image(spec) => image_pipeline(ctx, job, spec, deadline_at, signal).await,
        OpSpec::Package(spec) => package_pipeline(ctx, job, spec, deadline_at, signal).await,
        OpSpec::Batch(_) => Err(failed(
            codes::UNSUPPORTED_OPERATION,
            "batch execution is not implemented",
        )),
    }
}

async fn package_pipeline(
    ctx: &Arc<WorkerConfig>,
    job: &JobRecord,
    spec: &PackageSpec,
    deadline_at: SystemTime,
    signal: &mut watch::Receiver<Signal>,
) -> Result<OpResult, Finish> {
    check(signal)?;
    let url = resolve_input(ctx, spec.input(), deadline_at, 0).await?;
    check(signal)?;
    let info = probe_input(ctx, &url, 0).await?;
    gate_verdicts(job.id, OpKind::Package, &[input_report(&info)])?;

    let segments_expected = (info.duration.as_secs_f64() / spec.segment_seconds())
        .ceil()
        .max(1.0) as u64;
    let inputs = [spec.input().clone()];
    let infos = [info];
    let estimate =
        estimate_bytes(ctx, &inputs, &infos).await + segments_expected * SEGMENT_ESTIMATE_BYTES;
    let _reservation = admit_or_release(ctx, job.id, estimate).await?;
    let _ = remaining_limits(ctx, PACKAGE_RUN_SECONDS, deadline_at)?;

    let staging = ctx.staging_dir.join(job.id.to_string());
    std::fs::create_dir_all(&staging).map_err(|error| failed(codes::MERGE_LAUNCH_FAILED, error))?;
    let request = GstRequest::new(
        url.clone(),
        staging.clone(),
        staging.join("stream"),
        spec.segment_seconds(),
    );
    let description = packaging_description(&request);
    let seconds = spec.segment_seconds();

    check(signal)?;
    let produced = tokio::task::spawn_blocking(move || {
        mm_gst::run_package_pipeline(&description, &request, PACKAGE_RUN_SECONDS)
    })
    .await
    .map_err(|error| failed(codes::PACKAGE_LAUNCH_FAILED, error))
    .and_then(|outcome| outcome.map_err(|error| failed(codes::PACKAGE_FAILED, error)))?;

    let uploaded = upload_package(ctx, job.id, &produced, seconds).await;
    let _ = std::fs::remove_dir_all(&staging);
    uploaded
}

fn packaging_description(request: &GstRequest) -> GstDescription {
    GstDescription::new(
        vec![
            GstElement::new("souphttpsrc", "source"),
            GstElement::new("qtdemux", "demux"),
            GstElement::new("mp4mux", "mux")
                .with("streamable", GstProperty::Flag(true))
                .milliseconds("fragment-duration", request.fragment_milliseconds()),
            GstElement::new("filesink", "sink").text("location", request.stream_location()),
        ],
        vec![
            GstLink::direct("source", "demux"),
            GstLink::direct("demux", "mux"),
            GstLink::direct("mux", "sink"),
        ],
    )
    .expect("the packaging description is valid")
}

async fn upload_package(
    ctx: &Arc<WorkerConfig>,
    id: JobId,
    produced: &GstOutcome,
    segment_seconds: f64,
) -> Result<OpResult, Finish> {
    let bytes = tokio::fs::read(&produced.stream)
        .await
        .map_err(|error| failed(codes::OUTPUT_INVALID, error))?;
    let segmented =
        mm_gst::split_fragments(&bytes).map_err(|error| failed(codes::OUTPUT_INVALID, error))?;
    if segmented.segments.is_empty() {
        return Err(failed(
            codes::OUTPUT_INVALID,
            "the packaging produced no segment",
        ));
    }

    let dir = produced
        .stream
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let written = segmented
        .write_to(&dir)
        .map_err(|error| failed(codes::OUTPUT_INVALID, error))?;
    let playlist = mm_gst::render_playlist(Some("init.mp4"), segment_seconds, &segmented.names(0));
    let playlist_path = dir.join(mm_gst::playlist_name());
    std::fs::write(&playlist_path, playlist)
        .map_err(|error| failed(codes::OUTPUT_INVALID, error))?;

    let mut landed = Landed {
        store: ctx.store.as_ref(),
        keys: Vec::new(),
    };
    let outcome = upload_set(ctx, id, &written, &playlist_path, &mut landed).await;
    let staged: Vec<PathBuf> = std::iter::once(written.init.clone())
        .chain(written.segments.iter().cloned())
        .chain(std::iter::once(playlist_path.clone()))
        .collect();
    for path in staged {
        let _ = tokio::fs::remove_file(path).await;
    }
    match outcome {
        Ok(result) => Ok(result),
        Err(finish) => {
            landed.discard().await;
            Err(finish)
        }
    }
}

async fn upload_set(
    ctx: &Arc<WorkerConfig>,
    id: JobId,
    written: &GstWritten,
    playlist_path: &Path,
    landed: &mut Landed<'_>,
) -> Result<OpResult, Finish> {
    let init = artifact_object_named(ctx, id, "init.mp4").await?;
    let playlist = artifact_object_named(ctx, id, mm_gst::playlist_name()).await?;
    let mut segments = Vec::with_capacity(written.segments.len());
    upload_object(ctx, &init, &written.init).await?;
    landed.record(init.clone());
    upload_object(ctx, &playlist, playlist_path).await?;
    landed.record(playlist.clone());
    for path in &written.segments {
        let key = artifact_object_named(ctx, id, &segment_name(path)).await?;
        upload_object(ctx, &key, path).await?;
        landed.record(key.clone());
        segments.push(MediaRef::Object(key));
    }
    let result = PackageResult::new(MediaRef::Object(playlist), segments)
        .map_err(|error| failed(codes::OUTPUT_INVALID, error))?;
    Ok(OpResult::Package(result))
}

fn segment_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "segment.m4s".to_string())
}

async fn artifact_object_named(
    ctx: &WorkerConfig,
    id: JobId,
    artifact: &str,
) -> Result<ObjectRef, Finish> {
    OutputKey::new(&id, artifact)
        .map_err(|error| failed(codes::OUTPUT_INVALID, error))?
        .into_object(&ctx.bucket)
        .map_err(|error| failed(codes::OUTPUT_INVALID, error))
}

async fn upload_object(
    ctx: &Arc<WorkerConfig>,
    object: &ObjectRef,
    path: &Path,
) -> Result<(), Finish> {
    let bytes = file_bytes(path).await;
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|error| failed(codes::OUTPUT_UPLOAD_FAILED, error))?;
    ctx.store
        .put_multipart(object, file_body(file))
        .await
        .map_err(|error| failed(codes::OUTPUT_UPLOAD_FAILED, error))?;
    ctx.metrics.output_bytes.add(bytes);
    Ok(())
}

async fn image_pipeline(
    ctx: &Arc<WorkerConfig>,
    job: &JobRecord,
    spec: &ImageSpec,
    deadline_at: SystemTime,
    signal: &mut watch::Receiver<Signal>,
) -> Result<OpResult, Finish> {
    let mut urls = Vec::with_capacity(spec.inputs().len());
    for (index, input) in spec.inputs().iter().enumerate() {
        check(signal)?;
        urls.push(resolve_input(ctx, input, deadline_at, index).await?);
    }

    let mut stills = Vec::with_capacity(urls.len());
    for (index, url) in urls.iter().enumerate() {
        check(signal)?;
        stills.push(probe_still(ctx, url, index).await?);
    }
    let reports: Vec<_> = stills.iter().map(still_report).collect();
    gate_verdicts(job.id, spec.op().kind(), &reports)?;

    let expected = image_frame_expectation(spec, &stills);
    let inputs_bytes: u64 = stills
        .iter()
        .map(|still| still.size_bytes.unwrap_or_default())
        .sum();
    let (est_w, est_h) = image_estimate_frame(spec, &stills);
    let estimate = inputs_bytes + staged_frame_bytes(1, est_w, est_h);
    let _reservation = admit_or_release(ctx, job.id, estimate).await?;

    let staged = ctx
        .staging_dir
        .join(format!("{}.{}", job.id, spec.format().extension()));
    let args = image_args(spec, &urls, &staged.to_string_lossy());
    let _ = remaining_limits(ctx, IMAGE_RUN_SECONDS, deadline_at)?;

    check(signal)?;
    let produced = run_image(ctx, job.id, &args, &staged, expected, spec.format(), signal).await;
    let _ = tokio::fs::remove_file(&staged).await;
    let outputs = produced?;
    let result = ImageResult::new(outputs).map_err(|error| failed(codes::OUTPUT_INVALID, error))?;
    Ok(OpResult::Image(result))
}

async fn run_image(
    ctx: &Arc<WorkerConfig>,
    id: JobId,
    args: &[String],
    staged: &Path,
    expected: FrameExpectation,
    format: DomainImageFormat,
    signal: &mut watch::Receiver<Signal>,
) -> Result<Vec<MediaRef>, Finish> {
    ctx.image_tool
        .run(args, &ctx.image_limits)
        .await
        .map_err(|error| match error {
            ImageToolError::Failed { stderr_tail, .. } => failed(codes::IMAGE_FAILED, stderr_tail),
            other => failed(codes::IMAGE_LAUNCH_FAILED, other),
        })?;
    check(signal)?;

    let info = ctx
        .image_tool
        .identify(&staged.to_string_lossy(), &ctx.image_limits)
        .await
        .map_err(|error| failed(codes::OUTPUT_INVALID, error))?;
    if !expected.holds(info.width, info.height) {
        return Err(failed(
            codes::OUTPUT_INVALID,
            format!(
                "the produced frame is {}x{}, not {}",
                info.width,
                info.height,
                expected.describe()
            ),
        ));
    }

    let mut landed = Landed {
        store: ctx.store.as_ref(),
        keys: Vec::new(),
    };
    match upload_image(ctx, id, staged, format, &mut landed, signal).await {
        Ok(outputs) => Ok(outputs),
        Err(finish) => {
            landed.discard().await;
            Err(finish)
        }
    }
}

async fn upload_image(
    ctx: &Arc<WorkerConfig>,
    id: JobId,
    staged: &Path,
    format: DomainImageFormat,
    landed: &mut Landed<'_>,
    signal: &mut watch::Receiver<Signal>,
) -> Result<Vec<MediaRef>, Finish> {
    let artifact = format!("image.{}", format.extension());
    let object = OutputKey::new(&id, &artifact)
        .map_err(|error| failed(codes::OUTPUT_INVALID, error))?
        .into_object(&ctx.bucket)
        .map_err(|error| failed(codes::OUTPUT_INVALID, error))?;
    let file = tokio::fs::File::open(staged)
        .await
        .map_err(|error| failed(codes::OUTPUT_UPLOAD_FAILED, error))?;
    ctx.store
        .put_multipart(&object, file_body(file))
        .await
        .map_err(|error| failed(codes::OUTPUT_UPLOAD_FAILED, error))?;
    landed.record(object.clone());
    check(signal)?;
    Ok(vec![MediaRef::Object(object)])
}

fn image_format(format: DomainImageFormat) -> ToolImageFormat {
    match format {
        DomainImageFormat::Jpeg => ToolImageFormat::Jpeg,
        DomainImageFormat::Png => ToolImageFormat::Png,
    }
}

fn resize_mode(mode: DomainResizeMode) -> ToolResizeMode {
    match mode {
        DomainResizeMode::Fit => ToolResizeMode::Fit,
        DomainResizeMode::Exact => ToolResizeMode::Exact,
        DomainResizeMode::Fill => ToolResizeMode::Fill,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FrameExpectation {
    exact: Option<(u32, u32)>,
    bounded: Option<(u32, u32)>,
    origin: &'static str,
}

impl FrameExpectation {
    fn holds(self, width: u32, height: u32) -> bool {
        if width == 0 || height == 0 {
            return false;
        }
        match (self.exact, self.bounded) {
            (Some((wanted_w, wanted_h)), _) => (width, height) == (wanted_w, wanted_h),
            (None, Some((max_w, max_h))) => width <= max_w && height <= max_h,
            (None, None) => true,
        }
    }

    fn describe(self) -> String {
        match (self.exact, self.bounded) {
            (Some((width, height)), _) => format!("{width}x{height}"),
            (None, Some((max_w, max_h))) => format!("at most {max_w}x{max_h}"),
            (None, None) => self.origin.to_string(),
        }
    }
}

fn image_frame_expectation(spec: &ImageSpec, stills: &[StillInfo]) -> FrameExpectation {
    match spec.op() {
        ImageOp::Resize => match spec.mode() {
            DomainResizeMode::Fit => FrameExpectation {
                exact: None,
                bounded: Some((spec.width(), spec.height())),
                origin: "a frame inside the requested box",
            },
            DomainResizeMode::Exact | DomainResizeMode::Fill => FrameExpectation {
                exact: Some((spec.width(), spec.height())),
                bounded: None,
                origin: "the requested frame",
            },
        },
        ImageOp::Convert => FrameExpectation {
            exact: Some(frame_of(stills, 0)),
            bounded: None,
            origin: "the source frame",
        },
        ImageOp::Composite => FrameExpectation {
            exact: Some(frame_of(stills, 1)),
            bounded: None,
            origin: "the base frame",
        },
    }
}

fn frame_of(stills: &[StillInfo], index: usize) -> (u32, u32) {
    stills
        .get(index)
        .map(|still| (still.width, still.height))
        .unwrap_or_default()
}

fn image_estimate_frame(spec: &ImageSpec, stills: &[StillInfo]) -> (u32, u32) {
    match spec.op() {
        ImageOp::Resize => (spec.width().max(1), spec.height().max(1)),
        ImageOp::Convert => frame_of(stills, 0),
        ImageOp::Composite => frame_of(stills, 1),
    }
}

fn image_args(spec: &ImageSpec, urls: &[String], output: &str) -> Vec<String> {
    let format = image_format(spec.format());
    match spec.op() {
        ImageOp::Resize => resize_args(
            &urls[0],
            spec.width(),
            spec.height(),
            resize_mode(spec.mode()),
            format,
            output,
        ),
        ImageOp::Convert => convert_args(&urls[0], format, output),
        ImageOp::Composite => composite_args(&urls[0], &urls[1], format, output),
    }
}

async fn resolve_input(
    ctx: &WorkerConfig,
    input: &InputSource,
    deadline_at: SystemTime,
    index: usize,
) -> Result<String, Finish> {
    let media = input.media().map(|source| source.media()).ok_or_else(|| {
        failed(
            codes::UNSUPPORTED_OPERATION,
            "a node reference reached the worker",
        )
    })?;
    resolve_media_ref(ctx.store.as_ref(), media, deadline_at, SystemTime::now())
        .await
        .map_err(|error| failed(codes::INPUT_UNRESOLVABLE, format!("input {index}: {error}")))
}

async fn probe_input(ctx: &WorkerConfig, url: &str, index: usize) -> Result<MediaInfo, Finish> {
    ctx.prober
        .probe(url, &ctx.probe_limits)
        .await
        .map_err(|error| failed(codes::PROBE_FAILED, format!("input {index}: {error}")))
}

async fn probe_still(ctx: &WorkerConfig, url: &str, index: usize) -> Result<StillInfo, Finish> {
    ctx.prober
        .still(url, &ctx.probe_limits)
        .await
        .map_err(|error| failed(codes::PROBE_FAILED, format!("input {index}: {error}")))
}

fn gate_verdicts(id: JobId, kind: OpKind, reports: &[InputReport]) -> Result<(), Finish> {
    let verdicts = validate_inputs(kind, reports);
    let blocking = blocks(&verdicts);
    if !blocking.is_empty() {
        let failure = JobFailure::new(codes::VALIDATION_BLOCKED, &render_verdicts(&blocking))
            .expect("worker failure codes are valid")
            .with_inputs(blocking.iter().map(InputFailure::from).collect());
        return Err(Finish::Failed(failure));
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

    let source = spec.input().media();
    let target = TranscodeTarget {
        scale_mode: ScaleMode::PadToFit,
        profile: target_profile(Some(spec.profile())),
        cut: source.and_then(|source| {
            source.trim().map(|trim| SourceCut {
                start_seconds: trim.start_seconds(),
                duration_seconds: trim.duration_seconds(),
            })
        }),
        speed: source.and_then(|source| source.speed()),
        volume: source.and_then(|source| source.volume()),
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

async fn thumbnail_pipeline(
    ctx: &Arc<WorkerConfig>,
    job: &JobRecord,
    spec: &ThumbnailSpec,
    deadline_at: SystemTime,
    signal: &mut watch::Receiver<Signal>,
) -> Result<OpResult, Finish> {
    check(signal)?;
    let url = resolve_input(ctx, spec.input(), deadline_at, 0).await?;
    check(signal)?;
    let info = probe_input(ctx, &url, 0).await?;
    gate_verdicts(job.id, OpKind::Thumbnail, &[input_report(&info)])?;

    let offsets = spec.offsets_seconds();
    let estimate = estimate_bytes(ctx, &[spec.input().clone()], &[info]).await
        + staged_frame_bytes(offsets.len(), spec.width(), spec.height());
    let _reservation = admit_or_release(ctx, job.id, estimate).await?;

    let target = ThumbnailTarget {
        width: spec.width(),
        height: spec.height(),
        format: ImageFormat::Jpeg,
    };
    let staged: Vec<PathBuf> = (0..offsets.len())
        .map(|index| ctx.staging_dir.join(format!("{}-{index}.jpg", job.id)))
        .collect();
    let runs: Vec<Vec<String>> = offsets
        .iter()
        .zip(staged.iter())
        .map(|(offset, path)| build_thumbnail_args(&url, *offset, &target, &path.to_string_lossy()))
        .collect();
    let total = Duration::from_secs(u64::from(THUMBNAIL_RUN_SECONDS) * offsets.len() as u64);
    let limits = remaining_limits(ctx, total, deadline_at)?;

    check(signal)?;
    let produced = run_all_and_upload(
        ctx,
        job.id,
        &runs,
        &limits,
        &staged,
        (spec.width(), spec.height()),
        signal,
    )
    .await;
    for path in &staged {
        let _ = tokio::fs::remove_file(path).await;
    }
    let outputs = produced?;
    let result =
        ThumbnailResult::new(outputs).map_err(|error| failed(codes::OUTPUT_INVALID, error))?;
    Ok(OpResult::Thumbnail(result))
}

fn staged_frame_bytes(count: usize, width: u32, height: u32) -> u64 {
    u64::from(width)
        .saturating_mul(u64::from(height))
        .saturating_mul(3)
        .saturating_mul(count as u64)
}

struct Landed<'a> {
    store: &'a dyn ObjectStore,
    keys: Vec<ObjectRef>,
}

impl Landed<'_> {
    fn record(&mut self, key: ObjectRef) {
        self.keys.push(key);
    }

    async fn discard(&self) {
        for key in &self.keys {
            let _ = self.store.delete(key).await;
        }
    }
}

async fn run_all_and_upload(
    ctx: &Arc<WorkerConfig>,
    id: JobId,
    runs: &[Vec<String>],
    limits: &RunLimits,
    staged: &[PathBuf],
    expected: (u32, u32),
    signal: &mut watch::Receiver<Signal>,
) -> Result<Vec<MediaRef>, Finish> {
    let total = runs.len();
    for (index, args) in runs.iter().enumerate() {
        check(signal)?;
        supervise(ctx, id, args, limits, signal, index, total).await?;
    }

    let mut landed = Landed {
        store: ctx.store.as_ref(),
        keys: Vec::with_capacity(total),
    };
    let outcome = upload_each(ctx, id, staged, expected, &mut landed, signal).await;
    match outcome {
        Ok(outputs) => Ok(outputs),
        Err(finish) => {
            landed.discard().await;
            Err(finish)
        }
    }
}

async fn upload_each(
    ctx: &Arc<WorkerConfig>,
    id: JobId,
    staged: &[PathBuf],
    expected: (u32, u32),
    landed: &mut Landed<'_>,
    signal: &mut watch::Receiver<Signal>,
) -> Result<Vec<MediaRef>, Finish> {
    let mut outputs = Vec::with_capacity(staged.len());
    for (index, path) in staged.iter().enumerate() {
        check(signal)?;
        let still = ctx
            .prober
            .still(&path.to_string_lossy(), &ctx.probe_limits)
            .await
            .map_err(|error| failed(codes::OUTPUT_INVALID, format!("output {index}: {error}")))?;
        if (still.width, still.height) != expected {
            return Err(failed(
                codes::OUTPUT_INVALID,
                format!(
                    "output {index}: {}x{} instead of the requested {}x{}",
                    still.width, still.height, expected.0, expected.1
                ),
            ));
        }
        let object = artifact_object(ctx, id, index)?;
        let bytes = file_bytes(path).await;
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|error| failed(codes::OUTPUT_UPLOAD_FAILED, error))?;
        ctx.store
            .put_multipart(&object, file_body(file))
            .await
            .map_err(|error| failed(codes::OUTPUT_UPLOAD_FAILED, error))?;
        landed.record(object.clone());
        outputs.push(MediaRef::Object(object));
        ctx.metrics.output_bytes.add(bytes);
    }
    check(signal)?;
    Ok(outputs)
}

fn artifact_object(ctx: &WorkerConfig, id: JobId, index: usize) -> Result<ObjectRef, Finish> {
    let artifact = format!("thumb-{index}.jpg");
    OutputKey::new(&id, &artifact)
        .map_err(|error| failed(codes::OUTPUT_INVALID, error))?
        .into_object(&ctx.bucket)
        .map_err(|error| failed(codes::OUTPUT_INVALID, error))
}

async fn trim_pipeline(
    ctx: &Arc<WorkerConfig>,
    job: &JobRecord,
    spec: &TrimSpec,
    deadline_at: SystemTime,
    signal: &mut watch::Receiver<Signal>,
) -> Result<OpResult, Finish> {
    check(signal)?;
    let url = resolve_input(ctx, spec.input(), deadline_at, 0).await?;
    check(signal)?;
    let info = probe_input(ctx, &url, 0).await?;
    gate_verdicts(job.id, OpKind::Trim, &[input_report(&info)])?;

    let inputs = [spec.input().clone()];
    let infos = [info];
    let estimate = estimate_bytes(ctx, &inputs, &infos).await;
    let _reservation = admit_or_release(ctx, job.id, estimate).await?;

    let strategy = trim_strategy(ctx, &url, spec.start_seconds()).await?;
    let target = TrimTarget {
        strategy,
        ..TrimTarget::default()
    };
    let output_path = ctx.staging_dir.join(format!("{}.mp4", job.id));
    let args = build_trim_args(
        &url,
        &target,
        spec.start_seconds(),
        spec.duration_seconds(),
        &output_path.to_string_lossy(),
    )
    .map_err(|error| failed(codes::MERGE_LAUNCH_FAILED, error))?;
    let wanted = Duration::from_secs_f64(spec.duration_seconds());
    let limits = remaining_limits(ctx, wanted, deadline_at)?;

    check(signal)?;
    let trimmed = run_and_upload(
        ctx,
        job.id,
        &args,
        &limits,
        &output_path,
        signal,
        OutputShape::Trim,
    )
    .await;
    let _ = tokio::fs::remove_file(&output_path).await;
    trimmed
}

async fn trim_strategy(
    ctx: &WorkerConfig,
    url: &str,
    start_seconds: f64,
) -> Result<TrimStrategy, Finish> {
    let keyframes = ctx
        .prober
        .keyframes(url, &ctx.probe_limits)
        .await
        .map_err(|error| failed(codes::PROBE_FAILED, format!("keyframes: {error}")))?;
    Ok(
        if mm_ffmpeg::aligned_to_keyframe(&keyframes, start_seconds) {
            TrimStrategy::Copy
        } else {
            TrimStrategy::Reencode
        },
    )
}

enum OutputShape {
    Concat,
    Transcode,
    Trim,
}

async fn supervise(
    ctx: &Arc<WorkerConfig>,
    id: JobId,
    args: &[String],
    limits: &RunLimits,
    signal: &mut watch::Receiver<Signal>,
    completed: usize,
    total: usize,
) -> Result<(), Finish> {
    let started = Instant::now();
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
                            let percent = shared_percent(update.percent, completed, total);
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

    ctx.metrics
        .tool_run
        .observe(started.elapsed().as_secs_f64());
    record(
        Component::Worker,
        None,
        "tool.finished",
        &[
            ("job", id.to_string()),
            (
                "wall_seconds",
                format!("{:.3}", started.elapsed().as_secs_f64()),
            ),
        ],
    );
    match outcome {
        RunOutcome::Succeeded => Ok(()),
        RunOutcome::Cancelled => Err(interrupted.unwrap_or(Finish::Cancelled)),
        RunOutcome::DeadlineExceeded { after } => Err(failed(
            codes::TIMEOUT,
            format!("merge exceeded the deadline after {after:?}"),
        )),
        RunOutcome::Failed {
            status,
            stderr_tail,
        } => Err(failed(
            codes::MERGE_FAILED,
            format!("exit status {status:?}: {stderr_tail}"),
        )),
    }
}

fn shared_percent(run_percent: f64, completed: usize, total: usize) -> ProgressPercent {
    let share = completed as f64 + run_percent / 100.0;
    let overall = if total == 0 {
        0.0
    } else {
        (share * 100.0 / total as f64).clamp(0.0, 100.0)
    };
    ProgressPercent::new(overall as u8).unwrap_or(ProgressPercent::COMPLETE)
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
    let started = Instant::now();
    supervise(ctx, id, args, limits, signal, 0, 1).await?;
    let wall_seconds = started.elapsed().as_secs_f64();

    check(signal)?;
    let info = ctx
        .prober
        .probe(&output_path.to_string_lossy(), &ctx.probe_limits)
        .await
        .map_err(|error| failed(codes::OUTPUT_INVALID, error))?;
    ctx.metrics
        .tool_speed
        .observe(media_seconds(info.duration.as_secs_f64(), wall_seconds));
    let object = output_object(ctx, id).map_err(|error| failed(codes::OUTPUT_INVALID, error))?;
    let bytes = file_bytes(output_path).await;
    let file = tokio::fs::File::open(output_path)
        .await
        .map_err(|error| failed(codes::OUTPUT_UPLOAD_FAILED, error))?;
    ctx.store
        .put_multipart(&object, file_body(file))
        .await
        .map_err(|error| failed(codes::OUTPUT_UPLOAD_FAILED, error))?;
    ctx.metrics.output_bytes.add(bytes);
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
        OutputShape::Trim => {
            let timed = TimedOutput::new(MediaRef::Object(object), info.duration.as_secs_f64())
                .map_err(|error| failed(codes::OUTPUT_INVALID, error))?;
            Ok(OpResult::Trim(timed))
        }
    }
}

fn media_seconds(duration: f64, wall_seconds: f64) -> f64 {
    if wall_seconds <= 0.0 {
        return 0.0;
    }
    (duration / wall_seconds).max(0.0)
}

async fn file_bytes(path: &Path) -> u64 {
    tokio::fs::metadata(path)
        .await
        .map(|meta| meta.len())
        .unwrap_or_default()
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

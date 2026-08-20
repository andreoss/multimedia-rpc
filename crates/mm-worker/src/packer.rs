use std::path::{Path, PathBuf};

use async_trait::async_trait;
use thiserror::Error;

use mm_domain::{JobId, Ticks};
use mm_ffmpeg::{
    HlsTarget, RunLimits, RunOutcome, Runner, TargetProfile, Timestamp, build_hls_args,
};
use mm_gst::{
    Element as GstElement, Link as GstLink, PackageOutcome as GstOutcome,
    PackageRequest as GstRequest, PipelineDescription as GstDescription, Property as GstProperty,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LadderPacked {
    pub init: PathBuf,
    pub playlist: PathBuf,
    pub segments: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LadderPackError {
    #[error("the packer could not launch: {0}")]
    Launch(String),
    #[error("the packer refused the rung: {0}")]
    Refused(String),
}

#[async_trait]
pub trait RungPacker: Send + Sync {
    fn name(&self) -> &'static str;

    async fn pack(
        &self,
        job: JobId,
        rung: usize,
        media: &mm_domain::ObjectRef,
        staging_dir: &Path,
        segment: Ticks,
    ) -> Result<LadderPacked, LadderPackError>;
}

#[derive(Debug, Default)]
pub struct FfmpegRungPacker;

#[async_trait]
impl RungPacker for FfmpegRungPacker {
    fn name(&self) -> &'static str {
        "ffmpeg"
    }

    async fn pack(
        &self,
        _job: JobId,
        rung: usize,
        _media: &mm_domain::ObjectRef,
        staging_dir: &Path,
        segment: Ticks,
    ) -> Result<LadderPacked, LadderPackError> {
        let dir = staging_dir.to_path_buf();
        std::fs::create_dir_all(&dir)
            .map_err(|error| LadderPackError::Launch(error.to_string()))?;
        let args = build_hls_args(
            &dir.join("media.mp4").to_string_lossy(),
            &HlsTarget {
                profile: TargetProfile::default(),
                segment: Timestamp::from_micros(segment.micros()),
                copy_streams: true,
            },
            &dir.to_string_lossy(),
        )
        .map_err(|error| LadderPackError::Launch(error.to_string()))?;
        let limits = RunLimits {
            total_duration: crate::pipeline::PACKAGE_RUN_SECONDS,
            deadline: crate::pipeline::PACKAGE_RUN_SECONDS,
            stderr_tail_bytes: 4096,
        };
        let runner = Runner::default();
        let run = runner
            .start(&args, &limits)
            .map_err(|error| LadderPackError::Launch(error.to_string()))?;
        match run
            .wait()
            .await
            .map_err(|error| LadderPackError::Launch(error.to_string()))?
        {
            RunOutcome::Succeeded => collect_hls_outputs(&dir, rung),
            other => Err(LadderPackError::Refused(format!("rung {rung}: {other:?}"))),
        }
    }
}

fn collect_hls_outputs(dir: &Path, rung: usize) -> Result<LadderPacked, LadderPackError> {
    let playlist = dir.join(mm_gst::playlist_name());
    if !playlist.is_file() {
        return Err(LadderPackError::Refused(format!(
            "rung {rung} wrote no playlist at {}",
            playlist.display()
        )));
    }
    let init = dir.join("init.mp4");
    let mut segments: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.extension()
                        .map(|extension| extension == "m4s")
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    segments.sort();
    if segments.is_empty() {
        return Err(LadderPackError::Refused(format!(
            "rung {rung} wrote no segment"
        )));
    }
    if !init.is_file() {
        return Err(LadderPackError::Refused(format!(
            "rung {rung} wrote no init segment at {}",
            init.display()
        )));
    }
    Ok(LadderPacked {
        init,
        playlist,
        segments,
    })
}

#[derive(Debug, Default)]
pub struct GstRungPacker;

#[async_trait]
impl RungPacker for GstRungPacker {
    fn name(&self) -> &'static str {
        "gst"
    }

    async fn pack(
        &self,
        _job: JobId,
        _rung: usize,
        _media: &mm_domain::ObjectRef,
        staging_dir: &Path,
        segment: Ticks,
    ) -> Result<LadderPacked, LadderPackError> {
        std::fs::create_dir_all(staging_dir)
            .map_err(|error| LadderPackError::Launch(error.to_string()))?;
        let dir = staging_dir.to_path_buf();
        let request = GstRequest::new(
            dir.join("media.mp4").to_string_lossy().into_owned(),
            dir.clone(),
            dir.join("stream"),
            segment.seconds(),
        );
        let description = rung_packaging_description(&request);
        let outcome = tokio::task::spawn_blocking(move || {
            mm_gst::run_package_pipeline(
                &description,
                &request,
                crate::pipeline::PACKAGE_RUN_SECONDS,
            )
        })
        .await
        .map_err(|error| LadderPackError::Launch(error.to_string()))?
        .map_err(|error| LadderPackError::Refused(error.to_string()))?;
        split_packaged(&outcome, &dir, segment)
    }
}

pub fn rung_packaging_description(request: &GstRequest) -> GstDescription {
    GstDescription::new(
        vec![
            GstElement::new("filesrc", "source"),
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

fn split_packaged(
    outcome: &GstOutcome,
    dir: &Path,
    segment: Ticks,
) -> Result<LadderPacked, LadderPackError> {
    let bytes = std::fs::read(&outcome.stream)
        .map_err(|error| LadderPackError::Refused(error.to_string()))?;
    let segmented = mm_gst::split_fragments(&bytes)
        .map_err(|error| LadderPackError::Refused(error.to_string()))?;
    if segmented.segments.is_empty() {
        return Err(LadderPackError::Refused(
            "the packaging produced no segment".to_string(),
        ));
    }
    let written = segmented
        .write_to(dir)
        .map_err(|error| LadderPackError::Refused(error.to_string()))?;
    let playlist =
        mm_gst::render_playlist(Some("init.mp4"), segment.seconds(), &segmented.names(0));
    let playlist_path = dir.join(mm_gst::playlist_name());
    std::fs::write(&playlist_path, playlist)
        .map_err(|error| LadderPackError::Refused(error.to_string()))?;
    Ok(LadderPacked {
        init: written.init,
        playlist: playlist_path,
        segments: written.segments,
    })
}

static DEFAULT_PACKER: GstRungPacker = GstRungPacker;

pub fn selected(ctx: &crate::WorkerConfig) -> &dyn RungPacker {
    match &ctx.ladder_packer {
        Some(packer) => packer.as_ref(),
        None => default_for(ctx),
    }
}

fn default_for(ctx: &crate::WorkerConfig) -> &dyn RungPacker {
    static FFMPEG: std::sync::OnceLock<FfmpegRungPacker> = std::sync::OnceLock::new();
    match ctx.ladder_packer_name.as_str() {
        "ffmpeg" => FFMPEG.get_or_init(FfmpegRungPacker::default),
        _ => &DEFAULT_PACKER,
    }
}

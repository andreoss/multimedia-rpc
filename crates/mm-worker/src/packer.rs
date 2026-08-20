use std::path::{Path, PathBuf};

use async_trait::async_trait;
use thiserror::Error;

use mm_domain::JobId;
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
        segment_seconds: f64,
    ) -> Result<LadderPacked, LadderPackError>;
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
        rung: usize,
        _media: &mm_domain::ObjectRef,
        staging_dir: &Path,
        segment_seconds: f64,
    ) -> Result<LadderPacked, LadderPackError> {
        let dir = staging_dir.join(format!("rung{rung}"));
        std::fs::create_dir_all(&dir)
            .map_err(|error| LadderPackError::Launch(error.to_string()))?;
        let source = format!("{}/media.mp4", dir.display());
        let request = GstRequest::new(source, dir.clone(), dir.join("stream"), segment_seconds);
        let description = packaging_description(&request);
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
        split_packaged(&outcome, &dir, segment_seconds)
    }
}

pub fn packaging_description(request: &GstRequest) -> GstDescription {
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

fn split_packaged(
    outcome: &GstOutcome,
    dir: &Path,
    segment_seconds: f64,
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
    let playlist = mm_gst::render_playlist(Some("init.mp4"), segment_seconds, &segmented.names(0));
    let playlist_path = dir.join(mm_gst::playlist_name());
    std::fs::write(&playlist_path, playlist)
        .map_err(|error| LadderPackError::Refused(error.to_string()))?;
    Ok(LadderPacked {
        init: written.init,
        playlist: playlist_path,
        segments: written.segments,
    })
}

pub fn packer_name(packer: Option<&dyn RungPacker>) -> &'static str {
    match packer {
        Some(packer) => packer.name(),
        None => GstRungPacker.name(),
    }
}
static DEFAULT_PACKER: GstRungPacker = GstRungPacker;

pub fn selected(ctx: &crate::WorkerConfig) -> &dyn RungPacker {
    match &ctx.ladder_packer {
        Some(packer) => packer.as_ref(),
        None => &DEFAULT_PACKER,
    }
}

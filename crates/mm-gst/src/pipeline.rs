use std::path::PathBuf;
use std::time::Duration;

use gstreamer::prelude::*;
use gstreamer::{Bus, ClockTime, Message as RawBusMessage, MessageView, Pipeline, State};

use crate::description::PipelineDescription;
use crate::package::PackageRequest;

const BUS_POLL: Duration = Duration::from_millis(50);

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error("the pipeline toolkit could not be initialised: {0}")]
    Init(String),
    #[error("the pipeline description was refused: {0}")]
    Description(String),
    #[error("the pipeline carried no bus")]
    NoBus,
    #[error("the pipeline reported: {0}")]
    Bus(String),
    #[error("the pipeline did not finish inside {0:?}; last bus message: {1}")]
    Deadline(Duration, String),
    #[error("the pipeline produced no stream at {0}")]
    NoStream(PathBuf),
}

pub struct PackageOutcome {
    pub stream: PathBuf,
}

pub fn run_package_pipeline(
    description: &PipelineDescription,
    request: &PackageRequest,
    deadline: Duration,
) -> Result<PackageOutcome, PipelineError> {
    gstreamer::init().map_err(|error| PipelineError::Init(error.to_string()))?;

    let rendered = description.launcher_description();
    let parsed = gstreamer::parse::bin_from_description(&rendered, false)
        .map_err(|error| PipelineError::Description(error.to_string()))?;
    let pipeline = Pipeline::with_name("mm-gst-package");
    pipeline
        .add(&parsed)
        .map_err(|error| PipelineError::Description(error.to_string()))?;
    bind_input(&parsed, &request.input);

    let _ = pipeline.set_state(State::Playing);
    let outcome = wait_for_eos(&pipeline, deadline);
    let _ = pipeline.set_state(State::Null);
    outcome?;

    let stream = PathBuf::from(request.stream_location());
    if !stream.is_file() {
        return Err(PipelineError::NoStream(stream));
    }
    Ok(PackageOutcome { stream })
}

fn wait_for_eos(pipeline: &Pipeline, deadline: Duration) -> Result<(), PipelineError> {
    let bus = pipeline.bus().ok_or(PipelineError::NoBus)?;
    let started = std::time::Instant::now();
    let mut last: Option<String> = None;
    loop {
        if started.elapsed() > deadline {
            return Err(PipelineError::Deadline(deadline, last.unwrap_or_default()));
        }
        match next_message(&bus) {
            Some(message) => match message.view() {
                MessageView::Eos(_) => return Ok(()),
                MessageView::Error(error) => {
                    return Err(PipelineError::Bus(error.error().to_string()));
                }
                other => last = Some(format!("{other:?}")),
            },
            None => std::thread::sleep(BUS_POLL),
        }
    }
}

fn next_message(bus: &Bus) -> Option<RawBusMessage> {
    bus.timed_pop(ClockTime::from_nseconds(BUS_POLL.as_nanos() as u64))
}

fn bind_input(bin: &gstreamer::Bin, input: &str) {
    if let Some(source) = bin.by_name("source") {
        source.set_property("location", input);
    }
}

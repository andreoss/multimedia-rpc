#![cfg(feature = "gst")]

use std::path::{Path, PathBuf};
use std::time::Duration;

use mm_gst::{
    Element, Link, PackageRequest, PipelineDescription, Property, render_playlist,
    run_package_pipeline, split_fragments,
};

const DEADLINE: Duration = Duration::from_secs(180);

fn scratch_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scratch/s15");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn keyed_fixture(name: &str, seconds: u32) -> PathBuf {
    let dir = scratch_dir().join("fixtures");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    if path.is_file() && path.metadata().map(|meta| meta.len()).unwrap_or(0) > 0 {
        return path;
    }
    let output = std::process::Command::new("ffmpeg")
        .arg("-y")
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg(format!("testsrc2=duration={seconds}:size=640x360:rate=30"))
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg(format!(
            "sine=frequency=440:sample_rate=44100:duration={seconds}"
        ))
        .arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("veryfast")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-g")
        .arg("30")
        .arg("-keyint_min")
        .arg("30")
        .arg("-sc_threshold")
        .arg("0")
        .arg("-c:a")
        .arg("aac")
        .arg("-movflags")
        .arg("+faststart")
        .arg(&path)
        .output()
        .expect("the media tool must be runnable to generate fixtures");
    assert!(output.status.success(), "fixture generation failed: {name}");
    path
}

fn packaging_description(request: &PackageRequest) -> PipelineDescription {
    PipelineDescription::new(
        vec![
            Element::new("filesrc", "source").text("location", request.input.clone()),
            Element::new("qtdemux", "demux"),
            Element::new("mp4mux", "mux")
                .with("streamable", Property::Flag(true))
                .milliseconds("fragment-duration", request.fragment_milliseconds()),
            Element::new("filesink", "sink").text("location", request.stream_location()),
        ],
        vec![
            Link::Direct {
                from: "source".to_string(),
                from_pad: None,
                to: "demux".to_string(),
                to_pad: None,
            },
            Link::Direct {
                from: "demux".to_string(),
                from_pad: None,
                to: "mux".to_string(),
                to_pad: None,
            },
            Link::Direct {
                from: "mux".to_string(),
                from_pad: None,
                to: "sink".to_string(),
                to_pad: None,
            },
        ],
    )
    .expect("the packaging description is valid")
}

fn request_for(input: &Path, work: &Path) -> PackageRequest {
    PackageRequest::new(
        input.to_string_lossy().into_owned(),
        work.to_path_buf(),
        work.join("seg"),
        1.0,
    )
}

#[test]
fn a_described_pipeline_writes_a_fragmented_stream() {
    let input = keyed_fixture("package-keyed.mp4", 4);
    let work = scratch_dir().join("run-package");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).unwrap();
    let request = request_for(&input, &work);

    let outcome = run_package_pipeline(&packaging_description(&request), &request, DEADLINE)
        .expect("packaging runs");

    assert!(
        outcome.stream.is_file(),
        "no fragmented stream at {:?}",
        outcome.stream
    );
    let bytes = std::fs::read(&outcome.stream).expect("the stream reads");
    assert!(!bytes.is_empty(), "the fragmented stream is empty");
}

#[test]
fn the_fragmented_stream_splits_into_a_playlist_and_its_segments() {
    let input = keyed_fixture("package-segments.mp4", 4);
    let work = scratch_dir().join("run-segments");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).unwrap();
    let request = request_for(&input, &work);

    let outcome = run_package_pipeline(&packaging_description(&request), &request, DEADLINE)
        .expect("packaging runs");
    let bytes = std::fs::read(&outcome.stream).expect("the stream reads");
    let segmented = split_fragments(&bytes).expect("the stream segments");

    assert!(
        segmented.segments.len() >= 2,
        "expected several segments, got {}",
        segmented.segments.len()
    );
    assert!(!segmented.init.is_empty(), "the init segment is empty");

    let names = segmented.names(1);
    let playlist = render_playlist(Some("init.mp4"), request.segment_seconds, &names);
    assert!(playlist.starts_with("#EXTM3U\n"));
    assert!(playlist.contains("#EXT-X-MAP"));
    assert_eq!(
        playlist.matches("#EXTINF:").count(),
        segmented.segments.len()
    );
    let written = segmented.write_to(&work).expect("the set writes");
    assert!(written.init.is_file());
    assert_eq!(written.segments.len(), segmented.segments.len());
}

#[test]
fn a_missing_input_fails_the_pipeline_with_a_typed_error() {
    let work = scratch_dir().join("run-missing");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).unwrap();
    let request = request_for(&work.join("absent.mp4"), &work);

    let outcome = run_package_pipeline(
        &packaging_description(&request),
        &request,
        Duration::from_secs(30),
    );

    assert!(
        outcome.is_err(),
        "a missing input must not package silently"
    );
}

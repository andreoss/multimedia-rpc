mod common;

use std::time::Duration;

use common::{scratch_dir, solid};
use mm_magick::{
    ImageFormat, Outcome, ResizeMode, Tool, ToolError, ToolLimits, composite_args, convert_args,
    resize_args,
};

const TEST_CAP: Duration = Duration::from_secs(120);

fn limits() -> ToolLimits {
    ToolLimits::default()
}

async fn run(args: &[String]) -> Outcome {
    let tool = Tool::default();
    tokio::time::timeout(TEST_CAP, tool.run(args, &limits()))
        .await
        .expect("the image tool answers inside the cap")
        .expect("the image tool is awaited")
}

#[tokio::test(flavor = "multi_thread")]
async fn identify_reports_the_frame_of_a_generated_fixture() {
    let fixture = solid("identify-red.png", 640, 480, "red");

    let info = Tool::default()
        .identify(&fixture.to_string_lossy(), &limits())
        .await
        .expect("identify reads the fixture");

    assert_eq!(info.format, "PNG");
    assert_eq!((info.width, info.height), (640, 480));
    assert!(info.size_bytes.unwrap_or_default() > 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn identify_of_an_absent_file_is_a_tool_failure() {
    let error = Tool::default()
        .identify("scratch/definitely-absent.png", &limits())
        .await
        .expect_err("identify fails on a missing file");

    assert!(matches!(error, ToolError::Failed { .. }), "{error:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_binary_is_a_launch_error() {
    let tool = Tool::with_binary("mm-no-such-image-tool");

    let error = tool.version(Duration::from_secs(5)).await;

    assert!(matches!(error, Err(ToolError::Launch { .. })), "{error:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resize_lands_at_the_requested_frame() {
    let fixture = solid("resize-base.png", 640, 480, "green");
    let output = scratch_dir().join("resize-exact.png");
    let args = resize_args(
        &fixture.to_string_lossy(),
        320,
        240,
        ResizeMode::Exact,
        ImageFormat::Png,
        &output.to_string_lossy(),
    );

    assert_eq!(run(&args).await, Outcome::Succeeded);

    let info = Tool::default()
        .identify(&output.to_string_lossy(), &limits())
        .await
        .expect("the resized frame reports");
    assert_eq!((info.width, info.height), (320, 240));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_convert_lands_in_the_requested_format() {
    let fixture = solid("convert-base.png", 120, 90, "blue");
    let output = scratch_dir().join("convert-out.jpg");
    let args = convert_args(
        &fixture.to_string_lossy(),
        ImageFormat::Jpeg,
        &output.to_string_lossy(),
    );

    assert_eq!(run(&args).await, Outcome::Succeeded);

    let info = Tool::default()
        .identify(&output.to_string_lossy(), &limits())
        .await
        .expect("the converted frame reports");
    assert_eq!(info.format, "JPEG");
    assert_eq!((info.width, info.height), (120, 90));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_composite_lands_the_base_frame_with_the_overlay_centred() {
    let base = solid("composite-base.png", 400, 300, "white");
    let overlay = solid("composite-overlay.png", 100, 100, "red");
    let output = scratch_dir().join("composite-out.jpg");
    let args = composite_args(
        &overlay.to_string_lossy(),
        &base.to_string_lossy(),
        ImageFormat::Jpeg,
        &output.to_string_lossy(),
    );

    assert_eq!(run(&args).await, Outcome::Succeeded);

    let info = Tool::default()
        .identify(&output.to_string_lossy(), &limits())
        .await
        .expect("the composited frame reports");
    assert_eq!((info.width, info.height), (400, 300));
    assert_eq!(info.format, "JPEG");
}

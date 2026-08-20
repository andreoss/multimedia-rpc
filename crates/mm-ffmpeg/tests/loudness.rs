mod common;

use std::time::Duration;

use common::{fixture, scratch_dir};
use mm_ffmpeg::{
    LoudnessError, LoudnessTarget, RunLimits, RunOutcome, Runner, build_loudness_measure_args,
    build_loudness_normalize_args, parse_loudnorm_report,
};

const REPORT: &str = r#"[Parsed_loudnorm_0 @ 0x1] 
{
	"input_i" : "-23.01",
	"input_tp" : "-3.52",
	"input_lra" : "7.10",
	"input_thresh" : "-33.10",
	"output_i" : "-16.00",
	"target_offset" : "0.20"
}
"#;

fn limits() -> RunLimits {
    RunLimits {
        total_duration: Duration::from_secs(30),
        deadline: Duration::from_secs(120),
        stderr_tail_bytes: 64 * 1024,
    }
}

#[test]
fn the_measurement_parser_reads_the_report_the_tool_prints() {
    let report = parse_loudnorm_report(REPORT).expect("the report parses");
    assert_eq!(report.integrated_lufs, -23.01);
    assert_eq!(report.true_peak_db, -3.52);
    assert_eq!(report.loudness_range_lu, 7.10);
    assert_eq!(report.threshold_lufs, -33.10);
    assert_eq!(report.target_offset_db, 0.20);
}

#[test]
fn the_measurement_parser_refuses_a_report_it_cannot_read_by_name() {
    assert_eq!(
        parse_loudnorm_report("no report here"),
        Err(LoudnessError::MissingReport)
    );
    let missing = REPORT.replace("\"input_tp\" : \"-3.52\",", "");
    assert_eq!(
        parse_loudnorm_report(&missing),
        Err(LoudnessError::MissingReport)
    );
    let malformed = REPORT.replace("\"-23.01\"", "\"loud\"");
    assert!(matches!(
        parse_loudnorm_report(&malformed),
        Err(LoudnessError::MalformedField {
            field: "input_i",
            ..
        })
    ));
    let not_finite = REPORT.replace("\"-23.01\"", "\"NaN\"");
    assert!(matches!(
        parse_loudnorm_report(&not_finite),
        Err(LoudnessError::NonFinite {
            field: "input_i",
            ..
        })
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_measurement_and_a_normalisation_reach_the_target_and_the_ceiling() {
    let input = fixture("loudness-live.mp4", 4, 24, 320, 180);
    let measured = measure(&input).await;
    assert!(measured.integrated_lufs < 0.0);

    let output = scratch_dir().join("loudness-normalized.mp4");
    let args = build_loudness_normalize_args(
        input.to_string_lossy().as_ref(),
        &measured,
        &LoudnessTarget {
            integrated_lufs: -16.0,
            true_peak_db: -1.5,
        },
        output.to_string_lossy().as_ref(),
    );
    let run = Runner::default()
        .start(&args, &limits())
        .expect("the normalizer starts");
    let (outcome, stderr) = run.wait_with_stderr().await.expect("the run is waited");
    assert_eq!(outcome, RunOutcome::Succeeded, "{stderr}");

    let normalized = measure(&output).await;
    assert!(
        (normalized.integrated_lufs + 16.0).abs() <= 1.0,
        "integrated loudness {} is outside one unit of -16",
        normalized.integrated_lufs
    );
    assert!(
        normalized.true_peak_db <= -1.5,
        "true peak {} is above the ceiling",
        normalized.true_peak_db
    );
}

async fn measure(path: &std::path::Path) -> mm_ffmpeg::LoudnessMeasurement {
    let args = build_loudness_measure_args(path.to_string_lossy().as_ref());
    let run = Runner::default()
        .start(&args, &limits())
        .expect("the meter starts");
    let (outcome, stderr) = run.wait_with_stderr().await.expect("the run is waited");
    assert_eq!(outcome, RunOutcome::Succeeded, "{stderr}");
    parse_loudnorm_report(&stderr).expect("the meter reports")
}

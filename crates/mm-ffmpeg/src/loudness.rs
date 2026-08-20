use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoudnessMeasurement {
    pub integrated_lufs: f64,
    pub true_peak_db: f64,
    pub loudness_range_lu: f64,
    pub threshold_lufs: f64,
    pub target_offset_db: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoudnessTarget {
    pub integrated_lufs: f64,
    pub true_peak_db: f64,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum LoudnessError {
    #[error("the tool reported no loudness measurement")]
    MissingReport,
    #[error("the loudness report carries no {field}")]
    MissingField { field: &'static str },
    #[error("the loudness field {field} is not a number: {value:?}")]
    MalformedField { field: &'static str, value: String },
    #[error("the loudness field {field} is not finite: {value}")]
    NonFinite { field: &'static str, value: f64 },
}

#[derive(Debug, Deserialize)]
struct Report {
    input_i: String,
    input_tp: String,
    input_lra: String,
    input_thresh: String,
    target_offset: String,
}

fn number(field: &'static str, value: &str) -> Result<f64, LoudnessError> {
    let parsed: f64 = value
        .trim()
        .parse()
        .map_err(|_| LoudnessError::MalformedField {
            field,
            value: value.to_string(),
        })?;
    if !parsed.is_finite() {
        return Err(LoudnessError::NonFinite {
            field,
            value: parsed,
        });
    }
    Ok(parsed)
}

pub fn parse_loudnorm_report(stderr: &str) -> Result<LoudnessMeasurement, LoudnessError> {
    let start = stderr.find('{').ok_or(LoudnessError::MissingReport)?;
    let end = stderr.rfind('}').ok_or(LoudnessError::MissingReport)?;
    let report: Report =
        serde_json::from_str(&stderr[start..=end]).map_err(|_| LoudnessError::MissingReport)?;
    Ok(LoudnessMeasurement {
        integrated_lufs: number("input_i", &report.input_i)?,
        true_peak_db: number("input_tp", &report.input_tp)?,
        loudness_range_lu: number("input_lra", &report.input_lra)?,
        threshold_lufs: number("input_thresh", &report.input_thresh)?,
        target_offset_db: number("target_offset", &report.target_offset)?,
    })
}

pub fn build_loudness_measure_args(input: &str) -> Vec<String> {
    [
        "-y",
        "-nostdin",
        "-hide_banner",
        "-v",
        "info",
        "-i",
        input,
        "-map",
        "0:a:0",
        "-vn",
        "-af",
        "loudnorm=print_format=json",
        "-f",
        "null",
        "-",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

pub fn build_loudness_normalize_args(
    input: &str,
    measured: &LoudnessMeasurement,
    target: &LoudnessTarget,
    output: &str,
) -> Vec<String> {
    let filter = format!(
        "loudnorm=I={}:TP={}:LRA=11:measured_I={}:measured_TP={}:measured_LRA={}:measured_thresh={}:offset={}:linear=true:print_format=json",
        target.integrated_lufs,
        target.true_peak_db,
        measured.integrated_lufs,
        measured.true_peak_db,
        measured.loudness_range_lu,
        measured.threshold_lufs,
        measured.target_offset_db,
    );
    [
        "-y".to_string(),
        "-nostdin".to_string(),
        "-hide_banner".to_string(),
        "-i".to_string(),
        input.to_string(),
        "-map".to_string(),
        "0:v?".to_string(),
        "-map".to_string(),
        "0:a:0".to_string(),
        "-c:v".to_string(),
        "copy".to_string(),
        "-c:a".to_string(),
        "aac".to_string(),
        "-b:a".to_string(),
        "192k".to_string(),
        "-af".to_string(),
        filter,
        "-movflags".to_string(),
        "+faststart".to_string(),
        "-f".to_string(),
        "mp4".to_string(),
        output.to_string(),
    ]
    .to_vec()
}

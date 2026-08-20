use thiserror::Error;

use crate::profile::{TargetProfile, encode_args};

#[derive(Debug, Clone, PartialEq, Error)]
pub enum HlsError {
    #[error("segment length {value} is not positive and finite")]
    SegmentLength { value: f64 },
    #[error("the output directory is empty")]
    OutputDir,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HlsTarget {
    pub profile: TargetProfile,
    pub segment_seconds: f64,
    pub copy_streams: bool,
}

fn muxer_args(segment_seconds: f64, output_dir: &str) -> Vec<String> {
    let mut args = vec![
        "-f".to_string(),
        "hls".to_string(),
        "-hls_time".to_string(),
        format!("{segment_seconds:.3}"),
        "-hls_playlist_type".to_string(),
        "vod".to_string(),
        "-hls_flags".to_string(),
        "independent_segments+temp_file".to_string(),
        "-hls_segment_type".to_string(),
        "fmp4".to_string(),
        "-hls_segment_filename".to_string(),
        format!("{output_dir}/seg%05d.m4s"),
        "-master_pl_name".to_string(),
        "none".to_string(),
    ];
    args.shrink_to_fit();
    args
}

pub fn build_hls_args(
    input: &str,
    target: &HlsTarget,
    output_dir: &str,
) -> Result<Vec<String>, HlsError> {
    if !target.segment_seconds.is_finite() || target.segment_seconds <= 0.0 {
        return Err(HlsError::SegmentLength {
            value: target.segment_seconds,
        });
    }
    if output_dir.is_empty() {
        return Err(HlsError::OutputDir);
    }
    let mut args = vec![
        "-y".to_string(),
        "-nostdin".to_string(),
        "-hide_banner".to_string(),
        "-i".to_string(),
        input.to_string(),
    ];
    if target.copy_streams {
        args.push("-c:v".to_string());
        args.push("copy".to_string());
        args.push("-c:a".to_string());
        args.push("copy".to_string());
    } else {
        let mut encoded = encode_args(&target.profile, "libx264", "aac");
        let muxed = encoded.len() - 2;
        encoded.truncate(muxed);
        args.extend(encoded);
    }
    args.extend(muxer_args(target.segment_seconds, output_dir));
    args.push(format!("{output_dir}/playlist.m3u8"));
    Ok(args)
}

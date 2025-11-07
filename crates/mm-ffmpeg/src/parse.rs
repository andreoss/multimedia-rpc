use std::collections::HashMap;
use std::time::Duration;

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq)]
pub struct MediaInfo {
    pub container: String,
    pub duration: Duration,
    pub size_bytes: Option<u64>,
    pub bit_rate: Option<u64>,
    pub video: Vec<VideoStream>,
    pub audio: Vec<AudioStream>,
    pub drm: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoStream {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub avg_fps: Option<f64>,
    pub real_fps: Option<f64>,
    pub rotation: i32,
    pub pixel_format: Option<String>,
    pub sample_aspect_ratio: Option<(u32, u32)>,
    pub interlaced: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioStream {
    pub codec: String,
    pub channels: u32,
    pub sample_rate: Option<u32>,
    pub bit_rate: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StillInfo {
    pub container: String,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub size_bytes: Option<u64>,
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("probe output is not valid json: {source}")]
    Json {
        #[from]
        source: serde_json::Error,
    },
    #[error("probe output carries no format section")]
    MissingFormat,
    #[error("probe output carries no duration and no size and bit rate to estimate one")]
    MissingDuration,
    #[error("probe output carries no image stream")]
    NoImageStream,
}

#[derive(Deserialize)]
struct RawProbe {
    format: Option<RawFormat>,
    #[serde(default)]
    streams: Vec<RawStream>,
}

#[derive(Deserialize)]
struct RawFormat {
    format_name: Option<String>,
    duration: Option<String>,
    size: Option<String>,
    bit_rate: Option<String>,
}

#[derive(Deserialize)]
struct RawStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    codec_tag_string: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    avg_frame_rate: Option<String>,
    r_frame_rate: Option<String>,
    sample_aspect_ratio: Option<String>,
    pix_fmt: Option<String>,
    field_order: Option<String>,
    channels: Option<u32>,
    sample_rate: Option<String>,
    bit_rate: Option<String>,
    #[serde(default)]
    side_data_list: Vec<RawSideData>,
    #[serde(default)]
    tags: HashMap<String, String>,
}

#[derive(Deserialize)]
struct RawSideData {
    rotation: Option<f64>,
}

const ENCRYPTED_CODEC_TAGS: [&str; 2] = ["encv", "enca"];
const INTERLACED_FIELD_ORDERS: [&str; 4] = ["tt", "bb", "tb", "bt"];

fn parse_rational(text: &str) -> Option<f64> {
    let (numerator, denominator) = text.split_once('/')?;
    let numerator: f64 = numerator.parse().ok()?;
    let denominator: f64 = denominator.parse().ok()?;
    if denominator == 0.0 {
        return None;
    }
    Some(numerator / denominator)
}

fn parse_ratio_pair(text: &str) -> Option<(u32, u32)> {
    let (numerator, denominator) = text.split_once(':')?;
    Some((numerator.parse().ok()?, denominator.parse().ok()?))
}

fn parse_number<T: std::str::FromStr>(text: Option<&String>) -> Option<T> {
    text.and_then(|value| value.parse().ok())
}

fn rotation_of(stream: &RawStream) -> i32 {
    let side_data = stream
        .side_data_list
        .iter()
        .find_map(|side_data| side_data.rotation);
    if let Some(rotation) = side_data {
        return rotation.round() as i32;
    }
    stream
        .tags
        .get("rotate")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

fn duration_of(format: &RawFormat) -> Result<Duration, ParseError> {
    let declared: Option<f64> = parse_number(format.duration.as_ref());
    let size: Option<f64> = parse_number(format.size.as_ref());
    let bit_rate: Option<f64> = parse_number(format.bit_rate.as_ref());
    let estimated = match (size, bit_rate) {
        (Some(size), Some(bit_rate)) if bit_rate > 0.0 => Some(size * 8.0 / bit_rate),
        _ => None,
    };
    let seconds = match (declared, estimated) {
        (Some(declared), Some(estimated)) => declared.max(estimated),
        (Some(declared), None) => declared,
        (None, Some(estimated)) => estimated,
        (None, None) => return Err(ParseError::MissingDuration),
    };
    Ok(Duration::from_secs_f64(seconds.max(0.0)))
}

fn is_encrypted(stream: &RawStream) -> bool {
    stream
        .codec_tag_string
        .as_deref()
        .is_some_and(|tag| ENCRYPTED_CODEC_TAGS.contains(&tag))
}

pub fn parse_media_info(json: &str) -> Result<MediaInfo, ParseError> {
    let raw: RawProbe = serde_json::from_str(json)?;
    let format = raw.format.ok_or(ParseError::MissingFormat)?;
    let duration = duration_of(&format)?;

    let mut video = Vec::new();
    let mut audio = Vec::new();
    let mut drm = false;
    for stream in &raw.streams {
        if is_encrypted(stream) {
            drm = true;
        }
        let codec = stream.codec_name.clone().unwrap_or_default();
        match stream.codec_type.as_deref() {
            Some("video") => video.push(VideoStream {
                codec,
                width: stream.width.unwrap_or(0),
                height: stream.height.unwrap_or(0),
                avg_fps: stream.avg_frame_rate.as_deref().and_then(parse_rational),
                real_fps: stream.r_frame_rate.as_deref().and_then(parse_rational),
                rotation: rotation_of(stream),
                pixel_format: stream.pix_fmt.clone(),
                sample_aspect_ratio: stream
                    .sample_aspect_ratio
                    .as_deref()
                    .and_then(parse_ratio_pair),
                interlaced: stream
                    .field_order
                    .as_deref()
                    .is_some_and(|order| INTERLACED_FIELD_ORDERS.contains(&order)),
            }),
            Some("audio") => audio.push(AudioStream {
                codec,
                channels: stream.channels.unwrap_or(0),
                sample_rate: parse_number(stream.sample_rate.as_ref()),
                bit_rate: parse_number(stream.bit_rate.as_ref()),
            }),
            _ => {}
        }
    }

    Ok(MediaInfo {
        container: format.format_name.unwrap_or_default(),
        duration,
        size_bytes: parse_number(format.size.as_ref()),
        bit_rate: parse_number(format.bit_rate.as_ref()),
        video,
        audio,
        drm,
    })
}

pub fn parse_keyframe_times(text: &str) -> Vec<f64> {
    text.lines()
        .filter_map(|line| line.split(',').next())
        .filter_map(|field| field.trim().parse::<f64>().ok())
        .collect()
}

pub fn parse_still_info(json: &str) -> Result<StillInfo, ParseError> {
    let raw: RawProbe = serde_json::from_str(json)?;
    let format = raw.format.ok_or(ParseError::MissingFormat)?;
    let stream = raw
        .streams
        .iter()
        .find(|stream| stream.codec_type.as_deref() == Some("video"))
        .ok_or(ParseError::NoImageStream)?;
    Ok(StillInfo {
        container: format.format_name.unwrap_or_default(),
        codec: stream.codec_name.clone().unwrap_or_default(),
        width: stream.width.unwrap_or_default(),
        height: stream.height.unwrap_or_default(),
        size_bytes: parse_number(format.size.as_ref()),
    })
}

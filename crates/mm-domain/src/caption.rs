use thiserror::Error;

use crate::time::Ticks;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CaptionFormat {
    Srt,
    WebVtt,
    Scc,
    Embedded,
}

impl CaptionFormat {
    pub fn extension(self) -> &'static str {
        match self {
            CaptionFormat::Srt => "srt",
            CaptionFormat::WebVtt => "vtt",
            CaptionFormat::Scc => "scc",
            CaptionFormat::Embedded => "embedded",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "srt" | "subrip" => Some(CaptionFormat::Srt),
            "vtt" | "webvtt" => Some(CaptionFormat::WebVtt),
            "scc" => Some(CaptionFormat::Scc),
            "embedded" => Some(CaptionFormat::Embedded),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            CaptionFormat::Srt => "srt",
            CaptionFormat::WebVtt => "webvtt",
            CaptionFormat::Scc => "scc",
            CaptionFormat::Embedded => "embedded",
        }
    }
}

impl std::fmt::Display for CaptionFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptionFormat::Srt => write!(f, "Srt"),
            CaptionFormat::WebVtt => write!(f, "WebVtt"),
            CaptionFormat::Scc => write!(f, "Scc"),
            CaptionFormat::Embedded => write!(f, "Embedded"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BroadcastCodec {
    Eia608,
    DvbTeletext,
    DvbSubtitle,
    DvdSubtitle,
    MovText,
    SubRip,
    Ass,
    WebVtt,
}

impl BroadcastCodec {
    pub fn name(self) -> &'static str {
        match self {
            BroadcastCodec::Eia608 => "eia_608",
            BroadcastCodec::DvbTeletext => "dvb_teletext",
            BroadcastCodec::DvbSubtitle => "dvb_subtitle",
            BroadcastCodec::DvdSubtitle => "dvd_subtitle",
            BroadcastCodec::MovText => "mov_text",
            BroadcastCodec::SubRip => "subrip",
            BroadcastCodec::Ass => "ass",
            BroadcastCodec::WebVtt => "webvtt",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "eia_608" => Some(BroadcastCodec::Eia608),
            "dvb_teletext" => Some(BroadcastCodec::DvbTeletext),
            "dvb_subtitle" => Some(BroadcastCodec::DvbSubtitle),
            "dvd_subtitle" => Some(BroadcastCodec::DvdSubtitle),
            "mov_text" => Some(BroadcastCodec::MovText),
            "subrip" => Some(BroadcastCodec::SubRip),
            "ass" => Some(BroadcastCodec::Ass),
            "webvtt" => Some(BroadcastCodec::WebVtt),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cue {
    start: Ticks,
    end: Ticks,
    text: String,
}

impl Cue {
    pub fn start(&self) -> Ticks {
        self.start
    }

    pub fn end(&self) -> Ticks {
        self.end
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptionDocument {
    format: CaptionFormat,
    cues: Vec<Cue>,
}

impl CaptionDocument {
    pub fn parse(format: CaptionFormat, text: &str) -> Result<Self, CaptionError> {
        let lines: Vec<&str> = text.lines().collect();
        let cues = match format {
            CaptionFormat::Srt => parse_srt(&lines)?,
            CaptionFormat::WebVtt => parse_webvtt(&lines)?,
            CaptionFormat::Scc => return Err(CaptionError::UnsupportedFormat {
                format,
                message: "SCC parsing not yet fully implemented".to_string(),
            }),
            CaptionFormat::Embedded => parse_webvtt(&lines)?,
        };
        if cues.is_empty() {
            return Err(CaptionError::EmptyDocument);
        }
        Ok(Self { format, cues })
    }

    pub fn format(&self) -> CaptionFormat {
        self.format
    }

    pub fn cues(&self) -> &[Cue] {
        &self.cues
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CaptionError {
    #[error("caption document is empty")]
    EmptyDocument,
    #[error("caption line {line} carries no timing")]
    MissingTiming { line: usize },
    #[error("caption line {line} is not a {format} timing line")]
    MalformedTiming { line: usize, format: &'static str },
    #[error("caption line {line} carries a malformed timestamp {value:?}")]
    MalformedTimestamp { line: usize, value: String },
    #[error("caption cue at line {line} ends before it starts")]
    EndBeforeStart { line: usize },
    #[error("caption cue at line {line} has no text")]
    EmptyCue { line: usize },
    #[error("webvtt document has no header")]
    MissingWebVttHeader,
    #[error("caption document has more than {max} cues")]
    TooManyCues { max: usize },
    #[error("SCC timecode at line is malformed")]
    MalformedSccTimecode { line: usize },
    #[error("caption at line never ends")]
    UnterminatedCue { line: usize },
    #[error("caption format {format} is not supported by this function")]
    UnsupportedFormat { format: CaptionFormat, message: String },
    #[error("SCC document is missing its header")]
    MissingSccHeader,
    #[error("SCC word at line carries a malformed hex value")]
    MalformedSccWord { line: usize },
}

const MAX_CUES: usize = 10_000;
const TICKS_PER_MILLI: u64 = 254_016_000;

fn parse_timestamp(value: &str, line: usize) -> Result<Ticks, CaptionError> {
    let (clock, fraction) = value.split_once(['.', ',']).unwrap_or((value, "000"));
    let mut parts = clock.split(':');
    let hours: u64 = parts
        .next()
        .and_then(|part| part.parse().ok())
        .ok_or_else(|| CaptionError::MalformedTimestamp {
            line,
            value: value.to_string(),
        })?;
    let minutes: u64 = parts
        .next()
        .and_then(|part| part.parse().ok())
        .ok_or_else(|| CaptionError::MalformedTimestamp {
            line,
            value: value.to_string(),
        })?;
    let seconds: u64 = parts
        .next()
        .and_then(|part| part.parse().ok())
        .ok_or_else(|| CaptionError::MalformedTimestamp {
            line,
            value: value.to_string(),
        })?;
    if parts.next().is_some() || minutes > 59 || seconds > 59 {
        return Err(CaptionError::MalformedTimestamp {
            line,
            value: value.to_string(),
        });
    }
    let millis_text = format!("{fraction:0<3}");
    let millis: u64 = millis_text
        .get(..3)
        .and_then(|part| part.parse().ok())
        .ok_or_else(|| CaptionError::MalformedTimestamp {
            line,
            value: value.to_string(),
        })?;
    let total = hours
        .checked_mul(3_600)
        .and_then(|value| value.checked_add(minutes * 60))
        .and_then(|value| value.checked_add(seconds))
        .and_then(|value| value.checked_mul(1000))
        .and_then(|value| value.checked_add(millis))
        .and_then(|value| value.checked_mul(TICKS_PER_MILLI))
        .ok_or_else(|| CaptionError::MalformedTimestamp {
            line,
            value: value.to_string(),
        })?;
    Ok(Ticks::new(total))
}

fn parse_timing(
    line_text: &str,
    line: usize,
    format: &'static str,
) -> Result<(Ticks, Ticks), CaptionError> {
    let (start, end) = line_text
        .split_once("-->")
        .ok_or(CaptionError::MissingTiming { line })?;
    let end = end.split_whitespace().next().unwrap_or("");
    if start.trim().is_empty() || end.is_empty() {
        return Err(CaptionError::MalformedTiming { line, format });
    }
    let start = parse_timestamp(start.trim(), line)?;
    let end = parse_timestamp(end, line)?;
    if end <= start {
        return Err(CaptionError::EndBeforeStart { line });
    }
    Ok((start, end))
}

fn cue_from_block(
    lines: &[&str],
    timing_index: usize,
    start_line: usize,
) -> Result<Cue, CaptionError> {
    let (start, end) = parse_timing(
        lines[timing_index],
        start_line + timing_index + 1,
        "caption",
    )?;
    let text = lines[timing_index + 1..].join("\n");
    if text.trim().is_empty() {
        return Err(CaptionError::EmptyCue {
            line: start_line + timing_index + 1,
        });
    }
    Ok(Cue { start, end, text })
}

fn parse_srt(lines: &[&str]) -> Result<Vec<Cue>, CaptionError> {
    let mut cues = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        while index < lines.len() && lines[index].trim().is_empty() {
            index += 1;
        }
        if index >= lines.len() {
            break;
        }
        let start_line = index;
        if lines[index].trim().parse::<u64>().is_ok() {
            index += 1;
            while index < lines.len() && lines[index].trim().is_empty() {
                index += 1;
            }
        }
        if index >= lines.len() {
            break;
        }
        let timing = index;
        let mut end = index + 1;
        while end < lines.len() && !lines[end].trim().is_empty() {
            end += 1;
        }
        let cue = cue_from_block(&lines[start_line..end], timing - start_line, start_line)?;
        cues.push(cue);
        if cues.len() > MAX_CUES {
            return Err(CaptionError::TooManyCues { max: MAX_CUES });
        }
        index = end;
    }
    Ok(cues)
}

fn parse_webvtt(lines: &[&str]) -> Result<Vec<Cue>, CaptionError> {
    let Some(first) = lines.iter().position(|line| !line.trim().is_empty()) else {
        return Err(CaptionError::EmptyDocument);
    };
    if !lines[first].trim_start().starts_with("WEBVTT") {
        return Err(CaptionError::MissingWebVttHeader);
    }
    let mut cues = Vec::new();
    let mut index = first + 1;
    while index < lines.len() {
        while index < lines.len() && lines[index].trim().is_empty() {
            index += 1;
        }
        if index >= lines.len() {
            break;
        }
        if lines[index].trim_start().starts_with("NOTE")
            || lines[index].trim_start().starts_with("STYLE")
        {
            while index < lines.len() && !lines[index].trim().is_empty() {
                index += 1;
            }
            continue;
        }
        let start_line = index;
        let timing = if lines[index].contains("-->") {
            index
        } else {
            index + 1
        };
        if timing >= lines.len() || !lines[timing].contains("-->") {
            return Err(CaptionError::MissingTiming {
                line: start_line + 1,
            });
        }
        let mut end = timing + 1;
        while end < lines.len() && !lines[end].trim().is_empty() {
            end += 1;
        }
        let cue = cue_from_block(&lines[start_line..end], timing - start_line, start_line)?;
        cues.push(cue);
        if cues.len() > MAX_CUES {
            return Err(CaptionError::TooManyCues { max: MAX_CUES });
        }
        index = end;
    }
    Ok(cues)
}

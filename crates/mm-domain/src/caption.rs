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
            CaptionFormat::Embedded => "vtt",
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

    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "srt" | "subrip" => Some(CaptionFormat::Srt),
            "vtt" | "webvtt" => Some(CaptionFormat::WebVtt),
            "scc" => Some(CaptionFormat::Scc),
            "embedded" => Some(CaptionFormat::Embedded),
            _ => None,
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
    Cea708,
}

impl BroadcastCodec {
    pub fn name(self) -> &'static str {
        match self {
            Self::Eia608 => "eia_608",
            Self::DvbTeletext => "dvb_teletext",
            Self::DvbSubtitle => "dvb_subtitle",
            Self::DvdSubtitle => "dvd_subtitle",
            Self::MovText => "mov_text",
            Self::SubRip => "subrip",
            Self::Ass => "ass",
            Self::WebVtt => "webvtt",
            Self::Cea708 => "cea_708",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "eia_608" | "cea-608" | "cea_608" | "eia-608" => Some(Self::Eia608),
            "dvb_teletext" | "teletext" | "dvb_teletext_dec" => Some(Self::DvbTeletext),
            "dvb_subtitle" | "dvbsub" => Some(Self::DvbSubtitle),
            "dvd_subtitle" | "dvdsub" => Some(Self::DvdSubtitle),
            "mov_text" => Some(Self::MovText),
            "subrip" => Some(Self::SubRip),
            "ass" => Some(Self::Ass),
            "webvtt" => Some(Self::WebVtt),
            "c708" | "cea_708" | "cea-708" => Some(Self::Cea708),
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
            CaptionFormat::Scc => parse_scc(&lines)?,
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

    pub fn render_webvtt(&self) -> String {
        let mut rendered = String::from("WEBVTT\n\n");
        for cue in &self.cues {
            rendered.push_str(&render_timestamp(cue.start));
            rendered.push_str(" --> ");
            rendered.push_str(&render_timestamp(cue.end));
            rendered.push('\n');
            rendered.push_str(&cue.text);
            rendered.push_str("\n\n");
        }
        rendered
    }
}

fn render_timestamp(value: Ticks) -> String {
    let millis = (value.micros() + 500) / 1_000;
    let hours = millis / 3_600_000;
    let minutes = (millis / 60_000) % 60;
    let seconds = (millis / 1_000) % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}.{:03}", millis % 1_000)
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CaptionError {
    #[error("caption document is empty")]
    EmptyDocument,
    #[error("scc document carries no Scenarist_SCC header")]
    MissingSccHeader,
    #[error("scc line {line} carries the word {value:?}, which is not two hex bytes")]
    MalformedSccWord { line: usize, value: String },
    #[error("scc line {line} carries the timecode {value:?}, which is not HH:MM:SS:FF")]
    MalformedSccTimecode { line: usize, value: String },
    #[error("scc line {line} opens a cue that never ends")]
    UnterminatedCue { line: usize },
    #[error("caption track {codec} carries no text this service reads")]
    UnsupportedCodec { codec: String },
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
}

const MAX_CUES: usize = 10_000;
const TICKS_PER_MILLI: u64 = 254_016_000;

fn parse_timestamp(value: &str, line: usize) -> Result<Ticks, CaptionError> {
    let (clock, fraction) = value.split_once(['.', ',']).unwrap_or((value, "000"));
    let mut parts = clock.split(':');
    let first: u64 = parts
        .next()
        .and_then(|part| part.parse().ok())
        .ok_or_else(|| CaptionError::MalformedTimestamp {
            line,
            value: value.to_string(),
        })?;
    let second: u64 = parts
        .next()
        .and_then(|part| part.parse().ok())
        .ok_or_else(|| CaptionError::MalformedTimestamp {
            line,
            value: value.to_string(),
        })?;
    let third: Option<Result<u64, std::num::ParseIntError>> = parts.next().map(str::parse::<u64>);
    let (hours, minutes, seconds) = match third {
        None => (0, first, second),
        Some(Ok(seconds)) => (first, second, seconds),
        _ => {
            return Err(CaptionError::MalformedTimestamp {
                line,
                value: value.to_string(),
            });
        }
    };
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

const SCC_HEADER: &str = "Scenarist_SCC";
const SCC_FRAMES_PER_SECOND: u64 = 30;
const TICKS_PER_SCC_FRAME: u64 = 8_475_667_200;

const BASIC_ROW: [char; 96] = [
    ' ', '!', '"', '#', '$', '%', '&', '\'', '(', ')', 'á', '+', ',', '-', '.', '/', '0', '1', '2',
    '3', '4', '5', '6', '7', '8', '9', ':', ';', '<', '=', '>', '?', '@', 'A', 'B', 'C', 'D', 'E',
    'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X',
    'Y', 'Z', '[', '£', ']', 'é', 'í', 'ó', 'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k',
    'l', 'm', 'n', 'o', 'p', 'q', 'r', 's', 't', 'u', 'v', 'w', 'x', 'y', 'z', 'ç', '÷', 'Ñ', 'ñ',
    '█',
];

fn scc_frame_count(value: &str, line: usize) -> Result<u64, CaptionError> {
    let malformed = CaptionError::MalformedSccTimecode {
        line,
        value: value.to_string(),
    };
    let parts: Vec<&str> = value.split(':').collect();
    if parts.len() != 4 {
        return Err(malformed);
    }
    let mut numbers = [0u64; 4];
    for (at, part) in parts.iter().enumerate() {
        if part.is_empty() || part.len() > 2 || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(malformed);
        }
        numbers[at] = part.parse().map_err(|_| malformed.clone())?;
    }
    if numbers[2] > 59 || numbers[3] > 29 {
        return Err(malformed);
    }
    let minutes = numbers[0] * 60 + numbers[1];
    let dropped = 2 * (minutes - minutes / 10);
    let labelled =
        (numbers[0] * 3_600 + numbers[1] * 60 + numbers[2]) * SCC_FRAMES_PER_SECOND + numbers[3];
    labelled
        .checked_sub(dropped)
        .and_then(|frames| frames.checked_mul(TICKS_PER_SCC_FRAME))
        .ok_or(malformed)
}

enum SccWord {
    Control,
    Load,
    Display,
    Erase,
    Clear,
    Text(char),
}

fn scc_word(byte: u8, next: Option<u8>) -> (SccWord, usize) {
    match byte {
        0x12 | 0x14 => match next {
            Some(0x20) => (SccWord::Load, 2),
            Some(0x2C) => (SccWord::Erase, 2),
            Some(0x2E) => (SccWord::Clear, 2),
            Some(0x2F) => (SccWord::Display, 2),
            Some(0x20..=0x2F) => (SccWord::Control, 2),
            Some(_) => (SccWord::Control, 4),
            None => (SccWord::Control, 1),
        },
        0x10..=0x11 | 0x13 | 0x15..=0x17 | 0x19 | 0x1B..=0x1F => (SccWord::Control, 2),
        0x18 | 0x1A => match next {
            Some(value @ 0x20..=0x7E) => (SccWord::Text(char::from(value)), 3),
            _ => (SccWord::Control, 3),
        },
        0x20..=0x7F => (SccWord::Text(BASIC_ROW[usize::from(byte) - 0x20]), 1),
        _ => (SccWord::Control, 1),
    }
}

fn parse_scc(lines: &[&str]) -> Result<Vec<Cue>, CaptionError> {
    let header = lines
        .iter()
        .position(|line| !line.trim().is_empty())
        .ok_or(CaptionError::MissingSccHeader)?;
    if !lines[header].trim_start().starts_with(SCC_HEADER) {
        return Err(CaptionError::MissingSccHeader);
    }
    let mut cues = Vec::new();
    let mut pending = String::new();
    let mut displayed: Option<(Ticks, String)> = None;
    let mut last = header + 1;
    for (index, line) in lines.iter().enumerate().skip(header + 1) {
        if line.trim().is_empty() {
            continue;
        }
        let number = index + 1;
        last = number;
        let (code, words) = line
            .split_once('\t')
            .map(|(code, words)| (code.trim(), words))
            .unwrap_or((line.trim(), ""));
        let at = scc_frame_count(code, number).map_err(|error| match error {
            CaptionError::MalformedSccTimecode { value, .. } => {
                CaptionError::MalformedSccTimecode {
                    line: number,
                    value,
                }
            }
            other => other,
        })?;
        let at = Ticks::new(at);
        let mut bytes = Vec::new();
        for word in words.split_whitespace() {
            if word.len() != 4 {
                return Err(CaptionError::MalformedSccWord {
                    line: number,
                    value: word.to_string(),
                });
            }
            let pair =
                u16::from_str_radix(word, 16).map_err(|_| CaptionError::MalformedSccWord {
                    line: number,
                    value: word.to_string(),
                })?;
            bytes.push((pair >> 8) as u8 & 0x7F);
            bytes.push((pair & 0xFF) as u8 & 0x7F);
        }
        let mut cursor = 0;
        while cursor < bytes.len() {
            let (word, width) = scc_word(bytes[cursor], bytes.get(cursor + 1).copied());
            cursor += width;
            match word {
                SccWord::Text(character) => pending.push(character),
                SccWord::Load => pending.clear(),
                SccWord::Display => {
                    if !pending.trim().is_empty() {
                        if let Some((from, text)) = displayed.take()
                            && from < at
                        {
                            cues.push(Cue {
                                start: from,
                                end: at,
                                text,
                            });
                        }
                        displayed = Some((at, std::mem::take(&mut pending)));
                    }
                }
                SccWord::Erase => {
                    if let Some((from, text)) = displayed.take()
                        && from < at
                    {
                        cues.push(Cue {
                            start: from,
                            end: at,
                            text,
                        });
                    }
                    pending.clear();
                }
                SccWord::Clear => pending.clear(),
                SccWord::Control => {}
            }
            if cues.len() > MAX_CUES {
                return Err(CaptionError::TooManyCues { max: MAX_CUES });
            }
        }
    }
    if displayed.is_some() {
        return Err(CaptionError::UnterminatedCue { line: last });
    }
    Ok(cues)
}

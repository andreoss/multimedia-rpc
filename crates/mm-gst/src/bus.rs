#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusKind {
    Error,
    Warning,
    Eos,
    StateChanged,
    StreamStart,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusMessage {
    pub kind: BusKind,
    pub source: Option<String>,
    pub details: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BusError {
    #[error("bus line carries no severity")]
    MissingSeverity,
    #[error("bus severity {0:?} is not one this reader knows")]
    UnknownSeverity(String),
}

pub fn parse_bus_line(line: &str) -> Result<BusMessage, BusError> {
    let trimmed = line.trim();
    let Some((severity, rest)) = trimmed.split_once(char::is_whitespace) else {
        return match trimmed.to_ascii_uppercase().as_str() {
            "EOS" => Ok(BusMessage {
                kind: BusKind::Eos,
                source: None,
                details: String::new(),
            }),
            "" => Err(BusError::MissingSeverity),
            other => Err(BusError::UnknownSeverity(other.to_string())),
        };
    };
    let kind = match severity.to_ascii_uppercase().as_str() {
        "ERROR" => BusKind::Error,
        "WARNING" | "WARN" => BusKind::Warning,
        "EOS" => BusKind::Eos,
        "STATE" | "STATE_CHANGED" => BusKind::StateChanged,
        "STREAM_START" | "STREAM-START" => BusKind::StreamStart,
        other => return Err(BusError::UnknownSeverity(other.to_string())),
    };
    let rest = rest.trim();
    let (source, details) = match rest.split_once(" : ") {
        Some((source, details)) if !source.is_empty() => {
            (Some(source.trim().to_string()), details.trim().to_string())
        }
        _ => (None, rest.to_string()),
    };
    Ok(BusMessage {
        kind,
        source,
        details,
    })
}

pub fn parse_bus_output(text: &str) -> Vec<BusMessage> {
    text.lines()
        .filter_map(|line| parse_bus_line(line).ok())
        .collect()
}

pub fn first_error(messages: &[BusMessage]) -> Option<&BusMessage> {
    messages
        .iter()
        .find(|message| message.kind == BusKind::Error)
}

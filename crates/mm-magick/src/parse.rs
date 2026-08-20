#[derive(Debug, Clone, PartialEq)]
pub struct ImageInfo {
    pub format: String,
    pub width: u32,
    pub height: u32,
    pub colorspace: String,
    pub depth: u32,
    pub size_bytes: Option<u64>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("identify produced no frame line")]
    Empty,
    #[error("identify line carries {found} fields, expected 6")]
    FieldCount { found: usize },
    #[error("identify frame field {field} is not a number: {value:?}")]
    Field { field: &'static str, value: String },
}

fn size_bytes(text: &str) -> Result<Option<u64>, ParseError> {
    let trimmed = text.trim();
    let split = trimmed
        .find(|character: char| character.is_ascii_alphabetic())
        .unwrap_or(trimmed.len());
    let (number, suffix) = trimmed.split_at(split);
    let number: f64 = number.parse().map_err(|_| ParseError::Field {
        field: "size",
        value: text.to_string(),
    })?;
    let multiplier = match suffix.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1.0,
        "KB" | "K" => 1024.0,
        "MB" | "M" => 1024.0 * 1024.0,
        "GB" | "G" => 1024.0 * 1024.0 * 1024.0,
        _ => {
            return Err(ParseError::Field {
                field: "size",
                value: text.to_string(),
            });
        }
    };
    Ok(Some((number * multiplier) as u64))
}

pub fn parse_identify_output(text: &str) -> Result<ImageInfo, ParseError> {
    let line = text.lines().find(|line| !line.trim().is_empty());
    let Some(line) = line else {
        return Err(ParseError::Empty);
    };
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 6 {
        return Err(ParseError::FieldCount {
            found: fields.len(),
        });
    }
    let frame = |field: &'static str, value: &str| -> Result<u32, ParseError> {
        value.parse().map_err(|_| ParseError::Field {
            field,
            value: value.to_string(),
        })
    };
    Ok(ImageInfo {
        format: fields[0].to_string(),
        width: frame("width", fields[1])?,
        height: frame("height", fields[2])?,
        colorspace: fields[3].to_string(),
        depth: frame("depth", fields[4])?,
        size_bytes: size_bytes(fields[5])?,
    })
}

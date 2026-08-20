use std::fmt;
use std::str::FromStr;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobId(Uuid);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum JobIdError {
    #[error("job id is not a valid uuid")]
    Malformed,
    #[error("job id must not be the nil uuid")]
    Nil,
}

impl JobId {
    pub fn generate() -> Self {
        Self(Uuid::now_v7())
    }

    pub fn from_uuid(value: Uuid) -> Result<Self, JobIdError> {
        if value.is_nil() {
            return Err(JobIdError::Nil);
        }
        Ok(Self(value))
    }

    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl FromStr for JobId {
    type Err = JobIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let parsed = Uuid::try_parse(value).map_err(|_| JobIdError::Malformed)?;
        if parsed.is_nil() {
            return Err(JobIdError::Nil);
        }
        Ok(Self(parsed))
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TextIdError {
    #[error("identifier is empty")]
    Empty,
    #[error("identifier length {length} exceeds maximum {max}")]
    TooLong { length: usize, max: usize },
    #[error("identifier contains forbidden character {character:?}")]
    ForbiddenCharacter { character: char },
}

pub(crate) fn parse_text_id(value: &str, max: usize) -> Result<String, TextIdError> {
    if value.is_empty() {
        return Err(TextIdError::Empty);
    }
    let length = value.chars().count();
    if length > max {
        return Err(TextIdError::TooLong { length, max });
    }
    for character in value.chars() {
        if !character.is_ascii_graphic() {
            return Err(TextIdError::ForbiddenCharacter { character });
        }
    }
    Ok(value.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UserId(String);

pub type UserIdError = TextIdError;

impl UserId {
    pub const MAX_LENGTH: usize = 64;

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for UserId {
    type Err = UserIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_text_id(value, Self::MAX_LENGTH).map(Self)
    }
}

impl fmt::Display for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkerId(String);

pub type WorkerIdError = TextIdError;

impl WorkerId {
    pub const MAX_LENGTH: usize = 128;

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for WorkerId {
    type Err = WorkerIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_text_id(value, Self::MAX_LENGTH).map(Self)
    }
}

impl fmt::Display for WorkerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestId(String);

pub type RequestIdError = TextIdError;

impl RequestId {
    pub const MAX_LENGTH: usize = 128;

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for RequestId {
    type Err = RequestIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_text_id(value, Self::MAX_LENGTH).map(Self)
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TraceId(Uuid);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TraceIdError {
    #[error("trace id is not a valid uuid")]
    Malformed,
    #[error("trace id must not be the nil uuid")]
    Nil,
}

impl TraceId {
    pub fn generate() -> Self {
        TraceId(Uuid::now_v7())
    }

    pub fn from_uuid(value: Uuid) -> Result<Self, TraceIdError> {
        if value.is_nil() {
            return Err(TraceIdError::Nil);
        }
        Ok(TraceId(value))
    }

    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl FromStr for TraceId {
    type Err = TraceIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let parsed = Uuid::try_parse(value).map_err(|_| TraceIdError::Malformed)?;
        if parsed.is_nil() {
            return Err(TraceIdError::Nil);
        }
        Ok(TraceId(parsed))
    }
}

impl fmt::Display for TraceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

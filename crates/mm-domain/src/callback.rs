use async_trait::async_trait;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueUrl(String);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum QueueUrlError {
    #[error("queue url must be an http(s) url with a host: {value}")]
    Scheme { value: String },
}

impl QueueUrl {
    pub fn new(value: &str) -> Result<Self, QueueUrlError> {
        let rest = value
            .strip_prefix("http://")
            .or_else(|| value.strip_prefix("https://"));
        match rest {
            Some(rest) if !rest.is_empty() => Ok(Self(value.to_string())),
            _ => Err(QueueUrlError::Scheme {
                value: value.to_string(),
            }),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for QueueUrl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CallbackError {
    #[error("callback publish failed: {message}")]
    Backend { message: String },
}

#[async_trait]
pub trait CallbackPublisher: Send + Sync {
    async fn publish(&self, queue: &QueueUrl, message: &str) -> Result<(), CallbackError>;
}

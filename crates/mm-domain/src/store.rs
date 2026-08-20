use std::fmt;
use std::pin::Pin;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::Stream;
use futures_util::StreamExt;
use futures_util::stream;
use thiserror::Error;

use crate::id::JobId;
use crate::op::{MediaRefError, ObjectRef};

pub type ObjectBody = Pin<Box<dyn Stream<Item = Result<Bytes, ObjectStoreError>> + Send>>;

pub fn body_from_bytes(bytes: impl Into<Bytes>) -> ObjectBody {
    Box::pin(stream::once(std::future::ready(Ok(bytes.into()))))
}

pub async fn collect_body(mut body: ObjectBody) -> Result<Vec<u8>, ObjectStoreError> {
    let mut collected = Vec::new();
    while let Some(chunk) = body.next().await {
        collected.extend_from_slice(&chunk?);
    }
    Ok(collected)
}

#[derive(Debug, Error)]
pub enum ObjectStoreError {
    #[error("object {bucket}/{key} not found")]
    NotFound { bucket: String, key: String },
    #[error("object store backend error: {message}")]
    Backend { message: String },
}

#[async_trait]
pub trait ObjectStore: Send + Sync {
    async fn get(&self, object: &ObjectRef) -> Result<ObjectBody, ObjectStoreError>;

    async fn put(
        &self,
        object: &ObjectRef,
        body: ObjectBody,
        length: u64,
    ) -> Result<(), ObjectStoreError>;

    async fn put_multipart(
        &self,
        object: &ObjectRef,
        body: ObjectBody,
    ) -> Result<(), ObjectStoreError>;

    async fn head(&self, object: &ObjectRef) -> Result<u64, ObjectStoreError>;

    async fn delete(&self, object: &ObjectRef) -> Result<(), ObjectStoreError>;

    async fn presign_get(
        &self,
        object: &ObjectRef,
        expires_in: Duration,
    ) -> Result<String, ObjectStoreError>;

    async fn presign_put(
        &self,
        _object: &ObjectRef,
        _expires_in: Duration,
    ) -> Result<String, ObjectStoreError> {
        Err(ObjectStoreError::Backend {
            message: "presigned put is not implemented by this store".to_string(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum OutputKeyError {
    #[error("artifact path is empty")]
    Empty,
    #[error("artifact path has an empty segment")]
    EmptySegment,
    #[error("artifact path segment {segment:?} is reserved")]
    ReservedSegment { segment: String },
    #[error("artifact path contains forbidden character {character:?}")]
    ForbiddenCharacter { character: char },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputKey(String);

impl OutputKey {
    pub fn new(job_id: &JobId, artifact: &str) -> Result<Self, OutputKeyError> {
        if artifact.is_empty() {
            return Err(OutputKeyError::Empty);
        }
        for segment in artifact.split('/') {
            if segment.is_empty() {
                return Err(OutputKeyError::EmptySegment);
            }
            if segment == "." || segment == ".." {
                return Err(OutputKeyError::ReservedSegment {
                    segment: segment.to_string(),
                });
            }
            for character in segment.chars() {
                if !character.is_ascii_graphic() {
                    return Err(OutputKeyError::ForbiddenCharacter { character });
                }
            }
        }
        Ok(Self(format!("jobs/{job_id}/{artifact}")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_object(self, bucket: &str) -> Result<ObjectRef, MediaRefError> {
        ObjectRef::new(bucket, &self.0)
    }
}

impl fmt::Display for OutputKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

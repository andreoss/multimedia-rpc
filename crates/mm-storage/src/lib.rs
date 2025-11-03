use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_s3::Client;
use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use aws_smithy_types::body::SdkBody;
use aws_smithy_types::error::display::DisplayErrorContext;
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use futures_util::stream;
use http_body::{Body, Frame};
use mm_domain::{ObjectBody, ObjectRef, ObjectStore, ObjectStoreError};
use sync_wrapper::SyncWrapper;

const PART_SIZE: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone)]
pub enum CredentialSource {
    DefaultChain,
    Static {
        access_key_id: String,
        secret_access_key: String,
    },
}

#[derive(Debug, Clone)]
pub struct S3Settings {
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub force_path_style: bool,
    pub credentials: CredentialSource,
}

#[derive(Debug, Clone)]
pub struct S3ObjectStore {
    client: Client,
}

struct BodyAdapter(SyncWrapper<ObjectBody>);

impl Body for BodyAdapter {
    type Data = Bytes;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        self.get_mut()
            .0
            .get_mut()
            .as_mut()
            .poll_next(cx)
            .map(|next| {
                next.map(|chunk| {
                    chunk
                        .map(Frame::data)
                        .map_err(|error| Box::new(error) as Self::Error)
                })
            })
    }
}

fn backend<E: std::error::Error>(error: E) -> ObjectStoreError {
    ObjectStoreError::Backend {
        message: DisplayErrorContext(&error).to_string(),
    }
}

fn not_found(object: &ObjectRef) -> ObjectStoreError {
    ObjectStoreError::NotFound {
        bucket: object.bucket().to_string(),
        key: object.key().to_string(),
    }
}

impl S3ObjectStore {
    pub async fn connect(settings: S3Settings) -> Self {
        let base = aws_config::defaults(BehaviorVersion::latest()).load().await;
        let mut builder =
            aws_sdk_s3::config::Builder::from(&base).force_path_style(settings.force_path_style);
        if let Some(endpoint) = settings.endpoint {
            builder = builder.endpoint_url(endpoint);
        }
        if let Some(region) = settings.region {
            builder = builder.region(Region::new(region));
        }
        if let CredentialSource::Static {
            access_key_id,
            secret_access_key,
        } = settings.credentials
        {
            builder = builder.credentials_provider(Credentials::new(
                access_key_id,
                secret_access_key,
                None,
                None,
                "configured",
            ));
        }
        Self {
            client: Client::from_conf(builder.build()),
        }
    }

    pub async fn ensure_bucket(&self, bucket: &str) -> Result<(), ObjectStoreError> {
        match self.client.create_bucket().bucket(bucket).send().await {
            Ok(_) => Ok(()),
            Err(error) => {
                let already_there = error.as_service_error().is_some_and(|service| {
                    service.is_bucket_already_owned_by_you() || service.is_bucket_already_exists()
                });
                if already_there {
                    Ok(())
                } else {
                    Err(backend(error))
                }
            }
        }
    }

    pub async fn delete_bucket(&self, bucket: &str) -> Result<(), ObjectStoreError> {
        self.client
            .delete_bucket()
            .bucket(bucket)
            .send()
            .await
            .map(|_| ())
            .map_err(backend)
    }

    async fn upload_part(
        &self,
        object: &ObjectRef,
        upload_id: &str,
        part_number: i32,
        part: bytes::Bytes,
    ) -> Result<CompletedPart, ObjectStoreError> {
        let uploaded = self
            .client
            .upload_part()
            .bucket(object.bucket())
            .key(object.key())
            .upload_id(upload_id)
            .part_number(part_number)
            .body(ByteStream::from(part))
            .send()
            .await
            .map_err(backend)?;
        Ok(CompletedPart::builder()
            .set_e_tag(uploaded.e_tag().map(str::to_string))
            .part_number(part_number)
            .build())
    }

    async fn run_multipart(
        &self,
        object: &ObjectRef,
        upload_id: &str,
        mut body: ObjectBody,
    ) -> Result<(), ObjectStoreError> {
        let mut parts = Vec::new();
        let mut buffer = BytesMut::new();
        let mut part_number = 1;
        while let Some(chunk) = body.next().await {
            buffer.extend_from_slice(&chunk?);
            while buffer.len() >= PART_SIZE {
                let part = buffer.split_to(PART_SIZE).freeze();
                parts.push(
                    self.upload_part(object, upload_id, part_number, part)
                        .await?,
                );
                part_number += 1;
            }
        }
        if !buffer.is_empty() || parts.is_empty() {
            let part = buffer.freeze();
            parts.push(
                self.upload_part(object, upload_id, part_number, part)
                    .await?,
            );
        }
        self.client
            .complete_multipart_upload()
            .bucket(object.bucket())
            .key(object.key())
            .upload_id(upload_id)
            .multipart_upload(
                CompletedMultipartUpload::builder()
                    .set_parts(Some(parts))
                    .build(),
            )
            .send()
            .await
            .map_err(backend)?;
        Ok(())
    }
}

#[async_trait]
impl ObjectStore for S3ObjectStore {
    async fn get(&self, object: &ObjectRef) -> Result<ObjectBody, ObjectStoreError> {
        let response = self
            .client
            .get_object()
            .bucket(object.bucket())
            .key(object.key())
            .send()
            .await
            .map_err(|error| {
                if error
                    .as_service_error()
                    .is_some_and(|service| service.is_no_such_key())
                {
                    not_found(object)
                } else {
                    backend(error)
                }
            })?;
        let chunks = stream::unfold(response.body, |mut body| async move {
            match body.next().await {
                Some(Ok(bytes)) => Some((Ok(bytes), body)),
                Some(Err(error)) => Some((Err(backend(error)), body)),
                None => None,
            }
        });
        Ok(Box::pin(chunks))
    }

    async fn put(
        &self,
        object: &ObjectRef,
        body: ObjectBody,
        length: u64,
    ) -> Result<(), ObjectStoreError> {
        let sdk_body = SdkBody::from_body_1_x(BodyAdapter(SyncWrapper::new(body)));
        self.client
            .put_object()
            .bucket(object.bucket())
            .key(object.key())
            .content_length(length as i64)
            .body(ByteStream::new(sdk_body))
            .send()
            .await
            .map(|_| ())
            .map_err(backend)
    }

    async fn put_multipart(
        &self,
        object: &ObjectRef,
        body: ObjectBody,
    ) -> Result<(), ObjectStoreError> {
        let created = self
            .client
            .create_multipart_upload()
            .bucket(object.bucket())
            .key(object.key())
            .send()
            .await
            .map_err(backend)?;
        let upload_id = created
            .upload_id()
            .ok_or_else(|| ObjectStoreError::Backend {
                message: "multipart upload id missing".to_string(),
            })?
            .to_string();
        let outcome = self.run_multipart(object, &upload_id, body).await;
        if outcome.is_err() {
            let _ = self
                .client
                .abort_multipart_upload()
                .bucket(object.bucket())
                .key(object.key())
                .upload_id(&upload_id)
                .send()
                .await;
        }
        outcome
    }

    async fn head(&self, object: &ObjectRef) -> Result<u64, ObjectStoreError> {
        let response = self
            .client
            .head_object()
            .bucket(object.bucket())
            .key(object.key())
            .send()
            .await
            .map_err(|error| {
                if error
                    .as_service_error()
                    .is_some_and(|service| service.is_not_found())
                {
                    not_found(object)
                } else {
                    backend(error)
                }
            })?;
        response
            .content_length()
            .and_then(|length| u64::try_from(length).ok())
            .ok_or_else(|| ObjectStoreError::Backend {
                message: "content length missing".to_string(),
            })
    }

    async fn delete(&self, object: &ObjectRef) -> Result<(), ObjectStoreError> {
        self.client
            .delete_object()
            .bucket(object.bucket())
            .key(object.key())
            .send()
            .await
            .map(|_| ())
            .map_err(backend)
    }

    async fn presign_get(
        &self,
        object: &ObjectRef,
        expires_in: Duration,
    ) -> Result<String, ObjectStoreError> {
        let config = PresigningConfig::expires_in(expires_in).map_err(backend)?;
        let presigned = self
            .client
            .get_object()
            .bucket(object.bucket())
            .key(object.key())
            .presigned(config)
            .await
            .map_err(backend)?;
        Ok(presigned.uri().to_string())
    }
}

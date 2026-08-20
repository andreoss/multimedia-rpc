use std::time::Duration;

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_sqs::config::{Credentials, Region};
use aws_smithy_types::error::display::DisplayErrorContext;
use mm_domain::{CallbackError, CallbackPublisher, QueueUrl};

use crate::CredentialSource;

#[derive(Debug, Clone)]
pub struct SqsSettings {
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub credentials: CredentialSource,
}

#[derive(Debug, Clone)]
pub struct SqsCallbackPublisher {
    client: aws_sdk_sqs::Client,
}

fn publish_failure<E: std::error::Error>(error: E) -> CallbackError {
    CallbackError::Backend {
        message: DisplayErrorContext(&error).to_string(),
    }
}

impl SqsCallbackPublisher {
    pub async fn connect(settings: SqsSettings) -> Self {
        let base = aws_config::defaults(BehaviorVersion::latest()).load().await;
        let mut builder = aws_sdk_sqs::config::Builder::from(&base);
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
            client: aws_sdk_sqs::Client::from_conf(builder.build()),
        }
    }

    pub async fn ensure_queue(&self, name: &str) -> Result<QueueUrl, CallbackError> {
        let created = self
            .client
            .create_queue()
            .queue_name(name)
            .send()
            .await
            .map_err(publish_failure)?;
        let url = created.queue_url().ok_or_else(|| CallbackError::Backend {
            message: format!("queue {name} came back without a url"),
        })?;
        QueueUrl::new(url).map_err(|error| CallbackError::Backend {
            message: error.to_string(),
        })
    }

    pub async fn delete_queue(&self, queue: &QueueUrl) -> Result<(), CallbackError> {
        self.client
            .delete_queue()
            .queue_url(queue.as_str())
            .send()
            .await
            .map(|_| ())
            .map_err(publish_failure)
    }

    pub async fn receive(
        &self,
        queue: &QueueUrl,
        wait: Duration,
    ) -> Result<Vec<String>, CallbackError> {
        let received = self
            .client
            .receive_message()
            .queue_url(queue.as_str())
            .max_number_of_messages(10)
            .wait_time_seconds(wait.as_secs().min(20) as i32)
            .send()
            .await
            .map_err(publish_failure)?;
        let messages = received.messages.unwrap_or_default();
        let mut bodies = Vec::with_capacity(messages.len());
        for message in messages {
            if let Some(receipt) = message.receipt_handle() {
                self.client
                    .delete_message()
                    .queue_url(queue.as_str())
                    .receipt_handle(receipt)
                    .send()
                    .await
                    .map_err(publish_failure)?;
            }
            bodies.push(message.body.unwrap_or_default());
        }
        Ok(bodies)
    }
}

#[async_trait]
impl CallbackPublisher for SqsCallbackPublisher {
    async fn publish(&self, queue: &QueueUrl, message: &str) -> Result<(), CallbackError> {
        self.client
            .send_message()
            .queue_url(queue.as_str())
            .message_body(message)
            .send()
            .await
            .map(|_| ())
            .map_err(publish_failure)
    }
}

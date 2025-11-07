use std::str::FromStr;
use std::time::SystemTime;

use mm_auth::{Identity, Settings, Verifier, bearer_token};
use mm_domain::{TraceId, UserId};
use mm_observability::{Component, record};
use mm_proto::v1;
use mm_proto::v1::media_job_service_server::MediaJobService;
use tonic::metadata::MetadataMap;
use tonic::{Request, Response, Status};

pub const AUTHORIZATION: &str = "authorization";
pub const TRACE_ID: &str = "x-trace-id";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trace(TraceId);

impl Trace {
    pub fn id(self) -> TraceId {
        self.0
    }

    pub fn as_ref(&self) -> Option<&TraceId> {
        Some(&self.0)
    }
}

fn read_trace(metadata: &MetadataMap) -> TraceId {
    metadata
        .get(TRACE_ID)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| TraceId::from_str(value).ok())
        .unwrap_or_else(TraceId::generate)
}

pub trait DeprecatedOwner {
    fn deprecated_owner(&self) -> &str;
}

macro_rules! deprecated_owner {
    ($($message:ty),*) => {
        $(#[allow(deprecated)]
        impl DeprecatedOwner for $message {
            fn deprecated_owner(&self) -> &str {
                &self.user_id
            }
        })*
    };
}

deprecated_owner!(
    v1::CreateJobRequest,
    v1::GetJobRequest,
    v1::CancelJobRequest,
    v1::ListJobsRequest,
    v1::WatchJobRequest
);

#[derive(Clone)]
pub enum Auth {
    Open,
    Verified(Verifier),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    subject: String,
    verified: bool,
}

impl Owner {
    pub fn from_token(identity: Identity) -> Self {
        Owner {
            subject: identity.subject().to_string(),
            verified: true,
        }
    }

    pub fn from_deprecated_field(subject: &str) -> Self {
        Owner {
            subject: subject.to_string(),
            verified: false,
        }
    }

    pub fn is_verified(&self) -> bool {
        self.verified
    }

    pub fn subject(&self) -> &str {
        &self.subject
    }

    pub fn user_id(&self) -> Result<UserId, Status> {
        UserId::from_str(self.subject()).map_err(|error| {
            if self.verified {
                Status::unauthenticated(format!("the token subject is not a usable owner: {error}"))
            } else {
                Status::invalid_argument(format!("user_id is invalid: {error}"))
            }
        })
    }
}

pub(crate) fn owner_of<T>(request: &Request<T>) -> Result<UserId, Status> {
    request
        .extensions()
        .get::<Owner>()
        .ok_or_else(|| Status::unauthenticated("the call carries no identity"))?
        .user_id()
}

pub(crate) fn trace_of<T>(request: &Request<T>) -> TraceId {
    request
        .extensions()
        .get::<Trace>()
        .map(|trace| trace.id())
        .unwrap_or_else(TraceId::generate)
}

impl Auth {
    pub fn open() -> Self {
        Auth::Open
    }

    pub fn verifying(verifier: Verifier) -> Self {
        Auth::Verified(verifier)
    }

    pub fn verifies_tokens(&self) -> bool {
        matches!(self, Auth::Verified(_))
    }

    pub fn verify(&self, token: &str) -> Result<Identity, Status> {
        match self {
            Auth::Open => Err(Status::unauthenticated(
                "a token was offered and this service publishes no key to verify it with",
            )),
            Auth::Verified(verifier) => {
                verifier.verify(token, SystemTime::now()).map_err(|error| {
                    Status::unauthenticated(format!("the token was refused: {error}"))
                })
            }
        }
    }

    fn owner(&self, metadata: &MetadataMap, deprecated_owner: &str) -> Result<Owner, Status> {
        let Some(offered) = metadata.get(AUTHORIZATION) else {
            return self.deprecated_owner(deprecated_owner);
        };
        let offered = offered
            .to_str()
            .map_err(|_| Status::unauthenticated("the authorization header is not readable"))?;
        let offered =
            bearer_token(offered).map_err(|error| Status::unauthenticated(error.to_string()))?;
        self.verify(offered).map(Owner::from_token)
    }

    fn deprecated_owner(&self, deprecated_owner: &str) -> Result<Owner, Status> {
        match self {
            Auth::Open => Ok(Owner::from_deprecated_field(deprecated_owner)),
            Auth::Verified(_) => Err(Status::unauthenticated("the call carries no bearer token")),
        }
    }
}

pub struct Interceptor<T> {
    auth: Auth,
    inner: T,
}

impl<T> Interceptor<T> {
    pub fn new(auth: Auth, inner: T) -> Self {
        Self { auth, inner }
    }

    fn identify<TMessage: DeprecatedOwner>(
        &self,
        request: Request<TMessage>,
        method: &str,
    ) -> Result<Request<TMessage>, Status> {
        let deprecated_owner = request.get_ref().deprecated_owner().to_string();
        let (metadata, mut extensions, message) = request.into_parts();
        let owner = self.auth.owner(&metadata, &deprecated_owner)?;
        let trace = Trace(read_trace(&metadata));
        record(
            Component::Server,
            trace.as_ref(),
            "rpc.received",
            &[
                ("method", method.to_string()),
                (
                    "owner",
                    match owner.user_id() {
                        Ok(owner) => owner.to_string(),
                        Err(status) => format!("refused:{:?}", status.code()),
                    },
                ),
            ],
        );
        extensions.insert(owner);
        extensions.insert(trace);
        Ok(Request::from_parts(metadata, extensions, message))
    }
}

#[tonic::async_trait]
impl<T: MediaJobService> MediaJobService for Interceptor<T> {
    async fn create_job(
        &self,
        request: Request<v1::CreateJobRequest>,
    ) -> Result<Response<v1::CreateJobResponse>, Status> {
        self.inner
            .create_job(self.identify(request, "create_job")?)
            .await
    }

    async fn get_job(
        &self,
        request: Request<v1::GetJobRequest>,
    ) -> Result<Response<v1::GetJobResponse>, Status> {
        self.inner.get_job(self.identify(request, "get_job")?).await
    }

    async fn cancel_job(
        &self,
        request: Request<v1::CancelJobRequest>,
    ) -> Result<Response<v1::CancelJobResponse>, Status> {
        self.inner
            .cancel_job(self.identify(request, "cancel_job")?)
            .await
    }

    async fn list_jobs(
        &self,
        request: Request<v1::ListJobsRequest>,
    ) -> Result<Response<v1::ListJobsResponse>, Status> {
        self.inner
            .list_jobs(self.identify(request, "list_jobs")?)
            .await
    }

    type WatchJobStream = T::WatchJobStream;

    async fn watch_job(
        &self,
        request: Request<v1::WatchJobRequest>,
    ) -> Result<Response<Self::WatchJobStream>, Status> {
        self.inner
            .watch_job(self.identify(request, "watch_job")?)
            .await
    }
}

impl Auth {
    pub fn from_settings(
        jwks: Option<&str>,
        settings: Settings,
    ) -> Result<Self, mm_auth::KeySetError> {
        match jwks {
            None => Ok(Auth::Open),
            Some(document) => Ok(Auth::verifying(Verifier::from_jwks(document, settings)?)),
        }
    }
}

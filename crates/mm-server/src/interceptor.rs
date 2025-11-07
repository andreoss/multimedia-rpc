use std::str::FromStr;
use std::time::SystemTime;

use mm_auth::{Identity, Settings, Verifier, bearer_token};
use mm_domain::UserId;
use mm_proto::v1;
use mm_proto::v1::media_job_service_server::MediaJobService;
use tonic::metadata::MetadataMap;
use tonic::{Request, Response, Status};

pub const AUTHORIZATION: &str = "authorization";

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
    ) -> Result<Request<TMessage>, Status> {
        let deprecated_owner = request.get_ref().deprecated_owner().to_string();
        let (metadata, mut extensions, message) = request.into_parts();
        extensions.insert(self.auth.owner(&metadata, &deprecated_owner)?);
        Ok(Request::from_parts(metadata, extensions, message))
    }
}

#[tonic::async_trait]
impl<T: MediaJobService> MediaJobService for Interceptor<T> {
    async fn create_job(
        &self,
        request: Request<v1::CreateJobRequest>,
    ) -> Result<Response<v1::CreateJobResponse>, Status> {
        self.inner.create_job(self.identify(request)?).await
    }

    async fn get_job(
        &self,
        request: Request<v1::GetJobRequest>,
    ) -> Result<Response<v1::GetJobResponse>, Status> {
        self.inner.get_job(self.identify(request)?).await
    }

    async fn cancel_job(
        &self,
        request: Request<v1::CancelJobRequest>,
    ) -> Result<Response<v1::CancelJobResponse>, Status> {
        self.inner.cancel_job(self.identify(request)?).await
    }

    async fn list_jobs(
        &self,
        request: Request<v1::ListJobsRequest>,
    ) -> Result<Response<v1::ListJobsResponse>, Status> {
        self.inner.list_jobs(self.identify(request)?).await
    }

    type WatchJobStream = T::WatchJobStream;

    async fn watch_job(
        &self,
        request: Request<v1::WatchJobRequest>,
    ) -> Result<Response<Self::WatchJobStream>, Status> {
        self.inner.watch_job(self.identify(request)?).await
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

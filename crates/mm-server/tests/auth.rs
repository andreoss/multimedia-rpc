#![allow(deprecated)]
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use mm_auth::testing::{Issuer, tampered_signature};
use mm_auth::{Settings, Verifier};
use mm_proto::v1;
use mm_proto::v1::media_job_service_server::MediaJobService;
use mm_server::{Auth, Interceptor};
use tonic::metadata::MetadataValue;
use tonic::{Code, Request, Response, Status};

const JOB_ID: &str = "018f0000-0000-7000-8000-000000000001";
const DEPRECATED_FIELD: &str = "body-owner";

type Seen = Arc<Mutex<Vec<(String, String)>>>;

struct Recorder {
    seen: Seen,
}

impl Recorder {
    fn new() -> Self {
        Recorder {
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn record(&self, method: &str, request: &Request<impl Sized>) {
        let owner = match request.extensions().get::<mm_server::Owner>() {
            Some(owner) => match owner.user_id() {
                Ok(owner) => owner.to_string(),
                Err(status) => format!("refused:{:?}", status.code()),
            },
            None => "<no identity>".to_string(),
        };
        self.seen
            .lock()
            .expect("recorder lock")
            .push((method.to_string(), owner));
    }
}

#[tonic::async_trait]
impl MediaJobService for Recorder {
    async fn create_job(
        &self,
        request: Request<v1::CreateJobRequest>,
    ) -> Result<Response<v1::CreateJobResponse>, Status> {
        self.record("create_job", &request);
        Ok(Response::new(v1::CreateJobResponse::default()))
    }

    async fn get_job(
        &self,
        request: Request<v1::GetJobRequest>,
    ) -> Result<Response<v1::GetJobResponse>, Status> {
        self.record("get_job", &request);
        Ok(Response::new(v1::GetJobResponse::default()))
    }

    async fn cancel_job(
        &self,
        request: Request<v1::CancelJobRequest>,
    ) -> Result<Response<v1::CancelJobResponse>, Status> {
        self.record("cancel_job", &request);
        Ok(Response::new(v1::CancelJobResponse::default()))
    }

    async fn list_jobs(
        &self,
        request: Request<v1::ListJobsRequest>,
    ) -> Result<Response<v1::ListJobsResponse>, Status> {
        self.record("list_jobs", &request);
        Ok(Response::new(v1::ListJobsResponse::default()))
    }

    type WatchJobStream = std::pin::Pin<
        Box<dyn tokio_stream::Stream<Item = Result<v1::WatchJobResponse, Status>> + Send>,
    >;

    async fn watch_job(
        &self,
        request: Request<v1::WatchJobRequest>,
    ) -> Result<Response<Self::WatchJobStream>, Status> {
        self.record("watch_job", &request);
        Ok(Response::new(Box::pin(futures_util::stream::empty())))
    }
}

fn with_token<T>(message: T, token: Option<&str>) -> Request<T> {
    let mut request = Request::new(message);
    if let Some(token) = token {
        let value: MetadataValue<_> = token.parse().expect("metadata value is ascii");
        request.metadata_mut().insert("authorization", value);
    }
    request
}

fn get_request(token: Option<&str>, body_owner: &str) -> Request<v1::GetJobRequest> {
    with_token(
        v1::GetJobRequest {
            user_id: body_owner.to_string(),
            job_id: JOB_ID.to_string(),
        },
        token,
    )
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

fn live_token(issuer: &Issuer, subject: &str) -> String {
    bearer(&issuer.token(subject, SystemTime::now(), Duration::from_secs(300)))
}

fn open() -> (Interceptor<Recorder>, Seen) {
    let recorder = Recorder::new();
    let seen = Arc::clone(&recorder.seen);
    (Interceptor::new(Auth::open(), recorder), seen)
}

fn verifying(issuer: &Issuer) -> (Interceptor<Recorder>, Seen) {
    let verifier =
        Verifier::from_jwks(&issuer.jwks(), Settings::none()).expect("the published key set loads");
    let recorder = Recorder::new();
    let seen = Arc::clone(&recorder.seen);
    (Interceptor::new(Auth::verifying(verifier), recorder), seen)
}

fn owners(seen: &Seen) -> Vec<(String, String)> {
    seen.lock().expect("recorder lock").clone()
}

fn reached_handlers(seen: &Seen) -> usize {
    seen.lock().expect("recorder lock").len()
}

#[tokio::test]
async fn an_open_server_takes_the_owner_from_the_deprecated_field() {
    let (interceptor, seen) = open();
    interceptor
        .get_job(get_request(None, DEPRECATED_FIELD))
        .await
        .expect("the call reaches the handler");
    assert_eq!(
        owners(&seen),
        vec![("get_job".to_string(), DEPRECATED_FIELD.to_string())]
    );
}

#[tokio::test]
async fn a_verified_subject_is_authoritative_over_the_deprecated_field() {
    let issuer = Issuer::seeded("key-1", 7);
    let (interceptor, seen) = verifying(&issuer);
    let token = live_token(&issuer, "subject-owner");
    interceptor
        .get_job(get_request(Some(&token), DEPRECATED_FIELD))
        .await
        .expect("the call reaches the handler");
    assert_eq!(
        owners(&seen),
        vec![("get_job".to_string(), "subject-owner".to_string())]
    );
}

#[tokio::test]
async fn every_path_reads_the_owner_from_the_attached_identity() {
    let issuer = Issuer::seeded("key-1", 7);
    let (interceptor, seen) = verifying(&issuer);
    let token = live_token(&issuer, "subject-owner");

    interceptor
        .create_job(with_token(
            v1::CreateJobRequest {
                user_id: DEPRECATED_FIELD.to_string(),
                idempotency_key: "request-1".to_string(),
                ..Default::default()
            },
            Some(&token),
        ))
        .await
        .expect("create reaches the handler");
    interceptor
        .get_job(get_request(Some(&token), DEPRECATED_FIELD))
        .await
        .expect("get reaches the handler");
    interceptor
        .cancel_job(with_token(
            v1::CancelJobRequest {
                user_id: DEPRECATED_FIELD.to_string(),
                job_id: JOB_ID.to_string(),
            },
            Some(&token),
        ))
        .await
        .expect("cancel reaches the handler");
    interceptor
        .list_jobs(with_token(
            v1::ListJobsRequest {
                user_id: DEPRECATED_FIELD.to_string(),
                ..Default::default()
            },
            Some(&token),
        ))
        .await
        .expect("list reaches the handler");
    interceptor
        .watch_job(with_token(
            v1::WatchJobRequest {
                user_id: DEPRECATED_FIELD.to_string(),
                job_id: JOB_ID.to_string(),
            },
            Some(&token),
        ))
        .await
        .expect("watch reaches the handler");

    assert_eq!(
        owners(&seen)
            .into_iter()
            .map(|(method, owner)| format!("{method}={owner}"))
            .collect::<Vec<String>>(),
        vec![
            "create_job=subject-owner",
            "get_job=subject-owner",
            "cancel_job=subject-owner",
            "list_jobs=subject-owner",
            "watch_job=subject-owner",
        ]
    );
}

#[tokio::test]
async fn a_server_with_a_verifier_rejects_an_unauthenticated_call() {
    let issuer = Issuer::seeded("key-1", 7);
    let (interceptor, seen) = verifying(&issuer);
    let status = interceptor
        .get_job(get_request(None, DEPRECATED_FIELD))
        .await
        .expect_err("an unauthenticated call reaches a handler");
    assert_eq!(status.code(), Code::Unauthenticated);
    assert_eq!(reached_handlers(&seen), 0);
}

#[tokio::test]
async fn a_forged_signature_is_rejected() {
    let issuer = Issuer::seeded("key-1", 7);
    let (interceptor, seen) = verifying(&issuer);
    let forged = bearer(&tampered_signature(&issuer, "owner-1"));
    let status = interceptor
        .get_job(get_request(Some(&forged), DEPRECATED_FIELD))
        .await
        .expect_err("a forged token reaches a handler");
    assert_eq!(status.code(), Code::Unauthenticated);
    assert_eq!(reached_handlers(&seen), 0);
}

#[tokio::test]
async fn a_signature_from_another_key_is_rejected() {
    let publisher = Issuer::seeded("key-1", 7);
    let other = Issuer::seeded("key-1", 9);
    let (interceptor, _) = verifying(&publisher);
    let token = live_token(&other, "owner-1");
    let status = interceptor
        .get_job(get_request(Some(&token), DEPRECATED_FIELD))
        .await
        .expect_err("a foreign token reaches a handler");
    assert_eq!(status.code(), Code::Unauthenticated);
}

#[tokio::test]
async fn a_symmetric_token_is_rejected() {
    let issuer = Issuer::seeded("key-1", 7);
    let (interceptor, _) = verifying(&issuer);
    let token =
        bearer(&issuer.symmetric_token("owner-1", SystemTime::now(), Duration::from_secs(300)));
    let status = interceptor
        .get_job(get_request(Some(&token), DEPRECATED_FIELD))
        .await
        .expect_err("a shared-secret token reaches a handler");
    assert_eq!(status.code(), Code::Unauthenticated);
}

#[tokio::test]
async fn an_expired_token_is_rejected() {
    let issuer = Issuer::seeded("key-1", 7);
    let (interceptor, _) = verifying(&issuer);
    let token = bearer(&issuer.token(
        "owner-1",
        SystemTime::now() - Duration::from_secs(3600),
        Duration::from_secs(60),
    ));
    let status = interceptor
        .get_job(get_request(Some(&token), DEPRECATED_FIELD))
        .await
        .expect_err("an expired token reaches a handler");
    assert_eq!(status.code(), Code::Unauthenticated);
}

#[tokio::test]
async fn a_token_that_is_not_yet_valid_is_rejected() {
    let issuer = Issuer::seeded("key-1", 7);
    let (interceptor, _) = verifying(&issuer);
    let token = bearer(&issuer.token(
        "owner-1",
        SystemTime::now() + Duration::from_secs(3600),
        Duration::from_secs(300),
    ));
    let status = interceptor
        .get_job(get_request(Some(&token), DEPRECATED_FIELD))
        .await
        .expect_err("a future token reaches a handler");
    assert_eq!(status.code(), Code::Unauthenticated);
}

#[tokio::test]
async fn a_token_naming_an_unknown_key_is_rejected() {
    let issuer = Issuer::seeded("key-1", 7);
    let other = Issuer::seeded("key-2", 7);
    let (interceptor, _) = verifying(&issuer);
    let token = live_token(&other, "owner-1");
    let status = interceptor
        .get_job(get_request(Some(&token), DEPRECATED_FIELD))
        .await
        .expect_err("a token from an unpublished key reaches a handler");
    assert_eq!(status.code(), Code::Unauthenticated);
}

#[tokio::test]
async fn a_scheme_other_than_bearer_is_rejected() {
    let issuer = Issuer::seeded("key-1", 7);
    let (interceptor, _) = verifying(&issuer);
    let basic = format!("Basic {}", live_token(&issuer, "owner-1"));
    let status = interceptor
        .get_job(get_request(Some(&basic), DEPRECATED_FIELD))
        .await
        .expect_err("a non-bearer scheme reaches a handler");
    assert_eq!(status.code(), Code::Unauthenticated);
}

#[tokio::test]
async fn a_token_naming_a_subject_that_is_not_an_owner_is_refused() {
    let issuer = Issuer::seeded("key-1", 7);
    let (interceptor, seen) = verifying(&issuer);
    let token = live_token(&issuer, "owner one");
    interceptor
        .get_job(get_request(Some(&token), DEPRECATED_FIELD))
        .await
        .expect("the token verifies and the handler refuses the subject");
    assert_eq!(
        owners(&seen),
        vec![("get_job".to_string(), "refused:Unauthenticated".to_string())]
    );
}

#[tokio::test]
async fn an_open_server_keeps_answering_an_unusable_owner_as_an_argument_error() {
    let (interceptor, seen) = open();
    interceptor
        .get_job(get_request(None, "owner one"))
        .await
        .expect("the handler refuses the unusable owner");
    assert_eq!(
        owners(&seen),
        vec![("get_job".to_string(), "refused:InvalidArgument".to_string())]
    );
}

#[tokio::test]
async fn a_token_cannot_be_verified_by_a_server_with_no_published_key() {
    let issuer = Issuer::seeded("key-1", 7);
    let (interceptor, _) = open();
    let token = live_token(&issuer, "owner-1");
    let status = interceptor
        .get_job(get_request(Some(&token), DEPRECATED_FIELD))
        .await
        .expect_err("an unverifiable token reaches a handler");
    assert_eq!(status.code(), Code::Unauthenticated);
}

#[test]
fn the_open_policy_is_reported_as_the_one_that_takes_no_token() {
    assert!(!Auth::open().verifies_tokens());
    let issuer = Issuer::seeded("key-1", 7);
    let verifier =
        Verifier::from_jwks(&issuer.jwks(), Settings::none()).expect("the published key set loads");
    assert!(Auth::verifying(verifier).verifies_tokens());
}

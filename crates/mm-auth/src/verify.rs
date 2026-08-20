use std::time::{Duration, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::signature::Verifier as SignatureVerifier;
use p256::ecdsa::{Signature, VerifyingKey};
use serde::Deserialize;
use thiserror::Error;

use crate::claims::{Claims, Subject};
use crate::keys::KeySet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum AuthError {
    #[error("no token was offered")]
    Missing,
    #[error("the authorization header does not carry a bearer token")]
    UnsupportedScheme,
    #[error("the token is not readable")]
    Malformed,
    #[error("the token asks for an algorithm this service does not accept")]
    UnsupportedAlgorithm,
    #[error("the token names a key the issuer has not published")]
    UnknownKey,
    #[error("the token is not signed by the published key")]
    BadSignature,
    #[error("the token expired")]
    Expired,
    #[error("the token carries no expiry")]
    ExpiryMissing,
    #[error("the token is not valid yet")]
    NotYetValid,
    #[error("the token was issued by another issuer")]
    IssuerMismatch,
    #[error("the token carries no subject")]
    SubjectMissing,
}

#[derive(Debug, Deserialize)]
struct Header {
    alg: String,
    #[serde(default)]
    kid: String,
}

#[derive(Debug, Clone)]
pub struct Identity {
    subject: Subject,
}

impl Identity {
    pub fn from_subject(subject: Subject) -> Self {
        Self { subject }
    }

    pub fn subject(&self) -> &str {
        self.subject.as_str()
    }
}

#[derive(Debug, Clone, Default)]
pub struct Settings {
    issuer: Option<String>,
    leeway: Duration,
}

impl Settings {
    pub fn none() -> Self {
        Settings::default()
    }

    pub fn issued_by(mut self, issuer: &str) -> Self {
        self.issuer = Some(issuer.to_string());
        self
    }

    pub fn with_leeway(mut self, leeway: Duration) -> Self {
        self.leeway = leeway;
        self
    }
}

#[derive(Debug, Clone)]
pub struct Verifier {
    keys: KeySet,
    issuer: Option<String>,
    leeway: Duration,
}

impl Verifier {
    pub fn from_jwks(document: &str, settings: Settings) -> Result<Self, crate::KeySetError> {
        Ok(Verifier {
            keys: KeySet::from_published(document)?,
            issuer: settings.issuer,
            leeway: settings.leeway,
        })
    }

    pub fn keys(&self) -> &KeySet {
        &self.keys
    }

    pub fn verify(&self, token: &str, now: SystemTime) -> Result<Identity, AuthError> {
        let parts: Vec<&str> = token.split('.').collect();
        let [header, payload, signature] = parts[..] else {
            return Err(AuthError::Malformed);
        };
        let signing_input = format!("{header}.{payload}");
        let header: Header = decode(header)?;
        if header.alg != "ES256" {
            return Err(AuthError::UnsupportedAlgorithm);
        }
        let key: &VerifyingKey = self.keys.get(&header.kid).ok_or(AuthError::UnknownKey)?;
        let bytes = URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| AuthError::Malformed)?;
        let signature = Signature::from_slice(&bytes).map_err(|_| AuthError::Malformed)?;
        key.verify(signing_input.as_bytes(), &signature)
            .map_err(|_| AuthError::BadSignature)?;
        let claims: Claims = decode(payload)?;
        self.check_times(&claims, now)?;
        if let Some(expected) = &self.issuer
            && claims.issuer.as_deref() != Some(expected.as_str())
        {
            return Err(AuthError::IssuerMismatch);
        }
        let subject = claims.subject.ok_or(AuthError::SubjectMissing)?;
        Ok(Identity { subject })
    }

    fn check_times(&self, claims: &Claims, now: SystemTime) -> Result<(), AuthError> {
        let seconds = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs() as i64)
            .unwrap_or_default();
        let leeway = self.leeway.as_secs() as i64;
        let expiry = claims.expires_at.ok_or(AuthError::ExpiryMissing)?;
        if seconds >= expiry + leeway {
            return Err(AuthError::Expired);
        }
        if let Some(not_before) = claims.not_before
            && seconds + leeway < not_before
        {
            return Err(AuthError::NotYetValid);
        }
        Ok(())
    }
}

fn decode<T: serde::de::DeserializeOwned>(part: &str) -> Result<T, AuthError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(part)
        .map_err(|_| AuthError::Malformed)?;
    serde_json::from_slice(&bytes).map_err(|_| AuthError::Malformed)
}

pub fn bearer_token(header: &str) -> Result<&str, AuthError> {
    let (scheme, token) = header.split_once(' ').ok_or(AuthError::UnsupportedScheme)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err(AuthError::UnsupportedScheme);
    }
    let token = token.trim();
    if token.is_empty() {
        return Err(AuthError::UnsupportedScheme);
    }
    Ok(token)
}

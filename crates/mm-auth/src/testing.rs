use std::time::{Duration, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};

pub struct Issuer {
    pub kid: String,
    pub signing: SigningKey,
}

impl Issuer {
    pub fn seeded(kid: &str, seed: u8) -> Self {
        Issuer {
            kid: kid.to_string(),
            signing: SigningKey::from_slice(&[seed; 32]).expect("a 32-byte seed is a valid scalar"),
        }
    }

    pub fn jwks(&self) -> String {
        let point = self.signing.verifying_key().to_encoded_point(false);
        let bytes = point.as_bytes();
        format!(
            "{{\"keys\":[{{\"kty\":\"EC\",\"crv\":\"P-256\",\"alg\":\"ES256\",\"use\":\"sig\",\"kid\":\"{}\",\"x\":\"{}\",\"y\":\"{}\"}}]}}",
            self.kid,
            URL_SAFE_NO_PAD.encode(&bytes[1..33]),
            URL_SAFE_NO_PAD.encode(&bytes[33..])
        )
    }

    pub fn token(&self, subject: &str, issued_at: SystemTime, lifetime: Duration) -> String {
        let issued = unix(issued_at);
        self.token_with_claims(&format!(
            "{{\"sub\":\"{subject}\",\"iss\":\"test-issuer\",\"iat\":{issued},\"nbf\":{issued},\"exp\":{}}}",
            issued + lifetime.as_secs() as i64
        ))
    }

    pub fn token_with_claims(&self, claims: &str) -> String {
        self.sign(&format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(format!(
                "{{\"alg\":\"ES256\",\"typ\":\"JWT\",\"kid\":\"{}\"}}",
                self.kid
            )),
            URL_SAFE_NO_PAD.encode(claims)
        ))
    }

    pub fn symmetric_token(
        &self,
        subject: &str,
        issued_at: SystemTime,
        lifetime: Duration,
    ) -> String {
        let issued = unix(issued_at);
        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode("{\"alg\":\"HS256\",\"typ\":\"JWT\"}"),
            URL_SAFE_NO_PAD.encode(format!(
                "{{\"sub\":\"{subject}\",\"iat\":{issued},\"exp\":{}}}",
                issued + lifetime.as_secs() as i64
            ))
        );
        let mut shared = [0u8; 64];
        for (index, slot) in shared.iter_mut().enumerate().take(32) {
            *slot = self.signing.to_bytes()[index];
        }
        format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(shared))
    }

    pub fn sign(&self, signing_input: &str) -> String {
        let signature: Signature = self.signing.sign(signing_input.as_bytes());
        format!(
            "{}.{}",
            signing_input,
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        )
    }
}

pub fn tampered_signature(issuer: &Issuer, subject: &str) -> String {
    let signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(format!("{{\"alg\":\"ES256\",\"kid\":\"{}\"}}", issuer.kid)),
        URL_SAFE_NO_PAD.encode(format!("{{\"sub\":\"{subject}\"}}"))
    );
    let mut signature = [0u8; 64];
    for (index, slot) in signature.iter_mut().enumerate() {
        *slot = (index as u8).wrapping_add(1);
    }
    format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature))
}

pub fn unix(time: SystemTime) -> i64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .expect("time after epoch")
        .as_secs() as i64
}

pub fn fixed_now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

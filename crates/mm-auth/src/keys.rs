use std::collections::HashMap;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::VerifyingKey;
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum KeySetError {
    #[error("the published key document is not readable: {0}")]
    Unreadable(String),
    #[error("the published key document holds no keys")]
    Empty,
    #[error("a published key cannot verify a signature: {kind} {kid}")]
    UnsupportedKey { kind: String, kid: String },
    #[error("a published key is not usable: {0}")]
    Invalid(String),
}

#[derive(Debug, Deserialize)]
struct Document {
    keys: Vec<Published>,
}

#[derive(Debug, Deserialize)]
struct Published {
    #[serde(default)]
    kid: String,
    #[serde(rename = "kty", default)]
    kind: String,
    #[serde(default)]
    crv: String,
    #[serde(default)]
    alg: String,
    x: Option<String>,
    y: Option<String>,
}

#[derive(Debug, Clone)]
pub struct KeySet {
    keys: HashMap<String, VerifyingKey>,
}

impl KeySet {
    pub fn from_published(document: &str) -> Result<Self, KeySetError> {
        let document: Document = serde_json::from_str(document)
            .map_err(|error| KeySetError::Unreadable(error.to_string()))?;
        let mut keys = HashMap::new();
        for published in document.keys {
            let supported = published.kind == "EC"
                && published.crv == "P-256"
                && (published.alg.is_empty() || published.alg == "ES256");
            if !supported {
                return Err(KeySetError::UnsupportedKey {
                    kind: published.kind,
                    kid: published.kid,
                });
            }
            let (Some(x), Some(y)) = (published.x, published.y) else {
                return Err(KeySetError::Invalid(format!(
                    "key {} carries no curve point",
                    published.kid
                )));
            };
            let mut point = [0u8; 65];
            point[0] = 0x04;
            decode(&x, &mut point[1..33])?;
            decode(&y, &mut point[33..])?;
            let kid = published.kid;
            let key = VerifyingKey::from_sec1_bytes(&point)
                .map_err(|error| KeySetError::Invalid(error.to_string()))?;
            if keys.insert(kid.clone(), key).is_some() {
                return Err(KeySetError::Invalid(format!(
                    "key id {kid} is published twice"
                )));
            }
        }
        if keys.is_empty() {
            return Err(KeySetError::Empty);
        }
        Ok(KeySet { keys })
    }

    pub fn get(&self, kid: &str) -> Option<&VerifyingKey> {
        self.keys.get(kid)
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

fn decode(encoded: &str, out: &mut [u8]) -> Result<(), KeySetError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|error| KeySetError::Invalid(error.to_string()))?;
    if bytes.len() != out.len() {
        return Err(KeySetError::Invalid(format!(
            "a curve coordinate is {} bytes, expected {}",
            bytes.len(),
            out.len()
        )));
    }
    out.copy_from_slice(&bytes);
    Ok(())
}

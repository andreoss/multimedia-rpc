use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Claims {
    #[serde(rename = "sub", default)]
    pub subject: Option<Subject>,
    #[serde(rename = "iss", default)]
    pub issuer: Option<String>,
    #[serde(rename = "exp", default)]
    pub expires_at: Option<i64>,
    #[serde(rename = "nbf", default)]
    pub not_before: Option<i64>,
    #[serde(rename = "iat", default)]
    pub issued_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject(String);

impl Subject {
    pub fn parse(value: &str) -> Option<Self> {
        if value.is_empty() || value.len() > 128 || value.trim() != value {
            return None;
        }
        if value.chars().any(|character| character.is_control()) {
            return None;
        }
        Some(Subject(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Subject {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Subject::parse(&raw).ok_or_else(|| serde::de::Error::custom("the subject is not usable"))
    }
}

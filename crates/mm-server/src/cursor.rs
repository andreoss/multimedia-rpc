use std::time::{Duration, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use mm_domain::{JobId, PageCursor};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("page token is invalid")]
pub(crate) struct CursorError;

pub(crate) fn encode(cursor: &PageCursor) -> String {
    let micros = cursor
        .created_at
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_micros();
    URL_SAFE_NO_PAD.encode(format!("{micros}:{}", cursor.id))
}

pub(crate) fn decode(token: &str) -> Result<PageCursor, CursorError> {
    let bytes = URL_SAFE_NO_PAD.decode(token).map_err(|_| CursorError)?;
    let text = String::from_utf8(bytes).map_err(|_| CursorError)?;
    let (micros, id) = text.split_once(':').ok_or(CursorError)?;
    let micros: u64 = micros.parse().map_err(|_| CursorError)?;
    let id: JobId = id.parse().map_err(|_| CursorError)?;
    Ok(PageCursor {
        created_at: SystemTime::UNIX_EPOCH + Duration::from_micros(micros),
        id,
    })
}

#[cfg(test)]
mod tests {
    use super::{CursorError, decode, encode};
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use mm_domain::{JobId, PageCursor};
    use std::time::{Duration, SystemTime};

    fn cursor() -> PageCursor {
        PageCursor {
            created_at: SystemTime::UNIX_EPOCH + Duration::from_micros(1_726_000_000_123_456),
            id: JobId::generate(),
        }
    }

    #[test]
    fn a_cursor_roundtrips() {
        let original = cursor();
        let decoded = decode(&encode(&original)).expect("token decodes");
        assert_eq!(decoded, original);
    }

    #[test]
    fn the_token_is_opaque_base64url() {
        let token = encode(&cursor());
        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn garbage_tokens_are_rejected() {
        for token in [
            "not base64!",
            "////",
            &URL_SAFE_NO_PAD.encode("no-separator"),
            &URL_SAFE_NO_PAD.encode("abc:def"),
            &URL_SAFE_NO_PAD.encode("12:not-a-uuid"),
            &URL_SAFE_NO_PAD.encode("12:00000000-0000-0000-0000-000000000000"),
            "",
        ] {
            assert_eq!(decode(token), Err(CursorError), "token {token:?}");
        }
    }

    #[test]
    fn a_pre_epoch_timestamp_still_encodes() {
        let original = PageCursor {
            created_at: SystemTime::UNIX_EPOCH - Duration::from_secs(1),
            id: JobId::generate(),
        };
        let decoded = decode(&encode(&original)).expect("token decodes");
        assert_eq!(decoded.created_at, SystemTime::UNIX_EPOCH);
    }

    #[test]
    fn use_of_base64_engine_is_url_safe() {
        let token = encode(&cursor());
        assert!(!token.contains('='));
        assert!(!token.contains('+'));
        assert!(!token.contains('/'));
    }
}

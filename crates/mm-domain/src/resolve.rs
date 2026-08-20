use std::time::SystemTime;

use thiserror::Error;

use crate::op::MediaRef;
use crate::store::{ObjectStore, ObjectStoreError};

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("job deadline has already passed")]
    DeadlinePassed,
    #[error(transparent)]
    Store(#[from] ObjectStoreError),
}

pub async fn resolve_media_ref<S: ObjectStore + ?Sized>(
    store: &S,
    media: &MediaRef,
    deadline: SystemTime,
    now: SystemTime,
) -> Result<String, ResolveError> {
    match media {
        MediaRef::Http(http) => Ok(http.url().to_string()),
        MediaRef::Object(object) => {
            let expires_in = deadline
                .duration_since(now)
                .map_err(|_| ResolveError::DeadlinePassed)?;
            if expires_in.is_zero() {
                return Err(ResolveError::DeadlinePassed);
            }
            Ok(store.presign_get(object, expires_in).await?)
        }
    }
}

use chrono::{DateTime, Duration, Utc};
use dashmap::DashMap;
use sqlx::SqlitePool;
use std::sync::Arc;
use uuid::Uuid;

use crate::{db, sdks};

#[derive(Clone)]
struct PendingDeviceAuth {
    device_code: String,
    expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollStatus {
    Pending,
    Authorized,
    Expired,
    Denied,
}

/// Maps a Trakt `/oauth/device/token` error response to a poll outcome.
/// Returns `None` when the error should be propagated as a hard failure
/// (e.g. a transport error, or an unrecognized status code).
fn classify_poll_error(err: &sdks::ClientError) -> Option<PollStatus> {
    match err {
        sdks::ClientError::RateLimited { .. } => Some(PollStatus::Pending),
        sdks::ClientError::Http { status: 400, .. } => Some(PollStatus::Pending),
        sdks::ClientError::Http { status: 410, .. } => Some(PollStatus::Expired),
        sdks::ClientError::Http { status: 418, .. } => Some(PollStatus::Denied),
        _ => None,
    }
}

#[derive(Clone)]
pub struct TraktAuthService {
    pending: Arc<DashMap<Uuid, PendingDeviceAuth>>,
}

impl TraktAuthService {
    pub fn new() -> Self {
        Self {
            pending: Arc::new(DashMap::new()),
        }
    }

    pub async fn start(
        &self,
        db: &SqlitePool,
        trakt_base_url: &str,
        user_id: Uuid,
    ) -> anyhow::Result<sdks::trakt::DeviceCodeResponse> {
        let cfg = db::Settings::get_config(db).await?;
        let client_id = cfg
            .trakt_client_id
            .filter(|k| !k.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Trakt client_id is not configured"))?;

        let client = sdks::RestClient::new(trakt_base_url)?
            .with_auth(sdks::trakt::TraktOAuthAuth);
        let resp = client
            .execute(sdks::trakt::DeviceCodeEndpoint { client_id })
            .await?;

        self.pending
            .insert(
                user_id,
                PendingDeviceAuth {
                    device_code: resp
                        .device_code
                        .clone(),
                    expires_at: Utc::now() + Duration::seconds(resp.expires_in),
                },
            );
        Ok(resp)
    }

    pub async fn poll(
        &self,
        db: &SqlitePool,
        trakt_base_url: &str,
        user_id: Uuid,
    ) -> anyhow::Result<PollStatus> {
        let Some(pending) = self
            .pending
            .get(&user_id)
            .map(|e| e.clone())
        else {
            return Ok(PollStatus::Expired);
        };
        if Utc::now() > pending.expires_at {
            self.pending
                .remove(&user_id);
            return Ok(PollStatus::Expired);
        }

        let cfg = db::Settings::get_config(db).await?;
        let (Some(client_id), Some(client_secret)) =
            (cfg.trakt_client_id, cfg.trakt_client_secret)
        else {
            self.pending
                .remove(&user_id);
            anyhow::bail!("Trakt client_id/client_secret is not configured");
        };

        let client = sdks::RestClient::new(trakt_base_url)?
            .with_auth(sdks::trakt::TraktOAuthAuth);
        let result = client
            .execute(sdks::trakt::DeviceTokenEndpoint {
                client_id,
                client_secret,
                device_code: pending
                    .device_code
                    .clone(),
            })
            .await;

        match result {
            Ok(tokens) => {
                self.pending
                    .remove(&user_id);
                let expires_at = Utc::now() + Duration::seconds(tokens.expires_in);
                db::TraktToken::upsert(
                    db,
                    user_id,
                    &tokens.access_token,
                    &tokens.refresh_token,
                    expires_at,
                )
                .await?;
                Ok(PollStatus::Authorized)
            }
            Err(e) => match classify_poll_error(&e) {
                Some(status @ (PollStatus::Expired | PollStatus::Denied)) => {
                    self.pending
                        .remove(&user_id);
                    Ok(status)
                }
                Some(status) => Ok(status),
                None => Err(e.into()),
            },
        }
    }

    pub fn cancel(&self, user_id: Uuid) {
        self.pending
            .remove(&user_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limited_is_pending() {
        let err = sdks::ClientError::RateLimited {
            retry_after_secs: 5,
        };
        assert_eq!(classify_poll_error(&err), Some(PollStatus::Pending));
    }

    #[test]
    fn http_400_is_pending() {
        let err = sdks::ClientError::Http {
            status: 400,
            message: "authorization_pending".to_string(),
            endpoint: None,
            body: None,
        };
        assert_eq!(classify_poll_error(&err), Some(PollStatus::Pending));
    }

    #[test]
    fn http_410_is_expired() {
        let err = sdks::ClientError::Http {
            status: 410,
            message: "expired_token".to_string(),
            endpoint: None,
            body: None,
        };
        assert_eq!(classify_poll_error(&err), Some(PollStatus::Expired));
    }

    #[test]
    fn http_418_is_denied() {
        let err = sdks::ClientError::Http {
            status: 418,
            message: "access_denied".to_string(),
            endpoint: None,
            body: None,
        };
        assert_eq!(classify_poll_error(&err), Some(PollStatus::Denied));
    }

    #[test]
    fn unrecognized_status_propagates() {
        let err = sdks::ClientError::Http {
            status: 500,
            message: "server error".to_string(),
            endpoint: None,
            body: None,
        };
        assert_eq!(classify_poll_error(&err), None);
    }
}

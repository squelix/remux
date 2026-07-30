use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use remux_macros::{delete, get, post};

use crate::{
    AppState, ResultExt, db, db::auth, result_ext::IntoApiError, sdks,
    trakt::PollStatus,
};
use axum_anyhow::ApiResult as Result;
use remux_sdks::remux::{
    TraktAuthPollResponse, TraktAuthStartResponse, TraktAuthStatusResponse,
};

#[post("/remux/trakt/auth/start")]
pub async fn start_trakt_auth(
    State(state): State<AppState>,
    session: auth::AuthSession,
) -> Result<impl IntoResponse> {
    let resp = state
        .ctx
        .trakt_auth
        .start(
            &state.ctx.db,
            &state.ctx.config.trakt_base_url,
            session.user.id,
        )
        .await
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("is not configured") {
                e.context_internal(&msg)
            } else {
                tracing::warn!(error = ?e, "failed to start Trakt device authorization");
                e.context_internal("failed to start Trakt device authorization")
            }
        })?;

    Ok(Json(TraktAuthStartResponse {
        user_code: resp.user_code,
        verification_url: resp.verification_url,
        interval: resp.interval,
        expires_in: resp.expires_in,
    }))
}

#[post("/remux/trakt/auth/poll")]
pub async fn poll_trakt_auth(
    State(state): State<AppState>,
    session: auth::AuthSession,
) -> Result<impl IntoResponse> {
    let status = state
        .ctx
        .trakt_auth
        .poll(
            &state
                .ctx
                .db,
            &state
                .ctx
                .config
                .trakt_base_url,
            session
                .user
                .id,
        )
        .await
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("is not configured") {
                e.context_internal(&msg)
            } else {
                tracing::warn!(error = ?e, "failed to poll Trakt device authorization");
                e.context_internal("failed to poll Trakt device authorization")
            }
        })?;

    let status = match status {
        PollStatus::Pending => "pending",
        PollStatus::Authorized => "authorized",
        PollStatus::Expired => "expired",
        PollStatus::Denied => "denied",
    };

    Ok(Json(TraktAuthPollResponse {
        status: status.to_string(),
    }))
}

#[get("/remux/trakt/auth/status")]
pub async fn trakt_auth_status(
    State(state): State<AppState>,
    session: auth::AuthSession,
) -> Result<impl IntoResponse> {
    let connected = db::TraktToken::get_by_user(
        &state
            .ctx
            .db,
        session
            .user
            .id,
    )
    .await
    .context_internal("failed to load Trakt token")?
    .is_some();

    Ok(Json(TraktAuthStatusResponse { connected }))
}

#[delete("/remux/trakt/auth")]
pub async fn disconnect_trakt_auth(
    State(state): State<AppState>,
    session: auth::AuthSession,
) -> Result<impl IntoResponse> {
    state
        .ctx
        .trakt_auth
        .cancel(
            session
                .user
                .id,
        );

    // Best-effort revoke on Trakt's side — a failure here (network, already
    // revoked, missing config) must not block deleting the local token.
    if let Ok(Some(token)) = db::TraktToken::get_by_user(
        &state
            .ctx
            .db,
        session
            .user
            .id,
    )
    .await
    {
        let cfg = db::Settings::get_config(
            &state
                .ctx
                .db,
        )
        .await
        .unwrap_or_default();
        if let (Some(client_id), Some(client_secret)) =
            (cfg.trakt_client_id, cfg.trakt_client_secret)
        {
            if let Ok(client) = sdks::RestClient::new(
                &state
                    .ctx
                    .config
                    .trakt_base_url,
            )
            .map(|c| c.with_auth(sdks::trakt::TraktOAuthAuth))
            {
                let _ = client
                    .execute(sdks::trakt::RevokeTokenEndpoint {
                        client_id,
                        client_secret,
                        token: token.access_token,
                    })
                    .await;
            }
        }
    }

    db::TraktToken::delete(
        &state
            .ctx
            .db,
        session
            .user
            .id,
    )
    .await
    .context_internal("failed to delete Trakt token")?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

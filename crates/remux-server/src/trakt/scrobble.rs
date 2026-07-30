use uuid::Uuid;

use crate::{db, sdks};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrobbleAction {
    Start,
    Pause,
    Stop,
}

pub fn progress_percent(
    position_ticks: i64,
    runtime_seconds: Option<i64>,
) -> Option<f64> {
    let runtime = runtime_seconds.filter(|r| *r > 0)?;
    let position_seconds = position_ticks / 10_000_000;
    Some((position_seconds as f64 / runtime as f64 * 100.0).clamp(0.0, 100.0))
}

pub fn scrobble_target(media: &db::Media) -> Option<sdks::trakt::ScrobbleTarget> {
    use sdks::trakt::{ScrobbleTarget, TraktIdsRef};

    match media.kind {
        db::MediaKind::Movie => {
            let ids = TraktIdsRef {
                imdb: media
                    .external_ids
                    .imdb
                    .as_ref()
                    .map(|s| s.to_string()),
                tmdb: media
                    .external_ids
                    .tmdb,
            };
            if ids
                .imdb
                .is_none()
                && ids
                    .tmdb
                    .is_none()
            {
                return None;
            }
            Some(ScrobbleTarget::Movie { ids })
        }
        db::MediaKind::Episode => {
            let show_ids = TraktIdsRef {
                imdb: media
                    .external_ids
                    .series_imdb
                    .as_ref()
                    .map(|s| s.to_string()),
                tmdb: media
                    .external_ids
                    .series_tmdb,
            };
            if show_ids
                .imdb
                .is_none()
                && show_ids
                    .tmdb
                    .is_none()
            {
                return None;
            }
            let (Some(season), Some(number)) = (media.parent_idx, media.idx) else {
                return None;
            };
            Some(ScrobbleTarget::Episode {
                show_ids,
                season,
                number,
            })
        }
        _ => None,
    }
}

pub fn spawn(
    db: sqlx::SqlitePool,
    trakt_base_url: String,
    user_id: Uuid,
    media: db::Media,
    position_ticks: i64,
    action: ScrobbleAction,
) {
    tokio::spawn(async move {
        if let Err(e) = scrobble(
            &db,
            &trakt_base_url,
            user_id,
            &media,
            position_ticks,
            action,
        )
        .await
        {
            tracing::warn!(%user_id, media_id = %media.id, ?action, "trakt scrobble failed: {e:#}");
        }
    });
}

async fn scrobble(
    db: &sqlx::SqlitePool,
    trakt_base_url: &str,
    user_id: Uuid,
    media: &db::Media,
    position_ticks: i64,
    action: ScrobbleAction,
) -> anyhow::Result<()> {
    let Some(target) = scrobble_target(media) else {
        return Ok(());
    };
    let Some(progress) = progress_percent(position_ticks, media.runtime) else {
        return Ok(());
    };
    let Some(token) = db::TraktToken::get_by_user(db, user_id).await? else {
        return Ok(());
    };
    let cfg = db::Settings::get_config(db).await?;
    let (Some(client_id), Some(client_secret)) =
        (cfg.trakt_client_id, cfg.trakt_client_secret)
    else {
        return Ok(());
    };

    let result = execute_scrobble(
        &client_id,
        trakt_base_url,
        &token.access_token,
        action,
        target.clone(),
        progress,
    )
    .await;

    match result {
        Ok(()) => Ok(()),
        Err(sdks::ClientError::Unauthorized) => {
            let access_token = refresh_access_token(
                db,
                trakt_base_url,
                &client_id,
                &client_secret,
                user_id,
                &token.refresh_token,
            )
            .await?;
            execute_scrobble(
                &client_id,
                trakt_base_url,
                &access_token,
                action,
                target,
                progress,
            )
            .await?;
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

async fn execute_scrobble(
    client_id: &str,
    trakt_base_url: &str,
    access_token: &str,
    action: ScrobbleAction,
    target: sdks::trakt::ScrobbleTarget,
    progress: f64,
) -> Result<(), sdks::ClientError> {
    let client =
        sdks::trakt::trakt_user_client(client_id, access_token, trakt_base_url)
            .map_err(sdks::ClientError::Url)?;
    let result = match action {
        ScrobbleAction::Start => client
            .execute(sdks::trakt::ScrobbleStartEndpoint { target, progress })
            .await
            .map(|_| ()),
        ScrobbleAction::Pause => client
            .execute(sdks::trakt::ScrobblePauseEndpoint { target, progress })
            .await
            .map(|_| ()),
        ScrobbleAction::Stop => client
            .execute(sdks::trakt::ScrobbleStopEndpoint { target, progress })
            .await
            .map(|_| ()),
    };
    match result {
        // Trakt returns 409 when a scrobble is already in progress for this
        // user (e.g. a duplicate `start` after a session/psid reconnect) —
        // treat it as a no-op rather than a failure.
        Err(sdks::ClientError::Http { status: 409, .. }) => Ok(()),
        other => other,
    }
}

async fn refresh_access_token(
    db: &sqlx::SqlitePool,
    trakt_base_url: &str,
    client_id: &str,
    client_secret: &str,
    user_id: Uuid,
    refresh_token: &str,
) -> anyhow::Result<String> {
    let client =
        sdks::RestClient::new(trakt_base_url)?.with_auth(sdks::trakt::TraktOAuthAuth);
    let resp = client
        .execute(sdks::trakt::RefreshTokenEndpoint {
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
            refresh_token: refresh_token.to_string(),
        })
        .await?;
    let expires_at = chrono::Utc::now() + chrono::Duration::seconds(resp.expires_in);
    db::TraktToken::upsert(
        db,
        user_id,
        &resp.access_token,
        &resp.refresh_token,
        expires_at,
    )
    .await?;
    Ok(resp.access_token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_percent_computes_from_ticks_and_runtime() {
        // 1800s position (18_000_000_000 ticks) out of a 3600s runtime = 50%.
        assert_eq!(progress_percent(18_000_000_000, Some(3600)), Some(50.0));
    }

    #[test]
    fn progress_percent_clamps_above_100() {
        assert_eq!(progress_percent(100_000_000_000, Some(10)), Some(100.0));
    }

    #[test]
    fn progress_percent_none_without_runtime() {
        assert_eq!(progress_percent(1_000_000, None), None);
        assert_eq!(progress_percent(1_000_000, Some(0)), None);
    }

    fn movie_media(imdb: Option<&str>, tmdb: Option<i64>) -> db::Media {
        db::Media {
            kind: db::MediaKind::Movie,
            external_ids: db::ExternalIds {
                imdb: imdb.map(|s| db::NonEmptyString::try_new(s.to_string()).unwrap()),
                tmdb,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn episode_media(
        series_imdb: Option<&str>,
        season: Option<i64>,
        number: Option<i64>,
    ) -> db::Media {
        db::Media {
            kind: db::MediaKind::Episode,
            parent_idx: season,
            idx: number,
            external_ids: db::ExternalIds {
                series_imdb: series_imdb
                    .map(|s| db::NonEmptyString::try_new(s.to_string()).unwrap()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn scrobble_target_maps_movie_with_imdb() {
        let media = movie_media(Some("tt123"), None);
        match scrobble_target(&media) {
            Some(sdks::trakt::ScrobbleTarget::Movie { ids }) => {
                assert_eq!(
                    ids.imdb
                        .as_deref(),
                    Some("tt123")
                );
            }
            other => panic!("expected Movie target, got {other:?}"),
        }
    }

    #[test]
    fn scrobble_target_none_for_movie_without_external_ids() {
        let media = movie_media(None, None);
        assert!(scrobble_target(&media).is_none());
    }

    #[test]
    fn scrobble_target_maps_episode_with_series_imdb_and_indices() {
        let media = episode_media(Some("tt999"), Some(2), Some(5));
        match scrobble_target(&media) {
            Some(sdks::trakt::ScrobbleTarget::Episode {
                show_ids,
                season,
                number,
            }) => {
                assert_eq!(
                    show_ids
                        .imdb
                        .as_deref(),
                    Some("tt999")
                );
                assert_eq!(season, 2);
                assert_eq!(number, 5);
            }
            other => panic!("expected Episode target, got {other:?}"),
        }
    }

    #[test]
    fn scrobble_target_none_for_episode_missing_indices() {
        let media = episode_media(Some("tt999"), None, Some(5));
        assert!(scrobble_target(&media).is_none());
    }

    #[test]
    fn scrobble_target_none_for_unsupported_kind() {
        let media = db::Media {
            kind: db::MediaKind::Track,
            ..Default::default()
        };
        assert!(scrobble_target(&media).is_none());
    }
}

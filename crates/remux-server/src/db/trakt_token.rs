use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct TraktToken {
    pub user_id: Uuid,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TraktToken {
    pub async fn get_by_user(db: &SqlitePool, user_id: Uuid) -> Result<Option<Self>> {
        Ok(
            sqlx::query_as::<_, Self>("SELECT * FROM trakt_tokens WHERE user_id = ?1")
                .bind(user_id)
                .fetch_optional(db)
                .await?,
        )
    }

    pub async fn upsert(
        db: &SqlitePool,
        user_id: Uuid,
        access_token: &str,
        refresh_token: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO trakt_tokens (user_id, access_token, refresh_token, expires_at) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(user_id) DO UPDATE SET \
                access_token = excluded.access_token, \
                refresh_token = excluded.refresh_token, \
                expires_at = excluded.expires_at, \
                updated_at = CURRENT_TIMESTAMP",
        )
        .bind(user_id)
        .bind(access_token)
        .bind(refresh_token)
        .bind(expires_at)
        .execute(db)
        .await?;
        Ok(())
    }

    pub async fn delete(db: &SqlitePool, user_id: Uuid) -> Result<()> {
        sqlx::query("DELETE FROM trakt_tokens WHERE user_id = ?1")
            .bind(user_id)
            .execute(db)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::User;

    async fn test_db() -> SqlitePool {
        let db = crate::db::connect("sqlite::memory:", 10_000)
            .await
            .unwrap();
        crate::db::migrate(&db)
            .await
            .unwrap();
        db
    }

    async fn insert_stub_user(db: &SqlitePool) -> Uuid {
        let mut user = User {
            id: Uuid::new_v4(),
            username: format!("user-{}", Uuid::new_v4()),
            password_hash: "hash".to_string(),
            ..Default::default()
        };
        user.save(db)
            .await
            .unwrap();
        user.id
    }

    #[tokio::test]
    async fn upsert_then_get_round_trips() {
        let db = test_db().await;
        let user_id = insert_stub_user(&db).await;
        let expires_at = Utc::now();

        TraktToken::upsert(&db, user_id, "access-1", "refresh-1", expires_at)
            .await
            .unwrap();

        let token = TraktToken::get_by_user(&db, user_id)
            .await
            .unwrap()
            .expect("token should exist");
        assert_eq!(token.access_token, "access-1");
        assert_eq!(token.refresh_token, "refresh-1");
    }

    #[tokio::test]
    async fn upsert_twice_overwrites() {
        let db = test_db().await;
        let user_id = insert_stub_user(&db).await;

        TraktToken::upsert(&db, user_id, "access-1", "refresh-1", Utc::now())
            .await
            .unwrap();
        TraktToken::upsert(&db, user_id, "access-2", "refresh-2", Utc::now())
            .await
            .unwrap();

        let token = TraktToken::get_by_user(&db, user_id)
            .await
            .unwrap()
            .expect("token should exist");
        assert_eq!(token.access_token, "access-2");
        assert_eq!(token.refresh_token, "refresh-2");
    }

    #[tokio::test]
    async fn get_by_user_returns_none_when_absent() {
        let db = test_db().await;
        let user_id = insert_stub_user(&db).await;

        assert!(
            TraktToken::get_by_user(&db, user_id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn delete_removes_token() {
        let db = test_db().await;
        let user_id = insert_stub_user(&db).await;
        TraktToken::upsert(&db, user_id, "access-1", "refresh-1", Utc::now())
            .await
            .unwrap();

        TraktToken::delete(&db, user_id)
            .await
            .unwrap();

        assert!(
            TraktToken::get_by_user(&db, user_id)
                .await
                .unwrap()
                .is_none()
        );
    }
}

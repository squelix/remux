CREATE TABLE trakt_tokens (
    user_id       BLOB     NOT NULL PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    access_token  TEXT     NOT NULL,
    refresh_token TEXT     NOT NULL,
    expires_at    DATETIME NOT NULL,
    created_at    DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at    DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

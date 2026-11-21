-- What the account page shows and checks about a web session:
--
-- - `id`: an opaque id (32 hex digits) the page names a session by, so a session can be listed and
--   revoked without the token or its hash. It is random, never derived from the token.
-- - `last_seen_at`: when a request last used the session, to the resolution the core records
--   (Unix milliseconds); NULL until the first one.
-- - `reverified_at`: when the user last proved who they are again with one of their own sign-in
--   methods (the core's step-up check lets a session change sign-in methods for a while after),
--   NULL if they never have in this session.
--
-- SQLite can't add a NOT NULL, UNIQUE column in place, so the table is rebuilt. Every existing
-- session gets a fresh id.
CREATE TABLE web_sessions_new (
    token_hash    BLOB    PRIMARY KEY CHECK (length(token_hash) = 32),
    id            TEXT    NOT NULL UNIQUE CHECK (length(id) = 32),
    user_id       TEXT    NOT NULL,
    method        TEXT    NOT NULL
        CHECK (method IN ('invite', 'recovery', 'password', 'passkey', 'oidc')),
    created_at    INTEGER NOT NULL,
    expires_at    INTEGER NOT NULL,
    last_seen_at  INTEGER,
    reverified_at INTEGER
) STRICT, WITHOUT ROWID;

INSERT INTO web_sessions_new (token_hash, id, user_id, method, created_at, expires_at)
SELECT token_hash, lower(hex(randomblob(16))), user_id, method, created_at, expires_at
FROM web_sessions;

DROP TABLE web_sessions;
ALTER TABLE web_sessions_new RENAME TO web_sessions;

CREATE INDEX web_sessions_expires ON web_sessions (expires_at);
CREATE INDEX web_sessions_user ON web_sessions (user_id);

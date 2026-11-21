-- One-time links that sign a user in (PLAN 3.4): an `invite` sets up a new account, a `recovery`
-- link gets a user who lost every sign-in method back in. Both work once, until `expires_at`
-- (72 hours after `created_at`). Only the SHA-256 of the 256-bit token is stored, so a copy of the
-- database can't be used to sign in. `used_at` is set when the link is spent, which also ends it.
-- `created_by` is the admin who made the link; it is NULL for the operator's `doozers admin
-- login-link`, and when that admin is removed. Times are Unix milliseconds.
CREATE TABLE user_invites (
    token_hash BLOB    PRIMARY KEY CHECK (length(token_hash) = 32),
    user_id    TEXT    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    kind       TEXT    NOT NULL CHECK (kind IN ('invite', 'recovery')),
    created_by TEXT    REFERENCES users (id) ON DELETE SET NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL CHECK (expires_at > created_at),
    used_at    INTEGER
) STRICT, WITHOUT ROWID;

CREATE INDEX user_invites_user ON user_invites (user_id);
CREATE INDEX user_invites_expires ON user_invites (expires_at);

-- How each web session signed in: with a one-time `invite` or `recovery` link, or with a
-- `password`, a `passkey`, or an identity provider (`oidc`). Every session that exists before
-- this migration came from a one-time login link, which is what a recovery link replaces.
ALTER TABLE web_sessions ADD COLUMN method TEXT NOT NULL DEFAULT 'recovery'
    CHECK (method IN ('invite', 'recovery', 'password', 'passkey', 'oidc'));

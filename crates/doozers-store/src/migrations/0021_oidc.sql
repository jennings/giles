-- The OpenID Connect identity providers an admin configures (PLAN 3.4). `id` is a short slug the
-- admin chooses; it is in the redirect URI the provider is told (`/auth/oidc/<id>/callback`).
-- `issuer` is the issuer URL: the provider's endpoints, keys, and algorithms come from its
-- discovery document, which Doozers fetches, so none are stored. `client_secret` is the name of
-- the client secret in the secret store (`oidc/<id>/client-secret`), never the secret itself, and
-- NULL for a client that has none. `scopes` is the scope tokens separated by single spaces, with
-- `openid` among them. `enabled` is 0 or 1. Times are Unix milliseconds.
CREATE TABLE oidc_providers (
    id            TEXT    PRIMARY KEY CHECK (id <> ''),
    display_name  TEXT    NOT NULL CHECK (display_name <> ''),
    issuer        TEXT    NOT NULL CHECK (issuer <> ''),
    client_id     TEXT    NOT NULL CHECK (client_id <> ''),
    client_secret TEXT    CHECK (client_secret <> ''),
    scopes        TEXT    NOT NULL CHECK (scopes <> ''),
    enabled       INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

-- Which provider account signs in as which user. An account is its `subject` at an `issuer`; a
-- sign-in is matched on those two (under the provider it came through) and never on an email
-- address or a name. A user has at most one account per provider. A provider that is removed
-- takes its connections with it, and so does a user that is removed.
CREATE TABLE user_oidc_connections (
    provider_id  TEXT    NOT NULL REFERENCES oidc_providers (id) ON DELETE CASCADE,
    issuer       TEXT    NOT NULL CHECK (issuer <> ''),
    subject      TEXT    NOT NULL CHECK (subject <> '' AND length(subject) <= 255),
    user_id      TEXT    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    connected_at INTEGER NOT NULL,
    PRIMARY KEY (provider_id, issuer, subject),
    UNIQUE (provider_id, user_id)
) STRICT, WITHOUT ROWID;

CREATE INDEX user_oidc_connections_user ON user_oidc_connections (user_id, connected_at);

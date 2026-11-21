-- The passkeys users sign in with (PLAN 3.4): any number per user. `credential_id` is the
-- authenticator's id for the credential, which the browser sends back at sign-in. It is unique
-- across all users, because a sign-in finds the user by it. `credential` is the WebAuthn library's
-- serialized credential as JSON: the public key, the signature counter, the backup flags, and the
-- transports the browser reported. The store doesn't read it. `kind` is `discoverable` for a
-- passkey made with the "Add a passkey" button (the authenticator keeps it, so the browser can
-- offer it without a username) and `non_resident` for one made with "Add a non-resident key" (the
-- browser needs the username to ask for it). `name` is what the user calls it. A user who is
-- removed takes their passkeys with them. `id` is `AUTOINCREMENT` so one is never reused. Times are
-- Unix milliseconds; `last_used_at` stays NULL until the passkey first signs in.
CREATE TABLE user_passkeys (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id       TEXT    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    credential_id BLOB    NOT NULL UNIQUE CHECK (length(credential_id) > 0),
    credential    TEXT    NOT NULL CHECK (credential <> ''),
    kind          TEXT    NOT NULL CHECK (kind IN ('discoverable', 'non_resident')),
    name          TEXT    NOT NULL CHECK (name <> ''),
    created_at    INTEGER NOT NULL,
    last_used_at  INTEGER
) STRICT;

CREATE INDEX user_passkeys_user ON user_passkeys (user_id, created_at, id);

-- The secret the sign-in page keys its stand-in credential ids with. A username that has no
-- passkeys is answered with ids made up from this key and the name, the same ones every time, so
-- the answer doesn't say which usernames exist. It must survive a restart: ids that changed on
-- restart would tell made-up ones from real ones. At most one row.
CREATE TABLE passkey_fake_key (
    id  INTEGER PRIMARY KEY CHECK (id = 1),
    key BLOB    NOT NULL CHECK (length(key) = 32)
) STRICT;

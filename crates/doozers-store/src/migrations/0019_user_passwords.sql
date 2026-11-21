-- The passwords users sign in with (PLAN 3.4): at most one per user. `hash` is the whole Argon2id
-- hash in PHC string format (`$argon2id$v=19$m=19456,t=2,p=1$<salt>$<hash>`), so the salt and the
-- parameters it was made with travel with it. It lives here and not in the secret store because a
-- secret is something Doozers hands back out, and nothing can recover a password from its hash.
-- `changed_at` is when the user last set it, in Unix milliseconds. A user who is removed takes
-- their password with them.
CREATE TABLE user_passwords (
    user_id    TEXT    PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    hash       TEXT    NOT NULL CHECK (hash <> ''),
    changed_at INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

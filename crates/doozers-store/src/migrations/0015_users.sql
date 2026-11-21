-- The users of this Doozers (PLAN 1.1) and their channel identities (PLAN 3.2). Multi-user support
-- starts here: before this migration the one user, the owner, lived in `[owner]` of doozers.toml.
--
-- `id` is opaque and never changes: sessions, memories, usage rows, and secret paths name it. The
-- existing owner keeps the id `owner`. `username` is what people see and may change; it is unique
-- without regard to ASCII case. `role` is `admin` or `member`. A `disabled` user can't sign in and
-- their messages are dropped, but everything they own stays. `timezone` is an IANA name, or NULL to
-- use the agent's. `default_identity` is the `<namespace>:<native id>` of one of the user's
-- identities: where jobs and proactive messages go. It is NULL when the user has no identity a
-- channel can reach. Times are Unix milliseconds.
CREATE TABLE users (
    id               TEXT    PRIMARY KEY CHECK (id <> ''),
    username         TEXT    NOT NULL COLLATE NOCASE UNIQUE CHECK (username <> ''),
    display_name     TEXT    NOT NULL CHECK (display_name <> ''),
    role             TEXT    NOT NULL CHECK (role IN ('admin', 'member')),
    state            TEXT    NOT NULL CHECK (state IN ('active', 'disabled')),
    timezone         TEXT    CHECK (timezone <> ''),
    default_identity TEXT    CHECK (default_identity <> ''),
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL
) STRICT;

-- The channel identities that act as a user: a message from `namespace` with sender `native_id`
-- is from `user_id`. `identity` is the joined `<namespace>:<native id>`; it is unique, because a
-- namespace can contain `:` (`irc:libera.chat`) and so two splits of one string name one identity.
-- `verification` says how it was established: `link` (the user sent a link code from it), `admin`
-- (an admin or the operator set it: a proxy login identity, the `web:<id>` identity a login link
-- signs in as), or `imported` (it was in `[owner]`). Imported identities are recorded with the
-- namespace up to the first `:`, as the import can't tell where a namespace ends. The domain of an
-- `email` identity is stored in lowercase, so one sender can't belong to two users.
CREATE TABLE user_identities (
    namespace    TEXT    NOT NULL CHECK (namespace <> ''),
    native_id    TEXT    NOT NULL CHECK (native_id <> ''),
    user_id      TEXT    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    verified_at  INTEGER NOT NULL,
    verification TEXT    NOT NULL CHECK (verification IN ('link', 'admin', 'imported')),
    identity     TEXT    NOT NULL GENERATED ALWAYS AS (namespace || ':' || native_id) STORED,
    PRIMARY KEY (namespace, native_id),
    UNIQUE (identity)
) STRICT;

CREATE INDEX user_identities_user ON user_identities (user_id, verified_at, identity);

-- A row exists once `[owner]` of doozers.toml has been imported (or there was none to import).
-- After that doozers.toml's `[owner]` is never read again, even if every user is gone.
CREATE TABLE users_imported (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    at INTEGER NOT NULL
) STRICT;

-- The codes a user sends from a channel identity to prove it is theirs (PLAN 3.2): the web page
-- makes a code for a channel namespace, and sending `/link <code>` from an identity in that
-- namespace adds the identity to the user. A user has at most one code per namespace, so making a
-- new one replaces the old. Only a salted SHA-256 of the 8-character code is stored
-- (`code_hash` = SHA-256(`salt` || code)), so the code can't be read from the database. The code
-- has 40 bits, so someone with a copy of the database could find it by brute force in minutes; a
-- backup is safe because a code expires within 10 minutes (`expires_at`, 10 minutes after
-- `created_at`). `attempts` counts the wrong codes sent while this one was pending, from anyone, in
-- its namespace; the code stops working at the limit the core sets. A code that is used is deleted.
-- Times are Unix milliseconds.
CREATE TABLE user_link_codes (
    user_id    TEXT    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    namespace  TEXT    NOT NULL CHECK (namespace <> ''),
    salt       BLOB    NOT NULL CHECK (length(salt) = 16),
    code_hash  BLOB    NOT NULL CHECK (length(code_hash) = 32),
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL CHECK (expires_at > created_at),
    attempts   INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    PRIMARY KEY (user_id, namespace)
) STRICT, WITHOUT ROWID;

CREATE INDEX user_link_codes_expires ON user_link_codes (expires_at);

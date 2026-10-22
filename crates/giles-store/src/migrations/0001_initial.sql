-- Giles schema version 1. Docs/technical/storage.md describes every table.
-- Conventions: times are Unix milliseconds (INTEGER), booleans are 0 or 1, JSON columns are TEXT.

-- One conversation's history with one agent (PLAN 6.2). Keyed sessions (direct, group) have a
-- channel instance and conversation id; at most one of them per key is open. Isolated job runs
-- (kind 'job') have neither.
CREATE TABLE sessions (
    id           INTEGER PRIMARY KEY,
    agent        TEXT    NOT NULL,
    kind         TEXT    NOT NULL CHECK (kind IN ('direct', 'group', 'job')),
    channel      TEXT,
    conversation TEXT,
    user_id      TEXT,
    started_at   INTEGER NOT NULL,
    ended_at     INTEGER,
    CHECK ((channel IS NULL) = (conversation IS NULL)),
    CHECK ((kind = 'job') = (channel IS NULL))
) STRICT;

CREATE UNIQUE INDEX sessions_open_key ON sessions (agent, channel, conversation)
    WHERE ended_at IS NULL AND channel IS NOT NULL;
CREATE INDEX sessions_agent_user ON sessions (agent, user_id);

-- Transcript messages, numbered by seq within a session. content is a JSON array of
-- giles_types::Block; it's NULL only for an empty tool-result slot (tool_use_id set), which is a
-- user message reserved for the result of one ToolUse block (PLAN 6.3).
CREATE TABLE messages (
    id          INTEGER PRIMARY KEY,
    session_id  INTEGER NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    seq         INTEGER NOT NULL,
    role        TEXT    NOT NULL CHECK (role IN ('user', 'assistant')),
    content     TEXT,
    meta        TEXT    NOT NULL,
    tool_use_id TEXT,
    created_at  INTEGER NOT NULL,
    UNIQUE (session_id, seq),
    CHECK (content IS NOT NULL OR tool_use_id IS NOT NULL),
    CHECK (tool_use_id IS NULL OR role = 'user')
) STRICT;

CREATE INDEX messages_empty_slots ON messages (session_id) WHERE content IS NULL;

-- Compaction summaries (PLAN 6.8): content replaces messages from_seq..=through_seq.
CREATE TABLE summaries (
    id          INTEGER PRIMARY KEY,
    session_id  INTEGER NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    from_seq    INTEGER NOT NULL,
    through_seq INTEGER NOT NULL,
    content     TEXT    NOT NULL,
    created_at  INTEGER NOT NULL,
    CHECK (from_seq <= through_seq)
) STRICT;

CREATE INDEX summaries_session ON summaries (session_id, through_seq);

-- Full-text index over the top-level Text blocks of messages (PLAN 6.7), rowid = messages.id.
-- Only the triggers below write it.
CREATE VIRTUAL TABLE search USING fts5 (
    body,
    tokenize = 'porter unicode61 remove_diacritics 2'
);

CREATE TRIGGER messages_search_insert AFTER INSERT ON messages BEGIN
    INSERT INTO search (rowid, body)
    SELECT new.id, body FROM (
        SELECT group_concat(value ->> '$.text', char(10) ORDER BY key) AS body
        FROM json_each(new.content)
        WHERE value ->> '$.type' = 'text'
    ) WHERE body IS NOT NULL;
END;

CREATE TRIGGER messages_search_update AFTER UPDATE OF content ON messages BEGIN
    DELETE FROM search WHERE rowid = old.id;
    INSERT INTO search (rowid, body)
    SELECT new.id, body FROM (
        SELECT group_concat(value ->> '$.text', char(10) ORDER BY key) AS body
        FROM json_each(new.content)
        WHERE value ->> '$.type' = 'text'
    ) WHERE body IS NOT NULL;
END;

CREATE TRIGGER messages_search_delete AFTER DELETE ON messages BEGIN
    DELETE FROM search WHERE rowid = old.id;
END;

-- Every acked inbound event (PLAN 8.5), deduplicated on (instance, event_id). payload is the
-- event as JSON. Terminal rows (done, unknown) are pruned after 30 days.
CREATE TABLE inbox (
    id          INTEGER PRIMARY KEY,
    instance    TEXT    NOT NULL,
    event_id    TEXT    NOT NULL,
    session_id  INTEGER REFERENCES sessions (id) ON DELETE SET NULL,
    user_id     TEXT,
    payload     TEXT    NOT NULL,
    state       TEXT    NOT NULL CHECK (state IN ('queued', 'started', 'done', 'unknown')),
    received_at INTEGER NOT NULL,
    UNIQUE (instance, event_id)
) STRICT;

CREATE INDEX inbox_session_state ON inbox (session_id, state);
CREATE INDEX inbox_state_received ON inbox (state, received_at);

-- Proactive sends awaiting delivery to an identity (PLAN 6.11), retried until accepted or expired.
CREATE TABLE outbox (
    id              INTEGER PRIMARY KEY,
    instance        TEXT    NOT NULL,
    recipient       TEXT    NOT NULL,
    kind            TEXT    NOT NULL,
    user_id         TEXT,
    payload         TEXT    NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    last_error      TEXT,
    created_at      INTEGER NOT NULL,
    next_attempt_at INTEGER NOT NULL,
    expires_at      INTEGER NOT NULL
) STRICT;

CREATE INDEX outbox_next_attempt ON outbox (next_attempt_at);
CREATE INDEX outbox_expires ON outbox (expires_at);

-- Runtime scheduled jobs (PLAN 6.11). spec is the scheduler's JSON job definition. AUTOINCREMENT
-- so a cancelled job's id (and its job_runs key) is never reused.
CREATE TABLE jobs (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    agent      TEXT    NOT NULL,
    creator    TEXT    NOT NULL,
    spec       TEXT    NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;

CREATE INDEX jobs_agent ON jobs (agent);
CREATE INDEX jobs_creator ON jobs (creator);

-- Last run per job, config and runtime. job is 'config:<id>' or 'runtime:<jobs.id>'.
CREATE TABLE job_runs (
    agent       TEXT    NOT NULL,
    job         TEXT    NOT NULL,
    last_run_at INTEGER NOT NULL,
    PRIMARY KEY (agent, job)
) STRICT, WITHOUT ROWID;

CREATE TRIGGER jobs_delete_runs AFTER DELETE ON jobs BEGIN
    DELETE FROM job_runs WHERE agent = old.agent AND job = 'runtime:' || old.id;
END;

-- One row per provider request (PLAN 11.7). input_tokens excludes cached tokens.
CREATE TABLE usage (
    id                 INTEGER PRIMARY KEY,
    at                 INTEGER NOT NULL,
    provider           TEXT    NOT NULL,
    model              TEXT    NOT NULL,
    agent              TEXT    NOT NULL,
    user_id            TEXT,
    session_id         INTEGER REFERENCES sessions (id) ON DELETE SET NULL,
    input_tokens       INTEGER NOT NULL CHECK (input_tokens >= 0),
    output_tokens      INTEGER NOT NULL CHECK (output_tokens >= 0),
    cache_read_tokens  INTEGER NOT NULL CHECK (cache_read_tokens >= 0),
    cache_write_tokens INTEGER NOT NULL CHECK (cache_write_tokens >= 0),
    fallback           INTEGER NOT NULL CHECK (fallback IN (0, 1))
) STRICT;

CREATE INDEX usage_at ON usage (at);

-- Session deletes (`giles user forget`) set session_id to NULL here; without an index each one
-- would scan the whole table.
CREATE INDEX usage_session ON usage (session_id);

-- Web login sessions (PLAN 3.4). Only the SHA-256 of the cookie token is stored.
CREATE TABLE web_sessions (
    token_hash BLOB    PRIMARY KEY CHECK (length(token_hash) = 32),
    user_id    TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

CREATE INDEX web_sessions_expires ON web_sessions (expires_at);

-- An operator can delete a workspace from the web UI: `delete_reason` gains `operator`. SQLite
-- can't change a CHECK in place, so the table is rebuilt with the same columns, rows, and
-- indexes as 0005 and the one reason added.
CREATE TABLE workspaces_new (
    id                TEXT    NOT NULL PRIMARY KEY CHECK (id <> ''),
    environment       TEXT    NOT NULL CHECK (environment <> ''),
    agent             TEXT    CHECK (agent <> ''),
    channel           TEXT    CHECK (channel <> ''),
    conversation      TEXT    CHECK (conversation <> ''),
    job_session       INTEGER,
    options           TEXT    NOT NULL,
    description       TEXT,
    working_directory TEXT,
    persistence       TEXT,
    state             TEXT    NOT NULL CHECK (state IN ('creating', 'active', 'deleting', 'deleted')),
    created_at        INTEGER NOT NULL,
    last_used_at      INTEGER NOT NULL,
    deleted_at        INTEGER,
    delete_reason     TEXT    CHECK (delete_reason IN (
        'user_reset', 'model_reset', 'model_delete', 'inactive', 'job_run_ended',
        'removed_outside_giles', 'chat_archived', 'creation_failed', 'operator'
    )),
    CHECK (
        (job_session IS NULL AND agent IS NOT NULL AND channel IS NOT NULL
            AND conversation IS NOT NULL)
        OR (job_session IS NOT NULL AND agent IS NULL AND channel IS NULL
            AND conversation IS NULL)
    ),
    CHECK ((state IN ('deleting', 'deleted')) = (deleted_at IS NOT NULL)),
    CHECK ((state IN ('deleting', 'deleted')) = (delete_reason IS NOT NULL)),
    CHECK (state IN ('creating', 'deleting', 'deleted') OR description IS NOT NULL)
) STRICT;

INSERT INTO workspaces_new SELECT * FROM workspaces;
DROP TABLE workspaces;
ALTER TABLE workspaces_new RENAME TO workspaces;

CREATE UNIQUE INDEX workspaces_conversation_owner ON workspaces (agent, channel, conversation)
    WHERE state IN ('creating', 'active') AND job_session IS NULL;
CREATE UNIQUE INDEX workspaces_job_owner ON workspaces (job_session)
    WHERE state IN ('creating', 'active') AND job_session IS NOT NULL;
CREATE INDEX workspaces_environment ON workspaces (environment, state);

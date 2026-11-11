-- The workspaces environment plugins hold for the model's tools (PLAN 8.8). The core assigns `id`
-- (the plugin labels its resources with it), so a plugin's workspace list and these rows meet on
-- the id alone. `environment` is the `[environment.<name>]` instance.
--
-- A workspace has one owner, in one of two forms (the CHECK allows exactly one): a conversation
-- (`agent`, `channel`, `conversation`: the key of a session) or an isolated job run
-- (`job_session`, a `sessions.id` that has no foreign key: a job run that replied NO_REPLY
-- deletes its session, and the workspace row outlives it until the plugin's resources are gone).
--
-- `state`: `creating` (the plugin's `CreateWorkspace` is in flight), `active`, `deleting` (the
-- owner's slot is free but the plugin hasn't confirmed the delete yet, e.g. it was unreachable),
-- `deleted` (the plugin confirmed, or the workspace was found gone). `deleted_at` and
-- `delete_reason` are set exactly when the state is `deleting` or `deleted`; `deleted_at` is
-- when the owner's slot was freed. Deleted rows are kept so the model can be told when and why,
-- and are pruned after the instance's `delete_after`. `description`, `working_directory` and
-- `persistence` are what the plugin reported when it created the workspace.
--
-- At most one `creating` or `active` row per owner. Times are Unix milliseconds; `options` is
-- the JSON the workspace was created with.
CREATE TABLE workspaces (
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
        'removed_outside_giles', 'chat_archived', 'creation_failed'
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

CREATE UNIQUE INDEX workspaces_conversation_owner ON workspaces (agent, channel, conversation)
    WHERE state IN ('creating', 'active') AND job_session IS NULL;
CREATE UNIQUE INDEX workspaces_job_owner ON workspaces (job_session)
    WHERE state IN ('creating', 'active') AND job_session IS NOT NULL;
CREATE INDEX workspaces_environment ON workspaces (environment, state);

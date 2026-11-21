-- The user a direct conversation's workspace belongs to. A conversation id can pass from one user
-- to the next (an IRC nickname's direct conversation, once the first user's session has ended), so
-- a conversation's workspace is owned by agent, channel instance, conversation, and user: two
-- users never share one, and neither one's files or SSH keys can be reached from the other's
-- turns. A group conversation's workspace, which every member shares, and an isolated job run's
-- (owned by its job session) have no user here.
--
-- Workspaces created before this column take the user of `key_user`, which migration 0013 filled
-- from the newest direct session of the conversation; on an install made before users existed,
-- that is the owner. A conversation workspace with no `key_user` (a group's, or a conversation
-- with no direct session left) stays without a user and is treated as a group's.
ALTER TABLE workspaces ADD COLUMN user_id TEXT CHECK (user_id <> '');

UPDATE workspaces SET user_id = key_user WHERE job_session IS NULL;

-- One `creating` or `active` workspace per conversation and user. `coalesce` because NULLs are
-- distinct in a unique index, and a group's workspace has no user.
DROP INDEX workspaces_conversation_owner;
CREATE UNIQUE INDEX workspaces_conversation_owner
    ON workspaces (agent, channel, conversation, coalesce(user_id, ''))
    WHERE state IN ('creating', 'active') AND job_session IS NULL;

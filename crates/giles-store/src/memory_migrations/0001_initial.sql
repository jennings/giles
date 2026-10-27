-- Giles memory database schema version 1 (data/agents/<agent>/memory.db). It has its own
-- migration list and user_version, separate from giles.db. Docs/technical/storage.md describes
-- every table.
-- Conventions: times are Unix milliseconds (INTEGER), booleans are 0 or 1.

-- What an agent remembers. A memory is visible to every session when shared; otherwise only to
-- sessions of its owner: a user, or one group conversation. The owner is recorded with every
-- memory (also a shared one, as its author). Config jobs have no requester, so they own nothing:
-- owner_kind 'none', which only shared memories can have. source_session is the giles.db session
-- the memory was written in. It is only recorded: that database is another file, so it can't be
-- a foreign key.
CREATE TABLE memories (
    id                 INTEGER PRIMARY KEY,
    content            TEXT    NOT NULL CHECK (content <> ''),
    owner_kind         TEXT    NOT NULL CHECK (owner_kind IN ('user', 'group', 'none')),
    owner_user         TEXT,
    owner_instance     TEXT,
    owner_conversation TEXT,
    shared             INTEGER NOT NULL CHECK (shared IN (0, 1)),
    core               INTEGER NOT NULL CHECK (core IN (0, 1)),
    created_at         INTEGER NOT NULL,
    updated_at         INTEGER NOT NULL,
    source_session     INTEGER,
    CHECK (
        CASE owner_kind
            WHEN 'user' THEN owner_user IS NOT NULL
                AND owner_instance IS NULL AND owner_conversation IS NULL
            WHEN 'group' THEN owner_user IS NULL
                AND owner_instance IS NOT NULL AND owner_conversation IS NOT NULL
            ELSE owner_user IS NULL
                AND owner_instance IS NULL AND owner_conversation IS NULL
        END
    ),
    CHECK (shared = 1 OR owner_kind <> 'none')
) STRICT;

-- The core memories of the shared scope, and of each private scope: what the prompt loads and
-- what the core cap adds up.
CREATE INDEX memories_shared_core ON memories (id) WHERE shared = 1 AND core = 1;
CREATE INDEX memories_private_owner
    ON memories (owner_kind, owner_user, owner_instance, owner_conversation, core)
    WHERE shared = 0;

-- Full-text index over content, rowid = memories.id. Only the triggers below write it.
CREATE VIRTUAL TABLE memory_search USING fts5 (
    content,
    tokenize = 'porter unicode61 remove_diacritics 2'
);

CREATE TRIGGER memories_search_insert AFTER INSERT ON memories BEGIN
    INSERT INTO memory_search (rowid, content) VALUES (new.id, new.content);
END;

CREATE TRIGGER memories_search_update AFTER UPDATE OF content ON memories BEGIN
    DELETE FROM memory_search WHERE rowid = old.id;
    INSERT INTO memory_search (rowid, content) VALUES (new.id, new.content);
END;

CREATE TRIGGER memories_search_delete AFTER DELETE ON memories BEGIN
    DELETE FROM memory_search WHERE rowid = old.id;
END;

-- The MCP toolsets the web UI manages. `name` is the toolset's name and its identity: the
-- NOCASE collation makes the primary key unique regardless of ASCII case, so `Files` and `files`
-- can't both exist, and lookups by name ignore case. `spec` is the JSON of the toolset's
-- definition, which the store keeps as written; the core parses and validates it. Times are Unix
-- milliseconds; `updated_at` is what a stale-update check compares.
CREATE TABLE toolsets (
    name       TEXT    NOT NULL PRIMARY KEY COLLATE NOCASE CHECK (name <> ''),
    spec       TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

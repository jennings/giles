-- The ordered routing rules (PLAN 6.1, TASKS 5.2). The first rule that matches a conversation
-- wins, so `position` (0, 1, 2, ..., kept dense by the store) is the order. Ids are never reused
-- (AUTOINCREMENT), so a form for a deleted route can't act on a later one. A null `channel`,
-- `kind`, or `conversation` matches anything.
CREATE TABLE routes (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    position     INTEGER NOT NULL CHECK (position >= 0),
    channel      TEXT CHECK (channel <> ''),
    kind         TEXT CHECK (kind IN ('direct', 'group')),
    conversation TEXT CHECK (conversation <> ''),
    agent        TEXT    NOT NULL CHECK (agent <> '')
) STRICT;

CREATE INDEX routes_position ON routes (position, id);

-- A row exists once the `[[routes]]` of giles.toml have been imported (or found nothing to
-- import). After that giles.toml's routes are never read again, even if every route is deleted.
CREATE TABLE routes_imported (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    at INTEGER NOT NULL
) STRICT;

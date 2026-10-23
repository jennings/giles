-- Blob metadata (PLAN 11.5). The bytes are the file blobs/sha256/<id[0..2]>/<id>; this row is
-- written after the file is in place, so a file without a row is an interrupted put, garbage for
-- the sweep once its mtime is older than the grace period (Docs/technical/storage.md). media_type
-- and name are from the first put of the content.
CREATE TABLE blobs (
    id         TEXT    PRIMARY KEY CHECK (length(id) = 64),
    size       INTEGER NOT NULL CHECK (size >= 0),
    media_type TEXT    NOT NULL,
    name       TEXT,
    created_at INTEGER NOT NULL,
    stored_at  INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

-- stored_at is the latest put of the content; the sweep spares blobs stored in the last 48 hours.
CREATE INDEX blobs_stored_at ON blobs (stored_at);

-- The web channel asks whether a user's messages and inbox rows refer to a blob
-- (`BlobStore::reference_of`) for every attachment it shows, so those rows are found by user:
-- before this, by scanning the whole tables. The messages are reached through the user's sessions
-- (`sessions_user`), newest first. Rows without a user (commands run from the operator's side)
-- aren't indexed.
CREATE INDEX sessions_user ON sessions (user_id);
CREATE INDEX inbox_user ON inbox (user_id) WHERE user_id IS NOT NULL;

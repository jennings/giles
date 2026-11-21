-- When a user last signed in to the web chat, for the admin's list of users: the creation time of
-- the newest web session any sign-in method started (Unix milliseconds), NULL if they never have.
-- A web session is deleted by logging out, revoking it, and expiry, so the sessions can't answer
-- this later. The store sets the column in the transaction that creates a session. A login through
-- a trusted proxy starts no session and isn't recorded.
--
-- Existing users start with the creation time of their newest live session, which is when they last
-- signed in if it was in the last 30 days and is the best the database knows.
ALTER TABLE users ADD COLUMN last_sign_in_at INTEGER;

UPDATE users
SET last_sign_in_at = (SELECT max(created_at) FROM web_sessions WHERE web_sessions.user_id = users.id);

//! The plugin's state in its data directory: the IMAP cursor and the threads it can reply to.
//!
//! - `cursor.json`: `{"uid_validity": V, "uid": U}`, the last message of the mailbox that is
//!   either dropped or persisted in the SDK's event spool. IMAP can replay, so the plugin keeps a
//!   cursor rather than a spool of its own (PLAN section 8.5).
//! - `threads/<sha256 of the conversation id>.json`: whom to reply to and how to thread it.
//!
//! Both are replaced atomically (write a temporary file, sync, rename, sync the directory).

use std::path::{Path, PathBuf};

use giles_plugin_sdk::giles_types::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::Mutex;

/// How many ids a thread's `References` keeps: the root and the newest ones.
const MAX_REFERENCES: usize = 20;

/// Replaces `path` with `bytes` atomically and durably.
async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| Error::other("a state file has no directory"))?;
    let tmp = path.with_extension("tmp");
    let mut file = tokio::fs::File::create(&tmp).await.map_err(Error::other)?;
    file.write_all(bytes).await.map_err(Error::other)?;
    file.sync_all().await.map_err(Error::other)?;
    drop(file);
    tokio::fs::rename(&tmp, path).await.map_err(Error::other)?;
    tokio::fs::File::open(dir)
        .await
        .map_err(Error::other)?
        .sync_all()
        .await
        .map_err(Error::other)
}

/// Reads and parses a JSON state file; `None` if it doesn't exist.
async fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    match tokio::fs::read(path).await {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| Error::other(format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::other(e)),
    }
}

/// A position in a mailbox: everything up to `uid` is done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Cursor {
    pub uid_validity: u32,
    pub uid: u32,
}

/// The persisted [`Cursor`].
#[derive(Debug)]
pub(crate) struct CursorStore {
    path: PathBuf,
    current: Mutex<Option<Cursor>>,
}

impl CursorStore {
    pub(crate) async fn open(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join("cursor.json");
        let current = read_json(&path).await?;
        Ok(Self {
            path,
            current: Mutex::new(current),
        })
    }

    pub(crate) async fn get(&self) -> Option<Cursor> {
        *self.current.lock().await
    }

    /// Replaces the cursor (a new mailbox, or one whose UIDVALIDITY changed).
    pub(crate) async fn set(&self, cursor: Cursor) -> Result<()> {
        let mut current = self.current.lock().await;
        write_atomic(
            &self.path,
            &serde_json::to_vec(&cursor).map_err(Error::other)?,
        )
        .await?;
        *current = Some(cursor);
        Ok(())
    }

    /// Moves the cursor forward to `uid`, if it's in the same mailbox generation and ahead.
    pub(crate) async fn advance(&self, uid_validity: u32, uid: u32) -> Result<()> {
        let mut current = self.current.lock().await;
        let Some(cursor) = *current else {
            return Ok(());
        };
        if cursor.uid_validity != uid_validity || uid <= cursor.uid {
            return Ok(());
        }
        let next = Cursor { uid_validity, uid };
        write_atomic(
            &self.path,
            &serde_json::to_vec(&next).map_err(Error::other)?,
        )
        .await?;
        *current = Some(next);
        Ok(())
    }
}

/// What the plugin remembers about a conversation to reply into it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Thread {
    /// Where replies go.
    pub peer: String,
    pub peer_name: Option<String>,
    /// The latest subject (without our `Re: `).
    pub subject: String,
    /// The thread's message ids, root first, newest last, at most [`MAX_REFERENCES`].
    pub references: Vec<String>,
}

impl Thread {
    /// The message a reply answers when the core doesn't say: the newest one.
    pub(crate) fn last(&self) -> Option<&str> {
        self.references.last().map(String::as_str)
    }

    /// Appends a message id, keeping the root and the newest ids.
    pub(crate) fn push(&mut self, id: String) {
        self.references.push(id);
        cap(&mut self.references);
    }
}

/// Keeps the first id and the newest ones, at most [`MAX_REFERENCES`] in all.
pub(crate) fn cap(references: &mut Vec<String>) {
    if references.len() > MAX_REFERENCES {
        references.drain(1..references.len() - (MAX_REFERENCES - 1));
    }
}

/// The threads, one file each.
#[derive(Debug)]
pub(crate) struct Threads {
    dir: PathBuf,
    /// Serializes read-modify-write updates.
    lock: Mutex<()>,
}

impl Threads {
    pub(crate) async fn open(data_dir: &Path) -> Result<Self> {
        let dir = data_dir.join("threads");
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(Error::other)?;
        Ok(Self {
            dir,
            lock: Mutex::new(()),
        })
    }

    fn path(&self, conversation: &str) -> PathBuf {
        let digest = Sha256::digest(conversation.as_bytes());
        let name: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        self.dir.join(format!("{name}.json"))
    }

    pub(crate) async fn get(&self, conversation: &str) -> Result<Option<Thread>> {
        let _guard = self.lock.lock().await;
        read_json(&self.path(conversation)).await
    }

    /// Replaces a thread with what `update` makes of the stored one.
    pub(crate) async fn update(
        &self,
        conversation: &str,
        update: impl FnOnce(Option<Thread>) -> Thread,
    ) -> Result<Thread> {
        let _guard = self.lock.lock().await;
        let path = self.path(conversation);
        let thread = update(read_json(&path).await?);
        write_atomic(&path, &serde_json::to_vec(&thread).map_err(Error::other)?).await?;
        Ok(thread)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_cursor_survives_a_restart_and_only_moves_forward() {
        let dir = tempfile::tempdir().unwrap();
        let store = CursorStore::open(dir.path()).await.unwrap();
        assert_eq!(store.get().await, None);
        store.advance(7, 3).await.unwrap();
        assert_eq!(store.get().await, None, "nothing to advance before set");
        store
            .set(Cursor {
                uid_validity: 7,
                uid: 3,
            })
            .await
            .unwrap();
        store.advance(7, 5).await.unwrap();
        store.advance(7, 4).await.unwrap();
        store.advance(8, 9).await.unwrap();
        let expected = Cursor {
            uid_validity: 7,
            uid: 5,
        };
        assert_eq!(store.get().await, Some(expected));
        let reopened = CursorStore::open(dir.path()).await.unwrap();
        assert_eq!(reopened.get().await, Some(expected));
    }

    #[tokio::test]
    async fn a_corrupt_cursor_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cursor.json"), "{").unwrap();
        assert!(CursorStore::open(dir.path()).await.is_err());
    }

    #[tokio::test]
    async fn threads_are_kept_per_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let threads = Threads::open(dir.path()).await.unwrap();
        assert_eq!(threads.get("a/x@y").await.unwrap(), None);
        let thread = Thread {
            peer: "a@example.com".into(),
            peer_name: None,
            subject: "Hi".into(),
            references: vec!["x@y".into()],
        };
        threads.update("a/x@y", |_| thread.clone()).await.unwrap();
        let updated = threads
            .update("a/x@y", |old| {
                let mut old = old.unwrap();
                old.push("z@y".into());
                old
            })
            .await
            .unwrap();
        assert_eq!(updated.last(), Some("z@y"));
        let reopened = Threads::open(dir.path()).await.unwrap();
        assert_eq!(reopened.get("a/x@y").await.unwrap(), Some(updated));
        assert_eq!(reopened.get("b/x@y").await.unwrap(), None);
    }

    #[test]
    fn references_keep_the_root_and_the_newest() {
        let mut thread = Thread {
            peer: "a@example.com".into(),
            peer_name: None,
            subject: String::new(),
            references: vec!["root".into()],
        };
        for i in 0..30 {
            thread.push(format!("m{i}"));
        }
        assert_eq!(thread.references.len(), MAX_REFERENCES);
        assert_eq!(thread.references[0], "root");
        assert_eq!(thread.references[1], "m11");
        assert_eq!(thread.last(), Some("m29"));
    }
}

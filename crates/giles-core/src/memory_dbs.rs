//! [`MemoryDbs`]: the open per-agent memory databases (`agents/<agent>/memory.db`).
//!
//! Each agent has its own [`MemoryStore`]. The registry opens one on first use, in the journal mode
//! of `[core] journal_mode` (as for `giles.db`), and keeps it open: every [`get`](MemoryDbs::get)
//! of the same agent returns a handle to the same connections, and concurrent first calls open the
//! file once. A database nothing asks for is never opened, so an agent that has never remembered
//! anything has no `memory.db` until its first `get`.
//!
//! # Closing
//!
//! [`close`](MemoryDbs::close) drops the registry's handle. When the last handle is dropped the
//! connections close, and in WAL mode the writer checkpoints the log and deletes `-wal` and `-shm`,
//! leaving one complete file that can be moved or copied. Removing an agent does this before it
//! moves `agents/<agent>/` to the trash. A later `get` opens the file again, wherever it is by
//! then.
//!
//! Handles are clones that keep the connections open, so a caller must not hold a [`MemoryStore`]
//! across turns or between calls: call `get` for each use, so that `close` really closes the
//! database.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use giles_store::{JournalMode, MemoryStore, OpenOptions};
use giles_types::AgentId;
use tokio::sync::OnceCell;

use crate::paths::DataRoot;

/// The open memory databases of the agents. Cheap to clone; all clones share the same databases.
/// See the [module docs](self).
#[derive(Debug, Clone)]
pub struct MemoryDbs {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    root: DataRoot,
    journal_mode: JournalMode,
    /// A slot per agent, filled by the first `get`. Held only to find or insert a slot, never
    /// across an await.
    open: Mutex<HashMap<AgentId, Arc<OnceCell<MemoryStore>>>>,
    /// How many databases `get` has opened, for tests.
    #[cfg(test)]
    opens: std::sync::atomic::AtomicUsize,
}

impl MemoryDbs {
    /// A registry of the memory databases under `root`, opened in `journal_mode`. Nothing is
    /// opened until the first [`get`](Self::get).
    pub fn new(root: DataRoot, journal_mode: JournalMode) -> Self {
        Self {
            inner: Arc::new(Inner {
                root,
                journal_mode,
                open: Mutex::new(HashMap::new()),
                #[cfg(test)]
                opens: std::sync::atomic::AtomicUsize::new(0),
            }),
        }
    }

    /// The memory database of `agent`, opened (and created, with the agent's directory, if
    /// missing) on the first call and kept open after it.
    ///
    /// # Errors
    ///
    /// [`giles_store::Error::InvalidRequest`] if `agent` isn't a valid directory name, otherwise
    /// the error of [`MemoryStore::open`]. A failed open is not remembered: the next call tries
    /// again.
    pub async fn get(&self, agent: &AgentId) -> giles_store::Result<MemoryStore> {
        let path = self
            .inner
            .root
            .agent_memory_db(agent.as_str())
            .map_err(|error| giles_store::Error::InvalidRequest(error.to_string()))?;
        let slot = self
            .inner
            .open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(agent.clone())
            .or_default()
            .clone();
        let store = slot
            .get_or_try_init(|| async move {
                #[cfg(test)]
                self.inner
                    .opens
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                MemoryStore::open(
                    path,
                    OpenOptions::new().journal_mode(self.inner.journal_mode),
                )
                .await
            })
            .await?;
        Ok(store.clone())
    }

    /// Drops the registry's handle to the memory database of `agent`, if it is open. The database
    /// closes (and a WAL is checkpointed) once no clone of the handle is left; see the
    /// [module docs](self). Does nothing for an agent whose database isn't open.
    pub async fn close(&self, agent: &AgentId) {
        let slot = self
            .inner
            .open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(agent);
        // The connections close when the slot (and the store in it) drops; closing a SQLite
        // database blocks on a checkpoint, so do it off the async threads.
        if let Some(slot) = slot {
            let _ = tokio::task::spawn_blocking(move || drop(slot)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::Ordering;

    use giles_store::{MemoryOwner, MemoryViewer, NewMemory};
    use giles_types::{ConversationId, Timestamp};

    use super::*;

    const CAP: usize = 2200;

    fn dbs(dir: &tempfile::TempDir) -> MemoryDbs {
        let root = DataRoot::new(dir.path(), std::path::Path::new("/tmp")).unwrap();
        MemoryDbs::new(root, JournalMode::Wal)
    }

    fn agent(name: &str) -> AgentId {
        AgentId::new(name).unwrap()
    }

    fn viewer() -> MemoryViewer {
        MemoryViewer::new(
            MemoryOwner::Group {
                instance: "web".to_owned(),
                conversation: ConversationId::new("room").unwrap(),
            },
            false,
        )
    }

    async fn remember(store: &MemoryStore, content: &str) {
        let memory = NewMemory {
            content: content.to_owned(),
            core: true,
            shared: false,
            source_session: None,
        };
        store
            .add(
                &viewer(),
                memory,
                CAP,
                Timestamp::from_millisecond(1_000).unwrap(),
            )
            .await
            .unwrap();
    }

    async fn core(store: &MemoryStore) -> Vec<String> {
        let core = store.core_memories(&viewer()).await.unwrap();
        core.own.memories.into_iter().map(|m| m.content).collect()
    }

    #[tokio::test]
    async fn a_database_is_opened_lazily_in_the_agents_directory_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let dbs = dbs(&dir);
        let path = dir.path().join("agents/jeeves/memory.db");
        assert!(!path.exists());

        let first = dbs.get(&agent("jeeves")).await.unwrap();
        assert_eq!(first.path(), path);
        assert!(path.exists());
        remember(&first, "likes tea").await;
        drop(first);

        let second = dbs.get(&agent("jeeves")).await.unwrap();
        assert_eq!(core(&second).await, ["likes tea"]);
        assert_eq!(dbs.inner.opens.load(Ordering::SeqCst), 1);

        // A clone of the registry shares its databases; another agent has its own file.
        let other = dbs.clone().get(&agent("wooster")).await.unwrap();
        assert!(core(&other).await.is_empty());
        assert_eq!(other.path(), dir.path().join("agents/wooster/memory.db"));
        dbs.clone().get(&agent("jeeves")).await.unwrap();
        assert_eq!(dbs.inner.opens.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn concurrent_first_calls_open_the_database_once() {
        let dir = tempfile::tempdir().unwrap();
        let dbs = dbs(&dir);
        let id = agent("jeeves");
        let stores = futures::future::join_all((0..8).map(|_| dbs.get(&id))).await;
        assert_eq!(dbs.inner.opens.load(Ordering::SeqCst), 1);
        let stores: Vec<_> = stores.into_iter().map(Result::unwrap).collect();
        remember(&stores[0], "one").await;
        for store in &stores {
            assert_eq!(core(store).await, ["one"]);
        }
    }

    #[tokio::test]
    async fn close_then_get_reopens_with_the_data() {
        let dir = tempfile::tempdir().unwrap();
        let dbs = dbs(&dir);
        let id = agent("jeeves");
        let store = dbs.get(&id).await.unwrap();
        remember(&store, "likes tea").await;
        drop(store);

        dbs.close(&id).await;
        // Closing an agent that is not open is fine.
        dbs.close(&agent("ghost")).await;
        let store = dbs.get(&id).await.unwrap();
        assert_eq!(dbs.inner.opens.load(Ordering::SeqCst), 2);
        assert_eq!(core(&store).await, ["likes tea"]);
    }

    #[tokio::test]
    async fn close_checkpoints_so_the_directory_can_move_and_come_back() {
        let dir = tempfile::tempdir().unwrap();
        let dbs = dbs(&dir);
        let id = agent("jeeves");
        let store = dbs.get(&id).await.unwrap();
        remember(&store, "likes tea").await;
        drop(store);
        let live = dir.path().join("agents/jeeves");
        assert!(live.join("memory.db-wal").exists());

        dbs.close(&id).await;
        assert!(live.join("memory.db").exists());
        assert!(!live.join("memory.db-wal").exists());
        assert!(!live.join("memory.db-shm").exists());

        let aside = dir.path().join("aside");
        fs::rename(&live, &aside).unwrap();
        assert!(!live.exists());
        fs::rename(&aside, &live).unwrap();
        let store = dbs.get(&id).await.unwrap();
        assert_eq!(core(&store).await, ["likes tea"]);
    }

    #[tokio::test]
    async fn an_invalid_agent_name_is_an_error_and_a_failed_open_is_retried() {
        let dir = tempfile::tempdir().unwrap();
        let dbs = dbs(&dir);
        // `AgentId` accepts what the directory layout doesn't.
        let bad = AgentId::new("a/b").unwrap();
        assert!(matches!(
            dbs.get(&bad).await,
            Err(giles_store::Error::InvalidRequest(_))
        ));

        // The agents directory is a file, so the open fails, and works once that is fixed.
        fs::write(dir.path().join("agents"), "in the way").unwrap();
        let id = agent("jeeves");
        assert!(dbs.get(&id).await.is_err());
        fs::remove_file(dir.path().join("agents")).unwrap();
        assert!(dbs.get(&id).await.is_ok());
    }
}

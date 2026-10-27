//! `MemoryStore`: the file and its migrations, visibility, the shared-write rule, the limits, and
//! recall.

use std::os::unix::fs::PermissionsExt;

use super::*;
use crate::{JournalMode, MEMORY_SCHEMA_VERSION, SCHEMA_VERSION};

const CAP: usize = 2200;

fn at(ms: i64) -> Timestamp {
    Timestamp::from_millisecond(ms).unwrap()
}

async fn open_in(dir: &Path) -> MemoryStore {
    MemoryStore::open(dir.join("agents/helper/memory.db"), OpenOptions::new())
        .await
        .unwrap()
}

async fn open() -> (tempfile::TempDir, MemoryStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = open_in(dir.path()).await;
    (dir, store)
}

fn user_owner(name: &str) -> MemoryOwner {
    MemoryOwner::User(UserId::new(name).unwrap())
}

fn group_owner(instance: &str, conversation: &str) -> MemoryOwner {
    MemoryOwner::Group {
        instance: instance.to_owned(),
        conversation: ConversationId::new(conversation).unwrap(),
    }
}

/// The agent's owner in a DM: may write shared memories.
fn owner() -> MemoryViewer {
    MemoryViewer::new(user_owner("owner"), true)
}

/// A user who isn't the agent's owner.
fn guest(name: &str) -> MemoryViewer {
    MemoryViewer::new(user_owner(name), false)
}

fn group(instance: &str, conversation: &str) -> MemoryViewer {
    MemoryViewer::new(group_owner(instance, conversation), false)
}

/// A config job with no requester.
fn config_job() -> MemoryViewer {
    MemoryViewer::new(MemoryOwner::None, true)
}

fn new(content: &str) -> NewMemory {
    NewMemory {
        content: content.to_owned(),
        core: false,
        shared: false,
        source_session: None,
    }
}

fn new_core(content: &str) -> NewMemory {
    NewMemory {
        core: true,
        ..new(content)
    }
}

fn new_shared(content: &str) -> NewMemory {
    NewMemory {
        shared: true,
        ..new(content)
    }
}

fn new_shared_core(content: &str) -> NewMemory {
    NewMemory {
        core: true,
        shared: true,
        ..new(content)
    }
}

async fn add(store: &MemoryStore, viewer: &MemoryViewer, memory: NewMemory) -> Memory {
    store.add(viewer, memory, CAP, at(1_000)).await.unwrap()
}

fn content_edit(content: &str) -> MemoryEdit {
    MemoryEdit {
        content: Some(content.to_owned()),
        ..MemoryEdit::default()
    }
}

fn overflow(err: Error) -> CoreOverflow {
    match err {
        Error::CoreCapExceeded(overflow) => *overflow,
        other => panic!("expected CoreCapExceeded, got {other:?}"),
    }
}

async fn user_version(store: &Store) -> i64 {
    store
        .read(|conn| Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?))
        .await
        .unwrap()
}

async fn has_table(store: &Store, name: &'static str) -> bool {
    store
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [name],
                |row| row.get::<_, i64>(0),
            )? == 1)
        })
        .await
        .unwrap()
}

/// The ids among `ids` that `viewer` can `get`.
async fn visible_to(store: &MemoryStore, viewer: &MemoryViewer, ids: &[MemoryId]) -> Vec<MemoryId> {
    let mut seen = Vec::new();
    for &id in ids {
        match store.get(viewer, id).await {
            Ok(memory) => {
                assert_eq!(memory.id, id);
                seen.push(id);
            }
            Err(Error::UnknownMemory(unknown)) => assert_eq!(unknown, id),
            Err(other) => panic!("unexpected error {other:?}"),
        }
    }
    seen
}

fn ids(memories: &[Memory]) -> Vec<MemoryId> {
    memories.iter().map(|m| m.id).collect()
}

// --- The file and its migrations ----------------------------------------------------------------

#[tokio::test]
async fn first_open_creates_private_directories_and_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_in(dir.path()).await;
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(store.path(), dir.path().join("agents/helper/memory.db"));
    assert_eq!(mode(&dir.path().join("agents")), 0o700);
    assert_eq!(mode(&dir.path().join("agents/helper")), 0o700);
    assert_eq!(mode(store.path()), 0o600);

    // A write creates the WAL files, which follow the database's mode.
    add(&store, &owner(), new("remember")).await;
    for suffix in ["-wal", "-shm"] {
        let mut name = store.path().as_os_str().to_owned();
        name.push(suffix);
        assert_eq!(mode(Path::new(&name)), 0o600, "{suffix}");
    }
}

#[tokio::test]
async fn an_existing_file_keeps_its_mode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    std::fs::write(&path, b"").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    MemoryStore::open(&path, OpenOptions::new()).await.unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o640
    );
}

#[tokio::test]
async fn reopening_keeps_the_data_and_does_not_migrate_again() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_in(dir.path()).await;
    let memory = add(&store, &owner(), new_core("the cat is called Miso")).await;
    drop(store);

    let ran_hook = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = ran_hook.clone();
    let options = OpenOptions::new().before_migrate(move |_| {
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    });
    let store = MemoryStore::open(dir.path().join("agents/helper/memory.db"), options)
        .await
        .unwrap();
    assert!(!ran_hook.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(store.get(&owner(), memory.id).await.unwrap(), memory);
    assert_eq!(user_version(&store.store).await, MEMORY_SCHEMA_VERSION);
    let found = store.recall(&owner(), "miso", 10).await.unwrap();
    assert_eq!(ids(&found), [memory.id]);
}

#[tokio::test]
async fn the_schema_version_is_independent_from_giles_db() {
    let dir = tempfile::tempdir().unwrap();
    let giles = Store::open(dir.path().join("giles.db")).await.unwrap();
    let store = open_in(dir.path()).await;
    assert_eq!(user_version(&giles).await, SCHEMA_VERSION);
    assert_eq!(user_version(&store.store).await, MEMORY_SCHEMA_VERSION);
    assert!(has_table(&store.store, "memories").await);
    assert!(has_table(&store.store, "memory_search").await);
    assert!(!has_table(&store.store, "sessions").await);
    assert!(!has_table(&giles, "memories").await);
}

#[tokio::test]
async fn a_database_from_a_newer_giles_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let store = MemoryStore::open(&path, OpenOptions::new()).await.unwrap();
    store
        .store
        .write(|conn| {
            conn.pragma_update(None, "user_version", MEMORY_SCHEMA_VERSION + 1)?;
            Ok(())
        })
        .await
        .unwrap();
    drop(store);
    let err = MemoryStore::open(&path, OpenOptions::new())
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        Error::SchemaTooNew { found, supported }
            if found == MEMORY_SCHEMA_VERSION + 1 && supported == MEMORY_SCHEMA_VERSION
    ));
}

#[tokio::test]
async fn the_journal_mode_option_applies() {
    let dir = tempfile::tempdir().unwrap();
    let options = OpenOptions::new().journal_mode(JournalMode::Delete);
    let store = MemoryStore::open(dir.path().join("memory.db"), options)
        .await
        .unwrap();
    let mode: String = store
        .store
        .read(|conn| Ok(conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?))
        .await
        .unwrap();
    assert_eq!(mode, "delete");
}

#[tokio::test]
async fn vacuum_into_snapshots_the_memories() {
    let (dir, store) = open().await;
    let memory = add(&store, &owner(), new("snapshot me")).await;
    let dest = dir.path().join("snapshot.db");
    store.vacuum_into(dest.clone()).await.unwrap();
    let copy = MemoryStore::open(&dest, OpenOptions::new()).await.unwrap();
    assert_eq!(copy.get(&owner(), memory.id).await.unwrap(), memory);
}

// --- Basic operations ---------------------------------------------------------------------------

#[tokio::test]
async fn add_get_edit_and_forget_round_trip() {
    let (_dir, store) = open().await;
    let alice = guest("alice");
    let session = Some(SessionId::new(7));
    let added = store
        .add(
            &alice,
            NewMemory {
                content: "  likes tea  ".to_owned(),
                core: true,
                shared: false,
                source_session: session,
            },
            CAP,
            at(1_000),
        )
        .await
        .unwrap();
    assert_eq!(
        added,
        Memory {
            id: added.id,
            content: "likes tea".to_owned(),
            owner: user_owner("alice"),
            shared: false,
            core: true,
            created_at: at(1_000),
            updated_at: at(1_000),
            source_session: session,
        }
    );
    assert_eq!(store.get(&alice, added.id).await.unwrap(), added);

    let edited = store
        .edit(
            &alice,
            added.id,
            MemoryEdit {
                content: Some("likes coffee".to_owned()),
                core: Some(false),
                shared: None,
            },
            CAP,
            at(2_000),
        )
        .await
        .unwrap();
    assert_eq!(
        edited,
        Memory {
            content: "likes coffee".to_owned(),
            core: false,
            updated_at: at(2_000),
            ..added.clone()
        }
    );
    assert_eq!(store.get(&alice, added.id).await.unwrap(), edited);

    assert_eq!(store.forget(&alice, added.id).await.unwrap(), edited);
    let err = store.get(&alice, added.id).await.unwrap_err();
    assert!(matches!(err, Error::UnknownMemory(id) if id == added.id));
}

#[tokio::test]
async fn an_edit_that_changes_nothing_is_rejected() {
    let (_dir, store) = open().await;
    let alice = guest("alice");
    let memory = add(&store, &alice, new("x")).await;
    let err = store
        .edit(&alice, memory.id, MemoryEdit::default(), CAP, at(2))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidRequest(_)), "{err:?}");
}

// --- Visibility ---------------------------------------------------------------------------------

#[tokio::test]
async fn private_memories_are_visible_only_to_their_owner_scope() {
    let (_dir, store) = open().await;
    let alice = guest("alice");
    let bob = guest("bob");
    let g1 = group("telegram", "g1");
    let g2 = group("telegram", "g2");
    let g1_elsewhere = group("signal", "g1");
    let job = config_job();

    let m_alice = add(&store, &alice, new("zebra alice")).await.id;
    let m_bob = add(&store, &bob, new("zebra bob")).await.id;
    let m_g1 = add(&store, &g1, new("zebra g1")).await.id;
    let m_g2 = add(&store, &g2, new("zebra g2")).await.id;
    let m_shared = add(&store, &owner(), new_shared("zebra shared")).await.id;
    let m_job = add(&store, &job, new_shared("zebra job")).await.id;
    let all = [m_alice, m_bob, m_g1, m_g2, m_shared, m_job];

    let the_owner = owner();
    let expectations = [
        (&alice, vec![m_alice, m_shared, m_job]),
        (&bob, vec![m_bob, m_shared, m_job]),
        (&g1, vec![m_g1, m_shared, m_job]),
        (&g2, vec![m_g2, m_shared, m_job]),
        (&g1_elsewhere, vec![m_shared, m_job]),
        (&job, vec![m_shared, m_job]),
        (&the_owner, vec![m_shared, m_job]),
    ];
    for (viewer, expected) in expectations {
        assert_eq!(
            visible_to(&store, viewer, &all).await,
            expected,
            "{viewer:?}"
        );
        let mut recalled = ids(&store.recall(viewer, "zebra", 50).await.unwrap());
        recalled.sort();
        assert_eq!(recalled, expected, "recall for {viewer:?}");
    }
}

#[tokio::test]
async fn a_users_dm_memories_and_group_memories_do_not_mix() {
    let (_dir, store) = open().await;
    let alice_dm = guest("alice");
    let alice_in_group = group("telegram", "family");
    let dm = add(&store, &alice_dm, new("dm secret")).await.id;
    let in_group = add(&store, &alice_in_group, new("group note")).await.id;
    assert_eq!(visible_to(&store, &alice_dm, &[dm, in_group]).await, [dm]);
    assert_eq!(
        visible_to(&store, &alice_in_group, &[dm, in_group]).await,
        [in_group]
    );
}

#[tokio::test]
async fn a_config_job_sees_shared_memories_only_and_writes_with_no_owner() {
    let (_dir, store) = open().await;
    let job = config_job();
    let private = add(&store, &guest("alice"), new("private")).await.id;
    let shared = add(&store, &owner(), new_shared("shared")).await.id;
    assert_eq!(visible_to(&store, &job, &[private, shared]).await, [shared]);

    let written = add(&store, &job, new_shared("written by a job")).await;
    assert_eq!(written.owner, MemoryOwner::None);
    assert!(written.shared);
    // Everyone sees what it wrote.
    assert_eq!(
        visible_to(&store, &guest("bob"), &[written.id]).await,
        [written.id]
    );

    // It can't write a memory only nobody could read back.
    let err = store
        .add(&job, new("private job note"), CAP, at(1))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidRequest(_)), "{err:?}");
    let err = store
        .edit(
            &job,
            written.id,
            MemoryEdit {
                shared: Some(false),
                ..MemoryEdit::default()
            },
            CAP,
            at(2),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidRequest(_)), "{err:?}");
    assert!(store.get(&job, written.id).await.unwrap().shared);
}

#[tokio::test]
async fn unseen_ids_are_the_same_error_as_missing_ids() {
    let (_dir, store) = open().await;
    let (_other_dir, empty) = open().await;
    let alice = guest("alice");
    let bob = guest("bob");
    let hidden = add(&store, &alice, new("alice only")).await;
    // The same id in a database where nothing exists.
    let id = hidden.id;

    let from_hidden = [
        store.get(&bob, id).await.unwrap_err(),
        store
            .edit(&bob, id, content_edit("mine now"), CAP, at(2))
            .await
            .unwrap_err(),
        store.forget(&bob, id).await.unwrap_err(),
        store.get(&config_job(), id).await.unwrap_err(),
        store.get(&group("telegram", "g"), id).await.unwrap_err(),
    ];
    let from_missing = [
        empty.get(&bob, id).await.unwrap_err(),
        empty
            .edit(&bob, id, content_edit("mine now"), CAP, at(2))
            .await
            .unwrap_err(),
        empty.forget(&bob, id).await.unwrap_err(),
        empty.get(&config_job(), id).await.unwrap_err(),
        empty.get(&group("telegram", "g"), id).await.unwrap_err(),
    ];
    for (hidden, missing) in from_hidden.iter().zip(&from_missing) {
        assert!(matches!(hidden, Error::UnknownMemory(got) if *got == id));
        assert!(matches!(missing, Error::UnknownMemory(got) if *got == id));
        assert_eq!(hidden.to_string(), missing.to_string());
    }

    // The attempts changed nothing.
    assert_eq!(store.get(&alice, id).await.unwrap(), hidden);
}

// --- The shared-write rule ----------------------------------------------------------------------

#[tokio::test]
async fn only_a_shared_writer_can_create_a_shared_memory() {
    let (_dir, store) = open().await;
    for viewer in [guest("bob"), group("telegram", "g")] {
        let err = store
            .add(&viewer, new_shared("planted"), CAP, at(1))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::SharedWriteForbidden), "{err:?}");
        let err = store
            .add(&viewer, new_shared_core("planted"), CAP, at(1))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::SharedWriteForbidden), "{err:?}");
    }
    assert_eq!(
        store.recall(&owner(), "planted", 10).await.unwrap(),
        Vec::new()
    );
    let ok = add(&store, &owner(), new_shared("fine")).await;
    assert!(ok.shared);
}

#[tokio::test]
async fn only_a_shared_writer_can_mark_a_memory_shared() {
    let (_dir, store) = open().await;
    let bob = guest("bob");
    let mine = add(&store, &bob, new("bob's")).await;
    let share = MemoryEdit {
        shared: Some(true),
        ..MemoryEdit::default()
    };
    let err = store
        .edit(&bob, mine.id, share.clone(), CAP, at(2))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::SharedWriteForbidden), "{err:?}");
    assert!(!store.get(&bob, mine.id).await.unwrap().shared);

    // The owner shares their own memory, and then everyone sees it.
    let theirs = add(&store, &owner(), new("owner's")).await;
    let shared = store
        .edit(&owner(), theirs.id, share, CAP, at(2))
        .await
        .unwrap();
    assert!(shared.shared);
    assert_eq!(shared.owner, user_owner("owner"));
    assert_eq!(store.get(&bob, theirs.id).await.unwrap().content, "owner's");
}

#[tokio::test]
async fn only_a_shared_writer_can_edit_or_forget_a_shared_memory() {
    let (_dir, store) = open().await;
    let bob = guest("bob");
    let shared = add(&store, &owner(), new_shared("house rules")).await;

    let err = store
        .edit(&bob, shared.id, content_edit("vandalised"), CAP, at(2))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::SharedWriteForbidden), "{err:?}");
    let unshare = MemoryEdit {
        shared: Some(false),
        ..MemoryEdit::default()
    };
    let err = store
        .edit(&bob, shared.id, unshare, CAP, at(2))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::SharedWriteForbidden), "{err:?}");
    let err = store.forget(&bob, shared.id).await.unwrap_err();
    assert!(matches!(err, Error::SharedWriteForbidden), "{err:?}");
    assert_eq!(store.get(&bob, shared.id).await.unwrap(), shared);

    // A config job can change it, and so can the owner.
    let by_job = store
        .edit(
            &config_job(),
            shared.id,
            content_edit("house rules v2"),
            CAP,
            at(3),
        )
        .await
        .unwrap();
    assert_eq!(by_job.content, "house rules v2");
    store.forget(&owner(), shared.id).await.unwrap();
}

#[tokio::test]
async fn unsharing_makes_the_memory_private_to_whoever_unshared_it() {
    let (_dir, store) = open().await;
    let job_made = add(&store, &config_job(), new_shared("from a job")).await;
    let unshared = store
        .edit(
            &owner(),
            job_made.id,
            MemoryEdit {
                shared: Some(false),
                ..MemoryEdit::default()
            },
            CAP,
            at(2),
        )
        .await
        .unwrap();
    assert!(!unshared.shared);
    assert_eq!(unshared.owner, user_owner("owner"));
    assert_eq!(
        visible_to(&store, &owner(), &[job_made.id]).await,
        [job_made.id]
    );
    assert!(
        visible_to(&store, &config_job(), &[job_made.id])
            .await
            .is_empty()
    );
    assert!(
        visible_to(&store, &guest("bob"), &[job_made.id])
            .await
            .is_empty()
    );
}

// --- The per-memory limit -----------------------------------------------------------------------

#[tokio::test]
async fn content_may_be_a_thousand_characters_but_not_more() {
    let (_dir, store) = open().await;
    let alice = guest("alice");
    let max = "a".repeat(MAX_MEMORY_CHARS);
    let ok = add(&store, &alice, new(&max)).await;
    assert_eq!(ok.content, max);

    let err = store
        .add(&alice, new(&"a".repeat(MAX_MEMORY_CHARS + 1)), CAP, at(1))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::MemoryTooLong {
                chars: 1001,
                max: 1000
            }
        ),
        "{err:?}"
    );

    let err = store
        .edit(&alice, ok.id, content_edit(&"b".repeat(1001)), CAP, at(2))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::MemoryTooLong { .. }), "{err:?}");
    assert_eq!(store.get(&alice, ok.id).await.unwrap().content, max);
}

#[tokio::test]
async fn the_limit_counts_characters_not_bytes() {
    let (_dir, store) = open().await;
    let alice = guest("alice");
    // Two bytes each: 2,000 bytes.
    let accents = "é".repeat(1000);
    assert_eq!(add(&store, &alice, new(&accents)).await.content, accents);
    // Four bytes each.
    let emoji = "🦓".repeat(1000);
    assert_eq!(add(&store, &alice, new(&emoji)).await.content, emoji);
    let err = store
        .add(&alice, new(&"é".repeat(1001)), CAP, at(1))
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::MemoryTooLong { chars: 1001, .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn content_is_trimmed_and_must_not_be_empty() {
    let (_dir, store) = open().await;
    let alice = guest("alice");
    for blank in ["", "   ", "\n\t "] {
        let err = store.add(&alice, new(blank), CAP, at(1)).await.unwrap_err();
        assert!(matches!(err, Error::InvalidRequest(_)), "{err:?}");
    }
    // Whitespace around a full-length memory doesn't count.
    let padded = format!("  {}\n", "a".repeat(MAX_MEMORY_CHARS));
    assert_eq!(
        add(&store, &alice, new(&padded)).await.content.len(),
        MAX_MEMORY_CHARS
    );
    let memory = add(&store, &alice, new("x")).await;
    let err = store
        .edit(&alice, memory.id, content_edit("  "), CAP, at(2))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidRequest(_)), "{err:?}");
}

// --- The core cap -------------------------------------------------------------------------------

fn text(chars: usize) -> String {
    "a".repeat(chars)
}

#[tokio::test]
async fn core_memories_in_a_scope_are_capped() {
    let (_dir, store) = open().await;
    let alice = guest("alice");
    let first = store
        .add(&alice, new_core(&text(60)), 100, at(1))
        .await
        .unwrap();

    let err = store
        .add(&alice, new_core(&text(41)), 100, at(2))
        .await
        .unwrap_err();
    let over = overflow(err);
    assert_eq!(over.scope, MemoryScope::Private(user_owner("alice")));
    assert_eq!(over.memories, [first]);
    assert_eq!((over.used, over.cap, over.overflow), (60, 100, 1));

    // Exactly the cap fits, and non-core memories don't count.
    store
        .add(&alice, new_core(&text(40)), 100, at(3))
        .await
        .unwrap();
    store
        .add(&alice, new(&text(500)), 100, at(4))
        .await
        .unwrap();
    assert_eq!(store.core_memories(&alice).await.unwrap().own.chars, 100);
}

#[tokio::test]
async fn each_scope_has_its_own_core_cap() {
    let (_dir, store) = open().await;
    let full = |viewer: MemoryViewer, shared: bool| {
        let store = store.clone();
        async move {
            store
                .add(
                    &viewer,
                    NewMemory {
                        shared,
                        ..new_core(&text(100))
                    },
                    100,
                    at(1),
                )
                .await
        }
    };
    // Different users, a group, a group elsewhere, and the shared scope each take a full cap.
    full(guest("alice"), false).await.unwrap();
    full(guest("bob"), false).await.unwrap();
    full(group("telegram", "g1"), false).await.unwrap();
    full(group("telegram", "g2"), false).await.unwrap();
    full(group("signal", "g1"), false).await.unwrap();
    full(owner(), false).await.unwrap();
    full(owner(), true).await.unwrap();
    full(config_job(), true).await.unwrap_err();

    // And each is then full.
    for (viewer, shared) in [(guest("alice"), false), (group("telegram", "g1"), false)] {
        let over = overflow(full(viewer, shared).await.unwrap_err());
        assert_eq!((over.used, over.overflow), (100, 100));
    }
    let over = overflow(full(owner(), true).await.unwrap_err());
    assert_eq!(over.scope, MemoryScope::Shared);
    assert_eq!(over.memories.len(), 1);
}

#[tokio::test]
async fn an_edit_that_grows_a_core_memory_is_capped() {
    let (_dir, store) = open().await;
    let alice = guest("alice");
    let a = store
        .add(&alice, new_core(&text(60)), 100, at(1))
        .await
        .unwrap();
    let b = store
        .add(&alice, new_core(&text(30)), 100, at(2))
        .await
        .unwrap();

    let err = store
        .edit(&alice, a.id, content_edit(&text(71)), 100, at(3))
        .await
        .unwrap_err();
    let over = overflow(err);
    assert_eq!(over.memories, [a.clone(), b.clone()]);
    assert_eq!((over.used, over.cap, over.overflow), (90, 100, 1));
    // Rejected: nothing changed.
    assert_eq!(store.get(&alice, a.id).await.unwrap(), a);

    // The memory's own old content doesn't count twice.
    let grown = store
        .edit(&alice, a.id, content_edit(&text(70)), 100, at(3))
        .await
        .unwrap();
    assert_eq!(grown.chars(), 70);
}

#[tokio::test]
async fn an_edit_that_turns_core_on_is_capped() {
    let (_dir, store) = open().await;
    let alice = guest("alice");
    let core = store
        .add(&alice, new_core(&text(60)), 100, at(1))
        .await
        .unwrap();
    let plain = store.add(&alice, new(&text(50)), 100, at(2)).await.unwrap();
    let make_core = MemoryEdit {
        core: Some(true),
        ..MemoryEdit::default()
    };

    let over = overflow(
        store
            .edit(&alice, plain.id, make_core.clone(), 100, at(3))
            .await
            .unwrap_err(),
    );
    assert_eq!(over.memories, [core]);
    assert_eq!((over.used, over.overflow), (60, 10));
    assert!(!store.get(&alice, plain.id).await.unwrap().core);

    // Shortening it in the same edit makes it fit.
    let edit = MemoryEdit {
        content: Some(text(40)),
        ..make_core
    };
    let done = store
        .edit(&alice, plain.id, edit, 100, at(3))
        .await
        .unwrap();
    assert!(done.core);
    assert_eq!(store.core_memories(&alice).await.unwrap().own.chars, 100);
}

#[tokio::test]
async fn sharing_a_core_memory_checks_the_shared_cap() {
    let (_dir, store) = open().await;
    let shared = store
        .add(&owner(), new_shared_core(&text(60)), 100, at(1))
        .await
        .unwrap();
    let mine = store
        .add(&owner(), new_core(&text(60)), 100, at(2))
        .await
        .unwrap();
    let share = MemoryEdit {
        shared: Some(true),
        ..MemoryEdit::default()
    };

    let over = overflow(
        store
            .edit(&owner(), mine.id, share.clone(), 100, at(3))
            .await
            .unwrap_err(),
    );
    assert_eq!(over.scope, MemoryScope::Shared);
    assert_eq!(over.memories, [shared]);
    assert_eq!((over.used, over.overflow), (60, 20));
    assert!(!store.get(&owner(), mine.id).await.unwrap().shared);

    // With room, it moves from the owner's scope into the shared one.
    store
        .edit(&owner(), mine.id, share, 120, at(3))
        .await
        .unwrap();
    let core = store.core_memories(&owner()).await.unwrap();
    assert_eq!(core.shared.chars, 120);
    assert_eq!(core.own, CoreScope::default());
}

#[tokio::test]
async fn unsharing_a_core_memory_checks_the_viewers_own_cap() {
    let (_dir, store) = open().await;
    let shared = store
        .add(&owner(), new_shared_core(&text(60)), 100, at(1))
        .await
        .unwrap();
    let mine = store
        .add(&owner(), new_core(&text(60)), 100, at(2))
        .await
        .unwrap();
    let unshare = MemoryEdit {
        shared: Some(false),
        ..MemoryEdit::default()
    };
    let over = overflow(
        store
            .edit(&owner(), shared.id, unshare, 100, at(3))
            .await
            .unwrap_err(),
    );
    assert_eq!(over.scope, MemoryScope::Private(user_owner("owner")));
    assert_eq!(over.memories, [mine]);
    assert_eq!(over.overflow, 20);
}

#[tokio::test]
async fn a_scope_over_a_lowered_cap_can_still_shrink() {
    let (_dir, store) = open().await;
    let alice = guest("alice");
    let memory = store
        .add(&alice, new_core(&text(80)), 100, at(1))
        .await
        .unwrap();
    // The cap was lowered to 50 since.
    let err = store
        .edit(&alice, memory.id, content_edit(&text(81)), 50, at(2))
        .await
        .unwrap_err();
    assert_eq!(overflow(err).overflow, 31);
    let shrunk = store
        .edit(&alice, memory.id, content_edit(&text(70)), 50, at(3))
        .await
        .unwrap();
    assert_eq!(shrunk.chars(), 70);
    // Other edits to it still work too.
    store
        .edit(
            &alice,
            memory.id,
            MemoryEdit {
                core: Some(true),
                ..MemoryEdit::default()
            },
            50,
            at(4),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn the_core_cap_counts_characters_not_bytes() {
    let (_dir, store) = open().await;
    let alice = guest("alice");
    store
        .add(&alice, new_core(&"é".repeat(10)), 10, at(1))
        .await
        .unwrap();
    let err = store
        .add(&alice, new_core("é"), 10, at(2))
        .await
        .unwrap_err();
    assert_eq!(overflow(err).overflow, 1);
}

#[tokio::test]
async fn forgetting_a_core_memory_frees_its_room() {
    let (_dir, store) = open().await;
    let alice = guest("alice");
    let memory = store
        .add(&alice, new_core(&text(100)), 100, at(1))
        .await
        .unwrap();
    store
        .add(&alice, new_core("x"), 100, at(2))
        .await
        .unwrap_err();
    store.forget(&alice, memory.id).await.unwrap();
    store.add(&alice, new_core("x"), 100, at(3)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writers_never_break_the_cap() {
    let (_dir, store) = open().await;
    // Each core memory is 100 characters and the cap is 1,000: exactly 10 of 20 fit.
    let tasks: Vec<_> = (0..20)
        .map(|i| {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .add(&guest("alice"), new_core(&text(100)), 1000, at(i))
                    .await
            })
        })
        .collect();
    let mut added = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => added += 1,
            Err(err) => {
                let over = overflow(err);
                assert_eq!((over.used, over.overflow), (1000, 100));
            }
        }
    }
    assert_eq!(added, 10);
    let core = store.core_memories(&guest("alice")).await.unwrap();
    assert_eq!(core.own.memories.len(), 10);
    assert_eq!(core.own.chars, 1000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_edits_and_adds_never_break_the_cap() {
    let (_dir, store) = open().await;
    let alice = guest("alice");
    let base_id = store
        .add(&alice, new_core(&text(10)), 100, at(0))
        .await
        .unwrap()
        .id;
    // Twenty tasks each try to grow the same memory by 10 and add a 10-character one.
    let tasks: Vec<_> = (1..=20)
        .map(|i| {
            let (store, alice) = (store.clone(), alice.clone());
            tokio::spawn(async move {
                let id = base_id;
                let _ = store
                    .edit(
                        &alice,
                        id,
                        content_edit(&text(10 + 10 * i as usize)),
                        100,
                        at(i),
                    )
                    .await;
                let _ = store.add(&alice, new_core(&text(10)), 100, at(i)).await;
            })
        })
        .collect();
    for task in tasks {
        task.await.unwrap();
    }
    let core = store.core_memories(&alice).await.unwrap();
    assert!(core.own.chars <= 100, "{}", core.own.chars);
}

// --- core_memories ------------------------------------------------------------------------------

#[tokio::test]
async fn core_memories_list_shared_first_then_the_sessions_own() {
    let (_dir, store) = open().await;
    let alice = guest("alice");
    let s1 = add(&store, &owner(), new_shared_core("shared one")).await;
    let p1 = add(&store, &alice, new_core("alice one")).await;
    let _plain = add(&store, &alice, new("alice not core")).await;
    let s2 = add(&store, &config_job(), new_shared_core("shared two")).await;
    let _bob = add(&store, &guest("bob"), new_core("bob one")).await;
    let p2 = add(&store, &alice, new_core("alice two")).await;
    let _shared_plain = add(&store, &owner(), new_shared("shared not core")).await;

    let core = store.core_memories(&alice).await.unwrap();
    assert_eq!(core.shared.memories, [s1.clone(), s2.clone()]);
    assert_eq!(core.shared.chars, "shared one".len() + "shared two".len());
    assert_eq!(core.own.memories, [p1, p2]);
    assert_eq!(core.own.chars, "alice one".len() + "alice two".len());

    // A config job and a group see the shared ones and nothing of alice's.
    for viewer in [config_job(), group("telegram", "g")] {
        let core = store.core_memories(&viewer).await.unwrap();
        assert_eq!(core.shared.memories, [s1.clone(), s2.clone()]);
        assert_eq!(core.own, CoreScope::default());
    }
}

// --- Recall -------------------------------------------------------------------------------------

#[tokio::test]
async fn recall_ranks_by_bm25() {
    let (_dir, store) = open().await;
    let alice = guest("alice");
    let one = add(&store, &alice, new("dentist")).await;
    add(&store, &alice, new("groceries on saturday")).await;
    let both = add(&store, &alice, new("dentist on tuesday")).await;
    let rare = add(&store, &alice, new("tuesday")).await;

    let found = store.recall(&alice, "dentist tuesday", 10).await.unwrap();
    // Matching both words wins; the rest follow.
    assert_eq!(found[0].id, both.id);
    let rest: Vec<_> = ids(&found[1..]);
    assert_eq!(found.len(), 3);
    assert!(rest.contains(&one.id) && rest.contains(&rare.id));
}

#[tokio::test]
async fn recall_breaks_ties_newest_first() {
    let (_dir, store) = open().await;
    let alice = guest("alice");
    let old = store
        .add(&alice, new("garden notes"), CAP, at(1))
        .await
        .unwrap();
    let mid = store
        .add(&alice, new("garden notes"), CAP, at(2))
        .await
        .unwrap();
    let new_ = store
        .add(&alice, new("garden notes"), CAP, at(2))
        .await
        .unwrap();
    let found = store.recall(&alice, "garden", 10).await.unwrap();
    // Same updated_at: the higher id first.
    assert_eq!(ids(&found), [new_.id, mid.id, old.id]);

    // Touching the oldest makes it the newest.
    store
        .edit(&alice, old.id, content_edit("garden notes"), CAP, at(3))
        .await
        .unwrap();
    let found = store.recall(&alice, "garden", 10).await.unwrap();
    assert_eq!(ids(&found), [old.id, new_.id, mid.id]);
}

#[tokio::test]
async fn recall_honours_the_limit_and_includes_core_memories() {
    let (_dir, store) = open().await;
    let alice = guest("alice");
    let core = add(&store, &alice, new_core("pasta night")).await;
    let plain = add(&store, &alice, new("pasta recipe")).await;
    let shared_core = add(&store, &owner(), new_shared_core("pasta budget")).await;

    let found = store.recall(&alice, "pasta", 50).await.unwrap();
    assert_eq!(found.len(), 3);
    for id in [core.id, plain.id, shared_core.id] {
        assert!(ids(&found).contains(&id));
    }
    assert_eq!(store.recall(&alice, "pasta", 2).await.unwrap().len(), 2);
    // A limit of zero still returns one.
    assert_eq!(store.recall(&alice, "pasta", 0).await.unwrap().len(), 1);
}

#[tokio::test]
async fn recall_follows_edits_and_forgets() {
    let (_dir, store) = open().await;
    let alice = guest("alice");
    let memory = add(&store, &alice, new("likes tea")).await;
    assert_eq!(
        ids(&store.recall(&alice, "tea", 10).await.unwrap()),
        [memory.id]
    );

    store
        .edit(&alice, memory.id, content_edit("likes coffee"), CAP, at(2))
        .await
        .unwrap();
    assert!(store.recall(&alice, "tea", 10).await.unwrap().is_empty());
    assert_eq!(
        ids(&store.recall(&alice, "coffee", 10).await.unwrap()),
        [memory.id]
    );

    store.forget(&alice, memory.id).await.unwrap();
    assert!(store.recall(&alice, "coffee", 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn recall_with_no_words_finds_nothing() {
    let (_dir, store) = open().await;
    let alice = guest("alice");
    add(&store, &alice, new("something")).await;
    for query in ["", "   ", "-- !! ...", "\"", "*"] {
        assert!(
            store.recall(&alice, query, 10).await.unwrap().is_empty(),
            "{query:?}"
        );
    }
}

#[tokio::test]
async fn recall_matches_fts5_syntax_characters_as_text() {
    let (_dir, store) = open().await;
    let alice = guest("alice");
    let memory = add(
        &store,
        &alice,
        new(r#"the "quoted" star* rate-limit AND OR NEAR(x) note: (alpha)"#),
    )
    .await;
    let other = add(&store, &alice, new("an unrelated memory")).await;

    for query in [
        r#""quoted""#,
        r#"quoted""#,
        "star*",
        "rate-limit",
        "-limit",
        "note:",
        "AND",
        "OR",
        "NEAR(",
        "NEAR(x)",
        "(alpha)",
        "alpha)",
        "(",
        "a AND b OR c NOT d",
        "content:alpha",
        "^alpha",
        "alpha*",
    ] {
        let found = store
            .recall(&alice, query, 10)
            .await
            .unwrap_or_else(|err| panic!("{query:?}: {err}"));
        assert!(!ids(&found).contains(&other.id), "{query:?}");
    }
    // Words that are in the text find it.
    for query in [
        r#""quoted""#,
        "star*",
        "rate-limit",
        "-limit",
        "note:",
        "AND",
        "NEAR(x)",
        "(alpha)",
        "alpha*",
    ] {
        let found = store.recall(&alice, query, 10).await.unwrap();
        assert_eq!(ids(&found), [memory.id], "{query:?}");
    }
    // Operators don't act as operators: with AND, `unrelated AND nothing` would find nothing.
    let found = store
        .recall(&alice, "unrelated AND nothing", 10)
        .await
        .unwrap();
    let mut found = ids(&found);
    found.sort();
    assert_eq!(found, [memory.id, other.id]);
}

//! Backups of a real database and a real data root in a temporary directory.

use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::fs::{PermissionsExt, symlink};

use giles_store::{
    BlobStore, JobKey, JournalMode, Memory, MemoryOwner, MemoryViewer, NewMemory, Store,
};
use giles_types::{AgentId, ConversationId, UserId};
use tempfile::TempDir;

use super::*;

fn at(secs: i64) -> Timestamp {
    Timestamp::from_second(1_800_000_000 + secs).unwrap()
}

/// A data root with its layout created.
fn data_root() -> (TempDir, DataRoot) {
    let dir = tempfile::tempdir().unwrap();
    let root = DataRoot::new(dir.path(), Path::new("/tmp")).unwrap();
    root.create_layout().unwrap();
    (dir, root)
}

fn put(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// Fills `root` with one file of everything the layout holds, backed up or not.
fn populate(root: &DataRoot) {
    let r = root.root();
    put(&root.config_file(), "[owner]\n");
    put(&root.secrets_file(), "api_key = \"hunter2\"\n");
    put(
        &root.agent_dir("main").unwrap().join("agent.toml"),
        "model = \"x\"\n",
    );
    put(
        &root.agent_dir("main").unwrap().join("SOUL.md"),
        "I am Giles.\n",
    );
    put(
        &root
            .agent_dir("main")
            .unwrap()
            .join("users/owner/MEMORY.md"),
        "likes tea\n",
    );
    put(&root.user_dir("owner").unwrap().join("USER.md"), "Owner.\n");
    put(&root.skills_dir().join("tea/SKILL.md"), "Make tea.\n");
    put(&root.skills_dir().join("tea/steep.sh"), "#!/bin/sh\n");
    put(&root.blobs_dir().join("sha256/ab/abcdef"), "blob bytes");
    put(&root.blobs_dir().join("sha256/tmp/partial"), "half a put");
    put(&root.plugins_dir().join("giles-channel-irc"), "binary");
    put(
        &root.plugin_data_dir("irc").unwrap().join("state.json"),
        "{}",
    );
    put(&root.backups_dir().join("old.tar"), "an earlier backup");
    put(&root.cache_dir().join("describe.json"), "{}");
    put(
        &root
            .trash_dir()
            .join("agents/jeeves-20261024T120000Z/SOUL.md"),
        "a removed agent\n",
    );
    put(&root.admin_token_file(), "token");
    put(&r.join("data/giles.db-wal"), "wal");
}

/// The names in a tar, with their contents (empty for directories) and file modes.
fn read_archive(path: &Path) -> BTreeMap<String, (String, u32)> {
    let mut archive = tar::Archive::new(fs::File::open(path).unwrap());
    let mut entries = BTreeMap::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let name = entry.path().unwrap().to_string_lossy().into_owned();
        let mode = entry.header().mode().unwrap();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        assert!(entries.insert(name, (text, mode)).is_none());
    }
    entries
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// The database and the blob store, opened as the core does.
async fn open_stores(root: &DataRoot) -> (Store, BlobStore) {
    let store = Store::open(root.database_file()).await.unwrap();
    let blobs = BlobStore::open(root.blobs_dir(), store.clone())
        .await
        .unwrap();
    (store, blobs)
}

async fn job_run(store: &Store, job: &str) -> Option<Timestamp> {
    store
        .job_last_run(AgentId::new("main").unwrap(), JobKey::Config(job.into()))
        .await
        .unwrap()
}

#[tokio::test]
async fn the_database_snapshot_restores_to_a_usable_store_with_the_same_rows() {
    let (_dir, root) = data_root();
    let (store, blobs) = open_stores(&root).await;
    let agent = || AgentId::new("main").unwrap();
    for (i, job) in ["a", "b", "c"].into_iter().enumerate() {
        store
            .job_set_last_run(agent(), JobKey::Config(job.into()), at(i as i64))
            .await
            .unwrap();
    }
    // The core is running: its latest writes are still in the WAL.
    assert!(root.data_dir().join("giles.db-wal").exists());

    let out = tempfile::tempdir().unwrap();
    let snapshot = create(&root, &store, &blobs, out.path(), at(100))
        .await
        .unwrap();
    assert_eq!(
        snapshot.database,
        out.path().join("giles-20270115T080140Z.db")
    );
    assert_eq!(
        snapshot.archive,
        out.path().join("giles-20270115T080140Z.tar")
    );
    // Writes after the snapshot stay out of it.
    store
        .job_set_last_run(agent(), JobKey::Config("later".into()), at(200))
        .await
        .unwrap();

    // Restoring is copying the file into place; the copy opens like any database.
    let restored = tempfile::tempdir().unwrap();
    let restored_db = restored.path().join("giles.db");
    fs::copy(&snapshot.database, &restored_db).unwrap();
    let restored = Store::open(&restored_db).await.unwrap();
    for (i, job) in ["a", "b", "c"].into_iter().enumerate() {
        assert_eq!(job_run(&restored, job).await, Some(at(i as i64)), "{job}");
    }
    assert_eq!(job_run(&restored, "later").await, None);
    // It's a working database, not a read-only artifact.
    restored
        .job_set_last_run(agent(), JobKey::Config("new".into()), at(300))
        .await
        .unwrap();
    assert_eq!(job_run(&restored, "new").await, Some(at(300)));
}

#[tokio::test]
async fn the_archive_holds_what_a_restore_needs_and_nothing_else() {
    let (_dir, root) = data_root();
    populate(&root);
    let (store, blobs) = open_stores(&root).await;
    // Links are never followed: not to a file outside the root, and not to a directory.
    let outside = tempfile::tempdir().unwrap();
    put(&outside.path().join("secret.txt"), "outside");
    symlink(
        outside.path().join("secret.txt"),
        root.skills_dir().join("tea/link"),
    )
    .unwrap();
    symlink(outside.path(), root.skills_dir().join("linked-dir")).unwrap();

    let out = tempfile::tempdir().unwrap();
    let snapshot = create(&root, &store, &blobs, out.path(), at(0))
        .await
        .unwrap();

    let entries = read_archive(&snapshot.archive);
    let text = |name: &str| entries[name].0.as_str();
    let names: Vec<&str> = entries.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        [
            "agents",
            "agents/main",
            "agents/main/SOUL.md",
            "agents/main/agent.toml",
            "agents/main/users",
            "agents/main/users/owner",
            "agents/main/users/owner/MEMORY.md",
            "data/blobs",
            "data/blobs/sha256",
            "data/blobs/sha256/ab",
            "data/blobs/sha256/ab/abcdef",
            "giles.toml",
            "skills",
            "skills/tea",
            "skills/tea/SKILL.md",
            "skills/tea/steep.sh",
            "users",
            "users/owner",
            "users/owner/USER.md",
        ],
        "no secrets.toml, plugins, plugin data, backups, run, cache, trash (removed agents), \
         database, links, or blob puts in progress"
    );
    assert_eq!(text("giles.toml"), "[owner]\n");
    assert_eq!(text("agents/main/SOUL.md"), "I am Giles.\n");
    assert_eq!(text("agents/main/users/owner/MEMORY.md"), "likes tea\n");
    assert_eq!(text("users/owner/USER.md"), "Owner.\n");
    assert_eq!(text("skills/tea/steep.sh"), "#!/bin/sh\n");
    assert_eq!(text("data/blobs/sha256/ab/abcdef"), "blob bytes");
    // Directories keep the private mode the layout gives them.
    assert_eq!(entries["agents"].1 & 0o777, 0o700);
}

#[tokio::test]
async fn an_archive_unpacks_over_a_data_root() {
    let (_dir, root) = data_root();
    populate(&root);
    let (store, blobs) = open_stores(&root).await;
    let out = tempfile::tempdir().unwrap();
    let snapshot = create(&root, &store, &blobs, out.path(), at(0))
        .await
        .unwrap();

    let (_fresh_dir, fresh) = data_root();
    tar::Archive::new(fs::File::open(&snapshot.archive).unwrap())
        .unpack(fresh.root())
        .unwrap();
    assert_eq!(
        fs::read_to_string(fresh.agent_dir("main").unwrap().join("SOUL.md")).unwrap(),
        "I am Giles.\n"
    );
    assert_eq!(
        fs::read_to_string(fresh.blobs_dir().join("sha256/ab/abcdef")).unwrap(),
        "blob bytes"
    );
}

#[tokio::test]
async fn the_snapshot_never_contains_itself() {
    let (_dir, root) = data_root();
    populate(&root);
    let (store, blobs) = open_stores(&root).await;
    // A backup directory inside an archived tree. (An earlier backup in a tree is archived like
    // any file; the one being written is not.)
    for (dir, now) in [
        (root.agents_dir(), at(0)),
        (root.blobs_dir().join("sha256/ab"), at(1)),
    ] {
        let snapshot = create(&root, &store, &blobs, &dir, now).await.unwrap();
        let entries = read_archive(&snapshot.archive);
        let stem = manual_stem(now);
        assert!(
            entries.keys().all(|name| !name.contains(&stem)),
            "{:?}",
            entries.keys().collect::<Vec<_>>()
        );
        assert!(entries.contains_key("giles.toml"));
    }
}

#[tokio::test]
async fn snapshot_files_are_private() {
    let (_dir, root) = data_root();
    let (store, blobs) = open_stores(&root).await;
    let out = tempfile::tempdir().unwrap();
    let snapshot = create(&root, &store, &blobs, out.path(), at(0))
        .await
        .unwrap();
    assert_eq!(mode(&snapshot.database), 0o600);
    assert_eq!(mode(&snapshot.archive), 0o600);
}

#[tokio::test]
async fn a_backup_never_overwrites_an_existing_one() {
    let (_dir, root) = data_root();
    let (store, blobs) = open_stores(&root).await;
    let out = tempfile::tempdir().unwrap();
    let first = create(&root, &store, &blobs, out.path(), at(0))
        .await
        .unwrap();
    let before = fs::read(&first.database).unwrap();

    let err = create(&root, &store, &blobs, out.path(), at(0))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, BackupError::Exists(p) if *p == first.database),
        "{err}"
    );
    assert_eq!(fs::read(&first.database).unwrap(), before);
    assert!(first.archive.metadata().unwrap().len() > 0);

    // A leftover archive alone blocks the name too, and the database file isn't left behind.
    let stem = manual_stem(at(5));
    put(&out.path().join(format!("{stem}.tar")), "x");
    let err = create(&root, &store, &blobs, out.path(), at(5))
        .await
        .unwrap_err();
    assert!(matches!(err, BackupError::Exists(_)), "{err}");
    assert!(!out.path().join(format!("{stem}.db")).exists());
}

#[test]
fn names_carry_the_utc_time_and_sort_by_it() {
    assert_eq!(manual_stem(at(0)), "giles-20270115T080000Z");
    assert_eq!(
        pre_migration_stem(at(0), 2),
        "giles-20270115T080000Z-pre-migration-v2"
    );
    assert!(manual_stem(at(0)) < manual_stem(at(1)));
    assert!(manual_stem(at(59)) < manual_stem(at(61)));
}

#[test]
fn pruning_keeps_the_newest_pre_migration_snapshots_and_nothing_else_is_touched() {
    let dir = tempfile::tempdir().unwrap();
    let stems: Vec<String> = (0..7)
        .map(|i| pre_migration_stem(at(i * 3600), 1 + i))
        .collect();
    for stem in &stems {
        put(&dir.path().join(format!("{stem}.db")), "db");
        put(&dir.path().join(format!("{stem}.tar")), "tar");
    }
    // A snapshot missing its other half is pruned with its stem; unrelated and look-alike
    // names are never touched, however old.
    let others = [
        "giles-20200101T000000Z.db",
        "giles-20200101T000000Z.tar",
        "giles-20200101T000000Z-pre-migration-v.db",
        "giles-2020-pre-migration-v1.db",
        "giles-20200101T000000Z-pre-migration-v1.db.bak",
        "notes.txt",
    ];
    for name in others {
        put(&dir.path().join(name), "keep");
    }

    let removed = prune_pre_migration(dir.path(), 5).unwrap();
    let expected: Vec<PathBuf> = stems[..2]
        .iter()
        .flat_map(|stem| {
            [
                dir.path().join(format!("{stem}.db")),
                dir.path().join(format!("{stem}.tar")),
            ]
        })
        .collect();
    assert_eq!(removed, expected);
    for stem in &stems[2..] {
        assert!(dir.path().join(format!("{stem}.db")).exists(), "{stem}");
        assert!(dir.path().join(format!("{stem}.tar")).exists(), "{stem}");
    }
    for name in others {
        assert!(dir.path().join(name).exists(), "{name}");
    }

    // Nothing to do at or under the limit.
    assert!(prune_pre_migration(dir.path(), 5).unwrap().is_empty());
}

/// Rewinds the database at `path` to schema version 1, as an older binary left it.
fn downgrade_to_v1(path: &Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "DROP TABLE toolsets; DROP TABLE routes_imported; DROP TABLE routes; DROP TABLE blobs;
         PRAGMA user_version = 1;",
    )
    .unwrap();
}

fn user_version(path: &Path) -> i64 {
    rusqlite::Connection::open(path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap()
}

/// The `.db` and `.tar` files in `backups/`, by name.
fn backups(root: &DataRoot) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(root.backups_dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn a_backup_is_written_before_migrations_only_when_some_are_pending() {
    let (_dir, root) = data_root();
    populate(&root);
    fs::remove_file(root.backups_dir().join("old.tar")).unwrap();

    // A new database has nothing to lose: no backup.
    let store = Store::open_with(root.database_file(), open_options(&root))
        .await
        .unwrap();
    assert!(backups(&root).is_empty());
    store
        .job_set_last_run(
            AgentId::new("main").unwrap(),
            JobKey::Config("keep".into()),
            at(7),
        )
        .await
        .unwrap();
    drop(store);

    // An up-to-date database runs no migration: no backup.
    let store = Store::open_with(root.database_file(), open_options(&root))
        .await
        .unwrap();
    drop(store);
    assert!(backups(&root).is_empty());

    // One that is behind is backed up first, as it was, then migrated.
    downgrade_to_v1(&root.database_file());
    let store = Store::open_with(root.database_file(), open_options(&root))
        .await
        .unwrap();
    assert_eq!(
        user_version(&root.database_file()),
        giles_store::SCHEMA_VERSION
    );
    let names = backups(&root);
    assert_eq!(names.len(), 2, "{names:?}");
    let stem = names[0]
        .strip_suffix(".db")
        .expect("the database sorts first");
    assert!(
        stem.starts_with("giles-") && stem.ends_with("-pre-migration-v1"),
        "{stem}"
    );
    assert_eq!(names[1], format!("{stem}.tar"));
    let database = root.backups_dir().join(&names[0]);
    assert_eq!(user_version(&database), 1, "the snapshot is the old schema");
    assert_eq!(mode(&database), 0o600);
    let archive = read_archive(&root.backups_dir().join(&names[1]));
    assert!(archive.contains_key("giles.toml"));
    assert!(archive.contains_key("data/blobs/sha256/ab/abcdef"));
    assert!(!archive.contains_key("secrets.toml"));
    // The rows survived into both the snapshot and the migrated database.
    assert_eq!(job_run(&store, "keep").await, Some(at(7)));
    let old = rusqlite::Connection::open(&database).unwrap();
    let rows: i64 = old
        .query_row("SELECT count(*) FROM job_runs", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 1);
}

#[tokio::test]
async fn migrations_do_not_run_when_the_backup_fails() {
    let (_dir, root) = data_root();
    drop(Store::open(root.database_file()).await.unwrap());
    downgrade_to_v1(&root.database_file());
    // Something that isn't a directory where `backups/` should be.
    fs::remove_dir(root.backups_dir()).unwrap();
    put(&root.backups_dir(), "in the way");

    let err = Store::open_with(root.database_file(), open_options(&root))
        .await
        .unwrap_err();
    assert!(matches!(err, giles_store::Error::BeforeMigrate(_)), "{err}");
    assert_eq!(
        user_version(&root.database_file()),
        1,
        "still the old schema"
    );
}

#[tokio::test]
async fn only_the_newest_pre_migration_backups_are_kept() {
    let (_dir, root) = data_root();
    drop(Store::open(root.database_file()).await.unwrap());
    // Seven older snapshots, as earlier upgrades left them.
    for i in 0..7 {
        let stem = pre_migration_stem(at(i - 1000), 1);
        put(&root.backups_dir().join(format!("{stem}.db")), "db");
        put(&root.backups_dir().join(format!("{stem}.tar")), "tar");
    }
    put(
        &root.backups_dir().join("giles-20200101T000000Z.db"),
        "manual",
    );
    downgrade_to_v1(&root.database_file());
    drop(
        Store::open_with(root.database_file(), open_options(&root))
            .await
            .unwrap(),
    );

    let names = backups(&root);
    let pre_migration = names
        .iter()
        .filter(|n| n.contains("-pre-migration-"))
        .count();
    assert_eq!(pre_migration, 2 * KEEP_PRE_MIGRATION, "{names:?}");
    // The new one is among them, the oldest are gone, and the manual backup is untouched.
    assert!(
        names
            .iter()
            .any(|n| n.ends_with("-pre-migration-v1.db") && n.as_str() > "giles-2027")
    );
    assert!(!names.contains(&format!("{}.db", pre_migration_stem(at(-1000), 1))));
    assert!(names.contains(&"giles-20200101T000000Z.db".to_owned()));
}

#[tokio::test]
async fn a_backup_directory_reached_through_a_symlink_does_not_archive_itself() {
    // The data root is `real`; the operator names the directory through a link to it, as
    // `getcwd` and `~/.giles` being a link can make happen.
    let (_dir, root) = data_root();
    populate(&root);
    let (store, blobs) = open_stores(&root).await;
    let links = tempfile::tempdir().unwrap();
    symlink(root.root(), links.path().join("link")).unwrap();
    let dir = links.path().join("link/agents");

    // Before the fix this read its own growing archive until the disk was full.
    let snapshot = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        create(&root, &store, &blobs, &dir, at(0)),
    )
    .await
    .expect("the backup finished")
    .unwrap();
    assert_eq!(snapshot.archive.parent().unwrap(), dir);
    let entries = read_archive(&snapshot.archive);
    assert!(
        entries.keys().all(|name| !name.contains("giles-2027")),
        "{:?}",
        entries.keys().collect::<Vec<_>>()
    );
    assert!(entries.contains_key("agents/main/SOUL.md"));
    assert!(snapshot.archive.metadata().unwrap().len() < 1 << 20);
}

/// Collects what `tracing` logs.
#[derive(Clone, Default)]
struct Logs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
    type Writer = Logs;

    fn make_writer(&'a self) -> Logs {
        self.clone()
    }
}

#[test]
fn symlinks_are_left_out_with_a_warning_each() {
    let (_dir, root) = data_root();
    populate(&root);
    // A dotfiles manager's `giles.toml`, a linked agent file, and a linked directory.
    let elsewhere = tempfile::tempdir().unwrap();
    put(&elsewhere.path().join("real.toml"), "[owner]\n");
    put(&elsewhere.path().join("memory.md"), "remember\n");
    put(&elsewhere.path().join("dir/file.md"), "in a dir\n");
    fs::remove_file(root.config_file()).unwrap();
    symlink(elsewhere.path().join("real.toml"), root.config_file()).unwrap();
    let main = root.agent_dir("main").unwrap();
    symlink(elsewhere.path().join("memory.md"), main.join("MEMORY.md")).unwrap();
    symlink(elsewhere.path().join("dir"), main.join("linked")).unwrap();

    let out = tempfile::tempdir().unwrap();
    let snapshot = Snapshot::reserve(out.path(), "giles-test").unwrap();
    let logs = Logs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_ansi(false)
        .finish();
    let memory = MemorySnapshots::default();
    tracing::subscriber::with_default(subscriber, || snapshot.write_archive(&root, &memory))
        .unwrap();

    let entries = read_archive(&snapshot.archive);
    let names: Vec<&str> = entries.keys().map(String::as_str).collect();
    // No entry for any link (and so no link to restore dangling), the rest as before.
    assert!(!names.contains(&"giles.toml"), "{names:?}");
    assert!(
        names
            .iter()
            .all(|n| *n != "agents/main/MEMORY.md" && !n.starts_with("agents/main/linked")),
        "{names:?}"
    );
    assert!(names.contains(&"agents/main/SOUL.md"));

    let logged = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    for link in [
        root.config_file(),
        main.join("MEMORY.md"),
        main.join("linked"),
    ] {
        assert!(
            logged.contains("WARN") && logged.contains(&link.display().to_string()),
            "no warning for {}:\n{logged}",
            link.display()
        );
    }
    assert_eq!(logged.matches("WARN").count(), 3, "{logged}");
}

#[test]
fn an_entry_is_exactly_the_size_its_header_recorded() {
    use std::io::Read;
    // A file that grew after its header was written is cut at the recorded size.
    let mut longer = ExactReader::new(&b"0123456789"[..], 4, Path::new("grew"));
    let mut text = String::new();
    longer.read_to_string(&mut text).unwrap();
    assert_eq!(text, "0123");

    // One that shrank is an error, not a short entry that corrupts the ones after it.
    let mut shorter = ExactReader::new(&b"01"[..], 4, Path::new("memory/MEMORY.md"));
    let err = shorter.read_to_end(&mut Vec::new()).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    assert!(err.to_string().contains("memory/MEMORY.md"), "{err}");
}

#[tokio::test]
async fn the_blob_sweep_waits_while_a_backup_runs() {
    use giles_store::BlobMeta;
    let (_dir, root) = data_root();
    let (store, blobs) = open_stores(&root).await;
    // Old and unreferenced: the sweep would delete it, and the snapshot's rows name it.
    let old = blobs
        .put(
            &b"old attachment"[..],
            1024,
            BlobMeta {
                media_type: "text/plain".into(),
                name: None,
            },
            Timestamp::from_second(0).unwrap(),
        )
        .await
        .unwrap();

    let out = tempfile::tempdir().unwrap();
    let swept = std::sync::Arc::new(std::sync::Mutex::new(None));
    let slot = swept.clone();
    let sweeper = blobs.clone();
    let snapshot = create_detached(&root, &store, &blobs, out.path(), at(0), async move {
        // The window between the database snapshot and the archive walk.
        let handle = tokio::spawn(async move {
            sweeper
                .sweep(at(0), std::time::Duration::from_secs(48 * 3600))
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(!handle.is_finished(), "the sweep ran during the backup");
        *slot.lock().unwrap() = Some(handle);
    })
    .await
    .unwrap();

    // The archive got the blob the snapshot's rows refer to, and the sweep then removed it.
    let id = old.id.as_str();
    let name = format!("data/blobs/sha256/{}/{id}", &id[..2]);
    assert_eq!(read_archive(&snapshot.archive)[&name].0, "old attachment");
    let handle = swept.lock().unwrap().take().unwrap();
    let report = handle.await.unwrap().unwrap();
    assert_eq!(report.blobs, 1);
    assert!(blobs.get(&old.id).await.unwrap().is_none());
}

#[tokio::test]
async fn a_backup_finishes_when_its_caller_goes_away() {
    // The admin service's handler future is dropped when the client hangs up.
    let (_dir, root) = data_root();
    populate(&root);
    let (store, blobs) = open_stores(&root).await;
    let out = tempfile::tempdir().unwrap();
    let gate = std::sync::Arc::new(tokio::sync::Notify::new());
    let held = gate.clone();

    // The caller gives up once the backup has started, with the database written and the
    // archive not yet (the task waits at the gate).
    let abandoned = tokio::time::timeout(
        std::time::Duration::from_millis(300),
        create_detached(&root, &store, &blobs, out.path(), at(0), async move {
            held.notified().await;
        }),
    )
    .await;
    assert!(abandoned.is_err(), "the caller was dropped mid-backup");
    let listing = || {
        let mut names: Vec<_> = fs::read_dir(out.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        names.sort();
        names
    };
    let [database, archive] = &listing()[..] else {
        panic!("{:?}", listing());
    };
    assert!(
        database.metadata().unwrap().len() > 0,
        "the database is written"
    );
    assert_eq!(
        archive.metadata().unwrap().len(),
        0,
        "the archive isn't, yet"
    );
    // The work still holds the sweep off.
    let sweep = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        blobs.lock_for_sweep(),
    )
    .await;
    assert!(
        sweep.is_err(),
        "the sweep hold was released with the caller"
    );

    // Let it finish. Once the hold is gone the work has ended: a complete pair, no leftovers.
    gate.notify_one();
    let _sweep = tokio::time::timeout(std::time::Duration::from_secs(30), blobs.lock_for_sweep())
        .await
        .expect("the backup released the hold");
    let [database, archive] = &listing()[..] else {
        panic!("{:?}", listing());
    };
    let restored = tempfile::tempdir().unwrap();
    fs::copy(database, restored.path().join("giles.db")).unwrap();
    Store::open(restored.path().join("giles.db")).await.unwrap();
    assert!(read_archive(archive).contains_key("giles.toml"));
}

/// The modes with a rollback journal; the default WAL is covered by the tests above.
const ROLLBACK_MODES: [JournalMode; 3] = [
    JournalMode::Delete,
    JournalMode::Truncate,
    JournalMode::Persist,
];

#[tokio::test]
async fn a_snapshot_taken_in_a_rollback_journal_mode_restores_in_that_mode_and_in_wal() {
    for mode in ROLLBACK_MODES {
        let (_dir, root) = data_root();
        let options = open_options(&root).journal_mode(mode);
        let store = Store::open_with(root.database_file(), options)
            .await
            .unwrap();
        let blobs = BlobStore::open(root.blobs_dir(), store.clone())
            .await
            .unwrap();
        let agent = || AgentId::new("main").unwrap();
        for (i, job) in ["a", "b", "c"].into_iter().enumerate() {
            store
                .job_set_last_run(agent(), JobKey::Config(job.into()), at(i as i64))
                .await
                .unwrap();
        }
        assert!(!root.data_dir().join("giles.db-wal").exists(), "{mode}");

        let out = tempfile::tempdir().unwrap();
        let snapshot = create(&root, &store, &blobs, out.path(), at(100))
            .await
            .unwrap();
        // Writes after the snapshot stay out of it, and the daemon went on serving.
        store
            .job_set_last_run(agent(), JobKey::Config("later".into()), at(200))
            .await
            .unwrap();
        for leftover in ["-wal", "-journal"] {
            let mut name = snapshot.database.clone().into_os_string();
            name.push(leftover);
            assert!(!PathBuf::from(name).exists(), "{mode}: {leftover}");
        }

        for target in [mode, JournalMode::Wal] {
            let restored = tempfile::tempdir().unwrap();
            let restored_db = restored.path().join("giles.db");
            fs::copy(&snapshot.database, &restored_db).unwrap();
            let restored = Store::open_with(
                &restored_db,
                giles_store::OpenOptions::new().journal_mode(target),
            )
            .await
            .unwrap();
            for (i, job) in ["a", "b", "c"].into_iter().enumerate() {
                let run = job_run(&restored, job).await;
                assert_eq!(run, Some(at(i as i64)), "{mode} -> {target}: {job}");
            }
            assert_eq!(
                job_run(&restored, "later").await,
                None,
                "{mode} -> {target}"
            );
            restored
                .job_set_last_run(agent(), JobKey::Config("new".into()), at(300))
                .await
                .unwrap();
            assert_eq!(job_run(&restored, "new").await, Some(at(300)));
        }
    }
}

#[tokio::test]
async fn the_pre_migration_backup_is_consistent_in_a_rollback_journal_mode() {
    for mode in ROLLBACK_MODES {
        let (_dir, root) = data_root();
        let options = || open_options(&root).journal_mode(mode);
        let store = Store::open_with(root.database_file(), options())
            .await
            .unwrap();
        store
            .job_set_last_run(
                AgentId::new("main").unwrap(),
                JobKey::Config("keep".into()),
                at(7),
            )
            .await
            .unwrap();
        drop(store);
        downgrade_to_v1(&root.database_file());

        let store = Store::open_with(root.database_file(), options())
            .await
            .unwrap();
        assert_eq!(job_run(&store, "keep").await, Some(at(7)), "{mode}");
        let names = backups(&root);
        assert_eq!(names.len(), 2, "{mode}: {names:?}");
        let database = root.backups_dir().join(&names[0]);
        assert_eq!(
            user_version(&database),
            1,
            "{mode}: the snapshot is the old schema"
        );
        let rows: i64 = rusqlite::Connection::open(&database)
            .unwrap()
            .query_row("SELECT count(*) FROM job_runs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1, "{mode}");
        // Only the snapshot and its archive, no journal beside them.
        assert!(
            names
                .iter()
                .all(|n| n.ends_with(".db") || n.ends_with(".tar")),
            "{names:?}"
        );
    }
}

const CORE_CAP: usize = 2200;

fn owner_viewer() -> MemoryViewer {
    MemoryViewer::new(MemoryOwner::User(UserId::new("owner").unwrap()), true)
}

fn family_viewer() -> MemoryViewer {
    MemoryViewer::new(
        MemoryOwner::Group {
            instance: "telegram".to_owned(),
            conversation: ConversationId::new("family").unwrap(),
        },
        false,
    )
}

/// Opens an agent's memory database as the core will: by its path in the data root, in WAL mode.
async fn open_memory(root: &DataRoot, agent: &str) -> MemoryStore {
    MemoryStore::open(
        root.agent_memory_db(agent).unwrap(),
        giles_store::OpenOptions::new(),
    )
    .await
    .unwrap()
}

async fn remember(
    store: &MemoryStore,
    viewer: &MemoryViewer,
    content: &str,
    shared: bool,
) -> Memory {
    let new = NewMemory {
        content: content.to_owned(),
        core: false,
        shared,
        source_session: None,
    };
    store.add(viewer, new, CORE_CAP, at(1)).await.unwrap()
}

/// Unpacks `archive` over a fresh data root, as the restore instructions do.
fn restore(archive: &Path) -> (TempDir, DataRoot) {
    let (dir, fresh) = data_root();
    tar::Archive::new(fs::File::open(archive).unwrap())
        .unpack(fresh.root())
        .unwrap();
    (dir, fresh)
}

/// The names under the data root's `cache/` (where the memory snapshots are staged).
fn cache_names(root: &DataRoot) -> Vec<String> {
    fs::read_dir(root.cache_dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect()
}

#[tokio::test]
async fn a_backup_and_restore_keep_every_agents_memories() {
    let (_dir, root) = data_root();
    populate(&root);
    // The running core's databases, with their latest writes still in the WAL.
    let main = open_memory(&root, "main").await;
    let helper = open_memory(&root, "helper").await;
    let main_user = remember(&main, &owner_viewer(), "likes tea", false).await;
    let main_shared = remember(&main, &owner_viewer(), "lives in Leeds", true).await;
    let main_group = remember(
        &main,
        &family_viewer(),
        "the family picnic is on Sunday",
        false,
    )
    .await;
    let helper_user = remember(&helper, &owner_viewer(), "prefers short answers", false).await;
    let helper_group = remember(&helper, &family_viewer(), "grandma is vegetarian", false).await;
    for agent in ["main", "helper"] {
        let db = root.agent_memory_db(agent).unwrap();
        assert!(db.with_file_name("memory.db-wal").exists(), "{agent}");
    }
    // An agent without a memory database, and a removed agent's, which is in the trash.
    put(
        &root.agent_dir("plain").unwrap().join("agent.toml"),
        "model = \"x\"\n",
    );
    put(
        &root
            .trash_dir()
            .join("agents/jeeves-20261024T120000Z/memory.db"),
        "a removed agent's memories",
    );
    let (store, blobs) = open_stores(&root).await;

    let out = tempfile::tempdir().unwrap();
    let snapshot = create(&root, &store, &blobs, out.path(), at(0))
        .await
        .unwrap();
    // The core carries on: it writes after the backup, and its WAL is still there.
    let later = remember(&main, &owner_viewer(), "written after the backup", false).await;
    for agent in ["main", "helper"] {
        let db = root.agent_memory_db(agent).unwrap();
        assert!(db.with_file_name("memory.db-wal").exists(), "{agent}");
    }

    // One snapshot of each database, at its place, a database and not the live file; none of the
    // files SQLite keeps beside it, and nothing from the trash.
    let entries = read_archive(&snapshot.archive);
    let names: Vec<&str> = entries.keys().map(String::as_str).collect();
    let databases: Vec<&str> = names
        .iter()
        .copied()
        .filter(|name| name.contains("memory.db"))
        .collect();
    assert_eq!(
        databases,
        ["agents/helper/memory.db", "agents/main/memory.db"],
        "{names:?}"
    );
    assert!(
        names.iter().all(|name| !name.starts_with("trash")),
        "{names:?}"
    );
    assert!(names.contains(&"agents/plain/agent.toml"));
    for name in databases {
        assert!(
            entries[name].0.starts_with("SQLite format 3"),
            "{name} is not a database file"
        );
        assert_eq!(entries[name].1 & 0o777, 0o600, "{name}");
    }
    // Nothing is left staged, and the live databases are the daemon's, as they were.
    assert!(
        cache_names(&root)
            .iter()
            .all(|name| !name.starts_with(MEMORY_STAGE_PREFIX)),
        "{:?}",
        cache_names(&root)
    );

    // Restoring is unpacking the archive: every database opens with what it held.
    let (_fresh_dir, fresh) = restore(&snapshot.archive);
    let main_restored = open_memory(&fresh, "main").await;
    let helper_restored = open_memory(&fresh, "helper").await;
    for memory in [&main_user, &main_shared, &main_group] {
        let viewer = if memory.owner == family_viewer().owner {
            family_viewer()
        } else {
            owner_viewer()
        };
        assert_eq!(
            &main_restored.get(&viewer, memory.id).await.unwrap(),
            memory
        );
    }
    for memory in [&helper_user, &helper_group] {
        let viewer = if memory.owner == family_viewer().owner {
            family_viewer()
        } else {
            owner_viewer()
        };
        assert_eq!(
            &helper_restored.get(&viewer, memory.id).await.unwrap(),
            memory
        );
    }
    // The group memory is still the group's, and not visible to the owner's direct sessions.
    assert!(
        main_restored
            .get(&owner_viewer(), main_group.id)
            .await
            .is_err()
    );
    // Writes after the snapshot stay out of it, and the restored database is a working one.
    assert!(main_restored.get(&owner_viewer(), later.id).await.is_err());
    remember(
        &main_restored,
        &owner_viewer(),
        "new after the restore",
        false,
    )
    .await;
}

#[tokio::test]
async fn memory_databases_in_a_rollback_journal_mode_are_backed_up_too() {
    for mode in ROLLBACK_MODES {
        let (_dir, root) = data_root();
        let main = MemoryStore::open(
            root.agent_memory_db("main").unwrap(),
            giles_store::OpenOptions::new().journal_mode(mode),
        )
        .await
        .unwrap();
        let memory = remember(&main, &owner_viewer(), "likes tea", false).await;
        let (store, blobs) = open_stores(&root).await;

        let out = tempfile::tempdir().unwrap();
        let snapshot = create(&root, &store, &blobs, out.path(), at(0))
            .await
            .unwrap();
        // The live store still works after the backup.
        remember(&main, &owner_viewer(), "and coffee", false).await;

        let entries = read_archive(&snapshot.archive);
        let databases: Vec<&String> = entries.keys().filter(|n| n.contains("memory.db")).collect();
        assert_eq!(databases, ["agents/main/memory.db"], "{mode}");
        let (_fresh_dir, fresh) = restore(&snapshot.archive);
        let restored = open_memory(&fresh, "main").await;
        assert_eq!(
            restored.get(&owner_viewer(), memory.id).await.unwrap(),
            memory,
            "{mode}"
        );
    }
}

#[tokio::test]
async fn a_memory_database_that_cannot_be_snapshotted_fails_the_backup_and_leaves_nothing() {
    let (_dir, root) = data_root();
    populate(&root);
    let good = open_memory(&root, "alpha").await;
    remember(&good, &owner_viewer(), "likes tea", false).await;
    // Not a database at all.
    put(
        &root.agent_memory_db("beta").unwrap(),
        &"this is not a database, just text".repeat(100),
    );
    let (store, blobs) = open_stores(&root).await;

    let out = tempfile::tempdir().unwrap();
    let err = create(&root, &store, &blobs, out.path(), at(0))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, BackupError::MemoryDatabase { path, .. }
            if *path == root.agent_memory_db("beta").unwrap()),
        "{err}"
    );
    assert!(err.to_string().contains("memory.db"), "{err}");
    // The partial snapshot is discarded, with the database taken before the failure, and the
    // first agent's snapshot is not left staged.
    assert_eq!(fs::read_dir(out.path()).unwrap().count(), 0);
    assert!(
        cache_names(&root)
            .iter()
            .all(|name| !name.starts_with(MEMORY_STAGE_PREFIX)),
        "{:?}",
        cache_names(&root)
    );
    // The file that wasn't a database is untouched.
    assert!(
        fs::read_to_string(root.agent_memory_db("beta").unwrap())
            .unwrap()
            .starts_with("this is not a database")
    );
}

#[tokio::test]
async fn the_pre_migration_backup_carries_the_memory_databases() {
    let (_dir, root) = data_root();
    let main = open_memory(&root, "main").await;
    let memory = remember(&main, &owner_viewer(), "likes tea", false).await;
    drop(Store::open(root.database_file()).await.unwrap());
    downgrade_to_v1(&root.database_file());

    // The memory database is open with its writes in the WAL while the core migrates.
    let store = Store::open_with(root.database_file(), open_options(&root))
        .await
        .unwrap();
    drop(store);
    let names = backups(&root);
    assert_eq!(names.len(), 2, "{names:?}");
    let archive = root.backups_dir().join(&names[1]);
    let entries = read_archive(&archive);
    let databases: Vec<&String> = entries.keys().filter(|n| n.contains("memory.db")).collect();
    assert_eq!(databases, ["agents/main/memory.db"]);

    let (_fresh_dir, fresh) = restore(&archive);
    let restored = open_memory(&fresh, "main").await;
    assert_eq!(
        restored.get(&owner_viewer(), memory.id).await.unwrap(),
        memory
    );
    assert!(
        cache_names(&root)
            .iter()
            .all(|name| !name.starts_with(MEMORY_STAGE_PREFIX))
    );
}

#[tokio::test]
async fn a_symlinked_memory_database_is_left_out_with_a_warning() {
    let (_dir, root) = data_root();
    populate(&root);
    let main = open_memory(&root, "main").await;
    remember(&main, &owner_viewer(), "likes tea", false).await;
    // A dotfiles manager's memory database for another agent, and a linked agent directory.
    let elsewhere = tempfile::tempdir().unwrap();
    let linked_db = MemoryStore::open(
        elsewhere.path().join("memory.db"),
        giles_store::OpenOptions::new(),
    )
    .await
    .unwrap();
    remember(&linked_db, &owner_viewer(), "elsewhere", false).await;
    let linked = root.agent_dir("linked").unwrap();
    fs::create_dir_all(&linked).unwrap();
    symlink(
        elsewhere.path().join("memory.db"),
        root.agent_memory_db("linked").unwrap(),
    )
    .unwrap();
    let linked_dir = root.agent_dir("linked-dir").unwrap();
    symlink(elsewhere.path(), &linked_dir).unwrap();

    let out = tempfile::tempdir().unwrap();
    let snapshot = Snapshot::reserve(out.path(), "giles-test").unwrap();
    let memory = MemorySnapshots::take(&root).await.unwrap();
    let logs = Logs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_ansi(false)
        .finish();
    tracing::subscriber::with_default(subscriber, || snapshot.write_archive(&root, &memory))
        .unwrap();

    let entries = read_archive(&snapshot.archive);
    let databases: Vec<&String> = entries.keys().filter(|n| n.contains("memory.db")).collect();
    assert_eq!(databases, ["agents/main/memory.db"]);
    let logged = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    for link in [root.agent_memory_db("linked").unwrap(), linked_dir] {
        assert!(
            logged.contains("WARN") && logged.contains(&link.display().to_string()),
            "no warning for {}:\n{logged}",
            link.display()
        );
    }
    assert_eq!(logged.matches("WARN").count(), 2, "{logged}");
}

//! The renames done with Giles stopped: `giles channel rename`, `giles provider rename`, and
//! `giles toolset rename` (when no daemon runs). One driver serves them all; the command gives the
//! kind. It renames an instance in the database, the data directories, and the configuration
//! (PLAN section 7.2).

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, bail};
use giles_core::admin::{InstanceLock, LockError};
use giles_core::config::Kind;
use giles_core::paths::DataRoot;
use giles_core::rename::{RenameReport, rename_instance};
use giles_store::{OpenOptions, Store};

use crate::cli::RenameArgs;

/// A data root no daemon runs on. Holding it holds the data root's lock, which proves that and
/// keeps a daemon from starting meanwhile.
pub struct Stopped {
    root: DataRoot,
    lock: InstanceLock,
}

/// Whether a daemon runs on the data root.
pub enum Core {
    /// None does, and the caller holds the lock.
    Stopped(Stopped),
    /// One does: the lock is held, as the error says.
    Running(LockError),
}

/// Finds out whether a daemon runs on the data root (`--home`, `GILES_HOME`, or `~/.giles`) by
/// taking its lock. A mistyped data root, with no `giles.toml`, is an error that creates nothing.
pub fn lock(home: Option<&Path>) -> anyhow::Result<Core> {
    let root = DataRoot::from_env(home)?;
    if !root.config_file().is_file() {
        bail!(
            "no giles.toml in {}: is this the right data root? (set --home or GILES_HOME)",
            root.root().display()
        );
    }
    root.create_layout()?;
    match InstanceLock::acquire(&root) {
        Ok(lock) => Ok(Core::Stopped(Stopped { root, lock })),
        Err(err @ LockError::Held { .. }) => Ok(Core::Running(err)),
        Err(err) => Err(err.into()),
    }
}

/// Renames a `kind` instance with Giles stopped: refuses, with the lock's error, if a daemon
/// runs.
pub fn run(home: Option<&Path>, kind: Kind, args: &RenameArgs) -> anyhow::Result<()> {
    match lock(home)? {
        Core::Stopped(stopped) => stopped.rename(kind, args),
        Core::Running(err) => Err(err.into()),
    }
}

impl Stopped {
    /// Renames the `kind` instance and prints what changed.
    pub fn rename(&self, kind: Kind, args: &RenameArgs) -> anyhow::Result<()> {
        let root = &self.root;
        let report = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("cannot start the async runtime")?
            .block_on(async {
                // The configuration isn't loaded here (it may be half renamed), so leave the
                // journal mode the daemon chose as it is.
                let store =
                    Store::open_with(root.database_file(), OpenOptions::new().keep_journal_mode())
                        .await
                        .with_context(|| {
                            format!("cannot open {}", root.database_file().display())
                        })?;
                rename_instance(root, &store, &self.lock, kind, &args.old, &args.new)
                    .await
                    .map_err(anyhow::Error::new)
            })?;
        print!("{}", format_report(root, &args.old, &args.new, &report));
        Ok(())
    }
}

/// What was renamed, one line per kind of change, with paths relative to the data root.
fn format_report(root: &DataRoot, old: &str, new: &str, report: &RenameReport) -> String {
    let relative = |path: &Path| {
        path.strip_prefix(root.root())
            .unwrap_or(path)
            .display()
            .to_string()
    };
    let rows = &report.rows;
    let mut text = format!("Renamed {} instance {old:?} to {new:?}.\n", report.kind);
    // A toolset has no rows.
    if report.kind != Kind::Toolset {
        let _ = writeln!(
            text,
            "  database: {} sessions, {} inbox rows, {} outbox rows, {} usage rows, {} messages, \
             {} routes",
            rows.sessions, rows.inbox, rows.outbox, rows.usage, rows.messages, rows.routes
        );
    }
    if report.stored_toolset {
        let _ = writeln!(
            text,
            "  database: the stored toolset {old:?} is now {new:?}"
        );
    } else if report.config_table {
        let _ = writeln!(
            text,
            "  giles.toml: [{kind}.{old}] -> [{kind}.{new}]",
            kind = report.kind
        );
    } else {
        let _ = writeln!(
            text,
            "  giles.toml: [{}.{new}] was already renamed",
            report.kind
        );
    }
    if report.legacy_routes > 0 {
        let _ = writeln!(
            text,
            "  giles.toml: {} [[routes]] rule(s) updated (Giles no longer reads them once they \
             are imported)",
            report.legacy_routes
        );
    }
    for (path, changes) in &report.agent_files {
        let _ = writeln!(text, "  {}: {changes} reference(s) updated", relative(path));
    }
    for (path, memories) in &report.memory_dbs {
        let _ = writeln!(
            text,
            "  {}: {memories} group memor{} moved to {new:?}",
            relative(path),
            if *memories == 1 { "y" } else { "ies" }
        );
    }
    for (from, to) in &report.moved {
        let _ = writeln!(text, "  moved {} -> {}", relative(from), relative(to));
    }
    text.push_str("Start giles again to use the new name.\n");
    text
}

#[cfg(test)]
mod tests {
    use std::fs;

    use giles_core::config::Kind;
    use giles_store::RenamedRows;

    use super::*;

    fn root() -> DataRoot {
        DataRoot::new("/g", Path::new("/tmp")).unwrap()
    }

    #[test]
    fn the_report_names_every_change_relative_to_the_data_root() {
        let report = RenameReport {
            kind: Kind::Channel,
            rows: RenamedRows {
                sessions: 3,
                inbox: 10,
                outbox: 0,
                usage: 0,
                messages: 0,
                routes: 4,
            },
            config_table: true,
            stored_toolset: false,
            legacy_routes: 2,
            agent_files: vec![(Path::new("/g/agents/jeeves/agent.toml").to_owned(), 1)],
            moved: vec![(
                Path::new("/g/data/channel/libera").to_owned(),
                Path::new("/g/data/channel/libera-chat").to_owned(),
            )],
            memory_dbs: vec![(Path::new("/g/agents/jeeves/memory.db").to_owned(), 5)],
        };
        let text = format_report(&root(), "libera", "libera-chat", &report);
        let expected = [
            "Renamed channel instance \"libera\" to \"libera-chat\".",
            "  database: 3 sessions, 10 inbox rows, 0 outbox rows, 0 usage rows, 0 messages, 4 routes",
            "  giles.toml: [channel.libera] -> [channel.libera-chat]",
            "  giles.toml: 2 [[routes]] rule(s) updated (Giles no longer reads them once they are imported)",
            "memory.db: 5 group memories moved to \"libera-chat\"",
        ];
        for line in expected {
            assert!(text.contains(line), "{text}");
        }
        let sep = std::path::MAIN_SEPARATOR;
        assert!(
            text.contains(&format!(
                "  moved data{sep}channel{sep}libera -> data{sep}channel{sep}libera-chat"
            )),
            "{text}"
        );
        assert!(text.ends_with("Start giles again to use the new name.\n"));
    }

    #[test]
    fn a_stored_toolset_is_reported_as_a_renamed_row() {
        let report = RenameReport {
            kind: Kind::Toolset,
            rows: RenamedRows::default(),
            config_table: false,
            stored_toolset: true,
            legacy_routes: 0,
            agent_files: vec![(Path::new("/g/agents/jeeves/agent.toml").to_owned(), 2)],
            moved: Vec::new(),
            memory_dbs: Vec::new(),
        };
        let text = format_report(&root(), "notes", "memos", &report);
        assert!(text.starts_with("Renamed toolset instance \"notes\" to \"memos\".\n"));
        assert!(
            text.contains("  database: the stored toolset \"notes\" is now \"memos\"\n"),
            "{text}"
        );
        assert!(!text.contains("giles.toml"), "{text}");
        assert!(!text.contains("sessions"), "a toolset has no rows: {text}");
        assert!(
            text.contains("agent.toml: 2 reference(s) updated"),
            "{text}"
        );
    }

    #[test]
    fn a_missing_config_is_reported_before_anything_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("typo");
        let args = RenameArgs {
            old: "a".into(),
            new: "b".into(),
        };
        let err = run(Some(&home), Kind::Channel, &args).unwrap_err();
        assert!(err.to_string().contains("no giles.toml"), "{err}");
        assert!(!home.exists(), "a mistyped --home must not be created");
        let err = lock(Some(&home)).map(|_| ()).unwrap_err();
        assert!(err.to_string().contains("no giles.toml"), "{err}");
        assert!(!home.exists(), "a mistyped --home must not be created");
    }

    /// A data root with a `giles.toml`, a `run/` directory, and the lock of a running daemon.
    fn running_home() -> (tempfile::TempDir, InstanceLock) {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path(), Path::new("/tmp")).unwrap();
        fs::write(
            root.config_file(),
            "[owner]\nidentities = [\"web:owner\"]\n",
        )
        .unwrap();
        fs::create_dir_all(root.run_dir()).unwrap();
        let daemon = InstanceLock::acquire(&root).unwrap();
        (dir, daemon)
    }

    #[test]
    fn a_running_daemon_blocks_the_rename() {
        let (dir, _daemon) = running_home();
        let args = RenameArgs {
            old: "a".into(),
            new: "b".into(),
        };
        for kind in Kind::ALL {
            let err = run(Some(dir.path()), kind, &args).unwrap_err();
            assert!(err.to_string().contains("already running"), "{err}");
        }
    }

    #[test]
    fn the_lock_tells_a_running_daemon_from_a_stopped_one() {
        let (dir, daemon) = running_home();
        assert!(matches!(
            lock(Some(dir.path())).unwrap(),
            Core::Running(LockError::Held { .. })
        ));
        drop(daemon);
        assert!(matches!(lock(Some(dir.path())).unwrap(), Core::Stopped(_)));
    }
}

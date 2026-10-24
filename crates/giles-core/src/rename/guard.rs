//! The startup check that refuses a rename done by hand (PLAN section 7.2).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::path::PathBuf;

use giles_store::{InstanceRows, Store};

use super::dirs::{DataDir, scan_dirs};
use crate::config::{ChannelRuntime, Config, Kind, ToolsetTransport};
use crate::paths::DataRoot;

/// A name that has data but is not configured, next to configured instances without any.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct UnusedName {
    /// The name that has data.
    pub name: String,
    /// What the data is, for the message.
    pub evidence: Vec<String>,
    /// The configured instances that have no data of their own and could be what `name` was
    /// renamed to: the ones of a kind that `name`'s data can belong to.
    pub candidates: Vec<(String, Kind)>,
}

/// The startup check couldn't pass.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum InstanceDataError {
    /// Unused names have data while a configured instance has none.
    #[error("{}", unused_message(.0))]
    Unused(Vec<UnusedName>),
    /// The database couldn't be read.
    #[error("cannot check the instance names against the database: {0}")]
    Store(#[from] giles_store::Error),
    /// A data directory couldn't be read.
    #[error("cannot read {}: {source}", .path.display())]
    Io {
        /// The directory.
        path: PathBuf,
        /// Why.
        source: io::Error,
    },
}

fn unused_message(unused: &[UnusedName]) -> String {
    use fmt::Write as _;
    let mut text = String::from(
        "giles.toml configures an instance that has no data, while a name it no longer \
         configures still has data. That looks like a rename done in giles.toml alone, which \
         would orphan the old name's sessions and files:\n",
    );
    for name in unused {
        let candidates: Vec<_> = name
            .candidates
            .iter()
            .map(|(candidate, kind)| format!("[{kind}.{candidate}]"))
            .collect();
        let _ = writeln!(
            text,
            "  {:?} has {}; configured without data: {}",
            name.name,
            name.evidence.join(", "),
            candidates.join(", ")
        );
    }
    let first = &unused[0];
    let _ = write!(
        text,
        "If it was a rename, stop giles and run `giles instance rename {} {}`, which moves the \
         data to the new name. If the old instance was removed on purpose, keep its data and \
         silence this check by listing the name in giles.toml: `[core] retired_instances = \
         [{:?}]`.",
        first.name, first.candidates[0].0, first.name
    );
    text
}

/// Refuses to start while `config` configures an instance with no data of its own and some other
/// name that `config` doesn't configure still has data (PLAN section 7.2): the usual way that
/// happens is renaming `[channel.libera]` in `giles.toml` without `giles instance rename`, which
/// would silently strand the old sessions, inbox, and directories.
///
/// What counts, precisely:
///
/// - A name *has data* if it is in the database (`sessions.channel`, `inbox.instance`,
///   `outbox.instance`, `usage.provider`) or is a directory `data/plugins/<name>` or
///   `agents/*/groups/<name>`.
/// - A configured instance is *fresh* if its name has no data.
/// - An *unused name* has data, isn't configured, and isn't in `[core] retired_instances`.
/// - They *match* if the kinds fit: database rows of a channel (sessions, inbox, outbox) belong to
///   a channel, `usage` rows to a provider, a `groups` directory to a channel, and a
///   `data/plugins` directory to a channel or a toolset (v1 providers are built in and have none).
///
/// The check fails if any unused name matches a fresh instance. Unused names with no fresh match
/// (an instance was removed and nothing replaced it) and fresh instances with no unused name
/// (an instance was added) both start normally.
///
/// # Errors
///
/// [`InstanceDataError::Unused`] when the check fails; the others if the database or a data
/// directory can't be read.
pub async fn check_unused_instance_data(
    root: &DataRoot,
    config: &Config,
    store: &Store,
) -> Result<(), InstanceDataError> {
    let rows = store.instance_rows().await?;
    let dirs = scan_dirs(root).map_err(|(path, source)| InstanceDataError::Io { path, source })?;
    let configured = config
        .providers
        .keys()
        .map(|name| {
            (
                name.as_str().to_owned(),
                Instance::new(Kind::Provider, false),
            )
        })
        .chain(config.channels.iter().map(|(name, channel)| {
            let launched = matches!(channel.runtime, ChannelRuntime::Plugin(_));
            (
                name.as_str().to_owned(),
                Instance::new(Kind::Channel, launched),
            )
        }))
        .chain(config.toolsets.iter().map(|(name, toolset)| {
            let launched = matches!(toolset.transport, ToolsetTransport::Stdio(_));
            (
                name.as_str().to_owned(),
                Instance::new(Kind::Toolset, launched),
            )
        }))
        .collect();
    let retired = config
        .core
        .retired_instances
        .iter()
        .map(|name| name.as_str().to_owned())
        .collect();
    let unused = find_unused(&configured, &retired, &rows, &dirs);
    if unused.is_empty() {
        Ok(())
    } else {
        Err(InstanceDataError::Unused(unused))
    }
}

/// A configured instance, as the check sees it.
#[derive(Debug, Clone, Copy)]
pub(super) struct Instance {
    kind: Kind,
    /// Whether the core launches a process for it, which is what gives it a data directory:
    /// a plugin channel or a stdio toolset. Built-in and remote instances never have one.
    launched: bool,
}

impl Instance {
    pub(super) fn new(kind: Kind, launched: bool) -> Self {
        Self { kind, launched }
    }
}

/// The decision of [`check_unused_instance_data`], on gathered facts.
pub(super) fn find_unused(
    configured: &BTreeMap<String, Instance>,
    retired: &BTreeSet<String>,
    rows: &BTreeMap<String, InstanceRows>,
    dirs: &BTreeMap<String, Vec<DataDir>>,
) -> Vec<UnusedName> {
    let has_data = |name: &str| rows.contains_key(name) || dirs.contains_key(name);
    let fresh: Vec<(&String, Instance)> = configured
        .iter()
        .filter(|(name, _)| !has_data(name))
        .map(|(name, &instance)| (name, instance))
        .collect();
    if fresh.is_empty() {
        return Vec::new();
    }
    let names: BTreeSet<&String> = rows.keys().chain(dirs.keys()).collect();
    let mut unused = Vec::new();
    for name in names {
        if configured.contains_key(name) || retired.contains(name) {
            continue;
        }
        // Rows and group memory can only belong to one kind of instance (`specific`). A plugin
        // data directory belongs to an instance the core launches (`plugin_dir`), which no
        // built-in or remote instance is, and no provider in v1.
        let mut specific = BTreeSet::new();
        let mut plugin_dir = false;
        let mut evidence = Vec::new();
        if let Some(rows) = rows.get(name) {
            for (n, what) in [
                (rows.sessions, "sessions"),
                (rows.inbox, "inbox rows"),
                (rows.outbox, "outbox rows"),
                (rows.usage, "usage rows"),
            ] {
                if n > 0 {
                    evidence.push(format!("{n} {what}"));
                }
            }
            if rows.is_channel() {
                specific.insert(Kind::Channel);
            }
            if rows.is_provider() {
                specific.insert(Kind::Provider);
            }
        }
        for dir in dirs.get(name).into_iter().flatten() {
            evidence.push(format!("the directory {}", dir.path.display()));
            if dir.group {
                specific.insert(Kind::Channel);
            } else {
                plugin_dir = true;
            }
        }
        // The certain kinds first, so the suggested command is the likeliest one.
        let mut candidates: Vec<(String, Kind)> = fresh
            .iter()
            .filter(|(_, instance)| {
                specific.contains(&instance.kind) || (plugin_dir && instance.launched)
            })
            .map(|(candidate, instance)| ((*candidate).clone(), instance.kind))
            .collect();
        candidates.sort_by_key(|(candidate, kind)| (!specific.contains(kind), candidate.clone()));
        if !candidates.is_empty() {
            unused.push(UnusedName {
                name: name.clone(),
                evidence,
                candidates,
            });
        }
    }
    unused
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Configured instances; channels and toolsets are launched plugins, providers are built in.
    fn configured(names: &[(&str, Kind)]) -> BTreeMap<String, Instance> {
        names
            .iter()
            .map(|&(n, k)| (n.to_owned(), Instance::new(k, k != Kind::Provider)))
            .collect()
    }

    /// Like [`configured`], for instances the core doesn't launch (remote, built in).
    fn not_launched(names: &[(&str, Kind)]) -> BTreeMap<String, Instance> {
        names
            .iter()
            .map(|&(n, k)| (n.to_owned(), Instance::new(k, false)))
            .collect()
    }

    fn rows(entries: &[(&str, InstanceRows)]) -> BTreeMap<String, InstanceRows> {
        entries.iter().map(|&(n, r)| (n.to_owned(), r)).collect()
    }

    fn channel_rows() -> InstanceRows {
        InstanceRows {
            sessions: 2,
            inbox: 5,
            ..Default::default()
        }
    }

    fn usage_rows() -> InstanceRows {
        InstanceRows {
            usage: 9,
            ..Default::default()
        }
    }

    fn dir(name: &str, group: bool) -> BTreeMap<String, Vec<DataDir>> {
        let path = if group {
            format!("/root/agents/a/groups/{name}")
        } else {
            format!("/root/data/plugins/{name}")
        };
        BTreeMap::from([(
            name.to_owned(),
            vec![DataDir {
                path: PathBuf::from(path),
                group,
            }],
        )])
    }

    fn run(
        configured: &BTreeMap<String, Instance>,
        retired: &[&str],
        rows: &BTreeMap<String, InstanceRows>,
        dirs: &BTreeMap<String, Vec<DataDir>>,
    ) -> Vec<UnusedName> {
        let retired = retired.iter().map(|&s| s.to_owned()).collect();
        find_unused(configured, &retired, rows, dirs)
    }

    #[test]
    fn a_renamed_channel_is_found() {
        let found = run(
            &configured(&[("libera-chat", Kind::Channel), ("web", Kind::Channel)]),
            &[],
            &rows(&[("libera", channel_rows()), ("web", channel_rows())]),
            &BTreeMap::new(),
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "libera");
        assert_eq!(found[0].evidence, ["2 sessions", "5 inbox rows"]);
        assert_eq!(
            found[0].candidates,
            [("libera-chat".to_owned(), Kind::Channel)]
        );
        let message = InstanceDataError::Unused(found).to_string();
        assert!(
            message.contains("`giles instance rename libera libera-chat`"),
            "{message}"
        );
        assert!(
            message.contains("retired_instances = [\"libera\"]"),
            "{message}"
        );
    }

    #[test]
    fn a_new_instance_with_nothing_orphaned_is_fine() {
        let found = run(
            &configured(&[("libera", Kind::Channel), ("oftc", Kind::Channel)]),
            &[],
            &rows(&[("libera", channel_rows())]),
            &BTreeMap::new(),
        );
        assert!(found.is_empty());
    }

    #[test]
    fn a_removed_instance_with_nothing_new_is_fine() {
        let found = run(
            &configured(&[("web", Kind::Channel)]),
            &[],
            &rows(&[("web", channel_rows()), ("libera", channel_rows())]),
            &dir("libera", false),
        );
        assert!(found.is_empty());
    }

    #[test]
    fn a_retired_name_is_ignored() {
        let configured = configured(&[("oftc", Kind::Channel)]);
        let rows = rows(&[("libera", channel_rows())]);
        assert_eq!(run(&configured, &[], &rows, &BTreeMap::new()).len(), 1);
        assert!(run(&configured, &["libera"], &rows, &BTreeMap::new()).is_empty());
        // Retiring some other name doesn't help.
        assert_eq!(
            run(&configured, &["other"], &rows, &BTreeMap::new()).len(),
            1
        );
    }

    #[test]
    fn kinds_must_fit() {
        // Channel rows can't be a renamed provider or toolset.
        let found = run(
            &configured(&[("local", Kind::Provider), ("docs", Kind::Toolset)]),
            &[],
            &rows(&[("libera", channel_rows())]),
            &BTreeMap::new(),
        );
        assert!(found.is_empty());
        // Usage rows can only be a renamed provider.
        let found = run(
            &configured(&[("libera", Kind::Channel), ("local", Kind::Provider)]),
            &[],
            &rows(&[("ollama", usage_rows())]),
            &BTreeMap::new(),
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].candidates, [("local".to_owned(), Kind::Provider)]);
        // A group memory directory can only be a channel's.
        let found = run(
            &configured(&[("docs", Kind::Toolset), ("irc2", Kind::Channel)]),
            &[],
            &BTreeMap::new(),
            &dir("irc", true),
        );
        assert_eq!(found[0].candidates, [("irc2".to_owned(), Kind::Channel)]);
        // A plugin data directory fits a channel or a toolset, not a built-in provider.
        let found = run(
            &configured(&[
                ("docs", Kind::Toolset),
                ("irc2", Kind::Channel),
                ("local", Kind::Provider),
            ]),
            &[],
            &BTreeMap::new(),
            &dir("old", false),
        );
        assert_eq!(
            found[0].candidates,
            [
                ("docs".to_owned(), Kind::Toolset),
                ("irc2".to_owned(), Kind::Channel)
            ]
        );
        // Kinds the evidence is certain of come first, whatever the names sort like.
        let rows = rows(&[("old", channel_rows())]);
        let found = run(
            &configured(&[("docs", Kind::Toolset), ("irc2", Kind::Channel)]),
            &[],
            &rows,
            &dir("old", false),
        );
        assert_eq!(found[0].candidates[0], ("irc2".to_owned(), Kind::Channel));
    }

    #[test]
    fn instances_the_core_does_not_launch_never_match_a_plugin_directory() {
        // A remote toolset has no data directory and no rows, so it looks fresh forever. A
        // leftover directory of a removed stdio toolset must not make it a candidate.
        let found = run(
            &not_launched(&[("github", Kind::Toolset), ("web", Kind::Channel)]),
            &[],
            &BTreeMap::new(),
            &dir("files", false),
        );
        assert!(found.is_empty(), "{found:?}");
        // Rows of a removed channel still match a channel the core doesn't launch: it can be
        // the same channel under a new name (a renamed built-in `web`).
        let found = run(
            &not_launched(&[("chat", Kind::Channel)]),
            &[],
            &rows(&[("web", channel_rows())]),
            &BTreeMap::new(),
        );
        assert_eq!(found[0].candidates, [("chat".to_owned(), Kind::Channel)]);
    }

    #[test]
    fn a_configured_instance_with_a_data_directory_is_not_fresh() {
        // `files` ran once, so it has its directory: adding `docs` (also run) next to the removed
        // `old` toolset is then not mistaken for a rename of anything.
        let mut dirs = dir("files", false);
        dirs.extend(dir("old", false));
        let found = run(
            &configured(&[("files", Kind::Toolset)]),
            &[],
            &BTreeMap::new(),
            &dirs,
        );
        assert!(found.is_empty());
    }
}

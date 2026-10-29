//! The directories an instance name keys: `data/<kind>/<name>` and `agents/*/groups/<name>`.
//! `giles <kind> rename` moves them.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::config::{Kind, is_valid_name};
use crate::paths::DataRoot;

/// A directory keyed by an instance name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DataDir {
    pub path: PathBuf,
    /// The kind of instance that keys it: a plugin's data directory is its kind's, and a group's
    /// directory under an agent is a channel's.
    pub kind: Kind,
}

/// The sub-directories of `dir` whose names are valid instance names (`[a-zA-Z0-9-]+`), sorted
/// by name; none if `dir` doesn't exist. Other entries, such as `.DS_Store`, are ignored.
pub(super) fn list_dirs(dir: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if is_valid_name(&name) && path.is_dir() {
            dirs.push((name, path));
        }
    }
    dirs.sort();
    Ok(dirs)
}

/// Every directory an instance name keys, by name: `data/<kind>/<name>` for each kind and
/// `agents/*/groups/<name>`. The error names the directory that couldn't be read.
pub(super) fn scan_dirs(
    root: &DataRoot,
) -> Result<BTreeMap<String, Vec<DataDir>>, (PathBuf, io::Error)> {
    let mut found: BTreeMap<String, Vec<DataDir>> = BTreeMap::new();
    for kind in Kind::ALL {
        let plugins = root.plugin_data_root(kind);
        for (name, path) in list_dirs(&plugins).map_err(|e| (plugins.clone(), e))? {
            let dir = DataDir { path, kind };
            found.entry(name).or_default().push(dir);
        }
    }
    let agents = root.agents_dir();
    for (_, agent) in list_dirs(&agents).map_err(|e| (agents.clone(), e))? {
        let groups = agent.join("groups");
        for (name, path) in list_dirs(&groups).map_err(|e| (groups.clone(), e))? {
            let dir = DataDir {
                path,
                kind: Kind::Channel,
            };
            found.entry(name).or_default().push(dir);
        }
    }
    Ok(found)
}

/// The directories of `dirs` that renaming a `kind` instance moves: those that kind keys, not
/// another kind's of the same name.
pub(super) fn of_kind(
    dirs: &BTreeMap<String, Vec<DataDir>>,
    kind: Kind,
) -> BTreeMap<String, Vec<DataDir>> {
    dirs.iter()
        .filter_map(|(name, found)| {
            let found: Vec<_> = found
                .iter()
                .filter(|dir| dir.kind == kind)
                .cloned()
                .collect();
            (!found.is_empty()).then(|| (name.clone(), found))
        })
        .collect()
}

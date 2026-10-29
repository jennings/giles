//! Finding the binaries Giles launches: channel plugin binaries in `<root>/plugins/`, then on
//! `$PATH` (PLAN section 7.3), and stdio toolset commands on `$PATH` (section 7.10).
//!
//! The config loader uses this to protect the directories holding those binaries from write
//! grants; the plugin host uses the same lookup to launch them.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use giles_types::Kind;

use super::names::plugin_binary;
use crate::paths::DataRoot;

/// Finds the plugin binary `giles-<kind>-<component>`: in `<root>/plugins/`, then in each
/// absolute directory of `PATH` (as `env` reports it), in order. Only executable regular files
/// count (symlinks are followed).
pub fn discover_plugin(
    root: &DataRoot,
    kind: Kind,
    component: &str,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> Option<PathBuf> {
    let binary = plugin_binary(kind, component);
    let bundled = root.plugins_dir().join(&binary);
    if is_executable(&bundled) {
        return Some(bundled);
    }
    find_program(&binary, env)
}

/// Finds `program`: as given if it is an absolute path, otherwise in each absolute directory of
/// `PATH` (as `env` reports it), in order. Relative `PATH` entries are skipped, since they depend
/// on the current directory. Only executable regular files count (symlinks are followed).
pub fn find_program(program: &str, env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let program = Path::new(program);
    if program.is_absolute() {
        return is_executable(program).then(|| program.to_path_buf());
    }
    let path = env("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn executable(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn plugins_dir_then_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path().join("root"), Path::new("/tmp")).unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        let path =
            std::env::join_paths(["relative", a.to_str().unwrap(), b.to_str().unwrap()]).unwrap();
        let env = move |name: &str| (name == "PATH").then(|| path.clone());

        assert_eq!(discover_plugin(&root, Kind::Channel, "irc", &env), None);
        executable(&b.join("giles-channel-irc"));
        assert_eq!(
            discover_plugin(&root, Kind::Channel, "irc", &env),
            Some(b.join("giles-channel-irc"))
        );
        // Earlier PATH entries win; files that aren't executable don't count.
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("giles-channel-irc"), "").unwrap();
        assert_eq!(
            discover_plugin(&root, Kind::Channel, "irc", &env),
            Some(b.join("giles-channel-irc"))
        );
        executable(&a.join("giles-channel-irc"));
        assert_eq!(
            discover_plugin(&root, Kind::Channel, "irc", &env),
            Some(a.join("giles-channel-irc"))
        );
        // `<root>/plugins/` beats PATH.
        executable(&root.plugins_dir().join("giles-channel-irc"));
        assert_eq!(
            discover_plugin(&root, Kind::Channel, "irc", &env),
            Some(root.plugins_dir().join("giles-channel-irc"))
        );
        // The kind is part of the name.
        assert_eq!(discover_plugin(&root, Kind::Provider, "irc", &env), None);
        // Directories are not programs.
        fs::create_dir_all(b.join("giles-channel-dir")).unwrap();
        assert_eq!(discover_plugin(&root, Kind::Channel, "dir", &env), None);
        // Without PATH, only the plugins directory is searched.
        assert_eq!(discover_plugin(&root, Kind::Channel, "x", &|_| None), None);
    }

    #[test]
    fn programs() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("bin/tool");
        executable(&tool);
        let path = dir.path().join("bin").into_os_string();
        let env = move |name: &str| (name == "PATH").then(|| path.clone());
        assert_eq!(find_program("tool", &env), Some(tool.clone()));
        assert_eq!(find_program(tool.to_str().unwrap(), &|_| None), Some(tool));
        assert_eq!(find_program("missing", &env), None);
        assert_eq!(find_program("/nonexistent/tool", &env), None);
    }
}

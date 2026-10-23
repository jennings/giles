//! What a stdio toolset may read and run beyond its `sandbox` grants (PLAN section 7.10): the
//! install prefix of its command and of the command's script interpreter, and a `PATH` that finds
//! them.
//!
//! Servers are mostly scripts (Node, Python) installed by a package manager: `/opt/homebrew/bin/x`
//! is a link into `/opt/homebrew/lib/node_modules/...`, whose `#!/usr/bin/env node` runs
//! `/opt/homebrew/bin/node`. Running one needs the files under the prefix, and an `execute` grant
//! for the interpreter (the sandbox checks the interpreter of a script too).

use std::fs::File;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use crate::config::{InstanceName, check_grant};
use crate::paths::DataRoot;
use crate::sandbox::DEFAULT_PATH;

/// How much of a file is read looking for a `#!` line (Linux reads 256 bytes).
const SHEBANG_MAX: u64 = 256;

/// The extra grants and the search path for running one stdio server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    /// Paths granted to read and execute: install prefixes, or narrower paths where a prefix
    /// would overlap the data root. None contains another.
    pub grants: Vec<PathBuf>,
    /// The server's `PATH`: the directory of the command as found, the directory of an
    /// interpreter found through `env`, then [`DEFAULT_PATH`].
    pub path: String,
}

/// A script's interpreter, from its `#!` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Shebang {
    /// The interpreter as written, e.g. `/usr/bin/env` or `/opt/venv/bin/python3`.
    pub interpreter: PathBuf,
    /// For `#!/usr/bin/env [options] <program>`: the program `env` looks up on `PATH`.
    pub via_env: Option<String>,
}

/// Parses the `#!` line at the start of `head`. `None` unless it names an absolute interpreter.
pub(crate) fn parse_shebang(head: &[u8]) -> Option<Shebang> {
    let rest = head.strip_prefix(b"#!")?;
    let line = rest.split(|&b| b == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?;
    let mut words = line.split_ascii_whitespace();
    let interpreter = PathBuf::from(words.next()?);
    if !interpreter.is_absolute() {
        return None;
    }
    let via_env = if interpreter.file_name().is_some_and(|name| name == "env") {
        // Skip options (`-S`) and assignments (`NODE_ENV=production`).
        words
            .find(|word| !word.starts_with('-') && !word.contains('='))
            .map(str::to_owned)
    } else {
        None
    };
    Some(Shebang {
        interpreter,
        via_env,
    })
}

/// The install prefix of the program at `path`: the directory holding `bin/` or `sbin/` for a
/// program in one (`/opt/homebrew` for `/opt/homebrew/bin/node`, the venv for
/// `<venv>/bin/python`), else the program's own directory. Never `/`: a program in `/bin` gets
/// `/bin`, and one directly in `/` gets nothing.
pub(crate) fn install_prefix(path: &Path) -> Option<&Path> {
    let dir = path.parent()?;
    let up = dir.parent()?;
    let in_bin = dir
        .file_name()
        .is_some_and(|name| name == "bin" || name == "sbin");
    if in_bin && up.parent().is_some() {
        Some(up)
    } else {
        Some(dir)
    }
}

/// What no grant may be or contain: the data root (except the instance's own data directory) and
/// the socket directory (`check_grant`), and the operator's home directory.
pub(crate) struct Protect<'a> {
    /// The data root.
    pub root: &'a DataRoot,
    /// The instance granted.
    pub instance: &'a InstanceName,
    /// The operator's `HOME`, if set.
    pub home: Option<&'a Path>,
}

impl Protect<'_> {
    /// Whether `path` may be granted: not `/`, not the home directory or a directory containing
    /// it (compared as written and with links resolved), and allowed by `check_grant`.
    fn allows(&self, path: &Path) -> bool {
        if path.parent().is_none() {
            return false;
        }
        if let Some(home) = self.home {
            let homes = [Some(home.to_path_buf()), std::fs::canonicalize(home).ok()];
            let paths = [Some(path.to_path_buf()), std::fs::canonicalize(path).ok()];
            let contains_home = homes
                .iter()
                .flatten()
                .any(|home| paths.iter().flatten().any(|path| home.starts_with(path)));
            if contains_home {
                return false;
            }
        }
        check_grant(self.root, self.instance, path).is_ok()
    }
}

/// What to grant for `binary`: its install prefix, else its directory, else the file itself,
/// whichever is the first that `protect` allows (a prefix may be the home directory, as for
/// `~/bin/server`, or contain the data root). `None` if even the file is refused (a bundled plugin
/// inside the data root, which the sandbox lets the command run anyway).
fn grant_for(protect: &Protect<'_>, binary: &Path) -> Option<PathBuf> {
    let candidates = [install_prefix(binary), binary.parent(), Some(binary)];
    let grant = candidates
        .into_iter()
        .flatten()
        .find(|path| protect.allows(path))?;
    Some(grant.to_path_buf())
}

/// Reads the `#!` line of `program`, if it is a script.
fn read_shebang(program: &Path) -> Option<Shebang> {
    let mut head = Vec::new();
    File::open(program)
        .ok()?
        .take(SHEBANG_MAX)
        .read_to_end(&mut head)
        .ok()?;
    parse_shebang(&head)
}

/// Plans the grants for running a stdio server under `protect`. `found` is its command as found
/// (absolute), `program` the same with links resolved (the file that runs), and `find` looks a
/// program name up on the operator's `PATH` (for `#!/usr/bin/env <name>`).
///
/// Grants cover `found`, `program`, the script interpreter if `program` is a script, and the
/// program `env` runs, each also with links resolved. Paths with `..` are left out (the sandbox
/// spec refuses them).
pub(crate) fn plan(
    protect: &Protect<'_>,
    found: &Path,
    program: &Path,
    find: &dyn Fn(&str) -> Option<PathBuf>,
) -> Plan {
    let mut binaries = vec![found.to_path_buf(), program.to_path_buf()];
    let mut search: Vec<PathBuf> = found.parent().map(Path::to_path_buf).into_iter().collect();
    if let Some(shebang) = read_shebang(program) {
        binaries.push(shebang.interpreter);
        let run = shebang.via_env.and_then(|name| {
            if Path::new(&name).is_absolute() {
                Some(PathBuf::from(name))
            } else {
                find(&name)
            }
        });
        if let Some(run) = run {
            search.extend(run.parent().map(Path::to_path_buf));
            binaries.push(run);
        }
    }
    let resolved: Vec<PathBuf> = binaries
        .iter()
        .filter_map(|binary| std::fs::canonicalize(binary).ok())
        .collect();
    binaries.extend(resolved);

    let mut grants: Vec<PathBuf> = binaries
        .iter()
        .filter(|binary| usable(binary))
        .filter_map(|binary| grant_for(protect, binary))
        .collect();
    grants.sort();
    grants.dedup();
    let covered: Vec<PathBuf> = grants
        .iter()
        .filter(|grant| {
            grants
                .iter()
                .any(|other| other != *grant && grant.starts_with(other))
        })
        .cloned()
        .collect();
    grants.retain(|grant| !covered.contains(grant));

    let mut dirs: Vec<String> = Vec::new();
    let defaults = DEFAULT_PATH.split(':').map(str::to_owned);
    let found_dirs = search
        .iter()
        .filter(|dir| usable(dir))
        .filter_map(|dir| dir.to_str())
        .filter(|dir| !dir.contains(':'))
        .map(str::to_owned);
    for dir in found_dirs.chain(defaults) {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    Plan {
        grants,
        path: dirs.join(":"),
    }
}

/// Whether `path` can go in a sandbox spec: absolute and without `..`.
fn usable(path: &Path) -> bool {
    path.is_absolute() && !path.components().any(|c| c == Component::ParentDir)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    use super::*;

    #[test]
    fn shebangs() {
        let parse = |text: &str| parse_shebang(text.as_bytes());
        assert_eq!(
            parse("#!/usr/bin/env node\nconsole.log(1)"),
            Some(Shebang {
                interpreter: "/usr/bin/env".into(),
                via_env: Some("node".into()),
            })
        );
        assert_eq!(
            parse("#!/usr/bin/env -S NODE_ENV=production node --flag\r\n"),
            Some(Shebang {
                interpreter: "/usr/bin/env".into(),
                via_env: Some("node".into()),
            })
        );
        assert_eq!(
            parse("#! /opt/venv/bin/python3 -u\n"),
            Some(Shebang {
                interpreter: "/opt/venv/bin/python3".into(),
                via_env: None,
            })
        );
        for text in ["#!python\n", "\u{7f}ELF", "", "#!\n", "# comment\n"] {
            assert_eq!(parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn install_prefixes() {
        let prefix = |path: &str| install_prefix(Path::new(path)).map(Path::to_path_buf);
        assert_eq!(
            prefix("/opt/homebrew/bin/node"),
            Some("/opt/homebrew".into())
        );
        assert_eq!(prefix("/usr/local/sbin/x"), Some("/usr/local".into()));
        assert_eq!(
            prefix("/home/me/venv/bin/python"),
            Some("/home/me/venv".into())
        );
        assert_eq!(
            prefix("/opt/server/dist/index.js"),
            Some("/opt/server/dist".into())
        );
        assert_eq!(prefix("/bin/sh"), Some("/bin".into()));
        assert_eq!(prefix("/x"), None);
    }

    /// A data root, a home directory, and install prefixes, in one temporary directory.
    struct Setup {
        dir: tempfile::TempDir,
        root: DataRoot,
        home: PathBuf,
    }

    impl Setup {
        /// The data root in the home directory, as by default.
        fn new() -> Self {
            Self::with_root("home/.giles")
        }

        /// The data root at `root`, relative to the temporary directory; home is `home`.
        fn with_root(root: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let base = fs::canonicalize(dir.path()).unwrap();
            let home = base.join("home");
            fs::create_dir_all(&home).unwrap();
            let root = DataRoot::new(base.join(root), Path::new("/tmp")).unwrap();
            Self { dir, root, home }
        }

        fn base(&self) -> PathBuf {
            fs::canonicalize(self.dir.path()).unwrap()
        }

        fn file(&self, path: &str, contents: &str) -> PathBuf {
            let path = self.base().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        fn plan(&self, found: &Path, find: &dyn Fn(&str) -> Option<PathBuf>) -> Plan {
            self.plan_with_home(found, &self.home, find)
        }

        fn plan_with_home(
            &self,
            found: &Path,
            home: &Path,
            find: &dyn Fn(&str) -> Option<PathBuf>,
        ) -> Plan {
            let program = fs::canonicalize(found).unwrap();
            let instance = InstanceName::new("files").unwrap();
            let protect = Protect {
                root: &self.root,
                instance: &instance,
                home: Some(home),
            };
            plan(&protect, found, &program, find)
        }
    }

    #[test]
    fn a_linked_script_run_by_env_gets_both_prefixes() {
        let s = Setup::new();
        let base = s.base();
        let script = s.file(
            "brew/lib/node_modules/server/index.js",
            "#!/usr/bin/env node\n",
        );
        fs::create_dir_all(base.join("brew/bin")).unwrap();
        let found = base.join("brew/bin/mcp-server");
        symlink(&script, &found).unwrap();
        let node = s.file("node22/bin/node", "");
        let plan = s.plan(&found, &|name| (name == "node").then(|| node.clone()));
        assert_eq!(
            plan.grants,
            [base.join("brew"), base.join("node22"), "/usr".into()]
        );
        assert_eq!(
            plan.path,
            format!(
                "{}:{}:/usr/bin:/bin",
                base.join("brew/bin").display(),
                base.join("node22/bin").display()
            )
        );
    }

    #[test]
    fn an_interpreter_link_grants_where_it_leads() {
        let s = Setup::new();
        let base = s.base();
        let python = s.file("uv/python/cpython-3.12/bin/python3.12", "");
        fs::create_dir_all(base.join("uv/tools/fetch/bin")).unwrap();
        let venv_python = base.join("uv/tools/fetch/bin/python");
        symlink(&python, &venv_python).unwrap();
        let found = s.file(
            "uv/tools/fetch/bin/mcp-server-fetch",
            &format!("#!{}\n", venv_python.display()),
        );
        let plan = s.plan(&found, &|_| None);
        assert_eq!(
            plan.grants,
            [
                base.join("uv/python/cpython-3.12"),
                base.join("uv/tools/fetch")
            ]
        );
        assert_eq!(
            plan.path,
            format!(
                "{}:/usr/bin:/bin",
                base.join("uv/tools/fetch/bin").display()
            )
        );
    }

    #[test]
    fn a_prefix_containing_the_data_root_narrows_to_the_directory() {
        let s = Setup::new();
        let base = s.base();
        // `home/bin/server`: its prefix `home` contains the data root `home/.giles`.
        let found = s.file("home/bin/server", "");
        let plan = s.plan(&found, &|_| None);
        assert_eq!(plan.grants, [base.join("home/bin")]);
    }

    #[test]
    fn a_binary_inside_the_data_root_gets_no_grant() {
        let s = Setup::new();
        let found = s.file("home/.giles/plugins/giles-toolset-x", "");
        let plan = s.plan(&found, &|_| None);
        assert!(plan.grants.is_empty(), "{plan:?}");
    }

    #[test]
    fn the_home_directory_is_never_a_prefix() {
        // The data root outside home, so only the home rule stops `home/bin/server` from
        // granting all of `home`.
        let s = Setup::with_root("srv/giles");
        let base = s.base();
        let found = s.file("home/bin/server", "");
        let plan = s.plan(&found, &|_| None);
        assert_eq!(plan.grants, [base.join("home/bin")]);

        // Home given through a link counts by where it leads, and a prefix containing home is
        // refused too: `/<tmp>/bin/x` would grant the directory holding home.
        let link = base.join("home-link");
        symlink(&s.home, &link).unwrap();
        let plan = s.plan_with_home(&found, &link, &|_| None);
        assert_eq!(plan.grants, [base.join("home/bin")]);
        let outer = s.file("bin/outer", "");
        let plan = s.plan(&outer, &|_| None);
        assert_eq!(plan.grants, [base.join("bin")]);
    }
}

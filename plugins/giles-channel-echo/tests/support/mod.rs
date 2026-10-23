//! A data root with the echo plugin installed, for tests that launch it through the core's plugin
//! host.

#![allow(dead_code)]

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use giles_core::config::{Builtins, ChannelInstance, Config, LoadOptions};
use giles_core::paths::DataRoot;
use giles_core::plugin_host::Launcher;

/// The echo plugin binary Cargo built for these tests.
pub const ECHO: &str = env!("CARGO_BIN_EXE_giles-channel-echo");

/// Upper bound for anything that must not hang.
pub const WAIT: Duration = Duration::from_secs(10);

/// A temporary data root whose `giles.toml` has the given channel tables.
pub struct Home {
    _dir: tempfile::TempDir,
    /// The data root.
    pub root: DataRoot,
    env: Vec<(String, String)>,
}

impl Home {
    /// A data root with `channels` in `giles.toml` and `env` as the environment config references
    /// (`{ env = "NAME" }`) resolve against.
    pub fn new(channels: &str, env: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        // Resolved, so the data directory isn't below a symbolic link the sandbox would see
        // differently (macOS `/var` is a link to `/private/var`).
        let base = dir.path().canonicalize().unwrap();
        let root = DataRoot::new(base.join("home"), Path::new("/tmp")).unwrap();
        root.create_layout().unwrap();
        fs::write(
            root.config_file(),
            format!("[owner]\nidentities = [\"web:owner\"]\n\n{channels}"),
        )
        .unwrap();
        Self {
            _dir: dir,
            root,
            env: env
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    /// Links the echo binary into `<root>/plugins/` as `giles-channel-<component>`.
    pub fn install_echo_as(&self, component: &str) -> PathBuf {
        let link = self
            .root
            .plugins_dir()
            .join(format!("giles-channel-{component}"));
        std::os::unix::fs::symlink(ECHO, &link).unwrap();
        link
    }

    /// The loaded configuration.
    pub fn config(&self) -> Config {
        let env = |name: &str| {
            self.env
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| OsString::from(v))
        };
        Config::load_with(
            &self.root,
            &LoadOptions {
                builtins: Builtins::V1,
                env: &env,
                giles_exe: None,
            },
        )
        .unwrap()
    }

    /// The configured channel `name`.
    pub fn channel(&self, name: &str) -> ChannelInstance {
        self.config().channels[name].clone()
    }

    /// A launcher for this root that finds plugins only in `<root>/plugins/`, with `trampoline`.
    pub fn launcher(&self, trampoline: PathBuf) -> Launcher {
        Launcher::new(self.root.clone(), trampoline, Ok(())).env(|_| None)
    }
}

//! Validating one toolset against the rest of the configuration, whatever its source: a
//! `[toolset.<name>]` table of `giles.toml` or a [`ToolsetSpec`] parsed from JSON (PLAN section
//! 7.10).
//!
//! [`validate_toolset`] runs the per-toolset rules of instance validation (`Ctx::toolset`) and
//! the checks that involve other instances: `name_clash` and `check_binary_dirs`. Loading
//! `giles.toml` calls the same three, with every instance at once, so both sources follow the
//! same rules.

use std::cell::RefCell;
use std::fmt;
use std::path::Path;

use super::discovery::{discover_plugin, find_program};
use super::error::{ConfigError, KeyPath};
use super::instances::{
    ChannelInstance, ChannelRuntime, Ctx, Program, Sandbox, ToolsetInstance, ToolsetSpec,
    ToolsetTransport,
};
use super::names::{InstanceName, Kind};
use super::sandbox::{self, BinaryDirs};
use super::secrets::{Resolver, Secrets};
use super::{Config, LoadOptions};
use crate::paths::DataRoot;

/// What [`validate_toolset`] needs besides the spec.
#[derive(Clone, Copy)]
pub struct ToolsetContext<'a> {
    /// The data root: toolset data directories, `<root>/plugins/`, and the socket directory
    /// (`DataRoot::socket_dir`) that no grant may overlap.
    pub root: &'a DataRoot,
    /// The secrets that `{ secret = "name" }` references resolve against.
    pub secrets: &'a Secrets,
    /// The built-in components, the environment (for `{ env = "NAME" }` references and the `PATH`
    /// that commands and plugin binaries are found on), and the running `giles` binary.
    pub options: &'a LoadOptions<'a>,
    /// The running configuration: the other instances the toolset must not clash with.
    pub config: &'a Config,
    /// The toolset of `config` that the validated one replaces, if any: `None` to add a toolset,
    /// its own name to edit it, its old name to rename it. Only this instance is left out of the
    /// name check and of the other instances' grants and directories.
    pub replacing: Option<&'a InstanceName>,
}

/// A toolset that can't be used, and why.
///
/// Displayed as `<key>: <reason>`, or just the reason when `key` is the root.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ToolsetError {
    /// The key of the spec with the problem, as its table writes it (`env.TOKEN`,
    /// `sandbox.write[0]`), so a form can show the reason next to the field. The root if the
    /// problem is with the toolset as a whole: its name, or what it conflicts with.
    pub key: KeyPath,
    /// What is wrong. Never contains a secret value.
    pub reason: String,
}

impl ToolsetError {
    /// `error`, which `Ctx` or [`check_binary_dirs`] raised for the toolset whose table is `own`
    /// (`toolset.<name>`). A problem at another instance's key (its grant overlaps this toolset's
    /// binary directory) names that key in the reason.
    fn from_config(error: ConfigError, own: &KeyPath) -> Self {
        match error.key.strip_prefix(own) {
            Some(key) => Self {
                key,
                reason: error.reason,
            },
            None => Self {
                key: KeyPath::root(),
                reason: format!("{}: {}", error.key, error.reason),
            },
        }
    }
}

impl fmt::Display for ToolsetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.key.is_root() {
            write!(f, "{}: ", self.key)?;
        }
        f.write_str(&self.reason)
    }
}

impl std::error::Error for ToolsetError {}

/// Validates `spec` as the toolset `name`, exactly as loading `giles.toml` validates a
/// `[toolset.<name>]` table:
///
/// - the keys and values: which keys fit a stdio or a remote toolset, `command`, `url`, `env`
///   names, `auth`, durations, skill directories, and the sandbox grants against the data root and
///   the socket directory;
/// - credentials: each reference is resolved now (`secrets`, or the environment);
/// - the name: it must not be used by any other instance, of any kind, ignoring case. The toolset
///   `ctx.replacing`, if any, is the one being edited or renamed, so it doesn't count: adding a
///   toolset passes `None`, editing passes its own name, and renaming passes its old name (so the
///   new name only has to be free of every other instance, even if it differs from the old one
///   only in case);
/// - binary directories: the toolset's `write` and `unix` grants must not overlap the directory of
///   a binary Giles runs (the `giles` binary, plugins, every stdio server including this one),
///   and no other instance's grant may overlap the directory of this toolset's server. Another
///   instance's own problems are not this toolset's, and the toolset being replaced is not
///   another instance: it is left out of these checks.
///
/// The instance's `config_hash` is [`ToolsetSpec::config_hash`].
///
/// # Errors
///
/// The first problem found, with the key of `spec` that has it.
pub fn validate_toolset(
    name: &str,
    spec: &ToolsetSpec,
    ctx: &ToolsetContext<'_>,
) -> Result<ToolsetInstance, ToolsetError> {
    // A spec isn't in a file; errors lose the file name anyway.
    let file = Path::new("");
    let own = KeyPath::root().key(Kind::Toolset.as_str()).key(name);
    let relative = |error: ConfigError| ToolsetError::from_config(error, &own);

    let resolver = Resolver {
        secrets: ctx.secrets,
        env: ctx.options.env,
    };
    let validation = Ctx {
        file,
        root: ctx.root,
        builtins: &ctx.options.builtins,
        resolver: &resolver,
        unknown: RefCell::default(),
    };
    let instance = validation
        .toolset(name, spec.clone(), spec.config_hash())
        .map_err(relative)?;

    let config = ctx.config;
    if let Some((kind, other)) = instance_named(config, name, ctx.replacing) {
        return Err(ToolsetError {
            key: KeyPath::root(),
            reason: name_clash(kind, other.as_str(), name),
        });
    }

    let toolsets: Vec<&ToolsetInstance> = config
        .toolsets
        .values()
        .filter(|other| Some(&other.name) != ctx.replacing)
        .chain([&instance])
        .collect();
    check_binary_dirs(
        file,
        ctx.root,
        ctx.options,
        config.channels.values(),
        &toolsets,
        Some(&instance.name),
    )
    .map_err(relative)?;
    Ok(instance)
}

/// The instance of `config`, of any kind, whose name is `name` ignoring ASCII case, other than the
/// toolset `replacing`. The one rule for the name of a toolset, whether it is validated or the
/// running configuration is changed: instance names key data directories and tool prefixes, so
/// they are unique across kinds, ignoring case.
pub(super) fn instance_named<'a>(
    config: &'a Config,
    name: &str,
    replacing: Option<&InstanceName>,
) -> Option<(Kind, &'a InstanceName)> {
    let providers = config.providers.keys().map(|other| (Kind::Provider, other));
    let channels = config.channels.keys().map(|other| (Kind::Channel, other));
    let toolsets = config
        .toolsets
        .keys()
        .filter(|other| Some(*other) != replacing)
        .map(|other| (Kind::Toolset, other));
    providers
        .chain(channels)
        .chain(toolsets)
        .find(|(_, other)| other.as_str().eq_ignore_ascii_case(name))
}

/// Why `name` can't be an instance name next to the existing `[other.other_name]`.
pub(super) fn name_clash(other: Kind, other_name: &str, name: &str) -> String {
    format!(
        "instance names must be unique across kinds, ignoring case, and \
         `[{other}.{other_name}]` also uses {name:?}"
    )
}

/// Checks the sandbox grants of `channels` and `toolsets` (all validated) against the directories
/// holding the binaries and skills Giles reads or runs. Errors are in `file`.
///
/// Directories holding binaries Giles runs may be read but not written by any plugin: the `giles`
/// binary, each launched channel plugin, and each stdio server, as found now. A toolset's skill
/// directories are read by the core for the model (PLAN section 6.6), so no plugin may write
/// there, so none can race those reads; they must not overlap the plugins' data directories.
///
/// With `focus`, the toolset just validated (one of `toolsets`), only what involves it is checked:
/// its own grants and skills against everything, and every other instance's grants against its
/// binary and skill directories. Without, everything is checked, as when loading `giles.toml`.
pub(super) fn check_binary_dirs<'a>(
    file: &Path,
    root: &DataRoot,
    options: &LoadOptions<'_>,
    channels: impl IntoIterator<Item = &'a ChannelInstance>,
    toolsets: &[&'a ToolsetInstance],
    focus: Option<&InstanceName>,
) -> Result<(), ConfigError> {
    let env = options.env;
    let is_focus = |toolset: &InstanceName| focus == Some(toolset);

    let mut binary_dirs = BinaryDirs::default();
    // What the focused toolset alone contributes to `binary_dirs`.
    let mut focus_dirs = BinaryDirs::default();
    if let Some(exe) = &options.giles_exe {
        binary_dirs.add(exe, "the giles binary".to_owned());
    }
    let mut sandboxes: Vec<(Kind, &InstanceName, &Sandbox)> = Vec::new();
    for channel in channels {
        if let ChannelRuntime::Plugin(launch) = &channel.runtime {
            if let Some(binary) = discover_plugin(root, Kind::Channel, &channel.component, env) {
                let what = format!("channel {}'s plugin, {}", channel.name, binary.display());
                binary_dirs.add(&binary, what);
            }
            sandboxes.push((Kind::Channel, &channel.name, &launch.sandbox));
        }
    }
    for toolset in toolsets {
        if let ToolsetTransport::Stdio(stdio) = &toolset.transport {
            let binary = match &stdio.program {
                Program::Command(command) => find_program(command, env),
                Program::Plugin { component, .. } => {
                    discover_plugin(root, Kind::Toolset, component, env)
                }
            };
            if let Some(binary) = binary {
                let what = format!("toolset {}'s server, {}", toolset.name, binary.display());
                if is_focus(&toolset.name) {
                    focus_dirs.add(&binary, what.clone());
                }
                binary_dirs.add(&binary, what);
            }
            sandboxes.push((Kind::Toolset, &toolset.name, &stdio.sandbox));
        }
    }
    // Toolset skill directories are read by the core for the model (PLAN section 6.6): no
    // plugin may write there, so none can race those reads.
    for toolset in toolsets {
        for (i, dir) in toolset.skills.iter().enumerate() {
            if focus.is_none_or(|focus| focus == &toolset.name)
                && sandbox::overlaps(dir, &root.plugin_data_root())
            {
                return Err(ConfigError::at(
                    file,
                    KeyPath::root()
                        .key("toolset")
                        .key(toolset.name.as_str())
                        .key("skills")
                        .index(i),
                    format!(
                        "{} overlaps the plugins' data directories, {}",
                        dir.display(),
                        root.plugin_data_root().display()
                    ),
                ));
            }
            let what = format!("toolset {}'s skills", toolset.name);
            if is_focus(&toolset.name) {
                focus_dirs.add_dir(dir, what.clone());
            }
            binary_dirs.add_dir(dir, what);
        }
    }
    for (kind, name, sandbox) in sandboxes {
        let path = KeyPath::root()
            .key(kind.as_str())
            .key(name.as_str())
            .key("sandbox");
        let dirs = if focus.is_none() || (kind == Kind::Toolset && is_focus(name)) {
            &binary_dirs
        } else {
            &focus_dirs
        };
        dirs.check(file, &path, &sandbox.grants)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::*;
    use crate::config::{Auth, ConfigHash};

    /// The smallest valid `giles.toml`.
    const MINIMAL: &str = "[owner]\nidentities = [\"web:owner\"]\n";

    /// The environment tests validate with: only `GITHUB_TOKEN` is set.
    fn test_env(name: &str) -> Option<OsString> {
        (name == "GITHUB_TOKEN").then(|| "env-github-token".into())
    }

    /// A data root with a loaded `giles.toml` and `secrets.toml`.
    struct Fixture {
        _dir: TempDir,
        root: DataRoot,
        secrets: Secrets,
        options: LoadOptions<'static>,
        config: Config,
    }

    impl Fixture {
        fn new(giles_toml: &str, secrets_toml: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = DataRoot::new(dir.path(), Path::new("/tmp")).unwrap();
            std::fs::write(root.config_file(), format!("{MINIMAL}{giles_toml}")).unwrap();
            std::fs::write(root.secrets_file(), secrets_toml).unwrap();
            std::fs::set_permissions(root.secrets_file(), std::fs::Permissions::from_mode(0o600))
                .unwrap();
            let options = LoadOptions {
                env: &test_env,
                giles_exe: None,
                ..LoadOptions::default()
            };
            let config = Config::load_with(&root, &options).unwrap_or_else(|e| panic!("{e}"));
            let secrets = Secrets::load(&root.secrets_file()).unwrap_or_else(|e| panic!("{e}"));
            Self {
                _dir: dir,
                root,
                secrets,
                options,
                config,
            }
        }

        /// Validates `spec` as `name`, as an edit or rename of the toolset `replacing` if given.
        fn validate_replacing(
            &self,
            name: &str,
            replacing: Option<&str>,
            spec: Value,
        ) -> Result<ToolsetInstance, ToolsetError> {
            let spec = ToolsetSpec::from_json(&spec.to_string()).unwrap_or_else(|e| panic!("{e}"));
            let replacing = replacing.map(|name| InstanceName::new(name).unwrap());
            validate_toolset(
                name,
                &spec,
                &ToolsetContext {
                    root: &self.root,
                    secrets: &self.secrets,
                    options: &self.options,
                    config: &self.config,
                    replacing: replacing.as_ref(),
                },
            )
        }

        /// Validates `spec` as a toolset to add.
        fn validate(&self, name: &str, spec: Value) -> Result<ToolsetInstance, ToolsetError> {
            self.validate_replacing(name, None, spec)
        }

        #[track_caller]
        fn assert_valid(&self, name: &str, spec: Value) -> ToolsetInstance {
            self.validate(name, spec)
                .unwrap_or_else(|e| panic!("{name}: {e}"))
        }

        /// Asserts that `spec` is invalid at `key` (empty for the toolset as a whole), with a
        /// reason containing `reason`.
        #[track_caller]
        fn assert_invalid(&self, name: &str, spec: Value, key: &str, reason: &str) {
            let err = self
                .validate(name, spec)
                .expect_err("the spec should be invalid");
            assert_eq!(err.key.to_string(), key, "{err}");
            assert!(
                err.reason.contains(reason),
                "{err}\nexpected reason: {reason}"
            );
        }
    }

    fn executable(dir: &Path, relative: &str) -> PathBuf {
        let path = dir.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn a_valid_spec_becomes_an_instance() {
        let fixture = Fixture::new("", "");
        let remote = fixture.assert_valid(
            "github",
            json!({ "url": "https://api.githubcopilot.com/mcp/", "parallel": true,
                    "deadline": "90s", "skills": ["/srv/skills"] }),
        );
        assert_eq!(remote.name.as_str(), "github");
        assert!(remote.parallel);
        assert_eq!(remote.deadline, std::time::Duration::from_secs(90));
        assert_eq!(remote.skills, [PathBuf::from("/srv/skills")]);
        assert!(matches!(remote.transport, ToolsetTransport::Http(_)));

        let shell = fixture.assert_valid("shell", json!({}));
        let ToolsetTransport::Stdio(stdio) = &shell.transport else {
            panic!("{shell:?}");
        };
        assert_eq!(
            stdio.program,
            Program::Plugin {
                component: "shell".to_owned(),
                binary: "giles-toolset-shell".to_owned(),
            }
        );
    }

    #[test]
    fn specs_read_from_toml_and_json_alike() {
        let from_toml: ToolsetSpec = toml::from_str(
            "command = \"/bin/files\"\nargs = [\"/docs\"]\nidle_stop = \"5m\"\n\
             env = { TOKEN = { secret = \"files\" }, LOG_LEVEL = \"debug\" }\n\
             sandbox = { network = [\"tcp:443\"], read = [\"/docs\"] }\n",
        )
        .unwrap();
        let from_json = ToolsetSpec::from_json(
            r#"{ "command": "/bin/files", "args": ["/docs"], "idle_stop": "5m",
                 "env": { "TOKEN": { "secret": "files" }, "LOG_LEVEL": "debug" },
                 "sandbox": { "network": ["tcp:443"], "read": ["/docs"] } }"#,
        )
        .unwrap();
        assert_eq!(from_toml, from_json);

        // What the spec serializes to reads back as the same spec.
        let again = ToolsetSpec::from_json(&serde_json::to_string(&from_json).unwrap()).unwrap();
        assert_eq!(again, from_json);

        let err = ToolsetSpec::from_json(r#"{ "args": ["a", 1] }"#).unwrap_err();
        assert_eq!(err.key.to_string(), "args[1]", "{err}");
        let err = ToolsetSpec::from_json("{ not json").unwrap_err();
        assert!(err.key.is_root(), "{err}");
    }

    #[test]
    fn a_spec_round_trips_through_toml_text_with_references_as_written() {
        let spec = ToolsetSpec::from_toml(
            "url = \"https://files.example.com/mcp\"\ninsecure = true\ndeadline = \"90s\"\n\
             parallel = true\nskills = [\"/skills/files\"]\n\
             [auth]\nbearer = { secret = \"files\" }\n",
        )
        .unwrap();
        let text = spec.to_toml().unwrap();
        // The token is the reference, whatever it resolves to.
        assert!(text.contains("secret = \"files\""), "{text}");
        assert_eq!(ToolsetSpec::from_toml(&text).unwrap(), spec, "{text}");
        // And the same spec as the JSON the database stores.
        let json = serde_json::to_string(&spec).unwrap();
        assert_eq!(ToolsetSpec::from_json(&json).unwrap(), spec);
        assert_eq!(
            ToolsetSpec::default().to_toml().unwrap(),
            "",
            "an empty spec is empty text"
        );
    }

    #[test]
    fn toml_text_that_is_not_a_spec_is_refused_at_the_key() {
        for (text, key, reason) in [
            ("url = \n", "", "invalid TOML at line 1, column "),
            (
                "url = \"https://x.example.com/mcp\"\nparallel = \"yes\"\n",
                "parallel",
                "invalid type",
            ),
            ("skills = [\"/a\", 2]\n", "skills[1]", "invalid type"),
            ("ulr = \"x\"\n", "ulr", "unknown key"),
            (
                "[sandbox]\nredd = []\n",
                "sandbox.redd",
                "unknown field `redd`",
            ),
            ("deadline = \"soon\"\n", "deadline", ""),
        ] {
            let err = ToolsetSpec::from_toml(text).unwrap_err();
            assert_eq!(err.key.to_string(), key, "{text:?}: {err}");
            assert!(err.reason.contains(reason), "{text:?}: {err}");
        }
    }

    #[test]
    fn credential_references_resolve() {
        let fixture = Fixture::new("", "files = \"s3cret\"\n");
        let github = fixture.assert_valid(
            "github",
            json!({ "url": "https://x", "auth": { "bearer": { "env": "GITHUB_TOKEN" } } }),
        );
        let ToolsetTransport::Http(remote) = &github.transport else {
            panic!("{github:?}");
        };
        let Some(Auth::Bearer(token)) = &remote.auth else {
            panic!("{remote:?}");
        };
        assert_eq!(token.expose(), "env-github-token");

        let files = fixture.assert_valid(
            "files",
            json!({ "command": "/bin/files",
                    "env": { "TOKEN": { "secret": "files" }, "LOG_LEVEL": "debug" } }),
        );
        let ToolsetTransport::Stdio(stdio) = &files.transport else {
            panic!("{files:?}");
        };
        assert_eq!(stdio.env["TOKEN"].expose(), "s3cret");
        assert_eq!(stdio.env["LOG_LEVEL"].expose(), "debug");
    }

    #[test]
    fn credential_errors_name_the_key_and_never_show_values() {
        let fixture = Fixture::new("", "files = \"s3cret\"\n");
        let cases = [
            (
                json!({ "url": "https://x", "auth": { "bearer": { "env": "UNSET" } } }),
                "auth.bearer",
                "environment variable UNSET is not set",
            ),
            (
                json!({ "url": "https://x", "auth": { "bearer": { "secret": "nope" } } }),
                "auth.bearer",
                "no secret \"nope\" in",
            ),
            (
                json!({ "command": "/x", "env": { "A": { "secret": "nope" } } }),
                "env.A",
                "no secret",
            ),
            (
                json!({ "command": "/x", "env": { "A": { "env": 1 } } }),
                "env.A",
                "non-empty string",
            ),
            (
                json!({ "command": "/x", "env": { "A": 98765432 } }),
                "env.A",
                "not integer",
            ),
            (
                json!({ "command": "/x", "env": "98765432" }),
                "env",
                "must be a table",
            ),
            (
                json!({ "url": "https://x", "auth": { "bearer": [] } }),
                "auth.bearer",
                "not array",
            ),
            (
                json!({ "url": "https://x", "auth": "98765432" }),
                "auth",
                "must be { bearer = ... }",
            ),
            (
                json!({ "command": "/x", "auth": { "bearer": "t" } }),
                "auth",
                "without `url`",
            ),
            (
                json!({ "url": "https://x", "env": { "A": "b" } }),
                "env",
                "remote toolset",
            ),
        ];
        for (spec, key, reason) in cases {
            let shown = spec.to_string();
            fixture.assert_invalid("t", spec, key, reason);
            let err = fixture
                .validate("t", serde_json::from_str(&shown).unwrap())
                .unwrap_err();
            assert!(!format!("{err} {err:?}").contains("98765432"), "{err}");
        }

        // A plain value is kept in the spec, but `Debug` leaves `env` and `auth` out.
        let spec =
            ToolsetSpec::from_json(r#"{ "command": "/x", "env": { "A": "hunter2" } }"#).unwrap();
        assert!(!format!("{spec:?}").contains("hunter2"));
    }

    #[test]
    fn env_names_are_checked_and_derived_ones_are_refused() {
        let fixture = Fixture::new("", "");
        for name in crate::sandbox::DERIVED_ENV {
            fixture.assert_invalid(
                "t",
                json!({ "command": "/x", "env": { name: "v" } }),
                &format!("env.{name}"),
                "is set by Giles",
            );
        }
        fixture.assert_invalid(
            "t",
            json!({ "command": "/x", "env": { "A=B": "c" } }),
            "env.\"A=B\"",
            "invalid environment variable name",
        );
        fixture.assert_valid("t", json!({ "command": "/x", "env": { "A_B": "c" } }));
    }

    #[test]
    fn rules_name_the_key_they_apply_to() {
        let fixture = Fixture::new("", "");
        let plugin_data = fixture.root.plugin_data_root();
        let cases = [
            (
                json!({ "url": "https://x", "args": [] }),
                "args",
                "remote toolset",
            ),
            (
                json!({ "command": "/x", "component": "y" }),
                "component",
                "together with `command`",
            ),
            (
                json!({ "url": "https://x", "deadline": "0s" }),
                "deadline",
                "must be longer than 0s",
            ),
            (
                json!({ "command": "/x", "idle_stop": "0s" }),
                "idle_stop",
                "must be longer than 0s",
            ),
            (
                json!({ "command": "relative/x" }),
                "command",
                "absolute path or a program name",
            ),
            (
                json!({ "url": "http://example.com/mcp" }),
                "url",
                "insecure = true",
            ),
            (
                json!({ "url": "https://x", "skills": ["relative"] }),
                "skills[0]",
                "must be an absolute path",
            ),
            (
                json!({ "url": "https://x", "skills": [plugin_data] }),
                "skills[0]",
                "overlaps the plugins' data directories",
            ),
            (
                json!({ "command": "/x", "sandbox": { "read": ["relative"] } }),
                "sandbox.read[0]",
                "not an absolute path",
            ),
            (
                json!({ "command": "/x", "sandbox": { "write": [fixture.root.root()] } }),
                "sandbox.write[0]",
                "overlaps the data root",
            ),
            (
                json!({ "command": "/x", "sandbox": { "unix": [fixture.root.socket_dir()] } }),
                "sandbox.unix[0]",
                "overlaps",
            ),
            (
                json!({ "command": "/x", "sandbox": { "min_abi": 0 } }),
                "sandbox.min_abi",
                "at least 1",
            ),
            (
                json!({ "command": "/x", "unsandboxed": true, "sandbox": {} }),
                "sandbox",
                "no effect",
            ),
        ];
        for (spec, key, reason) in cases {
            fixture.assert_invalid("t", spec, key, reason);
        }
        // The name is checked too, as the toolset as a whole.
        fixture.assert_invalid(
            "bad name",
            json!({ "command": "/x" }),
            "",
            "invalid instance",
        );
    }

    #[test]
    fn a_name_taken_by_another_kind_is_refused() {
        let fixture = Fixture::new(
            "[provider.anthropic]\n[channel.web]\n[toolset.github]\nurl = \"https://x\"\n",
            "",
        );
        let spec = json!({ "url": "https://y" });
        fixture.assert_invalid("web", spec.clone(), "", "`[channel.web]` also uses \"web\"");
        fixture.assert_invalid(
            "Anthropic",
            spec.clone(),
            "",
            "`[provider.anthropic]` also uses \"Anthropic\"",
        );
        // Another toolset differing only in case is a clash too, as in `giles.toml`.
        fixture.assert_invalid("GitHub", spec.clone(), "", "`[toolset.github]` also uses");
        // Replacing the toolset of exactly this name (an edit) is not a clash.
        let replaced = fixture
            .validate_replacing("github", Some("github"), spec.clone())
            .unwrap();
        assert_eq!(replaced.name.as_str(), "github");
        fixture.assert_valid("other", spec);
    }

    #[test]
    fn adding_over_an_existing_name_is_refused() {
        let fixture = Fixture::new("[toolset.github]\nurl = \"https://x\"\n", "");
        let spec = json!({ "url": "https://y" });
        // Without a toolset being replaced, even the toolset of exactly this name is a clash.
        fixture.assert_invalid("github", spec.clone(), "", "`[toolset.github]` also uses");
        // Replacing some other toolset doesn't exempt it.
        let err = fixture
            .validate_replacing("github", Some("other"), spec)
            .unwrap_err();
        assert!(err.reason.contains("`[toolset.github]` also uses"), "{err}");
    }

    #[test]
    fn renaming_needs_a_free_name_but_may_change_only_the_case() {
        let fixture = Fixture::new(
            "[toolset.github]\nurl = \"https://x\"\n[toolset.files]\nurl = \"https://z\"\n\
             [channel.web]\n",
            "",
        );
        let spec = json!({ "url": "https://y" });
        // The old instance is exempt, so its name in another case is free.
        let renamed = fixture
            .validate_replacing("GitHub", Some("github"), spec.clone())
            .unwrap();
        assert_eq!(renamed.name.as_str(), "GitHub");
        let renamed = fixture
            .validate_replacing("hub", Some("github"), spec.clone())
            .unwrap();
        assert_eq!(renamed.name.as_str(), "hub");
        // Another instance's name stays taken, in any case and of any kind.
        for (name, clash) in [
            ("files", "`[toolset.files]` also uses"),
            ("Files", "`[toolset.files]` also uses"),
            ("web", "`[channel.web]` also uses"),
        ] {
            let err = fixture
                .validate_replacing(name, Some("github"), spec.clone())
                .unwrap_err();
            assert!(err.key.is_root() && err.reason.contains(clash), "{err}");
        }
    }

    #[test]
    fn the_replaced_toolset_is_not_checked_for_overlaps() {
        let tools = tempfile::tempdir().unwrap();
        let tools = tools.path();
        let a = tools.join("a");
        let server = executable(tools, "a/server");
        let fixture = Fixture::new(
            &format!(
                "[toolset.files]\ncommand = \"/bin/files\"\nsandbox = {{ write = [{a:?}] }}\n"
            ),
            "",
        );
        let spec = json!({ "command": server });
        // Added next to `files`, whose grant overlaps this server's directory.
        fixture.assert_invalid("srv", spec.clone(), "", "toolset.files.sandbox.write[0]");
        // Replacing `files` (an edit or a rename), its grants no longer count.
        fixture
            .validate_replacing("files", Some("files"), spec.clone())
            .unwrap();
        fixture
            .validate_replacing("srv", Some("files"), spec)
            .unwrap();
    }

    #[test]
    fn grants_must_not_overlap_binary_directories_of_other_instances() {
        let tools = tempfile::tempdir().unwrap();
        let tools = tools.path();
        let (a, b, c, d) = (
            tools.join("a"),
            tools.join("b"),
            tools.join("c"),
            tools.join("d"),
        );
        let files = executable(tools, "a/server");
        let fixture = Fixture::new(
            &format!(
                "[toolset.files]\ncommand = {files:?}\n\
                 [toolset.other]\ncommand = \"/bin/other\"\nsandbox = {{ write = [{b:?}] }}\n\
                 [toolset.drift]\ncommand = {:?}\n\
                 [toolset.wide]\ncommand = \"/bin/wide\"\nsandbox = {{ write = [{d:?}] }}\n",
                d.join("server")
            ),
            "",
        );

        // The toolset's own grants against another toolset's server.
        fixture.assert_invalid(
            "new",
            json!({ "command": "/bin/new", "sandbox": { "write": [&a] } }),
            "sandbox.write[0]",
            "toolset files's server",
        );
        fixture.assert_invalid(
            "new",
            json!({ "command": "/bin/new", "sandbox": { "unix": [a.join("x.sock")] } }),
            "sandbox.unix[0]",
            "toolset files's server",
        );
        fixture.assert_valid(
            "new",
            json!({ "command": "/bin/new", "sandbox": { "read": [&a] } }),
        );

        // And against its own server.
        let own = executable(tools, "c/server");
        fixture.assert_invalid(
            "mine",
            json!({ "command": own, "sandbox": { "write": [&c] } }),
            "sandbox.write[0]",
            "toolset mine's server",
        );

        // Another instance's grant against this toolset's server: the problem is that
        // instance's key, so it's named in the reason.
        let server = executable(tools, "b/server");
        fixture.assert_invalid(
            "srv",
            json!({ "command": server }),
            "",
            "toolset.other.sandbox.write[0]",
        );

        // Instances that already overlap (the binary appeared after loading) aren't blamed on
        // an unrelated toolset, though loading `giles.toml` again refuses them.
        executable(tools, "d/server");
        fixture.assert_valid("unrelated", json!({ "url": "https://x" }));
        let err = Config::load_with(&fixture.root, &fixture.options).unwrap_err();
        assert_eq!(
            err.key.to_string(),
            "toolset.wide.sandbox.write[0]",
            "{err}"
        );
    }

    #[test]
    fn the_config_hash_follows_the_spec() {
        let hash = |json: &str| ToolsetSpec::from_json(json).unwrap().config_hash();
        let base = hash(r#"{ "command": "/x", "args": ["a"], "deadline": "5m" }"#);
        // Key order and the spelling of a duration don't matter; a changed value does.
        assert_eq!(
            base,
            hash(r#"{ "deadline": "300s", "args": ["a"], "command": "/x" }"#)
        );
        assert_ne!(
            base,
            hash(r#"{ "command": "/x", "args": ["b"], "deadline": "5m" }"#)
        );
        assert_ne!(base, ConfigHash::of(&toml::Table::new()));

        // The instance carries the spec's hash, and no secret value enters it.
        let spec = json!({ "command": "/bin/files", "env": { "TOKEN": { "secret": "files" } } });
        let one = Fixture::new("", "files = \"one\"\n").assert_valid("files", spec.clone());
        let two = Fixture::new("", "files = \"two\"\n").assert_valid("files", spec.clone());
        assert_eq!(one.config_hash, two.config_hash);
        assert_eq!(
            one.config_hash,
            ToolsetSpec::from_json(&spec.to_string())
                .unwrap()
                .config_hash()
        );
    }

    #[test]
    fn a_toolset_in_giles_toml_keeps_the_hash_of_its_spec() {
        // Written the way `giles.toml` has always hashed it (the table as written), a toolset
        // whose table holds only keys the spec has gets the same hash as that spec: the tool-list
        // cache key doesn't change.
        let fixture = Fixture::new(
            "[toolset.files]\ncommand = \"/bin/files\"\nargs = [\"/docs\"]\nparallel = true\n\
             env = { TOKEN = { secret = \"files\" } }\n",
            "files = \"two\"\n",
        );
        let spec = ToolsetSpec::from_json(
            r#"{ "command": "/bin/files", "args": ["/docs"], "parallel": true,
                 "env": { "TOKEN": { "secret": "files" } } }"#,
        )
        .unwrap();
        assert_eq!(
            fixture.config.toolsets["files"].config_hash,
            spec.config_hash()
        );
    }
}

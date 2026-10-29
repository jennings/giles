//! Connecting to one toolset: launching a stdio server in its sandbox, or reaching a remote one.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use giles_store::BlobStore;
use giles_types::{Error, Result};
use tokio::process::Command;
use tracing::{info, warn};

use super::grants::{self, Plan, Protect};
use crate::config::{
    Auth, InstanceName, Kind, NetworkGrant, Remote, StdioServer, ToolsetInstance, ToolsetTransport,
    find_program,
};
use crate::mcp::{HttpServer, McpMedia, McpToolset};
use crate::paths::DataRoot;
use crate::plugin_host::{prepare_data_dir, write_private};
use crate::sandbox::{self, Network, Spec};

/// Looks up an environment variable of the operator's environment (for `PATH`).
type EnvFn = dyn Fn(&str) -> Option<OsString> + Send + Sync;

/// Connects to the toolsets of one data root: launches stdio servers through the sandbox
/// trampoline and connects to remote ones (PLAN section 7.10).
#[derive(Clone)]
pub struct ToolsetLauncher {
    root: DataRoot,
    trampoline: PathBuf,
    sandbox: std::result::Result<(), String>,
    blobs: BlobStore,
    env: Arc<EnvFn>,
}

impl std::fmt::Debug for ToolsetLauncher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolsetLauncher")
            .field("root", &self.root)
            .field("trampoline", &self.trampoline)
            .field("sandbox", &self.sandbox)
            .finish_non_exhaustive()
    }
}

/// A stdio server's program, found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProgram {
    /// Where it was found: `command` as given if absolute, else on the operator's `PATH`.
    pub found: PathBuf,
    /// The same with symbolic links resolved: the file that runs, and whose SHA-256 keys the
    /// tool-list cache (PLAN section 7.9).
    pub program: PathBuf,
}

impl ToolsetLauncher {
    /// A launcher for `root` whose sandbox trampoline is `trampoline` (the `giles` binary, run as
    /// `giles sandbox-exec`). `sandbox` is the host's sandbox check ([`sandbox::check_host`],
    /// run once by `serve`): an error refuses every sandboxed server with that message. Media
    /// the servers' tools return is stored in `blobs`. Programs are looked up on the process's
    /// `PATH`.
    pub fn new(
        root: DataRoot,
        trampoline: PathBuf,
        sandbox: std::result::Result<(), String>,
        blobs: BlobStore,
    ) -> Self {
        Self {
            root,
            trampoline,
            sandbox,
            blobs,
            env: Arc::new(|name| std::env::var_os(name)),
        }
    }

    /// Looks up `PATH` with `env` instead of the process environment.
    #[must_use]
    pub fn env(mut self, env: impl Fn(&str) -> Option<OsString> + Send + Sync + 'static) -> Self {
        self.env = Arc::new(env);
        self
    }

    /// The data root.
    pub fn root(&self) -> &DataRoot {
        &self.root
    }

    /// Finds the program of the stdio server `server` of `instance`: its `command`, with the
    /// operator's `PATH`.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] if it isn't found (it may be installed later) or its links can't be
    /// resolved.
    pub fn resolve(
        &self,
        instance: &InstanceName,
        server: &StdioServer,
    ) -> Result<ResolvedProgram> {
        let command = &server.command;
        let found = find_program(command, &*self.env).ok_or_else(|| {
            unavailable(
                instance,
                format_args!("command {command:?} not found (or not executable) on PATH"),
            )
        })?;
        let program = fs::canonicalize(&found).map_err(|error| {
            unavailable(
                instance,
                format_args!("cannot resolve {}: {error}", found.display()),
            )
        })?;
        Ok(ResolvedProgram { found, program })
    }

    /// Connects to `instance`: launches its stdio server (sandboxed unless `unsandboxed = true`)
    /// or connects to its remote server. Returns once the server has answered `initialize`.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] for what retrying won't fix: the host can't sandbox, an invalid
    /// sandbox spec, rejected credentials. [`Error::Unavailable`] for the rest: the program isn't
    /// found, the server failed to start or answer, a remote server is unreachable.
    pub async fn connect(&self, instance: &ToolsetInstance) -> Result<McpToolset> {
        match &instance.transport {
            ToolsetTransport::Stdio(server) => {
                let resolved = self.resolve(&instance.name, server)?;
                self.launch(&instance.name, server, &resolved).await
            }
            ToolsetTransport::Http(remote) => {
                connect_remote(&instance.name, remote, self.blobs.clone()).await
            }
        }
    }

    /// Launches the stdio server `server` of `instance` as `resolved`.
    ///
    /// # Errors
    ///
    /// As [`ToolsetLauncher::connect`].
    pub async fn launch(
        &self,
        instance: &InstanceName,
        server: &StdioServer,
        resolved: &ResolvedProgram,
    ) -> Result<McpToolset> {
        let name = instance.as_str();
        let sandboxed = !server.sandbox.unsandboxed;
        if sandboxed && let Err(reason) = &self.sandbox {
            return Err(invalid(
                instance,
                format_args!("this host can't sandbox it: {reason}"),
            ));
        }
        let env = &*self.env;
        let home = env("HOME").map(PathBuf::from);
        let protect = Protect {
            root: &self.root,
            instance,
            home: home.as_deref(),
        };
        let plan = grants::plan(&protect, &resolved.found, &resolved.program, &|program| {
            find_program(program, env)
        });
        let data_dir = self
            .root
            .plugin_data_dir(Kind::Toolset, name)
            .map_err(|error| invalid(instance, error))?;
        prepare_data_dir(&data_dir).map_err(|error| unavailable(instance, error))?;
        let spec = stdio_spec(&data_dir, &plan, server);
        let json = spec.to_json().map_err(|error| invalid(instance, error))?;
        let secrets = server.env.values().cloned().collect();

        let command = if sandboxed {
            let spec_file = self
                .root
                .sandbox_spec_file(Kind::Toolset, name)
                .map_err(|error| invalid(instance, error))?;
            write_private(&spec_file, json.as_bytes()).map_err(|error| {
                unavailable(
                    instance,
                    format_args!(
                        "cannot write the sandbox spec {}: {error}",
                        spec_file.display()
                    ),
                )
            })?;
            let mut command = Command::new(&self.trampoline);
            command
                .arg("sandbox-exec")
                .arg(&spec_file)
                .arg("--")
                .arg(&resolved.program)
                .args(&server.args);
            // Named in the spec's `inherit_env`, so the values never touch the spec file.
            for (variable, value) in &server.env {
                command.env(variable, value.expose());
            }
            command
        } else {
            warn!(
                toolset = %instance,
                program = %resolved.program.display(),
                "running the toolset unsandboxed (`unsandboxed = true`)",
            );
            // What the trampoline would do, without the sandbox.
            let inherited = |variable: &str| match server.env.get(variable) {
                Some(value) => Some(value.expose().into()),
                None => (variable == "LANG")
                    .then(|| std::env::var_os("LANG"))
                    .flatten(),
            };
            let mut command = Command::new(&resolved.program);
            command
                .args(&server.args)
                .env_clear()
                .envs(sandbox::scrubbed_env(&spec, &inherited))
                .current_dir(&data_dir);
            command
        };
        info!(
            toolset = %instance,
            program = %resolved.program.display(),
            sandboxed,
            "starting the toolset",
        );
        // `$media` inputs go to the server's `TMPDIR`, inside the data directory it may read.
        let media = McpMedia::new(self.blobs.clone()).files_in(spec.tmp_dir());
        McpToolset::connect_stdio(name, command, secrets, sandboxed, media).await
    }
}

/// The sandbox spec of a stdio server: its configured grants, plus the planned install prefixes
/// readable and executable, its `env` names inherited, and the planned `PATH`.
fn stdio_spec(data_dir: &Path, plan: &Plan, server: &StdioServer) -> Spec {
    let grants = &server.sandbox.grants;
    let mut spec = Spec::new(data_dir);
    spec.read.clone_from(&plan.grants);
    spec.read.extend(grants.read.iter().cloned());
    spec.execute.clone_from(&plan.grants);
    spec.write.clone_from(&grants.write);
    spec.unix.clone_from(&grants.unix);
    spec.network = match &grants.network {
        NetworkGrant::None => Network::Off,
        NetworkGrant::All => Network::All,
        NetworkGrant::TcpPorts(ports) => Network::Tcp(ports.iter().copied().collect()),
    };
    spec.min_abi = grants.min_abi;
    spec.unsandboxed = server.sandbox.unsandboxed;
    spec.inherit_env = server.env.keys().cloned().collect();
    spec.path = Some(plan.path.clone());
    spec
}

/// Connects to a remote server with its bearer token, if any, storing the media its tools return
/// in `blobs`.
async fn connect_remote(
    instance: &InstanceName,
    remote: &Remote,
    blobs: BlobStore,
) -> Result<McpToolset> {
    let mut server = HttpServer::new(remote.url.clone());
    server.bearer = match &remote.auth {
        None => None,
        Some(Auth::Bearer(token)) => Some(token.clone()),
        // Config loading refuses OAuth for toolsets.
        Some(_) => {
            return Err(invalid(
                instance,
                "remote toolsets support only bearer tokens",
            ));
        }
    };
    McpToolset::connect_http(instance.as_str(), &server, blobs).await
}

fn unavailable(instance: &InstanceName, message: impl std::fmt::Display) -> Error {
    Error::unavailable(format!("toolset {instance}: {message}"))
}

fn invalid(instance: &InstanceName, message: impl std::fmt::Display) -> Error {
    Error::InvalidRequest(format!("toolset {instance}: {message}"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::config::{Grants, Sandbox, Secret};

    fn server(env: &[(&str, &str)], grants: Grants) -> StdioServer {
        StdioServer {
            command: "x".into(),
            args: Vec::new(),
            env: env
                .iter()
                .map(|(k, v)| ((*k).to_owned(), Secret::new(*v)))
                .collect::<BTreeMap<_, _>>(),
            idle_stop: None,
            sandbox: Sandbox {
                unsandboxed: false,
                grants,
            },
        }
    }

    #[test]
    fn the_spec_grants_the_plan_and_inherits_env_by_name() {
        let plan = Plan {
            grants: vec!["/opt/homebrew".into()],
            path: "/opt/homebrew/bin:/usr/bin:/bin".into(),
        };
        let grants = Grants {
            read: vec!["/Users/me/Documents".into()],
            network: NetworkGrant::TcpPorts(vec![443]),
            ..Grants::default()
        };
        let server = server(&[("GITHUB_TOKEN", "ghp_secret")], grants);
        let spec = stdio_spec(Path::new("/g/data/toolset/files"), &plan, &server);
        assert_eq!(
            spec.read,
            [Path::new("/opt/homebrew"), Path::new("/Users/me/Documents")]
        );
        assert_eq!(spec.execute, [Path::new("/opt/homebrew")]);
        assert_eq!(spec.network, Network::Tcp([443].into()));
        assert_eq!(spec.search_path(), "/opt/homebrew/bin:/usr/bin:/bin");
        assert_eq!(spec.inherit_env, ["GITHUB_TOKEN".to_owned()].into());
        assert!(spec.env.is_empty());
        let json = spec.to_json().unwrap();
        assert!(!json.contains("ghp_secret"), "{json}");
    }

    #[test]
    fn network_is_off_without_a_grant() {
        let plan = Plan {
            grants: Vec::new(),
            path: "/usr/bin:/bin".into(),
        };
        let spec = stdio_spec(Path::new("/d"), &plan, &server(&[], Grants::default()));
        assert_eq!(spec.network, Network::Off);
        assert!(!spec.unsandboxed);
    }
}

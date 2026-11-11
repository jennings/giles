//! `giles-environment-docker`: the Giles environment plugin that runs workspaces in containers,
//! through the Docker Engine API (Docker, Podman's compatible API, Docker Desktop, OrbStack,
//! Colima).
//!
//! A workspace is a container plus a named volume mounted at `/workspace`. [`DockerEnvironment`]
//! has one method per environment RPC. The plugin keeps no state of its own: everything it needs
//! to find a workspace again is in labels on the volume (which outlives the container) and the
//! container:
//!
//! - `giles.instance` and `giles.workspace` say whose it is. A different plugin instance on the
//!   same engine never sees it.
//! - `giles.image-digest` pins the image the workspace was created from, so a recreated container
//!   runs the same software even if a tag like `latest` moved.
//! - `giles.pulled` marks an image this plugin pulled, so deleting the last workspace that uses it
//!   removes it again and an image the operator already had is left alone.
//!
//! Commands run through `giles-workspace-helper` (see the `giles-workspace-helper` crate), which
//! the plugin copies into each container: it is the process-group wrapper that lets a second exec
//! kill a cancelled command, and it runs `find`, `grep`, and the file calls.

mod config;
mod error;
mod exec;
mod files;
mod helper;
mod image;
mod lifecycle;
mod types;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bollard::Docker;
use serde_json::{Value, json};

pub use config::{Config, DEFAULT_NETWORK, DEFAULT_SOCKET, Registry, Settings, parse_memory};
pub use error::{Error, Result};
pub use helper::{Arch, HELPER_PATH};
pub use image::{Allowlist, DEFAULT_PATTERNS, ParseError, Reference};
pub use types::{
    EnvironmentInfo, ExecEvent, ExecExit, ExecRequest, FindRequest, FindResult, GrepMatch,
    GrepRequest, GrepResult, ReadFile, StartOutcome, Workspace, WorkspaceList, WorkspaceState,
};

/// The directory commands start in, where the workspace volume is mounted.
pub const WORKSPACE_DIR: &str = "/workspace";

/// How long a request to the engine may take before it is abandoned. (Streaming bodies, such as an
/// image pull or a command's output, aren't limited by it.)
const API_TIMEOUT_SECS: u64 = 120;

/// A Docker engine hosting workspaces for one `[environment.<name>]` instance.
///
/// Cheap to clone; clones share the connection. Methods mirror `giles.environment.v1`.
#[derive(Clone)]
pub struct DockerEnvironment {
    inner: Arc<Inner>,
}

struct Inner {
    docker: Docker,
    /// The `[environment.<name>]` instance name; labels keep instances' workspaces apart.
    instance: String,
    settings: Settings,
    arch: Arch,
    /// The helper binary for the engine's architecture.
    helper: Vec<u8>,
    /// One lock per workspace id, so two lifecycle calls on one workspace don't race (two
    /// `start_workspace` calls must not both recreate the container).
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl std::fmt::Debug for DockerEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DockerEnvironment")
            .field("instance", &self.inner.instance)
            .field("socket", &self.inner.settings.socket())
            .field("arch", &self.inner.arch)
            .finish_non_exhaustive()
    }
}

impl DockerEnvironment {
    /// Connects to the engine and checks everything that can be checked up front: the socket
    /// answers, the engine runs Linux containers, and a helper binary exists for its architecture.
    ///
    /// `instance` is the plugin instance's name (the `<name>` of `[environment.<name>]`).
    pub async fn connect(config: Config, instance: &str) -> Result<Self> {
        let docker_host = std::env::var("DOCKER_HOST").ok();
        let settings = Settings::resolve(config, docker_host.as_deref())?;
        Self::connect_with(settings, instance).await
    }

    /// Like [`DockerEnvironment::connect`], with settings that are already resolved.
    pub async fn connect_with(settings: Settings, instance: &str) -> Result<Self> {
        if instance.is_empty() {
            return Err(Error::invalid("the plugin instance needs a name"));
        }
        let socket = settings.socket().to_string_lossy().into_owned();
        let docker =
            Docker::connect_with_unix(&socket, API_TIMEOUT_SECS, bollard::API_DEFAULT_VERSION)
                .map_err(|e| {
                    Error::Unavailable(format!("can't use the Docker socket {socket}: {e}"))
                })?;
        let docker = docker
            .negotiate_version()
            .await
            .map_err(|e| Error::docker(&format!("the Docker API at {socket} didn't answer"), &e))?;
        let info = docker.info().await.map_err(|e| {
            Error::docker(
                &format!("asking the Docker API at {socket} about itself"),
                &e,
            )
        })?;
        if info.os_type.as_deref().is_some_and(|os| os != "linux") {
            return Err(Error::invalid(format!(
                "the container engine at {socket} runs {} containers; only Linux is supported",
                info.os_type.as_deref().unwrap_or_default()
            )));
        }
        let arch = Arch::from_engine(info.architecture.as_deref().unwrap_or_default())?;
        let helper_dir = match &settings.helper_dir {
            Some(dir) => dir.clone(),
            None => helper::default_dir()?,
        };
        let helper = helper::load(&helper_dir, arch)?;
        Ok(Self {
            inner: Arc::new(Inner {
                docker,
                instance: instance.to_owned(),
                settings,
                arch,
                helper,
                locks: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// The creation options this instance accepts. Only `image`; the schema lists the allowed
    /// image patterns, so the model sees them.
    pub async fn info(&self) -> Result<EnvironmentInfo> {
        Ok(EnvironmentInfo {
            options_schema: options_schema(&self.inner.settings.allowlist().describe()),
        })
    }

    /// The architecture the engine reports, and so the helper the workspaces use.
    pub fn arch(&self) -> Arch {
        self.inner.arch
    }

    pub(crate) fn docker(&self) -> &Docker {
        &self.inner.docker
    }

    pub(crate) fn settings(&self) -> &Settings {
        &self.inner.settings
    }

    pub(crate) fn instance(&self) -> &str {
        &self.inner.instance
    }

    pub(crate) fn helper_bytes(&self) -> &[u8] {
        &self.inner.helper
    }

    /// Locks one workspace id until the guard drops.
    pub(crate) async fn lock(&self, id: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.inner.locks.lock().unwrap_or_else(|e| e.into_inner());
            Arc::clone(locks.entry(id.to_owned()).or_default())
        };
        lock.lock_owned().await
    }

    /// Forgets a workspace's lock once the workspace is gone.
    pub(crate) fn forget_lock(&self, id: &str) {
        self.inner
            .locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
    }
}

/// The JSON Schema of the creation options, with `allowed` (the image patterns) in the
/// description.
fn options_schema(allowed: &str) -> Value {
    json!({
        "type": "object",
        "properties": {
            "image": {
                "type": "string",
                "description": format!(
                    "The Docker image to start the workspace from, as [registry/]image[:tag]; \
                     the tag defaults to latest. It needs /bin/sh. Allowed images: {allowed}. \
                     A `*` tag allows any tag. Software that isn't in the image has to be \
                     installed in the workspace."
                ),
            }
        },
        "required": ["image"],
        "additionalProperties": false,
    })
}

/// Checks a workspace id the way the protocol defines it: 1 to 64 characters of `a-z`, `0-9`, `-`.
pub(crate) fn check_workspace_id(id: &str) -> Result<()> {
    let valid = !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if valid {
        Ok(())
    } else {
        Err(Error::invalid(
            "a workspace id is 1 to 64 characters of a-z, 0-9, and \"-\"",
        ))
    }
}

/// Labels as the engine wants them.
pub(crate) fn label_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_ids_follow_the_protocol() {
        assert!(check_workspace_id("a").is_ok());
        assert!(check_workspace_id("0b1c-ff").is_ok());
        assert!(check_workspace_id(&"a".repeat(64)).is_ok());
        for bad in ["", "A", "a_b", "a b", "a/b", "../x", &"a".repeat(65)] {
            assert!(check_workspace_id(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_schema_takes_only_an_image_and_lists_the_allowed_patterns() {
        let schema = options_schema("ubuntu:*, busybox");
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["required"], json!(["image"]));
        assert_eq!(schema["properties"].as_object().unwrap().len(), 1);
        let description = schema["properties"]["image"]["description"]
            .as_str()
            .unwrap();
        assert!(description.contains("ubuntu:*, busybox"), "{description}");
    }
}

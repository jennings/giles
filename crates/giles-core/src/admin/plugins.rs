//! Plugin instances for `Status` and `ListPlugins`: what the configuration says, plus what plugin
//! supervision reports through [`PluginControl`].

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use giles_plugin_sdk::convert::{status_to_proto, timestamp_to_proto};
use giles_proto::admin::v1 as pb;
use giles_proto::plugin::v1::SandboxGrants;
use giles_types::Timestamp;

use crate::config::{
    ChannelRuntime, Config, InstanceName, NetworkGrant, Program, Sandbox, ToolsetTransport,
};
use crate::sandbox::HostReport;
use crate::sessions::ChannelStatusRecord;

/// Plugin supervision, as the admin service sees it: a snapshot of every supervised instance,
/// and a restart on request. The plugin host implements it; until it's wired,
/// `RestartPlugin` answers `UNAVAILABLE` and the lifecycle fields of `ListPlugins` stay unset.
pub trait PluginControl: Send + Sync {
    /// Every supervised instance, in any order.
    fn plugins(&self) -> Vec<PluginRuntime>;

    /// Relaunches (or reconnects) `instance` now, skipping its backoff or the wait for a new
    /// binary. `false` if it isn't supervised.
    fn restart(&self, instance: &str) -> bool;
}

/// Several [`PluginControl`]s as one: `serve` has a plugin host for channel plugins and a toolset
/// host for toolsets, and the admin service takes a single control.
///
/// `plugins()` is every control's instances; `restart` goes to the first control that supervises
/// the instance (instance names are unique across kinds, so at most one does).
pub struct MergedControl {
    controls: Vec<Arc<dyn PluginControl>>,
}

impl MergedControl {
    /// Merges `controls`.
    pub fn new(controls: Vec<Arc<dyn PluginControl>>) -> Self {
        Self { controls }
    }
}

impl PluginControl for MergedControl {
    fn plugins(&self) -> Vec<PluginRuntime> {
        self.controls
            .iter()
            .flat_map(|control| control.plugins())
            .collect()
    }

    fn restart(&self, instance: &str) -> bool {
        self.controls
            .iter()
            .any(|control| control.restart(instance))
    }
}

/// What supervision knows about one instance.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginRuntime {
    /// The instance.
    pub instance: InstanceName,
    /// The binary found for it (symlinks resolved), once looked up.
    pub binary: Option<PathBuf>,
    /// Where it is in its lifecycle.
    pub state: PluginRunState,
    /// Relaunches since the core started.
    pub restarts: u32,
    /// The last launch failure, crash, or failed health check.
    pub last_error: Option<String>,
    /// The version it reported (`Describe` or the MCP server info).
    pub version: Option<String>,
    /// The sandbox grants it asked for in `Describe`. Advisory only.
    pub requested_grants: Option<SandboxGrants>,
    /// The number of tools it offers to new sessions. Toolsets only.
    pub tool_count: Option<u32>,
}

/// Where a supervised instance is in its lifecycle (PLAN section 7.5).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PluginRunState {
    /// Being launched or connected.
    Starting,
    /// Up and healthy.
    Running {
        /// Its process id; `None` for a remote instance.
        pid: Option<u32>,
        /// When it came up.
        since: Timestamp,
    },
    /// A toolset stopped for idleness; the next call relaunches it.
    Idle,
    /// Down after a failure, relaunched at `until`.
    BackingOff {
        /// When the next attempt starts.
        until: Timestamp,
    },
    /// Down after a failure relaunching won't fix, until its binary changes or a restart is
    /// requested.
    Failed,
    /// Stopped because the core is shutting down.
    Stopped,
}

/// One configured instance that runs as a plugin, as the configuration describes it.
pub(super) struct Configured<'a> {
    pub(super) instance: &'a InstanceName,
    pub(super) kind: pb::InstanceKind,
    pub(super) component: &'a str,
    /// The program as configured, or the URL.
    pub(super) location: String,
    /// `None` for remote instances.
    pub(super) sandbox: Option<&'a Sandbox>,
}

/// Every configured instance that runs as a plugin, sorted by kind and then name. v1 providers are
/// all built in, and built-in channels run in the core, so neither is listed.
pub(super) fn configured(config: &Config) -> Vec<Configured<'_>> {
    let channels = config.channels.values().filter_map(|c| {
        let (location, sandbox) = match &c.runtime {
            ChannelRuntime::Plugin(launch) => (launch.binary.clone(), Some(&launch.sandbox)),
            ChannelRuntime::Remote(remote) => (remote.url.to_string(), None),
            ChannelRuntime::BuiltIn => return None,
        };
        Some(Configured {
            instance: &c.name,
            kind: pb::InstanceKind::Channel,
            component: &c.component,
            location,
            sandbox,
        })
    });
    let toolsets = config.toolsets.values().map(|t| {
        let (component, location, sandbox) = match &t.transport {
            ToolsetTransport::Stdio(stdio) => match &stdio.program {
                Program::Command(command) => ("", command.clone(), Some(&stdio.sandbox)),
                Program::Plugin { component, binary } => {
                    (component.as_str(), binary.clone(), Some(&stdio.sandbox))
                }
            },
            ToolsetTransport::Http(remote) => ("", remote.url.to_string(), None),
        };
        Configured {
            instance: &t.name,
            kind: pb::InstanceKind::Toolset,
            component,
            location,
            sandbox,
        }
    });
    channels.chain(toolsets).collect()
}

/// The `PluginInstance` of every configured plugin: the configuration, the host's sandbox, the
/// channel statuses recorded since startup, and what supervision reports in `runtimes`.
pub(super) fn plugin_instances(
    config: &Config,
    host: &Result<HostReport, String>,
    statuses: &BTreeMap<InstanceName, ChannelStatusRecord>,
    runtimes: Vec<PluginRuntime>,
) -> Vec<pb::PluginInstance> {
    let mut runtimes: HashMap<InstanceName, PluginRuntime> = runtimes
        .into_iter()
        .map(|r| (r.instance.clone(), r))
        .collect();
    configured(config)
        .into_iter()
        .map(|c| {
            let mut instance = pb::PluginInstance {
                instance: c.instance.to_string(),
                kind: c.kind.into(),
                component: c.component.to_owned(),
                location: c.location,
                sandbox: c.sandbox.map(|sandbox| sandbox_status(sandbox, host)),
                ..pb::PluginInstance::default()
            };
            if c.kind == pb::InstanceKind::Channel {
                instance.channel_status = statuses
                    .get(c.instance)
                    .and_then(|record| status_to_proto(record.status.clone()).ok());
            }
            if let Some(runtime) = runtimes.remove(c.instance) {
                apply_runtime(&mut instance, runtime);
            }
            instance
        })
        .collect()
}

fn apply_runtime(instance: &mut pb::PluginInstance, runtime: PluginRuntime) {
    use pb::PluginState as S;
    let state = match runtime.state {
        PluginRunState::Starting => S::Starting,
        PluginRunState::Running { pid, since } => {
            instance.pid = pid;
            instance.started_at = Some(timestamp_to_proto(since));
            S::Running
        }
        PluginRunState::Idle => S::Idle,
        PluginRunState::BackingOff { until } => {
            instance.retry_at = Some(timestamp_to_proto(until));
            S::Backoff
        }
        PluginRunState::Failed => S::Failed,
        PluginRunState::Stopped => S::Stopped,
    };
    instance.state = state.into();
    if let Some(binary) = runtime.binary {
        instance.location = binary.display().to_string();
    }
    instance.restarts = runtime.restarts;
    instance.last_error = runtime.last_error;
    instance.version = runtime.version;
    instance.requested_grants = runtime.requested_grants;
    instance.tool_count = runtime.tool_count;
}

/// The sandbox an instance with `sandbox` gets on this host.
fn sandbox_status(sandbox: &Sandbox, host: &Result<HostReport, String>) -> pb::SandboxStatus {
    if sandbox.unsandboxed {
        return pb::SandboxStatus::default();
    }
    match host {
        Ok(report) => {
            let tcp_ports = matches!(sandbox.grants.network, NetworkGrant::TcpPorts(_));
            let unix = !sandbox.grants.unix.is_empty();
            pb::SandboxStatus {
                enabled: true,
                landlock_abi: landlock_abi(report),
                missing: report
                    .lacking(tcp_ports, unix)
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
            }
        }
        Err(error) => pb::SandboxStatus {
            enabled: true,
            landlock_abi: None,
            missing: vec![format!(
                "everything: this host can't sandbox plugins, so the plugin can't start: {error}"
            )],
        },
    }
}

/// The effective Landlock ABI, on Linux.
pub(super) fn landlock_abi(report: &HostReport) -> Option<u32> {
    match report {
        HostReport::Linux { landlock_abi } => Some(*landlock_abi),
        _ => None,
    }
}

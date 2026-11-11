//! A configured gRPC plugin instance of any kind, as the host launches or connects to it.
//!
//! Channels and environments are both gRPC plugins that differ only in their service, their
//! `Describe` capabilities, and what the core does with them once they are up. The launch, the
//! remote connection, the pid files, the `Describe` cache, and supervision work on a
//! [`PluginInstance`], which each kind's configuration converts to.

use giles_plugin_sdk::convert::output_mode_from_proto;
use giles_proto::channel::v1::channel_service_server::SERVICE_NAME as CHANNEL_SERVICE;
use giles_proto::environment::v1::environment_service_server::SERVICE_NAME as ENVIRONMENT_SERVICE;
use giles_proto::plugin::v1::{DescribeResponse, PluginKind, describe_response};

use crate::config::{
    ChannelInstance, ChannelRuntime, ConfigHash, EnvironmentInstance, EnvironmentRuntime,
    InstanceName, Kind, Launch, PluginConfig, Remote,
};

/// What differs between the kinds of gRPC plugin.
#[derive(Debug)]
pub(crate) struct KindSpec {
    /// The kind: the directories of its instances and the name of its binaries.
    pub kind: Kind,
    /// The kind `Describe` must report.
    pub proto: PluginKind,
    /// The gRPC service whose health supervision checks.
    pub service: &'static str,
    /// Whether `Configure` returns an identity namespace the core checks (channels).
    pub identity_namespace: bool,
    /// Checks the capabilities `Describe` reported, naming what is wrong.
    capabilities: fn(&DescribeResponse) -> Result<(), String>,
}

/// Channel plugins.
pub(crate) static CHANNEL: KindSpec = KindSpec {
    kind: Kind::Channel,
    proto: PluginKind::Channel,
    service: CHANNEL_SERVICE,
    identity_namespace: true,
    capabilities: channel_capabilities,
};

fn channel_capabilities(describe: &DescribeResponse) -> Result<(), String> {
    let Some(describe_response::Capabilities::Channel(capabilities)) = &describe.capabilities
    else {
        return Err("it describes no channel capabilities".to_owned());
    };
    if output_mode_from_proto(capabilities.output).is_err() {
        return Err(format!(
            "its output mode {} is unspecified or unknown",
            capabilities.output
        ));
    }
    Ok(())
}

/// Environment plugins.
pub(crate) static ENVIRONMENT: KindSpec = KindSpec {
    kind: Kind::Environment,
    proto: PluginKind::Environment,
    service: ENVIRONMENT_SERVICE,
    identity_namespace: false,
    capabilities: environment_capabilities,
};

fn environment_capabilities(describe: &DescribeResponse) -> Result<(), String> {
    match &describe.capabilities {
        Some(describe_response::Capabilities::Environment(_)) => Ok(()),
        _ => Err("it describes no environment capabilities".to_owned()),
    }
}

/// Checks `Describe` against the binary `giles-<kind>-<component>`: the kind, the name, and the
/// capabilities of the kind. The error is the reason.
pub(crate) fn check_describe(
    describe: &DescribeResponse,
    spec: &KindSpec,
    component: &str,
) -> Result<(), String> {
    let kind = PluginKind::try_from(describe.kind).unwrap_or(PluginKind::Unspecified);
    if kind != spec.proto {
        return Err(format!(
            "it describes itself as {}, expected {}",
            kind.as_str_name(),
            spec.proto.as_str_name()
        ));
    }
    if describe.name != component {
        return Err(format!(
            "it is named {:?}, expected {component:?}",
            describe.name
        ));
    }
    (spec.capabilities)(describe)
}

/// Where an instance runs.
#[derive(Debug, Clone)]
pub(crate) enum InstanceRuntime {
    /// A binary the core launches.
    Launched(Launch),
    /// A plugin someone else runs.
    Remote(Remote),
}

/// One configured plugin instance: what a launch or a connection, and supervision, need.
#[derive(Debug, Clone)]
pub(crate) struct PluginInstance {
    pub spec: &'static KindSpec,
    pub name: InstanceName,
    pub component: String,
    pub runtime: InstanceRuntime,
    pub config: PluginConfig,
    pub config_hash: ConfigHash,
}

impl PluginInstance {
    /// The kind.
    pub(crate) fn kind(&self) -> Kind {
        self.spec.kind
    }

    /// The plugin channel `instance`, or `None` for a built-in channel.
    pub(crate) fn channel(instance: &ChannelInstance) -> Option<Self> {
        let runtime = match &instance.runtime {
            ChannelRuntime::BuiltIn => return None,
            ChannelRuntime::Plugin(launch) => InstanceRuntime::Launched(launch.clone()),
            ChannelRuntime::Remote(remote) => InstanceRuntime::Remote(remote.clone()),
        };
        Some(Self {
            spec: &CHANNEL,
            name: instance.name.clone(),
            component: instance.component.clone(),
            runtime,
            config: instance.config.clone(),
            config_hash: instance.config_hash,
        })
    }

    /// The environment `instance`.
    pub(crate) fn environment(instance: &EnvironmentInstance) -> Self {
        let runtime = match &instance.runtime {
            EnvironmentRuntime::Plugin(launch) => InstanceRuntime::Launched(launch.clone()),
            EnvironmentRuntime::Remote(remote) => InstanceRuntime::Remote(remote.clone()),
        };
        Self {
            spec: &ENVIRONMENT,
            name: instance.name.clone(),
            component: instance.component.clone(),
            runtime,
            config: instance.config.clone(),
            config_hash: instance.config_hash,
        }
    }
}

#[cfg(test)]
mod tests {
    use giles_proto::channel::v1::{ChannelCapabilities, OutputMode};
    use giles_proto::provider::v1::ProviderCapabilities;

    use super::*;

    fn describe(kind: PluginKind, name: &str, channel: bool) -> DescribeResponse {
        DescribeResponse {
            kind: kind.into(),
            name: name.to_owned(),
            version: "1.0".to_owned(),
            requested_grants: None,
            capabilities: Some(if channel {
                describe_response::Capabilities::Channel(ChannelCapabilities {
                    output: OutputMode::Blocks.into(),
                    can_initiate: false,
                })
            } else {
                describe_response::Capabilities::Provider(ProviderCapabilities::default())
            }),
        }
    }

    #[test]
    fn describe_must_match_the_binary() {
        let check = |describe: &DescribeResponse| check_describe(describe, &CHANNEL, "irc");
        assert_eq!(check(&describe(PluginKind::Channel, "irc", true)), Ok(()));
        let err = check(&describe(PluginKind::Provider, "irc", true)).unwrap_err();
        assert!(err.contains("PLUGIN_KIND_PROVIDER"), "{err}");
        let err = check(&describe(PluginKind::Channel, "irk", true)).unwrap_err();
        assert!(err.contains("\"irk\""), "{err}");
        let err = check(&describe(PluginKind::Channel, "irc", false)).unwrap_err();
        assert!(err.contains("capabilities"), "{err}");
        let mut unknown = describe(PluginKind::Channel, "irc", true);
        unknown.kind = 7;
        assert!(check(&unknown).is_err());
        let mut no_output = describe(PluginKind::Channel, "irc", true);
        no_output.capabilities = Some(describe_response::Capabilities::Channel(
            ChannelCapabilities::default(),
        ));
        let err = check(&no_output).unwrap_err();
        assert!(err.contains("output mode 0"), "{err}");
    }

    #[test]
    fn an_environment_describe_must_be_an_environment_with_its_capabilities() {
        use giles_proto::environment::v1::EnvironmentCapabilities;
        let environment = |kind: PluginKind, name: &str| DescribeResponse {
            kind: kind.into(),
            name: name.to_owned(),
            version: "1.0".to_owned(),
            requested_grants: None,
            capabilities: Some(describe_response::Capabilities::Environment(
                EnvironmentCapabilities {},
            )),
        };
        let check = |describe: &DescribeResponse| check_describe(describe, &ENVIRONMENT, "docker");
        assert_eq!(
            check(&environment(PluginKind::Environment, "docker")),
            Ok(())
        );
        // A channel plugin installed as an environment is refused, by its kind.
        let err = check(&environment(PluginKind::Channel, "docker")).unwrap_err();
        assert!(err.contains("PLUGIN_KIND_CHANNEL"), "{err}");
        assert!(err.contains("PLUGIN_KIND_ENVIRONMENT"), "{err}");
        let err = check(&environment(PluginKind::Environment, "other")).unwrap_err();
        assert!(err.contains("\"other\""), "{err}");
        // The right kind with another kind's capabilities.
        let mut channel_caps = describe(PluginKind::Environment, "docker", true);
        let err = check(&channel_caps).unwrap_err();
        assert!(err.contains("no environment capabilities"), "{err}");
        channel_caps.capabilities = None;
        assert!(check(&channel_caps).is_err());
        // And an environment's describe is no channel's.
        let err = check_describe(
            &environment(PluginKind::Environment, "docker"),
            &CHANNEL,
            "docker",
        )
        .unwrap_err();
        assert!(err.contains("PLUGIN_KIND_ENVIRONMENT"), "{err}");
    }
}

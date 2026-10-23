//! `giles status`: the running daemon's state, from the admin service's `Status`.

use std::fmt::Write as _;
use std::path::Path;

use giles_proto::admin::v1 as pb;
use giles_proto::channel::v1::channel_status::State;
use giles_proto::channel::v1::needs_auth::Challenge;
use giles_proto::channel::v1::{ChannelStatus, NeedsAuth};
use giles_proto::plugin::v1::SandboxGrants;
use jiff::tz::TimeZone;

use crate::admin_client::{self, proto_time};
use crate::cli::StatusArgs;

/// Prints the daemon's state.
pub fn run(home: Option<&Path>, _args: &StatusArgs) -> anyhow::Result<()> {
    let status = admin_client::call(home, async |mut client| {
        Ok(client.status(pb::StatusRequest {}).await?.into_inner())
    })?;
    print!("{}", format_status(&status, &TimeZone::system()));
    Ok(())
}

/// The human-readable summary, times in `tz`.
fn format_status(status: &pb::StatusResponse, tz: &TimeZone) -> String {
    let mut out = String::new();
    let time = |at: Option<prost_types::Timestamp>| at.map(|at| proto_time(at, tz));
    let _ = write!(out, "giles {}", status.version);
    if let Some(since) = time(status.started_at) {
        let _ = write!(out, ", running since {since}");
    }
    out.push('\n');
    if let Some(host) = &status.sandbox_host {
        if host.available {
            let _ = writeln!(out, "Plugin sandbox: {}", host.description);
        } else {
            let _ = writeln!(out, "Plugin sandbox: unavailable: {}", host.description);
        }
    }
    if let Some(error) = &status.config_error {
        let _ = writeln!(out, "Configuration error: {error}");
    }

    out.push_str("\nChannels:");
    if status.channels.is_empty() {
        out.push_str(" none");
    }
    out.push('\n');
    let width = status
        .channels
        .iter()
        .map(|c| c.instance.len())
        .max()
        .unwrap_or(0);
    for channel in &status.channels {
        let runtime = match channel.runtime() {
            pb::ChannelRuntime::BuiltIn => "built in",
            pb::ChannelRuntime::Plugin => "plugin",
            pb::ChannelRuntime::Remote => "remote",
            pb::ChannelRuntime::Unspecified => "unknown",
        };
        let mut state = channel_status(channel.status.as_ref(), tz);
        if let Some(at) = time(channel.status_at) {
            let _ = write!(state, " (since {at})");
        }
        let _ = writeln!(
            out,
            "  {:width$}  {} ({runtime}): {state}",
            channel.instance, channel.component
        );
    }

    out.push_str("\nPlugins:");
    if status.plugins.is_empty() {
        out.push_str(" none");
    }
    out.push('\n');
    for plugin in &status.plugins {
        format_plugin(&mut out, plugin, tz);
    }
    out
}

fn format_plugin(out: &mut String, plugin: &pb::PluginInstance, tz: &TimeZone) {
    let kind = match plugin.kind() {
        pb::InstanceKind::Provider => "provider",
        pb::InstanceKind::Channel => "channel",
        pb::InstanceKind::Toolset => "toolset",
        pb::InstanceKind::Unspecified => "unknown kind",
    };
    if plugin.component.is_empty() {
        let _ = writeln!(out, "  {} ({kind})", plugin.instance);
    } else {
        let _ = writeln!(out, "  {} ({kind}, {})", plugin.instance, plugin.component);
    }
    let field = |out: &mut String, name: &str, value: &str| {
        let _ = writeln!(out, "    {name:<10} {value}");
    };

    let mut state = match plugin.state() {
        pb::PluginState::Unspecified => "unknown (plugin supervision is not available)".to_owned(),
        pb::PluginState::Starting => "starting".to_owned(),
        pb::PluginState::Running => {
            let mut s = "running".to_owned();
            if let Some(pid) = plugin.pid {
                let _ = write!(s, ", pid {pid}");
            }
            if let Some(since) = plugin.started_at {
                let _ = write!(s, ", since {}", proto_time(since, tz));
            }
            s
        }
        pb::PluginState::Idle => "idle (starts on the next call)".to_owned(),
        pb::PluginState::Backoff => match plugin.retry_at {
            Some(at) => format!("down, retrying at {}", proto_time(at, tz)),
            None => "down, retrying".to_owned(),
        },
        pb::PluginState::Failed => {
            "failed (not retried until its binary changes or it's restarted)".to_owned()
        }
        pb::PluginState::Stopped => "stopped".to_owned(),
    };
    if plugin.restarts > 0 {
        let _ = write!(state, "; {} restarts", plugin.restarts);
    }
    field(out, "state:", &state);
    if let Some(error) = &plugin.last_error {
        field(out, "error:", error);
    }
    if plugin.kind() == pb::InstanceKind::Channel {
        field(
            out,
            "channel:",
            &channel_status(plugin.channel_status.as_ref(), tz),
        );
    }
    field(out, "location:", &plugin.location);
    if let Some(version) = &plugin.version {
        field(out, "version:", version);
    }
    match &plugin.sandbox {
        None => field(out, "sandbox:", "none (remote)"),
        Some(sandbox) if !sandbox.enabled => field(out, "sandbox:", "off (unsandboxed)"),
        Some(sandbox) => {
            let mut text = "on".to_owned();
            if let Some(abi) = sandbox.landlock_abi {
                let _ = write!(text, ", Landlock ABI {abi}");
            }
            field(out, "sandbox:", &text);
            for missing in &sandbox.missing {
                field(out, "lacking:", missing);
            }
        }
    }
    if let Some(grants) = &plugin.requested_grants {
        field(out, "requests:", &requested_grants(grants));
    }
}

/// A channel's last status, in words.
fn channel_status(status: Option<&ChannelStatus>, tz: &TimeZone) -> String {
    match status.and_then(|s| s.state.as_ref()) {
        None => "no status yet".to_owned(),
        Some(State::Connected(_)) => "connected".to_owned(),
        Some(State::Error(message)) => format!("error: {message}"),
        Some(State::NeedsAuth(NeedsAuth {
            challenge,
            expires_at,
        })) => {
            let mut text = match challenge {
                Some(Challenge::QrCode(payload)) => {
                    format!("needs linking: scan the QR code {payload}")
                }
                Some(Challenge::PairCode(code)) => format!("needs linking: enter the code {code}"),
                Some(Challenge::Url(url)) => format!("needs login: open {url}"),
                None => "needs login".to_owned(),
            };
            if let Some(at) = expires_at {
                let _ = write!(text, " (until {})", proto_time(*at, tz));
            }
            text
        }
    }
}

/// The grants a plugin asked for in `Describe`.
fn requested_grants(grants: &SandboxGrants) -> String {
    let mut parts = Vec::new();
    if grants.network_all {
        parts.push("network: all".to_owned());
    } else if !grants.network_ports.is_empty() {
        parts.push(format!("network: {}", grants.network_ports.join(", ")));
    }
    for (name, paths) in [("read", &grants.read), ("write", &grants.write)] {
        if !paths.is_empty() {
            parts.push(format!("{name}: {}", paths.join(", ")));
        }
    }
    if parts.is_empty() {
        "nothing".to_owned()
    } else {
        parts.join("; ")
    }
}

#[cfg(test)]
mod tests {
    use giles_proto::channel::v1::Connected;

    use super::*;

    fn ts(seconds: i64) -> Option<prost_types::Timestamp> {
        Some(prost_types::Timestamp { seconds, nanos: 0 })
    }

    #[test]
    fn a_full_status_reads_well() {
        let status = pb::StatusResponse {
            version: "0.1.0".into(),
            started_at: ts(1_800_000_000),
            config_error: None,
            sandbox_host: Some(pb::SandboxHost {
                available: true,
                description: "Landlock ABI 5 and seccomp filters".into(),
                landlock_abi: Some(5),
            }),
            channels: vec![
                pb::ChannelInstance {
                    instance: "libera".into(),
                    component: "irc".into(),
                    runtime: pb::ChannelRuntime::Plugin.into(),
                    status: Some(ChannelStatus {
                        state: Some(State::NeedsAuth(NeedsAuth {
                            challenge: Some(Challenge::PairCode("1234-5678".into())),
                            expires_at: ts(1_800_000_600),
                        })),
                    }),
                    status_at: ts(1_800_000_010),
                },
                pb::ChannelInstance {
                    instance: "web".into(),
                    component: "web".into(),
                    runtime: pb::ChannelRuntime::BuiltIn.into(),
                    status: Some(ChannelStatus {
                        state: Some(State::Connected(Connected {})),
                    }),
                    status_at: ts(1_800_000_001),
                },
            ],
            plugins: vec![
                pb::PluginInstance {
                    instance: "libera".into(),
                    kind: pb::InstanceKind::Channel.into(),
                    component: "irc".into(),
                    location: "/opt/giles/plugins/giles-channel-irc".into(),
                    state: pb::PluginState::Running.into(),
                    version: Some("1.2.3".into()),
                    pid: Some(4242),
                    restarts: 2,
                    last_error: Some("health check timed out".into()),
                    started_at: ts(1_800_000_005),
                    sandbox: Some(pb::SandboxStatus {
                        enabled: true,
                        landlock_abi: Some(5),
                        missing: vec!["signal scoping (ABI 6)".into()],
                    }),
                    requested_grants: Some(SandboxGrants {
                        network_all: false,
                        network_ports: vec!["tcp:6697".into()],
                        read: vec!["/etc/ssl".into()],
                        write: vec![],
                    }),
                    channel_status: Some(ChannelStatus {
                        state: Some(State::Error("netsplit".into())),
                    }),
                    retry_at: None,
                },
                pb::PluginInstance {
                    instance: "github".into(),
                    kind: pb::InstanceKind::Toolset.into(),
                    location: "https://api.example.com/mcp/".into(),
                    ..pb::PluginInstance::default()
                },
            ],
        };
        let expected = "\
giles 0.1.0, running since 2027-01-15 08:00:00 UTC
Plugin sandbox: Landlock ABI 5 and seccomp filters

Channels:
  libera  irc (plugin): needs linking: enter the code 1234-5678 (until 2027-01-15 08:10:00 UTC) \
(since 2027-01-15 08:00:10 UTC)
  web     web (built in): connected (since 2027-01-15 08:00:01 UTC)

Plugins:
  libera (channel, irc)
    state:     running, pid 4242, since 2027-01-15 08:00:05 UTC; 2 restarts
    error:     health check timed out
    channel:   error: netsplit
    location:  /opt/giles/plugins/giles-channel-irc
    version:   1.2.3
    sandbox:   on, Landlock ABI 5
    lacking:   signal scoping (ABI 6)
    requests:  network: tcp:6697; read: /etc/ssl
  github (toolset)
    state:     unknown (plugin supervision is not available)
    location:  https://api.example.com/mcp/
    sandbox:   none (remote)
";
        assert_eq!(format_status(&status, &TimeZone::UTC), expected);
    }

    #[test]
    fn an_empty_daemon_and_a_missing_sandbox_are_spelled_out() {
        let status = pb::StatusResponse {
            version: "0.1.0".into(),
            sandbox_host: Some(pb::SandboxHost {
                available: false,
                description: "Landlock is not available".into(),
                landlock_abi: None,
            }),
            config_error: Some("giles.toml:1:1: bad".into()),
            ..pb::StatusResponse::default()
        };
        let text = format_status(&status, &TimeZone::UTC);
        assert_eq!(
            text,
            "giles 0.1.0\nPlugin sandbox: unavailable: Landlock is not available\n\
             Configuration error: giles.toml:1:1: bad\n\nChannels: none\n\nPlugins: none\n"
        );
    }

    #[test]
    fn plugin_states_and_grants_in_words() {
        let backoff = pb::PluginInstance {
            instance: "mail".into(),
            kind: pb::InstanceKind::Channel.into(),
            state: pb::PluginState::Backoff.into(),
            retry_at: ts(1_800_000_000),
            sandbox: Some(pb::SandboxStatus::default()),
            ..pb::PluginInstance::default()
        };
        let mut out = String::new();
        format_plugin(&mut out, &backoff, &TimeZone::UTC);
        assert!(
            out.contains("state:     down, retrying at 2027-01-15 08:00:00 UTC"),
            "{out}"
        );
        assert!(out.contains("channel:   no status yet"), "{out}");
        assert!(out.contains("sandbox:   off (unsandboxed)"), "{out}");

        assert_eq!(
            requested_grants(&SandboxGrants {
                network_all: true,
                network_ports: vec!["tcp:1".into()],
                read: vec![],
                write: vec!["/w".into()],
            }),
            "network: all; write: /w"
        );
        assert_eq!(requested_grants(&SandboxGrants::default()), "nothing");
    }
}

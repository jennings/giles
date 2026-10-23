//! The core's plugin host launching the echo plugin end to end (unsandboxed): discovery, the
//! handshake, `Describe` and `Configure` with resolved secrets, the namespace, the stop sequence,
//! and stdin end-of-file. The sandboxed launch is in `host_sandbox.rs`; the plugin as a
//! registered channel of a running core is in `remote_channel.rs`.

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use giles_core::plugin_host::{LaunchError, STOP_GRACE};
use giles_proto::plugin::v1::PluginKind;
use tonic_health::pb::HealthCheckRequest;
use tonic_health::pb::health_check_response::ServingStatus;

use self::support::{ECHO, Home, WAIT};

/// No trampoline: these instances are unsandboxed, which bypasses it.
fn no_trampoline() -> PathBuf {
    PathBuf::from("/nonexistent/giles")
}

#[tokio::test]
async fn launches_describes_and_configures_with_resolved_secrets() {
    let home = Home::new(
        "[channel.echo]\nunsandboxed = true\nconfig = { namespace = { env = \"ECHO_NS\" } }\n",
        &[("ECHO_NS", "echo:from-a-secret")],
    );
    home.install_echo_as("echo");
    let mut plugin = home
        .launcher(no_trampoline())
        .launch_channel(&home.channel("echo"))
        .await
        .unwrap();

    // `Configure` got the resolved value, and the namespace is the plugin's own.
    assert_eq!(plugin.identity_namespace, "echo:from-a-secret");
    assert_eq!(plugin.describe.kind(), PluginKind::Channel);
    assert_eq!(plugin.describe.name, "echo");
    assert_eq!(plugin.describe.version, env!("CARGO_PKG_VERSION"));
    assert!(!plugin.sandboxed);
    let binary = PathBuf::from(ECHO).canonicalize().unwrap();
    assert_eq!(plugin.binary, binary);

    // The per-instance data directory, private, with its TMPDIR.
    let data = home.root.plugin_data_dir("echo").unwrap();
    assert_eq!(
        fs::metadata(&data).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(data.join("tmp").is_dir());
    assert!(
        data.join("giles-sdk").is_dir(),
        "the plugin used its data directory"
    );

    // The pid file records the process and its binary.
    let pid = plugin.process.pid();
    let pid_file = home.root.pid_file("echo").unwrap();
    assert_eq!(
        fs::read_to_string(&pid_file).unwrap(),
        format!("{pid}\n{}\n", binary.display())
    );

    // Calls carry the token: health answers, and the channel serves after `Configure`.
    let status = plugin
        .connection
        .health()
        .check(HealthCheckRequest {
            service: "giles.channel.v1.ChannelService".into(),
        })
        .await
        .unwrap()
        .into_inner()
        .status();
    assert_eq!(status, ServingStatus::Serving);

    // `Shutdown` stops it cleanly within the grace period.
    assert!(plugin.process.exit_status().is_none());
    let status = tokio::time::timeout(WAIT, plugin.stop(STOP_GRACE))
        .await
        .unwrap()
        .unwrap();
    assert!(status.success(), "{status}");
    assert!(!pid_file.exists());
}

#[tokio::test]
async fn a_rejected_config_is_scrubbed_and_not_retryable() {
    let home = Home::new(
        "[channel.echo]\nunsandboxed = true\nconfig = { namespace = { env = \"ECHO_NS\" } }\n",
        &[("ECHO_NS", "not-echo-secret-ns")],
    );
    home.install_echo_as("echo");
    let error = home
        .launcher(no_trampoline())
        .launch_channel(&home.channel("echo"))
        .await
        .unwrap_err();
    let LaunchError::Rpc { call, message, .. } = &error else {
        panic!("{error}");
    };
    assert_eq!(*call, "Configure");
    assert!(message.contains("<redacted>"), "{message}");
    assert!(!error.to_string().contains("not-echo-secret-ns"), "{error}");
    assert!(!error.is_retryable());
    assert!(!home.root.pid_file("echo").unwrap().exists());
}

#[tokio::test]
async fn an_unknown_config_key_fails_configure() {
    let home = Home::new(
        "[channel.echo]\nunsandboxed = true\nconfig = { bogus = 1 }\n",
        &[],
    );
    home.install_echo_as("echo");
    let error = home
        .launcher(no_trampoline())
        .launch_channel(&home.channel("echo"))
        .await
        .unwrap_err();
    assert!(
        matches!(
            &error,
            LaunchError::Rpc { call: "Configure", code: tonic::Code::InvalidArgument, message }
                if message.contains("bogus")
        ),
        "{error}"
    );
    assert!(!error.is_retryable());
}

#[tokio::test]
async fn a_binary_installed_under_another_name_is_refused() {
    let home = Home::new("[channel.other]\nunsandboxed = true\n", &[]);
    home.install_echo_as("other");
    let error = home
        .launcher(no_trampoline())
        .launch_channel(&home.channel("other"))
        .await
        .unwrap_err();
    assert!(matches!(error, LaunchError::Mismatch { .. }), "{error}");
    assert_eq!(
        error.to_string(),
        "the plugin doesn't match its binary giles-channel-other: it is named \"echo\", \
         expected \"other\""
    );
    assert!(!error.is_retryable());
}

#[tokio::test]
async fn closing_stdin_makes_the_plugin_exit() {
    let home = Home::new("[channel.echo]\nunsandboxed = true\n", &[]);
    home.install_echo_as("echo");
    let mut plugin = home
        .launcher(no_trampoline())
        .launch_channel(&home.channel("echo"))
        .await
        .unwrap();
    // What the plugin sees when the core dies.
    plugin.process.close_stdin();
    let status = tokio::time::timeout(WAIT, plugin.process.wait())
        .await
        .expect("the plugin exits on stdin end-of-file")
        .unwrap();
    assert!(status.success(), "{status}");
}

//! A sandboxed launch of the echo plugin through the sandbox trampoline.
//!
//! The trampoline is this test binary itself: run as `<test> sandbox-exec <spec> -- <command>`, it
//! does what `giles sandbox-exec` does (`giles_core::sandbox::exec`, single-threaded, before any
//! runtime starts), because Cargo gives tests here the echo binary's path but not the `giles`
//! binary's. Hence no libtest harness.
//!
//! Where the host can sandbox (macOS; Linux with Landlock) the launch succeeds. On a Linux kernel
//! without Landlock it fails cleanly: a non-retryable sandbox error carrying the trampoline's own
//! message, and nothing runs.

mod support;

use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;

use giles_core::config::Kind;
use giles_core::plugin_host::{LaunchError, STOP_GRACE};
use giles_core::sandbox::{self, SandboxError, Spec};
use tonic_health::pb::HealthCheckRequest;
use tonic_health::pb::health_check_response::ServingStatus;

use self::support::{Home, WAIT};

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    if args.get(1).is_some_and(|arg| arg == "sandbox-exec") {
        trampoline(&args[2..]);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    print!("test sandboxed_echo_launch ... ");
    runtime.block_on(sandboxed_echo_launch());
    println!("ok");
    ExitCode::SUCCESS
}

/// `giles sandbox-exec <spec> -- <command>`.
fn trampoline(args: &[OsString]) -> ! {
    assert_eq!(args.get(1).map(OsString::as_os_str), Some("--".as_ref()));
    let err = match Spec::load(Path::new(&args[0])) {
        Ok(spec) => sandbox::exec(&spec, &args[2..]),
        Err(err) => SandboxError::from(err),
    };
    eprintln!("Error: {err}");
    std::process::exit(err.exit_code().into())
}

async fn sandboxed_echo_launch() {
    let home = Home::new("[channel.echo]\n", &[]);
    home.install_echo_as("echo");
    let launcher = home.launcher(std::env::current_exe().unwrap());
    let result = launcher.launch_channel(&home.channel("echo")).await;

    if sandbox::check_host().is_ok() {
        let plugin = result.unwrap_or_else(|e| panic!("{e}\nstderr: {:#?}", e.stderr()));
        assert!(plugin.sandboxed);
        assert_eq!(plugin.identity_namespace, "echo");
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
        let spec =
            Spec::load(&home.root.sandbox_spec_file(Kind::Channel, "echo").unwrap()).unwrap();
        assert!(!spec.unsandboxed);
        assert_eq!(
            spec.data_dir,
            home.root.plugin_data_dir(Kind::Channel, "echo").unwrap()
        );
        let status = tokio::time::timeout(WAIT, plugin.stop(STOP_GRACE))
            .await
            .unwrap()
            .unwrap();
        assert!(status.success(), "{status}");
    } else {
        let error = match result {
            Ok(plugin) => panic!("launched {} without a usable sandbox", plugin.instance),
            Err(error) => error,
        };
        assert!(matches!(error, LaunchError::Sandbox { .. }), "{error}");
        assert!(!error.is_retryable());
        let message = error.to_string();
        assert!(
            message.contains("Error: cannot sandbox the command on this host, so it was not run"),
            "{message}"
        );
        assert!(!home.root.pid_file(Kind::Channel, "echo").unwrap().exists());
    }
}

//! A sandboxed launch of the environment echo plugin through the sandbox trampoline, with a `unix`
//! grant and the default network (off) for an environment.
//!
//! The trampoline is this test binary itself: run as `<test> sandbox-exec <spec> -- <command>`, it
//! does what `giles sandbox-exec` does (`giles_core::sandbox::exec`, single-threaded, before any
//! runtime starts), as for the channel echo plugin. Hence no libtest harness.
//!
//! Where the host can sandbox the launch succeeds and the spec carries the grants. On a Linux
//! kernel without Landlock it fails cleanly with a non-retryable sandbox error.

mod support;

use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;

use giles_core::config::Kind;
use giles_core::plugin_host::{LaunchError, STOP_GRACE};
use giles_core::sandbox::{self, Network, SandboxError, Spec};
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
    print!("test sandboxed_environment_launch ... ");
    runtime.block_on(sandboxed_environment_launch());
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

async fn sandboxed_environment_launch() {
    let home = Home::new(
        "[environment.box]\ncomponent = \"echo\"\nsandbox = { unix = [\"/var/run/docker.sock\"] }\n",
    );
    home.install_echo_as("echo");
    let launcher = home.launcher_with(std::env::current_exe().unwrap());
    let instance = home.config().environments["box"].clone();
    let result = launcher.launch_environment(&instance).await;

    if sandbox::check_host().is_ok() {
        let plugin = result.unwrap_or_else(|e| panic!("{e}\nstderr: {:#?}", e.stderr()));
        assert!(plugin.sandboxed);
        assert_eq!(plugin.kind, Kind::Environment);
        let status = plugin
            .connection
            .health()
            .check(HealthCheckRequest {
                service: "giles.environment.v1.EnvironmentService".into(),
            })
            .await
            .unwrap()
            .into_inner()
            .status();
        assert_eq!(status, ServingStatus::Serving);
        let spec = Spec::load(
            &home
                .root
                .sandbox_spec_file(Kind::Environment, "box")
                .unwrap(),
        )
        .unwrap();
        assert!(!spec.unsandboxed);
        assert_eq!(spec.unix, [Path::new("/var/run/docker.sock")]);
        assert_eq!(spec.network, Network::Off);
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
    }
}

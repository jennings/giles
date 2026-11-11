//! Shared setup for the integration tests: finding a Docker API socket, building the Linux helper
//! for the engine's architecture, and cleaning up what a test created.

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use bollard::Docker;
use bollard::query_parameters::CreateImageOptionsBuilder;
use futures::StreamExt;
use giles_environment_docker::{Arch, Config, DockerEnvironment, ExecEvent, ExecRequest};

/// The image most tests run in. The harness pulls it up front, so no test pulls it (and no test's
/// delete removes it).
pub const IMAGE: &str = "alpine:3.20";

/// An engine to test against, with a helper built for it.
pub struct Harness {
    socket: PathBuf,
    helper_dir: PathBuf,
}

/// The harness, or `None` (after printing why) when there is no engine to test against.
///
/// `GILES_REQUIRE_DOCKER=1` turns "no engine" into a failure, so CI can't skip silently.
/// `GILES_HELPER_DIR` names a directory that already holds `giles-workspace-helper-<arch>`;
/// otherwise the helper is built with `cargo build --target <arch>-unknown-linux-musl`.
pub fn harness() -> Option<&'static Harness> {
    static HARNESS: OnceLock<Result<Harness, String>> = OnceLock::new();
    // Setup runs its own runtime, so it can't run on a thread that drives a test's.
    let init = || {
        std::thread::spawn(setup)
            .join()
            .unwrap_or_else(|_| Err("the harness setup panicked".to_owned()))
    };
    match HARNESS.get_or_init(init) {
        Ok(harness) => Some(harness),
        Err(reason) => {
            let required = std::env::var("GILES_REQUIRE_DOCKER").is_ok_and(|v| !v.is_empty());
            assert!(!required, "GILES_REQUIRE_DOCKER is set, but: {reason}");
            eprintln!("skipping: {reason}");
            None
        }
    }
}

fn setup() -> Result<Harness, String> {
    let socket = socket_path();
    if !socket.exists() {
        return Err(format!("no Docker API socket at {}", socket.display()));
    }
    // Everything async runs on a throwaway runtime, since the harness is built outside any test.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let socket_text = socket.to_string_lossy().into_owned();
    let docker = Docker::connect_with_unix(&socket_text, 30, bollard::API_DEFAULT_VERSION)
        .map_err(|e| format!("can't connect to {socket_text}: {e}"))?;
    let info = runtime
        .block_on(async {
            docker.ping().await?;
            docker.info().await
        })
        .map_err(|e| format!("the Docker API at {socket_text} didn't answer: {e}"))?;
    let arch = Arch::from_engine(info.architecture.as_deref().unwrap_or_default())
        .map_err(|e| e.to_string())?;

    let helper_dir = match std::env::var_os("GILES_HELPER_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => build_helper(arch)?,
    };
    runtime
        .block_on(pull(&docker, IMAGE))
        .map_err(|e| format!("pulling {IMAGE}: {e}"))?;
    Ok(Harness { socket, helper_dir })
}

fn socket_path() -> PathBuf {
    match std::env::var("DOCKER_HOST") {
        Ok(host) if host.starts_with("unix://") => PathBuf::from(&host["unix://".len()..]),
        _ => PathBuf::from("/var/run/docker.sock"),
    }
}

/// Cross-builds the helper for `arch` and copies it to a directory under its plugin file name.
fn build_helper(arch: Arch) -> Result<PathBuf, String> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args([
            "build",
            "--package",
            "giles-workspace-helper",
            "--target",
            arch.musl_target(),
            "--message-format=json-render-diagnostics",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .map_err(|e| format!("couldn't run cargo to build the helper: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "building the helper for {} failed (is the target installed? `rustup target add {}`):\n{}",
            arch.as_str(),
            arch.musl_target(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let executable = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|m| {
            m["reason"] == "compiler-artifact" && m["target"]["name"] == "giles-workspace-helper"
        })
        .find_map(|m| m["executable"].as_str().map(PathBuf::from))
        .ok_or("cargo didn't report the helper executable")?;
    let dir = std::env::temp_dir().join(format!("giles-helper-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::copy(&executable, dir.join(arch.file_name())).map_err(|e| e.to_string())?;
    Ok(dir)
}

/// Pulls an image if the engine doesn't have it.
async fn pull(docker: &Docker, image: &str) -> Result<(), bollard::errors::Error> {
    if docker.inspect_image(image).await.is_ok() {
        return Ok(());
    }
    let (name, tag) = image.split_once(':').unwrap_or((image, "latest"));
    let options = CreateImageOptionsBuilder::default()
        .from_image(name)
        .tag(tag)
        .build();
    let mut stream = docker.create_image(Some(options), None, None);
    while let Some(item) = stream.next().await {
        item?;
    }
    Ok(())
}

impl Harness {
    pub fn docker(&self) -> Docker {
        Docker::connect_with_unix(
            &self.socket.to_string_lossy(),
            60,
            bollard::API_DEFAULT_VERSION,
        )
        .unwrap()
    }

    /// A config for this engine.
    pub fn config(&self) -> Config {
        Config {
            socket: Some(self.socket.clone()),
            helper_dir: Some(self.helper_dir.clone()),
            ..Config::default()
        }
    }

    /// Connects an instance named `name` plus a random suffix, so tests never see each other's
    /// workspaces.
    pub async fn scope(&self, name: &str, config: Config) -> Scope {
        let instance = format!("{name}-{}", uuid_suffix());
        let env = DockerEnvironment::connect(config, &instance).await.unwrap();
        Scope {
            env,
            docker: self.docker(),
            instance,
        }
    }
}

fn uuid_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    format!("{:x}{:x}", std::process::id(), nanos)
}

/// An environment instance that deletes its workspaces when the test ends, even by panic.
pub struct Scope {
    pub env: DockerEnvironment,
    pub docker: Docker,
    /// The instance name, which is the `giles.instance` label of its workspaces.
    pub instance: String,
}

impl Drop for Scope {
    fn drop(&mut self) {
        let env = self.env.clone();
        // A runtime of its own: this may run while the test's runtime is shutting down.
        let _ = std::thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(async {
                if let Ok(list) = env.list_workspaces().await {
                    for workspace in list.workspaces {
                        let _ = env.delete_workspace(&workspace.id).await;
                    }
                }
            });
        })
        .join();
    }
}

/// What a command printed and how it ended.
#[derive(Debug, Default)]
pub struct Ran {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
    pub timed_out: bool,
    pub dropped: u64,
}

impl Scope {
    /// Creates a workspace in `IMAGE` and returns its id.
    pub async fn workspace(&self, id: &str) -> String {
        self.env
            .create_workspace(id, serde_json::json!({ "image": IMAGE }))
            .await
            .unwrap();
        id.to_owned()
    }

    /// Runs a command with a generous timeout and output limit.
    pub async fn run(&self, id: &str, command: &str) -> Ran {
        self.run_with(id, command, Duration::from_secs(60), 1 << 20)
            .await
    }

    pub async fn run_with(&self, id: &str, command: &str, timeout: Duration, limit: u64) -> Ran {
        let mut events = self
            .env
            .exec(
                id,
                ExecRequest {
                    command: command.to_owned(),
                    timeout,
                    output_limit: limit,
                },
            )
            .await
            .unwrap();
        let mut ran = Ran::default();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exited = false;
        while let Some(event) = events.next().await {
            assert!(!exited, "an event came after the exit");
            match event.unwrap() {
                ExecEvent::Stdout(bytes) => stdout.extend(bytes),
                ExecEvent::Stderr(bytes) => stderr.extend(bytes),
                ExecEvent::Exit(exit) => {
                    exited = true;
                    ran.code = exit.code;
                    ran.timed_out = exit.timed_out;
                    ran.dropped = exit.dropped_bytes;
                }
            }
        }
        assert!(exited, "the stream ended without an exit event");
        ran.stdout = String::from_utf8_lossy(&stdout).into_owned();
        ran.stderr = String::from_utf8_lossy(&stderr).into_owned();
        ran
    }
}

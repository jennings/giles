//! What the SSH integration tests share: the core's side of a relay (keys, agent, known hosts in a
//! real store and secret store), an `sshd` container to log in to, and the helpers that install an
//! OpenSSH client in a workspace and skip when the network can't.

use std::sync::Arc;
use std::time::Duration;

use bollard::Docker;
use bollard::models::{ContainerCreateBody, HostConfig, PortBinding};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, LogsOptionsBuilder, RemoveContainerOptionsBuilder,
};
use doozers_core::paths::DataRoot;
use doozers_core::secrets::{KeyFile, LocalSecrets, SecretPath, SecretProvider};
use doozers_core::sessions::{Clock, SystemClock};
use doozers_core::ssh::{Agent, Caller, KeyType, KnownHosts, SshKey, SshKeys};
use doozers_environment_docker::SSH_AGENT_SOCKET;
use doozers_store::Store;
use doozers_types::{AgentReply, Environment, KnownHostsReply, RelayReply, RelayRequest, UserId};
use futures::StreamExt;

use super::{IMAGE, Scope, wid};

/// Whether a step that needs the network must work, instead of skipping the test.
pub fn network_required() -> bool {
    std::env::var("DOOZERS_REQUIRE_DOCKER").is_ok_and(|value| !value.is_empty())
}

/// Ends the test for a network step that failed: a skip with the reason, or a failure when the
/// network is required.
pub fn network_failed(what: &str, detail: &str) {
    assert!(
        !network_required(),
        "DOOZERS_REQUIRE_DOCKER is set, but {what} failed: {detail}"
    );
    eprintln!("skipping: {what} failed (no network?): {detail}");
}

/// The core's side: keys in a secret store, and the agent answering one workspace's relay.
pub struct Core {
    _dir: tempfile::TempDir,
    pub owner: UserId,
    pub keys: SshKeys,
    pub known_hosts: KnownHosts,
    secrets: Arc<LocalSecrets>,
    relays: Vec<tokio::task::JoinHandle<()>>,
}

impl Core {
    /// A secret store and keys service in a temporary directory.
    pub async fn new() -> (Self, Arc<dyn Clock>) {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path(), dir.path()).unwrap();
        let store = Store::open(dir.path().join("doozers.db")).await.unwrap();
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let secrets = Arc::new(
            LocalSecrets::open(
                store.clone(),
                &KeyFile::new(&root, &root.secret_key_file()),
                clock.clone(),
            )
            .await
            .unwrap(),
        );
        let keys = SshKeys::new(
            store.clone(),
            secrets.clone(),
            clock.clone(),
            doozers_core::users::UserList::default(),
        );
        let known_hosts = KnownHosts::new(store, clock.clone());
        let core = Self {
            _dir: dir,
            owner: UserId::new("owner").unwrap(),
            keys,
            known_hosts,
            secrets,
            relays: Vec::new(),
        };
        (core, clock)
    }

    pub async fn generate(&self, kind: KeyType, label: &str) -> SshKey {
        self.keys
            .generate(&self.owner, kind, Some(label))
            .await
            .unwrap()
    }

    /// Opens the relay of workspace `id` and answers it as the core's relay task does: the agent
    /// for `user`'s keys, and known-hosts lookups from this core's known hosts, with first uses
    /// verified by a [`WorkspaceScanner`] in that workspace (the workspace needs `ssh-keyscan`).
    pub async fn serve(
        &mut self,
        scope: &Scope,
        clock: Arc<dyn Clock>,
        id: &str,
        user: Option<UserId>,
    ) {
        let scanner: Arc<dyn Environment> = Arc::new(WorkspaceScanner {
            env: scope.env.clone(),
        });
        self.serve_scanned(scope, clock, id, user, scanner).await;
    }

    /// Like [`serve`](Self::serve), with `scanner` doing the scans.
    pub async fn serve_scanned(
        &mut self,
        scope: &Scope,
        clock: Arc<dyn Clock>,
        id: &str,
        user: Option<UserId>,
        scanner: Arc<dyn Environment>,
    ) {
        let mut agent = Agent::new(self.keys.clone(), clock, wid(id), user.clone());
        let caller = Caller::new(
            scope.instance.clone(),
            wid(id),
            Some("jeeves".to_owned()),
            user,
        );
        let known_hosts = self.known_hosts.clone();
        let (replies, incoming) = futures::channel::mpsc::unbounded::<RelayReply>();
        let mut requests = scope
            .env
            .relay(&wid(id), incoming.boxed())
            .await
            .expect("opening the relay");
        self.relays.push(tokio::spawn(async move {
            while let Some(Ok(request)) = requests.next().await {
                let reply = match request {
                    RelayRequest::Agent(request) => {
                        let message = agent.handle(request.connection, &request.message).await;
                        RelayReply::Agent(AgentReply {
                            connection: request.connection,
                            message,
                        })
                    }
                    RelayRequest::AgentClosed(closed) => {
                        agent.close(closed.connection);
                        continue;
                    }
                    RelayRequest::KnownHosts(request) => {
                        let keys = known_hosts
                            .lookup(scanner.as_ref(), &caller, &request)
                            .await;
                        RelayReply::KnownHosts(KnownHostsReply {
                            id: request.id,
                            keys,
                        })
                    }
                    _ => continue,
                };
                if replies.unbounded_send(reply).is_err() {
                    break;
                }
            }
        }));
    }

    /// The private key text of `key`, as the secret store holds it.
    pub async fn private_key(&self, key: &SshKey) -> String {
        let path = SecretPath::new(format!("users/{}/ssh-keys/{}", key.user, key.id)).unwrap();
        self.secrets
            .get(&path)
            .await
            .unwrap()
            .expect("the key's secret")
            .expose()
            .to_owned()
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        for relay in &self.relays {
            relay.abort();
        }
    }
}

/// Scans from inside a workspace with `ssh-keyscan`, standing in for a plugin that shares the
/// workspace's network. The real plugin scans from the engine host, which reaches a container's
/// address on Linux but not through a VM (Docker Desktop, Podman machine); the plugin's own scan is
/// tested against a published port instead.
pub struct WorkspaceScanner {
    pub env: doozers_environment_docker::DockerEnvironment,
}

#[async_trait::async_trait]
impl Environment for WorkspaceScanner {
    async fn scan_host_key(
        &self,
        id: &doozers_types::WorkspaceId,
        host: &str,
        port: u16,
        key_type: &str,
    ) -> doozers_types::Result<Vec<u8>> {
        use base64::Engine as _;
        let kind = match key_type {
            "ssh-ed25519" => "ed25519",
            "ssh-rsa" => "rsa",
            _ => "ecdsa",
        };
        let mut events = self
            .env
            .exec(
                id,
                doozers_types::ExecRequest {
                    command: format!("ssh-keyscan -p {port} -t {kind} {host} 2>/dev/null"),
                    timeout: Duration::from_secs(30),
                    output_limit: 1 << 16,
                },
            )
            .await?;
        let mut out = Vec::new();
        while let Some(event) = events.next().await {
            if let doozers_types::ExecEvent::Stdout(bytes) = event? {
                out.extend(bytes);
            }
        }
        String::from_utf8_lossy(&out)
            .lines()
            .filter(|line| !line.starts_with('#'))
            .find_map(|line| {
                let mut fields = line.split_whitespace().skip(1);
                (fields.next()? == key_type)
                    .then(|| fields.next())
                    .flatten()
                    .and_then(|blob| base64::engine::general_purpose::STANDARD.decode(blob).ok())
            })
            .ok_or_else(|| doozers_types::Error::unavailable("ssh-keyscan found no key"))
    }

    async fn info(&self) -> doozers_types::Result<doozers_types::EnvironmentInfo> {
        unreachable!("the scanner only scans")
    }
    async fn create_workspace(
        &self,
        _: &doozers_types::WorkspaceId,
        _: serde_json::Value,
    ) -> doozers_types::Result<doozers_types::Workspace> {
        unreachable!("the scanner only scans")
    }
    async fn delete_workspace(&self, _: &doozers_types::WorkspaceId) -> doozers_types::Result<()> {
        unreachable!("the scanner only scans")
    }
    async fn start_workspace(
        &self,
        _: &doozers_types::WorkspaceId,
    ) -> doozers_types::Result<doozers_types::StartOutcome> {
        unreachable!("the scanner only scans")
    }
    async fn stop_workspace(&self, _: &doozers_types::WorkspaceId) -> doozers_types::Result<()> {
        unreachable!("the scanner only scans")
    }
    async fn list_workspaces(&self) -> doozers_types::Result<doozers_types::WorkspaceList> {
        unreachable!("the scanner only scans")
    }
    async fn exec(
        &self,
        id: &doozers_types::WorkspaceId,
        request: doozers_types::ExecRequest,
    ) -> doozers_types::Result<
        futures::stream::BoxStream<'static, doozers_types::Result<doozers_types::ExecEvent>>,
    > {
        self.env.exec(id, request).await
    }
    async fn read_file(
        &self,
        _: &doozers_types::WorkspaceId,
        _: &str,
        _: u64,
    ) -> doozers_types::Result<doozers_types::ReadFile> {
        unreachable!("the scanner only scans")
    }
    async fn write_file(
        &self,
        _: &doozers_types::WorkspaceId,
        _: &str,
        _: Vec<u8>,
    ) -> doozers_types::Result<()> {
        unreachable!("the scanner only scans")
    }
    async fn find(
        &self,
        _: &doozers_types::WorkspaceId,
        _: doozers_types::FindRequest,
    ) -> doozers_types::Result<doozers_types::FindResult> {
        unreachable!("the scanner only scans")
    }
    async fn grep(
        &self,
        _: &doozers_types::WorkspaceId,
        _: doozers_types::GrepRequest,
    ) -> doozers_types::Result<doozers_types::GrepResult> {
        unreachable!("the scanner only scans")
    }
}

/// A workspace with an OpenSSH client with `packages` installed, or `None` after skipping.
pub async fn workspace_with(scope: &Scope, id: &str, packages: &str) -> Option<String> {
    let id = scope.workspace(id).await;
    let installed = scope
        .run(&id, &format!("apk add --no-cache {packages} 2>&1"))
        .await;
    if installed.code != 0 {
        network_failed(
            &format!("installing {packages} in the workspace"),
            installed.stdout.trim(),
        );
        return None;
    }
    Some(id)
}

/// Waits until the helper's socket is there.
pub async fn wait_for_socket(scope: &Scope, id: &str) {
    let probe = format!("test -S {SSH_AGENT_SOCKET}");
    for _ in 0..100 {
        if scope.run(id, &probe).await.code == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the agent socket never appeared");
}

/// A container running `sshd` that accepts one key for the user `tester`. Removed when dropped.
pub struct Sshd {
    docker: Docker,
    name: String,
    /// Its address on the engine's network: what a workspace connects to.
    pub address: String,
    /// The port on the engine host's loopback that reaches it: what the plugin's own network sees.
    pub published_port: u16,
}

impl Sshd {
    /// Starts it and waits until it listens; `Err` says what went wrong (an install that
    /// failed, usually for lack of network).
    pub async fn start(docker: Docker, name: &str, authorized_key: &str) -> Result<Self, String> {
        let script = format!(
            "set -e\n\
             apk add --no-cache openssh >/dev/null\n\
             ssh-keygen -A >/dev/null\n\
             adduser -D tester\n\
             echo \"tester:$(head -c 18 /dev/urandom | base64)\" | chpasswd\n\
             mkdir -p /home/tester/.ssh\n\
             echo '{authorized_key}' > /home/tester/.ssh/authorized_keys\n\
             chown -R tester:tester /home/tester/.ssh\n\
             chmod 700 /home/tester/.ssh\n\
             chmod 600 /home/tester/.ssh/authorized_keys\n\
             exec /usr/sbin/sshd -D -e -p 2222 -o PasswordAuthentication=no \
                 -o KbdInteractiveAuthentication=no\n"
        );
        let body = ContainerCreateBody {
            image: Some(IMAGE.to_owned()),
            cmd: Some(vec!["sh".to_owned(), "-c".to_owned(), script]),
            exposed_ports: Some(vec!["2222/tcp".to_owned()]),
            host_config: Some(HostConfig {
                network_mode: Some("bridge".to_owned()),
                port_bindings: Some(
                    [(
                        "2222/tcp".to_owned(),
                        Some(vec![PortBinding {
                            host_ip: Some("127.0.0.1".to_owned()),
                            host_port: Some("0".to_owned()),
                        }]),
                    )]
                    .into(),
                ),
                ..Default::default()
            }),
            ..Default::default()
        };
        docker
            .create_container(
                Some(CreateContainerOptionsBuilder::default().name(name).build()),
                body,
            )
            .await
            .map_err(|e| format!("creating the sshd container: {e}"))?;
        let mut sshd = Self {
            docker,
            name: name.to_owned(),
            address: String::new(),
            published_port: 0,
        };
        sshd.docker
            .start_container(name, None)
            .await
            .map_err(|e| format!("starting the sshd container: {e}"))?;
        for _ in 0..180 {
            let inspected = sshd
                .docker
                .inspect_container(name, None)
                .await
                .map_err(|e| format!("inspecting the sshd container: {e}"))?;
            let running = inspected
                .state
                .as_ref()
                .and_then(|state| state.running)
                .unwrap_or(false);
            if !running {
                return Err(format!(
                    "the sshd container stopped: {}",
                    sshd.logs().await.trim()
                ));
            }
            if sshd.logs().await.contains("Server listening") {
                sshd.published_port = inspected
                    .network_settings
                    .as_ref()
                    .and_then(|settings| settings.ports.as_ref())
                    .and_then(|ports| ports.get("2222/tcp"))
                    .and_then(|bindings| bindings.as_ref())
                    .and_then(|bindings| bindings.iter().find_map(|b| b.host_port.as_deref()))
                    .and_then(|port| port.parse().ok())
                    .ok_or("the sshd container's port isn't published")?;
                sshd.address = inspected
                    .network_settings
                    .and_then(|settings| settings.networks)
                    .and_then(|networks| {
                        networks
                            .into_values()
                            .filter_map(|network| network.ip_address)
                            .find(|address| !address.is_empty())
                    })
                    .ok_or("the sshd container has no address")?;
                return Ok(sshd);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        Err(format!("sshd never listened: {}", sshd.logs().await.trim()))
    }

    /// Runs `command` in the container with `sh -c` and returns what it printed.
    pub async fn run(&self, command: &str) -> String {
        use bollard::exec::{CreateExecOptions, StartExecResults};
        let created = self
            .docker
            .create_exec(
                &self.name,
                CreateExecOptions {
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    cmd: Some(vec!["sh".to_owned(), "-c".to_owned(), command.to_owned()]),
                    ..Default::default()
                },
            )
            .await
            .expect("creating an exec in the sshd container");
        let StartExecResults::Attached { mut output, .. } = self
            .docker
            .start_exec(&created.id, None)
            .await
            .expect("starting an exec in the sshd container")
        else {
            panic!("the exec detached");
        };
        let mut text = String::new();
        while let Some(Ok(chunk)) = output.next().await {
            text.push_str(&chunk.to_string());
        }
        text
    }

    /// Replaces the host keys and has `sshd` load them, as a server that rotated its keys.
    pub async fn rotate_host_keys(&self) {
        let out = self
            .run("rm -f /etc/ssh/ssh_host_* && ssh-keygen -A >/dev/null && pkill -HUP -x sshd; echo done")
            .await;
        assert!(out.contains("done"), "{out}");
        // `sshd -D` re-executes itself on SIGHUP and listens again within moments.
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    pub async fn logs(&self) -> String {
        let options = LogsOptionsBuilder::default()
            .stdout(true)
            .stderr(true)
            .build();
        let mut stream = self.docker.logs(&self.name, Some(options));
        let mut text = String::new();
        while let Some(Ok(chunk)) = stream.next().await {
            text.push_str(&chunk.to_string());
        }
        text
    }
}

impl Drop for Sshd {
    fn drop(&mut self) {
        let docker = self.docker.clone();
        let name = self.name.clone();
        // A runtime of its own: this may run while the test's runtime is shutting down.
        let _ = std::thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(async {
                let options = RemoveContainerOptionsBuilder::default().force(true).build();
                let _ = docker.remove_container(&name, Some(options)).await;
            });
        })
        .join();
    }
}

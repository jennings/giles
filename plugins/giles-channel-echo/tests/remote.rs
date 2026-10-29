//! The echo plugin as a remote plugin (PLAN section 7.6) of a running core: the binary runs on
//! its own (`serve --listen 127.0.0.1:...`), and the core connects to it with a bearer token, OAuth
//! client credentials from a fake token endpoint, or TLS with a test certificate authority. A
//! wrong token, a refused token request, or an untrusted certificate leaves it backing off; an
//! unreachable plugin is retried until it comes up, and again after it goes away; stopping the
//! core only disconnects, and the next core gets the events that arrived in between; a restart
//! closes the old connection for good, even though its channel object is still referenced.

mod support;

use std::net::SocketAddr;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use giles_core::admin::PluginControl as _;
use giles_core::config::Kind;
use giles_core::plugin_host::PluginState;
use giles_plugin_sdk::giles_types::{ConversationId, OutboundMessage, SendTarget};
use giles_plugin_sdk::tls::rustls::RootCertStore;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpListener;
use tokio::process::{Child, Command};

use self::support::{Core, ECHO, Home, WAIT, owner_message, wait_until};

/// The echo binary running as a remote plugin.
struct Remote {
    child: Child,
    addr: SocketAddr,
}

impl Remote {
    /// Starts it listening on `listen` with data directory `data`, accepting `token` (any call
    /// without one), serving TLS with `tls`'s certificate if given.
    async fn start(listen: &str, data: &Path, token: Option<&str>, tls: Option<&Pki>) -> Self {
        let mut command = Command::new(ECHO);
        command
            .args(["serve", "--listen", listen, "--data"])
            .arg(data)
            .env_remove("GILES_CHANNEL_ECHO_TOKEN")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        match token {
            Some(token) => command.env("GILES_CHANNEL_ECHO_TOKEN", token),
            None => command.arg("--no-auth"),
        };
        if let Some(pki) = tls {
            command
                .arg("--tls-cert")
                .arg(pki.dir.path().join("chain.pem"))
                .arg("--tls-key")
                .arg(pki.dir.path().join("key.pem"));
        }
        let mut child = command.spawn().unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let line = tokio::time::timeout(WAIT, lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .expect("the listening line");
        let addr = line.strip_prefix("listening on ").unwrap().parse().unwrap();
        Self { child, addr }
    }

    fn alive(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    async fn kill(mut self) {
        self.child.kill().await.unwrap();
    }
}

/// A test certificate authority and a certificate for `localhost` it issued, as PEM files.
struct Pki {
    dir: tempfile::TempDir,
    roots: RootCertStore,
}

impl Pki {
    fn new() -> Self {
        use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair};
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = CertifiedIssuer::self_signed(ca_params, KeyPair::generate().unwrap()).unwrap();
        let key = KeyPair::generate().unwrap();
        let cert = CertificateParams::new(vec!["localhost".to_owned()])
            .unwrap()
            .signed_by(&key, &ca)
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("chain.pem"),
            format!("{}{}", cert.pem(), ca.pem()),
        )
        .unwrap();
        std::fs::write(dir.path().join("key.pem"), key.serialize_pem()).unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(ca.der().clone()).unwrap();
        Self { dir, roots }
    }
}

/// A fake OAuth token endpoint on `127.0.0.1` answering every request with `status` and the JSON
/// `body`. Returns its URL and the `authorization` header of each request.
async fn token_endpoint(
    status: &'static str,
    body: &'static str,
) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/token", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let requests = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let requests = Arc::clone(&requests);
            tokio::spawn(async move {
                // Read the head and the body (`content-length`), then answer and close.
                let mut request = Vec::new();
                let mut buf = [0; 4096];
                let head = loop {
                    let n = stream.read(&mut buf).await.unwrap();
                    assert!(n > 0, "the request ended early");
                    request.extend_from_slice(&buf[..n]);
                    let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let head = String::from_utf8_lossy(&request[..end]).into_owned();
                    let length: usize =
                        header(&head, "content-length").map_or(0, |value| value.parse().unwrap());
                    if request.len() >= end + 4 + length {
                        break head;
                    }
                };
                requests
                    .lock()
                    .unwrap()
                    .push(header(&head, "authorization").unwrap_or_default());
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                let _ = stream.shutdown().await;
            });
        }
    });
    (url, seen)
}

/// The value of header `name` in an HTTP/1.1 request head.
fn header(head: &str, name: &str) -> Option<String> {
    head.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name)
            .then(|| value.trim().to_owned())
    })
}

/// The echo instance's state.
fn state(core: &Core) -> PluginState {
    core.host.status("echo").unwrap().state
}

fn last_error(core: &Core) -> String {
    core.host
        .status("echo")
        .unwrap()
        .last_error
        .unwrap_or_default()
}

/// Waits until the echo instance is running.
async fn running(core: &Core) {
    wait_until(|| matches!(state(core), PluginState::Running { .. }).then_some(())).await;
}

/// Waits until the echo instance is backing off after a failure, and returns the failure.
async fn backing_off(core: &Core) -> String {
    wait_until(|| matches!(state(core), PluginState::BackingOff { .. }).then(|| last_error(core)))
        .await
}

/// Sends a message through the plugin and waits for the turn (the `count`th of this core) and
/// its reply.
async fn round_trip(home: &Home, core: &Core, id: &str, count: usize) {
    home.inject("echo", id, &owner_message(id, id, &[]));
    let messages = core.turns.wait_for(count).await;
    assert_eq!(messages[count - 1].text, id);
    wait_until(|| {
        home.lines("echo", "sent.jsonl")
            .iter()
            .any(|line| line["message"]["markdown"] == format!("echo: {id}"))
            .then_some(())
    })
    .await;
}

#[tokio::test]
async fn a_remote_plugin_carries_messages_and_a_stopping_core_only_disconnects() {
    let mut home = Home::serving("");
    let data = home.data_dir("echo");
    let mut plugin = Remote::start("127.0.0.1:0", &data, Some("s3cret-token"), None).await;
    home.add_channels(&format!(
        "[channel.echo]\naddress = \"http://{}\"\nauth = {{ bearer = {{ env = \"ECHO_TOKEN\" }} }}",
        plugin.addr
    ));
    home.set_env("ECHO_TOKEN", "s3cret-token");

    let core = home.start_core().await;
    let status = core.host.status("echo").unwrap();
    assert!(
        matches!(status.state, PluginState::Running { pid: None, .. }),
        "{status:?}"
    );
    assert_eq!(status.binary, None);
    assert_eq!(status.describe.unwrap().version, env!("CARGO_PKG_VERSION"));
    round_trip(&home, &core, "first", 1).await;

    // Stopping the core disconnects; the plugin keeps running.
    core.stop().await;
    assert!(plugin.alive());

    // A message arriving while no core is connected waits in the plugin's spool.
    home.inject("echo", "between", &owner_message("between", "between", &[]));
    wait_until(|| (!data.join("inject/between.json").exists()).then_some(())).await;

    // The next core connects (configuring the plugin the same way again) and gets it.
    let core = home.start_core().await;
    assert!(matches!(state(&core), PluginState::Running { .. }));
    let messages = core.turns.wait_for(1).await;
    assert_eq!(messages[0].text, "between");
    round_trip(&home, &core, "second", 2).await;
    core.stop().await;
    assert!(plugin.alive());
    plugin.kill().await;
}

#[tokio::test]
async fn a_restart_closes_the_old_connection_for_good() {
    let home = Home::serving("");
    let plugin = Remote::start("127.0.0.1:0", &home.data_dir("echo"), None, None).await;
    home.add_channels(&format!(
        "[channel.echo]\naddress = \"http://{}\"",
        plugin.addr
    ));
    let core = home.start_core().await;
    round_trip(&home, &core, "before", 1).await;

    // Someone (a session, the outbox) still holds the first connection's channel.
    let old = core.runtime.registry.channel("echo").unwrap();
    assert!(core.host.restart(Kind::Channel, "echo"));
    wait_until(|| {
        let current = core.runtime.registry.channel("echo")?;
        (!Arc::ptr_eq(&current, &old) && matches!(state(&core), PluginState::Running { .. }))
            .then_some(())
    })
    .await;

    // Calls through it fail: its connection is closed, not just abandoned.
    let stale = OutboundMessage {
        target: SendTarget::Conversation(ConversationId::new("c1").unwrap()),
        markdown: "stale".into(),
        attachments: Vec::new(),
        reply_to: None,
    };
    assert!(old.send(stale).await.is_err());
    assert!(old.events().await.is_err());
    assert!(
        !home
            .lines("echo", "sent.jsonl")
            .iter()
            .any(|line| line["message"]["markdown"] == "stale")
    );

    // The new connection carries the traffic.
    round_trip(&home, &core, "after", 2).await;
    core.stop().await;
    plugin.kill().await;
}

#[tokio::test]
async fn a_wrong_bearer_token_is_refused_and_retried() {
    let mut home = Home::serving("");
    let plugin = Remote::start("127.0.0.1:0", &home.data_dir("echo"), Some("right"), None).await;
    home.add_channels(&format!(
        "[channel.echo]\naddress = \"http://{}\"\nauth = {{ bearer = {{ env = \"ECHO_TOKEN\" }} }}",
        plugin.addr
    ));
    home.set_env("ECHO_TOKEN", "wrong-token");
    let core = home.start_core().await;
    let error = backing_off(&core).await;
    assert!(
        error.starts_with("Describe failed: Unauthenticated"),
        "{error}"
    );
    assert!(!error.contains("wrong-token"), "{error}");
    assert!(core.runtime.registry.channel("echo").is_none());
    core.stop().await;
    plugin.kill().await;
}

#[tokio::test]
async fn an_unreachable_plugin_is_retried_until_it_comes_up_and_after_it_goes_away() {
    let home = Home::serving("");
    // Bound but not listening: connects are refused, and nothing else can take the port.
    let reserved =
        socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
    reserved
        .bind(&std::net::SocketAddr::from(([127, 0, 0, 1], 0)).into())
        .unwrap();
    let addr = reserved.local_addr().unwrap().as_socket().unwrap();
    home.add_channels(&format!("[channel.echo]\naddress = \"http://{addr}\""));
    let core = home.start_core().await;
    let error = backing_off(&core).await;
    assert!(error.starts_with("cannot connect to the plugin"), "{error}");

    // It comes up: the next attempt (after 1 s, then 2 s) connects.
    let data = home.data_dir("echo");
    drop(reserved);
    let plugin = Remote::start(&addr.to_string(), &data, None, None).await;
    running(&core).await;
    round_trip(&home, &core, "up", 1).await;

    // It goes away: the core notices the closed connection, and reconnects once it's back.
    plugin.kill().await;
    let error = wait_until(|| {
        (!matches!(state(&core), PluginState::Running { .. })).then(|| last_error(&core))
    })
    .await;
    assert_eq!(error, "the plugin disconnected");
    let plugin = Remote::start(&addr.to_string(), &data, None, None).await;
    running(&core).await;
    assert_eq!(core.host.status("echo").unwrap().restarts, 2);
    round_trip(&home, &core, "back", 2).await;
    core.stop().await;
    plugin.kill().await;
}

#[tokio::test]
async fn oauth_client_credentials_get_the_bearer_token() {
    let (token_url, requests) = token_endpoint(
        "200 OK",
        r#"{"access_token":"issued-token","token_type":"Bearer","expires_in":3600}"#,
    )
    .await;
    let mut home = Home::serving("");
    let plugin = Remote::start(
        "127.0.0.1:0",
        &home.data_dir("echo"),
        Some("issued-token"),
        None,
    )
    .await;
    home.add_channels(&format!(
        "[channel.echo]\naddress = \"http://{}\"\n\n[channel.echo.auth.oauth]\n\
         token_url = \"{token_url}\"\nclient_id = \"giles\"\n\
         client_secret = {{ env = \"CLIENT_SECRET\" }}\nscopes = [\"echo\"]",
        plugin.addr
    ));
    home.set_env("CLIENT_SECRET", "client-s3cret");
    let core = home.start_core().await;
    running(&core).await;
    round_trip(&home, &core, "oauth", 1).await;
    // Basic base64("giles:client-s3cret")
    assert_eq!(
        *requests.lock().unwrap(),
        ["Basic Z2lsZXM6Y2xpZW50LXMzY3JldA=="]
    );
    core.stop().await;
    plugin.kill().await;
}

#[tokio::test]
async fn a_refused_token_request_leaves_the_plugin_backing_off() {
    let (token_url, requests) = token_endpoint(
        "401 Unauthorized",
        r#"{"error":"invalid_client","error_description":"client-s3cret is wrong"}"#,
    )
    .await;
    let mut home = Home::serving("");
    let plugin = Remote::start("127.0.0.1:0", &home.data_dir("echo"), None, None).await;
    home.add_channels(&format!(
        "[channel.echo]\naddress = \"http://{}\"\n\n[channel.echo.auth.oauth]\n\
         token_url = \"{token_url}\"\nclient_id = \"giles\"\n\
         client_secret = {{ env = \"CLIENT_SECRET\" }}",
        plugin.addr
    ));
    home.set_env("CLIENT_SECRET", "client-s3cret");
    let core = home.start_core().await;
    let error = backing_off(&core).await;
    assert_eq!(
        error,
        "cannot authenticate to the plugin: the token endpoint answered 401 Unauthorized: \
         invalid_client (<redacted> is wrong)"
    );
    assert!(!requests.lock().unwrap().is_empty());
    core.stop().await;
    plugin.kill().await;
}

#[tokio::test]
async fn tls_connects_to_a_trusted_certificate_only() {
    let pki = Pki::new();
    let home = Home::serving("");
    let plugin = Remote::start("127.0.0.1:0", &home.data_dir("echo"), None, Some(&pki)).await;
    home.add_channels(&format!(
        "[channel.echo]\naddress = \"https://localhost:{}\"",
        plugin.addr.port()
    ));

    // The bundled roots don't know the test authority.
    let core = home.start_core().await;
    let error = backing_off(&core).await;
    assert!(error.contains("certificate"), "{error}");
    core.stop().await;

    let launcher = home
        .launcher("/nonexistent/giles".into())
        .remote_tls_roots(pki.roots.clone());
    let core = home.start_core_with(launcher).await;
    running(&core).await;
    round_trip(&home, &core, "tls", 1).await;
    core.stop().await;
    plugin.kill().await;
}

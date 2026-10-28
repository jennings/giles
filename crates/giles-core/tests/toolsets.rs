//! Toolsets end to end through `ToolsetLauncher`: a stdio MCP server found on `PATH` and launched
//! through the sandbox trampoline (real Seatbelt on macOS, Landlock where the kernel has it), the
//! same server unsandboxed, and a remote server over streamable HTTP with a bearer token.
//!
//! The trampoline and the server are this test binary. Run as `<test> sandbox-exec <spec> --
//! <command>` it does what `giles sandbox-exec` does (`giles_core::sandbox::exec`, single-threaded,
//! before any runtime starts; Cargo gives tests here no `giles` binary). With
//! `GILES_TOOLSET_TEST_SERVER=<port>` set it is the server: it reports its environment, working
//! directory, and whether it can connect to `127.0.0.1:<port>` on stderr, then serves the MCP test
//! server on stdio. Hence no libtest harness: `main` runs the tests in order.
#![allow(missing_docs)]

#[path = "support/mcp_server.rs"]
mod mcp_server;
#[path = "support/stored_toolsets.rs"]
mod stored_toolsets;

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use futures::StreamExt as _;
use giles_core::config::{Builtins, Config, LoadOptions};
use giles_core::paths::DataRoot;
use giles_core::sandbox::{self, SandboxError, Spec};
use giles_core::toolsets::ToolsetLauncher;
use giles_store::{BlobMeta, BlobStore, Store};
use giles_types::{
    AgentId, Block, CallContext, Error, Timestamp, ToolCall, ToolCallEvent, Toolset,
};
use rmcp::ServiceExt;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::json;

use self::mcp_server::{TOOLS, TestServer};
use self::stored_toolsets::add_toolsets;

/// Set (to the port to probe) in the server's configured `env`.
const SERVER_ENV: &str = "GILES_TOOLSET_TEST_SERVER";
/// A credential under an arbitrary name, as MCP servers take them.
const TOKEN_ENV: &str = "TEST_TOOLSET_TOKEN";
const TOKEN: &str = "tok-0123456789";
/// The server's name on the `PATH` the launcher gets.
const SERVER_NAME: &str = "giles-test-mcp-server";
const WAIT: Duration = Duration::from_secs(20);

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    if args.get(1).is_some_and(|arg| arg == "sandbox-exec") {
        trampoline(&args[2..]);
    }
    if let Some(port) = std::env::var_os(SERVER_ENV) {
        serve(&port.to_string_lossy());
        return ExitCode::SUCCESS;
    }
    let log = Log::default();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .with_writer({
            let log = log.clone();
            move || log.clone()
        })
        .init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    macro_rules! run {
        ($($test:ident),* $(,)?) => {$(
            print!("test {} ... ", stringify!($test));
            runtime
                .block_on(async { tokio::time::timeout(Duration::from_secs(120), $test(&log)).await })
                .expect("timed out");
            println!("ok");
        )*};
    }
    run!(
        sandboxed_stdio_server,
        unsandboxed_stdio_server,
        remote_server_with_a_bearer_token,
    );
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

/// The server: reports what it sees on stderr, then serves MCP on stdio until stdin closes.
fn serve(port: &str) {
    let mut names: Vec<String> = std::env::vars_os()
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .filter(|name| name != "LANG" && !name.starts_with("__CF"))
        .collect();
    names.sort();
    eprintln!("env: {}", names.join(","));
    let token = std::env::var(TOKEN_ENV).is_ok_and(|token| token == TOKEN);
    eprintln!("token: {}", if token { "passed" } else { "missing" });
    let cwd = std::env::current_dir().unwrap();
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    eprintln!(
        "cwd: {}",
        if cwd == home { "data dir" } else { "elsewhere" }
    );
    let network = match std::net::TcpStream::connect(("127.0.0.1", port.parse().unwrap())) {
        Ok(_) => "connected".to_owned(),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => "denied".to_owned(),
        Err(error) => format!("failed ({error})"),
    };
    eprintln!("network: {network}");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let running = TestServer::default()
            .serve(rmcp::transport::io::stdio())
            .await
            .expect("server handshake");
        let _ = running.waiting().await;
    });
}

/// A data root with `giles.toml` declaring `toolsets`, and a `PATH` directory holding the server
/// (this test binary) as [`SERVER_NAME`].
struct Home {
    dir: tempfile::TempDir,
    root: DataRoot,
}

impl Home {
    fn new(toolsets: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = DataRoot::new(base.join("home"), Path::new("/tmp")).unwrap();
        root.create_layout().unwrap();
        fs::write(
            root.config_file(),
            format!("[owner]\nidentities = [\"web:owner\"]\n\n{toolsets}"),
        )
        .unwrap();
        let bin = base.join("tools/bin");
        fs::create_dir_all(&bin).unwrap();
        symlink(std::env::current_exe().unwrap(), bin.join(SERVER_NAME)).unwrap();
        Self { dir, root }
    }

    fn path(&self) -> OsString {
        fs::canonicalize(self.dir.path())
            .unwrap()
            .join("tools/bin")
            .into()
    }

    fn config(&self) -> Config {
        let path = self.path();
        let env = move |name: &str| (name == "PATH").then(|| path.clone());
        Config::load_with(
            &self.root,
            &LoadOptions {
                builtins: Builtins::V1,
                env: &env,
                giles_exe: None,
            },
        )
        .unwrap()
    }

    /// The data root's blob store.
    async fn blobs(&self) -> BlobStore {
        let db = Store::open(self.root.database_file()).await.unwrap();
        BlobStore::open(self.root.blobs_dir(), db).await.unwrap()
    }

    fn launcher(&self, sandbox: Result<(), String>, blobs: BlobStore) -> ToolsetLauncher {
        let path = self.path();
        let exe = std::env::current_exe().unwrap();
        ToolsetLauncher::new(self.root.clone(), exe, sandbox, blobs)
            .env(move |name| (name == "PATH").then(|| path.clone()))
    }
}

/// A `[toolset.<name>]` running the server by name, probing `port`, with `extra` keys.
fn stdio_toolset(name: &str, port: u16, extra: &str) -> String {
    format!(
        "[toolset.{name}]\ncommand = \"{SERVER_NAME}\"\n\
         env = {{ {SERVER_ENV} = \"{port}\", {TOKEN_ENV} = \"{TOKEN}\" }}\n{extra}\n"
    )
}

/// The recorded log of this process.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl Log {
    fn contents(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }

    async fn wait_for(&self, needle: &str) {
        tokio::time::timeout(WAIT, async {
            while !self.contents().contains(needle) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{needle:?} never logged; log:\n{}", self.contents()));
    }

    /// Waits for the server of `instance` to report `what` (e.g. `network`) and returns the rest
    /// of that line.
    async fn report(&self, instance: &str, what: &str) -> String {
        let prefix = format!("mcp{{instance={instance}}}: giles_core::mcp::stderr: {what}: ");
        self.wait_for(&prefix).await;
        let contents = self.contents();
        let start = contents.find(&prefix).unwrap() + prefix.len();
        contents[start..].lines().next().unwrap().to_owned()
    }
}

impl Write for Log {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The environment every launched server sees (apart from `LANG`).
const SERVER_ENV_NAMES: &str = "GILES_PLUGIN_DATA,GILES_TOOLSET_TEST_SERVER,HOME,PATH,\
                                TEST_TOOLSET_TOKEN,TMPDIR";

async fn tool_names(toolset: &dyn Toolset) -> Vec<String> {
    let mut names: Vec<String> = toolset
        .list_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    names.sort();
    names
}

/// Calls `read_file` on the server of `instance` with an attachment as `{"$media": "<id>"}`. The
/// server gets the path of a copy in its `TMPDIR` and reads it; the copy is gone once the call
/// ends.
async fn media_reaches_the_server(
    home: &Home,
    instance: &str,
    toolset: &dyn Toolset,
    blobs: &BlobStore,
) {
    let meta = BlobMeta {
        media_type: "text/plain".to_owned(),
        name: Some("my notes.txt".to_owned()),
    };
    let media = blobs
        .put(&b"attached notes"[..], 1024, meta, Timestamp::now())
        .await
        .unwrap();
    let call = ToolCall {
        id: "toolu_read".to_owned(),
        name: "read_file".to_owned(),
        input: json!({"path": {"$media": media.id.as_str()}}),
        media: vec![media],
    };
    let ctx = CallContext {
        agent: AgentId::new("agent").unwrap(),
        requester: None,
        conversation: None,
    };
    let mut handle = toolset.call(call, ctx).await.unwrap();
    let event = tokio::time::timeout(WAIT, handle.events.next())
        .await
        .expect("a result")
        .expect("an event")
        .unwrap();
    let ToolCallEvent::Result { content, is_error } = &event else {
        panic!("not a result: {event:?}");
    };
    let [Block::Text { text }] = content.as_slice() else {
        panic!("unexpected content {content:?}");
    };
    assert!(!is_error, "{text}");
    let (path, contents) = text.split_once('\n').unwrap();
    assert_eq!(contents, "attached notes");
    let path = Path::new(path);
    let tmp = home.root.plugin_data_dir(instance).unwrap().join("tmp");
    assert_eq!(path.parent(), Some(tmp.as_path()));
    let name = path.file_name().unwrap().to_str().unwrap();
    assert!(
        name.starts_with("giles-media-") && name.ends_with("-my_notes.txt"),
        "{name}"
    );
    drop(handle);
    tokio::time::timeout(WAIT, async {
        while path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the copy outlived the call");
}

async fn sandboxed_stdio_server(log: &Log) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let granted = format!("sandbox = {{ network = [\"tcp:{port}\"] }}");
    let home = Home::new(&format!(
        "{}{}",
        stdio_toolset("probe", port, ""),
        stdio_toolset("netprobe", port, &granted)
    ));
    let config = home.config();
    let host = sandbox::check_host().map(|_| ()).map_err(|e| e.to_string());
    let blobs = home.blobs().await;
    let launcher = home.launcher(host.clone(), blobs.clone());

    if let Err(reason) = host {
        // No sandbox on this host (a Linux kernel without Landlock): nothing runs.
        match launcher.connect(&config.toolsets["probe"]).await {
            Err(Error::InvalidRequest(message)) => {
                assert!(message.contains("this host can't sandbox it"), "{message}");
                assert!(message.contains(&reason), "{message}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        println!("(no sandbox on this host: {reason}) ");
        return;
    }

    let toolset = launcher
        .connect(&config.toolsets["probe"])
        .await
        .unwrap_or_else(|e| panic!("{e}\nlog:\n{}", log.contents()));
    assert_eq!(tool_names(&toolset).await, TOOLS);
    media_reaches_the_server(&home, "probe", &toolset, &blobs).await;
    assert_eq!(log.report("probe", "env").await, SERVER_ENV_NAMES);
    assert_eq!(log.report("probe", "token").await, "passed");
    assert_eq!(log.report("probe", "cwd").await, "data dir");
    assert_eq!(log.report("probe", "network").await, "denied");
    let found = PathBuf::from(home.path()).join(SERVER_NAME);
    log.wait_for(&format!(
        "starting the toolset toolset=probe program={} sandboxed=true",
        fs::canonicalize(&found).unwrap().display()
    ))
    .await;

    // The spec: private, the instance's data directory, the server's directories granted, and
    // the credential's name but not its value.
    let spec_file = home.root.sandbox_spec_file("probe").unwrap();
    let mode = fs::metadata(&spec_file).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let json = fs::read_to_string(&spec_file).unwrap();
    assert!(!json.contains(TOKEN), "{json}");
    let spec = Spec::parse(&json).unwrap();
    assert_eq!(spec.data_dir, home.root.plugin_data_dir("probe").unwrap());
    assert!(!spec.unsandboxed);
    assert!(spec.inherit_env.contains(TOKEN_ENV));
    // Found as `<tmp>/tools/bin/<name>`: the install prefix `<tmp>/tools` is granted, and the
    // directory of the file the link leads to (the test binary's). Grants are sorted.
    let bin_dir = PathBuf::from(home.path());
    let exe_dir = fs::canonicalize(std::env::current_exe().unwrap())
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let mut granted = vec![exe_dir, bin_dir.parent().unwrap().to_path_buf()];
    granted.sort();
    assert_eq!(spec.read, granted, "{spec:?}");
    assert_eq!(spec.execute, granted, "{spec:?}");
    assert_eq!(
        spec.search_path(),
        format!("{}:/usr/bin:/bin", bin_dir.display())
    );
    drop(toolset);

    // A port grant lets it connect.
    let toolset = launcher
        .connect(&config.toolsets["netprobe"])
        .await
        .unwrap_or_else(|e| panic!("{e}\nlog:\n{}", log.contents()));
    assert_eq!(log.report("netprobe", "network").await, "connected");
    drop(toolset);
    drop(listener);
}

async fn unsandboxed_stdio_server(log: &Log) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let home = Home::new(&stdio_toolset("loose", port, "unsandboxed = true"));
    let config = home.config();
    // Even a host without a sandbox runs it.
    let blobs = home.blobs().await;
    let launcher = home.launcher(Err("no sandbox here".to_owned()), blobs.clone());
    let toolset = launcher.connect(&config.toolsets["loose"]).await.unwrap();
    assert_eq!(tool_names(&toolset).await, TOOLS);
    media_reaches_the_server(&home, "loose", &toolset, &blobs).await;
    // The same scrubbed environment and working directory, without the sandbox.
    assert_eq!(log.report("loose", "env").await, SERVER_ENV_NAMES);
    assert_eq!(log.report("loose", "token").await, "passed");
    assert_eq!(log.report("loose", "cwd").await, "data dir");
    assert_eq!(log.report("loose", "network").await, "connected");
    log.wait_for("running the toolset unsandboxed (`unsandboxed = true`) toolset=loose")
        .await;
    assert!(!home.root.sandbox_spec_file("loose").unwrap().exists());
    drop(toolset);

    // A copy a crash left behind is removed at the next start; the server's own files stay.
    let tmp = home.root.plugin_data_dir("loose").unwrap().join("tmp");
    fs::write(tmp.join("giles-media-0123-left.txt"), "left").unwrap();
    fs::write(tmp.join("server.txt"), "mine").unwrap();
    let toolset = launcher.connect(&config.toolsets["loose"]).await.unwrap();
    let mut left: Vec<_> = fs::read_dir(&tmp)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    left.sort();
    assert_eq!(left, ["server.txt"]);
    drop(toolset);
}

const BEARER: &str = "remote-bearer-token";

async fn require_bearer(request: Request, next: Next) -> Response {
    let authorization = request.headers().get(axum::http::header::AUTHORIZATION);
    if authorization.is_some_and(|value| value == format!("Bearer {BEARER}").as_str()) {
        next.run(request).await
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

async fn remote_server_with_a_bearer_token(_log: &Log) {
    let server = TestServer::default();
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let router = axum::Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn(require_bearer));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await });

    // Remote toolsets are stored ones, with their tokens in `secrets.toml`.
    let home = Home::new("");
    let secrets = home.root.secrets_file();
    fs::write(
        &secrets,
        format!("remote = \"{BEARER}\"\nwrong = \"not-the-token\"\n"),
    )
    .unwrap();
    fs::set_permissions(&secrets, fs::Permissions::from_mode(0o600)).unwrap();
    let mut config = home.config();
    add_toolsets(
        &home.root,
        &mut config,
        &[
            (
                "remote",
                &format!("url = \"{url}\"\nauth = {{ bearer = {{ secret = \"remote\" }} }}\n"),
            ),
            (
                "wrong",
                &format!("url = \"{url}\"\nauth = {{ bearer = {{ secret = \"wrong\" }} }}\n"),
            ),
        ],
    );
    let no_sandbox = Err("remote servers need no sandbox".to_owned());
    let launcher = home.launcher(no_sandbox, home.blobs().await);
    let toolset = launcher.connect(&config.toolsets["remote"]).await.unwrap();
    assert_eq!(tool_names(&toolset).await, TOOLS);
    match launcher.connect(&config.toolsets["wrong"]).await {
        Err(error @ Error::InvalidRequest(_)) => assert!(!error.is_retryable()),
        other => panic!("expected rejected credentials, got {other:?}"),
    }
}

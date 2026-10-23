//! `giles-channel-echo`: a channel plugin for testing the plugin host, never for real use.
//!
//! It talks to nobody. Tests drive it through files in its data directory (`GILES_PLUGIN_DATA`):
//!
//! - `inject/*.json`: inbound events to emit, in file-name order. Each file is
//!   `{"event": <ChannelEvent as giles-types JSON>, "attachments": [{"text": "...", "media_type":
//!   "text/plain", "name": "a.txt"}]}`, `attachments` optional (message events only; each becomes
//!   a blob appended to the message's attachments). Write the file elsewhere and rename it in. A
//!   file is deleted once the SDK has persisted its event; one that doesn't parse is renamed to
//!   `*.bad`.
//! - `sent.jsonl`: one line per `Send`: `{"message_id": "echo-<n>", "message": <OutboundMessage>,
//!   "attachment_texts": [...]}`, the attachments read back through the SDK as UTF-8 (lossy).
//! - `acks.jsonl`: one line per event the core acked: `{"event_id": "..."}`.
//!
//! Config (all optional): `namespace` (default `"echo"`), `echo` (default `false`: when set, every
//! sent message also comes back as an inbound message from `echo_sender`, default `"echo-user"`, in
//! the same conversation, with its attachments, event id `echo-<n>`), and `unavailable` (recipients
//! whose sends fail with `UNAVAILABLE`). Unknown keys are refused.
//!
//! Run as a remote plugin (PLAN section 7.6), an example of `ChannelPlugin::serve_remote`:
//!
//! ```text
//! giles-channel-echo --listen <address> --data <dir> [--tls-cert <pem> --tls-key <pem>]
//! ```
//!
//! It accepts calls with `authorization: Bearer $GILES_ECHO_TOKEN` (any call if that isn't set),
//! prints `listening on <address>` to stdout once it listens (useful with port 0), and runs until
//! `SIGINT` or `SIGTERM`. `<dir>` is its data directory.

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::stream::{self, BoxStream, StreamExt as _};
use giles_plugin_sdk::giles_types::{
    Channel, ChannelEvent, ChannelInfo, ConversationId, ConversationKind, ConversationRef, Error,
    EventId, InboundMessage, MessageId, NativeId, OutboundMessage, OutputMode, Result, SendTarget,
};
use giles_plugin_sdk::{Blobs, ChannelPlugin, Configure, RemoteAuth, RemoteServer, async_trait};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::Notify;
use tracing_subscriber::EnvFilter;

/// Where tests drop events to emit.
const INJECT_DIR: &str = "inject";
/// Where sends are recorded.
const SENT_FILE: &str = "sent.jsonl";
/// Where acks are recorded.
const ACKS_FILE: &str = "acks.jsonl";
/// How often `inject/` is checked.
const POLL: Duration = Duration::from_millis(20);

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_ansi(false)
        .with_env_filter(
            EnvFilter::try_from_env("GILES_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let plugin = ChannelPlugin::new("echo", env!("CARGO_PKG_VERSION"))
        .output(OutputMode::Blocks)
        .can_initiate(true);
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        return plugin.serve(Echo::configure).await;
    }
    match serve_remote(plugin, &args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("giles-channel-echo: {e}");
            ExitCode::from(2)
        }
    }
}

/// Remote mode: `--listen <address> --data <dir> [--tls-cert <pem> --tls-key <pem>]`.
async fn serve_remote(plugin: ChannelPlugin, args: &[String]) -> io::Result<()> {
    let usage = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: giles-channel-echo --listen <address> --data <dir> \
             [--tls-cert <pem> --tls-key <pem>]",
        )
    };
    let (mut listen, mut data, mut cert, mut key) = (None, None, None, None);
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let slot = match flag.as_str() {
            "--listen" => &mut listen,
            "--data" => &mut data,
            "--tls-cert" => &mut cert,
            "--tls-key" => &mut key,
            _ => return Err(usage()),
        };
        *slot = Some(args.next().ok_or_else(usage)?.clone());
    }
    let (Some(listen), Some(data)) = (listen, data) else {
        return Err(usage());
    };
    let auth = match std::env::var("GILES_ECHO_TOKEN") {
        Ok(token) => RemoteAuth::Bearer(token),
        Err(_) => RemoteAuth::None,
    };
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    let mut server = RemoteServer::new(listener, data, auth);
    match (cert, key) {
        (Some(cert), Some(key)) => {
            let config = giles_plugin_sdk::tls::server_config(
                &tokio::fs::read(cert).await?,
                &tokio::fs::read(key).await?,
            )?;
            server = server.tls(config);
        }
        (None, None) => {}
        _ => return Err(usage()),
    }
    {
        use std::io::Write as _;
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "listening on {}", server.local_addr()?)?;
        stdout.flush()?;
    }
    plugin.serve_remote(server, Echo::configure).await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default = "default_namespace")]
    namespace: String,
    #[serde(default)]
    echo: bool,
    #[serde(default = "default_echo_sender")]
    echo_sender: String,
    #[serde(default)]
    unavailable: Vec<String>,
}

fn default_namespace() -> String {
    "echo".into()
}

fn default_echo_sender() -> String {
    "echo-user".into()
}

/// One `inject/*.json` file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Injection {
    event: ChannelEvent,
    #[serde(default)]
    attachments: Vec<Attachment>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Attachment {
    text: String,
    media_type: String,
    #[serde(default)]
    name: Option<String>,
}

/// One `sent.jsonl` line.
#[derive(Serialize)]
struct SentRecord<'a> {
    message_id: &'a MessageId,
    message: &'a OutboundMessage,
    attachment_texts: Vec<String>,
}

/// Echoed messages waiting for the event stream.
#[derive(Default)]
struct Echoes {
    queue: Mutex<VecDeque<ChannelEvent>>,
    added: Notify,
}

impl Echoes {
    fn push(&self, event: ChannelEvent) {
        self.queue.lock().expect("never poisoned").push_back(event);
        self.added.notify_one();
    }

    fn pop(&self) -> Option<ChannelEvent> {
        self.queue.lock().expect("never poisoned").pop_front()
    }
}

struct Echo {
    info: ChannelInfo,
    config: Config,
    data_dir: PathBuf,
    blobs: Blobs,
    echoes: Arc<Echoes>,
    /// Sends so far, over every run: the next message id's number.
    sent: tokio::sync::Mutex<usize>,
}

impl Echo {
    async fn configure(ctx: Configure) -> Result<Self> {
        let config: Config = ctx.parse_config()?;
        let data_dir = ctx.data_dir().to_owned();
        tokio::fs::create_dir_all(data_dir.join(INJECT_DIR))
            .await
            .map_err(Error::other)?;
        let sent = count_lines(&data_dir.join(SENT_FILE))
            .await
            .map_err(Error::other)?;
        tracing::info!(
            instance = ctx.instance(),
            ?config,
            "echo channel configured"
        );
        Ok(Self {
            info: ChannelInfo {
                identity_namespace: config.namespace.clone(),
                output: OutputMode::Blocks,
                can_initiate: true,
            },
            config,
            data_dir,
            blobs: ctx.blobs().clone(),
            echoes: Arc::default(),
            sent: tokio::sync::Mutex::new(sent),
        })
    }
}

#[async_trait]
impl Channel for Echo {
    fn info(&self) -> &ChannelInfo {
        &self.info
    }

    async fn events(&self) -> Result<BoxStream<'static, Result<ChannelEvent>>> {
        let source = Source {
            inject: self.data_dir.join(INJECT_DIR),
            blobs: self.blobs.clone(),
            echoes: Arc::clone(&self.echoes),
            emitted: None,
        };
        Ok(stream::unfold(source, |mut source| async move {
            let event = source.next().await;
            Some((event, source))
        })
        .boxed())
    }

    async fn ack(&self, event: &EventId) -> Result<()> {
        let line = serde_json::json!({ "event_id": event });
        append_line(&self.data_dir.join(ACKS_FILE), &line).await
    }

    async fn send(&self, msg: OutboundMessage) -> Result<MessageId> {
        if let SendTarget::Recipient(recipient) = &msg.target
            && self
                .config
                .unavailable
                .iter()
                .any(|r| r == recipient.as_str())
        {
            return Err(Error::unavailable(format!("{recipient} is not reachable")));
        }
        let mut contents = Vec::new();
        for media in &msg.attachments {
            contents.push(self.blobs.read(media).await?);
        }
        let mut sent = self.sent.lock().await;
        *sent += 1;
        let id = MessageId::new(format!("echo-{sent}"))?;
        let record = SentRecord {
            message_id: &id,
            message: &msg,
            attachment_texts: contents
                .iter()
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
                .collect(),
        };
        append_line(&self.data_dir.join(SENT_FILE), &record).await?;
        if self.config.echo {
            let event = self.echo_of(&id, msg, contents).await?;
            self.echoes.push(event);
        }
        Ok(id)
    }
}

impl Echo {
    /// The sent message as an inbound one from `echo_sender`, its attachments stored again as the
    /// plugin's own blobs (the core's are gone once the `Send` returns).
    async fn echo_of(
        &self,
        id: &MessageId,
        msg: OutboundMessage,
        contents: Vec<Vec<u8>>,
    ) -> Result<ChannelEvent> {
        let conversation = match msg.target {
            SendTarget::Conversation(id) => id,
            SendTarget::Recipient(id) => ConversationId::new(id.as_str())?,
            other => return Err(Error::InvalidRequest(format!("unknown target {other:?}"))),
        };
        let mut attachments = Vec::new();
        for (media, bytes) in msg.attachments.into_iter().zip(contents) {
            attachments.push(self.blobs.put(bytes, media.media_type, media.name).await?);
        }
        Ok(ChannelEvent::Message {
            id: EventId::new(id.as_str())?,
            message: InboundMessage {
                conversation: ConversationRef {
                    id: conversation,
                    kind: ConversationKind::Direct,
                    title: None,
                },
                sender: NativeId::new(self.config.echo_sender.as_str())?,
                sender_aliases: vec![],
                sender_authenticated: true,
                sender_display_name: None,
                text: msg.markdown,
                attachments,
                message_id: Some(id.clone()),
                reply_to: msg.reply_to,
                sent_at: None,
            },
        })
    }
}

/// The event stream: echoes first, then injected files.
struct Source {
    inject: PathBuf,
    blobs: Blobs,
    echoes: Arc<Echoes>,
    /// The file of the event yielded last.
    emitted: Option<PathBuf>,
}

impl Source {
    async fn next(&mut self) -> Result<ChannelEvent> {
        // The SDK persists an event before it polls the stream again, so the file can go now.
        if let Some(path) = self.emitted.take()
            && let Err(e) = tokio::fs::remove_file(&path).await
        {
            tracing::warn!(path = %path.display(), "removing an emitted injection: {e}");
        }
        loop {
            if let Some(event) = self.echoes.pop() {
                return Ok(event);
            }
            let next = match first_injection(&self.inject).await {
                Ok(next) => next,
                Err(e) => {
                    tokio::time::sleep(POLL).await;
                    return Err(Error::other(e));
                }
            };
            if let Some(path) = next {
                match self.load(&path).await {
                    Ok(event) => {
                        self.emitted = Some(path);
                        return Ok(event);
                    }
                    Err(e) => {
                        tracing::error!(path = %path.display(), "bad injection, renamed to .bad: {e}");
                        tokio::fs::rename(&path, path.with_extension("bad"))
                            .await
                            .map_err(Error::other)?;
                        continue;
                    }
                }
            }
            tokio::select! {
                () = self.echoes.added.notified() => {}
                () = tokio::time::sleep(POLL) => {}
            }
        }
    }

    async fn load(&self, path: &Path) -> Result<ChannelEvent> {
        let bytes = tokio::fs::read(path).await.map_err(Error::other)?;
        let injection: Injection =
            serde_json::from_slice(&bytes).map_err(|e| Error::InvalidRequest(e.to_string()))?;
        let mut event = injection.event;
        if injection.attachments.is_empty() {
            return Ok(event);
        }
        let ChannelEvent::Message { message, .. } = &mut event else {
            return Err(Error::InvalidRequest(
                "attachments need a message event".into(),
            ));
        };
        for attachment in injection.attachments {
            let media = self
                .blobs
                .put(attachment.text, attachment.media_type, attachment.name)
                .await?;
            message.attachments.push(media);
        }
        Ok(event)
    }
}

/// The first `*.json` file in `dir` by name.
async fn first_injection(dir: &Path) -> io::Result<Option<PathBuf>> {
    let mut entries = tokio::fs::read_dir(dir).await?;
    let mut first: Option<PathBuf> = None;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "json")
            && first.as_ref().is_none_or(|first| path < *first)
        {
            first = Some(path);
        }
    }
    Ok(first)
}

/// Lines in a file that may not exist yet.
async fn count_lines(path: &Path) -> io::Result<usize> {
    match tokio::fs::read(path).await {
        Ok(bytes) => Ok(bytes.iter().filter(|&&b| b == b'\n').count()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e),
    }
}

/// Appends one JSON line and syncs it, so a test reading the file sees whole lines.
async fn append_line(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut line = serde_json::to_vec(value).map_err(Error::other)?;
    line.push(b'\n');
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .map_err(Error::other)?;
    file.write_all(&line).await.map_err(Error::other)?;
    file.sync_data().await.map_err(Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_has_defaults_and_refuses_unknown_keys() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(config.namespace, "echo");
        assert!(!config.echo);
        assert_eq!(config.echo_sender, "echo-user");
        assert!(config.unavailable.is_empty());
        let err = serde_json::from_str::<Config>(r#"{"ecco": true}"#).unwrap_err();
        assert!(err.to_string().contains("unknown field `ecco`"), "{err}");
    }

    #[test]
    fn injections_parse_with_optional_attachments() {
        let injection: Injection = serde_json::from_value(serde_json::json!({
            "event": {"type": "status", "id": "s1", "status": {"type": "connected"}},
        }))
        .unwrap();
        assert_eq!(injection.event.id().as_str(), "s1");
        assert!(injection.attachments.is_empty());
        assert!(
            serde_json::from_value::<Injection>(serde_json::json!({"event": {}, "extra": 1}))
                .is_err()
        );
    }

    #[tokio::test]
    async fn injections_are_taken_in_name_order_and_only_json() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(first_injection(dir.path()).await.unwrap(), None);
        for name in ["b.json", "a.json.tmp", "c.json", "0.bad", "a.json"] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        assert_eq!(
            first_injection(dir.path()).await.unwrap(),
            Some(dir.path().join("a.json"))
        );
    }

    #[tokio::test]
    async fn lines_are_appended_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.jsonl");
        assert_eq!(count_lines(&path).await.unwrap(), 0);
        append_line(&path, &serde_json::json!({"a": 1}))
            .await
            .unwrap();
        append_line(&path, &serde_json::json!({"a": 2}))
            .await
            .unwrap();
        assert_eq!(count_lines(&path).await.unwrap(), 2);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"a\":1}\n{\"a\":2}\n"
        );
    }
}

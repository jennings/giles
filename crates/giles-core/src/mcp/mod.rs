//! The MCP client: a [`Toolset`] over one MCP server, using `rmcp` (PLAN sections 7.10, 8.6).
//!
//! Doc: `Docs/technical/mcp.md`. Each [`McpToolset`] is one connection, made ready by the
//! `initialize` handshake; [`crate::toolsets`] launches stdio servers in their sandbox and
//! supervises every connection.

mod client;
mod convert;
mod http;
mod media;
mod stdio;

use std::borrow::Cow;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use giles_types::{
    ApprovalAnswer, CallContext, Error, Result, Skill, SkillSummary, ToolCall, ToolCallEvent,
    ToolCallHandle, ToolSpec, Toolset,
};
use rmcp::model::{
    CallToolRequestParams, CancelledNotificationParam, ClientRequest, PaginatedRequestParams,
    Request, ServerResult,
};
use rmcp::service::{
    Peer, PeerRequestOptions, RequestHandle, RoleClient, RunningService, RxJsonRpcMessage,
    TxJsonRpcMessage, serve_client,
};
use rmcp::transport::{IntoTransport, Transport};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, watch};
use tracing::Instrument;

use self::client::{ClientState, ProgressSubscription};
use self::convert::Origin;
pub use self::http::HttpServer;
pub use self::media::{MAX_RESULT_MEDIA_BYTES, McpMedia};
pub use self::stdio::{DirectCommand, STDERR_TARGET};
use crate::config::Secret;
use crate::sandbox;

/// How long a server has to answer `initialize`.
pub const INIT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for a stdio server's exit status once its connection has closed.
const EXIT_GRACE: Duration = Duration::from_secs(2);

/// The most `tools/list` pages read for one list, so a server that keeps returning a cursor can't
/// keep the core listing forever.
const MAX_TOOL_PAGES: usize = 100;

/// Events of one call buffered before the call waits for the consumer.
const CALL_EVENT_BUFFER: usize = 16;

/// One MCP server as a [`Toolset`].
///
/// Dropping it closes the connection; a stdio server is killed with its whole process group.
pub struct McpToolset {
    origin: Arc<Origin>,
    service: RunningService<RoleClient, ClientState>,
    /// Becomes `true` when the connection ends.
    closed: watch::Receiver<bool>,
    /// Where media in tool results goes.
    media: McpMedia,
    /// A stdio server's exit status, once it has exited.
    exit: Option<watch::Receiver<Option<ExitStatus>>>,
    /// Kills a stdio server on drop.
    _child: Option<stdio::ChildGuard>,
}

impl std::fmt::Debug for McpToolset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpToolset")
            .field("instance", &self.origin.instance)
            .finish_non_exhaustive()
    }
}

impl McpToolset {
    /// Spawns a stdio server and connects to it. `command` is spawned as given (see
    /// [`DirectCommand::command`], or the sandbox trampoline's command from
    /// [`ToolsetLauncher`](crate::toolsets::ToolsetLauncher)), with
    /// stdin, stdout, and stderr replaced by pipes. Returns once the server has answered
    /// `initialize`.
    ///
    /// `secrets` are the values the server was given (its `env`, see
    /// [`DirectCommand::secrets`]). They are scrubbed from its stderr, which is logged line by
    /// line under the `mcp` span with target [`STDERR_TARGET`], and from error messages.
    ///
    /// Failing to spawn is [`Error::Unavailable`], and so is the server exiting or not answering
    /// within [`INIT_TIMEOUT`]; the message then includes the exit status if the server exited.
    /// With `trampoline` (`command` runs the sandbox trampoline), an exit that
    /// [`sandbox::is_trampoline_failure`] recognizes is [`Error::InvalidRequest`] instead:
    /// retrying won't help.
    ///
    /// `media` says where media in tool results goes.
    pub async fn connect_stdio(
        instance: impl Into<String>,
        command: tokio::process::Command,
        secrets: Vec<Secret>,
        trampoline: bool,
        media: McpMedia,
    ) -> Result<Self> {
        let origin = Arc::new(Origin {
            instance: instance.into(),
            secrets,
        });
        let span = span(&origin.instance);
        let program = command
            .as_std()
            .get_program()
            .to_string_lossy()
            .into_owned();
        let spawned =
            stdio::spawn(command, &span, origin.secrets.clone().into()).map_err(|error| {
                Error::unavailable(origin.message(format_args!("can't start {program}: {error}")))
            })?;
        let child = Child {
            guard: spawned.guard,
            exit: spawned.exit,
            trampoline,
        };
        let io = (spawned.stdout, spawned.stdin);
        connect(origin, span, io, Some(child), media).await
    }

    /// Connects to a remote server over streamable HTTP, with the server's bearer token if it has
    /// one; the token is scrubbed from error messages. Returns once the server has answered
    /// `initialize`. Media in tool results is stored in `blobs`.
    ///
    /// A non-HTTP url and rejected credentials (HTTP 401 or 403) are [`Error::InvalidRequest`]; an
    /// unreachable server is [`Error::Unavailable`].
    pub async fn connect_http(
        instance: impl Into<String>,
        server: &HttpServer,
        blobs: giles_store::BlobStore,
    ) -> Result<Self> {
        let origin = Arc::new(Origin {
            instance: instance.into(),
            secrets: server.secrets(),
        });
        let span = span(&origin.instance);
        let transport = server.transport(&origin)?;
        connect(origin, span, transport, None, McpMedia::new(blobs)).await
    }

    /// Connects over an already open byte stream pair carrying newline-delimited JSON-RPC, as
    /// stdio does. For servers in the same process (tests, embedded servers). `media` says where
    /// media in tool results goes.
    pub async fn connect_io<R, W>(
        instance: impl Into<String>,
        read: R,
        write: W,
        media: McpMedia,
    ) -> Result<Self>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let origin = Arc::new(Origin {
            instance: instance.into(),
            secrets: Vec::new(),
        });
        let span = span(&origin.instance);
        connect(origin, span, (read, write), None, media).await
    }

    /// The instance name, for messages; tool names are bare.
    pub fn instance(&self) -> &str {
        &self.origin.instance
    }

    /// The version the server reported in `initialize` (`serverInfo.version`), part of the
    /// tool-list cache key (PLAN section 7.9).
    pub fn server_version(&self) -> Option<String> {
        let info = self.peer().peer_info()?;
        info.server_info
            .as_ref()
            .map(|server| server.version.clone())
    }

    /// Resolves once the connection has ended: the server hung up or exited, or the transport
    /// failed. For a stdio server, returns its exit status if it exited (waiting up to two seconds
    /// for it after the connection closed), so a supervisor can tell a sandbox trampoline failure
    /// (exit 125, 126, 127) from a crash. `None` for other transports, or a server still running.
    pub async fn closed(&self) -> Option<ExitStatus> {
        let mut closed = self.closed.clone();
        let Some(exit) = &self.exit else {
            let _ = closed.wait_for(|closed| *closed).await;
            return None;
        };
        let mut exited = exit.clone();
        tokio::select! {
            _ = closed.wait_for(|closed| *closed) => {}
            _ = exited.wait_for(Option::is_some) => {}
        }
        exit_status(exit, EXIT_GRACE).await
    }

    fn peer(&self) -> &Peer<RoleClient> {
        self.service.peer()
    }
}

/// A spawned stdio server.
struct Child {
    guard: stdio::ChildGuard,
    exit: watch::Receiver<Option<ExitStatus>>,
    /// Launched through the sandbox trampoline, whose failures exit with a known status.
    trampoline: bool,
}

/// The exit status, waiting up to `grace` for it.
async fn exit_status(
    exit: &watch::Receiver<Option<ExitStatus>>,
    grace: Duration,
) -> Option<ExitStatus> {
    let mut exit = exit.clone();
    let status = tokio::time::timeout(grace, exit.wait_for(Option::is_some)).await;
    status.ok()?.ok().and_then(|status| *status)
}

fn span(instance: &str) -> tracing::Span {
    tracing::info_span!("mcp", instance = %instance)
}

async fn connect<T, E, A>(
    origin: Arc<Origin>,
    span: tracing::Span,
    transport: T,
    child: Option<Child>,
    media: McpMedia,
) -> Result<McpToolset>
where
    T: IntoTransport<RoleClient, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    let (closed_tx, closed) = watch::channel(false);
    let transport = Watched {
        inner: transport.into_transport(),
        closed: closed_tx,
    };
    let state = ClientState::new(origin.instance.clone());
    let handshake = serve_client(state, transport).instrument(span.clone());
    let failure = match tokio::time::timeout(INIT_TIMEOUT, handshake).await {
        Ok(Ok(service)) => {
            if let Some(info) = service.peer_info() {
                tracing::info!(
                    parent: &span,
                    protocol = %info.protocol_version,
                    server = info.server_info.as_ref().map(|s| format!("{} {}", s.name, s.version)),
                    "connected to MCP server"
                );
            }
            let (exit, guard) = child.map(|child| (child.exit, child.guard)).unzip();
            return Ok(McpToolset {
                origin,
                service,
                closed,
                media,
                exit,
                _child: guard,
            });
        }
        Ok(Err(error)) => (convert::init_error(&origin, error), EXIT_GRACE),
        Err(_) => (
            Error::unavailable(origin.message(format_args!(
                "no answer to initialize within {INIT_TIMEOUT:?}"
            ))),
            Duration::ZERO,
        ),
    };
    let (error, grace) = failure;
    let Some(child) = &child else {
        return Err(error);
    };
    let (
        Some(status),
        Error::Unavailable {
            message,
            retry_after,
        },
    ) = (exit_status(&child.exit, grace).await, &error)
    else {
        return Err(error);
    };
    let message = format!("{message} (server process: {status})");
    Err(
        if child.trampoline && sandbox::is_trampoline_failure(status) {
            // The sandbox trampoline failed: retrying won't help.
            Error::InvalidRequest(message)
        } else {
            Error::Unavailable {
                message,
                retry_after: *retry_after,
            }
        },
    )
}

/// A transport that reports when the connection ends: the server hung up (`receive` returns
/// `None`) or the service loop dropped the transport.
struct Watched<T> {
    inner: T,
    closed: watch::Sender<bool>,
}

impl<T: Transport<RoleClient>> Transport<RoleClient> for Watched<T> {
    type Error = T::Error;

    fn name() -> Cow<'static, str> {
        T::name()
    }

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = std::result::Result<(), Self::Error>> + Send + 'static {
        self.inner.send(item)
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        let message = self.inner.receive().await;
        if message.is_none() {
            self.closed.send_replace(true);
        }
        message
    }

    fn close(&mut self) -> impl Future<Output = std::result::Result<(), Self::Error>> + Send {
        self.inner.close()
    }
}

impl<T> Drop for Watched<T> {
    fn drop(&mut self) {
        self.closed.send_replace(true);
    }
}

/// The whole tool list, following `nextCursor` through every page.
async fn list_all(origin: &Origin, peer: &Peer<RoleClient>) -> Result<Vec<ToolSpec>> {
    let mut tools = Vec::new();
    let mut cursor = None;
    for _ in 0..MAX_TOOL_PAGES {
        let page = peer
            .list_tools(Some(PaginatedRequestParams::default().with_cursor(cursor)))
            .await
            .map_err(|error| convert::service_error(origin, error))?;
        tools.extend(page.tools);
        cursor = page.next_cursor;
        if cursor.is_none() {
            return Ok(convert::tool_specs(tools));
        }
    }
    Err(Error::other(origin.message(format_args!(
        "tools/list returned more than {MAX_TOOL_PAGES} pages"
    ))))
}

#[async_trait]
impl Toolset for McpToolset {
    async fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        list_all(&self.origin, self.peer()).await
    }

    /// Present if the server declared `tools.listChanged`. Each `notifications/tools/list_changed`
    /// re-lists the tools and yields the new list; changes that arrive while a list is being read
    /// are coalesced into one more read. A re-list that fails is logged and skipped, unless the
    /// connection is gone, which ends the stream. The stream also ends when the toolset is
    /// dropped.
    fn tool_changes(&self) -> Option<BoxStream<'static, Vec<ToolSpec>>> {
        let declared = self
            .peer()
            .peer_info()
            .and_then(|info| info.capabilities.tools.as_ref()?.list_changed)
            .unwrap_or(false);
        if !declared {
            return None;
        }
        let changed = self.service.service().tools_changed.subscribe();
        let state = (changed, self.peer().clone(), self.origin.clone());
        let stream = futures::stream::unfold(state, |(mut changed, peer, origin)| async move {
            loop {
                changed.changed().await.ok()?;
                match list_all(&origin, &peer).await {
                    Ok(tools) => return Some((tools, (changed, peer, origin))),
                    Err(error) if error.is_retryable() => {
                        tracing::warn!(%error, "re-listing MCP tools failed");
                        return None;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "re-listing MCP tools failed");
                    }
                }
            }
        });
        Some(stream.boxed())
    }

    /// Toolset skill directories are task 2.21; until then an MCP toolset has no skills.
    async fn list_skills(&self) -> Result<Vec<SkillSummary>> {
        Ok(Vec::new())
    }

    /// Always [`Error::NotFound`] until task 2.21.
    async fn get_skill(&self, name: &str) -> Result<Skill> {
        Err(Error::NotFound(format!(
            "skill {name}: MCP toolset {} has no skills",
            self.origin.instance
        )))
    }

    /// Sends `tools/call` with a progress token and returns at once. The events are the server's
    /// progress notifications, then the result, its images, audio, and blob resources stored in
    /// the blob store as media blocks ([`MAX_RESULT_MEDIA_BYTES`] each); a JSON-RPC error
    /// response or a lost connection is the stream's last item, as an error. Dropping `events`
    /// before the result sends `notifications/cancelled` for the request.
    ///
    /// `answers` takes no answers yet: elicitation is task 2.21, and until then the server's
    /// requests are declined. `call.media` isn't handed to the server yet either (2.21); the input
    /// is sent as is.
    async fn call(&self, call: ToolCall, _ctx: CallContext) -> Result<ToolCallHandle> {
        let parent = span(&self.origin.instance);
        let span = tracing::debug_span!(
            parent: &parent,
            "tools/call",
            tool = %call.name,
            id = %call.id
        );
        let mut params = CallToolRequestParams::new(call.name);
        match call.input {
            serde_json::Value::Object(arguments) => params = params.with_arguments(arguments),
            serde_json::Value::Null => {}
            _ => {
                return Err(Error::InvalidRequest(
                    "tool input must be a JSON object".to_owned(),
                ));
            }
        }
        let request = ClientRequest::CallToolRequest(Request::new(params));
        let handle = self
            .peer()
            .send_cancellable_request(request, PeerRequestOptions::no_options())
            .await
            .map_err(|error| convert::service_error(&self.origin, error))?;
        let progress = self
            .service
            .service()
            .progress
            .subscribe(handle.progress_token.clone());
        let (events, events_rx) = mpsc::channel(CALL_EVENT_BUFFER);
        let (answers, answers_rx) = mpsc::channel(1);
        tokio::spawn(
            run_call(
                self.origin.clone(),
                self.media.clone(),
                handle,
                progress,
                events,
                answers_rx,
            )
            .instrument(span),
        );
        let events = futures::stream::unfold(events_rx, |mut events| async move {
            events.recv().await.map(|event| (event, events))
        });
        Ok(ToolCallHandle {
            events: events.boxed(),
            answers,
        })
    }
}

/// Drives one call: forwards progress, then the result, and cancels the request if the consumer
/// drops the events first.
async fn run_call(
    origin: Arc<Origin>,
    media: McpMedia,
    handle: RequestHandle<RoleClient>,
    mut progress: ProgressSubscription,
    events: mpsc::Sender<Result<ToolCallEvent>>,
    // Held so the caller's sends succeed while the call runs; nothing reads it until 2.21.
    _answers: mpsc::Receiver<ApprovalAnswer>,
) {
    let id = handle.id.clone();
    let peer = handle.peer.clone();
    let response = handle.await_response();
    tokio::pin!(response);
    let mut last = None;
    let response = loop {
        tokio::select! {
            biased;
            () = events.closed() => {
                tracing::debug!("call dropped; cancelling it");
                let reason = "the caller stopped waiting".to_owned();
                let cancelled = CancelledNotificationParam::new(Some(id), Some(reason));
                if let Err(error) = peer.notify_cancelled(cancelled).await {
                    tracing::debug!(%error, "sending notifications/cancelled failed");
                }
                return;
            }
            response = &mut response => break response,
            Some(params) = progress.receiver.recv() => {
                if let Some(event) = convert::progress_event(params, &mut last) {
                    // A closed channel is noticed on the next turn.
                    let _ = events.send(Ok(event)).await;
                }
            }
        }
    };
    // Progress already routed when the response came in still goes first.
    while let Ok(params) = progress.receiver.try_recv() {
        if let Some(event) = convert::progress_event(params, &mut last) {
            let _ = events.send(Ok(event)).await;
        }
    }
    let event = match response {
        Ok(ServerResult::CallToolResult(result)) => {
            let result = convert::tool_result(result);
            Ok(media::store_result(result, &media.blobs, MAX_RESULT_MEDIA_BYTES).await)
        }
        Ok(_) => Err(Error::other(
            origin.message("unexpected response to tools/call"),
        )),
        Err(error) => Err(convert::service_error(&origin, error)),
    };
    let _ = events.send(event).await;
}

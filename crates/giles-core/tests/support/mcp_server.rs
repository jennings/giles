//! A small MCP server for the MCP client tests, written with `rmcp`. Used in process over a duplex
//! pipe, as a stdio child process, and behind streamable HTTP.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ElicitRequestParams,
    ErrorData, Implementation, JsonObject, ListToolsResult, PaginatedRequestParams,
    ProgressNotificationParam, ResourceContents, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use serde_json::{Value, json};
use tokio::sync::{Notify, Semaphore};

/// Tools per `tools/list` page, small so listing takes several pages.
pub const PAGE_SIZE: usize = 2;

/// The tools every server starts with, sorted.
pub const TOOLS: [&str; 11] = [
    "add_tool",
    "ask",
    "cancellations",
    "echo",
    "exit",
    "fail",
    "hang",
    "media",
    "pid",
    "progress_steps",
    "read_file",
];

/// What the `media` tool's image, audio, and resource carry, base64: `hello`.
pub const MEDIA_BASE64: &str = "aGVsbG8=";

/// What the tests observe and steer.
pub struct Shared {
    /// Tools added by `add_tool`.
    pub extra_tools: Mutex<Vec<String>>,
    /// Calls of `hang` that saw their cancellation.
    pub cancelled: Mutex<usize>,
    /// Notified when a `hang` call is cancelled.
    pub cancelled_notify: Notify,
    /// A gated `progress_steps` takes one permit before each step after the first, and before the
    /// result.
    pub gate: Semaphore,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            extra_tools: Mutex::default(),
            cancelled: Mutex::default(),
            cancelled_notify: Notify::new(),
            gate: Semaphore::new(0),
        }
    }
}

/// The server. Clones share state, so a streamable HTTP service factory can hand out one per
/// session.
#[derive(Clone, Default)]
pub struct TestServer {
    pub shared: Arc<Shared>,
    /// Leaves `tools.listChanged` undeclared.
    pub without_list_changed: bool,
}

fn tool(name: &str, description: &str, schema: Value) -> Tool {
    let Value::Object(schema) = schema else {
        panic!("schema must be an object");
    };
    Tool::new(name.to_owned(), description.to_owned(), schema)
}

fn object() -> Value {
    json!({"type": "object"})
}

fn text(text: impl Into<String>) -> CallToolResponse {
    CallToolResult::success(vec![ContentBlock::text(text)]).into()
}

impl TestServer {
    fn tools(&self) -> Vec<Tool> {
        let mut tools = vec![
            tool(
                "echo",
                "Echoes its input",
                json!({"type": "object", "properties": {"text": {"type": "string"}}}),
            ),
            tool("add_tool", "Adds a tool and announces it", object()),
            tool("ask", "Asks the user via elicitation", object()),
            tool(
                "cancellations",
                "How many hang calls were cancelled",
                object(),
            ),
            tool("exit", "Exits the process with `code`", object()),
            tool("fail", "Fails", object()),
            tool(
                "hang",
                "Reports progress once, then waits to be cancelled",
                object(),
            ),
            tool(
                "media",
                "Returns an image, audio, and a PDF resource",
                object(),
            ),
            tool("pid", "The server's process id", object()),
            tool("progress_steps", "Reports `steps` progress steps", object()),
            tool(
                "read_file",
                "Reads the file at `path`: `<path>` and `<contents>` lines",
                object(),
            ),
        ];
        let extra = self.shared.extra_tools.lock().unwrap();
        tools.extend(
            extra
                .iter()
                .map(|name| tool(name, "Added at runtime", object())),
        );
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        tools
    }

    async fn progress(context: &RequestContext<RoleServer>, step: f64, total: Option<f64>) {
        let token = context
            .meta
            .get_progress_token()
            .expect("the client sends a progress token");
        let mut params =
            ProgressNotificationParam::new(token, step).with_message(format!("step {step}"));
        params.total = total;
        context.peer.notify_progress(params).await.unwrap();
    }
}

impl ServerHandler for TestServer {
    fn get_info(&self) -> ServerConfig {
        let capabilities = if self.without_list_changed {
            ServerCapabilities::builder().enable_tools().build()
        } else {
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build()
        };
        ServerConfig::new(capabilities)
            .with_server_info(Implementation::new("giles-test-server", "0.0.0"))
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let start: usize = match request.and_then(|request| request.cursor) {
            Some(cursor) => cursor
                .parse()
                .map_err(|_| ErrorData::invalid_params("bad cursor", None))?,
            None => 0,
        };
        let tools = self.tools();
        let end = (start + PAGE_SIZE).min(tools.len());
        let mut page = ListToolsResult::with_all_items(tools[start..end].to_vec());
        page.next_cursor = (end < tools.len()).then(|| end.to_string());
        Ok(page)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let arguments = request.arguments.unwrap_or_default();
        let argument = |name: &str| arguments.get(name).cloned().unwrap_or(Value::Null);
        match request.name.as_ref() {
            "echo" => Ok(text(argument("text").as_str().unwrap_or_default())),
            "add_tool" => {
                let name = argument("name").as_str().unwrap_or("extra").to_owned();
                self.shared.extra_tools.lock().unwrap().push(name);
                context.peer.notify_tool_list_changed().await.unwrap();
                Ok(text("added"))
            }
            "ask" => {
                let params = ElicitRequestParams::FormElicitationParams {
                    meta: None,
                    message: "Your name?".to_owned(),
                    requested_schema: serde_json::from_value(json!({
                        "type": "object",
                        "properties": {"name": {"type": "string"}},
                    }))
                    .unwrap(),
                };
                match context.peer.create_elicitation(params).await {
                    Ok(result) => Ok(text(format!("{:?}", result.action))),
                    Err(error) => Ok(text(format!("elicitation failed: {error}"))),
                }
            }
            "cancellations" => Ok(text(self.shared.cancelled.lock().unwrap().to_string())),
            "exit" => std::process::exit(argument("code").as_i64().unwrap_or(1) as i32),
            "fail" => Ok(CallToolResult::error(vec![ContentBlock::text("boom")]).into()),
            "hang" => {
                Self::progress(&context, 1.0, None).await;
                context.ct.cancelled().await;
                *self.shared.cancelled.lock().unwrap() += 1;
                self.shared.cancelled_notify.notify_waiters();
                Err(ErrorData::internal_error("cancelled", None))
            }
            "media" => Ok(CallToolResult::success(vec![
                ContentBlock::text("Here they are."),
                ContentBlock::image(MEDIA_BASE64, "image/png"),
                ContentBlock::audio(MEDIA_BASE64, "audio/ogg"),
                ContentBlock::resource(ResourceContents::BlobResourceContents {
                    uri: "file:///reports/q3.pdf".to_owned(),
                    mime_type: Some("application/pdf".to_owned()),
                    blob: MEDIA_BASE64.to_owned(),
                    meta: None,
                }),
            ])
            .into()),
            "pid" => Ok(text(std::process::id().to_string())),
            "read_file" => {
                let path = argument("path").as_str().unwrap_or_default().to_owned();
                Ok(match std::fs::read_to_string(&path) {
                    Ok(contents) => text(format!("{path}\n{contents}")),
                    Err(error) => {
                        CallToolResult::error(vec![ContentBlock::text(error.to_string())]).into()
                    }
                })
            }
            "progress_steps" => {
                let steps = argument("steps").as_u64().unwrap_or(2);
                let gated = argument("gated").as_bool().unwrap_or(false);
                for step in 1..=steps {
                    if gated && step > 1 {
                        self.shared.gate.acquire().await.unwrap().forget();
                    }
                    Self::progress(&context, step as f64, Some(steps as f64)).await;
                }
                if gated {
                    self.shared.gate.acquire().await.unwrap().forget();
                }
                Ok(text("done"))
            }
            name => Err(ErrorData::invalid_params(
                format!("Unknown tool: {name}"),
                None,
            )),
        }
    }
}

/// An empty JSON object, for tool inputs.
pub fn no_input() -> Value {
    Value::Object(JsonObject::new())
}

/// A blob store in a fresh temporary directory, which must outlive it.
pub async fn blob_store() -> (tempfile::TempDir, giles_store::BlobStore) {
    let dir = tempfile::tempdir().unwrap();
    let db = giles_store::Store::open(dir.path().join("giles.db"))
        .await
        .unwrap();
    let blobs = giles_store::BlobStore::open(dir.path().join("blobs"), db)
        .await
        .unwrap();
    (dir, blobs)
}

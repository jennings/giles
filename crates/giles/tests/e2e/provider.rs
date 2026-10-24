//! A scriptable model provider: a loopback server speaking the OpenAI Chat Completions streaming
//! protocol, which the daemon reaches through its real `openai` provider (`base_url` in the
//! instance's config). Nothing in the daemon knows it is fake.
//!
//! Each test queues the replies it wants, one per `POST /v1/chat/completions`, and reads back the
//! requests the daemon sent. A request with nothing queued is answered with a 400 (which fails
//! the turn visibly) and counted, so [`FakeProvider::assert_no_surprises`] can fail the test.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::harness::WAIT;

/// The one model the provider lists; agents use `fake/test-model`.
pub const MODEL: &str = "test-model";

/// The longest a held response lingers if nothing ends it, so a failed test leaks no thread.
const HOLD_LIMIT: Duration = Duration::from_secs(120);

/// One tool call in a reply.
#[derive(Debug, Clone)]
pub struct Call {
    id: String,
    name: String,
    arguments: Value,
}

/// What the provider answers to one request.
#[derive(Debug, Clone)]
pub enum Reply {
    /// A text answer that ends the turn.
    Text(String),
    /// Optional text, then tool calls.
    Tools(Option<String>, Vec<Call>),
    /// Starts streaming and then goes silent until the client hangs up: a turn in flight.
    Hold,
}

impl Reply {
    /// A text answer.
    pub fn text(text: &str) -> Self {
        Self::Text(text.to_owned())
    }

    /// One tool call, `arguments` being the tool's input.
    pub fn call(id: &str, name: &str, arguments: Value) -> Self {
        Self::Tools(
            None,
            vec![Call {
                id: id.to_owned(),
                name: name.to_owned(),
                arguments,
            }],
        )
    }
}

#[derive(Default)]
struct State {
    script: VecDeque<Reply>,
    requests: Vec<Value>,
    unscripted: usize,
    hung_up: usize,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    closed: AtomicBool,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }
}

/// The running fake provider. The server stops when this is dropped.
pub struct FakeProvider {
    addr: SocketAddr,
    shared: Arc<Shared>,
}

impl FakeProvider {
    /// Starts listening on a free loopback port.
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            closed: AtomicBool::new(false),
        });
        let accepting = shared.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                if accepting.closed.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let connection = accepting.clone();
                thread::spawn(move || serve(stream, &connection));
            }
        });
        Self { addr, shared }
    }

    /// The `base_url` for the daemon's provider config.
    pub fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    /// Queues the replies for the next requests, in order.
    pub fn script(&self, replies: impl IntoIterator<Item = Reply>) {
        self.shared.lock().script.extend(replies);
    }

    /// Every completion request received so far, as the JSON bodies the daemon sent.
    pub fn requests(&self) -> Vec<Value> {
        self.shared.lock().requests.clone()
    }

    /// Waits until `count` completion requests have arrived; returns them all.
    pub fn wait_requests(&self, count: usize) -> Vec<Value> {
        self.wait(&format!("{count} provider requests"), |state| {
            (state.requests.len() >= count).then(|| state.requests.clone())
        })
    }

    /// Waits until `count` held responses have been abandoned by the client (the daemon dropped
    /// the request, as it does for `/stop`).
    pub fn wait_hang_ups(&self, count: usize) {
        self.wait(&format!("{count} hung-up requests"), |state| {
            (state.hung_up >= count).then_some(())
        });
    }

    /// Fails the test if a request arrived with nothing scripted, or a reply is left unused.
    pub fn assert_no_surprises(&self) {
        let state = self.shared.lock();
        assert_eq!(
            state.unscripted, 0,
            "requests arrived with no scripted reply"
        );
        assert!(
            state.script.is_empty(),
            "scripted replies never requested: {:?}",
            state.script
        );
    }

    fn wait<T>(&self, what: &str, mut ready: impl FnMut(&State) -> Option<T>) -> T {
        let deadline = Instant::now() + WAIT;
        let mut state = self.shared.lock();
        loop {
            if let Some(value) = ready(&state) {
                return value;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "timed out waiting for {what}");
            state = self.shared.changed.wait_timeout(state, left).unwrap().0;
        }
    }
}

impl Drop for FakeProvider {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::SeqCst);
        // Wakes the accept loop so it sees the flag.
        let _ = TcpStream::connect(self.addr);
    }
}

/// The `messages` of a completion request.
pub fn messages(request: &Value) -> &[Value] {
    request["messages"].as_array().unwrap()
}

/// The text of one request message (its string content, or its text parts joined).
pub fn text_of(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The result the model was given for tool call `id` in `request`.
pub fn tool_result(request: &Value, id: &str) -> String {
    messages(request)
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == id)
        .map(text_of)
        .unwrap_or_else(|| panic!("no result for {id} in {:#}", request["messages"]))
}

/// The messages of `role` in `request`, as text.
pub fn texts_with_role(request: &Value, role: &str) -> Vec<String> {
    messages(request)
        .iter()
        .filter(|message| message["role"] == role)
        .map(text_of)
        .collect()
}

/// Answers one connection: the model list, or a scripted completion.
fn serve(mut stream: TcpStream, shared: &Shared) {
    stream.set_nodelay(true).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let mut length = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap();
        }
    }
    let mut body = vec![0; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }

    if request_line.starts_with("GET /v1/models") {
        let models = json!({"object": "list", "data": [{"id": MODEL, "object": "model"}]});
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{models}",
            models.to_string().len()
        );
        return;
    }

    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let reply = {
        let mut state = shared.lock();
        state.requests.push(request);
        let reply = state.script.pop_front();
        if reply.is_none() {
            state.unscripted += 1;
        }
        shared.changed.notify_all();
        reply
    };
    let Some(reply) = reply else {
        let error = r#"{"error":{"message":"no reply scripted","type":"invalid_request_error"}}"#;
        let _ = write!(
            stream,
            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{error}",
            error.len()
        );
        return;
    };

    // The body has no length: it ends when the connection closes.
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n"
    );
    let _ = stream.write_all(chunk(json!({"role": "assistant", "content": ""}), None).as_bytes());
    match reply {
        Reply::Text(text) => {
            let _ = stream.write_all(chunk(json!({"content": text}), None).as_bytes());
            let _ = stream.write_all(chunk(json!({}), Some("stop")).as_bytes());
        }
        Reply::Tools(text, calls) => {
            if let Some(text) = text {
                let _ = stream.write_all(chunk(json!({"content": text}), None).as_bytes());
            }
            for (index, call) in calls.iter().enumerate() {
                // The id and name first, then the arguments in two pieces, as real servers do.
                let arguments = call.arguments.to_string();
                let (first, second) = arguments.split_at(arguments.len() / 2);
                for (id_and_name, arguments) in [(true, ""), (false, first), (false, second)] {
                    let mut function = json!({"arguments": arguments});
                    let mut entry = json!({"index": index});
                    if id_and_name {
                        function["name"] = json!(call.name);
                        entry["id"] = json!(call.id);
                        entry["type"] = json!("function");
                    }
                    entry["function"] = function;
                    let _ =
                        stream.write_all(chunk(json!({"tool_calls": [entry]}), None).as_bytes());
                }
            }
            let _ = stream.write_all(chunk(json!({}), Some("tool_calls")).as_bytes());
        }
        Reply::Hold => {
            hold(&mut stream, shared);
            return;
        }
    }
    let _ = stream.write_all(b"data: [DONE]\n\n");
    let _ = stream.shutdown(Shutdown::Both);
}

/// Waits for the client to hang up on a response that never finishes.
fn hold(stream: &mut TcpStream, shared: &Shared) {
    stream
        .set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    let deadline = Instant::now() + HOLD_LIMIT;
    let mut buf = [0; 64];
    while Instant::now() < deadline && !shared.closed.load(Ordering::SeqCst) {
        match stream.read(&mut buf) {
            Ok(1..) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            // End of stream or a reset: the daemon dropped the request.
            Ok(0) | Err(_) => {
                shared.lock().hung_up += 1;
                shared.changed.notify_all();
                return;
            }
        }
    }
}

/// One server-sent event: a `chat.completion.chunk` with `delta`.
fn chunk(delta: Value, finish: Option<&str>) -> String {
    let body = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    });
    format!("data: {body}\n\n")
}

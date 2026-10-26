//! A REST server of the test's own on loopback, plain HTTP/1.1, playing Discord from its
//! documentation: it records every request and answers each from a function the test gives it.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// A request the client made.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    /// The path and query, as sent (`/api/v10/channels/1/messages?after=2&limit=100`).
    pub target: String,
    /// Header names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The value of a query parameter.
    pub fn query(&self, name: &str) -> Option<&str> {
        let (_, query) = self.target.split_once('?')?;
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
    }
}

/// What the server answers.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Response {
    pub fn json(status: u16, body: &serde_json::Value) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.to_string(),
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

/// Answers the nth request (from 0) of the server.
pub type Handler = dyn Fn(&Request, usize) -> Response + Send + Sync;

/// A REST server on loopback.
pub struct RestServer {
    /// `127.0.0.1:<port>`: what `RealDiscord::with_urls` takes.
    pub host: String,
    requests: Arc<Mutex<Vec<Request>>>,
    task: JoinHandle<()>,
}

impl RestServer {
    pub async fn start(
        handler: impl Fn(&Request, usize) -> Response + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let handler: Arc<Handler> = Arc::new(handler);
        let seen = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(serve(tcp, Arc::clone(&handler), Arc::clone(&seen)));
            }
        });
        Self {
            host,
            requests,
            task,
        }
    }

    /// The requests so far.
    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for RestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// One connection: requests and answers until the client hangs up.
async fn serve(mut tcp: TcpStream, handler: Arc<Handler>, seen: Arc<Mutex<Vec<Request>>>) {
    let mut buffer = Vec::new();
    loop {
        let Some(request) = read_request(&mut tcp, &mut buffer).await else {
            return;
        };
        let response = {
            let mut seen = seen.lock().unwrap();
            seen.push(request.clone());
            handler(&request, seen.len() - 1)
        };
        let mut head = format!(
            "HTTP/1.1 {} Test\r\ncontent-length: {}\r\n",
            response.status,
            response.body.len()
        );
        for (name, value) in &response.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        if tcp.write_all(head.as_bytes()).await.is_err()
            || tcp.write_all(response.body.as_bytes()).await.is_err()
        {
            return;
        }
    }
}

/// Reads one request from `tcp`, keeping what follows it in `buffer`.
async fn read_request(tcp: &mut TcpStream, buffer: &mut Vec<u8>) -> Option<Request> {
    let head_end = loop {
        if let Some(at) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break at + 4;
        }
        let mut chunk = [0_u8; 4096];
        let n = tcp.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut start = lines.next()?.split(' ');
    let method = start.next()?.to_owned();
    let target = start.next()?.to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    while buffer.len() < head_end + length {
        let mut chunk = [0_u8; 4096];
        let n = tcp.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..n]);
    }
    let body = buffer[head_end..head_end + length].to_vec();
    buffer.drain(..head_end + length);
    Some(Request {
        method,
        target,
        headers,
        body,
    })
}

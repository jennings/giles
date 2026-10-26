//! `RealDiscord::download` against a CDN of the test's own on loopback (plain HTTP, which only a
//! test policy allows): what it asks for, what it refuses to ask for, how it follows redirects,
//! and how it ends a body that is too long, cut short, or too slow. Nothing here reaches Discord.

mod loopback;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt as _;
use giles_channel_discord::client::{Body, Discord, RestError, limited};
use giles_channel_discord::real::{CdnPolicy, MAX_REDIRECTS, RealDiscord};
use loopback::{TOKEN, WAIT, rest_client};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

const MIB: u64 = 1024 * 1024;

/// A request the CDN got.
#[derive(Debug, Clone)]
struct Hit {
    /// Path and query, as sent.
    target: String,
    /// Header names in lower case.
    headers: Vec<(String, String)>,
}

impl Hit {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// What the CDN does with a request.
#[derive(Clone)]
enum Script {
    /// 200 with `body`, its length announced.
    Ok(Vec<u8>),
    /// A status and no body.
    Status(u16),
    /// 302 to `location`.
    Redirect(String),
    /// 200 announcing `declared` bytes, sending `sent` of them, and closing.
    Truncated { declared: usize, sent: Vec<u8> },
    /// 200 announcing `declared` bytes and then nothing at all.
    AnnouncesOnly { declared: u64 },
    /// 200 chunked, with chunks of `chunk` bytes for as long as the client reads; `written`
    /// counts the bytes put on the wire.
    Endless {
        chunk: usize,
        written: Arc<AtomicUsize>,
    },
    /// Accepts the connection and never answers.
    Silent,
    /// 200 chunked: `first`, then nothing.
    Stalls { first: Vec<u8> },
    /// 200 chunked, a byte every `every`, for ever.
    Drips { every: Duration },
}

struct Cdn {
    addr: SocketAddr,
    hits: Arc<Mutex<Vec<Hit>>>,
    task: JoinHandle<()>,
}

impl Cdn {
    /// A CDN that answers a request for path `p` as `route(p)` says.
    async fn start(route: impl Fn(&str) -> Script + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(Mutex::new(Vec::new()));
        let route: Arc<dyn Fn(&str) -> Script + Send + Sync> = Arc::new(route);
        let seen = Arc::clone(&hits);
        let task = tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(serve(tcp, Arc::clone(&route), Arc::clone(&seen)));
            }
        });
        Self { addr, hits, task }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    fn hits(&self) -> Vec<Hit> {
        self.hits.lock().unwrap().clone()
    }
}

impl Drop for Cdn {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(
    mut tcp: TcpStream,
    route: Arc<dyn Fn(&str) -> Script + Send + Sync>,
    hits: Arc<Mutex<Vec<Hit>>>,
) {
    let mut buffer = Vec::new();
    loop {
        let mut chunk = [0_u8; 4096];
        match tcp.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
        }
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buffer).into_owned();
    let mut lines = head.split("\r\n");
    let target = lines.next().unwrap().split(' ').nth(1).unwrap().to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    hits.lock().unwrap().push(Hit {
        target: target.clone(),
        headers,
    });
    let path = target.split('?').next().unwrap().to_owned();
    match route(&path) {
        Script::Ok(body) => {
            let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body.len());
            let _ = tcp.write_all(head.as_bytes()).await;
            let _ = tcp.write_all(&body).await;
        }
        Script::Status(status) => {
            let head = format!("HTTP/1.1 {status} Test\r\ncontent-length: 0\r\n\r\n");
            let _ = tcp.write_all(head.as_bytes()).await;
        }
        Script::Redirect(location) => {
            let head =
                format!("HTTP/1.1 302 Found\r\nlocation: {location}\r\ncontent-length: 0\r\n\r\n");
            let _ = tcp.write_all(head.as_bytes()).await;
        }
        Script::Truncated { declared, sent } => {
            let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {declared}\r\n\r\n");
            let _ = tcp.write_all(head.as_bytes()).await;
            let _ = tcp.write_all(&sent).await;
            // Closing with fewer bytes than announced.
        }
        Script::AnnouncesOnly { declared } => {
            let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {declared}\r\n\r\n");
            let _ = tcp.write_all(head.as_bytes()).await;
            std::future::pending::<()>().await;
        }
        Script::Endless { chunk, written } => {
            let head = "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n";
            if tcp.write_all(head.as_bytes()).await.is_err() {
                return;
            }
            let data = vec![b'z'; chunk];
            loop {
                let mut frame = format!("{chunk:x}\r\n").into_bytes();
                frame.extend_from_slice(&data);
                frame.extend_from_slice(b"\r\n");
                if tcp.write_all(&frame).await.is_err() {
                    return;
                }
                written.fetch_add(chunk, Ordering::SeqCst);
            }
        }
        Script::Silent => std::future::pending::<()>().await,
        Script::Stalls { first } => {
            let head = "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n";
            let _ = tcp.write_all(head.as_bytes()).await;
            let mut frame = format!("{:x}\r\n", first.len()).into_bytes();
            frame.extend_from_slice(&first);
            frame.extend_from_slice(b"\r\n");
            let _ = tcp.write_all(&frame).await;
            std::future::pending::<()>().await;
        }
        Script::Drips { every } => {
            let head = "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n";
            let _ = tcp.write_all(head.as_bytes()).await;
            loop {
                if tcp.write_all(b"1\r\nd\r\n").await.is_err() {
                    return;
                }
                tokio::time::sleep(every).await;
            }
        }
    }
}

/// A client that may download from `127.0.0.1` over plain HTTP, with short timeouts.
fn client() -> RealDiscord {
    client_with(CdnPolicy::loopback(&["127.0.0.1"]))
}

fn client_with(policy: CdnPolicy) -> RealDiscord {
    rest_client("127.0.0.1:9").with_cdn(policy)
}

/// A policy with the given timeouts.
fn timed(read: Duration, total: Duration) -> CdnPolicy {
    CdnPolicy {
        connect_timeout: Duration::from_secs(5),
        read_timeout: read,
        total_timeout: total,
        ..CdnPolicy::loopback(&["127.0.0.1"])
    }
}

async fn download(client: &RealDiscord, url: &str, max: u64) -> Result<Body, RestError> {
    tokio::time::timeout(WAIT, client.download(url, max))
        .await
        .expect("an answer in time")
}

/// Reads `body` to its end, as the plugin does: the bytes, and how it ended.
async fn read_all(body: Body) -> (Vec<u8>, Option<RestError>) {
    tokio::time::timeout(WAIT, async {
        let mut body = body;
        let mut bytes = Vec::new();
        while let Some(chunk) = body.next().await {
            match chunk {
                Ok(chunk) => bytes.extend_from_slice(&chunk),
                Err(error) => return (bytes, Some(error)),
            }
        }
        (bytes, None)
    })
    .await
    .expect("the body ends in time")
}

#[tokio::test]
async fn it_gets_the_file_with_no_credential_of_any_kind() {
    let cdn = Cdn::start(|_| Script::Ok(b"the file".to_vec())).await;
    let client = client();
    let url = cdn.url("/attachments/1/2/photo.png?ex=1&is=2&hm=signature");
    let body = download(&client, &url, MIB).await.unwrap();
    assert_eq!(read_all(body).await, (b"the file".to_vec(), None));

    let hits = cdn.hits();
    assert_eq!(hits.len(), 1);
    // The signed URL is requested as it is.
    assert_eq!(
        hits[0].target,
        "/attachments/1/2/photo.png?ex=1&is=2&hm=signature"
    );
    // The bot's token must never reach the CDN, in any header or the URL.
    for (name, value) in &hits[0].headers {
        assert!(!value.contains(TOKEN), "{name}: {value}");
    }
    for forbidden in ["authorization", "cookie", "referer", "proxy-authorization"] {
        assert_eq!(hits[0].header(forbidden), None, "{forbidden}");
    }
    assert!(
        hits[0]
            .header("user-agent")
            .unwrap()
            .starts_with("DiscordBot ("),
        "{:?}",
        hits[0].header("user-agent")
    );
}

#[tokio::test]
async fn it_never_connects_for_a_url_that_is_not_the_cdns() {
    let cdn = Cdn::start(|_| Script::Ok(b"secret".to_vec())).await;
    let port = cdn.addr.port();
    let client = client();
    for url in [
        // Another host, even one that is the same machine.
        format!("http://localhost:{port}/x"),
        format!("http://[::1]:{port}/x"),
        format!("http://user:pass@127.0.0.1:{port}/x"),
        "ftp://127.0.0.1/x".to_owned(),
        "file:///etc/passwd".to_owned(),
        "not a url".to_owned(),
        String::new(),
    ] {
        assert!(
            matches!(download(&client, &url, MIB).await, Err(RestError::BadUrl)),
            "{url}"
        );
    }
    assert!(cdn.hits().is_empty(), "nothing was requested");

    // Discord's own policy, which is what runs in production: https and the CDN's hosts only, so
    // not this server, not plain HTTP, not another port, not an address.
    let production = rest_client("127.0.0.1:9");
    for url in [
        format!("http://127.0.0.1:{port}/x"),
        format!("https://127.0.0.1:{port}/x"),
        "http://cdn.discordapp.com/attachments/1/2/a.png".to_owned(),
        "https://cdn.discordapp.com:8443/attachments/1/2/a.png".to_owned(),
        "https://cdn.discordapp.com.evil.example/a.png".to_owned(),
        "https://169.254.169.254/latest/meta-data/".to_owned(),
    ] {
        assert!(
            matches!(
                download(&production, &url, MIB).await,
                Err(RestError::BadUrl)
            ),
            "{url}"
        );
    }
    assert!(cdn.hits().is_empty());
}

#[tokio::test]
async fn a_redirect_inside_the_policy_is_followed() {
    let cdn = Cdn::start(|path| match path {
        "/old" => Script::Redirect("/new?ex=2".to_owned()),
        "/new" => Script::Ok(b"moved".to_vec()),
        _ => Script::Status(404),
    })
    .await;
    let body = download(&client(), &cdn.url("/old?ex=1"), MIB)
        .await
        .unwrap();
    assert_eq!(read_all(body).await.0, b"moved");
    let hits = cdn.hits();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[1].target, "/new?ex=2");
    assert_eq!(hits[1].header("referer"), None);
    assert_eq!(hits[1].header("authorization"), None);
}

#[tokio::test]
async fn a_redirect_to_another_host_is_not_followed() {
    let other = Cdn::start(|_| Script::Ok(b"internal secret".to_vec())).await;
    let elsewhere = format!("http://localhost:{}/metadata", other.addr.port());
    let cdn = Cdn::start(move |_| Script::Redirect(elsewhere.clone())).await;
    let result = download(&client(), &cdn.url("/a.png"), MIB).await;
    assert!(
        matches!(
            result,
            Err(RestError::Api {
                status: 302,
                code: None
            })
        ),
        "{:?}",
        result.err()
    );
    assert!(other.hits().is_empty(), "the other host was never asked");
    assert_eq!(cdn.hits().len(), 1);
}

#[tokio::test]
async fn redirects_are_followed_a_few_times_and_not_for_ever() {
    let cdn = Cdn::start(|path| {
        let n: usize = path.trim_start_matches("/r").parse().unwrap_or(0);
        Script::Redirect(format!("/r{}", n + 1))
    })
    .await;
    let result = download(&client(), &cdn.url("/r0"), MIB).await;
    assert!(
        matches!(result, Err(RestError::Transport)),
        "{:?}",
        result.err()
    );
    // The first request and exactly MAX_REDIRECTS redirects followed, then the next is refused.
    assert_eq!(cdn.hits().len(), MAX_REDIRECTS + 1);
}

#[tokio::test]
async fn an_error_status_is_the_cdns_status_and_no_body() {
    let cdn =
        Cdn::start(|path| Script::Status(path.trim_start_matches('/').parse().unwrap())).await;
    let client = client();
    for status in [403, 404, 410, 429, 500, 503] {
        let result = download(&client, &cdn.url(&format!("/{status}")), MIB).await;
        assert!(
            matches!(result, Err(RestError::Api { status: s, code: None }) if s == status),
            "{status}: {:?}",
            result.err()
        );
    }
}

#[tokio::test]
async fn a_declared_length_over_the_limit_is_refused_before_the_body() {
    // It announces 10 MiB and sends nothing: only the header could have been read.
    let cdn = Cdn::start(|_| Script::AnnouncesOnly { declared: 10 * MIB }).await;
    let result = download(&client(), &cdn.url("/big"), MIB).await;
    assert!(
        matches!(result, Err(RestError::TooLarge)),
        "{:?}",
        result.err()
    );
    // Exactly the limit is fine.
    let cdn = Cdn::start(|_| Script::Ok(vec![1; 1024])).await;
    let body = download(&client(), &cdn.url("/exact"), 1024).await.unwrap();
    assert_eq!(read_all(limited(body, 1024)).await.0.len(), 1024);
}

#[tokio::test]
async fn a_body_with_no_length_is_cut_off_at_the_limit_and_the_connection_dropped() {
    let written = Arc::new(AtomicUsize::new(0));
    let sent = Arc::clone(&written);
    let cdn = Cdn::start(move |_| Script::Endless {
        chunk: 16 * 1024,
        written: Arc::clone(&sent),
    })
    .await;
    let body = download(&client(), &cdn.url("/endless"), MIB)
        .await
        .unwrap();
    let (bytes, ended) = read_all(limited(body, MIB)).await;
    assert_eq!(ended, Some(RestError::TooLarge));
    assert!(bytes.len() as u64 <= MIB, "{} bytes passed on", bytes.len());

    // The client dropped the body, and with it the connection: the CDN's writes start to fail
    // and it stops long before it could have sent a body of any real size.
    let mut last = written.load(Ordering::SeqCst);
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(25)).await;
        let now = written.load(Ordering::SeqCst);
        if now == last {
            break;
        }
        last = now;
    }
    let total = written.load(Ordering::SeqCst) as u64;
    assert!(total < 64 * MIB, "the CDN got to write {total} bytes");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        total,
        written.load(Ordering::SeqCst) as u64,
        "it stopped writing"
    );
}

#[tokio::test]
async fn a_body_cut_short_is_an_error_after_what_arrived() {
    let cdn = Cdn::start(|_| Script::Truncated {
        declared: 1000,
        sent: b"only ten b".to_vec(),
    })
    .await;
    let body = download(&client(), &cdn.url("/short"), MIB).await.unwrap();
    let (bytes, ended) = read_all(body).await;
    assert_eq!(bytes, b"only ten b");
    assert_eq!(ended, Some(RestError::Transport));
}

#[tokio::test]
async fn a_cdn_that_never_answers_times_out() {
    let cdn = Cdn::start(|_| Script::Silent).await;
    let client = client_with(timed(Duration::from_millis(300), Duration::from_secs(30)));
    let started = std::time::Instant::now();
    let result = download(&client, &cdn.url("/x"), MIB).await;
    assert!(
        matches!(result, Err(RestError::Transport)),
        "{:?}",
        result.err()
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_body_that_stalls_midway_times_out() {
    let cdn = Cdn::start(|_| Script::Stalls {
        first: b"first bit".to_vec(),
    })
    .await;
    let client = client_with(timed(Duration::from_millis(300), Duration::from_secs(30)));
    let body = download(&client, &cdn.url("/x"), MIB).await.unwrap();
    let (bytes, ended) = read_all(body).await;
    assert_eq!(bytes, b"first bit");
    assert_eq!(ended, Some(RestError::Transport));
}

#[tokio::test]
async fn a_download_that_keeps_dripping_still_ends_at_the_total_timeout() {
    let cdn = Cdn::start(|_| Script::Drips {
        every: Duration::from_millis(50),
    })
    .await;
    // Never silent for the read timeout, but the whole download may take only a second.
    let client = client_with(timed(Duration::from_secs(5), Duration::from_secs(1)));
    let body = download(&client, &cdn.url("/x"), MIB).await.unwrap();
    let started = std::time::Instant::now();
    let (_, ended) = read_all(body).await;
    assert_eq!(ended, Some(RestError::Transport));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_cdn_that_is_not_there_is_a_transport_error() {
    // Bind and close, so the port is free and refuses connections.
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    };
    let result = download(&client(), &format!("http://127.0.0.1:{port}/x"), MIB).await;
    assert!(
        matches!(result, Err(RestError::Transport)),
        "{:?}",
        result.err()
    );
}

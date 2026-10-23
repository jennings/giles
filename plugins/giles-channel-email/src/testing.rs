//! Test doubles: fake IMAP and SMTP servers on 127.0.0.1 (plain, implicit TLS, or STARTTLS with
//! a throwaway certificate for `localhost`), and an in-memory blob store.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use giles_plugin_sdk::async_trait;
use giles_plugin_sdk::giles_types::{Error, MediaId, MediaRef, Result};
use giles_plugin_sdk::tls::rustls::crypto::aws_lc_rs;
use giles_plugin_sdk::tls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use giles_plugin_sdk::tls::rustls::{ClientConfig, RootCertStore, ServerConfig};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufStream};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;

use crate::media::MediaStore;
use crate::net::Stream;

/// Upper bound for anything in a test that must not hang.
pub(crate) const WAIT: Duration = Duration::from_secs(10);

/// Polls `check` every 10 ms until it holds, for at most [`WAIT`].
pub(crate) async fn wait_until(mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !check() {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// How a fake server protects its connections.
#[derive(Clone)]
pub(crate) enum Security {
    Plain,
    Implicit(TlsAcceptor),
    Starttls(TlsAcceptor),
}

/// A throwaway certificate for `localhost`: the server side, and a client config trusting only
/// it.
pub(crate) struct TestTls {
    pub client: Arc<ClientConfig>,
    pub acceptor: TlsAcceptor,
}

pub(crate) fn test_tls() -> TestTls {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let cert: CertificateDer<'static> = certified.cert.der().clone();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        certified.signing_key.serialize_der(),
    ));
    let provider = Arc::new(aws_lc_rs::default_provider());
    let server = ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], key)
        .unwrap();
    let mut roots = RootCertStore::empty();
    roots.add(cert).unwrap();
    let client = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    TestTls {
        client: Arc::new(client),
        acceptor: TlsAcceptor::from(Arc::new(server)),
    }
}

/// The client config tests use when no TLS is involved.
pub(crate) fn no_tls() -> Arc<ClientConfig> {
    Arc::new(giles_plugin_sdk::tls::client_config())
}

/// Accepts connections until dropped, serving each with `serve`.
fn accept_loop<F, Fut>(listener: TcpListener, serve: F) -> JoinHandle<()>
where
    F: Fn(Stream) -> Fut + Send + 'static,
    Fut: Future<Output = std::io::Result<()>> + Send + 'static,
{
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            tokio::spawn(serve(Box::new(tcp)));
        }
    })
}

/// Reads one line (through `\n`) into `buf`; cancel-safe. `false` at EOF.
async fn read_line(conn: &mut BufStream<Stream>, buf: &mut Vec<u8>) -> std::io::Result<bool> {
    let n = conn.read_until(b'\n', buf).await?;
    Ok(n > 0 && buf.ends_with(b"\n"))
}

async fn send(conn: &mut BufStream<Stream>, data: &[u8]) -> std::io::Result<()> {
    conn.write_all(data).await?;
    conn.flush().await
}

async fn start_tls(
    conn: BufStream<Stream>,
    acceptor: &TlsAcceptor,
) -> std::io::Result<BufStream<Stream>> {
    let tls = acceptor.accept(conn.into_inner()).await?;
    Ok(BufStream::new(Box::new(tls)))
}

// ------------------------------------------------------------------------------------------------
// IMAP

#[derive(Default)]
struct MailboxData {
    uid_validity: u32,
    next_uid: u32,
    messages: Vec<(u32, Vec<u8>)>,
}

struct ImapState {
    mailbox: Mutex<MailboxData>,
    /// Bumped on every change to the mailbox (wakes IDLE).
    changed: watch::Sender<u64>,
    /// Bumped to drop every connection.
    kill: watch::Sender<u64>,
    can_idle: bool,
    security: Security,
    failing_logins: AtomicUsize,
    logins: AtomicUsize,
    idles: AtomicUsize,
    commands: Mutex<Vec<String>>,
    quirks: Mutex<Quirks>,
}

/// Ways the fake misbehaves, like real servers do.
#[derive(Default)]
struct Quirks {
    /// `UID FETCH` of this UID always answers `NO`.
    broken_uid: Option<u32>,
    /// This UID is expunged when the client first fetches it (by another client, say).
    vanishing_uid: Option<u32>,
    /// Sends `* OK Still here` this often during IDLE.
    keepalive: Option<Duration>,
    /// Delivered while the next `UID SEARCH` runs, announced with its response.
    append_during_search: Option<Vec<u8>>,
    /// Sends a forged LOGIN answer as plain text right after the STARTTLS OK, and refuses
    /// every real LOGIN.
    inject_at_starttls: bool,
    /// Advertises IDLE but answers it with `BAD`.
    refuse_idle: bool,
}

/// Adds a message to the mailbox; returns its UID.
fn append_to(state: &ImapState, raw: &[u8]) -> u32 {
    let uid = {
        let mut mailbox = state.mailbox.lock().unwrap();
        let uid = mailbox.next_uid;
        mailbox.next_uid += 1;
        mailbox.messages.push((uid, raw.to_vec()));
        uid
    };
    state.changed.send_modify(|n| *n += 1);
    uid
}

/// A fake IMAP server with one mailbox, `INBOX`, and one user, `giles` / `secret`.
pub(crate) struct FakeImap {
    pub addr: SocketAddr,
    state: Arc<ImapState>,
    task: JoinHandle<()>,
}

impl Drop for FakeImap {
    fn drop(&mut self) {
        self.task.abort();
        self.state.kill.send_modify(|n| *n += 1);
    }
}

impl FakeImap {
    pub(crate) async fn start(security: Security, can_idle: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(ImapState {
            mailbox: Mutex::new(MailboxData {
                uid_validity: 1000,
                next_uid: 1,
                messages: Vec::new(),
            }),
            changed: watch::channel(0).0,
            kill: watch::channel(0).0,
            can_idle,
            security,
            failing_logins: AtomicUsize::new(0),
            logins: AtomicUsize::new(0),
            idles: AtomicUsize::new(0),
            commands: Mutex::default(),
            quirks: Mutex::default(),
        });
        let serving = Arc::clone(&state);
        let task = accept_loop(listener, move |stream| {
            let state = Arc::clone(&serving);
            async move { imap_connection(stream, state).await }
        });
        Self { addr, state, task }
    }

    /// Adds a message; returns its UID.
    pub(crate) fn append(&self, raw: &[u8]) -> u32 {
        append_to(&self.state, raw)
    }

    pub(crate) fn uid_validity(&self) -> u32 {
        self.state.mailbox.lock().unwrap().uid_validity
    }

    /// Recreates the mailbox under a new UIDVALIDITY, keeping its messages (renumbered from 1),
    /// and drops every connection.
    pub(crate) fn recreate(&self, uid_validity: u32) {
        {
            let mut mailbox = self.state.mailbox.lock().unwrap();
            mailbox.uid_validity = uid_validity;
            let messages = std::mem::take(&mut mailbox.messages);
            mailbox.next_uid = 1;
            for (_, raw) in messages {
                let uid = mailbox.next_uid;
                mailbox.next_uid += 1;
                mailbox.messages.push((uid, raw));
            }
        }
        self.drop_connections();
    }

    pub(crate) fn drop_connections(&self) {
        self.state.kill.send_modify(|n| *n += 1);
    }

    /// Makes the next `n` logins fail.
    pub(crate) fn fail_logins(&self, n: usize) {
        self.state.failing_logins.store(n, Ordering::SeqCst);
    }

    pub(crate) fn logins(&self) -> usize {
        self.state.logins.load(Ordering::SeqCst)
    }

    pub(crate) fn idles(&self) -> usize {
        self.state.idles.load(Ordering::SeqCst)
    }

    /// Every command received, without its tag.
    pub(crate) fn commands(&self) -> Vec<String> {
        self.state.commands.lock().unwrap().clone()
    }

    /// Makes every `UID FETCH` of `uid` fail with `NO`.
    pub(crate) fn break_fetch(&self, uid: u32) {
        self.state.quirks.lock().unwrap().broken_uid = Some(uid);
    }

    /// Expunges `uid` when the client first fetches it.
    pub(crate) fn expunge_on_fetch(&self, uid: u32) {
        self.state.quirks.lock().unwrap().vanishing_uid = Some(uid);
    }

    /// Sends a keepalive this often during IDLE.
    pub(crate) fn keepalive(&self, every: Duration) {
        self.state.quirks.lock().unwrap().keepalive = Some(every);
    }

    /// Delivers `raw` while the next `UID SEARCH` runs (after its results), announcing it only
    /// with an `EXISTS` in that response.
    pub(crate) fn append_during_next_search(&self, raw: &[u8]) {
        self.state.quirks.lock().unwrap().append_during_search = Some(raw.to_vec());
    }

    /// Injects a forged LOGIN answer as plain text after the STARTTLS OK and refuses real logins.
    pub(crate) fn inject_at_starttls(&self) {
        self.state.quirks.lock().unwrap().inject_at_starttls = true;
    }

    /// Keeps advertising IDLE but answers it with `BAD`.
    pub(crate) fn refuse_idle(&self) {
        self.state.quirks.lock().unwrap().refuse_idle = true;
    }
}

/// An IMAP astring argument: quoted (with `\` escapes) or an atom.
fn imap_args(text: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c == ' ' {
            chars.next();
        } else if c == '"' {
            chars.next();
            let mut arg = String::new();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => arg.extend(chars.next()),
                    '"' => break,
                    c => arg.push(c),
                }
            }
            args.push(arg);
        } else {
            let mut arg = String::new();
            while let Some(&c) = chars.peek() {
                if c == ' ' {
                    break;
                }
                arg.push(c);
                chars.next();
            }
            args.push(arg);
        }
    }
    args
}

async fn imap_connection(stream: Stream, state: Arc<ImapState>) -> std::io::Result<()> {
    let mut kill = state.kill.subscribe();
    tokio::select! {
        result = imap_session(stream, &state) => result,
        _ = kill.changed() => Ok(()),
    }
}

async fn imap_session(mut stream: Stream, state: &ImapState) -> std::io::Result<()> {
    if let Security::Implicit(acceptor) = &state.security {
        stream = Box::new(acceptor.accept(stream).await?);
    }
    let mut conn = BufStream::new(stream);
    let mut tls = matches!(state.security, Security::Implicit(_));
    send(&mut conn, b"* OK fake IMAP ready\r\n").await?;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if !read_line(&mut conn, &mut buf).await? {
            return Ok(());
        }
        let line = String::from_utf8_lossy(&buf).trim_end().to_owned();
        let (tag, command) = line.split_once(' ').unwrap_or((&line, ""));
        state.commands.lock().unwrap().push(command.to_owned());
        let upper = command.to_ascii_uppercase();
        let reply = if upper == "CAPABILITY" {
            let mut caps = "* CAPABILITY IMAP4rev1".to_owned();
            if state.can_idle {
                caps.push_str(" IDLE");
            }
            if matches!(state.security, Security::Starttls(_)) && !tls {
                caps.push_str(" STARTTLS");
            }
            format!("{caps}\r\n{tag} OK done\r\n")
        } else if upper == "STARTTLS" {
            let Security::Starttls(acceptor) = &state.security else {
                send(&mut conn, format!("{tag} BAD no TLS here\r\n").as_bytes()).await?;
                continue;
            };
            let mut reply = format!("{tag} OK begin TLS\r\n");
            if state.quirks.lock().unwrap().inject_at_starttls {
                // Plain text after the OK, in the same write: a forged answer to the LOGIN that
                // follows (the next tag), which the client must never see.
                let next: u32 = tag.trim_start_matches('A').parse().unwrap_or(0) + 1;
                reply.push_str(&format!("A{next:04} OK LOGIN completed\r\n"));
            }
            send(&mut conn, reply.as_bytes()).await?;
            conn = start_tls(conn, acceptor).await?;
            tls = true;
            continue;
        } else if upper.starts_with("LOGIN ") {
            if matches!(state.security, Security::Starttls(_)) && !tls {
                format!("{tag} NO [PRIVACYREQUIRED] STARTTLS first\r\n")
            } else {
                let args = imap_args(&command["LOGIN ".len()..]);
                let failing = state
                    .failing_logins
                    .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                    .is_ok();
                let injecting = state.quirks.lock().unwrap().inject_at_starttls;
                if !failing && !injecting && args == ["giles", "secret"] {
                    state.logins.fetch_add(1, Ordering::SeqCst);
                    format!("{tag} OK logged in\r\n")
                } else {
                    format!("{tag} NO [AUTHENTICATIONFAILED] invalid credentials\r\n")
                }
            }
        } else if upper.starts_with("SELECT ") {
            let mailbox = state.mailbox.lock().unwrap();
            format!(
                "* FLAGS (\\Seen)\r\n* {} EXISTS\r\n* OK [UIDVALIDITY {}] ok\r\n\
                 * OK [UIDNEXT {}] ok\r\n{tag} OK [READ-WRITE] selected\r\n",
                mailbox.messages.len(),
                mailbox.uid_validity,
                mailbox.next_uid
            )
        } else if let Some(query) = upper.strip_prefix("UID SEARCH ") {
            let found: Vec<u32> = {
                let mailbox = state.mailbox.lock().unwrap();
                let all = mailbox.messages.iter().map(|(uid, _)| *uid);
                if query == "ALL" {
                    all.collect()
                } else {
                    let set = query.strip_prefix("UID ").expect("UID SEARCH UID <set>");
                    if let Some(first) = set.strip_suffix(":*") {
                        let first: u32 = first.parse().unwrap();
                        // Like a real server, `n:*` also matches the newest message.
                        let newest = mailbox.messages.last().map(|(uid, _)| *uid);
                        all.filter(|uid| *uid >= first || Some(*uid) == newest)
                            .collect()
                    } else {
                        let only: u32 = set.parse().unwrap();
                        all.filter(|uid| *uid == only).collect()
                    }
                }
            };
            let list: Vec<String> = found.iter().map(u32::to_string).collect();
            let mut reply =
                format!("* SEARCH {}\r\n", list.join(" ")).replace("SEARCH \r\n", "SEARCH\r\n");
            // Mail delivered while the search ran: announced before the tagged OK, and not
            // again later.
            let arriving = state.quirks.lock().unwrap().append_during_search.take();
            if let Some(raw) = arriving {
                append_to(state, &raw);
                let exists = state.mailbox.lock().unwrap().messages.len();
                reply.push_str(&format!("* {exists} EXISTS\r\n"));
            }
            reply.push_str(&format!("{tag} OK done\r\n"));
            reply
        } else if upper.starts_with("UID FETCH ") {
            let rest = &command["UID FETCH ".len()..];
            let (uid, query) = rest.split_once(' ').unwrap();
            let uid: u32 = uid.parse().unwrap();
            if state.quirks.lock().unwrap().broken_uid == Some(uid) {
                let reply = format!("{tag} NO [SERVERBUG] Some messages could not be FETCHed\r\n");
                send(&mut conn, reply.as_bytes()).await?;
                continue;
            }
            let vanishing = state
                .quirks
                .lock()
                .unwrap()
                .vanishing_uid
                .take_if(|u| *u == uid);
            if vanishing.is_some() {
                state
                    .mailbox
                    .lock()
                    .unwrap()
                    .messages
                    .retain(|(u, _)| *u != uid);
            }
            let messages = state.mailbox.lock().unwrap().messages.clone();
            let found = messages.iter().enumerate().find(|(_, (u, _))| *u == uid);
            let mut out = Vec::new();
            if let Some((index, (_, raw))) = found {
                let seq = index + 1;
                let query = query.to_ascii_uppercase();
                if query.contains("RFC822.SIZE") {
                    out.extend(
                        format!("* {seq} FETCH (UID {uid} RFC822.SIZE {})\r\n", raw.len())
                            .as_bytes(),
                    );
                } else {
                    let (section, bytes) = if query.contains("BODY.PEEK[HEADER]") {
                        let end = raw
                            .windows(4)
                            .position(|w| w == b"\r\n\r\n")
                            .map_or(raw.len(), |p| p + 4);
                        ("BODY[HEADER]", &raw[..end])
                    } else {
                        ("BODY[]", &raw[..])
                    };
                    out.extend(
                        format!("* {seq} FETCH (UID {uid} {section} {{{}}}\r\n", bytes.len())
                            .as_bytes(),
                    );
                    out.extend(bytes);
                    out.extend(b")\r\n");
                }
            }
            out.extend(format!("{tag} OK done\r\n").as_bytes());
            send(&mut conn, &out).await?;
            continue;
        } else if upper == "IDLE" {
            if !state.can_idle || state.quirks.lock().unwrap().refuse_idle {
                send(&mut conn, format!("{tag} BAD no IDLE\r\n").as_bytes()).await?;
                continue;
            }
            let mut changed = state.changed.subscribe();
            changed.borrow_and_update();
            state.idles.fetch_add(1, Ordering::SeqCst);
            send(&mut conn, b"+ idling\r\n").await?;
            buf.clear();
            let keepalive = state.quirks.lock().unwrap().keepalive;
            loop {
                tokio::select! {
                    line = read_line(&mut conn, &mut buf) => {
                        if !line? {
                            return Ok(());
                        }
                        break;
                    }
                    _ = changed.changed() => {
                        let exists = state.mailbox.lock().unwrap().messages.len();
                        send(&mut conn, format!("* {exists} EXISTS\r\n").as_bytes()).await?;
                    }
                    () = tokio::time::sleep(keepalive.unwrap_or(WAIT)), if keepalive.is_some() => {
                        send(&mut conn, b"* OK Still here\r\n").await?;
                    }
                }
            }
            assert_eq!(String::from_utf8_lossy(&buf).trim_end(), "DONE");
            format!("{tag} OK IDLE done\r\n")
        } else if upper == "NOOP" {
            format!("{tag} OK done\r\n")
        } else if upper == "LOGOUT" {
            send(&mut conn, format!("* BYE\r\n{tag} OK bye\r\n").as_bytes()).await?;
            return Ok(());
        } else {
            format!("{tag} BAD unknown command\r\n")
        };
        send(&mut conn, reply.as_bytes()).await?;
    }
}

// ------------------------------------------------------------------------------------------------
// SMTP

/// A message the fake SMTP server accepted.
#[derive(Debug, Clone)]
pub(crate) struct Received {
    pub from: String,
    pub to: Vec<String>,
    pub data: Vec<u8>,
    pub user: Option<String>,
    pub tls: bool,
}

struct SmtpState {
    security: Security,
    /// Required credentials, if any.
    auth: Option<(String, String)>,
    /// A reply to every `RCPT TO`, instead of `250`.
    rcpt_reply: Mutex<Option<String>>,
    received: Mutex<Vec<Received>>,
}

/// A fake SMTP server.
pub(crate) struct FakeSmtp {
    pub addr: SocketAddr,
    state: Arc<SmtpState>,
    task: JoinHandle<()>,
}

impl Drop for FakeSmtp {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeSmtp {
    pub(crate) async fn start(security: Security, auth: Option<(&str, &str)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(SmtpState {
            security,
            auth: auth.map(|(u, p)| (u.to_owned(), p.to_owned())),
            rcpt_reply: Mutex::default(),
            received: Mutex::default(),
        });
        let serving = Arc::clone(&state);
        let task = accept_loop(listener, move |stream| {
            let state = Arc::clone(&serving);
            async move { smtp_session(stream, state).await }
        });
        Self { addr, state, task }
    }

    pub(crate) fn received(&self) -> Vec<Received> {
        self.state.received.lock().unwrap().clone()
    }

    /// Answers every `RCPT TO` with `reply` (e.g. `550 5.1.1 no such user`).
    pub(crate) fn reject_recipients(&self, reply: &str) {
        *self.state.rcpt_reply.lock().unwrap() = Some(reply.to_owned());
    }
}

async fn smtp_session(mut stream: Stream, state: Arc<SmtpState>) -> std::io::Result<()> {
    if let Security::Implicit(acceptor) = &state.security {
        stream = Box::new(acceptor.accept(stream).await?);
    }
    let mut tls = matches!(state.security, Security::Implicit(_));
    let mut conn = BufStream::new(stream);
    send(&mut conn, b"220 fake ESMTP\r\n").await?;
    let mut user = None;
    let mut envelope: Option<(String, Vec<String>)> = None;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if !read_line(&mut conn, &mut buf).await? {
            return Ok(());
        }
        let line = String::from_utf8_lossy(&buf).trim_end().to_owned();
        let upper = line.to_ascii_uppercase();
        let reply = if upper.starts_with("EHLO ") {
            let mut reply = "250-fake\r\n250-PIPELINING\r\n250-8BITMIME\r\n".to_owned();
            if matches!(state.security, Security::Starttls(_)) && !tls {
                reply.push_str("250-STARTTLS\r\n");
            }
            reply.push_str("250 AUTH PLAIN LOGIN\r\n");
            reply
        } else if upper == "STARTTLS" {
            let Security::Starttls(acceptor) = &state.security else {
                send(&mut conn, b"502 no TLS here\r\n").await?;
                continue;
            };
            send(&mut conn, b"220 go ahead\r\n").await?;
            conn = start_tls(conn, acceptor).await?;
            tls = true;
            continue;
        } else if let Some(encoded) = line.strip_prefix("AUTH PLAIN ") {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap_or_default();
            let parts: Vec<String> = decoded
                .split(|b| *b == 0)
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .collect();
            match (&state.auth, parts.as_slice()) {
                (Some((u, p)), [_, user_given, pass]) if u == user_given && p == pass => {
                    user = Some(u.clone());
                    "235 2.7.0 authenticated\r\n".to_owned()
                }
                _ => "535 5.7.8 bad credentials\r\n".to_owned(),
            }
        } else if upper.starts_with("MAIL FROM:") {
            if state.auth.is_some() && user.is_none() {
                "530 5.7.0 authentication required\r\n".to_owned()
            } else {
                let from = line["MAIL FROM:".len()..]
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .trim_matches(['<', '>'])
                    .to_owned();
                envelope = Some((from, Vec::new()));
                "250 2.1.0 ok\r\n".to_owned()
            }
        } else if upper.starts_with("RCPT TO:") {
            let custom = state.rcpt_reply.lock().unwrap().clone();
            match (custom, envelope.as_mut()) {
                (Some(reply), _) => format!("{reply}\r\n"),
                (None, Some((_, to))) => {
                    let address = line["RCPT TO:".len()..]
                        .split_whitespace()
                        .next()
                        .unwrap_or_default()
                        .trim_matches(['<', '>'])
                        .to_owned();
                    to.push(address);
                    "250 2.1.5 ok\r\n".to_owned()
                }
                (None, None) => "503 5.5.1 MAIL first\r\n".to_owned(),
            }
        } else if upper == "DATA" {
            match envelope.take() {
                Some((from, to)) if !to.is_empty() => {
                    send(&mut conn, b"354 go ahead\r\n").await?;
                    let mut data = Vec::new();
                    loop {
                        let mut line = Vec::new();
                        if !read_line(&mut conn, &mut line).await? {
                            return Ok(());
                        }
                        if line == b".\r\n" {
                            break;
                        }
                        let line = line.strip_prefix(b".").unwrap_or(&line);
                        data.extend_from_slice(line);
                    }
                    state.received.lock().unwrap().push(Received {
                        from,
                        to,
                        data,
                        user: user.clone(),
                        tls,
                    });
                    "250 2.0.0 queued\r\n".to_owned()
                }
                _ => "503 5.5.1 no valid recipients\r\n".to_owned(),
            }
        } else if upper == "QUIT" {
            send(&mut conn, b"221 bye\r\n").await?;
            return Ok(());
        } else if upper == "RSET" || upper == "NOOP" {
            "250 ok\r\n".to_owned()
        } else {
            "500 unknown command\r\n".to_owned()
        };
        send(&mut conn, reply.as_bytes()).await?;
    }
}

// ------------------------------------------------------------------------------------------------
// Blobs

/// Blobs in memory, keyed by SHA-256 like the SDK's.
#[derive(Default)]
pub(crate) struct MemoryMedia {
    blobs: Mutex<HashMap<MediaId, Vec<u8>>>,
}

impl MemoryMedia {
    pub(crate) fn get(&self, id: &MediaId) -> Option<Vec<u8>> {
        self.blobs.lock().unwrap().get(id).cloned()
    }
}

#[async_trait]
impl MediaStore for MemoryMedia {
    async fn put(
        &self,
        bytes: Vec<u8>,
        media_type: String,
        name: Option<String>,
    ) -> Result<MediaRef> {
        let id = MediaId::from_digest(Sha256::digest(&bytes).into());
        let media = MediaRef {
            id: id.clone(),
            media_type,
            size: bytes.len() as u64,
            name,
        };
        self.blobs.lock().unwrap().insert(id, bytes);
        Ok(media)
    }

    async fn read(&self, media: &MediaRef) -> Result<Vec<u8>> {
        self.get(&media.id)
            .ok_or_else(|| Error::NotFound(format!("blob {}", media.id)))
    }
}

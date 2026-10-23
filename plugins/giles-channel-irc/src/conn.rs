//! The connection task: connects, registers (CAP, SASL), turns server lines into events, sends
//! messages with flood protection, resolves recipients with `WHOIS`, and reconnects with backoff.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::BytesMut;
use futures::StreamExt as _;
use giles_plugin_sdk::giles_types::{
    ChannelEvent, ChannelStatus, Error, EventId, MessageId, Result,
};
use irc_proto::{CapSubCommand, Command, Message, Prefix, Response};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::ClientConfig;
use tokio_util::codec::{Decoder, FramedRead};
use tokio_util::sync::CancellationToken;

use crate::Timing;
use crate::casemap::CaseMapping;
use crate::config::{Sasl, Settings, is_nick};
use crate::inbound::{self, Privmsg, View};
use crate::outbound::{self, Recipient, Throttle};

/// Capabilities requested when the server offers them (`sasl` too, when configured).
const WANTED_CAPS: [&str; 6] = [
    "account-notify",
    "account-tag",
    "echo-message",
    "extended-join",
    "message-tags",
    "server-time",
];
/// The longest line read from the server: 8191 bytes of tags plus a 512-byte message.
const MAX_LINE: usize = 8191 + 512;
/// The most accounts remembered for recipient lookups.
const MAX_TRACKED: usize = 10_000;
/// How many alternative nicks (`nick_`, `nick__`, ...) registration tries.
const NICK_ATTEMPTS: usize = 3;

/// A request from [`crate::Irc`] to the connection task.
pub(crate) struct Request {
    /// Where to send.
    pub to: To,
    /// What to send.
    pub markdown: String,
    /// Gets the message id once every line is written, or the error.
    pub done: oneshot::Sender<Result<MessageId>>,
}

/// A send's destination.
pub(crate) enum To {
    /// A conversation id: a channel or a nick.
    Conversation(String),
    /// An identity, resolved to a nick with `WHOIS` first.
    Recipient(Recipient),
}

/// The connection task's ends of its channels to [`crate::Irc`].
pub(crate) struct Handles {
    /// Inbound events.
    pub events: mpsc::Sender<ChannelEvent>,
    /// Send requests.
    pub requests: mpsc::Receiver<Request>,
    /// Whether the connection is registered (ready to send).
    pub registered: watch::Sender<bool>,
    /// Cancelled when the channel is dropped.
    pub stop: CancellationToken,
    /// The status reported last, to skip repeats.
    last_status: Option<ChannelStatus>,
    /// Counts events, for unique ids.
    counter: u64,
}

impl Handles {
    pub(crate) fn new(
        events: mpsc::Sender<ChannelEvent>,
        requests: mpsc::Receiver<Request>,
        registered: watch::Sender<bool>,
        stop: CancellationToken,
    ) -> Self {
        Self {
            events,
            requests,
            registered,
            stop,
            last_status: None,
            counter: 0,
        }
    }

    /// A string unique to this moment and process, for ids.
    fn unique(&mut self) -> String {
        self.counter += 1;
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("{nanos}:{}", self.counter)
    }

    /// Emits a status event unless it's the one reported last.
    async fn report(&mut self, status: ChannelStatus) {
        if self.last_status.as_ref() == Some(&status) {
            return;
        }
        self.last_status = Some(status.clone());
        let Ok(id) = EventId::new(format!("status:{}", self.unique())) else {
            return;
        };
        // Fails only once the channel is gone, when nobody needs the status.
        let _ = self.events.send(ChannelEvent::Status { id, status }).await;
    }
}

/// How a session ended.
enum Ended {
    /// The channel was dropped.
    Stopped,
    /// The connection failed or was lost.
    Failed {
        message: String,
        /// Whether it had been registered for at least [`Timing::stable_after`].
        stable: bool,
    },
}

/// Runs connections until the channel is dropped.
pub(crate) async fn run(
    settings: Arc<Settings>,
    tls: Option<Arc<ClientConfig>>,
    timing: Timing,
    mut io: Handles,
) {
    let mut backoff = timing.reconnect_min;
    loop {
        let connecting =
            tokio::time::timeout(timing.connect_timeout, connect(&settings, tls.clone()));
        let connected = tokio::select! {
            () = io.stop.cancelled() => return,
            connected = connecting => connected,
        };
        let ended = match connected {
            Err(_) => Ended::Failed {
                message: format!(
                    "connecting to {}:{} timed out after {:?}",
                    settings.host, settings.port, timing.connect_timeout
                ),
                stable: false,
            },
            Ok(Err(message)) => Ended::Failed {
                message,
                stable: false,
            },
            Ok(Ok(stream)) => {
                tracing::info!(host = %settings.host, port = settings.port, "connected");
                Session::new(&settings, &timing, &mut io).run(stream).await
            }
        };
        io.registered.send_replace(false);
        let Ended::Failed { message, stable } = ended else {
            return;
        };
        if stable {
            backoff = timing.reconnect_min;
        }
        tracing::warn!("{message}; reconnecting in {backoff:?}");
        io.report(ChannelStatus::Error { message }).await;
        let wake = tokio::time::Instant::now() + backoff;
        loop {
            tokio::select! {
                () = io.stop.cancelled() => return,
                () = tokio::time::sleep_until(wake) => break,
                request = io.requests.recv() => match request {
                    None => return,
                    Some(request) => refuse(request),
                },
            }
        }
        backoff = (backoff * 2).min(timing.reconnect_max);
    }
}

fn refuse(request: Request) {
    let _ = request
        .done
        .send(Err(Error::unavailable("not connected to the IRC server")));
}

/// A TCP or TLS stream.
trait Io: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}

async fn connect(
    settings: &Settings,
    tls: Option<Arc<ClientConfig>>,
) -> std::result::Result<Box<dyn Io>, String> {
    let (host, port) = (settings.host.as_str(), settings.port);
    let tcp = TcpStream::connect((host, port))
        .await
        .map_err(|e| format!("connecting to {host}:{port}: {e}"))?;
    // Lines are small and interactive.
    let _ = tcp.set_nodelay(true);
    match tls {
        None => Ok(Box::new(tcp)),
        Some(config) => {
            let stream = TlsConnector::from(config)
                .connect(settings.server_name.clone(), tcp)
                .await
                .map_err(|e| format!("TLS with {host}:{port}: {e}"))?;
            Ok(Box::new(stream))
        }
    }
}

/// Splits the server's bytes into lines (CRLF or LF), decoding invalid UTF-8 lossily so a stray
/// Latin-1 byte doesn't end the connection. A line longer than [`MAX_LINE`] is skipped.
#[derive(Default)]
struct Lines {
    /// Dropping the rest of an overlong line.
    discarding: bool,
}

impl Decoder for Lines {
    type Item = String;
    type Error = io::Error;

    fn decode(&mut self, buf: &mut BytesMut) -> io::Result<Option<String>> {
        loop {
            let Some(end) = buf.iter().position(|&b| b == b'\n') else {
                if buf.len() > MAX_LINE {
                    buf.clear();
                    if !self.discarding {
                        tracing::warn!("skipping a line longer than {MAX_LINE} bytes");
                    }
                    self.discarding = true;
                }
                return Ok(None);
            };
            let line = buf.split_to(end + 1);
            if std::mem::take(&mut self.discarding) {
                continue;
            }
            let line = line.strip_suffix(b"\n").unwrap_or(&line);
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            return Ok(Some(String::from_utf8_lossy(line).into_owned()));
        }
    }
}

/// A line to write once the throttle allows, or a send that's complete when reached.
enum Out {
    Line(String),
    Done {
        done: oneshot::Sender<Result<MessageId>>,
        id: MessageId,
    },
}

/// A recipient being resolved, one `WHOIS` per candidate nick.
struct Lookup {
    want: Recipient,
    candidates: VecDeque<String>,
    markdown: String,
    done: oneshot::Sender<Result<MessageId>>,
    /// The `WHOIS` in flight.
    whois: Option<Whois>,
}

/// What a `WHOIS` reply said.
struct Whois {
    /// The nick asked about.
    nick: String,
    /// The nick as the server spells it (`RPL_WHOISUSER`).
    found: Option<String>,
    user: Option<String>,
    host: Option<String>,
    /// The services account (`330`).
    account: Option<String>,
    deadline: Instant,
}

/// One connection.
struct Session<'a> {
    settings: &'a Settings,
    timing: &'a Timing,
    io: &'a mut Handles,
    writer: Option<WriteHalf<Box<dyn Io>>>,
    started: Instant,
    /// Our current nick.
    nick: String,
    nick_attempts: usize,
    registered_at: Option<Instant>,
    /// Still in CAP negotiation (registration waits for `CAP END`).
    negotiating: bool,
    offered: HashMap<String, Option<String>>,
    caps: HashSet<String>,
    sasl_started: bool,
    sasl_done: bool,
    casemap: CaseMapping,
    chantypes: String,
    statusmsg: String,
    /// Folded account -> the nick last seen logged in to it.
    accounts: HashMap<String, String>,
    queue: VecDeque<Out>,
    throttle: Throttle,
    lookups: VecDeque<Lookup>,
    last_rx: Instant,
    pinged: bool,
}

/// The session's own result: `Err` ends the connection with that message.
type Step = std::result::Result<(), String>;

impl<'a> Session<'a> {
    fn new(settings: &'a Settings, timing: &'a Timing, io: &'a mut Handles) -> Self {
        let now = Instant::now();
        Self {
            settings,
            timing,
            io,
            writer: None,
            started: now,
            nick: settings.nick.clone(),
            nick_attempts: 0,
            registered_at: None,
            negotiating: true,
            offered: HashMap::new(),
            caps: HashSet::new(),
            sasl_started: false,
            sasl_done: false,
            casemap: CaseMapping::default(),
            chantypes: "#&".to_owned(),
            statusmsg: String::new(),
            accounts: HashMap::new(),
            queue: VecDeque::new(),
            throttle: Throttle::new(settings.send_burst, settings.send_interval, now),
            lookups: VecDeque::new(),
            last_rx: now,
            pinged: false,
        }
    }

    async fn run(mut self, stream: Box<dyn Io>) -> Ended {
        let (reader, writer) = tokio::io::split(stream);
        self.writer = Some(writer);
        let result = self.serve(FramedRead::new(reader, Lines::default())).await;
        // Pending sends fail: their senders are dropped with the session.
        match result {
            Ok(()) => Ended::Stopped,
            Err(message) => Ended::Failed {
                message,
                stable: self
                    .registered_at
                    .is_some_and(|at| at.elapsed() >= self.timing.stable_after),
            },
        }
    }

    /// Registers and serves until the connection fails (`Err`) or the channel is dropped.
    async fn serve(&mut self, mut reader: FramedRead<ReadHalf<Box<dyn Io>>, Lines>) -> Step {
        self.write("CAP LS 302").await?;
        if let Some(password) = &self.settings.password {
            self.write(&format!("PASS :{}", password.expose())).await?;
        }
        self.write(&format!("NICK {}", self.nick)).await?;
        self.write(&format!(
            "USER {} 0 * :{}",
            self.settings.username, self.settings.realname
        ))
        .await?;
        loop {
            let deadline = self.deadline(Instant::now());
            tokio::select! {
                () = self.io.stop.cancelled() => return self.quit().await,
                line = reader.next() => match line {
                    None => return Err("the server closed the connection".into()),
                    Some(Err(e)) => return Err(format!("reading from the server: {e}")),
                    Some(Ok(line)) => {
                        self.last_rx = Instant::now();
                        self.pinged = false;
                        self.on_line(&line).await?;
                    }
                },
                request = self.io.requests.recv() => match request {
                    None => return self.quit().await,
                    Some(request) => self.on_request(request).await?,
                },
                () = tokio::time::sleep_until(deadline.into()) => self.on_timer(Instant::now()).await?,
            }
            self.flush().await?;
            self.next_lookup().await?;
        }
    }

    /// Says goodbye: the channel was dropped.
    async fn quit(&mut self) -> Step {
        // Best effort: the connection is closed either way.
        let _ = self.write("QUIT :Giles is shutting down").await;
        Ok(())
    }

    /// The next time something is due: a throttled line or deferred `WHOIS`, a timeout, a
    /// keepalive.
    fn deadline(&self, now: Instant) -> Instant {
        let mut next = now + Duration::from_secs(3600);
        let line_waiting = matches!(self.queue.front(), Some(Out::Line(_)));
        let lookup_waiting = self.lookups.front().is_some_and(|l| l.whois.is_none());
        if (line_waiting || lookup_waiting)
            && let Some(wait) = self.throttle.wait(now)
        {
            next = next.min(now + wait);
        }
        if self.registered_at.is_none() {
            next = next.min(self.started + self.timing.register_timeout);
        }
        next = next.min(if self.pinged {
            self.last_rx + self.timing.ping_after + self.timing.ping_timeout
        } else {
            self.last_rx + self.timing.ping_after
        });
        if let Some(whois) = self.lookups.front().and_then(|l| l.whois.as_ref()) {
            next = next.min(whois.deadline);
        }
        next
    }

    async fn on_timer(&mut self, now: Instant) -> Step {
        if self.registered_at.is_none() && now >= self.started + self.timing.register_timeout {
            return Err(format!(
                "registration didn't finish within {:?}",
                self.timing.register_timeout
            ));
        }
        let quiet = now.saturating_duration_since(self.last_rx);
        if self.pinged && quiet >= self.timing.ping_after + self.timing.ping_timeout {
            return Err(format!("no reply from the server for {quiet:?}"));
        }
        if !self.pinged && quiet >= self.timing.ping_after {
            self.pinged = true;
            self.write("PING :giles").await?;
        }
        if self
            .lookups
            .front()
            .and_then(|l| l.whois.as_ref())
            .is_some_and(|w| now >= w.deadline)
            && let Some(lookup) = self.lookups.pop_front()
        {
            let _ = lookup.done.send(Err(Error::unavailable(
                "the server didn't answer WHOIS in time",
            )));
            self.next_lookup().await?;
        }
        Ok(())
    }

    /// Writes queued lines the throttle allows, and completes sends whose lines are all written.
    async fn flush(&mut self) -> Step {
        loop {
            let now = Instant::now();
            match self.queue.front() {
                None => return Ok(()),
                Some(Out::Line(_)) if self.throttle.wait(now).is_some() => return Ok(()),
                Some(_) => {}
            }
            match self.queue.pop_front() {
                Some(Out::Line(line)) => {
                    self.throttle.take(now);
                    self.write(&line).await?;
                }
                Some(Out::Done { done, id }) => {
                    let _ = done.send(Ok(id));
                }
                None => return Ok(()),
            }
        }
    }

    /// Writes one line now. Never logs it: it may carry a password.
    async fn write(&mut self, line: &str) -> Step {
        let writer = self.writer.as_mut().ok_or("not connected")?;
        let mut bytes = Vec::with_capacity(line.len() + 2);
        bytes.extend_from_slice(line.as_bytes());
        bytes.extend_from_slice(b"\r\n");
        let write = async {
            writer.write_all(&bytes).await?;
            writer.flush().await
        };
        match tokio::time::timeout(self.timing.write_timeout, write).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(format!("writing to the server: {e}")),
            Err(_) => Err("the server stopped reading (write timed out)".into()),
        }
    }

    async fn on_request(&mut self, request: Request) -> Step {
        if self.registered_at.is_none() {
            refuse(request);
            return Ok(());
        }
        let Request { to, markdown, done } = request;
        match to {
            To::Conversation(target) => {
                if outbound::is_target(&target) {
                    self.enqueue(&target, &markdown, done);
                } else {
                    let _ = done.send(Err(Error::InvalidRequest(format!(
                        "{target:?} is not a channel or nick"
                    ))));
                }
            }
            To::Recipient(want) => {
                let candidates = self.candidates(&want);
                self.lookups.push_back(Lookup {
                    want,
                    candidates,
                    markdown,
                    done,
                    whois: None,
                });
                if self.lookups.len() == 1 {
                    self.next_lookup().await?;
                }
            }
        }
        Ok(())
    }

    /// The nicks that may belong to `want`: for an account, the nick last seen logged in to it,
    /// then the account name itself (services usually reserve it for the account's owner).
    fn candidates(&self, want: &Recipient) -> VecDeque<String> {
        let mut candidates = VecDeque::new();
        match want {
            Recipient::Account(account) => {
                if let Some(nick) = self.accounts.get(&self.casemap.fold(account)) {
                    candidates.push_back(nick.clone());
                }
                if is_nick(account) && !candidates.iter().any(|n| self.casemap.eq(n, account)) {
                    candidates.push_back(account.clone());
                }
            }
            Recipient::Mask { nick, .. } => candidates.push_back(nick.clone()),
        }
        candidates
    }

    /// Whether a lookup's `WHOIS` may start now: no message lines are waiting and the throttle has
    /// a slot. Then the confirmed nick gets its first line at most one interval after the reply,
    /// rather than after everything queued before it, leaving no time for the nick to change hands.
    fn lookup_ready(&self, now: Instant) -> bool {
        !self.queue.iter().any(|out| matches!(out, Out::Line(_)))
            && self.throttle.wait(now).is_none()
    }

    /// Starts the next `WHOIS` of the lookup at the front once [`Self::lookup_ready`], finishing
    /// lookups that have no candidates left. Called again after every flush, so a deferred lookup
    /// starts as soon as the queue drains.
    async fn next_lookup(&mut self) -> Step {
        loop {
            let now = Instant::now();
            let ready = self.lookup_ready(now);
            let Some(lookup) = self.lookups.front_mut() else {
                return Ok(());
            };
            if lookup.whois.is_some() {
                return Ok(());
            }
            if !lookup.candidates.is_empty() {
                if !ready {
                    return Ok(());
                }
                let Some(nick) = lookup.candidates.pop_front() else {
                    return Ok(());
                };
                lookup.whois = Some(Whois {
                    nick: nick.clone(),
                    found: None,
                    user: None,
                    host: None,
                    account: None,
                    deadline: now + self.timing.lookup_timeout,
                });
                self.throttle.take(now);
                return self.write(&format!("WHOIS {nick}")).await;
            }
            if let Some(lookup) = self.lookups.pop_front() {
                let who = match &lookup.want {
                    Recipient::Account(account) => format!("account {account}"),
                    Recipient::Mask { nick, user, host } => format!("{nick}!{user}@{host}"),
                };
                let _ = lookup
                    .done
                    .send(Err(Error::unavailable(format!("{who} is not online"))));
            }
        }
    }

    /// Queues `markdown` to `target` as lines that fit, then the send's completion.
    fn enqueue(&mut self, target: &str, markdown: &str, done: oneshot::Sender<Result<MessageId>>) {
        let budget = outbound::line_budget(
            target,
            &self.nick,
            &self.settings.username,
            self.settings.max_line_bytes,
        );
        for line in outbound::lines(markdown, budget, self.settings.max_lines) {
            self.queue
                .push_back(Out::Line(format!("PRIVMSG {target} :{line}")));
        }
        match MessageId::new(format!("sent:{}", self.io.unique())) {
            Ok(id) => self.queue.push_back(Out::Done { done, id }),
            Err(e) => {
                let _ = done.send(Err(e.into()));
            }
        }
    }

    async fn on_line(&mut self, line: &str) -> Step {
        if line.is_empty() {
            return Ok(());
        }
        let message: Message = match line.parse() {
            Ok(message) => message,
            Err(e) => {
                tracing::debug!("skipping a line that doesn't parse: {e}");
                return Ok(());
            }
        };
        let (nick, user, host) = match &message.prefix {
            Some(Prefix::Nickname(nick, user, host)) => {
                (nick.as_str(), user.as_str(), host.as_str())
            }
            _ => ("", "", ""),
        };
        let tags = message.tags.as_deref().unwrap_or(&[]);
        match &message.command {
            Command::PING(token, _) => self.write(&format!("PONG :{token}")).await?,
            Command::ERROR(reason) => {
                return Err(format!("the server closed the connection: {reason}"));
            }
            Command::CAP(_, sub, a, b) => self.on_cap(*sub, a.as_deref(), b.as_deref()).await?,
            Command::AUTHENTICATE(data) if data == "+" => self.on_authenticate().await?,
            Command::Response(response, params) => self.on_numeric(*response, params).await?,
            Command::Raw(code, params) if code == "330" => self.on_whois_account(params),
            Command::NICK(new) => self.on_nick(nick, new),
            Command::ACCOUNT(account) => self.track(account, nick),
            Command::JOIN(_, Some(account), _) if self.caps.contains("extended-join") => {
                self.track(account, nick);
            }
            Command::QUIT(_) => self.forget(nick),
            Command::PRIVMSG(target, text) => {
                let msg = Privmsg {
                    tags,
                    nick,
                    user,
                    host,
                    target,
                    text,
                };
                self.on_privmsg(&msg).await;
            }
            _ => {}
        }
        Ok(())
    }

    async fn on_cap(&mut self, sub: CapSubCommand, a: Option<&str>, b: Option<&str>) -> Step {
        // `CAP <target> <sub> [*] :<caps>`: irc-proto puts the caps last, after a `*` that
        // marks a multi-line reply.
        let (more, list) = match b {
            Some(list) => (a == Some("*"), list),
            None => (false, a.unwrap_or("")),
        };
        match sub {
            CapSubCommand::LS => {
                for cap in list.split_whitespace() {
                    let (name, value) = match cap.split_once('=') {
                        Some((name, value)) => (name, Some(value.to_owned())),
                        None => (cap, None),
                    };
                    self.offered.insert(name.to_owned(), value);
                }
                if !more && self.negotiating {
                    self.request_caps().await?;
                }
            }
            CapSubCommand::ACK => {
                for cap in list.split_whitespace() {
                    match cap.strip_prefix('-') {
                        Some(cap) => self.caps.remove(cap),
                        None => self.caps.insert(cap.to_owned()),
                    };
                }
                if self.negotiating {
                    self.after_ack().await?;
                }
            }
            CapSubCommand::NAK => {
                tracing::warn!("the server refused capabilities: {list}");
                if self.negotiating {
                    if self.settings.sasl.is_some() {
                        return Err(format!("the server refused capabilities: {list}"));
                    }
                    self.cap_end().await?;
                }
            }
            CapSubCommand::DEL => {
                for cap in list.split_whitespace() {
                    self.caps.remove(cap);
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn request_caps(&mut self) -> Step {
        let mut wanted: Vec<&str> = WANTED_CAPS
            .into_iter()
            .filter(|cap| self.offered.contains_key(*cap))
            .collect();
        if let Some(sasl) = &self.settings.sasl {
            let mechanism = sasl.mechanism();
            match self.offered.get("sasl") {
                None => return Err("the server doesn't offer SASL".into()),
                Some(Some(mechanisms))
                    if !mechanisms
                        .split(',')
                        .any(|m| m.eq_ignore_ascii_case(mechanism)) =>
                {
                    return Err(format!(
                        "the server doesn't offer SASL {mechanism} (only {mechanisms})"
                    ));
                }
                Some(_) => wanted.push("sasl"),
            }
        }
        if wanted.is_empty() {
            self.cap_end().await
        } else {
            self.write(&format!("CAP REQ :{}", wanted.join(" "))).await
        }
    }

    async fn after_ack(&mut self) -> Step {
        match &self.settings.sasl {
            Some(sasl) if !self.sasl_started => {
                if !self.caps.contains("sasl") {
                    return Err("the server didn't enable SASL".into());
                }
                self.sasl_started = true;
                self.write(&format!("AUTHENTICATE {}", sasl.mechanism()))
                    .await
            }
            Some(_) => Ok(()),
            None => self.cap_end().await,
        }
    }

    async fn on_authenticate(&mut self) -> Step {
        let chunks = match &self.settings.sasl {
            Some(Sasl::Plain { username, password }) => {
                outbound::sasl_plain(username, password.expose())
            }
            Some(Sasl::External) => vec!["+".to_owned()],
            None => return Ok(()),
        };
        for chunk in chunks {
            self.write(&format!("AUTHENTICATE {chunk}")).await?;
        }
        Ok(())
    }

    async fn cap_end(&mut self) -> Step {
        self.negotiating = false;
        self.write("CAP END").await
    }

    async fn on_numeric(&mut self, response: Response, params: &[String]) -> Step {
        let last = params.last().map_or("", String::as_str);
        match response {
            Response::RPL_WELCOME => {
                if self.settings.sasl.is_some() && !self.sasl_done {
                    return Err("registered without SASL authentication".into());
                }
                if let Some(nick) = params.first() {
                    self.nick.clone_from(nick);
                }
                self.registered_at = Some(Instant::now());
                self.negotiating = false;
                tracing::info!(nick = %self.nick, "registered");
                for channel in &self.settings.channels {
                    self.queue.push_back(Out::Line(format!("JOIN {channel}")));
                }
                self.io.registered.send_replace(true);
                self.io.report(ChannelStatus::Connected).await;
            }
            Response::RPL_ISUPPORT => {
                let tokens = params.get(1..params.len().saturating_sub(1)).unwrap_or(&[]);
                for token in tokens {
                    let (key, value) = token.split_once('=').unwrap_or((token, ""));
                    match key {
                        "CASEMAPPING" => self.casemap = CaseMapping::from_token(value),
                        "CHANTYPES" => value.clone_into(&mut self.chantypes),
                        "STATUSMSG" => value.clone_into(&mut self.statusmsg),
                        _ => {}
                    }
                }
            }
            Response::ERR_NICKNAMEINUSE | Response::ERR_UNAVAILRESOURCE
                if self.registered_at.is_none() =>
            {
                self.nick_attempts += 1;
                if self.nick_attempts > NICK_ATTEMPTS {
                    return Err(format!(
                        "the nick {} and its alternatives are taken",
                        self.settings.nick
                    ));
                }
                self.nick = format!("{}{}", self.settings.nick, "_".repeat(self.nick_attempts));
                tracing::warn!(nick = %self.nick, "nick taken; trying another");
                self.write(&format!("NICK {}", self.nick)).await?;
            }
            Response::ERR_ERRONEOUSNICKNAME if self.registered_at.is_none() => {
                return Err(format!("the server refused the nick {}: {last}", self.nick));
            }
            Response::ERR_PASSWDMISMATCH | Response::ERR_YOUREBANNEDCREEP => {
                return Err(format!("the server refused the connection: {last}"));
            }
            Response::RPL_LOGGEDIN => {
                tracing::info!(
                    account = params.get(2).map_or("", String::as_str),
                    "logged in"
                );
            }
            Response::RPL_SASLSUCCESS | Response::ERR_SASLALREADY => {
                self.sasl_done = true;
                self.cap_end().await?;
            }
            Response::ERR_NICKLOCKED
            | Response::ERR_SASLFAIL
            | Response::ERR_SASLTOOLONG
            | Response::ERR_SASLABORT => {
                return Err(format!("SASL authentication failed: {last}"));
            }
            Response::RPL_WHOISUSER => {
                if let (Some(nick), Some(user), Some(host)) =
                    (params.get(1), params.get(2), params.get(3))
                    && let Some(whois) = self.whois(nick)
                {
                    whois.found = Some(nick.clone());
                    whois.user = Some(user.clone());
                    whois.host = Some(host.clone());
                }
            }
            Response::ERR_NOSUCHNICK | Response::RPL_ENDOFWHOIS => {
                if let Some(nick) = params.get(1)
                    && self.whois(nick).is_some()
                {
                    self.finish_whois().await?;
                }
            }
            response if response.is_error() => {
                tracing::warn!("the server said {}: {}", response as u16, params.join(" "));
            }
            _ => {}
        }
        Ok(())
    }

    /// The `WHOIS` in flight, if it's about `nick`.
    fn whois(&mut self, nick: &str) -> Option<&mut Whois> {
        let casemap = self.casemap;
        self.lookups
            .front_mut()
            .and_then(|lookup| lookup.whois.as_mut())
            .filter(|whois| casemap.eq(&whois.nick, nick))
    }

    /// `330 <me> <nick> <account> :is logged in as`.
    fn on_whois_account(&mut self, params: &[String]) {
        if let (Some(nick), Some(account)) = (params.get(1), params.get(2))
            && let Some(whois) = self.whois(nick)
        {
            whois.account = Some(account.clone());
        }
    }

    /// Decides the lookup at the front after its `WHOIS` ended: sends to the nick if it matches,
    /// else tries the next candidate.
    async fn finish_whois(&mut self) -> Step {
        let casemap = self.casemap;
        let Some(lookup) = self.lookups.front_mut() else {
            return Ok(());
        };
        let Some(whois) = lookup.whois.take() else {
            return Ok(());
        };
        let matches = match (&lookup.want, &whois.found) {
            (_, None) => false,
            (Recipient::Account(account), Some(_)) => whois
                .account
                .as_deref()
                .is_some_and(|found| casemap.eq(found, account)),
            (Recipient::Mask { nick, user, host }, Some(found)) => {
                casemap.eq(found, nick)
                    && whois.user.as_deref() == Some(user.as_str())
                    && whois
                        .host
                        .as_deref()
                        .is_some_and(|h| h.eq_ignore_ascii_case(host))
            }
        };
        // What the server said replaces what was tracked about the nick.
        match (&whois.found, &whois.account) {
            (Some(nick), Some(account)) => self.track(account, nick),
            (Some(nick), None) => self.forget(nick),
            (None, _) => self.forget(&whois.nick),
        }
        if let (true, Some(nick)) = (matches, whois.found)
            && let Some(lookup) = self.lookups.pop_front()
        {
            self.enqueue(&nick, &lookup.markdown, lookup.done);
        }
        self.next_lookup().await
    }

    fn on_nick(&mut self, old: &str, new: &str) {
        let casemap = self.casemap;
        if casemap.eq(old, &self.nick) {
            new.clone_into(&mut self.nick);
        }
        for nick in self.accounts.values_mut() {
            if casemap.eq(nick, old) {
                new.clone_into(nick);
            }
        }
    }

    /// Remembers that `nick` is logged in to `account` (`*`: logged out).
    fn track(&mut self, account: &str, nick: &str) {
        if nick.is_empty() || account.is_empty() {
            return;
        }
        self.forget(nick);
        if account == "*" {
            return;
        }
        let key = self.casemap.fold(account);
        if self.accounts.len() < MAX_TRACKED || self.accounts.contains_key(&key) {
            self.accounts.insert(key, nick.to_owned());
        }
    }

    fn forget(&mut self, nick: &str) {
        let casemap = self.casemap;
        self.accounts.retain(|_, n| !casemap.eq(n, nick));
    }

    async fn on_privmsg(&mut self, msg: &Privmsg<'_>) {
        let view = View {
            nick: &self.nick,
            casemap: self.casemap,
            chantypes: &self.chantypes,
            statusmsg: &self.statusmsg,
            account_tag: self.caps.contains("account-tag"),
            mask_identities: self.settings.mask_identities,
        };
        let received = self.io.unique();
        let event = match inbound::event(&view, msg, &received) {
            Ok(Some(event)) => event,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!("dropping a message that can't be reported: {e}");
                return;
            }
        };
        if view.account_tag
            && let Some(account) = inbound::tag(msg.tags, "account")
        {
            self.track(account, msg.nick);
        }
        // Waits while the SDK persists earlier events; fails only once the channel is gone.
        let _ = self.io.events.send(event).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(chunks: &[&[u8]]) -> Vec<String> {
        let mut lines = Lines::default();
        let mut buf = BytesMut::new();
        let mut out = Vec::new();
        for chunk in chunks {
            buf.extend_from_slice(chunk);
            while let Some(line) = lines.decode(&mut buf).unwrap() {
                out.push(line);
            }
        }
        out
    }

    #[test]
    fn lines_split_on_crlf_or_lf_across_reads() {
        assert_eq!(
            decode(&[b"PING :a\r\nPI", b"NG :b\n", b"\r\nx"]),
            ["PING :a", "PING :b", ""]
        );
    }

    #[test]
    fn invalid_utf8_is_replaced_not_fatal() {
        assert_eq!(decode(&[b"caf\xe9\r\n"]), ["caf\u{fffd}"]);
    }

    #[test]
    fn overlong_lines_are_skipped() {
        let long = vec![b'x'; MAX_LINE + 10];
        assert_eq!(decode(&[&long, b"yy\r\nok\r\n"]), ["ok"]);
    }
}

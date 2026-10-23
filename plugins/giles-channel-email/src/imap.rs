//! The IMAP side: log in, select the mailbox, find and fetch new messages by UID, and wait for more
//! with IDLE (RFC 2177) or by polling.

use std::sync::Arc;
use std::time::Duration;

use async_imap::types::UnsolicitedResponse;
use async_imap::{Client, Session};
use futures::TryStreamExt as _;
use giles_plugin_sdk::giles_types::{Error, Result};
use giles_plugin_sdk::tls::rustls::ClientConfig;

use crate::config::{Imap, Tls};
use crate::net::{self, Stream};

/// How long one command (other than IDLE) may take.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

/// Runs an IMAP command with [`COMMAND_TIMEOUT`], mapping failures to `Unavailable`.
async fn command<T>(
    what: &str,
    future: impl Future<Output = async_imap::error::Result<T>>,
) -> Result<T> {
    tokio::time::timeout(COMMAND_TIMEOUT, future)
        .await
        .map_err(|_| Error::unavailable(format!("IMAP {what} timed out")))?
        .map_err(|e| Error::unavailable(format!("IMAP {what}: {e}")))
}

/// A logged-in session with the mailbox selected.
#[derive(Debug)]
pub(crate) struct Mailbox {
    session: Session<Stream>,
    pub uid_validity: u32,
    /// The UID the next message will get, if the server said.
    pub uid_next: Option<u32>,
    /// Whether the server offers IDLE.
    pub can_idle: bool,
}

/// Connects, logs in, and selects the configured mailbox.
pub(crate) async fn open(config: &Imap, tls: &Arc<ClientConfig>) -> Result<Mailbox> {
    let server = &config.server;
    let mut stream = net::connect(&server.host, server.port).await?;
    if server.tls == Tls::Implicit {
        stream = net::tls(stream, &server.host, tls).await?;
    }
    let mut client = Client::new(stream);
    let greeting = tokio::time::timeout(COMMAND_TIMEOUT, client.read_response())
        .await
        .map_err(|_| Error::unavailable("the IMAP greeting timed out"))?
        .map_err(|e| Error::unavailable(format!("reading the IMAP greeting: {e}")))?;
    if greeting.is_none() {
        return Err(Error::unavailable("the IMAP server closed the connection"));
    }
    if server.tls == Tls::Starttls {
        command(
            "STARTTLS",
            client.run_command_and_check_ok("STARTTLS", None),
        )
        .await?;
        // Anything the server sent after its STARTTLS response is discarded with the client, so
        // plain text can't be injected into the TLS session.
        let stream = net::tls(client.into_inner(), &server.host, tls).await?;
        client = Client::new(stream);
    }
    let mut session = tokio::time::timeout(
        COMMAND_TIMEOUT,
        client.login(&config.user, config.password.expose()),
    )
    .await
    .map_err(|_| Error::unavailable("IMAP LOGIN timed out"))?
    .map_err(|(e, _)| Error::unavailable(format!("IMAP login as {:?} failed: {e}", config.user)))?;
    let capabilities = command("CAPABILITY", session.capabilities()).await?;
    let can_idle = capabilities.has_str("IDLE");
    let selected = command("SELECT", session.select(&config.mailbox)).await?;
    let uid_validity = selected.uid_validity.ok_or_else(|| {
        Error::unavailable(format!(
            "the IMAP server didn't report UIDVALIDITY for {:?}",
            config.mailbox
        ))
    })?;
    Ok(Mailbox {
        session,
        uid_validity,
        uid_next: selected.uid_next,
        can_idle,
    })
}

impl Mailbox {
    /// The UIDs above `after`, ascending.
    pub(crate) async fn uids_after(&mut self, after: u32) -> Result<Vec<u32>> {
        let first = after.saturating_add(1);
        let found = command(
            "UID SEARCH",
            self.session.uid_search(format!("UID {first}:*")),
        )
        .await?;
        // `n:*` always matches the newest message, even when its UID is below `n`.
        let mut uids: Vec<u32> = found.into_iter().filter(|uid| *uid > after).collect();
        uids.sort_unstable();
        Ok(uids)
    }

    /// The highest UID in the mailbox (0 if it's empty).
    pub(crate) async fn last_uid(&mut self) -> Result<u32> {
        let found = command("UID SEARCH", self.session.uid_search("ALL")).await?;
        Ok(found.into_iter().max().unwrap_or(0))
    }

    /// A message's size in bytes; `None` if it's gone.
    pub(crate) async fn size(&mut self, uid: u32) -> Result<Option<u32>> {
        let fetches: Vec<_> = command("UID FETCH", async {
            self.session
                .uid_fetch(uid.to_string(), "(UID RFC822.SIZE)")
                .await?
                .try_collect()
                .await
        })
        .await?;
        match fetches
            .iter()
            .find(|f| f.uid == Some(uid))
            .and_then(|f| f.size)
        {
            Some(size) => Ok(Some(size)),
            None => self.gone(uid).await.map(|()| None),
        }
    }

    /// A message's bytes, or only its header section; `None` if it's gone. `BODY.PEEK` leaves
    /// the message's flags (`\Seen`) alone.
    pub(crate) async fn fetch(&mut self, uid: u32, header_only: bool) -> Result<Option<Vec<u8>>> {
        let query = if header_only {
            "(UID BODY.PEEK[HEADER])"
        } else {
            "(UID BODY.PEEK[])"
        };
        let fetches: Vec<_> = command("UID FETCH", async {
            self.session
                .uid_fetch(uid.to_string(), query)
                .await?
                .try_collect()
                .await
        })
        .await?;
        let bytes = fetches
            .iter()
            .find(|f| f.uid == Some(uid))
            .and_then(|f| if header_only { f.header() } else { f.body() })
            .map(<[u8]>::to_vec);
        match bytes {
            Some(bytes) => Ok(Some(bytes)),
            None => self.gone(uid).await.map(|()| None),
        }
    }

    /// After a `UID FETCH` returned nothing: `Ok` if the message is really gone (expunged).
    /// `async-imap` ignores a FETCH's tagged status, so a `NO` (a damaged message, a server
    /// that asks to try later) looks the same as no message, and must not skip the message.
    async fn gone(&mut self, uid: u32) -> Result<()> {
        let found = command("UID SEARCH", self.session.uid_search(format!("UID {uid}"))).await?;
        if found.contains(&uid) {
            Err(Error::unavailable(format!(
                "IMAP UID FETCH of message {uid} returned nothing, but the message exists"
            )))
        } else {
            Ok(())
        }
    }

    /// Waits until the mailbox may have changed: with IDLE for at most `idle_timeout` when
    /// `use_idle` and the server offers it, else by sleeping `poll_interval`. Returns at once if
    /// an earlier response already announced new mail.
    pub(crate) async fn wait(
        mut self,
        use_idle: bool,
        idle_timeout: Duration,
        poll_interval: Duration,
    ) -> Result<Self> {
        if !(use_idle && self.can_idle) {
            tokio::time::sleep(poll_interval).await;
            // Keeps the connection alive, and finds out now if it died.
            command("NOOP", self.session.noop()).await?;
            return Ok(self);
        }
        // `* N EXISTS` that came with the search's response (mail delivered while the server
        // searched) is queued, not repeated during IDLE: search again instead of waiting.
        let unsolicited = self.session.unsolicited_responses.clone();
        if news(|| unsolicited.try_recv().ok()) {
            return Ok(self);
        }
        let mut idle = self.session.idle();
        command("IDLE", idle.init()).await?;
        // The same for news that arrived before the server's `+` continuation.
        if !news(|| unsolicited.try_recv().ok()) {
            // `wait` restarts its own timer on every response, keepalives (`* OK Still here`)
            // included, so the deadline is enforced here. Any other response (new mail,
            // expunges, flag changes) ends the wait; the caller searches again either way.
            let (wait, _stop) = idle.wait();
            if let Ok(result) = tokio::time::timeout(idle_timeout, wait).await {
                result.map_err(|e| Error::unavailable(format!("IMAP IDLE: {e}")))?;
            }
        }
        self.session = command("DONE", idle.done()).await?;
        // What arrived with DONE's response is found by the next search anyway.
        news(|| unsolicited.try_recv().ok());
        Ok(self)
    }
}

/// Drains queued unsolicited responses; whether any announced new mail (`EXISTS`, `RECENT`).
fn news(mut next: impl FnMut() -> Option<UnsolicitedResponse>) -> bool {
    let mut news = false;
    while let Some(response) = next() {
        news |= matches!(
            response,
            UnsolicitedResponse::Exists(_) | UnsolicitedResponse::Recent(_)
        );
    }
    news
}

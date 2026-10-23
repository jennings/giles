//! The SMTP side: one connection per message (connect, TLS, EHLO, AUTH, send, QUIT).

use std::sync::Arc;
use std::time::Duration;

use async_smtp::authentication::{Credentials, Mechanism};
use async_smtp::extension::{ClientId, ServerInfo};
use async_smtp::{EmailAddress, Envelope, SendableEmail, SmtpClient, SmtpTransport};
use giles_plugin_sdk::giles_types::{Error, Result};
use giles_plugin_sdk::tls::rustls::ClientConfig;
use tokio::io::BufStream;

use crate::config::{Smtp, Tls};
use crate::net::{self, Stream};

/// How long sending one message may take, connecting included.
const SEND_TIMEOUT: Duration = Duration::from_secs(120);

/// Maps an SMTP failure: a `5xx` refusal is permanent (retrying won't help), anything else
/// (`4xx`, network, TLS) is `Unavailable` so the outbox retries.
fn smtp_error(what: &str, error: async_smtp::error::Error) -> Error {
    match error {
        async_smtp::error::Error::Permanent(response) => Error::InvalidRequest(format!(
            "the SMTP server refused {what}: {} {}",
            response.code,
            response.message.join(" ")
        )),
        other => Error::unavailable(format!("SMTP {what}: {other}")),
    }
}

/// Sends `message` from `from` to `to`.
pub(crate) async fn send(
    config: &Smtp,
    tls: &Arc<ClientConfig>,
    from: &str,
    to: &str,
    message: Vec<u8>,
) -> Result<()> {
    tokio::time::timeout(SEND_TIMEOUT, send_now(config, tls, from, to, message))
        .await
        .map_err(|_| Error::unavailable("sending by SMTP timed out"))?
}

async fn send_now(
    config: &Smtp,
    tls: &Arc<ClientConfig>,
    from: &str,
    to: &str,
    message: Vec<u8>,
) -> Result<()> {
    let envelope = Envelope::new(
        Some(EmailAddress::new(from.to_owned()).map_err(|e| invalid_address(from, e))?),
        vec![EmailAddress::new(to.to_owned()).map_err(|e| invalid_address(to, e))?],
    )
    .map_err(|e| Error::InvalidRequest(e.to_string()))?;
    let server = &config.server;
    let hello = ClientId::new(crate::address::domain(from).to_owned());
    let mut stream = net::connect(&server.host, server.port).await?;
    if server.tls == Tls::Implicit {
        stream = net::tls(stream, &server.host, tls).await?;
    }
    let client = || SmtpClient::new().hello_name(hello.clone());
    let mut transport = SmtpTransport::new(client(), BufStream::new(stream))
        .await
        .map_err(|e| smtp_error("the connection", e))?;
    if server.tls == Tls::Starttls {
        // Dropping the buffer discards anything sent after the STARTTLS response, so plain text
        // can't be injected into the TLS session.
        let plain = transport
            .starttls()
            .await
            .map_err(|e| smtp_error("STARTTLS", e))?
            .into_inner();
        let stream: Stream = net::tls(plain, &server.host, tls).await?;
        transport = SmtpTransport::new(client().without_greeting(), BufStream::new(stream))
            .await
            .map_err(|e| smtp_error("EHLO after STARTTLS", e))?;
    }
    if let Some((user, password)) = &config.credentials {
        // The transport keeps what EHLO said to itself; ask again to pick a mechanism.
        let ehlo = transport
            .get_mut()
            .ehlo(hello.clone())
            .await
            .map_err(|e| smtp_error("EHLO", e))?;
        let info = ServerInfo::from_response(&ehlo).map_err(|e| smtp_error("EHLO", e))?;
        let mechanism = [Mechanism::Plain, Mechanism::Login]
            .into_iter()
            .find(|m| info.supports_auth_mechanism(*m))
            .ok_or_else(|| {
                Error::InvalidRequest(
                    "the SMTP server offers neither AUTH PLAIN nor AUTH LOGIN".into(),
                )
            })?;
        let credentials = Credentials::new(user.clone(), password.expose().to_owned());
        transport
            .auth(mechanism, &credentials)
            .await
            .map_err(|e| smtp_error(&format!("logging in as {user:?}"), e))?;
    }
    transport
        .send(SendableEmail::new(envelope, message))
        .await
        .map_err(|e| smtp_error("the message", e))?;
    // The message is accepted; a failed QUIT changes nothing.
    if let Err(e) = transport.quit().await {
        tracing::debug!("SMTP QUIT: {e}");
    }
    Ok(())
}

fn invalid_address(address: &str, error: impl std::fmt::Display) -> Error {
    Error::InvalidRequest(format!("can't send to or from {address:?}: {error}"))
}

//! What the plugin logs: with a fake Discord client behind the real plugin SDK, and with the real
//! client against a loopback Gateway. A test binary of its own: `tracing` caches whether a call
//! site is enabled, which makes a subscriber installed for one test unreliable next to tests that
//! run without one.

mod loopback;
mod support;

use std::io;
use std::sync::{Arc, Mutex};

use giles_channel_discord::client::Discord;
use giles_channel_discord::client::GatewayEvent;
use loopback::{BOT, Gateway, client, next};
use serde_json::json;
use support::{Served, TOKEN, next_status};
use tracing_subscriber::fmt::MakeWriter;

/// Collects what `tracing` writes.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Self;

    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

/// Logs everything on this thread into the returned capture, until the guard is dropped.
fn capture_logs() -> (Capture, tracing::subscriber::DefaultGuard) {
    let capture = Capture::default();
    let guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish(),
    );
    (capture, guard)
}

#[tokio::test]
async fn the_token_is_in_no_log_line_at_any_level() {
    let (capture, _logging) = capture_logs();
    let served = Served::start().await;
    // A rejected configuration logs too.
    served
        .configure(serde_json::json!({"token": format!("Bot {TOKEN}")}))
        .await
        .unwrap_err();
    served
        .configure(serde_json::json!({"token": TOKEN}))
        .await
        .unwrap();
    let (_acks, mut events) = served.events().await;
    served.client.push(GatewayEvent::Ready { user_id: 7 });
    next_status(&mut events).await;
    served
        .client
        .push(GatewayEvent::Closed { code: Some(4004) });
    next_status(&mut events).await;
    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("Discord channel configured"), "{logs}");
    assert!(logs.contains("connected to Discord"), "{logs}");
    assert!(!logs.contains("not-a-real-token"), "{logs}");
}

#[tokio::test]
async fn the_real_client_never_logs_the_token() {
    let (capture, _logging) = capture_logs();
    let gateway = Gateway::bind().await;
    let client = client(&gateway.url);
    let server = tokio::spawn(async move {
        // Identify, then a connection that drops, a resume, and a rejected token.
        let mut connection = gateway.accept(100).await;
        connection.expect_identify().await;
        connection.send(gateway.ready()).await;
        drop(connection);
        let mut connection = gateway.accept(100).await;
        let resume = connection.recv_json().await;
        assert_eq!(resume["op"], 6, "{resume}");
        connection
            .send(json!({"op": 0, "s": 2, "t": "RESUMED", "d": {}}))
            .await;
        connection.close(4004).await;
    });
    assert_eq!(
        next(&client).await,
        Some(GatewayEvent::Ready { user_id: BOT })
    );
    assert_eq!(
        next(&client).await,
        Some(GatewayEvent::Closed { code: None })
    );
    assert_eq!(next(&client).await, Some(GatewayEvent::Resumed));
    assert_eq!(
        next(&client).await,
        Some(GatewayEvent::Closed { code: Some(4004) })
    );
    assert_eq!(next(&client).await, None);
    client.shutdown().await;
    server.await.unwrap();

    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    // The capture saw the library: it logs sending the Identify and the Resume at debug.
    assert!(logs.contains("sending identify"), "{logs}");
    assert!(!logs.contains("not-a-real-token"), "{logs}");
    assert!(!logs.contains(loopback::TOKEN), "{logs}");
}

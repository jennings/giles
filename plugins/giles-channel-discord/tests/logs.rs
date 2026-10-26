//! What the plugin logs, with a fake Discord client behind the real plugin SDK. A test binary of
//! its own: `tracing` caches whether a call site is enabled, which makes a subscriber installed
//! for one test unreliable next to tests that run without one.

mod support;

use std::io;
use std::sync::{Arc, Mutex};

use giles_channel_discord::client::GatewayEvent;
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

#[tokio::test]
async fn the_token_is_in_no_log_line_at_any_level() {
    let capture = Capture::default();
    let _logging = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish(),
    );
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

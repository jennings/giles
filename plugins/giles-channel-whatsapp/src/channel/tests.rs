//! The channel against a fake WhatsApp client: status reporting, event delivery and the
//! durability handshake, reconnection, and sends.

use std::time::{Duration, Instant};

use giles_plugin_sdk::giles_types::{
    AuthChallenge, ChannelStatus, ConversationId, InboundMessage, NativeId, Timestamp,
};

use super::*;
use crate::backend::{Address, BackendError, WaEvent};
use crate::config::Link;
use crate::testing::{
    FakeClient, FakeConnector, MemoryMedia, PHONE, Sent, WAIT, dm, image, wait_until,
};

type Events = BoxStream<'static, Result<ChannelEvent>>;

fn fast() -> Timing {
    Timing {
        disconnect_grace: Duration::from_millis(100),
        restart_min: Duration::from_millis(20),
        restart_max: Duration::from_millis(100),
        stable_after: Duration::from_secs(60),
        send_wait: Duration::from_millis(300),
        shutdown_wait: Duration::from_secs(1),
    }
}

fn settings(link: Link) -> Arc<Settings> {
    Arc::new(Settings {
        link,
        device_name: "Giles".into(),
        max_blob: 1024,
    })
}

struct Harness {
    channel: WhatsApp,
    events: Events,
    client: FakeClient,
    connector: Arc<FakeConnector>,
    media: Arc<MemoryMedia>,
    dir: tempfile::TempDir,
}

impl Harness {
    async fn start() -> Self {
        Self::start_with(Link::Qr).await
    }

    async fn start_with(link: Link) -> Self {
        let (client, connection) = FakeClient::new();
        let connector = Arc::new(FakeConnector::default());
        connector.push(Ok(connection));
        let media = Arc::new(MemoryMedia::default());
        let dir = tempfile::tempdir().unwrap();
        let channel = WhatsApp::start(
            settings(link),
            Arc::clone(&connector) as Arc<dyn Connector>,
            Arc::clone(&media) as Arc<dyn MediaStore>,
            DataDir::open(dir.path()).unwrap(),
            fast(),
        );
        let events = channel.events().await.unwrap();
        Self {
            channel,
            events,
            client,
            connector,
            media,
            dir,
        }
    }

    /// Reports `Connected` and consumes its status event.
    async fn connected(&mut self) {
        self.client.emit(WaEvent::Connected);
        expect_status(&mut self.events, &ChannelStatus::Connected).await;
    }

    async fn message(&mut self) -> (String, InboundMessage) {
        next_message(&mut self.events).await
    }
}

/// The next event, within [`WAIT`].
async fn next(events: &mut Events) -> ChannelEvent {
    tokio::time::timeout(WAIT, events.next())
        .await
        .expect("an event in time")
        .expect("the stream doesn't end")
        .expect("no stream error")
}

async fn next_status(events: &mut Events) -> ChannelStatus {
    match next(events).await {
        ChannelEvent::Status { status, .. } => status,
        other => panic!("expected a status, got {other:?}"),
    }
}

async fn expect_status(events: &mut Events, expected: &ChannelStatus) {
    assert_eq!(&next_status(events).await, expected);
}

async fn next_message(events: &mut Events) -> (String, InboundMessage) {
    match next(events).await {
        ChannelEvent::Message { id, message } => (id.into_string(), message),
        other => panic!("expected a message, got {other:?}"),
    }
}

/// Polls the stream for `duration`; nothing may arrive. Like the SDK's next poll, this also
/// confirms the previous event as persisted.
async fn expect_quiet(events: &mut Events, duration: Duration) {
    let polled = tokio::time::timeout(duration, events.next()).await;
    assert!(polled.is_err(), "unexpected event: {polled:?}");
}

/// Whether a commit has been resolved, without waiting for it.
async fn pending(durable: &mut tokio::sync::oneshot::Receiver<bool>) -> bool {
    tokio::time::timeout(Duration::from_millis(100), durable)
        .await
        .is_err()
}

fn error_message(status: ChannelStatus) -> String {
    match status {
        ChannelStatus::Error { message } => message,
        other => panic!("expected an error status, got {other:?}"),
    }
}

// ------------------------------------------------------------------------------------------------
// Status

#[tokio::test]
async fn it_reports_what_it_is() {
    let h = Harness::start().await;
    assert_eq!(h.channel.info().identity_namespace, "whatsapp");
    assert_eq!(h.channel.info().output, OutputMode::Blocks);
    assert!(h.channel.info().can_initiate);
}

#[tokio::test]
async fn linking_by_qr_code_goes_from_needs_auth_to_connected() {
    let mut h = Harness::start().await;
    let before = Timestamp::now();
    h.client.emit(WaEvent::PairingQr {
        code: "2@first".into(),
        timeout: Duration::from_secs(60),
    });
    let ChannelStatus::NeedsAuth {
        challenge,
        expires_at,
    } = next_status(&mut h.events).await
    else {
        panic!("not NeedsAuth");
    };
    assert_eq!(
        challenge,
        AuthChallenge::QrCode {
            payload: "2@first".into()
        }
    );
    // The code is good for its timeout from when it arrived.
    let expires_at = expires_at.expect("an expiry");
    assert!(expires_at >= before.checked_add(Duration::from_secs(60)).unwrap());
    assert!(
        expires_at
            <= Timestamp::now()
                .checked_add(Duration::from_secs(60))
                .unwrap()
    );

    // The next code replaces it.
    h.client.emit(WaEvent::PairingQr {
        code: "2@second".into(),
        timeout: Duration::from_secs(20),
    });
    let ChannelStatus::NeedsAuth { challenge, .. } = next_status(&mut h.events).await else {
        panic!("not NeedsAuth");
    };
    assert_eq!(
        challenge,
        AuthChallenge::QrCode {
            payload: "2@second".into()
        }
    );

    // Scanned: paired, then connected.
    h.client.emit(WaEvent::Paired);
    h.connected().await;
}

#[tokio::test]
async fn linking_by_pair_code_shows_the_code_and_ignores_qr_codes() {
    let mut h = Harness::start_with(Link::PairCode {
        phone: "15551234567".into(),
    })
    .await;
    // The client offers both at once; only the pair code is wanted.
    h.client.emit(WaEvent::PairingQr {
        code: "2@ignored".into(),
        timeout: Duration::from_secs(60),
    });
    h.client.emit(WaEvent::PairingCode {
        code: "ABCD1234".into(),
        timeout: Duration::from_secs(180),
    });
    let ChannelStatus::NeedsAuth {
        challenge,
        expires_at,
    } = next_status(&mut h.events).await
    else {
        panic!("not NeedsAuth");
    };
    assert_eq!(
        challenge,
        AuthChallenge::PairCode {
            code: "ABCD1234".into()
        }
    );
    assert!(expires_at.is_some());
    h.connected().await;
}

#[tokio::test]
async fn a_failed_pair_code_request_is_an_error_status() {
    let mut h = Harness::start_with(Link::PairCode {
        phone: "15551234567".into(),
    })
    .await;
    h.client.emit(WaEvent::LinkFailed("rate limited".into()));
    let message = error_message(next_status(&mut h.events).await);
    assert!(message.contains("rate limited"), "{message}");
    assert!(message.contains("Restart this plugin"), "{message}");
}

#[tokio::test]
async fn a_short_disconnect_is_not_reported() {
    let mut h = Harness::start().await;
    h.connected().await;
    h.client
        .emit(WaEvent::Disconnected("stream recycled".into()));
    tokio::time::sleep(Duration::from_millis(20)).await;
    h.client.emit(WaEvent::Connected);
    // Back within the grace period: nothing to say, not even a second `Connected`.
    expect_quiet(&mut h.events, Duration::from_millis(300)).await;
}

#[tokio::test]
async fn a_long_disconnect_is_reported_and_then_the_reconnect() {
    let mut h = Harness::start().await;
    h.connected().await;
    h.client
        .emit(WaEvent::Disconnected("connection reset".into()));
    let message = error_message(next_status(&mut h.events).await);
    assert!(message.contains("connection reset"), "{message}");
    assert!(message.contains("reconnecting"), "{message}");
    // The client reconnected by itself.
    h.connected().await;
    // And it can drop again.
    h.client.emit(WaEvent::Disconnected("again".into()));
    assert!(error_message(next_status(&mut h.events).await).contains("again"));
}

#[tokio::test]
async fn qr_codes_running_out_ends_linking_without_trying_again() {
    let mut h = Harness::start().await;
    h.client.emit(WaEvent::PairingQr {
        code: "2@last".into(),
        timeout: Duration::from_secs(20),
    });
    assert!(matches!(
        next_status(&mut h.events).await,
        ChannelStatus::NeedsAuth { .. }
    ));
    h.client.emit(WaEvent::QrExhausted { closed: true });
    let message = error_message(next_status(&mut h.events).await);
    assert!(message.contains("no QR code was scanned"), "{message}");
    assert!(message.contains("Restart this plugin"), "{message}");
    // A new client, which would ask WhatsApp for pairing again, only comes with a restart.
    expect_quiet(&mut h.events, Duration::from_millis(400)).await;
    assert_eq!(h.connector.connects(), 1);
    wait_until(|| {
        h.client
            .backend
            .shut_down
            .load(std::sync::atomic::Ordering::SeqCst)
    })
    .await;
}

#[tokio::test]
async fn a_pair_code_outstanding_keeps_the_client_when_qr_codes_run_out() {
    let mut h = Harness::start_with(Link::PairCode {
        phone: "15551234567".into(),
    })
    .await;
    h.client.emit(WaEvent::QrExhausted { closed: false });
    h.client.emit(WaEvent::PairingCode {
        code: "ABCD1234".into(),
        timeout: Duration::from_secs(180),
    });
    // The first status is the pair code: exhaustion with the socket up said nothing.
    assert!(matches!(
        next_status(&mut h.events).await,
        ChannelStatus::NeedsAuth {
            challenge: AuthChallenge::PairCode { .. },
            ..
        }
    ));
    h.connected().await;
}

#[tokio::test]
async fn a_logout_is_reported_remembered_and_not_retried() {
    let mut h = Harness::start().await;
    h.connected().await;
    h.client.emit(WaEvent::LoggedOut("LoggedOut".into()));
    let message = error_message(next_status(&mut h.events).await);
    assert!(message.contains("logged this device out"), "{message}");
    // The marker makes the next start link a new device instead of reusing the dead session.
    wait_until(|| h.dir.path().join("logged-out").exists()).await;
    expect_quiet(&mut h.events, Duration::from_millis(300)).await;
    assert_eq!(h.connector.connects(), 1);
}

#[tokio::test]
async fn a_fatal_error_is_reported_and_not_retried() {
    let mut h = Harness::start().await;
    h.client
        .emit(WaEvent::Fatal("WhatsApp banned this account".into()));
    expect_status(
        &mut h.events,
        &ChannelStatus::Error {
            message: "WhatsApp banned this account".into(),
        },
    )
    .await;
    expect_quiet(&mut h.events, Duration::from_millis(300)).await;
    assert_eq!(h.connector.connects(), 1);
}

#[tokio::test]
async fn a_client_that_stops_by_itself_is_started_again() {
    let mut h = Harness::start().await;
    h.connected().await;
    let (second, connection) = FakeClient::new();
    h.connector.push(Ok(connection));
    h.client.emit(WaEvent::Stopped("Stopped".into()));
    let message = error_message(next_status(&mut h.events).await);
    assert!(message.contains("starting it again"), "{message}");
    // The new client takes over.
    wait_until(|| h.connector.connects() == 2).await;
    second.emit(WaEvent::Connected);
    expect_status(&mut h.events, &ChannelStatus::Connected).await;
    assert!(
        h.client
            .backend
            .shut_down
            .load(std::sync::atomic::Ordering::SeqCst)
    );
}

#[tokio::test]
async fn a_client_whose_events_end_is_started_again() {
    let mut h = Harness::start().await;
    h.connected().await;
    let (second, connection) = FakeClient::new();
    h.connector.push(Ok(connection));
    h.client.close();
    let message = error_message(next_status(&mut h.events).await);
    assert!(message.contains("event stream ended"), "{message}");
    wait_until(|| h.connector.connects() == 2).await;
    second.emit(WaEvent::Connected);
    expect_status(&mut h.events, &ChannelStatus::Connected).await;
}

#[tokio::test]
async fn a_client_that_cannot_start_is_retried_with_a_delay() {
    let (client, connection) = FakeClient::new();
    let connector = Arc::new(FakeConnector::default());
    connector.push(Err("the session store is locked".into()));
    connector.push(Err("the session store is locked".into()));
    connector.push(Ok(connection));
    let dir = tempfile::tempdir().unwrap();
    let channel = WhatsApp::start(
        settings(Link::Qr),
        Arc::clone(&connector) as Arc<dyn Connector>,
        Arc::new(MemoryMedia::default()),
        DataDir::open(dir.path()).unwrap(),
        fast(),
    );
    let mut events = channel.events().await.unwrap();
    let message = error_message(next_status(&mut events).await);
    assert!(message.contains("the session store is locked"), "{message}");
    wait_until(|| connector.connects() == 3).await;
    client.emit(WaEvent::Connected);
    // The repeated failure said the same thing, so it was reported once.
    expect_status(&mut events, &ChannelStatus::Connected).await;
}

// ------------------------------------------------------------------------------------------------
// Inbound events and durability

#[tokio::test]
async fn a_message_is_acknowledged_only_after_the_sdk_polls_again() {
    let mut h = Harness::start().await;
    h.connected().await;
    let mut durable = h.client.deliver(vec![dm("3EB0AAAA", "hello")]);
    let (id, message) = h.message().await;
    assert_eq!(message.text, "hello");
    assert!(message.sender_authenticated);
    assert_eq!(
        id,
        format!("{PHONE}@s.whatsapp.net:{PHONE}@s.whatsapp.net:3EB0AAAA")
    );
    // The SDK is persisting the event: WhatsApp isn't told yet.
    assert!(pending(&mut durable).await);
    // The SDK asks for the next event, which means the last one is on disk.
    expect_quiet(&mut h.events, Duration::from_millis(100)).await;
    assert_eq!(tokio::time::timeout(WAIT, durable).await.unwrap(), Ok(true));
}

#[tokio::test]
async fn a_batch_is_acknowledged_after_its_last_event_and_order_is_kept() {
    let mut h = Harness::start().await;
    h.connected().await;
    let mut first = h.client.deliver(vec![
        dm("3EB0A001", "one"),
        dm("3EB0A002", "two"),
        dm("3EB0A003", "three"),
    ]);
    let mut second = h.client.deliver(vec![dm("3EB0B001", "four")]);
    let (_, one) = h.message().await;
    let (_, two) = h.message().await;
    assert_eq!((one.text.as_str(), two.text.as_str()), ("one", "two"));
    // The batch isn't durable until its last event is.
    assert!(pending(&mut first).await);
    let (_, three) = h.message().await;
    assert_eq!(three.text, "three");
    assert!(pending(&mut first).await);
    // Polling again persists the third and so the batch; the next batch comes after.
    let (_, four) = h.message().await;
    assert_eq!(four.text, "four");
    assert_eq!(
        tokio::time::timeout(WAIT, &mut first).await.unwrap(),
        Ok(true)
    );
    assert!(pending(&mut second).await);
    expect_quiet(&mut h.events, Duration::from_millis(100)).await;
    assert_eq!(tokio::time::timeout(WAIT, second).await.unwrap(), Ok(true));
}

#[tokio::test]
async fn a_stalled_persist_keeps_the_message_unacknowledged() {
    // The SDK retries a failing write without polling the stream; here nobody polls at all.
    let mut h = Harness::start().await;
    h.connected().await;
    let mut durable = h.client.deliver(vec![dm("3EB0AAAA", "hello")]);
    let _ = h.message().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(pending(&mut durable).await);
    // Shutting down before it was persisted is a failure, so WhatsApp delivers it again.
    drop(h.channel);
    assert_eq!(
        tokio::time::timeout(WAIT, durable).await.unwrap(),
        Ok(false)
    );
}

#[tokio::test]
async fn a_stream_dropped_before_the_next_poll_does_not_acknowledge() {
    let mut h = Harness::start().await;
    h.connected().await;
    let durable = h.client.deliver(vec![dm("3EB0AAAA", "hello")]);
    let _ = h.message().await;
    // The SDK's pump ended (it asks `events()` again): we can't say the event was persisted.
    drop(std::mem::replace(&mut h.events, stream::pending().boxed()));
    assert_eq!(
        tokio::time::timeout(WAIT, durable).await.unwrap(),
        Ok(false)
    );
    // The redelivered message comes through the next stream.
    let mut events = h.channel.events().await.unwrap();
    let mut again = h.client.deliver(vec![dm("3EB0AAAA", "hello")]);
    let (_, message) = next_message(&mut events).await;
    assert_eq!(message.text, "hello");
    expect_quiet(&mut events, Duration::from_millis(100)).await;
    assert_eq!(
        tokio::time::timeout(WAIT, &mut again).await.unwrap(),
        Ok(true)
    );
}

#[tokio::test]
async fn messages_that_are_dropped_are_acknowledged_without_waiting() {
    let mut h = Harness::start().await;
    h.connected().await;
    let mut echo = dm("3EB0AAAA", "my own reply");
    echo.from_me = true;
    let mut own_status = dm("3EB0AAAB", "status");
    own_status.kind = crate::backend::ChatKind::Other;
    let durable = h.client.deliver(vec![echo, own_status]);
    // Nobody polls the stream, and it doesn't matter: there's nothing to keep.
    assert_eq!(tokio::time::timeout(WAIT, durable).await.unwrap(), Ok(true));
    expect_quiet(&mut h.events, Duration::from_millis(100)).await;
}

#[tokio::test]
async fn a_message_that_could_not_be_kept_is_not_acknowledged() {
    let mut h = Harness::start().await;
    h.connected().await;
    h.client
        .backend
        .downloads
        .lock()
        .unwrap()
        .insert("/v/img".into(), b"x".to_vec());
    h.media
        .fail_puts
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut message = dm("3EB0AAAA", "photo");
    message.media = Some(image("/v/img", Some(1)));
    let durable = h.client.deliver(vec![message]);
    assert_eq!(
        tokio::time::timeout(WAIT, durable).await.unwrap(),
        Ok(false)
    );
    expect_quiet(&mut h.events, Duration::from_millis(100)).await;
    // WhatsApp delivers it again once the disk has room.
    h.media
        .fail_puts
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let mut again = dm("3EB0AAAA", "photo");
    again.media = Some(image("/v/img", Some(1)));
    let durable = h.client.deliver(vec![again]);
    let (_, message) = h.message().await;
    assert_eq!(message.attachments.len(), 1);
    expect_quiet(&mut h.events, Duration::from_millis(100)).await;
    assert_eq!(tokio::time::timeout(WAIT, durable).await.unwrap(), Ok(true));
}

#[tokio::test]
async fn status_and_messages_keep_their_arrival_order() {
    let mut h = Harness::start().await;
    h.client.emit(WaEvent::Connected);
    let durable = h.client.deliver(vec![dm("3EB0AAAA", "first")]);
    // The status comes first, then the message, whatever the polling does in between.
    assert_eq!(next_status(&mut h.events).await, ChannelStatus::Connected);
    let (_, message) = h.message().await;
    assert_eq!(message.text, "first");
    expect_quiet(&mut h.events, Duration::from_millis(100)).await;
    assert_eq!(tokio::time::timeout(WAIT, durable).await.unwrap(), Ok(true));
}

// ------------------------------------------------------------------------------------------------
// Sends

fn reply(text: &str) -> OutboundMessage {
    OutboundMessage {
        target: SendTarget::Conversation(
            ConversationId::new(format!("{PHONE}@s.whatsapp.net")).unwrap(),
        ),
        markdown: text.to_owned(),
        attachments: vec![],
        reply_to: None,
    }
}

#[tokio::test]
async fn a_reply_goes_out_through_the_connected_client() {
    let mut h = Harness::start().await;
    h.connected().await;
    let id = h.channel.send(reply("**hi**")).await.unwrap();
    assert_eq!(id.as_str(), "3EB00000");
    assert_eq!(
        h.client.backend.sent(),
        [Sent::Text {
            to: format!("{PHONE}@s.whatsapp.net"),
            text: "*hi*".into()
        }]
    );
}

#[tokio::test]
async fn a_reply_waits_for_a_dropped_connection_to_come_back() {
    let mut h = Harness::start().await;
    h.connected().await;
    h.client.emit(WaEvent::Disconnected("reset".into()));
    tokio::time::sleep(Duration::from_millis(20)).await;
    let send = h.channel.send(reply("still there?"));
    let reconnect = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        h.client.emit(WaEvent::Connected);
    };
    let (sent, ()) = tokio::join!(send, reconnect);
    sent.unwrap();
    assert_eq!(h.client.backend.sent().len(), 1);
}

#[tokio::test]
async fn a_reply_gives_up_when_the_connection_stays_down() {
    let h = Harness::start().await;
    // Never connected.
    let started = Instant::now();
    let error = h.channel.send(reply("hello?")).await.unwrap_err();
    assert!(error.is_retryable(), "{error}");
    assert!(started.elapsed() >= Duration::from_millis(250));
    assert!(h.client.backend.sent().is_empty());
}

#[tokio::test]
async fn a_proactive_send_does_not_wait_for_the_connection() {
    let h = Harness::start().await;
    let started = Instant::now();
    let error = h
        .channel
        .send(OutboundMessage {
            target: SendTarget::Recipient(NativeId::new("+15557654321").unwrap()),
            markdown: "reminder".into(),
            attachments: vec![],
            reply_to: None,
        })
        .await
        .unwrap_err();
    assert!(error.is_retryable(), "{error}");
    assert!(started.elapsed() < Duration::from_millis(250));
}

#[tokio::test]
async fn a_proactive_send_resolves_the_recipient() {
    let mut h = Harness::start().await;
    h.connected().await;
    h.client
        .backend
        .registered
        .lock()
        .unwrap()
        .insert(PHONE.into(), format!("{PHONE}@s.whatsapp.net"));
    let send = |id: &str| OutboundMessage {
        target: SendTarget::Recipient(NativeId::new(id).unwrap()),
        markdown: "reminder".into(),
        attachments: vec![],
        reply_to: None,
    };
    h.channel.send(send("+15557654321")).await.unwrap();
    assert_eq!(h.client.backend.sent().len(), 1);
    let error = h.channel.send(send("+15550000000")).await.unwrap_err();
    assert!(error.is_retryable(), "{error}");
    assert_eq!(h.client.backend.sent().len(), 1);
    // A failing lookup is retryable too.
    *h.client.backend.fail.lock().unwrap() = Some(BackendError::Unavailable("offline".into()));
    assert!(
        h.channel
            .send(send("+15557654321"))
            .await
            .unwrap_err()
            .is_retryable()
    );
}

#[tokio::test]
async fn the_linked_accounts_own_number_is_an_echo_source() {
    let mut h = Harness::start().await;
    h.connected().await;
    h.client
        .backend
        .own
        .lock()
        .unwrap()
        .push(Address::phone(PHONE));
    let durable = h.client.deliver(vec![dm("3EB0AAAA", "from my own number")]);
    assert_eq!(tokio::time::timeout(WAIT, durable).await.unwrap(), Ok(true));
    expect_quiet(&mut h.events, Duration::from_millis(100)).await;
}

// ------------------------------------------------------------------------------------------------
// Shutdown

#[tokio::test]
async fn a_normal_stop_disconnects_the_client_and_waits_for_it() {
    let mut h = Harness::start().await;
    h.connected().await;
    assert!(
        !h.client
            .backend
            .shut_down
            .load(std::sync::atomic::Ordering::SeqCst)
    );
    h.channel.take_session().shutdown(WAIT).await;
    // Done by the time `shutdown` returns, not eventually.
    assert!(
        h.client
            .backend
            .shut_down
            .load(std::sync::atomic::Ordering::SeqCst)
    );
}

#[tokio::test]
async fn the_slot_main_waits_on_stops_the_session_once() {
    let mut h = Harness::start().await;
    h.connected().await;
    let slot = SessionSlot::default();
    // Nothing to stop yet.
    slot.shutdown(WAIT).await;
    slot.set(h.channel.take_session());
    slot.shutdown(WAIT).await;
    assert!(
        h.client
            .backend
            .shut_down
            .load(std::sync::atomic::Ordering::SeqCst)
    );
    // A second call has nothing left to do.
    slot.shutdown(WAIT).await;
}

#[tokio::test]
async fn a_client_stuck_closing_does_not_hold_up_the_exit() {
    let mut h = Harness::start().await;
    h.connected().await;
    h.client
        .backend
        .hang_on_shutdown
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let started = Instant::now();
    h.channel
        .take_session()
        .shutdown(Duration::from_millis(200))
        .await;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(
        h.client
            .backend
            .shut_down
            .load(std::sync::atomic::Ordering::SeqCst)
    );
}

#[tokio::test]
async fn a_stop_while_connecting_ends_the_session() {
    // A connector that never answers, like a store that can't be opened.
    struct Stuck;
    #[async_trait]
    impl Connector for Stuck {
        async fn connect(&self) -> std::result::Result<crate::backend::Connection, String> {
            std::future::pending().await
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let channel = WhatsApp::start(
        settings(Link::Qr),
        Arc::new(Stuck),
        Arc::new(MemoryMedia::default()),
        DataDir::open(dir.path()).unwrap(),
        fast(),
    );
    let started = Instant::now();
    channel.take_session().shutdown(WAIT).await;
    // The session ended on the stop, not on the wait running out.
    assert!(started.elapsed() < Duration::from_secs(2));
}

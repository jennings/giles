//! The channel against fake IMAP and SMTP servers.

use std::path::Path;

use giles_plugin_sdk::giles_types::{ConversationId, MediaRef, NativeId};
use mail_parser::{HeaderValue, MessageParser, MimeHeaders as _};
use serde_json::{Value, json};

use super::*;
use crate::testing::{
    FakeImap, FakeSmtp, MemoryMedia, Security, WAIT, no_tls, test_tls, wait_until,
};

type Events = BoxStream<'static, Result<ChannelEvent>>;

fn config(imap: &FakeImap, smtp: &FakeSmtp) -> Value {
    json!({
        "authserv_id": "mx.example.net",
        "address": "giles@example.net",
        "name": "Giles",
        "imap": {
            "host": "127.0.0.1",
            "port": imap.addr.port(),
            "tls": "none",
            "user": "giles",
            "password": "secret",
        },
        "smtp": {
            "host": "127.0.0.1",
            "port": smtp.addr.port(),
            "tls": "none",
        },
    })
}

async fn channel_with(
    config: Value,
    dir: &Path,
    media: &Arc<MemoryMedia>,
    tls: Arc<ClientConfig>,
) -> EmailChannel {
    let media: Arc<dyn MediaStore> = Arc::clone(media) as _;
    EmailChannel::new(Config::from_json(&config).unwrap(), dir, media, tls)
        .await
        .unwrap()
}

/// The next event, within [`WAIT`].
async fn next(events: &mut Events) -> ChannelEvent {
    tokio::time::timeout(WAIT, events.next())
        .await
        .expect("an event in time")
        .expect("the stream doesn't end")
        .expect("no stream error")
}

/// The next event, which must be a message.
async fn next_message(events: &mut Events) -> (String, InboundMessage) {
    match next(events).await {
        ChannelEvent::Message { id, message } => (id.as_str().to_owned(), message),
        other => panic!("expected a message, got {other:?}"),
    }
}

async fn expect_status(events: &mut Events, expected: &ChannelStatus) {
    match next(events).await {
        ChannelEvent::Status { status, .. } if status == *expected => {}
        other => panic!("expected {expected:?}, got {other:?}"),
    }
}

/// Polls the stream briefly; nothing may arrive. Like the SDK's next poll, this also confirms
/// the previous event as persisted.
async fn expect_quiet(events: &mut Events) {
    expect_quiet_for(events, Duration::from_millis(300)).await;
}

/// Polls the stream for `duration`; nothing may arrive.
async fn expect_quiet_for(events: &mut Events, duration: Duration) {
    let polled = tokio::time::timeout(duration, events.next()).await;
    assert!(polled.is_err(), "unexpected event: {polled:?}");
}

/// A message from the owner, authenticated by the receiving server.
fn owner_mail(id: &str, extra_headers: &str, body: &str) -> Vec<u8> {
    format!(
        "Authentication-Results: mx.example.net; dmarc=pass header.from=example.com\r\n\
         From: Stephen <me@example.com>\r\n\
         To: giles@example.net\r\n\
         Subject: Groceries\r\n\
         Date: Thu, 22 Oct 2026 14:03:00 -0700\r\n\
         Message-ID: <{id}>\r\n{extra_headers}\r\n{body}\r\n"
    )
    .into_bytes()
}

struct Setup {
    imap: FakeImap,
    smtp: FakeSmtp,
    dir: tempfile::TempDir,
    media: Arc<MemoryMedia>,
}

impl Setup {
    async fn new(can_idle: bool) -> Self {
        Self {
            imap: FakeImap::start(Security::Plain, can_idle).await,
            smtp: FakeSmtp::start(Security::Plain, None).await,
            dir: tempfile::tempdir().unwrap(),
            media: Arc::default(),
        }
    }

    async fn channel(&self) -> EmailChannel {
        self.channel_with(config(&self.imap, &self.smtp)).await
    }

    async fn channel_with(&self, config: Value) -> EmailChannel {
        channel_with(config, self.dir.path(), &self.media, no_tls()).await
    }

    /// A channel whose stream has connected (and reported it).
    async fn connected(&self) -> (EmailChannel, Events) {
        let channel = self.channel().await;
        let mut events = channel.events().await.unwrap();
        expect_status(&mut events, &ChannelStatus::Connected).await;
        (channel, events)
    }
}

#[tokio::test]
async fn new_mail_arrives_with_idle_and_old_mail_is_ignored() {
    let setup = Setup::new(true).await;
    setup
        .imap
        .append(&owner_mail("old@example.com", "", "Old news."));
    let (_channel, mut events) = setup.connected().await;
    let validity = setup.imap.uid_validity();

    // The stream is waiting in IDLE when the message arrives.
    let (event, uid) = tokio::join!(next_message(&mut events), async {
        wait_until(|| setup.imap.idles() == 1).await;
        setup
            .imap
            .append(&owner_mail("new@example.com", "", "Buy milk."))
    });
    let (id, message) = event;
    assert_eq!(id, format!("{validity}:{uid}"));
    assert_eq!(uid, 2);
    assert_eq!(message.sender.as_str(), "me@example.com");
    assert!(message.sender_authenticated);
    assert_eq!(message.sender_display_name.as_deref(), Some("Stephen"));
    assert_eq!(
        message.conversation.id.as_str(),
        "me@example.com/new@example.com"
    );
    assert_eq!(message.conversation.kind, ConversationKind::Direct);
    assert_eq!(message.conversation.title.as_deref(), Some("Groceries"));
    assert_eq!(message.text, "Subject: Groceries\n\nBuy milk.");
    assert_eq!(message.message_id.unwrap().as_str(), "new@example.com");
    assert_eq!(message.reply_to, None);
    assert!(message.sent_at.is_some());
    expect_quiet(&mut events).await;
    assert!(
        setup
            .imap
            .commands()
            .iter()
            .all(|c| !c.contains("BODY.PEEK[HEADER]")),
        "small messages are fetched whole"
    );
}

#[tokio::test]
async fn servers_without_idle_are_polled() {
    let setup = Setup::new(false).await;
    let mut config = config(&setup.imap, &setup.smtp);
    config["imap"]["poll_interval"] = json!("50ms");
    let channel = setup.channel_with(config).await;
    let mut events = channel.events().await.unwrap();
    expect_status(&mut events, &ChannelStatus::Connected).await;
    let (event, _) = tokio::join!(next_message(&mut events), async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        setup.imap.append(&owner_mail("a@example.com", "", "hi"))
    });
    assert_eq!(event.1.text, "Subject: Groceries\n\nhi");
    assert_eq!(setup.imap.idles(), 0);
    assert!(setup.imap.commands().iter().any(|c| c == "NOOP"));
}

#[tokio::test]
async fn the_cursor_survives_a_restart_without_duplicates() {
    let setup = Setup::new(true).await;
    let validity = setup.imap.uid_validity();
    {
        let (channel, mut events) = setup.connected().await;
        setup.imap.append(&owner_mail("one@example.com", "", "one"));
        setup.imap.append(&owner_mail("two@example.com", "", "two"));
        setup
            .imap
            .append(&owner_mail("three@example.com", "", "three"));
        // One persisted by the SDK (the stream was polled again), and that one acked.
        assert_eq!(next_message(&mut events).await.0, format!("{validity}:1"));
        assert_eq!(next_message(&mut events).await.0, format!("{validity}:2"));
        channel
            .ack(&EventId::new(format!("{validity}:2")).unwrap())
            .await
            .unwrap();
        // Three is yielded but never confirmed: the plugin dies before the SDK persists it.
        assert_eq!(next_message(&mut events).await.0, format!("{validity}:3"));
    }
    setup
        .imap
        .append(&owner_mail("four@example.com", "", "four"));
    let (_channel, mut events) = setup.connected().await;
    // At least once: three again (same event id, so the core drops the copy), then four.
    assert_eq!(next_message(&mut events).await.0, format!("{validity}:3"));
    assert_eq!(next_message(&mut events).await.0, format!("{validity}:4"));
    expect_quiet(&mut events).await;
    drop(events);

    let (_channel, mut events) = setup.connected().await;
    expect_quiet(&mut events).await;
}

#[tokio::test]
async fn a_recreated_mailbox_restarts_the_cursor() {
    let setup = Setup::new(true).await;
    let (_channel, mut events) = setup.connected().await;
    setup.imap.append(&owner_mail("one@example.com", "", "one"));
    assert_eq!(next_message(&mut events).await.0, "1000:1");

    setup.imap.recreate(2000);
    let (event, uid) = tokio::join!(next_message(&mut events), async {
        // Reconnected, the cursor reset to the new mailbox's end, and idling again.
        wait_until(|| setup.imap.logins() == 2 && setup.imap.idles() == 1).await;
        setup.imap.append(&owner_mail("two@example.com", "", "two"))
    });
    assert_eq!(event.0, format!("2000:{uid}"));
    assert_eq!(event.1.text, "Subject: Groceries\n\ntwo");
    // No status event: reconnecting worked at once.
    expect_quiet(&mut events).await;
}

#[tokio::test]
async fn failures_are_reported_and_retried() {
    let setup = Setup::new(true).await;
    setup.imap.fail_logins(1);
    let channel = setup.channel().await;
    let mut events = channel.events().await.unwrap();
    match next(&mut events).await {
        ChannelEvent::Status {
            status: ChannelStatus::Error { message },
            ..
        } => {
            assert!(message.contains("login"), "{message}");
            assert!(!message.contains("secret"), "{message}");
        }
        other => panic!("expected an error status, got {other:?}"),
    }
    // Retried after a second.
    expect_status(&mut events, &ChannelStatus::Connected).await;
    assert_eq!(setup.imap.logins(), 1);

    // A dropped connection is reopened silently.
    let (event, _) = tokio::join!(next_message(&mut events), async {
        wait_until(|| setup.imap.idles() == 1).await;
        setup.imap.drop_connections();
        wait_until(|| setup.imap.idles() == 2).await;
        setup.imap.append(&owner_mail("a@example.com", "", "after"))
    });
    assert_eq!(event.1.text, "Subject: Groceries\n\nafter");
}

#[tokio::test]
async fn automated_and_own_mail_is_skipped() {
    let setup = Setup::new(true).await;
    let (_channel, mut events) = setup.connected().await;
    setup.imap.append(&owner_mail(
        "auto@example.com",
        "Auto-Submitted: auto-replied\r\n",
        "I'm away.",
    ));
    setup.imap.append(&owner_mail(
        "list@example.com",
        "List-Id: <friends.example.com>\r\n",
        "Hi all",
    ));
    setup.imap.append(
        b"From: Giles <giles@example.net>\r\nMessage-ID: <giles.abc@example.net>\r\n\r\nours\r\n",
    );
    setup
        .imap
        .append(&owner_mail("real@example.com", "", "real"));
    let (id, message) = next_message(&mut events).await;
    assert_eq!(id, "1000:4");
    assert_eq!(message.text, "Subject: Groceries\n\nreal");
    expect_quiet(&mut events).await;
    // The skipped ones moved the cursor too.
    let cursor = setup.channel().await.shared.cursor.get().await.unwrap();
    assert_eq!(cursor.uid, 4);
}

#[tokio::test]
async fn attachments_become_blobs_and_large_mail_is_cut() {
    let setup = Setup::new(true).await;
    let (_channel, mut events) = setup.connected().await;
    let multipart = owner_mail(
        "files@example.com",
        "MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=b\r\n",
        "--b\r\nContent-Type: text/plain\r\n\r\nSee attached.\r\n\
         --b\r\nContent-Type: text/csv; name=list.csv\r\n\
         Content-Disposition: attachment; filename=list.csv\r\n\r\nmilk,eggs\r\n--b--",
    );
    setup.imap.append(&multipart);
    let (_, message) = next_message(&mut events).await;
    assert_eq!(message.text, "Subject: Groceries\n\nSee attached.");
    let [media] = message.attachments.as_slice() else {
        panic!("one attachment: {:?}", message.attachments);
    };
    assert_eq!(media.media_type, "text/csv");
    assert_eq!(media.name.as_deref(), Some("list.csv"));
    assert_eq!(setup.media.get(&media.id).unwrap(), b"milk,eggs");
    // Confirmed (the SDK persisted it), so the next channel starts after it.
    expect_quiet(&mut events).await;
    drop(events);

    // Over the limit: headers only, and a note.
    let mut config = config(&setup.imap, &setup.smtp);
    config["max_message_size"] = json!(200);
    let channel = setup.channel_with(config).await;
    let mut events = channel.events().await.unwrap();
    expect_status(&mut events, &ChannelStatus::Connected).await;
    let uid = setup.imap.append(&multipart);
    let (_, message) = next_message(&mut events).await;
    assert!(message.attachments.is_empty());
    assert!(
        message
            .text
            .starts_with("Subject: Groceries\n\n[message body not received: it is "),
        "{}",
        message.text
    );
    assert!(
        message.text.ends_with("limit of 200 bytes]"),
        "{}",
        message.text
    );
    assert!(
        setup
            .imap
            .commands()
            .contains(&format!("UID FETCH {uid} (UID BODY.PEEK[HEADER])"))
    );
}

/// A received message, parsed.
fn sent(
    smtp: &FakeSmtp,
    index: usize,
) -> (crate::testing::Received, mail_parser::Message<'static>) {
    let received = smtp.received()[index].clone();
    let message = MessageParser::default()
        .parse(&received.data)
        .unwrap()
        .into_owned();
    (received, message)
}

fn outbound(target: SendTarget, markdown: &str, attachments: Vec<MediaRef>) -> OutboundMessage {
    OutboundMessage {
        target,
        markdown: markdown.into(),
        attachments,
        reply_to: None,
    }
}

#[tokio::test]
async fn replies_thread_under_the_conversation() {
    let setup = Setup::new(true).await;
    let (channel, mut events) = setup.connected().await;
    setup.imap.append(&owner_mail(
        "q@example.com",
        "References: <root@example.com>\r\nIn-Reply-To: <root@example.com>\r\n",
        "Anything else?",
    ));
    let (_, message) = next_message(&mut events).await;
    assert_eq!(message.text, "Anything else?");
    assert_eq!(
        message.reply_to.as_ref().unwrap().as_str(),
        "root@example.com"
    );
    let conversation = message.conversation.id.clone();
    assert_eq!(conversation.as_str(), "me@example.com/root@example.com");

    let attachment = setup
        .media
        .put(
            b"a,b\n".to_vec(),
            "text/csv".into(),
            Some("list.csv".into()),
        )
        .await
        .unwrap();
    let first = channel
        .send(outbound(
            SendTarget::Conversation(conversation.clone()),
            "**Eggs.** ![tracker](https://evil.example/t.png)",
            vec![attachment],
        ))
        .await
        .unwrap();
    let (received, mail) = sent(&setup.smtp, 0);
    assert_eq!(received.from, "giles@example.net");
    assert_eq!(received.to, ["me@example.com"]);
    assert_eq!(mail.message_id(), Some(first.as_str()));
    assert!(first.as_str().starts_with("giles.") && first.as_str().ends_with("@example.net"));
    assert_eq!(mail.subject(), Some("Re: Groceries"));
    assert_eq!(
        mail.in_reply_to(),
        &HeaderValue::Text("q@example.com".into())
    );
    assert_eq!(
        mail.references(),
        &HeaderValue::TextList(vec!["root@example.com".into(), "q@example.com".into()])
    );
    let auto = mail.header_raw("Auto-Submitted").unwrap().trim().to_owned();
    assert_eq!(auto, "auto-replied");
    let to = mail.to().unwrap().first().unwrap();
    assert_eq!(
        (to.name(), to.address()),
        (Some("Stephen"), Some("me@example.com"))
    );
    assert_eq!(
        mail.body_text(0).unwrap().trim(),
        "*Eggs.* [image: tracker]"
    );
    let html = mail.body_html(0).unwrap();
    assert!(
        html.contains("<strong>Eggs.</strong>") && !html.contains("evil.example"),
        "{html}"
    );
    assert_eq!(
        mail.attachment(0).unwrap().attachment_name(),
        Some("list.csv")
    );
    assert_eq!(mail.attachment(0).unwrap().contents(), b"a,b\n");

    // A second reply answers the first and keeps the chain.
    channel
        .send(outbound(
            SendTarget::Conversation(conversation),
            "More.",
            vec![],
        ))
        .await
        .unwrap();
    let (_, mail) = sent(&setup.smtp, 1);
    assert_eq!(
        mail.in_reply_to(),
        &HeaderValue::Text(first.as_str().into())
    );
    assert_eq!(
        mail.references(),
        &HeaderValue::TextList(vec![
            "root@example.com".into(),
            "q@example.com".into(),
            first.as_str().into()
        ])
    );

    // Our copy of it in the mailbox is skipped.
    setup.imap.append(&received.data);
    expect_quiet(&mut events).await;
}

#[tokio::test]
async fn proactive_mail_starts_a_thread_the_owner_can_answer() {
    let setup = Setup::new(true).await;
    let (channel, mut events) = setup.connected().await;
    let id = channel
        .send(outbound(
            SendTarget::Recipient(NativeId::new("me@Example.com").unwrap()),
            "Reminder: **water the plants**.\n\nThey're thirsty.",
            vec![],
        ))
        .await
        .unwrap();
    let (received, mail) = sent(&setup.smtp, 0);
    assert_eq!(received.to, ["me@example.com"]);
    assert_eq!(mail.subject(), Some("Reminder: *water the plants*."));
    assert_eq!(
        mail.header_raw("Auto-Submitted").unwrap().trim(),
        "auto-generated"
    );
    assert_eq!(mail.in_reply_to(), &HeaderValue::Empty);
    assert_eq!(mail.references(), &HeaderValue::Empty);

    // The owner answers it.
    setup.imap.append(&owner_mail(
        "answer@example.com",
        &format!("In-Reply-To: <{id}>\r\nReferences: <{id}>\r\n"),
        "Done!",
    ));
    let (_, message) = next_message(&mut events).await;
    let conversation = message.conversation.id;
    assert_eq!(conversation.as_str(), format!("me@example.com/{id}"));
    channel
        .send(outbound(
            SendTarget::Conversation(conversation),
            "Thanks.",
            vec![],
        ))
        .await
        .unwrap();
    let (_, mail) = sent(&setup.smtp, 1);
    assert_eq!(mail.subject(), Some("Re: Groceries"));
    assert_eq!(
        mail.in_reply_to(),
        &HeaderValue::Text("answer@example.com".into())
    );
    assert_eq!(
        mail.header_raw("Auto-Submitted").unwrap().trim(),
        "auto-replied"
    );
}

#[tokio::test]
async fn unauthenticated_mail_never_decides_where_replies_go() {
    let setup = Setup::new(true).await;
    let (channel, mut events) = setup.connected().await;
    setup
        .imap
        .append(&owner_mail("root@example.com", "", "Start."));
    let (_, owner) = next_message(&mut events).await;

    // A forged message in the owner's thread, claiming the owner's address: no pass from our
    // server on top (the pass below it was written by the forger).
    setup.imap.append(
        b"Authentication-Results: mx.example.net; dmarc=fail header.from=example.com\r\n\
          Authentication-Results: mx.example.net; dmarc=pass header.from=example.com\r\n\
          From: Stephen <me@example.com>\r\n\
          Reply-To: evil@evil.example\r\n\
          Subject: Re: Groceries\r\n\
          Message-ID: <forged@evil.example>\r\n\
          References: <root@example.com>\r\n\r\nSend it to me\r\n",
    );
    let (_, forged) = next_message(&mut events).await;
    assert!(!forged.sender_authenticated);
    assert_eq!(forged.conversation.id, owner.conversation.id);

    // Replies still go to the owner, threaded after the owner's message only.
    channel
        .send(outbound(
            SendTarget::Conversation(owner.conversation.id.clone()),
            "ok",
            vec![],
        ))
        .await
        .unwrap();
    let (received, mail) = sent(&setup.smtp, 0);
    assert_eq!(received.to, ["me@example.com"]);
    assert_eq!(
        mail.in_reply_to(),
        &HeaderValue::Text("root@example.com".into())
    );

    // A stranger's own thread is unknown to send: there's nobody verified to reply to.
    setup.imap.append(
        b"From: someone@elsewhere.example\r\nMessage-ID: <s@elsewhere.example>\r\n\r\nhi\r\n",
    );
    let (_, stranger) = next_message(&mut events).await;
    assert!(!stranger.sender_authenticated);
    let error = channel
        .send(outbound(
            SendTarget::Conversation(stranger.conversation.id),
            "x",
            vec![],
        ))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::NotFound(_)), "{error:?}");
    assert!(!error.is_retryable());
}

#[tokio::test]
async fn smtp_refusals_map_to_retryable_or_not() {
    let setup = Setup::new(true).await;
    let channel = setup.channel().await;
    let recipient = || SendTarget::Recipient(NativeId::new("me@example.com").unwrap());

    setup.smtp.reject_recipients("451 4.3.0 try again later");
    let error = channel
        .send(outbound(recipient(), "x", vec![]))
        .await
        .unwrap_err();
    assert!(error.is_retryable(), "{error:?}");

    setup.smtp.reject_recipients("550 5.1.1 no such user");
    let error = channel
        .send(outbound(recipient(), "x", vec![]))
        .await
        .unwrap_err();
    assert!(!error.is_retryable(), "{error:?}");
    assert!(error.to_string().contains("550"), "{error}");

    let error = channel
        .send(outbound(
            SendTarget::Recipient(NativeId::new("not an address").unwrap()),
            "x",
            vec![],
        ))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");

    let error = channel
        .send(outbound(
            SendTarget::Conversation(ConversationId::new("nobody/none@x").unwrap()),
            "x",
            vec![],
        ))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::NotFound(_)), "{error:?}");

    // Nothing listening: retryable.
    let config = config(&setup.imap, &setup.smtp);
    drop(setup.smtp);
    let channel = channel_with(config, setup.dir.path(), &setup.media, no_tls()).await;
    let error = channel
        .send(outbound(recipient(), "x", vec![]))
        .await
        .unwrap_err();
    assert!(error.is_retryable(), "{error:?}");
}

/// A config for servers on `localhost` with TLS.
fn tls_config(imap: &FakeImap, imap_tls: &str, smtp: &FakeSmtp, smtp_tls: &str) -> Value {
    json!({
        "authserv_id": "mx.example.net",
        "address": "giles@example.net",
        "imap": {
            "host": "localhost",
            "port": imap.addr.port(),
            "tls": imap_tls,
            "user": "giles",
            "password": "secret",
        },
        "smtp": {
            "host": "localhost",
            "port": smtp.addr.port(),
            "tls": smtp_tls,
            "user": "giles",
            "password": "smtp-secret",
        },
    })
}

#[tokio::test]
async fn tls_implicit_and_starttls() {
    let tls = test_tls();
    for (mode, imap_security, smtp_security) in [
        (
            "implicit",
            Security::Implicit(tls.acceptor.clone()),
            Security::Implicit(tls.acceptor.clone()),
        ),
        (
            "starttls",
            Security::Starttls(tls.acceptor.clone()),
            Security::Starttls(tls.acceptor.clone()),
        ),
    ] {
        let imap = FakeImap::start(imap_security, true).await;
        let smtp = FakeSmtp::start(smtp_security, Some(("giles", "smtp-secret"))).await;
        let dir = tempfile::tempdir().unwrap();
        let media = Arc::default();
        let config = tls_config(&imap, mode, &smtp, mode);
        let channel = channel_with(config, dir.path(), &media, Arc::clone(&tls.client)).await;
        let mut events = channel.events().await.unwrap();
        expect_status(&mut events, &ChannelStatus::Connected).await;
        imap.append(&owner_mail("a@example.com", "", "over TLS"));
        let (_, message) = next_message(&mut events).await;
        assert_eq!(message.text, "Subject: Groceries\n\nover TLS", "{mode}");
        if mode == "starttls" {
            assert_eq!(imap.commands()[0], "STARTTLS");
        }

        channel
            .send(outbound(
                SendTarget::Conversation(message.conversation.id),
                "ok",
                vec![],
            ))
            .await
            .unwrap();
        let received = &smtp.received()[0];
        assert!(received.tls, "{mode}");
        assert_eq!(received.user.as_deref(), Some("giles"), "{mode}");
    }

    // The default trust store doesn't know the throwaway certificate.
    let imap = FakeImap::start(Security::Implicit(tls.acceptor.clone()), true).await;
    let smtp = FakeSmtp::start(Security::Implicit(tls.acceptor.clone()), None).await;
    let dir = tempfile::tempdir().unwrap();
    let media = Arc::default();
    let config = tls_config(&imap, "implicit", &smtp, "implicit");
    let channel = channel_with(config, dir.path(), &media, no_tls()).await;
    let mut events = channel.events().await.unwrap();
    match next(&mut events).await {
        ChannelEvent::Status {
            status: ChannelStatus::Error { message },
            ..
        } => assert!(message.contains("TLS"), "{message}"),
        other => panic!("expected an error status, got {other:?}"),
    }
    let error = channel
        .send(outbound(
            SendTarget::Recipient(NativeId::new("me@example.com").unwrap()),
            "x",
            vec![],
        ))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("TLS"), "{error}");
}

#[test]
fn event_ids_carry_the_cursor() {
    assert_eq!(parse_event_id("1000:42"), Some((1000, 42)));
    assert_eq!(parse_event_id("status:1:2"), None);
    assert_eq!(parse_event_id("1000"), None);
}

#[tokio::test]
async fn a_message_that_cannot_be_fetched_backs_off() {
    let setup = Setup::new(true).await;
    let (_channel, mut events) = setup.connected().await;
    setup.imap.break_fetch(1);
    assert_eq!(
        setup.imap.append(&owner_mail("bad@example.com", "", "x")),
        1
    );

    // The first failure reconnects quietly after 1 s; the second is reported.
    let started = tokio::time::Instant::now();
    match next(&mut events).await {
        ChannelEvent::Status {
            status: ChannelStatus::Error { message },
            ..
        } => assert!(message.contains("FETCH"), "{message}"),
        other => panic!("expected an error status, got {other:?}"),
    }
    assert!(started.elapsed() >= Duration::from_millis(900));
    assert_eq!(setup.imap.logins(), 2);
    // Then 2 s until the next attempt, not a login storm.
    expect_quiet_for(&mut events, Duration::from_millis(1500)).await;
    assert_eq!(setup.imap.logins(), 2);
    expect_quiet_for(&mut events, Duration::from_millis(1500)).await;
    assert_eq!(setup.imap.logins(), 3);
}

#[tokio::test]
async fn a_server_that_refuses_idle_backs_off() {
    let setup = Setup::new(true).await;
    setup.imap.refuse_idle();
    // Connecting and searching work, so it reports itself working at first.
    let (_channel, mut events) = setup.connected().await;

    // Every IDLE fails: quietly once, then reported, with growing pauses between logins.
    let started = tokio::time::Instant::now();
    match next(&mut events).await {
        ChannelEvent::Status {
            status: ChannelStatus::Error { message },
            ..
        } => assert!(message.contains("IDLE"), "{message}"),
        other => panic!("expected an error status, got {other:?}"),
    }
    assert!(started.elapsed() >= Duration::from_millis(900));
    assert_eq!(setup.imap.logins(), 2);
    expect_quiet_for(&mut events, Duration::from_millis(1500)).await;
    assert_eq!(setup.imap.logins(), 2);
    // The third attempt (2 s after the second) fails the same way: no new status.
    expect_quiet_for(&mut events, Duration::from_millis(1500)).await;
    assert_eq!(setup.imap.logins(), 3);
}

#[tokio::test]
async fn a_message_expunged_before_its_fetch_is_skipped() {
    let setup = Setup::new(true).await;
    let (channel, mut events) = setup.connected().await;
    setup.imap.expunge_on_fetch(1);
    setup
        .imap
        .append(&owner_mail("gone@example.com", "", "gone"));
    setup
        .imap
        .append(&owner_mail("kept@example.com", "", "kept"));
    let (id, message) = next_message(&mut events).await;
    assert_eq!(id, "1000:2");
    assert_eq!(message.text, "Subject: Groceries\n\nkept");
    expect_quiet(&mut events).await;
    assert_eq!(channel.shared.cursor.get().await.unwrap().uid, 2);
}

#[tokio::test]
async fn idle_restarts_on_time_despite_keepalives() {
    let setup = Setup::new(true).await;
    setup.imap.keepalive(Duration::from_millis(50));
    let mut config = config(&setup.imap, &setup.smtp);
    config["imap"]["idle_timeout"] = json!("300ms");
    let channel = setup.channel_with(config).await;
    let mut events = channel.events().await.unwrap();
    expect_status(&mut events, &ChannelStatus::Connected).await;
    expect_quiet_for(&mut events, Duration::from_millis(1000)).await;
    let idles = setup.imap.idles();
    assert!(idles >= 3, "{idles} IDLEs in 1 s");
    assert!(setup.imap.commands().iter().all(|c| c != "NOOP"));
}

#[tokio::test]
async fn mail_announced_with_a_search_response_is_not_missed() {
    let setup = Setup::new(true).await;
    let (_channel, mut events) = setup.connected().await;
    // Delivered while the next search runs: its only announcement is an EXISTS in the search's
    // response, before IDLE starts (and IDLE's timeout is 10 minutes).
    setup
        .imap
        .append_during_next_search(&owner_mail("late@example.com", "", "late"));
    let (id, message) = tokio::time::timeout(Duration::from_secs(2), next_message(&mut events))
        .await
        .expect("the message in time");
    assert_eq!(id, "1000:1");
    assert_eq!(message.text, "Subject: Groceries\n\nlate");
    assert_eq!(setup.imap.idles(), 0, "searched again instead of idling");
}

#[tokio::test]
async fn plain_text_injected_at_starttls_is_ignored() {
    let tls = test_tls();
    let imap = FakeImap::start(Security::Starttls(tls.acceptor.clone()), true).await;
    imap.inject_at_starttls();
    let smtp = FakeSmtp::start(Security::Starttls(tls.acceptor.clone()), None).await;
    let dir = tempfile::tempdir().unwrap();
    let media = Arc::default();
    let config = tls_config(&imap, "starttls", &smtp, "starttls");
    let channel = channel_with(config, dir.path(), &media, Arc::clone(&tls.client)).await;
    let mut events = channel.events().await.unwrap();
    // The forged "OK" for LOGIN was sent before TLS; the real answer over TLS is NO.
    match next(&mut events).await {
        ChannelEvent::Status {
            status: ChannelStatus::Error { message },
            ..
        } => assert!(message.contains("login"), "{message}"),
        other => panic!("expected an error status, got {other:?}"),
    }
    assert_eq!(imap.logins(), 0);
    assert_eq!(imap.commands()[0], "STARTTLS");
    assert!(
        imap.commands()[1].starts_with("LOGIN "),
        "{:?}",
        imap.commands()
    );
}

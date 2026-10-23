use super::*;

fn view() -> View<'static> {
    View {
        nick: "Giles",
        casemap: CaseMapping::Rfc1459,
        chantypes: "#&",
        statusmsg: "@+",
        account_tag: true,
        mask_identities: false,
    }
}

fn tags(pairs: &[(&str, &str)]) -> Vec<Tag> {
    pairs
        .iter()
        .map(|(k, v)| Tag((*k).to_owned(), Some((*v).to_owned())))
        .collect()
}

fn privmsg<'a>(tags: &'a [Tag], target: &'a str, text: &'a str) -> Privmsg<'a> {
    Privmsg {
        tags,
        nick: "Stephen",
        user: "~sj",
        host: "User/Stephen",
        target,
        text,
    }
}

fn message(view: &View<'_>, msg: &Privmsg<'_>) -> (EventId, InboundMessage) {
    match event(view, msg, "t0").unwrap() {
        Some(ChannelEvent::Message { id, message }) => (id, message),
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_account_tag_is_a_verified_account_identity() {
    let tags = tags(&[("account", "Stephen"), ("msgid", "abc123")]);
    let (id, msg) = message(&view(), &privmsg(&tags, "giles", "hello"));
    assert_eq!(id.as_str(), "msgid:abc123");
    assert_eq!(msg.sender.as_str(), "account:stephen");
    assert!(msg.sender_authenticated);
    assert!(msg.sender_aliases.is_empty());
    assert_eq!(msg.sender_display_name.as_deref(), Some("Stephen"));
    assert_eq!(
        msg.message_id.as_ref().map(MessageId::as_str),
        Some("abc123")
    );
    assert_eq!(msg.text, "hello");
}

#[test]
fn an_account_tag_counts_only_with_the_capability() {
    let tags = tags(&[("account", "stephen")]);
    let view = View {
        account_tag: false,
        ..view()
    };
    let (_, msg) = message(&view, &privmsg(&tags, "giles", "hi"));
    assert_eq!(msg.sender.as_str(), "mask:stephen!~sj@user/stephen");
    assert!(!msg.sender_authenticated);
}

#[test]
fn without_an_account_the_mask_is_unverified_unless_opted_in() {
    let (_, msg) = message(&view(), &privmsg(&[], "giles", "hi"));
    assert_eq!(msg.sender.as_str(), "mask:stephen!~sj@user/stephen");
    assert!(!msg.sender_authenticated);

    let masks = View {
        mask_identities: true,
        ..view()
    };
    let (_, msg) = message(&masks, &privmsg(&[], "giles", "hi"));
    assert_eq!(msg.sender.as_str(), "mask:stephen!~sj@user/stephen");
    assert!(msg.sender_authenticated);

    // With an account too, the account is canonical and the mask an alias.
    let tags = tags(&[("account", "sj")]);
    let (_, msg) = message(&masks, &privmsg(&tags, "giles", "hi"));
    assert_eq!(msg.sender.as_str(), "account:sj");
    assert_eq!(
        msg.sender_aliases,
        [NativeId::new("mask:stephen!~sj@user/stephen").unwrap()]
    );
    assert!(msg.sender_authenticated);

    // An incomplete source never verifies a mask.
    let incomplete = Privmsg {
        user: "",
        ..privmsg(&[], "giles", "hi")
    };
    let (_, msg) = message(&masks, &incomplete);
    assert!(!msg.sender_authenticated);
}

#[test]
fn queries_are_direct_by_nick_and_channels_are_groups() {
    let (_, msg) = message(&view(), &privmsg(&[], "GILES", "hi"));
    assert_eq!(msg.conversation.kind, ConversationKind::Direct);
    assert_eq!(msg.conversation.id.as_str(), "stephen");
    assert_eq!(msg.conversation.title.as_deref(), Some("Stephen"));

    let (_, msg) = message(&view(), &privmsg(&[], "#Giles[dev]", "hi"));
    assert_eq!(msg.conversation.kind, ConversationKind::Group);
    assert_eq!(msg.conversation.id.as_str(), "#giles{dev}");
    assert_eq!(msg.conversation.title.as_deref(), Some("#Giles[dev]"));

    // A message to the channel's ops is still the channel.
    let (_, msg) = message(&view(), &privmsg(&[], "@#giles", "hi"));
    assert_eq!(msg.conversation.id.as_str(), "#giles");
    assert_eq!(msg.conversation.kind, ConversationKind::Group);
}

#[test]
fn echoes_and_strays_are_dropped() {
    let echo = Privmsg {
        nick: "giles",
        ..privmsg(&[], "stephen", "my own reply")
    };
    assert_eq!(event(&view(), &echo, "t").unwrap(), None);
    // To neither us nor a channel (e.g. our previous nick).
    assert_eq!(
        event(&view(), &privmsg(&[], "oldnick", "hi"), "t").unwrap(),
        None
    );
    assert_eq!(
        event(&view(), &privmsg(&[], "giles", "\x02\x03"), "t").unwrap(),
        None
    );
    assert_eq!(
        event(&view(), &privmsg(&[], "giles", "\x01VERSION\x01"), "t").unwrap(),
        None
    );
}

#[test]
fn event_ids_without_msgid_are_stable_for_replays_and_unique_otherwise() {
    let id = |tags: &[Tag], text: &str, received: &str| match event(
        &view(),
        &privmsg(tags, "giles", text),
        received,
    )
    .unwrap()
    {
        Some(event) => event.id().clone(),
        None => panic!("no event"),
    };
    let time = tags(&[("time", "2026-10-23T12:00:00.000Z")]);
    // Same server time and content: the same id, whenever it arrives (a bouncer's replay).
    assert_eq!(id(&time, "yes", "t1"), id(&time, "yes", "t2"));
    assert!(id(&time, "yes", "t1").as_str().starts_with("sha256:"));
    assert_ne!(id(&time, "yes", "t1"), id(&time, "no", "t1"));
    // Without a time, every arrival is new: "yes" twice is two messages.
    assert_ne!(id(&[], "yes", "t1"), id(&[], "yes", "t2"));
}

#[test]
fn tags_fill_time_and_reply() {
    let tags = tags(&[
        ("time", "2026-10-23T12:00:00.123Z"),
        ("+draft/reply", "parent1"),
    ]);
    let (_, msg) = message(&view(), &privmsg(&tags, "giles", "hi"));
    assert_eq!(
        msg.sent_at,
        Some("2026-10-23T12:00:00.123Z".parse().unwrap())
    );
    assert_eq!(
        msg.reply_to.as_ref().map(MessageId::as_str),
        Some("parent1")
    );
    let bad = self::tags(&[("time", "yesterday")]);
    assert_eq!(
        message(&view(), &privmsg(&bad, "giles", "hi")).1.sent_at,
        None
    );
}

#[test]
fn text_is_cleaned() {
    assert_eq!(
        text("\x02bold\x02 \x1ditalic\x1d").as_deref(),
        Some("bold italic")
    );
    assert_eq!(text("\x0304,12red\x03 plain").as_deref(), Some("red plain"));
    assert_eq!(text("\x033,x").as_deref(), Some(",x"));
    assert_eq!(text("\x0312345").as_deref(), Some("345"));
    assert_eq!(text("\x04ff00ff,000000hex").as_deref(), Some("hex"));
    assert_eq!(text("a\tb\x07c").as_deref(), Some("a bc"));
    assert_eq!(text("\x01ACTION waves\x01").as_deref(), Some("/me waves"));
    assert_eq!(text("\x01ACTION waves").as_deref(), Some("/me waves"));
    assert_eq!(text("\x01ACTIONS\x01"), None);
    assert_eq!(text("\x01PING 123\x01"), None);
    assert_eq!(text("   "), None);
}

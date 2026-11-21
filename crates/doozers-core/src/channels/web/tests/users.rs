//! Two users, logged in through real sessions, against the pages of the other's data. Users are
//! made through the `Users` API: the imported owner (an admin), a second admin `root`, and a
//! member `mia`. Every request here carries only a session cookie, as a browser's does, so the
//! role comes from the stored users.
//!
//! Each test names what the other user tries and what must be left as it was.

use doozers_store::{ConversationAccess, InboxState, NewInboxEvent};

use super::admin::{session_of, stored_users};
use super::chat_actions::Chats;
use super::jobs::create as create_job;
use super::memory::{MEMORY, owner_viewer};
use super::ssh_keys::Ssh;
use super::toolsets::spec as toolset_spec;
use super::*;
use crate::ssh::KeyType;

/// What a user's browser sends.
struct Browser {
    token: String,
}

impl Browser {
    async fn log_in(env: &Env, user: &str) -> Self {
        Self {
            token: session_of(env, user).await,
        }
    }

    /// `method path` on `router`, with the session cookie and this login's CSRF token in the
    /// header and, for a post, the form (`fields` after the CSRF field).
    async fn send(
        &self,
        router: axum::Router,
        method: Method,
        path: &str,
        fields: &str,
    ) -> Response {
        let csrf = auth::cookie_csrf(&self.token);
        let cookie = session(&self.token);
        let form = format!("csrf={csrf}&{fields}");
        call(
            router,
            method.clone(),
            path,
            &[("cookie", &cookie), ("x-csrf-token", &csrf)],
            (method == Method::POST).then_some(form.as_str()),
        )
        .await
    }
}

fn mia_user() -> UserId {
    UserId::new("mia").unwrap()
}

/// Stores a chat of `user` in `chat` with one user message `text`.
async fn chat_of(env: &Env, user: &UserId, chat: &ConversationId, text: &str) {
    let (session, _) = env
        .store
        .open_session(
            SessionKey {
                agent: AgentId::new("jeeves").unwrap(),
                channel: INSTANCE.into(),
                conversation: chat.clone(),
            },
            SessionKind::Direct,
            Some(user.clone()),
            Timestamp::now(),
        )
        .await
        .unwrap();
    env.store
        .append_messages(
            session.id,
            vec![stored(Role::User, vec![super::text(text)])],
        )
        .await
        .unwrap();
}

fn chat_n(n: u8) -> ConversationId {
    ConversationId::new(format!("{n:02x}").repeat(16)).unwrap()
}

/// The status of every request, and a check that none of the bodies has `secret`.
async fn assert_all_not_found(
    t: &Chats,
    who: &Browser,
    secret: &str,
    requests: &[(Method, String, &str)],
) {
    for (method, path, fields) in requests {
        let response = who.send(t.router(None), method.clone(), path, fields).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method} {path}");
        let html = body_text(response).await;
        assert!(!html.contains(secret), "{method} {path}: {html}");
    }
}

#[tokio::test]
async fn another_users_chat_is_not_found_on_every_chat_route() {
    let t = Chats::new().await;
    let env = &t.env;
    stored_users(env).await;
    let owner_browser = Browser::log_in(env, "owner").await;
    let mia = Browser::log_in(env, "mia").await;
    let theirs = chat_n(0x11);
    chat_of(env, &identity::owner(), &theirs, "the owner secret plan").await;
    // The owner's chat has a prompt waiting for an answer and is archived apart from the list.
    env.channel
        .send_approval(&theirs, "4821", "Run `rm -rf /`?")
        .unwrap();
    let archived = chat_n(0x22);
    chat_of(
        env,
        &identity::owner(),
        &archived,
        "the owner archived secret",
    )
    .await;
    env.store
        .archive_conversation(INSTANCE, &archived, &identity::owner(), Timestamp::now())
        .await
        .unwrap();

    // The owner reads their chat.
    let response = owner_browser
        .send(t.router(None), Method::GET, &format!("/chats/{theirs}"), "")
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = body_text(response).await;
    assert!(page.contains("the owner secret plan"), "{page}");
    assert!(page.contains("4821"), "{page}");

    // Mia names the owner's chat on every route that takes a chat id.
    let mut requests = Vec::new();
    for chat in [&theirs, &archived] {
        requests.extend([
            (Method::GET, format!("/chats/{chat}"), ""),
            (Method::GET, format!("/chats/{chat}/events"), ""),
            (Method::GET, format!("/chats/{chat}/events?after=0"), ""),
            (
                Method::POST,
                format!("/chats/{chat}/messages"),
                "text=hello",
            ),
            (
                Method::POST,
                format!("/chats/{chat}/approvals"),
                "code=4821&answer=yes",
            ),
            (
                Method::POST,
                format!("/chats/{chat}/approvals"),
                "code=4821&answer=yes!",
            ),
            (Method::POST, format!("/chats/{chat}/archive"), ""),
            (Method::POST, format!("/chats/{chat}/unarchive"), ""),
            (Method::POST, format!("/chats/{chat}/delete"), ""),
        ]);
    }
    assert_all_not_found(&t, &mia, "secret", &requests).await;

    // Nothing was posted, answered, archived, unarchived, or deleted, and the prompt's buttons
    // are still on the owner's page.
    assert!(env.pending_texts().await.is_empty());
    assert_eq!(
        env.store
            .conversation_archived_at(INSTANCE, &theirs)
            .await
            .unwrap(),
        None
    );
    assert!(
        env.store
            .conversation_archived_at(INSTANCE, &archived)
            .await
            .unwrap()
            .is_some()
    );
    for chat in [&theirs, &archived] {
        let rows = env
            .store
            .conversation_transcript(INSTANCE, chat, &identity::owner(), 10)
            .await
            .unwrap();
        assert_eq!(rows.messages.len(), 1, "{chat}");
    }
    let page = body_text(
        owner_browser
            .send(t.router(None), Method::GET, &format!("/chats/{theirs}"), "")
            .await,
    )
    .await;
    assert!(!page.contains("Sent: yes"), "{page}");

    // Mia's lists have neither chat.
    for path in ["/", "/chats/archived"] {
        let html = body_text(mia.send(t.router(None), Method::GET, path, "").await).await;
        assert!(!html.contains("secret"), "{path}: {html}");
        assert!(!html.contains(&theirs.to_string()), "{path}: {html}");
        assert!(!html.contains(&archived.to_string()), "{path}: {html}");
    }
}

#[tokio::test]
async fn a_chat_one_user_has_begun_is_not_found_for_another_before_the_core_takes_it() {
    let t = Chats::new().await;
    let env = &t.env;
    stored_users(env).await;
    let owner_browser = Browser::log_in(env, "owner").await;
    let mia = Browser::log_in(env, "mia").await;
    let fresh = chat_n(0x33);

    // The owner's first message is waiting for the core, so the chat has no session yet.
    let response = owner_browser
        .send(
            t.router(None),
            Method::POST,
            &format!("/chats/{fresh}/messages"),
            "text=my+first+message",
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        env.store
            .conversation_access(INSTANCE, &fresh, &identity::owner())
            .await
            .unwrap(),
        ConversationAccess::Unstarted
    );

    let requests = [
        (Method::GET, format!("/chats/{fresh}"), ""),
        (Method::GET, format!("/chats/{fresh}/events"), ""),
        (Method::POST, format!("/chats/{fresh}/messages"), "text=hi"),
        (
            Method::POST,
            format!("/chats/{fresh}/approvals"),
            "code=1&answer=no",
        ),
        (Method::POST, format!("/chats/{fresh}/delete"), ""),
    ];
    assert_all_not_found(&t, &mia, "my first message", &requests).await;
    assert_eq!(env.pending_texts().await, ["my first message"]);

    // The owner goes on using it.
    let response = owner_browser
        .send(t.router(None), Method::GET, &format!("/chats/{fresh}"), "")
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(body_text(response).await.contains("my first message"));
}

#[tokio::test]
async fn each_user_has_their_own_chats_and_the_same_id_never_joins_two() {
    let t = Chats::new().await;
    let env = &t.env;
    stored_users(env).await;
    let owner_browser = Browser::log_in(env, "owner").await;
    let mia = Browser::log_in(env, "mia").await;
    let hers = chat_n(0x44);
    let his = chat_n(0x55);
    chat_of(env, &mia_user(), &hers, "mia family recipe").await;
    chat_of(env, &identity::owner(), &his, "the owner's tax return").await;

    // Each sees only their own chat in the list and can open it, post, archive, and unarchive.
    let list = body_text(mia.send(t.router(None), Method::GET, "/", "").await).await;
    assert!(list.contains("mia family recipe"), "{list}");
    assert!(!list.contains("tax return"), "{list}");
    let list = body_text(
        owner_browser
            .send(t.router(None), Method::GET, "/", "")
            .await,
    )
    .await;
    assert!(list.contains("tax return"), "{list}");
    assert!(!list.contains("recipe"), "{list}");

    let response = mia
        .send(
            t.router(None),
            Method::POST,
            &format!("/chats/{hers}/messages"),
            "text=more+salt",
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let events = env.pending().await;
    let ChannelEvent::Message { message, .. } = &events[1] else {
        panic!("{events:?}");
    };
    assert_eq!(message.sender.as_str(), "mia");
    assert_eq!(message.conversation.id, hers);

    for action in ["archive", "unarchive"] {
        let response = mia
            .send(
                t.router(None),
                Method::POST,
                &format!("/chats/{hers}/{action}"),
                "",
            )
            .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "{action}");
    }

    // An inbound row the owner has waiting in their chat isn't in anyone else's transcript.
    let (open, _) = env
        .store
        .open_session(
            SessionKey {
                agent: AgentId::new("jeeves").unwrap(),
                channel: INSTANCE.into(),
                conversation: his.clone(),
            },
            SessionKind::Direct,
            Some(identity::owner()),
            Timestamp::now(),
        )
        .await
        .unwrap();
    env.store
        .inbox_insert(NewInboxEvent {
            instance: INSTANCE.into(),
            event_id: EventId::new("waiting-1").unwrap(),
            session: Some(open.id),
            user: Some(identity::owner()),
            payload: serde_json::json!({"type": "message", "text": "waiting for a turn"}),
            state: InboxState::Queued,
            received_at: Timestamp::now(),
        })
        .await
        .unwrap();
    let transcript = env
        .store
        .conversation_transcript(INSTANCE, &his, &mia_user(), 10)
        .await
        .unwrap();
    assert!(transcript.messages.is_empty());
    assert!(transcript.waiting.is_empty());
}

#[tokio::test]
async fn only_the_chats_user_can_answer_its_approval_prompt() {
    let t = Chats::new().await;
    let env = &t.env;
    stored_users(env).await;
    let owner_browser = Browser::log_in(env, "owner").await;
    let root = Browser::log_in(env, "root").await;
    let mia = Browser::log_in(env, "mia").await;
    let chat = chat_n(0x66);
    chat_of(env, &identity::owner(), &chat, "deploy it").await;
    env.channel
        .send_approval(&chat, "9d2", "Run `deploy`?")
        .unwrap();

    // No other user answers it, an admin included (an admin's role is no part of the rule).
    let path = format!("/chats/{chat}/approvals");
    for who in [&mia, &root] {
        for answer in ["yes", "yes!", "no"] {
            let response = who
                .send(
                    t.router(None),
                    Method::POST,
                    &path,
                    &format!("code=9d2&answer={answer}"),
                )
                .await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{answer}");
        }
    }
    assert!(
        env.pending_texts().await.is_empty(),
        "nothing was sent to the core"
    );
    let page = body_text(
        owner_browser
            .send(t.router(None), Method::GET, &format!("/chats/{chat}"), "")
            .await,
    )
    .await;
    assert!(
        page.contains(r#"value="yes""#),
        "the prompt is still open: {page}"
    );

    // The chat's user answers it, as the core will read: `yes 9d2` from their own identity.
    let response = owner_browser
        .send(t.router(None), Method::POST, &path, "code=9d2&answer=yes")
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let events = env.pending().await;
    let [_, ChannelEvent::Message { message, .. }] = &events[..] else {
        panic!("{events:?}");
    };
    assert_eq!(message.text, "yes 9d2");
    assert_eq!(message.sender.as_str(), "owner");
}

#[tokio::test]
async fn a_deleted_chat_leaves_nothing_for_the_next_user_of_its_id_to_replay() {
    let t = Chats::new().await;
    let env = &t.env;
    stored_users(env).await;
    let owner_browser = Browser::log_in(env, "owner").await;
    let mia = Browser::log_in(env, "mia").await;
    let chat = chat_n(0x88);
    chat_of(env, &identity::owner(), &chat, "the deleted secret").await;
    env.channel
        .send_approval(&chat, "5150", "Run `the deleted secret`?")
        .unwrap();

    let response = owner_browser
        .send(
            t.router(None),
            Method::POST,
            &format!("/chats/{chat}/delete"),
            "",
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);

    // The id is free again, but the events of the chat that had it are gone with its hub.
    let path = format!("/chats/{chat}/events?after={}", env.channel.hubs.cursor(0));
    let response = mia.send(t.router(None), Method::GET, &path, "").await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    // Told to reload, as for any page from before a hub was dropped, and nothing more.
    let chunk = tokio::time::timeout(Duration::from_millis(200), body.next())
        .await
        .expect("the stream starts with the reload notice")
        .unwrap()
        .unwrap();
    let chunk = String::from_utf8(chunk.to_vec()).unwrap();
    assert!(chunk.contains("Reload the page"), "{chunk}");
    assert!(
        !chunk.contains("deleted secret") && !chunk.contains("5150"),
        "{chunk}"
    );
    let more = tokio::time::timeout(Duration::from_millis(200), body.next()).await;
    assert!(more.is_err(), "nothing is replayed: {more:?}");
    let page = body_text(
        mia.send(t.router(None), Method::GET, &format!("/chats/{chat}"), "")
            .await,
    )
    .await;
    assert!(!page.contains("deleted secret"), "{page}");
}

/// Puts an image blob of `bytes`.
async fn put_image(env: &Env, bytes: &'static [u8]) -> MediaRef {
    env.blobs
        .put(
            bytes,
            1024,
            BlobMeta {
                media_type: "image/png".into(),
                name: Some("pic.png".into()),
            },
            Timestamp::now(),
        )
        .await
        .unwrap()
}

/// Stores a chat of `user` in `chat` that shows `media`.
async fn chat_with_image(env: &Env, user: &UserId, chat: &ConversationId, media: &MediaRef) {
    let (session, _) = env
        .store
        .open_session(
            SessionKey {
                agent: AgentId::new("jeeves").unwrap(),
                channel: INSTANCE.into(),
                conversation: chat.clone(),
            },
            SessionKind::Direct,
            Some(user.clone()),
            Timestamp::now(),
        )
        .await
        .unwrap();
    env.store
        .append_messages(
            session.id,
            vec![stored(
                Role::User,
                vec![Block::Image {
                    media: media.clone(),
                }],
            )],
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn a_blob_is_served_only_to_users_who_own_a_message_that_refers_to_it() {
    let env = env().await;
    stored_users(&env).await;
    let owner_browser = Browser::log_in(&env, "owner").await;
    let root = Browser::log_in(&env, "root").await;
    let mia = Browser::log_in(&env, "mia").await;
    let private = put_image(&env, b"the owner's private photo").await;
    let shared = put_image(&env, b"a photo both sent").await;
    let mias = put_image(&env, b"mia's photo").await;
    let nobodys = put_image(&env, b"nobody's photo").await;
    chat_with_image(&env, &identity::owner(), &chat_n(0x77), &private).await;
    // Both sent the same bytes under their own names; the blob row keeps the first sender's.
    let named = |name: &str| MediaRef {
        name: Some(name.to_owned()),
        media_type: "application/octet-stream".to_owned(),
        ..shared.clone()
    };
    chat_with_image(
        &env,
        &identity::owner(),
        &chat_n(0x78),
        &named("owner-name.bin"),
    )
    .await;
    chat_with_image(&env, &mia_user(), &chat_n(0x79), &named("mia-name.bin")).await;
    chat_with_image(&env, &mia_user(), &chat_n(0x7a), &mias).await;

    let get = async |who: &Browser, media: &MediaRef| {
        who.send(
            env.main_app(),
            Method::GET,
            &format!("/blobs/{}", media.id),
            "",
        )
        .await
        .status()
    };
    let (ok, missing) = (StatusCode::OK, StatusCode::NOT_FOUND);
    // The same bytes are each sender's own; the rest belong to whoever's messages show them.
    assert_eq!(get(&owner_browser, &private).await, ok);
    assert_eq!(get(&mia, &private).await, missing);
    assert_eq!(get(&root, &private).await, missing, "an admin is no owner");
    assert_eq!(get(&owner_browser, &shared).await, ok);
    assert_eq!(get(&mia, &shared).await, ok);
    assert_eq!(get(&root, &shared).await, missing);
    // Each is shown the name of their own copy.
    for (who, name) in [(&owner_browser, "owner-name.bin"), (&mia, "mia-name.bin")] {
        let response = who
            .send(
                env.main_app(),
                Method::GET,
                &format!("/blobs/{}", shared.id),
                "",
            )
            .await;
        assert_eq!(
            response.headers()[CONTENT_DISPOSITION],
            format!("attachment; filename*=UTF-8''{name}")
        );
        assert_eq!(response.headers()[CONTENT_TYPE], "application/octet-stream");
    }
    assert_eq!(get(&mia, &mias).await, ok);
    assert_eq!(get(&owner_browser, &mias).await, missing);
    for who in [&owner_browser, &root, &mia] {
        assert_eq!(get(who, &nobodys).await, missing);
    }
}

#[tokio::test]
async fn another_users_jobs_are_not_found_on_every_job_route() {
    let env = env().await;
    stored_users(&env).await;
    let owner_browser = Browser::log_in(&env, "owner").await;
    let mia = Browser::log_in(&env, "mia").await;
    let theirs = create_job(
        &env,
        "bunter",
        "owner",
        "0 9 * * *",
        "Owner's tax reminder.",
    )
    .await;
    let spec_of = async || env.store.job(theirs).await.unwrap().map(|job| job.spec);
    let before = spec_of().await.unwrap();

    // Mia names the owner's job on every route that takes a job id.
    let change = "agent=bunter&repeat=cron&cron=0+1+*+*+*&prompt=Mia+now.&session=dm";
    for (method, action, fields) in [
        (Method::GET, "edit", ""),
        (Method::POST, "edit", change),
        (Method::POST, "disable", ""),
        (Method::POST, "enable", ""),
        (Method::POST, "delete", ""),
    ] {
        let path = format!("/jobs/{theirs}/{action}");
        let response = mia
            .send(env.main_app(), method.clone(), &path, fields)
            .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method} {path}");
        assert!(!body_text(response).await.contains("tax reminder"));
    }
    assert_eq!(spec_of().await, Some(before));

    // Her page has neither the owner's job nor the owner's config jobs.
    let page = body_text(mia.send(env.main_app(), Method::GET, "/jobs", "").await).await;
    assert!(!page.contains("tax reminder"), "{page}");
    assert!(!page.contains("morning"), "{page}");

    // She schedules her own, for any agent, and the owner can't touch that one either.
    let response = mia
        .send(
            env.main_app(),
            Method::POST,
            "/jobs",
            "agent=jeeves&repeat=cron&cron=0+9+*+*+*&prompt=Water+the+plants.&session=dm",
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let hers = env
        .store
        .jobs(None, Some(mia_user()))
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("her job")
        .id;
    let page = body_text(mia.send(env.main_app(), Method::GET, "/jobs", "").await).await;
    assert!(page.contains("Water the plants."), "{page}");
    let page = body_text(
        owner_browser
            .send(env.main_app(), Method::GET, "/jobs", "")
            .await,
    )
    .await;
    assert!(!page.contains("Water the plants."), "{page}");
    for action in ["disable", "delete"] {
        let response = owner_browser
            .send(
                env.main_app(),
                Method::POST,
                &format!("/jobs/{hers}/{action}"),
                "",
            )
            .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{action}");
    }
    assert!(env.store.job(hers).await.unwrap().is_some());
}

#[tokio::test]
async fn another_users_ssh_key_is_not_found_on_every_ssh_key_route() {
    let ssh = Ssh::new().await;
    let env = &ssh.env;
    stored_users(env).await;
    let owner_browser = Browser::log_in(env, "owner").await;
    let mia = Browser::log_in(env, "mia").await;
    let key = ssh
        .keys
        .generate(&identity::owner(), KeyType::Ed25519, Some("owner-laptop"))
        .await
        .unwrap();

    for (method, path, fields) in [
        (Method::GET, format!("/ssh-keys/{}/rename", key.id), ""),
        (
            Method::POST,
            format!("/ssh-keys/{}/rename", key.id),
            "label=mine",
        ),
        (Method::GET, format!("/ssh-keys/{}/delete", key.id), ""),
        (Method::POST, format!("/ssh-keys/{}/delete", key.id), ""),
    ] {
        let response = mia
            .send(ssh.router(None), method.clone(), &path, fields)
            .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method} {path}");
        assert!(!body_text(response).await.contains("owner-laptop"));
    }
    // The admin page and its delete are for admins, whoever's the key.
    for (method, path) in [
        (Method::GET, "/admin/ssh-keys".to_owned()),
        (Method::GET, format!("/admin/ssh-keys/{}/delete", key.id)),
        (Method::POST, format!("/admin/ssh-keys/{}/delete", key.id)),
    ] {
        let response = mia.send(ssh.router(None), method.clone(), &path, "").await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method} {path}");
    }
    let page = body_text(
        mia.send(ssh.router(None), Method::GET, "/ssh-keys", "")
            .await,
    )
    .await;
    assert!(!page.contains("owner-laptop"), "{page}");
    assert_eq!(ssh.keys.list(&identity::owner()).await.unwrap(), [key]);
    assert!(ssh.keys.list(&mia_user()).await.unwrap().is_empty());
    let page = body_text(
        owner_browser
            .send(ssh.router(None), Method::GET, "/ssh-keys", "")
            .await,
    )
    .await;
    assert!(page.contains("owner-laptop"), "{page}");
}

#[tokio::test]
async fn another_users_memory_is_not_found_on_the_memory_page() {
    let env = env().await;
    stored_users(&env).await;
    let manager = env.manager().await;
    let mia = Browser::log_in(&env, "mia").await;
    let private = env
        .seed(
            &owner_viewer(),
            "The owner's private memory.",
            false,
            false,
            "2026-03-01T00:00:00Z",
        )
        .await;
    let shared = env
        .seed(
            &owner_viewer(),
            "House rule.",
            false,
            true,
            "2026-03-01T00:00:00Z",
        )
        .await;
    let updated = private.updated_at.as_millisecond();

    let page = body_text(
        mia.send(env.agents_router(&manager, None), Method::GET, MEMORY, "")
            .await,
    )
    .await;
    assert!(!page.contains("private memory"), "{page}");
    assert!(page.contains("House rule."), "{page}");

    for (action, fields) in [
        (
            "edit",
            format!("id={}&content=mine&updated={updated}", private.id),
        ),
        ("forget", format!("id={}", private.id)),
    ] {
        let response = mia
            .send(
                env.agents_router(&manager, None),
                Method::POST,
                &format!("{MEMORY}/{action}"),
                &fields,
            )
            .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{action}");
        assert!(!body_text(response).await.contains("private memory"));
    }
    // A member can't write the shared ones either.
    let updated = shared.updated_at.as_millisecond();
    for (action, fields) in [
        (
            "edit",
            format!("id={}&content=mine&updated={updated}", shared.id),
        ),
        ("forget", format!("id={}", shared.id)),
    ] {
        let response = mia
            .send(
                env.agents_router(&manager, None),
                Method::POST,
                &format!("{MEMORY}/{action}"),
                &fields,
            )
            .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{action}");
    }
    let store = env.memory_store().await;
    for memory in [&private, &shared] {
        let now = store.get(&owner_viewer(), memory.id).await.unwrap();
        assert_eq!(now.content, memory.content);
    }
}

#[tokio::test]
async fn signing_out_of_a_connection_leaves_another_users_tokens() {
    let env = env().await;
    stored_users(&env).await;
    let t = env.toolsets("").await;
    t.manager
        .add(
            "notes",
            &toolset_spec(
                r#"{ "url": "http://127.0.0.1:9/mcp", "auth": { "oauth": { "client_id": "doozers" } } }"#,
            ),
        )
        .await
        .unwrap();
    let owner_browser = Browser::log_in(&env, "owner").await;
    let mia = Browser::log_in(&env, "mia").await;
    let token = crate::secrets::SecretPath::new("users/owner/toolsets/notes/oauth-token").unwrap();
    t.secrets
        .put(&token, &crate::config::Secret::new("{}"), None)
        .await
        .unwrap();

    // Only the owner's page says they're signed in.
    let signed_in = r#"<span class="pill pill-ok m-aside">Signed in</span>"#;
    let page = body_text(
        owner_browser
            .send(t.router(None), Method::GET, "/connections", "")
            .await,
    )
    .await;
    assert!(page.contains(signed_in), "{page}");
    let page = body_text(
        mia.send(t.router(None), Method::GET, "/connections", "")
            .await,
    )
    .await;
    assert!(!page.contains(signed_in), "{page}");

    // Mia signing out changes nothing of the owner's. That another user can't finish the owner's
    // sign-in, by its state, is tested against a real authorization server in
    // `tests/toolset_oauth.rs`.
    let response = mia
        .send(
            t.router(None),
            Method::POST,
            "/connections/notes/sign-out",
            "",
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert!(t.secrets.get(&token).await.unwrap().is_some());
}

#[tokio::test]
async fn a_member_starts_a_chat_with_any_agent_and_posts_as_themselves() {
    let env = env().await;
    stored_users(&env).await;
    let mia = Browser::log_in(&env, "mia").await;

    let list = body_text(mia.send(env.main_app(), Method::GET, "/", "").await).await;
    for agent in ["jeeves", "bunter"] {
        assert!(
            list.contains(&format!(r#"<option value="{agent}">"#)),
            "{list}"
        );
    }
    let response = mia
        .send(env.main_app(), Method::GET, "/chats/new?agent=bunter", "")
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let chat = response.headers()[LOCATION].to_str().unwrap().to_owned();
    let id = chat.strip_prefix("/chats/").unwrap();
    assert!(id.starts_with("bunter."), "{chat}");

    let response = mia
        .send(
            env.main_app(),
            Method::POST,
            &format!("{chat}/messages"),
            "text=hello+bunter",
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let events = env.pending().await;
    let [_, ChannelEvent::Message { message, .. }] = &events[..] else {
        panic!("{events:?}");
    };
    assert_eq!(message.sender.as_str(), "mia");
    assert_eq!(message.conversation.id.as_str(), id);
    assert_eq!(message.text, "hello bunter");
}

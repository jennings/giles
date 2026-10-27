//! Router-level tests of the agent memory page over the test configuration's `jeeves` (whose
//! memory cap is the default, 2,200 characters). Memories are seeded straight into the agent's
//! memory database, through the same [`MemoryDbs`](crate::memory_dbs::MemoryDbs) the page reads.

use giles_store::{
    Error as StoreError, Memory, MemoryId, MemoryOwner, MemoryStore, MemoryViewer, NewMemory,
};

use super::super::memory::{PAGE_SIZE, ScopeError, group_viewer, user_viewer};
use super::agents::guest;
use super::*;
use crate::agents::AgentManager;

const XSS: &str = "<script>alert(1)</script>";
const XSS_ESCAPED: &str = "&#60;script&#62;alert(1)&#60;/script&#62;";
const MEMORY: &str = "/admin/agents/jeeves/memory";
const CAP: usize = 2200;

fn jeeves() -> AgentId {
    AgentId::new("jeeves").unwrap()
}

fn ts(text: &str) -> Timestamp {
    text.parse().unwrap()
}

fn owner_viewer() -> MemoryViewer {
    MemoryViewer::new(MemoryOwner::User(identity::owner()), true)
}

fn guest_viewer() -> MemoryViewer {
    MemoryViewer::new(MemoryOwner::User(guest().user), false)
}

fn group_owner(instance: &str, conversation: &str) -> MemoryOwner {
    MemoryOwner::Group {
        instance: instance.into(),
        conversation: ConversationId::new(conversation).unwrap(),
    }
}

/// The viewer of a group, as the page builds it: with the owner's right to write shared memories.
fn group_of(instance: &str, conversation: &str) -> MemoryViewer {
    MemoryViewer::new(group_owner(instance, conversation), true)
}

/// A memory in the database as stored: id, content, owner kind, shared, core.
type Row = (i64, String, String, bool, bool);

/// `fields` as a URL-encoded form body.
fn encoded(fields: &[(&str, &str)]) -> String {
    fn escape(text: &str) -> String {
        text.bytes()
            .map(|b| match b {
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                b => format!("%{b:02X}"),
            })
            .collect()
    }
    fields
        .iter()
        .map(|(name, value)| format!("{}={}", escape(name), escape(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// A form body with the CSRF token and `fields`.
fn post(fields: &[(&str, &str)]) -> String {
    let mut all = vec![("csrf", CSRF)];
    all.extend_from_slice(fields);
    encoded(&all)
}

/// The fields of the add form: ticked boxes are sent, unticked ones left out.
fn add_fields(content: &str, core: bool, shared: bool) -> Vec<(&str, &str)> {
    let mut fields = vec![("content", content)];
    if core {
        fields.push(("core", "on"));
    }
    if shared {
        fields.push(("shared", "on"));
    }
    fields
}

/// The edit form of `memory` as its page rendered it, with the new `content` and flags.
fn edit_form(memory: &Memory, content: &str, core: bool, shared: bool) -> String {
    let id = memory.id.to_string();
    let updated = memory.updated_at.as_millisecond().to_string();
    let mut fields = vec![
        ("id", id.as_str()),
        ("updated", updated.as_str()),
        ("content", content),
    ];
    if core {
        fields.push(("core", "on"));
    }
    if shared {
        fields.push(("shared", "on"));
    }
    post(&fields)
}

/// The id of each memory on the page, in the order the page lists them.
fn listed(html: &str) -> Vec<i64> {
    html.match_indices("id=\"memory-")
        .map(|(at, marker)| {
            let digits: String = html[at + marker.len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            digits.parse().unwrap()
        })
        .collect()
}

/// The part of the page that is memory `id`'s item.
fn item(html: &str, id: MemoryId) -> &str {
    let start = html
        .find(&format!("id=\"memory-{id}\""))
        .unwrap_or_else(|| panic!("memory {id} is not on the page\n{html}"));
    let rest = &html[start + 1..];
    let end = rest.find("id=\"memory-").unwrap_or(rest.len());
    &rest[..end]
}

impl Env {
    async fn memory_store(&self) -> MemoryStore {
        self.memory_dbs.get(&jeeves()).await.unwrap()
    }

    /// Adds a memory to the database as `viewer` at `at`.
    async fn seed(
        &self,
        viewer: &MemoryViewer,
        content: &str,
        core: bool,
        shared: bool,
        at: &str,
    ) -> Memory {
        self.memory_store()
            .await
            .add(
                viewer,
                NewMemory {
                    content: content.into(),
                    core,
                    shared,
                    source_session: None,
                },
                CAP,
                ts(at),
            )
            .await
            .unwrap()
    }

    /// Every memory in the database, whoever it belongs to, by id.
    fn rows(&self) -> Vec<Row> {
        let path = self.root().agent_memory_db("jeeves").unwrap();
        if !path.exists() {
            return Vec::new();
        }
        let conn = rusqlite::Connection::open(path).unwrap();
        let mut stmt = conn
            .prepare("SELECT id, content, owner_kind, shared, core FROM memories ORDER BY id")
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    }

    /// `method path` as `user`: the status, body, and `Location`. Every answer is `no-store`.
    async fn memory_call(
        &self,
        manager: &Arc<AgentManager>,
        user: Option<WebUser>,
        method: Method,
        path: &str,
        form: Option<&str>,
    ) -> (StatusCode, String, Option<String>) {
        let response = call(self.agents_router(manager, user), method, path, &[], form).await;
        assert_eq!(response.headers()[CACHE_CONTROL], "no-store", "{path}");
        let status = response.status();
        let location = response
            .headers()
            .get(LOCATION)
            .map(|l| l.to_str().unwrap().to_owned());
        (status, body_text(response).await, location)
    }

    /// `GET path` as the owner.
    async fn memory_get(&self, manager: &Arc<AgentManager>, path: &str) -> String {
        let (status, html, _) = self
            .memory_call(manager, Some(owner()), Method::GET, path, None)
            .await;
        assert_eq!(status, StatusCode::OK, "{html}");
        html
    }

    /// `POST path` of `form` as `user`.
    async fn memory_post(
        &self,
        manager: &Arc<AgentManager>,
        user: WebUser,
        path: &str,
        form: &str,
    ) -> (StatusCode, String, Option<String>) {
        self.memory_call(manager, Some(user), Method::POST, path, Some(form))
            .await
    }
}

#[tokio::test]
async fn the_page_needs_a_login_and_a_known_agent() {
    let env = env().await;
    let manager = env.manager().await;
    for method in [Method::GET, Method::POST] {
        let (status, html, _) = env
            .memory_call(&manager, None, method, MEMORY, Some(&post(&[])))
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(!html.contains("Jeeves"));
    }
    for path in [
        "/admin/agents/nobody/memory",
        "/admin/agents/no%20such/memory",
    ] {
        let (status, html, _) = env
            .memory_call(&manager, Some(owner()), Method::GET, path, None)
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert!(html.contains("There is no agent with that name."), "{html}");
        for sub in ["", "/edit", "/forget"] {
            let (status, _, _) = env
                .memory_post(
                    &manager,
                    owner(),
                    &format!("{path}{sub}"),
                    &post(&add_fields("Tea.", false, false)),
                )
                .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}{sub}");
        }
    }
    // An unknown agent gets no directory (and so no database) from the page.
    assert_eq!(env.agent_dirs(), ["bunter", "jeeves"]);
    assert!(!env.root().agents_dir().join("nobody").exists());
}

#[tokio::test]
async fn the_agent_page_links_to_the_memory_page() {
    let env = env().await;
    let manager = env.manager().await;
    let (status, html, _) = env
        .agents_call(&manager, Method::GET, "/admin/agents/jeeves", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains(r#"<a class="button" href="/admin/agents/jeeves/memory">Memory</a>"#),
        "{html}"
    );
}

#[tokio::test]
async fn an_agent_without_memories_gets_an_empty_page_with_the_sessions_notice() {
    let env = env().await;
    let manager = env.manager().await;
    let html = env.memory_get(&manager, MEMORY).await;
    for expected in [
        "<h1>Memory of Jeeves (jeeves)</h1>",
        "There are no memories here yet.",
        "Running sessions keep the core memories they started with.",
        "reaches a session after it compacts, or when a new session starts.",
        "Shared core memories: <code>[0% — 0/2,200]</code>",
        "Your core memories: <code>[0% — 0/2,200]</code>",
        r#"<form class="inline-form" method="get" action="/admin/agents/jeeves/memory" role="search">"#,
        r#"<input type="hidden" name="csrf" value="csrf-token-for-tests">"#,
        r#"<a href="/admin/routes">Routes</a>"#,
    ] {
        assert!(html.contains(expected), "{expected}\n{html}");
    }
    assert!(!html.contains("Group "), "{html}");
}

#[tokio::test]
async fn the_owner_sees_their_own_and_the_shared_memories_but_not_other_users_private_ones() {
    let env = env().await;
    let manager = env.manager().await;
    let mine = env
        .seed(
            &owner_viewer(),
            "Likes tea.",
            true,
            false,
            "2026-03-04T05:06:07Z",
        )
        .await;
    let shared = env
        .seed(
            &owner_viewer(),
            "No cats.",
            true,
            true,
            "2026-03-05T00:00:00Z",
        )
        .await;
    let theirs = env
        .seed(
            &guest_viewer(),
            "Guest secret.",
            true,
            false,
            "2026-03-06T00:00:00Z",
        )
        .await;
    let quiet = env
        .seed(
            &owner_viewer(),
            "Quiet hours.",
            false,
            true,
            "2026-03-01T00:00:00Z",
        )
        .await;

    let html = env.memory_get(&manager, MEMORY).await;
    assert!(!html.contains("Guest secret."), "{html}");
    assert!(
        html.contains("Shared core memories: <code>[0% — 8/2,200]</code>"),
        "{html}"
    );
    assert!(
        html.contains("Your core memories: <code>[0% — 10/2,200]</code>"),
        "{html}"
    );
    // Newest change first: the shared "No cats." (March 5), then "Likes tea." (March 4)...
    assert_eq!(
        listed(&html),
        [shared.id.get(), mine.id.get(), quiet.id.get()]
    );
    assert!(!listed(&html).contains(&theirs.id.get()));

    let mine_item = item(&html, mine.id);
    for expected in [
        "<h3>#1 <span class=\"detail\">core</span></h3>",
        "<pre>Likes tea.</pre>",
        "10 characters.",
        r#"<time datetime="2026-03-04T05:06:07Z">2026-03-04 05:06 UTC</time>"#,
        format!(
            r#"<input type="hidden" name="updated" value="{}">"#,
            mine.updated_at.as_millisecond()
        )
        .as_str(),
        r#"action="/admin/agents/jeeves/memory/edit""#,
        r#"action="/admin/agents/jeeves/memory/forget""#,
        r#"<input type="checkbox" name="core" checked>"#,
        r#"<input type="checkbox" name="shared">"#,
    ] {
        assert!(mine_item.contains(expected), "{expected}\n{mine_item}");
    }
    let shared_item = item(&html, shared.id);
    assert!(shared_item.contains("core, shared"), "{shared_item}");
    assert!(shared_item.contains(r#"<input type="checkbox" name="shared" checked>"#));
    assert!(item(&html, quiet.id).contains("<span class=\"detail\">shared</span>"));
    // The add form offers *shared* to the owner.
    assert!(
        html.contains(r#"<input type="checkbox" name="shared"> Shared: every user and group"#),
        "{html}"
    );
}

#[tokio::test]
async fn a_memory_is_shown_escaped_everywhere() {
    let env = env().await;
    let manager = env.manager().await;
    let mine = env
        .seed(
            &owner_viewer(),
            &format!("Mine {XSS}"),
            false,
            false,
            "2026-03-04T05:06:07Z",
        )
        .await;
    let group = env
        .seed(
            &group_of("libera", "#giles"),
            &format!("Group {XSS}"),
            false,
            false,
            "2026-03-04T05:06:07Z",
        )
        .await;
    let html = env.memory_get(&manager, MEMORY).await;
    assert!(!html.contains(XSS), "{html}");
    assert!(
        html.contains(&format!("<pre>Mine {XSS_ESCAPED}</pre>")),
        "{html}"
    );
    assert!(
        html.contains(&format!("<pre>Group {XSS_ESCAPED}</pre>")),
        "{html}"
    );
    assert!(listed(&html).contains(&group.id.get()) && listed(&html).contains(&mine.id.get()));

    // Added text, the search box, and what is shown again after a refusal are escaped too.
    let (status, html, _) = env
        .memory_post(
            &manager,
            owner(),
            MEMORY,
            &post(&add_fields(
                &format!("{XSS}{}", "x".repeat(1000)),
                false,
                false,
            )),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!html.contains(XSS), "{html}");
    assert!(html.contains(&format!("{XSS_ESCAPED}xxx")), "{html}");
    let html = env
        .memory_get(&manager, &format!("{MEMORY}?{}", encoded(&[("q", XSS)])))
        .await;
    assert!(!html.contains(XSS), "{html}");
    assert!(
        html.contains(&format!(r#"value="{XSS_ESCAPED}""#)),
        "{html}"
    );
}

#[tokio::test]
async fn the_owner_adds_edits_and_forgets_a_memory() {
    let env = env().await;
    let manager = env.manager().await;

    // Add: browsers send line breaks as CRLF; the memory keeps LF. Boxes that are ticked are sent.
    let (status, _, location) = env
        .memory_post(
            &manager,
            owner(),
            MEMORY,
            &post(&add_fields("Likes tea\r\nand toast", true, true)),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        location.as_deref(),
        Some("/admin/agents/jeeves/memory?done=added#memory-1")
    );
    assert_eq!(
        env.rows(),
        [(
            1,
            "Likes tea\nand toast".to_owned(),
            "user".to_owned(),
            true,
            true
        )]
    );
    let stored = env
        .memory_store()
        .await
        .get(&owner_viewer(), MemoryId::new(1))
        .await
        .unwrap();
    assert_eq!(stored.owner, MemoryOwner::User(identity::owner()));
    assert_eq!(stored.source_session, None);

    // The redirect target says so; a made-up `done` says nothing.
    let html = env
        .memory_get(&manager, &format!("{MEMORY}?done=added"))
        .await;
    assert!(html.contains("Memory added."), "{html}");
    let html = env
        .memory_get(
            &manager,
            &format!("{MEMORY}?done=%3Cscript%3Ealert(1)%3C%2Fscript%3E"),
        )
        .await;
    assert!(!html.contains("role=\"status\""), "{html}");
    assert!(!html.contains(XSS), "{html}");

    // Edit content and flags: un-ticking *shared* makes it the owner's private memory.
    let (status, _, location) = env
        .memory_post(
            &manager,
            owner(),
            &format!("{MEMORY}/edit"),
            &edit_form(&stored, "Likes coffee", false, false),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        location.as_deref(),
        Some("/admin/agents/jeeves/memory?done=saved#memory-1")
    );
    assert_eq!(
        env.rows(),
        [(
            1,
            "Likes coffee".to_owned(),
            "user".to_owned(),
            false,
            false
        )]
    );
    let html = env
        .memory_get(&manager, &format!("{MEMORY}?done=saved"))
        .await;
    assert!(html.contains("Memory saved."), "{html}");
    assert!(html.contains("<pre>Likes coffee</pre>"), "{html}");

    // Forget.
    let (status, _, location) = env
        .memory_post(
            &manager,
            owner(),
            &format!("{MEMORY}/forget"),
            &post(&[("id", "1")]),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        location.as_deref(),
        Some("/admin/agents/jeeves/memory?done=forgotten")
    );
    assert!(env.rows().is_empty());
    let html = env
        .memory_get(&manager, &format!("{MEMORY}?done=forgotten"))
        .await;
    assert!(html.contains("Memory forgotten."), "{html}");
    assert!(html.contains("There are no memories here yet."), "{html}");
}

#[tokio::test]
async fn a_post_without_the_csrf_token_changes_nothing() {
    let env = env().await;
    let manager = env.manager().await;
    let kept = env
        .seed(
            &owner_viewer(),
            "Kept.",
            false,
            false,
            "2026-03-04T05:06:07Z",
        )
        .await;
    let id = kept.id.to_string();
    let updated = kept.updated_at.as_millisecond().to_string();
    for (path, fields) in [
        (MEMORY, vec![("content", "Added."), ("core", "on")]),
        (
            "/admin/agents/jeeves/memory/edit",
            vec![
                ("id", id.as_str()),
                ("updated", updated.as_str()),
                ("content", "Edited."),
            ],
        ),
        (
            "/admin/agents/jeeves/memory/forget",
            vec![("id", id.as_str())],
        ),
    ] {
        for csrf in [None, Some("wrong"), Some("")] {
            let mut all = fields.clone();
            if let Some(csrf) = csrf {
                all.push(("csrf", csrf));
            }
            let (status, html, _) = env
                .memory_post(&manager, owner(), path, &encoded(&all))
                .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path} {csrf:?}");
            assert!(!html.contains("Kept."), "{html}");
        }
        // A wrong token in the htmx header is refused too.
        let response = call(
            env.agents_router(&manager, Some(owner())),
            Method::POST,
            path,
            &[("x-csrf-token", "wrong")],
            Some(&encoded(&fields)),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
    }
    assert_eq!(
        env.rows(),
        [(1, "Kept.".to_owned(), "user".to_owned(), false, false)]
    );

    // The token in the header (what htmx sends) is as good as the form field.
    let response = call(
        env.agents_router(&manager, Some(owner())),
        Method::POST,
        MEMORY,
        &[("x-csrf-token", CSRF)],
        Some(&encoded(&[("content", "Added.")])),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(env.rows().len(), 2);
}

#[tokio::test]
async fn content_too_long_or_empty_is_refused_and_shown_again() {
    let env = env().await;
    let manager = env.manager().await;

    let long = "x".repeat(1001);
    let (status, html, _) = env
        .memory_post(
            &manager,
            owner(),
            MEMORY,
            &post(&add_fields(&long, true, false)),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        html.contains("the memory is 1001 characters; the limit is 1000"),
        "{html}"
    );
    // The add form keeps what was typed.
    assert!(html.contains(&format!("{long}</textarea>")), "{html}");
    assert!(
        html.contains(r#"<input type="checkbox" name="core" checked>"#),
        "{html}"
    );
    assert!(env.rows().is_empty());

    for blank in ["", "  \r\n "] {
        let (status, html, _) = env
            .memory_post(
                &manager,
                owner(),
                MEMORY,
                &post(&add_fields(blank, false, false)),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{blank:?}");
        assert!(
            html.contains("Not saved: memory content is empty."),
            "{html}"
        );
    }
    assert!(env.rows().is_empty());

    // Exactly the limit fits.
    let exact = "y".repeat(1000);
    let (status, _, _) = env
        .memory_post(
            &manager,
            owner(),
            MEMORY,
            &post(&add_fields(&exact, false, false)),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    // An edit that makes a memory too long, or empty, is refused and says what was typed.
    let memory = env
        .memory_store()
        .await
        .get(&owner_viewer(), MemoryId::new(1))
        .await
        .unwrap();
    let (status, html, _) = env
        .memory_post(
            &manager,
            owner(),
            &format!("{MEMORY}/edit"),
            &edit_form(&memory, &format!("z{exact}"), false, false),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        html.contains("the memory is 1001 characters; the limit is 1000"),
        "{html}"
    );
    assert!(
        html.contains("What you typed, so you can try again:"),
        "{html}"
    );
    assert!(html.contains(&format!("<pre>z{exact}</pre>")), "{html}");
    let (status, html, _) = env
        .memory_post(
            &manager,
            owner(),
            &format!("{MEMORY}/edit"),
            &edit_form(&memory, "   ", false, false),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        html.contains("Not saved: memory content is empty."),
        "{html}"
    );
    assert_eq!(env.rows(), [(1, exact, "user".to_owned(), false, false)]);
}

#[tokio::test]
async fn core_memories_stay_within_the_cap_of_their_scope() {
    let env = env().await;
    let manager = env.manager().await;
    let first = env
        .seed(
            &owner_viewer(),
            &"a".repeat(1000),
            true,
            false,
            "2026-03-01T00:00:00Z",
        )
        .await;
    env.seed(
        &owner_viewer(),
        &"b".repeat(1000),
        true,
        false,
        "2026-03-02T00:00:00Z",
    )
    .await;

    // 2,000 of 2,200 are used: 300 more would be 100 over. The page says so and lists the scope's
    // core memories so one can be trimmed.
    let (status, html, _) = env
        .memory_post(
            &manager,
            owner(),
            MEMORY,
            &post(&add_fields(&"c".repeat(300), true, false)),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    for expected in [
        "your core memories would use 2300/2200 characters, 100 over the cap. Nothing was changed.",
        "Your core memories now: [90% — 2,000/2,200]",
        format!("<li>#1, 1000 characters: {}</li>", "a".repeat(1000)).as_str(),
        format!("<li>#2, 1000 characters: {}</li>", "b".repeat(1000)).as_str(),
    ] {
        assert!(html.contains(expected), "{expected}\n{html}");
    }
    assert_eq!(env.rows().len(), 2);

    // Without Core it fits; promoting it afterwards is refused the same way.
    let (status, _, _) = env
        .memory_post(
            &manager,
            owner(),
            MEMORY,
            &post(&add_fields(&"c".repeat(300), false, false)),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let third = env
        .memory_store()
        .await
        .get(&owner_viewer(), MemoryId::new(3))
        .await
        .unwrap();
    let (status, html, _) = env
        .memory_post(
            &manager,
            owner(),
            &format!("{MEMORY}/edit"),
            &edit_form(&third, &"c".repeat(300), true, false),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(html.contains("100 over the cap"), "{html}");
    assert!(!env.rows()[2].4, "still not core");

    // The shared scope has a cap of its own.
    let (status, _, _) = env
        .memory_post(
            &manager,
            owner(),
            MEMORY,
            &post(&add_fields(&"s".repeat(300), true, true)),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let html = env.memory_get(&manager, MEMORY).await;
    assert!(
        html.contains("Shared core memories: <code>[13% — 300/2,200]</code>"),
        "{html}"
    );

    // Demoting the first makes room.
    let (status, _, _) = env
        .memory_post(
            &manager,
            owner(),
            &format!("{MEMORY}/edit"),
            &edit_form(&first, &"a".repeat(1000), false, false),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let third = env
        .memory_store()
        .await
        .get(&owner_viewer(), MemoryId::new(3))
        .await
        .unwrap();
    let (status, _, _) = env
        .memory_post(
            &manager,
            owner(),
            &format!("{MEMORY}/edit"),
            &edit_form(&third, &"c".repeat(300), true, false),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn an_edit_of_a_memory_that_changed_since_the_form_was_made_is_refused() {
    let env = env().await;
    let manager = env.manager().await;
    let loaded = env
        .seed(
            &owner_viewer(),
            "Likes tea.",
            false,
            false,
            "2026-03-01T00:00:00Z",
        )
        .await;
    // The page the user opened carries the `updated_at` they edit against.
    let html = env.memory_get(&manager, MEMORY).await;
    assert!(
        item(&html, loaded.id).contains(
            format!(
                r#"name="updated" value="{}""#,
                loaded.updated_at.as_millisecond()
            )
            .as_str()
        ),
        "{html}"
    );

    // Meanwhile the agent changes it (a tool call writes with its own clock).
    env.memory_store()
        .await
        .edit(
            &owner_viewer(),
            loaded.id,
            giles_store::MemoryEdit {
                content: Some("Likes coffee now.".into()),
                ..giles_store::MemoryEdit::default()
            },
            CAP,
            ts("2026-03-02T00:00:00Z"),
        )
        .await
        .unwrap();

    let (status, html, _) = env
        .memory_post(
            &manager,
            owner(),
            &format!("{MEMORY}/edit"),
            &edit_form(&loaded, "Likes tea and toast.", false, false),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    for expected in [
        "memory #1 changed after you opened the form",
        "Nothing was changed.",
        "What you typed, so you can try again:",
        "<pre>Likes tea and toast.</pre>",
        "The memory now reads:",
        "<pre>Likes coffee now.</pre>",
    ] {
        assert!(html.contains(expected), "{expected}\n{html}");
    }
    assert_eq!(
        env.rows(),
        [(
            1,
            "Likes coffee now.".to_owned(),
            "user".to_owned(),
            false,
            false
        )]
    );

    // With the current `updated_at` the same edit goes through.
    let current = env
        .memory_store()
        .await
        .get(&owner_viewer(), loaded.id)
        .await
        .unwrap();
    let (status, _, _) = env
        .memory_post(
            &manager,
            owner(),
            &format!("{MEMORY}/edit"),
            &edit_form(&current, "Likes tea and toast.", false, false),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(env.rows()[0].1, "Likes tea and toast.");

    // A form that doesn't carry the version it edits is refused, not applied blindly.
    for fields in [
        vec![("id", "1"), ("content", "Blind.")],
        vec![("id", "1"), ("updated", "soon"), ("content", "Blind.")],
        vec![("updated", "0"), ("content", "Blind.")],
    ] {
        let (status, html, _) = env
            .memory_post(&manager, owner(), &format!("{MEMORY}/edit"), &post(&fields))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{fields:?}");
        assert!(html.contains("That form is incomplete"), "{html}");
    }
    assert_eq!(env.rows()[0].1, "Likes tea and toast.");
}

#[tokio::test]
async fn unknown_and_invisible_ids_are_refused_alike() {
    let env = env().await;
    let manager = env.manager().await;
    let mine = env
        .seed(
            &owner_viewer(),
            "Mine.",
            false,
            false,
            "2026-03-01T00:00:00Z",
        )
        .await;
    let theirs = env
        .seed(
            &guest_viewer(),
            "Theirs.",
            false,
            false,
            "2026-03-01T00:00:00Z",
        )
        .await;

    // Nobody edits or forgets what they can't see, and the answer for it is the one for a
    // memory that isn't there.
    let (status, html, _) = env
        .memory_post(
            &manager,
            owner(),
            &format!("{MEMORY}/forget"),
            &post(&[("id", "99")]),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        html.contains("There is no memory #99 that you can see."),
        "{html}"
    );
    let (status, html, _) = env
        .memory_post(
            &manager,
            owner(),
            &format!("{MEMORY}/forget"),
            &post(&[("id", theirs.id.to_string().as_str())]),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        html.contains("There is no memory #2 that you can see."),
        "{html}"
    );
    assert!(!html.contains("Theirs."), "{html}");
    let (status, _, _) = env
        .memory_post(
            &manager,
            owner(),
            &format!("{MEMORY}/edit"),
            &edit_form(&theirs, "Mine now.", false, false),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = env
        .memory_post(
            &manager,
            guest(),
            &format!("{MEMORY}/edit"),
            &edit_form(&mine, "Guest's now.", false, false),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = env
        .memory_post(
            &manager,
            owner(),
            &format!("{MEMORY}/forget"),
            &post(&[("id", "x")]),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        env.rows(),
        [
            (1, "Mine.".to_owned(), "user".to_owned(), false, false),
            (2, "Theirs.".to_owned(), "user".to_owned(), false, false),
        ]
    );
}

#[tokio::test]
async fn the_owner_gets_a_section_for_each_group_with_memories_and_edits_them() {
    let env = env().await;
    let manager = env.manager().await;
    let libera = group_of("libera", "#giles");
    let quiet = env
        .seed(
            &libera,
            "Channel is quiet",
            true,
            false,
            "2026-03-01T00:00:00Z",
        )
        .await;
    let topic = env
        .seed(&libera, "Topic: tea", false, false, "2026-03-02T00:00:00Z")
        .await;
    // What a group wrote as shared belongs to everyone, so it's in the owner's own list.
    let rule = env
        .seed(&libera, "Group rule", false, true, "2026-03-03T00:00:00Z")
        .await;
    let elsewhere = env
        .seed(
            &group_of("discord", "room-9"),
            "Elsewhere",
            false,
            false,
            "2026-03-01T00:00:00Z",
        )
        .await;
    let mine = env
        .seed(
            &owner_viewer(),
            "Mine",
            false,
            false,
            "2026-03-01T00:00:00Z",
        )
        .await;

    let html = env.memory_get(&manager, MEMORY).await;
    let discord = html.find("<h2>Group discord / room-9</h2>").expect(&html);
    let libera_at = html.find("<h2>Group libera / #giles</h2>").expect(&html);
    assert!(discord < libera_at, "ordered by instance");
    let own = &html[..discord];
    let discord_section = &html[discord..libera_at];
    let libera_section = &html[libera_at..];
    assert!(
        own.contains("<pre>Mine</pre>") && own.contains("<pre>Group rule</pre>"),
        "{own}"
    );
    assert!(
        !own.contains("Channel is quiet") && !own.contains("Elsewhere"),
        "{own}"
    );
    assert!(
        discord_section.contains("<pre>Elsewhere</pre>"),
        "{discord_section}"
    );
    assert!(
        !discord_section.contains("Channel is quiet"),
        "{discord_section}"
    );
    assert_eq!(listed(libera_section), [topic.id.get(), quiet.id.get()]);
    assert_eq!(listed(discord_section), [elsewhere.id.get()]);
    assert!(listed(own).contains(&mine.id.get()) && listed(own).contains(&rule.id.get()));
    assert!(
        libera_section.contains("This group&#39;s core memories: <code>[0% — 16/2,200]</code>"),
        "{libera_section}"
    );
    assert!(
        libera_section.contains("which only you, the owner, can see here"),
        "{libera_section}"
    );
    // Each of the group's forms names the group, and offers *shared* with the owner's right.
    let topic_item = item(libera_section, topic.id);
    for expected in [
        r#"<input type="hidden" name="group_instance" value="libera">"#,
        r##"<input type="hidden" name="group_conversation" value="#giles">"##,
        r#"action="/admin/agents/jeeves/memory/edit""#,
        r#"action="/admin/agents/jeeves/memory/forget""#,
        r#"<input type="checkbox" name="shared">"#,
    ] {
        assert!(topic_item.contains(expected), "{expected}\n{topic_item}");
    }

    // Editing and forgetting go through the group's viewer, so the memories stay the group's.
    let group_fields = |id: &str, updated: &str, content: &str| {
        post(&[
            ("id", id),
            ("updated", updated),
            ("content", content),
            ("group_instance", "libera"),
            ("group_conversation", "#giles"),
        ])
    };
    let (status, _, location) = env
        .memory_post(
            &manager,
            owner(),
            &format!("{MEMORY}/edit"),
            &group_fields(
                topic.id.to_string().as_str(),
                topic.updated_at.as_millisecond().to_string().as_str(),
                "Topic: coffee",
            ),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        location.as_deref(),
        Some("/admin/agents/jeeves/memory?done=saved#memory-2")
    );
    let rows = env.rows();
    assert_eq!(
        rows[1],
        (
            2,
            "Topic: coffee".to_owned(),
            "group".to_owned(),
            false,
            false
        )
    );
    let (status, _, _) = env
        .memory_post(
            &manager,
            owner(),
            &format!("{MEMORY}/forget"),
            &post(&[
                ("id", quiet.id.to_string().as_str()),
                ("group_instance", "libera"),
                ("group_conversation", "#giles"),
            ]),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(env.rows().len(), 4);
    assert!(env.rows().iter().all(|row| row.0 != quiet.id.get()));

    // A group's memory is addressed through its group only: not from the owner's own scope, and
    // not from another group.
    let (status, _, _) = env
        .memory_post(
            &manager,
            owner(),
            &format!("{MEMORY}/forget"),
            &post(&[("id", topic.id.to_string().as_str())]),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = env
        .memory_post(
            &manager,
            owner(),
            &format!("{MEMORY}/forget"),
            &post(&[
                ("id", topic.id.to_string().as_str()),
                ("group_instance", "discord"),
                ("group_conversation", "room-9"),
            ]),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // A half-named group is not a group.
    let (status, _, _) = env
        .memory_post(
            &manager,
            owner(),
            &format!("{MEMORY}/forget"),
            &post(&[
                ("id", topic.id.to_string().as_str()),
                ("group_instance", "libera"),
            ]),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(env.rows().len(), 4);
}

#[tokio::test]
async fn only_the_owner_can_reach_a_groups_memories() {
    let env = env().await;
    let manager = env.manager().await;
    let group = env
        .seed(
            &group_of("libera", "#giles"),
            "Group secret",
            false,
            false,
            "2026-03-01T00:00:00Z",
        )
        .await;

    let html = env
        .memory_call(&manager, Some(guest()), Method::GET, MEMORY, None)
        .await
        .1;
    assert!(
        !html.contains("Group secret") && !html.contains("Group libera"),
        "{html}"
    );

    for sub in ["/forget", "/edit"] {
        let (status, html, _) = env
            .memory_post(
                &manager,
                guest(),
                &format!("{MEMORY}{sub}"),
                &post(&[
                    ("id", group.id.to_string().as_str()),
                    (
                        "updated",
                        group.updated_at.as_millisecond().to_string().as_str(),
                    ),
                    ("content", "Mine now."),
                    ("group_instance", "libera"),
                    ("group_conversation", "#giles"),
                ]),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{sub}");
        assert!(
            html.contains("Only the owner can see or change a group"),
            "{html}"
        );
        assert!(!html.contains("Group secret"), "{html}");
    }
    let (status, _, _) = env
        .memory_post(
            &manager,
            guest(),
            MEMORY,
            &post(&[
                ("content", "Planted."),
                ("group_instance", "libera"),
                ("group_conversation", "#giles"),
            ]),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        env.rows(),
        [(
            1,
            "Group secret".to_owned(),
            "group".to_owned(),
            false,
            false
        )]
    );
}

#[tokio::test]
async fn a_user_who_is_not_the_owner_writes_only_private_memories() {
    let env = env().await;
    let manager = env.manager().await;
    let shared = env
        .seed(
            &owner_viewer(),
            "House rule.",
            false,
            true,
            "2026-03-01T00:00:00Z",
        )
        .await;
    let secret = env
        .seed(
            &owner_viewer(),
            "Owner secret.",
            false,
            false,
            "2026-03-01T00:00:00Z",
        )
        .await;
    let own = env
        .seed(
            &guest_viewer(),
            "Guest note.",
            false,
            false,
            "2026-03-01T00:00:00Z",
        )
        .await;

    let (status, html, _) = env
        .memory_call(&manager, Some(guest()), Method::GET, MEMORY, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed(&html), [own.id.get(), shared.id.get()]);
    assert!(!html.contains("Owner secret."), "{html}");
    assert!(!listed(&html).contains(&secret.id.get()), "{html}");
    // No *shared* box anywhere, the shared memory has no forms, the guest's own one has.
    assert!(!html.contains(r#"name="shared""#), "{html}");
    let shared_item = item(&html, shared.id);
    assert!(
        shared_item.contains("Only the owner can change shared memories."),
        "{shared_item}"
    );
    assert!(!shared_item.contains("memory/edit") && !shared_item.contains("memory/forget"));
    assert!(item(&html, own.id).contains("memory/edit"), "{html}");
    // Not the admin pages' navigation, which would lead to pages they can't open.
    assert!(!html.contains("/admin/agents\">Agents"), "{html}");

    // The server refuses what the form doesn't offer, whatever is posted.
    let shared_edit = format!("{MEMORY}/edit");
    let (status, html, _) = env
        .memory_post(
            &manager,
            guest(),
            MEMORY,
            &post(&add_fields("Rule.", false, true)),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        html.contains("Only the owner can create, change, share, or forget shared memories."),
        "{html}"
    );
    let (status, _, _) = env
        .memory_post(
            &manager,
            guest(),
            &shared_edit,
            &edit_form(&shared, "Mine.", false, true),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = env
        .memory_post(
            &manager,
            guest(),
            &format!("{MEMORY}/forget"),
            &post(&[("id", shared.id.to_string().as_str())]),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = env
        .memory_post(
            &manager,
            guest(),
            &shared_edit,
            &edit_form(&own, "Guest note.", false, true),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "sharing one's own memory is writing a shared one"
    );
    assert_eq!(
        env.rows(),
        [
            (1, "House rule.".to_owned(), "user".to_owned(), true, false),
            (
                2,
                "Owner secret.".to_owned(),
                "user".to_owned(),
                false,
                false
            ),
            (3, "Guest note.".to_owned(), "user".to_owned(), false, false),
        ]
    );

    // Their own private memories work like the owner's, in their own scope.
    let (status, _, _) = env
        .memory_post(
            &manager,
            guest(),
            MEMORY,
            &post(&add_fields("Likes jam.", true, false)),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, _, _) = env
        .memory_post(
            &manager,
            guest(),
            &shared_edit,
            &edit_form(&own, "Guest note, edited.", false, false),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, _, _) = env
        .memory_post(
            &manager,
            guest(),
            &format!("{MEMORY}/forget"),
            &post(&[("id", "4")]),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let rows = env.rows();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[2].1, "Guest note, edited.");
    let owners_view = env
        .memory_store()
        .await
        .list(&owner_viewer(), 10)
        .await
        .unwrap();
    assert!(
        owners_view
            .iter()
            .all(|m| m.content != "Guest note, edited.")
    );
}

#[tokio::test]
async fn search_ranks_with_the_same_full_text_search_as_memory_recall() {
    let env = env().await;
    let manager = env.manager().await;
    // The long one is newer, the short one repeats the word: ranked first by the search, last by
    // the list.
    let long = env
        .seed(
            &owner_viewer(),
            "Garden notes: one tin of tea next to the biscuits and the old kettle and more",
            false,
            false,
            "2026-03-05T00:00:00Z",
        )
        .await;
    let short = env
        .seed(
            &owner_viewer(),
            "tea tea tea",
            true,
            false,
            "2026-03-01T00:00:00Z",
        )
        .await;
    let shared = env
        .seed(
            &owner_viewer(),
            "Shared: tea is at four.",
            false,
            true,
            "2026-03-02T00:00:00Z",
        )
        .await;
    env.seed(
        &owner_viewer(),
        "Cats are not allowed.",
        false,
        false,
        "2026-03-06T00:00:00Z",
    )
    .await;
    env.seed(
        &guest_viewer(),
        "Guest tea secret.",
        false,
        false,
        "2026-03-07T00:00:00Z",
    )
    .await;

    let expected: Vec<i64> = env
        .memory_store()
        .await
        .recall(&owner_viewer(), "tea", 50)
        .await
        .unwrap()
        .iter()
        .map(|m| m.id.get())
        .collect();
    assert_eq!(expected.len(), 3);
    assert_eq!(expected[0], short.id.get(), "the repeated word ranks first");
    let all = env.memory_get(&manager, MEMORY).await;
    assert_ne!(listed(&all), expected);

    let html = env.memory_get(&manager, &format!("{MEMORY}?q=tea")).await;
    assert_eq!(listed(&html), expected);
    assert!(listed(&html).contains(&long.id.get()) && listed(&html).contains(&shared.id.get()));
    assert!(html.contains("<h2>Matching memories</h2>"), "{html}");
    assert!(
        html.contains(r#"<input id="q" type="search" name="q" value="tea">"#),
        "{html}"
    );
    assert!(
        !html.contains("Cats are not allowed.") && !html.contains("Guest tea secret."),
        "{html}"
    );
    // Matches can be edited like any listed memory.
    assert!(item(&html, short.id).contains("memory/edit"), "{html}");

    // English stemming applies, as in `memory_recall`.
    let html = env.memory_get(&manager, &format!("{MEMORY}?q=teas")).await;
    assert_eq!(listed(&html), expected);

    // A search that finds nothing says so.
    let html = env.memory_get(&manager, &format!("{MEMORY}?q=zebra")).await;
    assert!(listed(&html).is_empty());
    assert!(html.contains("No memory matches that search."), "{html}");

    // FTS syntax in the words is text, not syntax: no error, and the plain word still matches.
    for query in [
        "%22tea%22%20*%20OR%20(%20NEAR%2F2",
        "tea%22",
        "*",
        "AND",
        "%22",
        "%3A%5E-",
    ] {
        let (status, html, _) = env
            .memory_call(
                &manager,
                Some(owner()),
                Method::GET,
                &format!("{MEMORY}?q={query}"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{query}\n{html}");
        assert!(!html.contains("Memory unavailable"), "{query}");
    }
    let html = env
        .memory_get(
            &manager,
            &format!("{MEMORY}?q=%22tea%22%20*%20OR%20(%20NEAR%2F2"),
        )
        .await;
    assert!(listed(&html).contains(&short.id.get()), "{html}");

    // An empty (or blank) search is the plain list.
    let html = env
        .memory_get(&manager, &format!("{MEMORY}?q=%20%20"))
        .await;
    assert_eq!(listed(&html), listed(&all));
    assert!(!html.contains("Matching memories"));
}

#[tokio::test]
async fn a_long_list_shows_the_newest_and_says_it_is_cut_short() {
    let env = env().await;
    let manager = env.manager().await;
    for i in 0..=PAGE_SIZE {
        let at = Timestamp::from_second(1_800_000_000 + i64::from(i)).unwrap();
        env.memory_store()
            .await
            .add(
                &owner_viewer(),
                NewMemory {
                    content: format!("memory number {i}"),
                    core: false,
                    shared: false,
                    source_session: None,
                },
                CAP,
                at,
            )
            .await
            .unwrap();
    }
    let html = env.memory_get(&manager, MEMORY).await;
    assert_eq!(listed(&html).len(), PAGE_SIZE as usize);
    assert!(
        html.contains(&format!("Only the newest {PAGE_SIZE} memories are shown.")),
        "{html}"
    );
    assert!(
        html.contains(&format!("<pre>memory number {PAGE_SIZE}</pre>")),
        "{html}"
    );
    assert!(!html.contains("<pre>memory number 0</pre>"), "{html}");
    // The cut-off one is still found by search.
    let html = env.memory_get(&manager, &format!("{MEMORY}?q=0")).await;
    assert!(html.contains("<pre>memory number 0</pre>"), "{html}");
}

#[test]
fn the_viewer_comes_from_the_signed_in_user() {
    let owner_viewer = user_viewer(&owner());
    assert_eq!(owner_viewer.owner, MemoryOwner::User(identity::owner()));
    assert!(owner_viewer.can_write_shared);
    let guest_viewer = user_viewer(&guest());
    assert_eq!(guest_viewer.owner, MemoryOwner::User(guest().user));
    assert!(!guest_viewer.can_write_shared);

    let group = group_viewer(&owner(), "libera", "#giles").unwrap();
    assert_eq!(group.owner, group_owner("libera", "#giles"));
    assert!(group.can_write_shared);
    assert_eq!(
        group_viewer(&guest(), "libera", "#giles"),
        Err(ScopeError::NotOwner)
    );
    assert_eq!(
        group_viewer(&owner(), "", "#giles"),
        Err(ScopeError::Invalid)
    );
    assert_eq!(
        group_viewer(&owner(), "libera", ""),
        Err(ScopeError::Invalid)
    );
}

#[tokio::test]
async fn a_stale_edit_error_is_the_stores() {
    // The page maps the store's error; make sure the store really raises it for the guard the
    // form uses, so the 409 above can't pass for another reason.
    let env = env().await;
    let memory = env
        .seed(
            &owner_viewer(),
            "Once.",
            false,
            false,
            "2026-03-01T00:00:00Z",
        )
        .await;
    let store = env.memory_store().await;
    let stale = store
        .edit(
            &owner_viewer(),
            memory.id,
            giles_store::MemoryEdit {
                content: Some("Twice.".into()),
                expected_updated_at: Some(ts("2026-02-28T00:00:00Z")),
                ..giles_store::MemoryEdit::default()
            },
            CAP,
            ts("2026-03-02T00:00:00Z"),
        )
        .await;
    assert!(matches!(stale, Err(StoreError::StaleMemory(id)) if id == memory.id));
}

//! The memory page of an agent: the memories the signed-in user can see, a search over them, and
//! the forms that add, change, and forget one. Part of the agent pages (`agents.rs` routes them):
//! behind the login, every answer `no-store`, every post CSRF-checked.
//!
//! # Who sees what
//!
//! The page asks the agent's memory database ([`giles_store::MemoryStore`], fetched from
//! [`MemoryDbs`](crate::memory_dbs::MemoryDbs) for each request and not kept) as the signed-in
//! user, with the same rules as the memory tools ([`user_viewer`]): their own private memories and
//! the shared ones, and the right to write shared memories only for the owner. The store enforces
//! all of it; the forms only leave out what the store would refuse (the *shared* box for anyone but
//! the owner). The owner also gets one section per group conversation that has memories, under that
//! group's own viewer ([`group_viewer`]); every form of such a section names its group, and the
//! handler checks the user is the owner before it builds that viewer, so nobody else can address a
//! group.
//!
//! Unlike the other agent pages this one is not for the owner only: it shows every signed-in user
//! what they could ask the agent to recall anyway. A `GET` opens (and so creates) the agent's
//! `memory.db` when it doesn't exist yet; an unknown agent never gets one.
//!
//! # Writes
//!
//! A post that succeeds answers 303 to the page with a fixed `done` code for the notice. One the
//! store refuses re-renders the page with the reason (and what was typed), with a status that
//! tells why: 400 for input, 403 for the shared-write rule, 404 for an unknown memory, and 409 for
//! a stale edit or a core scope over its cap. An edit carries the memory's `updated_at` (Unix
//! milliseconds) from when its form was rendered, and the store refuses it if the memory changed
//! since ([`MemoryEdit::expected_updated_at`]); the page then shows the current text too.

use askama::Template;
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use giles_store::{
    Error as StoreError, MAX_MEMORY_CHARS, Memory, MemoryEdit, MemoryId, MemoryOwner, MemoryScope,
    MemoryStore, MemoryViewer, NewMemory,
};
use giles_types::{ConversationId, Timestamp};
use serde::Deserialize;
use tracing::error;

use super::admin::may_administer;
use super::agents::AgentPages;
use super::auth::{WebUser, check_csrf};
use super::pages::{agent_label, error_page};
use crate::config::AgentConfig;
use crate::http;
use crate::identity;
use crate::memory::{flags, usage_header};

/// How many memories a list shows; the page says when there are more.
pub(super) const PAGE_SIZE: u32 = 200;

/// How many matches a search shows.
const SEARCH_LIMIT: u32 = 50;

/// How many group sections the page shows; the page says when there are more.
const MAX_GROUPS: usize = 20;

/// How many rows to read for a group's section: the store lists a group viewer's shared memories
/// with the group's own, and the page keeps the group's only, so it reads past the shared ones.
const GROUP_FETCH: u32 = 10_000;

/// The viewer of the user's own scope: their private memories and the shared ones, writing the
/// shared ones only if they are the owner.
pub(super) fn user_viewer(user: &WebUser) -> MemoryViewer {
    MemoryViewer::new(
        MemoryOwner::User(user.user.clone()),
        user.user == identity::owner(),
    )
}

/// The viewer of one group conversation's private memories, with the owner's right to write
/// shared ones. `Err` unless `user` is the owner and the group is well-formed.
pub(super) fn group_viewer(
    user: &WebUser,
    instance: &str,
    conversation: &str,
) -> Result<MemoryViewer, ScopeError> {
    if !may_administer(user) {
        return Err(ScopeError::NotOwner);
    }
    let conversation = ConversationId::new(conversation).map_err(|_| ScopeError::Invalid)?;
    if instance.is_empty() {
        return Err(ScopeError::Invalid);
    }
    Ok(MemoryViewer::new(
        MemoryOwner::Group {
            instance: instance.to_owned(),
            conversation,
        },
        true,
    ))
}

/// Why a form's scope can't be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ScopeError {
    /// A group's memories belong to the owner's page only.
    NotOwner,
    /// The group in the form isn't an instance and a conversation id.
    Invalid,
}

impl ScopeError {
    fn response(self) -> Response {
        match self {
            Self::NotOwner => error_page(
                StatusCode::FORBIDDEN,
                "Not allowed",
                "Only the owner can see or change a group's memories.",
            ),
            Self::Invalid => error_page(
                StatusCode::BAD_REQUEST,
                "Not a group",
                "The form names a group conversation that doesn't exist.",
            ),
        }
    }
}

/// A group on the page and in its forms.
struct GroupKey {
    instance: String,
    conversation: String,
}

/// One memory on the page.
struct MemoryView {
    id: i64,
    content: String,
    core: bool,
    shared: bool,
    flags: String,
    chars: usize,
    created: String,
    created_iso: String,
    updated: String,
    updated_iso: String,
    /// The memory's `updated_at` in Unix milliseconds: what its edit form must send back.
    updated_ms: i64,
    /// Whether the user may change or forget it.
    writable: bool,
}

impl MemoryView {
    fn new(memory: &Memory, can_write_shared: bool) -> Self {
        let when = |at: Timestamp| {
            (
                at.strftime("%Y-%m-%d %H:%M UTC").to_string(),
                at.to_string(),
            )
        };
        let (created, created_iso) = when(memory.created_at);
        let (updated, updated_iso) = when(memory.updated_at);
        Self {
            id: memory.id.get(),
            content: memory.content.clone(),
            core: memory.core,
            shared: memory.shared,
            flags: flags(memory),
            chars: memory.chars(),
            created,
            created_iso,
            updated,
            updated_iso,
            updated_ms: memory.updated_at.as_millisecond(),
            writable: !memory.shared || can_write_shared,
        }
    }
}

/// The usage of one core scope: `[67% — 1,474/2,200]` under a title.
struct Usage {
    title: &'static str,
    header: String,
}

/// A list of memories with the forms that go with it.
struct Section {
    title: String,
    /// The group this section lists, if it is a group's.
    group: Option<GroupKey>,
    /// The user's own section: the one with the add form.
    own: bool,
    /// Whether the forms offer *shared*.
    can_share: bool,
    searching: bool,
    truncated: bool,
    limit: u32,
    usages: Vec<Usage>,
    memories: Vec<MemoryView>,
}

/// A line of a scope's core memories, shown when one would go over the cap.
struct CoreLine {
    id: i64,
    chars: usize,
    content: String,
}

/// Why a post wasn't done, and what helps the user redo it.
struct Problem {
    message: String,
    /// What the user typed, for an edit (an add keeps it in the form).
    typed: Option<String>,
    /// What a memory now says, when an edit found it changed.
    current: Option<String>,
    /// The title of the scope's core memories, when the cap was the problem.
    core_title: Option<String>,
    core: Vec<CoreLine>,
}

impl Problem {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            typed: None,
            current: None,
            core_title: None,
            core: Vec::new(),
        }
    }
}

/// The add form's fields, kept when an add is refused.
#[derive(Default)]
struct AddForm {
    content: String,
    core: bool,
    shared: bool,
}

#[derive(Template)]
#[template(path = "web/admin_agent_memory.html")]
struct MemoryPage {
    csrf: String,
    agent: String,
    label: String,
    /// Whether to show the admin navigation (the owner) or just a way back to the chats.
    admin: bool,
    notice: Option<&'static str>,
    problem: Option<Problem>,
    query: String,
    sections: Vec<Section>,
    more_groups: usize,
    add: AddForm,
    max_chars: usize,
    cap: u32,
}

/// What a render shows besides the memories.
#[derive(Default)]
struct View {
    query: String,
    notice: Option<&'static str>,
    problem: Option<Problem>,
    add: AddForm,
    /// The status of the answer; `None` is 200.
    status: Option<StatusCode>,
}

/// The configured agent `agent`, or the 404 page.
fn find_agent(pages: &AgentPages, agent: &str) -> Result<AgentConfig, Box<Response>> {
    let config = pages.manager.config().get();
    giles_types::AgentId::new(agent)
        .ok()
        .and_then(|id| config.agents.get(&id))
        .cloned()
        .ok_or_else(|| {
            Box::new(error_page(
                StatusCode::NOT_FOUND,
                "No such agent",
                "There is no agent with that name.",
            ))
        })
}

/// The agent's memory database for this request.
async fn open(pages: &AgentPages, agent: &AgentConfig) -> Result<MemoryStore, Box<Response>> {
    pages
        .manager
        .memory_dbs()
        .get(&agent.id)
        .await
        .map_err(|err| Box::new(unavailable(agent, &err)))
}

fn unavailable(agent: &AgentConfig, err: &StoreError) -> Response {
    error!(agent = %agent.id, error = %err, "reading an agent's memories failed");
    error_page(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Memory unavailable",
        "Giles couldn't read or change the agent's memories. The error is in its log.",
    )
}

/// What `GET /admin/agents/<agent>/memory` takes.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct PageQuery {
    /// The search words.
    q: String,
    /// What a redirect after a change says happened.
    done: String,
}

/// `GET /admin/agents/<agent>/memory`: the memories the user can see (or the ones `q` finds),
/// and for the owner a section for each group conversation that has memories.
pub(super) async fn page(
    State(pages): State<AgentPages>,
    user: WebUser,
    Path(agent): Path<String>,
    Query(query): Query<PageQuery>,
) -> Response {
    let config = match find_agent(&pages, &agent) {
        Ok(config) => config,
        Err(response) => return *response,
    };
    let notice = match query.done.as_str() {
        "added" => Some("Memory added."),
        "saved" => Some("Memory saved."),
        "forgotten" => Some("Memory forgotten."),
        _ => None,
    };
    render(
        &pages,
        &user,
        &config,
        View {
            query: query.q,
            notice,
            ..View::default()
        },
    )
    .await
}

/// Renders the page with `view`.
async fn render(
    pages: &AgentPages,
    user: &WebUser,
    agent: &AgentConfig,
    mut view: View,
) -> Response {
    let store = match open(pages, agent).await {
        Ok(store) => store,
        Err(response) => return *response,
    };
    let status = view.status.take();
    let page = match build(pages, user, agent, &store, view).await {
        Ok(page) => page,
        Err(err) => return unavailable(agent, &err),
    };
    let mut response = http::render(&page);
    if let (Some(status), true) = (status, response.status().is_success()) {
        *response.status_mut() = status;
    }
    response
}

async fn build(
    pages: &AgentPages,
    user: &WebUser,
    agent: &AgentConfig,
    store: &MemoryStore,
    view: View,
) -> giles_store::Result<MemoryPage> {
    let cap = agent.memory_cap as usize;
    let viewer = user_viewer(user);
    let query = view.query.trim().to_owned();
    let searching = !query.is_empty();
    let limit = if searching { SEARCH_LIMIT } else { PAGE_SIZE };
    let mut found = if searching {
        store.recall(&viewer, &query, limit + 1).await?
    } else {
        store.list(&viewer, limit + 1).await?
    };
    let truncated = found.len() > limit as usize;
    found.truncate(limit as usize);
    let core = store.core_memories(&viewer).await?;
    let mut sections = vec![Section {
        title: if searching {
            "Matching memories".to_owned()
        } else {
            "Your memories and the shared ones".to_owned()
        },
        group: None,
        own: true,
        can_share: viewer.can_write_shared,
        searching,
        truncated,
        limit,
        usages: vec![
            Usage {
                title: "Shared core memories",
                header: usage_header(core.shared.chars, cap),
            },
            Usage {
                title: "Your core memories",
                header: usage_header(core.own.chars, cap),
            },
        ],
        memories: found
            .iter()
            .map(|memory| MemoryView::new(memory, viewer.can_write_shared))
            .collect(),
    }];

    let mut more_groups = 0;
    if may_administer(user) {
        let groups = store.group_owners().await?;
        more_groups = groups.len().saturating_sub(MAX_GROUPS);
        for group in groups.into_iter().take(MAX_GROUPS) {
            let owner = MemoryOwner::Group {
                instance: group.instance.clone(),
                conversation: group.conversation.clone(),
            };
            let viewer = MemoryViewer::new(owner.clone(), true);
            let mut memories: Vec<Memory> = store
                .list(&viewer, GROUP_FETCH)
                .await?
                .into_iter()
                .filter(|memory| !memory.shared && memory.owner == owner)
                .collect();
            let truncated = memories.len() > PAGE_SIZE as usize;
            memories.truncate(PAGE_SIZE as usize);
            let core = store.core_memories(&viewer).await?;
            sections.push(Section {
                title: format!("Group {} / {}", group.instance, group.conversation),
                group: Some(GroupKey {
                    instance: group.instance,
                    conversation: group.conversation.to_string(),
                }),
                own: false,
                can_share: true,
                searching: false,
                truncated,
                limit: PAGE_SIZE,
                usages: vec![Usage {
                    title: "This group's core memories",
                    header: usage_header(core.own.chars, cap),
                }],
                memories: memories
                    .iter()
                    .map(|memory| MemoryView::new(memory, true))
                    .collect(),
            });
        }
    }

    let config = pages.manager.config().get();
    Ok(MemoryPage {
        csrf: user.csrf.clone(),
        agent: agent.id.to_string(),
        label: agent_label(&config, &agent.id),
        admin: may_administer(user),
        notice: view.notice,
        problem: view.problem,
        query,
        sections,
        more_groups,
        add: view.add,
        max_chars: MAX_MEMORY_CHARS,
        cap: agent.memory_cap,
    })
}

/// The fields of the add, edit, and forget forms, as posted. A field left out is empty.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct MemoryForm {
    csrf: Option<String>,
    /// The memory's id (edit and forget).
    id: String,
    content: String,
    /// A checkbox: present when ticked.
    core: Option<String>,
    shared: Option<String>,
    /// The memory's `updated_at` in Unix milliseconds, as the edit form was rendered.
    updated: String,
    /// The group of a group section's forms; both empty for the user's own scope.
    group_instance: String,
    group_conversation: String,
}

impl MemoryForm {
    /// The viewer the form acts as: the user's own, or the named group's for the owner.
    fn viewer(&self, user: &WebUser) -> Result<MemoryViewer, Box<Response>> {
        if self.group_instance.is_empty() && self.group_conversation.is_empty() {
            return Ok(user_viewer(user));
        }
        group_viewer(user, &self.group_instance, &self.group_conversation)
            .map_err(|err| Box::new(err.response()))
    }

    /// The text with browsers' CRLF line breaks as LF.
    fn content(&self) -> String {
        self.content.replace("\r\n", "\n")
    }
}

/// A write the store refused, as the page answers it.
struct Refusal {
    status: StatusCode,
    problem: Problem,
}

/// How a refused write reads on the page, or `None` for an error that is the database's and not
/// the user's.
fn refusal(error: &StoreError) -> Option<Refusal> {
    let (status, problem) = match error {
        StoreError::UnknownMemory(id) => (
            StatusCode::NOT_FOUND,
            Problem::new(format!(
                "There is no memory #{id} that you can see. It may have been forgotten; reload the page."
            )),
        ),
        StoreError::SharedWriteForbidden => (
            StatusCode::FORBIDDEN,
            Problem::new(
                "Only the owner can create, change, share, or forget shared memories. Private memories aren't affected.",
            ),
        ),
        StoreError::MemoryTooLong { chars, max } => (
            StatusCode::BAD_REQUEST,
            Problem::new(format!(
                "Not saved: the memory is {chars} characters; the limit is {max}. Shorten it, or split it into several memories."
            )),
        ),
        StoreError::StaleMemory(id) => (
            StatusCode::CONFLICT,
            Problem::new(format!(
                "Not saved: memory #{id} changed after you opened the form, so your edit would have overwritten that change. Nothing was changed."
            )),
        ),
        StoreError::CoreCapExceeded(overflow) => {
            let (label, title) = match &overflow.scope {
                MemoryScope::Shared => ("the shared core memories", "Shared core memories"),
                MemoryScope::Private(MemoryOwner::Group { .. }) => {
                    ("this group's core memories", "This group's core memories")
                }
                MemoryScope::Private(_) => ("your core memories", "Your core memories"),
            };
            let mut problem = Problem::new(format!(
                "Not saved: {label} would use {}/{} characters, {} over the cap. Nothing was changed. Make room by shortening or forgetting some of the core memories below, or untick Core on some (a memory that isn't core is still found by search and by the agent's memory_recall). A memory too long for the cap can only be saved without Core.",
                overflow.cap + overflow.overflow,
                overflow.cap,
                overflow.overflow,
            ));
            problem.core_title = Some(format!(
                "{title} now: {}",
                usage_header(overflow.used, overflow.cap)
            ));
            problem.core = overflow
                .memories
                .iter()
                .map(|memory| CoreLine {
                    id: memory.id.get(),
                    chars: memory.chars(),
                    content: memory.content.clone(),
                })
                .collect();
            (StatusCode::CONFLICT, problem)
        }
        StoreError::InvalidRequest(message) => (
            StatusCode::BAD_REQUEST,
            Problem::new(format!("Not saved: {message}.")),
        ),
        _ => return None,
    };
    Some(Refusal { status, problem })
}

/// The 303 to the page, with `done` for its notice and the memory to scroll to.
fn done(agent: &str, done: &str, id: Option<MemoryId>) -> Response {
    let anchor = id.map(|id| format!("#memory-{id}")).unwrap_or_default();
    Redirect::to(&format!("/admin/agents/{agent}/memory?done={done}{anchor}")).into_response()
}

/// Answers a refused write: the page again with why, or the error page for a database failure.
async fn refused(
    pages: &AgentPages,
    user: &WebUser,
    agent: &AgentConfig,
    error: &StoreError,
    view: View,
) -> Response {
    match refusal(error) {
        Some(Refusal { status, problem }) => {
            render(
                pages,
                user,
                agent,
                View {
                    problem: Some(problem),
                    status: Some(status),
                    ..view
                },
            )
            .await
        }
        None => unavailable(agent, error),
    }
}

/// `POST /admin/agents/<agent>/memory` (CSRF-checked): adds a memory, then 303 to the page. A
/// memory the store refuses is shown again in the add form with why; nothing is written.
pub(super) async fn add(
    State(pages): State<AgentPages>,
    user: WebUser,
    Path(agent): Path<String>,
    headers: HeaderMap,
    Form(form): Form<MemoryForm>,
) -> Response {
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let config = match find_agent(&pages, &agent) {
        Ok(config) => config,
        Err(response) => return *response,
    };
    let viewer = match form.viewer(&user) {
        Ok(viewer) => viewer,
        Err(response) => return *response,
    };
    let store = match open(&pages, &config).await {
        Ok(store) => store,
        Err(response) => return *response,
    };
    let new = NewMemory {
        content: form.content(),
        core: form.core.is_some(),
        shared: form.shared.is_some(),
        source_session: None,
    };
    let add = AddForm {
        content: new.content.clone(),
        core: new.core,
        shared: new.shared,
    };
    match store
        .add(&viewer, new, config.memory_cap as usize, Timestamp::now())
        .await
    {
        Ok(memory) => done(&agent, "added", Some(memory.id)),
        Err(err) => {
            refused(
                &pages,
                &user,
                &config,
                &err,
                View {
                    add,
                    ..View::default()
                },
            )
            .await
        }
    }
}

/// The page for a form that doesn't say which memory (or which version of it) it is about.
async fn incomplete(pages: &AgentPages, user: &WebUser, agent: &AgentConfig) -> Response {
    render(
        pages,
        user,
        agent,
        View {
            problem: Some(Problem::new(
                "That form is incomplete: it doesn't say which memory it changes. Reload the page and try again.",
            )),
            status: Some(StatusCode::BAD_REQUEST),
            ..View::default()
        },
    )
    .await
}

/// The memory id a form names.
fn form_id(form: &MemoryForm) -> Option<MemoryId> {
    form.id.trim().parse::<i64>().ok().map(MemoryId::new)
}

/// `POST /admin/agents/<agent>/memory/edit` (CSRF-checked): changes a memory's content and flags,
/// then 303 to the page. The form's `updated` is the memory's `updated_at` when the form was
/// made; if the memory changed since, nothing is written and the page shows what it says now.
pub(super) async fn edit(
    State(pages): State<AgentPages>,
    user: WebUser,
    Path(agent): Path<String>,
    headers: HeaderMap,
    Form(form): Form<MemoryForm>,
) -> Response {
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let config = match find_agent(&pages, &agent) {
        Ok(config) => config,
        Err(response) => return *response,
    };
    let viewer = match form.viewer(&user) {
        Ok(viewer) => viewer,
        Err(response) => return *response,
    };
    let expected = form
        .updated
        .trim()
        .parse::<i64>()
        .ok()
        .and_then(|millis| Timestamp::from_millisecond(millis).ok());
    let (Some(id), Some(expected)) = (form_id(&form), expected) else {
        return incomplete(&pages, &user, &config).await;
    };
    let store = match open(&pages, &config).await {
        Ok(store) => store,
        Err(response) => return *response,
    };
    let content = form.content();
    let change = MemoryEdit {
        content: Some(content.clone()),
        core: Some(form.core.is_some()),
        shared: Some(form.shared.is_some()),
        expected_updated_at: Some(expected),
    };
    match store
        .edit(
            &viewer,
            id,
            change,
            config.memory_cap as usize,
            Timestamp::now(),
        )
        .await
    {
        Ok(memory) => done(&agent, "saved", Some(memory.id)),
        Err(err) => {
            let mut response = refusal(&err);
            if let Some(Refusal { problem, .. }) = &mut response {
                problem.typed = Some(content);
                if matches!(err, StoreError::StaleMemory(_)) {
                    problem.current = store.get(&viewer, id).await.ok().map(|m| m.content);
                }
            }
            match response {
                Some(Refusal { status, problem }) => {
                    render(
                        &pages,
                        &user,
                        &config,
                        View {
                            problem: Some(problem),
                            status: Some(status),
                            ..View::default()
                        },
                    )
                    .await
                }
                None => unavailable(&config, &err),
            }
        }
    }
}

/// `POST /admin/agents/<agent>/memory/forget` (CSRF-checked): forgets a memory, then 303 to the
/// page.
pub(super) async fn forget(
    State(pages): State<AgentPages>,
    user: WebUser,
    Path(agent): Path<String>,
    headers: HeaderMap,
    Form(form): Form<MemoryForm>,
) -> Response {
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let config = match find_agent(&pages, &agent) {
        Ok(config) => config,
        Err(response) => return *response,
    };
    let viewer = match form.viewer(&user) {
        Ok(viewer) => viewer,
        Err(response) => return *response,
    };
    let Some(id) = form_id(&form) else {
        return incomplete(&pages, &user, &config).await;
    };
    let store = match open(&pages, &config).await {
        Ok(store) => store,
        Err(response) => return *response,
    };
    match store.forget(&viewer, id).await {
        Ok(_) => done(&agent, "forgotten", None),
        Err(err) => refused(&pages, &user, &config, &err, View::default()).await,
    }
}

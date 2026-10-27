//! The agent pages: every agent, one agent's configuration and files, and the
//! forms that create and edit an agent and the page that removes one while the core runs. Next to
//! the admin pages (`admin.rs`) and like them: behind the login, the owner only, every answer
//! `no-store`, the form post CSRF-checked.
//!
//! The pages call the core's agent management ([`crate::agents`]) in-process. File contents are
//! shown as escaped text in `<pre>`, within the caps of [`list_files`]; `agent.toml` holds no
//! secrets (they live in `secrets.toml`, which no page shows).

use std::sync::Arc;

use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{from_fn_with_state, map_response};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use giles_types::AgentId;
use serde::Deserialize;
use tracing::error;

use super::admin::{forbidden, may_administer};
use super::auth::{WebUser, check_csrf};
use super::memory;
use super::pages::{WebApp, agent_label, error_page, no_store};
use crate::agents::{
    AgentEdit, AgentFile, AgentForm, AgentManager, Content, CreateError, DEFAULT_SOUL, EditError,
    MAX_FILE_BYTES, MAX_FILES, MAX_TOTAL_BYTES, NewAgent, RemovalPreview, RemoveError, Removed,
    agent_id_from_name, list_files, split_list,
};
use crate::config::AgentConfig;
use crate::http::{self, RouterBuilder};

/// The agent pages' state (shared with the memory page, `memory.rs`).
#[derive(Clone)]
pub(super) struct AgentPages {
    pub(super) manager: Arc<AgentManager>,
}

impl WebApp {
    /// Adds the agent pages to `builder`, creating agents with `manager` (whose running
    /// configuration should be this app's, so the chat list offers a new agent at once), behind
    /// the same authentication middleware as the chat pages. Every answer on these routes is
    /// `no-store`.
    pub fn install_agents(
        &self,
        builder: RouterBuilder,
        manager: Arc<AgentManager>,
    ) -> RouterBuilder {
        let auth = from_fn_with_state(self.clone(), super::login::authenticate);
        builder.merge(
            Router::new()
                .route("/admin/agents", get(agents_page).post(create))
                .route("/admin/agents/new", get(new_agent_page))
                .route("/admin/agents/{agent}", get(agent_page))
                .route("/admin/agents/{agent}/edit", get(edit_page).post(edit))
                .route(
                    "/admin/agents/{agent}/remove",
                    get(remove_page).post(remove),
                )
                .route(
                    "/admin/agents/{agent}/memory",
                    get(memory::page).post(memory::add),
                )
                .route("/admin/agents/{agent}/memory/edit", post(memory::edit))
                .route("/admin/agents/{agent}/memory/forget", post(memory::forget))
                .with_state(AgentPages { manager })
                .route_layer(auth)
                .route_layer(map_response(async |response: Response| no_store(response))),
        )
    }
}

/// An agent in the list.
struct AgentRow {
    id: String,
    label: String,
    models: String,
}

#[derive(Template)]
#[template(path = "web/admin_agents.html")]
struct AgentsPage {
    agents: Vec<AgentRow>,
}

/// `GET /admin/agents`: every agent with its models, a link to its page, and one to chat with it.
async fn agents_page(State(pages): State<AgentPages>, user: WebUser) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    let config = pages.manager.config().get();
    let agents = config
        .agents
        .values()
        .map(|agent| AgentRow {
            id: agent.id.to_string(),
            label: agent_label(&config, &agent.id),
            models: models(agent),
        })
        .collect();
    http::render(&AgentsPage { agents })
}

fn models(agent: &AgentConfig) -> String {
    agent
        .models
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A file on an agent's page.
struct FileView {
    path: String,
    label: Option<String>,
    /// The text shown, if any.
    text: Option<String>,
    /// What's known about it besides: its size, or why it isn't shown.
    note: String,
}

impl FileView {
    fn new(file: AgentFile) -> Self {
        let (text, note) = match file.content {
            Content::Text {
                text,
                size,
                truncated: false,
            } => (Some(text), bytes(size)),
            Content::Text { text, size, .. } => (
                Some(text),
                format!(
                    "{}; only the first {} KiB are shown",
                    bytes(size),
                    MAX_FILE_BYTES / 1024
                ),
            ),
            Content::Binary { size } => (None, format!("{}, not text: not shown", bytes(size))),
            Content::NotShown { size } => (
                None,
                format!(
                    "{}; not shown: the page already shows {} MiB",
                    bytes(size),
                    MAX_TOTAL_BYTES / (1024 * 1024)
                ),
            ),
            Content::Link => (None, "a symbolic link: not followed".to_owned()),
            Content::Unreadable(error) => (None, format!("cannot be read: {error}")),
            // `Special` and anything added later.
            _ => (None, "not a regular file: not shown".to_owned()),
        };
        Self {
            label: file.kind.label(),
            path: file.path,
            text,
            note,
        }
    }
}

fn bytes(size: u64) -> String {
    if size == 1 {
        "1 byte".to_owned()
    } else {
        format!("{size} bytes")
    }
}

#[derive(Template)]
#[template(path = "web/admin_agent.html")]
struct AgentPage {
    id: String,
    label: String,
    models: String,
    tools: String,
    ask: String,
    timezone: Option<String>,
    files: Vec<FileView>,
    /// Why the files couldn't be listed.
    files_error: Option<String>,
    truncated: bool,
    max_files: usize,
}

/// `GET /admin/agents/<agent>`: the agent's settings and the files in its directory that the
/// user may see.
async fn agent_page(
    State(pages): State<AgentPages>,
    user: WebUser,
    Path(agent): Path<String>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    let config = pages.manager.config().get();
    let Some(agent) = AgentId::new(&agent)
        .ok()
        .and_then(|id| config.agents.get(&id))
        .cloned()
    else {
        return error_page(
            StatusCode::NOT_FOUND,
            "No such agent",
            "There is no agent with that name.",
        );
    };
    let viewer = user.user.clone();
    let listed = {
        let agent = agent.clone();
        tokio::task::spawn_blocking(move || list_files(&agent, &viewer)).await
    };
    let (files, files_error, truncated) = match listed {
        Ok(Ok(listed)) => (
            listed.files.into_iter().map(FileView::new).collect(),
            None,
            listed.truncated,
        ),
        Ok(Err(err)) => (Vec::new(), Some(err.to_string()), false),
        Err(err) => {
            error!(agent = %agent.id, error = %err, "listing an agent's files failed");
            (Vec::new(), Some("listing them failed".to_owned()), false)
        }
    };
    let list = |items: &[String]| {
        if items.is_empty() {
            "none".to_owned()
        } else {
            items.join(", ")
        }
    };
    http::render(&AgentPage {
        id: agent.id.to_string(),
        label: agent_label(&config, &agent.id),
        models: models(&agent),
        tools: list(&agent.tools),
        ask: list(&agent.ask),
        timezone: agent
            .timezone
            .as_ref()
            .and_then(|tz| tz.iana_name())
            .map(str::to_owned),
        files,
        files_error,
        truncated,
        max_files: MAX_FILES,
    })
}

/// The create form's fields, as posted (and shown again after an error). A field the post leaves
/// out is empty, except `soul`, which is then [`DEFAULT_SOUL`]: the first `GET` of the form shows
/// it, and a post of only a name and models still gets it, while a `soul` the owner emptied
/// (posted, but empty) stays empty.
#[derive(Debug, Deserialize)]
#[serde(default)]
struct CreateForm {
    csrf: Option<String>,
    id: String,
    name: String,
    models: String,
    tools: String,
    ask: String,
    timezone: String,
    soul: String,
}

impl Default for CreateForm {
    fn default() -> Self {
        Self {
            csrf: None,
            id: String::new(),
            name: String::new(),
            models: String::new(),
            tools: String::new(),
            ask: String::new(),
            timezone: String::new(),
            soul: DEFAULT_SOUL.to_owned(),
        }
    }
}

#[derive(Template)]
#[template(path = "web/admin_agent_new.html")]
struct NewAgentPage<'a> {
    csrf: &'a str,
    form: &'a CreateForm,
    error: Option<String>,
    /// `<provider>/<model>` the providers list, offered as suggestions.
    known_models: Vec<String>,
}

async fn render_form(
    pages: &AgentPages,
    user: &WebUser,
    form: &CreateForm,
    error: Option<String>,
) -> Response {
    http::render(&NewAgentPage {
        csrf: &user.csrf,
        form,
        error,
        known_models: pages.manager.known_models().await,
    })
}

/// `GET /admin/agents/new`: the form.
async fn new_agent_page(State(pages): State<AgentPages>, user: WebUser) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    render_form(&pages, &user, &CreateForm::default(), None).await
}

/// `POST /admin/agents` (CSRF-checked): creates the agent, then 303 to its page. An empty id is
/// derived from the name ([`agent_id_from_name`]). An invalid form is shown again with the problem
/// (400, or 409 for an id that's taken), with the id that was used; nothing is written.
async fn create(
    State(pages): State<AgentPages>,
    user: WebUser,
    headers: HeaderMap,
    Form(mut form): Form<CreateForm>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    form.id = form.id.trim().to_owned();
    if form.id.is_empty() {
        form.id = agent_id_from_name(&form.name);
    }
    let agent = NewAgent {
        id: form.id.clone(),
        name: form.name.clone(),
        models: split_list(&form.models),
        tools: split_list(&form.tools),
        ask: split_list(&form.ask),
        timezone: form.timezone.clone(),
        // Browsers submit a textarea's line breaks as CRLF; the file keeps plain LF.
        soul: form.soul.replace("\r\n", "\n"),
    };
    match pages.manager.create(&agent).await {
        Ok(created) => Redirect::to(&format!("/admin/agents/{}", created.id)).into_response(),
        Err(err) if err.is_invalid_input() => {
            let status = match err {
                CreateError::Exists(_) => StatusCode::CONFLICT,
                _ => StatusCode::BAD_REQUEST,
            };
            let mut response = render_form(&pages, &user, &form, Some(err.form_message())).await;
            if response.status().is_success() {
                *response.status_mut() = status;
            }
            response
        }
        Err(err) => {
            error!(agent = %agent.id, error = %err, "creating an agent failed");
            error_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Agent not created",
                "Giles couldn't write the agent's files. The error is in its log.",
            )
        }
    }
}

/// The edit form's fields, as posted and as shown (again after an error). The lists are the text
/// the owner types, separated by commas.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct EditForm {
    csrf: Option<String>,
    name: String,
    models: String,
    tools: String,
    ask: String,
    timezone: String,
    soul: String,
    /// What the form was rendered from ([`AgentForm::version`]), posted back unchanged.
    version: String,
}

impl EditForm {
    /// The form for an agent as its files have it.
    fn of(agent: AgentForm) -> Self {
        let list = |items: &[String]| items.join(", ");
        Self {
            csrf: None,
            models: list(&agent.models),
            tools: list(&agent.tools),
            ask: list(&agent.ask),
            name: agent.name,
            timezone: agent.timezone,
            soul: agent.soul,
            version: agent.version,
        }
    }
}

#[derive(Template)]
#[template(path = "web/admin_agent_edit.html")]
struct EditAgentPage<'a> {
    csrf: &'a str,
    id: &'a str,
    label: String,
    form: &'a EditForm,
    error: Option<String>,
    /// The files changed since the form was opened: offer a reload.
    stale: bool,
    /// `<provider>/<model>` the providers list, offered as suggestions.
    known_models: Vec<String>,
}

async fn render_edit_form(
    pages: &AgentPages,
    user: &WebUser,
    id: &str,
    form: &EditForm,
    error: Option<String>,
    stale: bool,
) -> Response {
    let config = pages.manager.config().get();
    let label = AgentId::new(id)
        .map(|agent| agent_label(&config, &agent))
        .unwrap_or_else(|_| id.to_owned());
    http::render(&EditAgentPage {
        csrf: &user.csrf,
        id,
        label,
        form,
        error,
        stale,
        known_models: pages.manager.known_models().await,
    })
}

/// The page for an agent that can't be edited: gone, or its files can't be edited with the form.
fn edit_failed(error: &EditError, id: &str) -> Response {
    match error {
        EditError::NotFound(_) => error_page(
            StatusCode::NOT_FOUND,
            "No such agent",
            "There is no agent with that name.",
        ),
        EditError::Io { .. } | EditError::Task(_) => {
            error!(agent = %id, error = %error, "editing an agent failed");
            error_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Agent not changed",
                "Giles couldn't read or write the agent's files. The error is in its log.",
            )
        }
        other => error_page(
            StatusCode::CONFLICT,
            "Can't edit this agent",
            &other.form_message(),
        ),
    }
}

/// `GET /admin/agents/<agent>/edit`: the form, filled in from the agent's files.
async fn edit_page(
    State(pages): State<AgentPages>,
    user: WebUser,
    Path(agent): Path<String>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    match pages.manager.edit_form(&agent).await {
        Ok(form) => render_edit_form(&pages, &user, &agent, &EditForm::of(form), None, false).await,
        Err(err) => edit_failed(&err, &agent),
    }
}

/// `POST /admin/agents/<agent>/edit` (CSRF-checked): saves the edit, then 303 to the agent's page.
/// A form that can't be saved is shown again with the problem and what was typed (400; 409 if
/// the files changed since the form was opened), and nothing is written.
async fn edit(
    State(pages): State<AgentPages>,
    user: WebUser,
    Path(agent): Path<String>,
    headers: HeaderMap,
    Form(form): Form<EditForm>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let change = AgentEdit {
        id: agent.clone(),
        name: form.name.clone(),
        models: split_list(&form.models),
        tools: split_list(&form.tools),
        ask: split_list(&form.ask),
        timezone: form.timezone.clone(),
        // Browsers submit a textarea's line breaks as CRLF; the file keeps plain LF.
        soul: form.soul.replace("\r\n", "\n"),
        version: form.version.clone(),
    };
    match pages.manager.edit(&change).await {
        Ok(edited) => Redirect::to(&format!("/admin/agents/{}", edited.id)).into_response(),
        Err(err @ (EditError::NotFound(_) | EditError::Io { .. } | EditError::Task(_))) => {
            edit_failed(&err, &agent)
        }
        Err(err) => {
            let stale = matches!(err, EditError::Stale);
            let status = match err {
                EditError::Stale | EditError::Unsupported(_) => StatusCode::CONFLICT,
                _ => StatusCode::BAD_REQUEST,
            };
            let mut response = render_edit_form(
                &pages,
                &user,
                &agent,
                &form,
                Some(err.form_message()),
                stale,
            )
            .await;
            if response.status().is_success() {
                *response.status_mut() = status;
            }
            response
        }
    }
}

/// Where removed agents go, under the data root (shown on the pages).
const TRASH: &str = "trash/agents";

#[derive(Template)]
#[template(path = "web/admin_agent_remove.html")]
struct RemovePage<'a> {
    csrf: &'a str,
    id: String,
    label: String,
    routes: Vec<RouteLink>,
    runtime_jobs: usize,
    config_jobs: usize,
    busy: bool,
    trash: &'static str,
    error: Option<String>,
}

/// A route that keeps an agent from being removed.
struct RouteLink {
    id: String,
    position: usize,
    criteria: String,
}

#[derive(Template)]
#[template(path = "web/admin_agent_removed.html")]
struct RemovedPage {
    id: String,
    label: String,
    trash: String,
    runtime_jobs: usize,
    config_jobs: usize,
    jobs_error: Option<String>,
}

/// The confirmation page for `preview`'s agent, with `error` if a removal was just refused.
fn confirm_page(
    pages: &AgentPages,
    user: &WebUser,
    preview: RemovalPreview,
    error: Option<String>,
) -> Response {
    let config = pages.manager.config().get();
    http::render(&RemovePage {
        csrf: &user.csrf,
        label: agent_label(&config, &preview.id),
        id: preview.id.to_string(),
        routes: preview
            .routes
            .into_iter()
            .map(|route| RouteLink {
                id: route.id.to_string(),
                position: route.position,
                criteria: route.criteria,
            })
            .collect(),
        runtime_jobs: preview.runtime_jobs,
        config_jobs: preview.config_jobs,
        busy: preview.busy,
        trash: TRASH,
        error,
    })
}

/// The page for a removal that failed for a reason that isn't the owner's to fix.
fn remove_failed(error: &RemoveError, id: &str) -> Response {
    match error {
        RemoveError::NotFound(_) => error_page(
            StatusCode::NOT_FOUND,
            "No such agent",
            "There is no agent with that name.",
        ),
        other => {
            error!(agent = %id, error = %other, "removing an agent failed");
            error_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Agent not removed",
                "Giles couldn't move the agent's files or read what it needs. Nothing was changed. The error is in its log.",
            )
        }
    }
}

/// `GET /admin/agents/<agent>/remove`: what removing the agent would do, and the button.
async fn remove_page(
    State(pages): State<AgentPages>,
    user: WebUser,
    Path(agent): Path<String>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    match pages.manager.removal_preview(&agent).await {
        Ok(preview) => confirm_page(&pages, &user, preview, None),
        Err(err) => remove_failed(&err, &agent),
    }
}

/// `POST /admin/agents/<agent>/remove` (CSRF-checked): removes the agent and says what happened.
/// A removal that is refused (routes name the agent, or it's busy) shows the confirmation page
/// again with why (409), and nothing changes.
async fn remove(
    State(pages): State<AgentPages>,
    user: WebUser,
    Path(agent): Path<String>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let label = AgentId::new(&agent)
        .map(|id| agent_label(&pages.manager.config().get(), &id))
        .unwrap_or_else(|_| agent.clone());
    match pages.manager.remove(&agent).await {
        Ok(Removed {
            trash,
            runtime_jobs,
            config_jobs,
            jobs_error,
        }) => http::render(&RemovedPage {
            id: agent,
            label,
            trash: trash.display().to_string(),
            runtime_jobs,
            config_jobs,
            jobs_error,
        }),
        Err(err) if err.is_blocked() && !matches!(err, RemoveError::NotFound(_)) => {
            let message = err.to_string();
            match pages.manager.removal_preview(&agent).await {
                Ok(preview) => {
                    let mut response = confirm_page(&pages, &user, preview, Some(message));
                    if response.status().is_success() {
                        *response.status_mut() = StatusCode::CONFLICT;
                    }
                    response
                }
                Err(err) => remove_failed(&err, &agent),
            }
        }
        Err(err) => remove_failed(&err, &agent),
    }
}

/// A form with nothing but the CSRF token.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CsrfForm {
    csrf: Option<String>,
}

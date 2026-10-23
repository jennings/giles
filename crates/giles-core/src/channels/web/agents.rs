//! The agent pages (TASKS.md 5.1): every agent, one agent's configuration and files, and the form
//! that creates an agent while the core runs. Next to the admin pages (`admin.rs`) and like them:
//! behind the login, the owner only, every answer `no-store`, the form post CSRF-checked.
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
use axum::routing::get;
use giles_types::AgentId;
use serde::Deserialize;
use tracing::error;

use super::admin::{forbidden, may_administer};
use super::auth::{WebUser, check_csrf};
use super::pages::{WebApp, agent_label, error_page, no_store};
use crate::agents::{
    AgentCreator, AgentFile, Content, CreateError, MAX_FILE_BYTES, MAX_FILES, MAX_TOTAL_BYTES,
    NewAgent, list_files, split_list,
};
use crate::config::AgentConfig;
use crate::http::{self, RouterBuilder};

/// The agent pages' state.
#[derive(Clone)]
struct AgentPages {
    creator: Arc<AgentCreator>,
}

impl WebApp {
    /// Adds the agent pages to `builder`, creating agents with `creator` (whose running
    /// configuration should be this app's, so the chat list offers a new agent at once), behind
    /// the same authentication middleware as the chat pages. Every answer on these routes is
    /// `no-store`.
    pub fn install_agents(
        &self,
        builder: RouterBuilder,
        creator: Arc<AgentCreator>,
    ) -> RouterBuilder {
        let auth = from_fn_with_state(self.clone(), super::login::authenticate);
        builder.merge(
            Router::new()
                .route("/admin/agents", get(agents_page).post(create))
                .route("/admin/agents/new", get(new_agent_page))
                .route("/admin/agents/{agent}", get(agent_page))
                .with_state(AgentPages { creator })
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
    let config = pages.creator.config().get();
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
    let config = pages.creator.config().get();
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

/// The create form's fields, as posted (and shown again after an error).
#[derive(Debug, Default, Deserialize)]
struct CreateForm {
    #[serde(default)]
    csrf: Option<String>,
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    models: String,
    #[serde(default)]
    tools: String,
    #[serde(default)]
    ask: String,
    #[serde(default)]
    timezone: String,
    #[serde(default)]
    soul: String,
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
        known_models: pages.creator.known_models().await,
    })
}

/// `GET /admin/agents/new`: the form.
async fn new_agent_page(State(pages): State<AgentPages>, user: WebUser) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    render_form(&pages, &user, &CreateForm::default(), None).await
}

/// `POST /admin/agents` (CSRF-checked): creates the agent, then 303 to its page. An invalid form
/// is shown again with the problem (400, or 409 for an id that's taken); nothing is written.
async fn create(
    State(pages): State<AgentPages>,
    user: WebUser,
    headers: HeaderMap,
    Form(form): Form<CreateForm>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let agent = NewAgent {
        id: form.id.trim().to_owned(),
        name: form.name.clone(),
        models: split_list(&form.models),
        tools: split_list(&form.tools),
        ask: split_list(&form.ask),
        timezone: form.timezone.clone(),
        soul: form.soul.clone(),
    };
    match pages.creator.create(&agent).await {
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

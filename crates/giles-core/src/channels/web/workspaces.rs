//! The workspaces page: every tracked environment workspace with what its plugin says now, and
//! the buttons that stop and delete a workspace. Next to the admin pages (`admin.rs`), the agent
//! pages (`agents.rs`), the route pages (`routes.rs`), and the toolset pages (`toolsets.rs`), and
//! like them: behind the login, the owner only, every answer `no-store`, every change a
//! CSRF-checked form post.
//!
//! The data comes from [`Workspaces::overview`], which asks each plugin at the same time. What a
//! plugin reports about a workspace (its description and its details) is the plugin's text: the
//! templates escape it, and nothing here interprets it.
//!
//! A workspace a running turn is using can be stopped or deleted: the pages say so and ask for
//! confirmation first. A workspace nothing is using is stopped by its button directly, and deleted
//! after the plain confirmation.

use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{from_fn_with_state, map_response};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use giles_store::WorkspaceOwner;
use giles_types::{WorkspaceDetail, WorkspaceId, WorkspaceState as PluginState};
use serde::Deserialize;
use tracing::error;

use super::admin::{forbidden, may_administer};
use super::auth::{WebUser, check_csrf};
use super::is_chat_id;
use super::pages::{WebApp, agent_label, error_page, no_store};
use super::toolsets::when;
use crate::config::Config;
use crate::http::{self, RouterBuilder};
use crate::workspaces::{Live, ManageError, WorkspaceState, WorkspaceView, Workspaces};

/// The workspaces pages' state.
#[derive(Clone)]
struct WorkspacePages {
    workspaces: Workspaces,
    app: WebApp,
}

impl WebApp {
    /// Adds the workspaces page to `builder`, listing and changing workspaces through
    /// `workspaces`, behind the same authentication middleware as the chat pages. Every answer on
    /// these routes is `no-store`.
    pub fn install_workspaces(
        &self,
        builder: RouterBuilder,
        workspaces: Workspaces,
    ) -> RouterBuilder {
        let auth = from_fn_with_state(self.clone(), super::login::authenticate);
        builder.merge(
            Router::new()
                .route("/admin/workspaces", get(list_page))
                .route("/admin/workspaces/{id}/stop", get(stop_page).post(stop))
                .route(
                    "/admin/workspaces/{id}/delete",
                    get(delete_page).post(delete),
                )
                .with_state(WorkspacePages {
                    workspaces,
                    app: self.clone(),
                })
                .route_layer(auth)
                .route_layer(map_response(async |response: Response| no_store(response))),
        )
    }
}

/// A size as the largest binary unit that keeps it above 1: `0 B`, `1.5 MiB`, `12 GiB`.
fn bytes_text(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 || (value - value.round()).abs() < 0.05 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Who a workspace belongs to, as the page shows it.
struct OwnerCell {
    /// For a conversation: the agent's id, its label, and whether it is configured.
    agent: Option<(String, String, bool)>,
    channel: String,
    conversation: String,
    /// The web chat, if the conversation is one of this instance's.
    chat: Option<String>,
    /// For a job run: the job session.
    job: Option<i64>,
}

impl OwnerCell {
    fn new(owner: &WorkspaceOwner, config: &Config, web_instance: &str) -> Self {
        match owner {
            WorkspaceOwner::Conversation {
                agent,
                channel,
                conversation,
            } => Self {
                agent: Some((
                    agent.to_string(),
                    agent_label(config, agent),
                    config.agents.contains_key(agent),
                )),
                channel: channel.clone(),
                conversation: conversation.to_string(),
                chat: (channel == web_instance && is_chat_id(conversation.as_str()))
                    .then(|| format!("/chats/{conversation}")),
                job: None,
            },
            WorkspaceOwner::Job { session } => Self {
                agent: None,
                channel: String::new(),
                conversation: String::new(),
                chat: None,
                job: Some(session.get()),
            },
        }
    }
}

/// A tracked workspace in the list.
struct WorkspaceRow {
    id: String,
    environment: String,
    owner: OwnerCell,
    created: String,
    last_used: String,
    in_use: bool,
    state: &'static str,
    state_class: &'static str,
    disk: String,
    /// What the plugin reported now, or failing that, when the workspace was created.
    description: String,
    working_directory: String,
    /// The plugin's details; `None` while the plugin's answer is unknown.
    details: Option<Vec<WorkspaceDetail>>,
    /// Whether Stop and Delete apply (`active`).
    can_act: bool,
}

fn workspace_row(view: WorkspaceView, config: &Config, web_instance: &str) -> WorkspaceRow {
    let record = view.record;
    let (state, state_class) = match (&record.state, &view.live) {
        (WorkspaceState::Creating, _) => ("being created", "warn"),
        (_, Live::Known(w)) if w.state == PluginState::Running => ("running", "ok"),
        (_, Live::Known(_)) => ("stopped", "warn"),
        (_, Live::Missing) => ("missing from its environment", "bad"),
        (_, Live::Unknown) => ("unknown", "warn"),
    };
    let (disk, details, description, working_directory) = match view.live {
        Live::Known(w) => (
            w.disk_used_bytes
                .map_or_else(|| "unknown".to_owned(), bytes_text),
            Some(w.details),
            w.description,
            w.working_directory,
        ),
        Live::Missing => (
            "unknown".to_owned(),
            Some(Vec::new()),
            String::new(),
            String::new(),
        ),
        Live::Unknown => ("unknown".to_owned(), None, String::new(), String::new()),
    };
    // Without the plugin's answer, the page shows what it said when the workspace was created.
    let description = if description.is_empty() {
        record.description.clone().unwrap_or_default()
    } else {
        description
    };
    let working_directory = if working_directory.is_empty() {
        record.working_directory.clone().unwrap_or_default()
    } else {
        working_directory
    };
    WorkspaceRow {
        id: record.id.to_string(),
        environment: record.environment.clone(),
        owner: OwnerCell::new(&record.owner, config, web_instance),
        created: when(record.created_at),
        last_used: when(record.last_used_at),
        in_use: view.in_use,
        state,
        state_class,
        disk,
        description,
        working_directory,
        details,
        can_act: record.state == WorkspaceState::Active,
    }
}

/// An environment's line at the top of the page.
struct EnvironmentRow {
    name: String,
    free: Option<String>,
    problem: Option<String>,
}

#[derive(Template)]
#[template(path = "web/admin_workspaces.html")]
struct WorkspacesPage<'a> {
    csrf: &'a str,
    environments: Vec<EnvironmentRow>,
    workspaces: Vec<WorkspaceRow>,
}

/// The page for a failed read or change: 404 for a workspace that is gone, 409 for what the
/// operator can act on (still being created, an environment that can't be reached), 502 for a
/// plugin that failed the call, and a logged 500 for anything else.
fn failed(err: &ManageError, title: &str) -> Response {
    match err {
        ManageError::NotFound => {
            error_page(StatusCode::NOT_FOUND, "No such workspace", &err.to_string())
        }
        ManageError::StillCreating
        | ManageError::UnknownEnvironment { .. }
        | ManageError::Unreachable { .. } => {
            error_page(StatusCode::CONFLICT, title, &err.to_string())
        }
        ManageError::Environment(_) => error_page(StatusCode::BAD_GATEWAY, title, &err.to_string()),
        err => {
            error!(error = %err, "using the workspaces failed");
            error_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Something went wrong",
                "Giles couldn't use its workspaces. The error is in its log.",
            )
        }
    }
}

/// `GET /admin/workspaces`: every tracked workspace with the plugin's live view, and each
/// environment's free space or why it is unknown.
async fn list_page(State(pages): State<WorkspacePages>, user: WebUser) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    let overview = match pages.workspaces.overview().await {
        Ok(overview) => overview,
        Err(err) => return failed(&err, "Workspaces unavailable"),
    };
    let config = pages.app.inner.agents.get();
    let web = pages.app.inner.channel.instance().as_str();
    http::render(&WorkspacesPage {
        csrf: &user.csrf,
        environments: overview
            .environments
            .into_iter()
            .map(|e| EnvironmentRow {
                name: e.name,
                free: e.free_bytes.map(bytes_text),
                problem: e.problem,
            })
            .collect(),
        workspaces: overview
            .workspaces
            .into_iter()
            .map(|view| workspace_row(view, &config, web))
            .collect(),
    })
}

/// The workspace id in a path, if it is one.
fn workspace_id(id: &str) -> Option<WorkspaceId> {
    WorkspaceId::new(id).ok()
}

fn no_such_workspace() -> Response {
    failed(&ManageError::NotFound, "No such workspace")
}

/// The fields of the one-button forms.
#[derive(Debug, Default, Deserialize)]
struct ActionForm {
    #[serde(default)]
    csrf: Option<String>,
}

#[derive(Template)]
#[template(path = "web/admin_workspace_stop.html")]
struct StopPage<'a> {
    csrf: &'a str,
    id: String,
}

/// `GET /admin/workspaces/<id>/stop`: the confirmation for a workspace a running turn is using,
/// which says what stopping ends. A workspace nothing is using is stopped by its button, so this
/// goes straight back to the list.
async fn stop_page(
    State(pages): State<WorkspacePages>,
    user: WebUser,
    Path(id): Path<String>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    let Some(id) = workspace_id(&id) else {
        return no_such_workspace();
    };
    if let Err(err) = pages.workspaces.managed_workspace(&id).await {
        return failed(&err, "Workspace not stopped");
    }
    if !pages.workspaces.in_use(&id) {
        return Redirect::to(&format!("/admin/workspaces#workspace-{id}")).into_response();
    }
    http::render(&StopPage {
        csrf: &user.csrf,
        id: id.to_string(),
    })
}

/// `POST /admin/workspaces/<id>/stop` (CSRF-checked): has the plugin stop the workspace, then 303
/// to the list. Nothing else changes: the next workspace call starts it again. If a running turn
/// is using it, the turn's command and background processes end.
async fn stop(
    State(pages): State<WorkspacePages>,
    user: WebUser,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<ActionForm>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let Some(id) = workspace_id(&id) else {
        return no_such_workspace();
    };
    match pages.workspaces.stop_by_id(&id).await {
        Ok(_) => Redirect::to(&format!("/admin/workspaces#workspace-{id}")).into_response(),
        Err(err) => failed(&err, "Workspace not stopped"),
    }
}

#[derive(Template)]
#[template(path = "web/admin_workspace_delete.html")]
struct DeletePage<'a> {
    csrf: &'a str,
    id: String,
    environment: String,
    owner: OwnerCell,
    description: String,
    /// A running turn is using the workspace.
    in_use: bool,
}

/// `GET /admin/workspaces/<id>/delete`: what deleting does, with the warning that it stops a
/// running turn's command if one is using the workspace, and the button.
async fn delete_page(
    State(pages): State<WorkspacePages>,
    user: WebUser,
    Path(id): Path<String>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    let Some(id) = workspace_id(&id) else {
        return no_such_workspace();
    };
    let record = match pages.workspaces.managed_workspace(&id).await {
        Ok(record) => record,
        Err(err) => return failed(&err, "Workspace not deleted"),
    };
    let config = pages.app.inner.agents.get();
    let web = pages.app.inner.channel.instance().as_str();
    http::render(&DeletePage {
        csrf: &user.csrf,
        in_use: pages.workspaces.in_use(&record.id),
        id: record.id.to_string(),
        environment: record.environment,
        owner: OwnerCell::new(&record.owner, &config, web),
        description: record.description.unwrap_or_default(),
    })
}

/// `POST /admin/workspaces/<id>/delete` (CSRF-checked): deletes the workspace for an operator,
/// then 303 to the list. If a running turn is using it, its command is stopped first and the
/// turn's next workspace call is told an operator deleted the workspace. If the plugin can't be
/// told, the workspace is gone for its owner at once and the plugin is asked again later.
async fn delete(
    State(pages): State<WorkspacePages>,
    user: WebUser,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<ActionForm>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let Some(id) = workspace_id(&id) else {
        return no_such_workspace();
    };
    match pages.workspaces.delete_by_id(&id).await {
        Ok(_) => Redirect::to("/admin/workspaces").into_response(),
        Err(err) => failed(&err, "Workspace not deleted"),
    }
}

#[cfg(test)]
mod tests {
    use super::bytes_text;

    #[test]
    fn sizes_are_shown_in_binary_units() {
        assert_eq!(bytes_text(0), "0 B");
        assert_eq!(bytes_text(1023), "1023 B");
        assert_eq!(bytes_text(1024), "1 KiB");
        assert_eq!(bytes_text(1536), "1.5 KiB");
        assert_eq!(bytes_text(5 * 1024 * 1024), "5 MiB");
        assert_eq!(bytes_text(12 * 1024 * 1024 * 1024 + 1), "12 GiB");
        assert_eq!(bytes_text(u64::MAX), "16384 PiB");
    }
}

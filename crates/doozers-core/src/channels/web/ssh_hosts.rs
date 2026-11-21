//! The Known SSH hosts page, `/admin/ssh-hosts`: the host keys workspaces trust, per environment
//! instance, with an admin's Add (pin a key before any workspace connects) and Delete (forget a
//! key, so the next connection trusts whatever the host then presents). The service behind it is
//! [`KnownHosts`]; the design is in `Docs/technical/ssh.md`.
//!
//! Like the other admin pages: behind the login, admins only, every answer `no-store`, every
//! change a CSRF-checked form post, and every form working without JavaScript. Deleting asks first.
//! A pinned line that the service refuses shows on the page again with the reason and the pasted
//! text, 400 for a line that isn't usable and 409 for a key that is already known.

use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{from_fn_with_state, map_response};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use doozers_store::KnownHostId;
use serde::Deserialize;
use tracing::error;

use super::admin::{forbidden, may_administer};
use super::auth::{WebUser, check_csrf};
use super::pages::{WebApp, error_page, no_store};
use super::shell::{Nav, Shell};
use super::toolsets::when;
use crate::http::{self, RouterBuilder};
use crate::ssh::{KnownHost, KnownHosts, KnownHostsError};

/// The most keys a `?pinned=` notice names: a line pins one key for each host it lists.
const MAX_NOTICE_IDS: usize = 20;

#[derive(Clone)]
struct HostPages {
    hosts: KnownHosts,
    app: WebApp,
}

impl WebApp {
    /// Adds the Known SSH hosts page to `builder`, listing and changing the keys through `hosts`,
    /// behind the same authentication middleware as the chat pages. Every answer is `no-store`.
    pub fn install_ssh_hosts(&self, builder: RouterBuilder, hosts: KnownHosts) -> RouterBuilder {
        let auth = from_fn_with_state(self.clone(), super::login::authenticate);
        builder.merge(
            Router::new()
                .route("/admin/ssh-hosts", get(list_page))
                .route("/admin/ssh-hosts/add", post(add))
                .route(
                    "/admin/ssh-hosts/{id}/delete",
                    get(delete_page).post(delete),
                )
                .with_state(HostPages {
                    hosts,
                    app: self.clone(),
                })
                .route_layer(auth)
                .route_layer(map_response(async |response: Response| no_store(response))),
        )
    }
}

/// A host as `known_hosts` and `ssh` write it: the host alone on port 22, else `[host]:port`.
fn host_label(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_owned()
    } else {
        format!("[{host}]:{port}")
    }
}

/// A key on the page.
struct HostRow {
    id: i64,
    /// [`host_label`].
    host: String,
    key_type: String,
    fingerprint: String,
    first_seen: String,
    /// Who or what recorded the key: `pinned by <admin>`, or `by workspace W, agent A, user U`
    /// with the parts it has.
    origin: String,
    last_used: String,
}

impl From<KnownHost> for HostRow {
    fn from(key: KnownHost) -> Self {
        let origin = match &key.added_by {
            Some(admin) => format!("pinned by {admin}"),
            None => {
                let parts: Vec<String> = [
                    ("workspace", &key.first_workspace),
                    ("agent", &key.first_agent),
                    ("user", &key.first_user),
                ]
                .into_iter()
                .filter_map(|(what, value)| Some(format!("{what} {}", value.as_ref()?)))
                .collect();
                if parts.is_empty() {
                    "recorded on first use".to_owned()
                } else {
                    format!("by {}", parts.join(", "))
                }
            }
        };
        Self {
            id: key.id.get(),
            host: host_label(&key.host, key.port),
            key_type: key.key_type,
            fingerprint: key.fingerprint,
            first_seen: when(key.first_seen_at),
            origin,
            last_used: key.last_used_at.map_or_else(|| "never".to_owned(), when),
        }
    }
}

/// The keys of one environment instance.
struct Group {
    environment: String,
    keys: Vec<HostRow>,
}

/// What the page shows besides the keys.
#[derive(Default)]
struct ListState {
    notice: Option<String>,
    error: Option<String>,
    environment: String,
    line: String,
}

#[derive(Template)]
#[template(path = "web/admin_ssh_hosts.html")]
struct ListPage<'a> {
    shell: Shell,
    csrf: &'a str,
    groups: Vec<Group>,
    /// The configured environment instances: the Add form's choices. The form is left out when
    /// there are none.
    environments: Vec<String>,
    notice: Option<String>,
    error: Option<String>,
    /// The environment the Add form has selected.
    environment: String,
    /// The pasted line.
    line: String,
}

/// `?notice=` and `?pinned=`, after a change.
#[derive(Deserialize)]
struct Notice {
    notice: Option<String>,
    /// The ids of the keys just pinned, comma-separated.
    pinned: Option<String>,
}

/// The page for a call that failed for a reason the operator can't fix by editing the form.
fn failed(err: &KnownHostsError) -> Response {
    error!(error = %err, "using the known SSH hosts failed");
    error_page(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Something went wrong",
        "Doozers couldn't use its known SSH hosts. The error is in its log.",
    )
}

fn no_such_host() -> Response {
    error_page(
        StatusCode::NOT_FOUND,
        "No such known SSH host",
        "There is no known SSH host key with that id. It may have been deleted.",
    )
}

/// The key id in a path, if it is one.
fn host_id(id: &str) -> Option<KnownHostId> {
    id.parse().ok().map(KnownHostId::new)
}

/// The sentence for what the page redirected with: a fixed text for `?notice=`, and for
/// `?pinned=` the keys it names, read from the store (an id that isn't a key is skipped).
async fn notice_text(
    hosts: &KnownHosts,
    notice: &Notice,
) -> Result<Option<String>, KnownHostsError> {
    if let Some("deleted") = notice.notice.as_deref() {
        return Ok(Some("Deleted the host key.".to_owned()));
    }
    let Some(pinned) = &notice.pinned else {
        return Ok(None);
    };
    let mut keys = Vec::new();
    for id in pinned.split(',').filter_map(host_id).take(MAX_NOTICE_IDS) {
        keys.extend(hosts.get(id).await?);
    }
    let Some(first) = keys.first() else {
        return Ok(None);
    };
    let names: Vec<String> = keys.iter().map(|k| host_label(&k.host, k.port)).collect();
    Ok(Some(format!(
        "Pinned the {} key for {} in {}.",
        first.key_type,
        names.join(", "),
        first.environment
    )))
}

/// The names of the configured environment instances.
fn environment_names(pages: &HostPages) -> Vec<String> {
    pages
        .app
        .inner
        .agents
        .get()
        .environments
        .keys()
        .map(|name| name.as_str().to_owned())
        .collect()
}

/// Renders the page for `user` with `state`, answering `status`.
async fn render_list(
    pages: &HostPages,
    user: &WebUser,
    status: StatusCode,
    state: ListState,
) -> Response {
    let keys = match pages.hosts.all().await {
        Ok(keys) => keys,
        Err(err) => return failed(&err),
    };
    let mut groups: Vec<Group> = Vec::new();
    for key in keys {
        if groups
            .last()
            .is_none_or(|g| g.environment != key.environment)
        {
            groups.push(Group {
                environment: key.environment.clone(),
                keys: Vec::new(),
            });
        }
        if let Some(group) = groups.last_mut() {
            group.keys.push(key.into());
        }
    }
    let environments = environment_names(pages);
    let environment = if state.environment.is_empty() {
        environments.first().cloned().unwrap_or_default()
    } else {
        state.environment
    };
    let mut response = http::render(&ListPage {
        shell: Shell::new(user, Nav::AdminSshHosts),
        csrf: &user.csrf,
        groups,
        environments,
        notice: state.notice,
        error: state.error,
        environment,
        line: state.line,
    });
    if response.status().is_success() {
        *response.status_mut() = status;
    }
    response
}

/// `GET /admin/ssh-hosts`: the keys by environment, each with Delete, and the Add form. Admins
/// only.
async fn list_page(
    State(pages): State<HostPages>,
    user: WebUser,
    Query(notice): Query<Notice>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    let notice = match notice_text(&pages.hosts, &notice).await {
        Ok(notice) => notice,
        Err(err) => return failed(&err),
    };
    let state = ListState {
        notice,
        ..ListState::default()
    };
    render_list(&pages, &user, StatusCode::OK, state).await
}

/// The fields of the Add form.
#[derive(Debug, Default, Deserialize)]
struct AddForm {
    #[serde(default)]
    csrf: Option<String>,
    #[serde(default)]
    environment: String,
    #[serde(default)]
    line: String,
}

/// `POST /admin/ssh-hosts/add` (CSRF-checked): pins the key of the pasted line for the
/// environment, then 303 to the page. A line that can't be pinned shows the page again with the
/// reason and the pasted text. Admins only.
async fn add(
    State(pages): State<HostPages>,
    user: WebUser,
    headers: HeaderMap,
    Form(form): Form<AddForm>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let refused = |status, message: String| {
        let state = ListState {
            error: Some(message),
            environment: form.environment.clone(),
            line: form.line.clone(),
            ..ListState::default()
        };
        render_list(&pages, &user, status, state)
    };
    if !environment_names(&pages).contains(&form.environment) {
        let message = if form.environment.is_empty() {
            "Choose an environment.".to_owned()
        } else {
            format!("There is no environment named {:?}.", form.environment)
        };
        return refused(StatusCode::BAD_REQUEST, message).await;
    }
    match pages
        .hosts
        .pin(&user.user, &form.environment, &form.line)
        .await
    {
        Ok(added) => {
            let ids: Vec<String> = added.iter().map(|k| k.id.get().to_string()).collect();
            Redirect::to(&format!("/admin/ssh-hosts?pinned={}", ids.join(","))).into_response()
        }
        Err(KnownHostsError::Invalid(message)) => {
            refused(
                StatusCode::BAD_REQUEST,
                format!("Couldn't pin that line: {message}."),
            )
            .await
        }
        Err(KnownHostsError::AlreadyKnown) => {
            refused(
                StatusCode::CONFLICT,
                "That key is already known for the host.".to_owned(),
            )
            .await
        }
        Err(err) => failed(&err),
    }
}

#[derive(Template)]
#[template(path = "web/admin_ssh_host_delete.html")]
struct DeletePage<'a> {
    shell: Shell,
    csrf: &'a str,
    key: HostRow,
    environment: String,
}

/// `GET /admin/ssh-hosts/<id>/delete`: what deleting does, and the button. Admins only.
async fn delete_page(
    State(pages): State<HostPages>,
    user: WebUser,
    Path(id): Path<String>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    let Some(id) = host_id(&id) else {
        return no_such_host();
    };
    match pages.hosts.get(id).await {
        Ok(Some(key)) => {
            let environment = key.environment.clone();
            http::render(&DeletePage {
                shell: Shell::new(&user, Nav::AdminSshHosts),
                csrf: &user.csrf,
                key: key.into(),
                environment,
            })
        }
        Ok(None) => no_such_host(),
        Err(err) => failed(&err),
    }
}

/// The fields of the one-button forms.
#[derive(Debug, Default, Deserialize)]
struct ActionForm {
    #[serde(default)]
    csrf: Option<String>,
}

/// `POST /admin/ssh-hosts/<id>/delete` (CSRF-checked): deletes the key, then 303 to the page. Admins
/// only.
async fn delete(
    State(pages): State<HostPages>,
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
    let Some(id) = host_id(&id) else {
        return no_such_host();
    };
    match pages.hosts.delete(id).await {
        Ok(true) => Redirect::to("/admin/ssh-hosts?notice=deleted").into_response(),
        Ok(false) => no_such_host(),
        Err(err) => failed(&err),
    }
}

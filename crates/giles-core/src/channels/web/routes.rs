//! The route pages: the ordered routes, and the forms that add, edit, remove, and
//! move them while the core runs. Next to the admin pages (`admin.rs`) and the agent pages
//! (`agents.rs`) and like them: behind the login, the owner only, every answer `no-store`, every
//! change a CSRF-checked form post. A change is made by [`Routes`], which validates it, stores it,
//! and gives the router the new list, so it applies to the next inbound message.
//!
//! Nothing here is secret: a route holds a channel instance name, a conversation id, and an agent
//! id, all escaped by the templates.

use std::sync::Arc;

use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{from_fn_with_state, map_response};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use giles_store::RouteId;
use serde::Deserialize;
use tracing::error;

use super::admin::{forbidden, may_administer};
use super::auth::{WebUser, check_csrf};
use super::pages::{WebApp, agent_label, error_page, no_store};
use crate::config::Config;
use crate::http::{self, RouterBuilder};
use crate::routes::{RouteError, RouteSpec, RouteView, Routes};

/// The route pages' state.
#[derive(Clone)]
struct RoutePages {
    routes: Arc<Routes>,
}

impl WebApp {
    /// Adds the route pages to `builder`, changing `routes` (the core's, whose list the router
    /// reads), behind the same authentication middleware as the chat pages. Every answer on these
    /// routes is `no-store`.
    pub fn install_routes(&self, builder: RouterBuilder, routes: Arc<Routes>) -> RouterBuilder {
        let auth = from_fn_with_state(self.clone(), super::login::authenticate);
        builder.merge(
            Router::new()
                .route("/admin/routes", get(routes_page).post(create))
                .route("/admin/routes/new", get(new_route_page))
                .route("/admin/routes/{id}", post(update))
                .route("/admin/routes/{id}/edit", get(edit_route_page))
                .route("/admin/routes/{id}/remove", post(remove))
                .route("/admin/routes/{id}/move", post(move_route))
                .with_state(RoutePages { routes })
                .route_layer(auth)
                .route_layer(map_response(async |response: Response| no_store(response))),
        )
    }
}

/// A route in the list.
struct RouteRow {
    id: i64,
    position: usize,
    channel: Option<String>,
    kind: Option<&'static str>,
    conversation: Option<String>,
    agent_id: String,
    agent_label: String,
    agent_exists: bool,
    problems: Vec<String>,
    first: bool,
    last: bool,
}

#[derive(Template)]
#[template(path = "web/admin_routes.html")]
struct RoutesPage<'a> {
    csrf: &'a str,
    routes: Vec<RouteRow>,
    /// How many `[[routes]]` `giles.toml` still has.
    legacy: usize,
}

fn rows(views: Vec<RouteView>, config: &Config) -> Vec<RouteRow> {
    let last = views.len().saturating_sub(1);
    views
        .into_iter()
        .enumerate()
        .map(|(i, view)| RouteRow {
            id: view.id.get(),
            position: view.position,
            channel: view.route.channel.as_ref().map(ToString::to_string),
            kind: view.route.kind.map(crate::routes::kind_name),
            conversation: view.route.conversation.clone(),
            agent_label: agent_label(config, &view.route.agent),
            agent_exists: config.agents.contains_key(&view.route.agent),
            agent_id: view.route.agent.to_string(),
            problems: view.problems,
            first: i == 0,
            last: i == last,
        })
        .collect()
}

/// The answer to a failed change, or read: the message for what the owner can fix, a logged 500
/// for anything else.
fn failed(err: &RouteError, title: &str) -> Response {
    match err {
        RouteError::NotFound => error_page(
            StatusCode::NOT_FOUND,
            "No such route",
            "That route no longer exists. It may have been removed on another page. Reload the list.",
        ),
        err if err.is_invalid_input() => error_page(StatusCode::CONFLICT, title, &err.to_string()),
        err => {
            error!(error = %err, "changing the routes failed");
            error_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Something went wrong",
                "Giles couldn't use its routes. The error is in its log.",
            )
        }
    }
}

/// `GET /admin/routes`: every route in order with its actions, and what's wrong with any of them.
async fn routes_page(State(pages): State<RoutePages>, user: WebUser) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    let views = match pages.routes.view().await {
        Ok(views) => views,
        Err(err) => return failed(&err, "Routes unavailable"),
    };
    let config = pages.routes.config().get();
    http::render(&RoutesPage {
        csrf: &user.csrf,
        routes: rows(views, &config),
        legacy: config.legacy_routes.len(),
    })
}

/// One `<option>`.
struct Choice {
    value: String,
    label: String,
    selected: bool,
}

#[derive(Template)]
#[template(path = "web/admin_route_form.html")]
struct RouteFormPage<'a> {
    csrf: &'a str,
    title: &'a str,
    action: String,
    submit: &'a str,
    error: Option<String>,
    conversation: &'a str,
    channels: Vec<Choice>,
    kinds: Vec<Choice>,
    agents: Vec<Choice>,
    /// Where to put a new route; empty when editing.
    befores: Vec<Choice>,
}

/// The form's fields, as posted (and shown again after an error).
#[derive(Debug, Default, Deserialize)]
struct RouteForm {
    #[serde(default)]
    csrf: Option<String>,
    #[serde(default)]
    channel: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    conversation: String,
    #[serde(default)]
    agent: String,
    /// The route to insert before, by id; empty for the end.
    #[serde(default)]
    before: String,
}

impl RouteForm {
    fn spec(&self) -> RouteSpec {
        RouteSpec {
            channel: self.channel.clone(),
            kind: self.kind.clone(),
            conversation: self.conversation.clone(),
            agent: self.agent.clone(),
        }
    }
}

/// `value` among `known`, plus `value` itself if it isn't there (a route whose channel instance or
/// agent is gone keeps showing it), as the selected one.
fn choices(known: Vec<(String, String)>, value: &str, extra_label: &str) -> Vec<Choice> {
    let mut out: Vec<Choice> = known
        .into_iter()
        .map(|(v, label)| Choice {
            selected: v == value,
            value: v,
            label,
        })
        .collect();
    if !value.is_empty() && !out.iter().any(|c| c.value == value) {
        out.push(Choice {
            value: value.to_owned(),
            label: format!("{value} ({extra_label})"),
            selected: true,
        });
    }
    out
}

/// The form for `form`. `editing` is the route being edited, if any; otherwise the form adds one,
/// and offers positions.
async fn render_form(
    pages: &RoutePages,
    user: &WebUser,
    form: &RouteForm,
    editing: Option<RouteId>,
    error: Option<String>,
) -> Response {
    let config = pages.routes.config().get();
    let mut channels = vec![("".to_owned(), "Any channel".to_owned())];
    channels.extend(
        config
            .channels
            .keys()
            .map(|c| (c.to_string(), c.to_string())),
    );
    let kinds = vec![
        ("".to_owned(), "Any kind".to_owned()),
        ("direct".to_owned(), "Direct".to_owned()),
        ("group".to_owned(), "Group".to_owned()),
    ];
    let mut agents = Vec::new();
    if form.agent.is_empty() {
        agents.push((String::new(), "Choose an agent".to_owned()));
    }
    agents.extend(
        config
            .agents
            .keys()
            .map(|id| (id.to_string(), agent_label(&config, id))),
    );
    let befores = if editing.is_some() {
        Vec::new()
    } else {
        let mut befores = vec![Choice {
            value: String::new(),
            label: "At the end, after every route".to_owned(),
            selected: form.before.is_empty(),
        }];
        match pages.routes.view().await {
            Ok(views) => befores.extend(views.iter().map(|view| Choice {
                value: view.id.to_string(),
                label: format!("Before route {} ({})", view.position, view.route.criteria()),
                selected: view.id.to_string() == form.before,
            })),
            Err(err) => return failed(&err, "Routes unavailable"),
        }
        befores
    };
    let (title, action, submit) = match editing {
        Some(id) => ("Edit route", format!("/admin/routes/{id}"), "Save"),
        None => ("New route", "/admin/routes".to_owned(), "Add"),
    };
    http::render(&RouteFormPage {
        csrf: &user.csrf,
        title,
        action,
        submit,
        error,
        conversation: &form.conversation,
        channels: choices(channels, &form.channel, "not configured"),
        kinds: choices(kinds, &form.kind, "unknown"),
        agents: choices(agents, &form.agent, "no such agent"),
        befores,
    })
}

/// `GET /admin/routes/new`: the form.
async fn new_route_page(State(pages): State<RoutePages>, user: WebUser) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    render_form(&pages, &user, &RouteForm::default(), None, None).await
}

/// The route id in a path or a form, if it is one.
fn route_id(id: &str) -> Option<RouteId> {
    id.parse().ok().map(RouteId::new)
}

/// The 404 page for a path that isn't a route id.
fn no_such_route() -> Response {
    error_page(
        StatusCode::NOT_FOUND,
        "No such route",
        "That route doesn't exist. Go back to the list.",
    )
}

/// `GET /admin/routes/<id>/edit`: the form, filled in with the route.
async fn edit_route_page(
    State(pages): State<RoutePages>,
    user: WebUser,
    Path(id): Path<String>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    let Some(id) = route_id(&id) else {
        return no_such_route();
    };
    let views = match pages.routes.view().await {
        Ok(views) => views,
        Err(err) => return failed(&err, "Routes unavailable"),
    };
    let Some(view) = views.iter().find(|view| view.id == id) else {
        return failed(&RouteError::NotFound, "");
    };
    let spec = RouteSpec::of(&view.route);
    let form = RouteForm {
        channel: spec.channel,
        kind: spec.kind,
        conversation: spec.conversation,
        agent: spec.agent,
        ..RouteForm::default()
    };
    render_form(&pages, &user, &form, Some(id), None).await
}

/// Shows `form` again with the problem: 400 for an invalid value, 409 for a route that couldn't
/// be used or would hide one. Anything else is [`failed`].
async fn form_error(
    pages: &RoutePages,
    user: &WebUser,
    form: &RouteForm,
    editing: Option<RouteId>,
    err: RouteError,
) -> Response {
    if !err.is_invalid_input() {
        return failed(&err, "");
    }
    let status = match err {
        RouteError::Invalid { .. } => StatusCode::BAD_REQUEST,
        _ => StatusCode::CONFLICT,
    };
    let mut response = render_form(pages, user, form, editing, Some(err.to_string())).await;
    if response.status().is_success() {
        *response.status_mut() = status;
    }
    response
}

/// `POST /admin/routes` (CSRF-checked): adds the route, then 303 to the list. An invalid form is
/// shown again with the problem; nothing is stored.
async fn create(
    State(pages): State<RoutePages>,
    user: WebUser,
    headers: HeaderMap,
    Form(form): Form<RouteForm>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let before = match form.before.trim() {
        "" => None,
        id => match route_id(id) {
            Some(id) => Some(id),
            None => return failed(&RouteError::NotFound, ""),
        },
    };
    match pages.routes.add(&form.spec(), before).await {
        Ok(id) => Redirect::to(&format!("/admin/routes#route-{id}")).into_response(),
        Err(err) => form_error(&pages, &user, &form, None, err).await,
    }
}

/// `POST /admin/routes/<id>` (CSRF-checked): replaces the route's rule, keeping its place.
async fn update(
    State(pages): State<RoutePages>,
    user: WebUser,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<RouteForm>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let Some(id) = route_id(&id) else {
        return no_such_route();
    };
    match pages.routes.update(id, &form.spec()).await {
        Ok(()) => Redirect::to(&format!("/admin/routes#route-{id}")).into_response(),
        Err(err) => form_error(&pages, &user, &form, Some(id), err).await,
    }
}

/// The fields of the one-button forms.
#[derive(Debug, Default, Deserialize)]
struct ActionForm {
    #[serde(default)]
    csrf: Option<String>,
    #[serde(default)]
    direction: String,
}

/// `POST /admin/routes/<id>/remove` (CSRF-checked): removes the route, then 303 to the list.
async fn remove(
    State(pages): State<RoutePages>,
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
    let Some(id) = route_id(&id) else {
        return no_such_route();
    };
    match pages.routes.remove(id).await {
        Ok(()) => Redirect::to("/admin/routes").into_response(),
        Err(err) => failed(&err, "Route not removed"),
    }
}

/// `POST /admin/routes/<id>/move` (CSRF-checked, `direction` is `up` or `down`): moves the route
/// one place, then 303 to the list. A move that would hide a route is refused with the reason.
async fn move_route(
    State(pages): State<RoutePages>,
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
    let Some(id) = route_id(&id) else {
        return no_such_route();
    };
    let up = match form.direction.as_str() {
        "up" => true,
        "down" => false,
        _ => {
            return error_page(
                StatusCode::BAD_REQUEST,
                "Route not moved",
                "Say whether to move the route up or down.",
            );
        }
    };
    match pages.routes.move_route(id, up).await {
        Ok(()) => Redirect::to(&format!("/admin/routes#route-{id}")).into_response(),
        Err(err) => failed(&err, "Route not moved"),
    }
}

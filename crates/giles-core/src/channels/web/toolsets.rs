//! The toolset pages: every toolset, one toolset's settings and tools, and the forms that add,
//! edit, and remove a stored toolset while the core runs. Next to the admin pages (`admin.rs`), the
//! agent pages (`agents.rs`), and the route pages (`routes.rs`), and like them: behind the login,
//! the owner only, every answer `no-store`, every change a CSRF-checked form post.
//!
//! A change is made by the [`ToolsetManager`], which validates it, stores it, and starts, restarts,
//! or stops the server. What the pages say about a toolset's state and tool count comes from the
//! admin service, as it does on the plugins page. The tools listed on a toolset's page are the ones
//! new conversations get now.
//!
//! The form adds and edits remote MCP servers only, as the manager stores nothing else: a server
//! Giles runs on the host is configured in `giles.toml`, and the pages list it read-only.
//!
//! No page shows a secret value. A bearer token is the name of a secret in `secrets.toml`, shown
//! by name.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{from_fn_with_state, map_response};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use giles_proto::admin::v1 as pb;
use giles_proto::admin::v1::admin_service_server::AdminService as AdminRpc;
use giles_types::Timestamp;
use serde_json::{Map, Value};
use tonic::Request;
use tracing::error;

use super::admin::{PluginRow, call_failed, forbidden, may_administer, plugin_row};
use super::auth::{WebUser, check_csrf};
use super::pages::{WebApp, agent_label, error_page, no_store};
use crate::config::{
    Config, InstanceName, KeyPath, LiveConfig, Program, ToolsetError, ToolsetInstance, ToolsetSpec,
    ToolsetTransport,
};
use crate::http::{self, RouterBuilder};
use crate::registry::{Registry, ToolTarget};
use crate::toolsets::{
    RESERVED_TOOLSET_NAME, ToolsetManager, ToolsetManagerError, ToolsetSource, ToolsetView,
};

/// The toolset pages' state.
#[derive(Clone)]
struct ToolsetPages {
    manager: Arc<ToolsetManager>,
    admin: Arc<dyn AdminRpc>,
    registry: Arc<Registry>,
    config: LiveConfig,
}

impl ToolsetPages {
    /// The toolsets the admin service reports, by instance name.
    async fn plugins(&self) -> Result<HashMap<String, PluginRow>, Box<Response>> {
        match self
            .admin
            .list_plugins(Request::new(pb::ListPluginsRequest {}))
            .await
        {
            Ok(listed) => Ok(listed
                .get_ref()
                .plugins
                .iter()
                .filter(|plugin| plugin.kind() == pb::InstanceKind::Toolset)
                .map(|plugin| (plugin.instance.clone(), plugin_row(plugin)))
                .collect()),
            Err(status) => Err(Box::new(call_failed(
                "ListPlugins",
                "MCP servers unavailable",
                &status,
            ))),
        }
    }
}

impl WebApp {
    /// Adds the toolset pages to `builder`, changing toolsets with `manager`, behind the same
    /// authentication middleware as the chat pages. `admin` (in `giles serve`, the
    /// [`AdminService`](crate::admin::AdminService)) says how each toolset is doing, and
    /// `registry` holds the tools new conversations get. The pages read this app's running
    /// configuration, which should be the one `manager` changes. Every answer on these routes is
    /// `no-store`.
    pub fn install_toolsets(
        &self,
        builder: RouterBuilder,
        manager: Arc<ToolsetManager>,
        admin: Arc<dyn AdminRpc>,
        registry: Arc<Registry>,
    ) -> RouterBuilder {
        let auth = from_fn_with_state(self.clone(), super::login::authenticate);
        let pages = ToolsetPages {
            manager,
            admin,
            registry,
            config: self.inner.agents.clone(),
        };
        builder.merge(
            Router::new()
                .route("/admin/mcp-servers", get(list_page).post(create))
                .route("/admin/mcp-servers/new", get(new_page))
                .route("/admin/mcp-servers/{name}", get(detail_page))
                .route(
                    "/admin/mcp-servers/{name}/edit",
                    get(edit_page).post(update),
                )
                .route(
                    "/admin/mcp-servers/{name}/remove",
                    get(remove_page).post(remove),
                )
                .route("/admin/mcp-servers/{name}/restart", post(restart))
                .with_state(pages)
                .route_layer(auth)
                .route_layer(map_response(async |response: Response| no_store(response))),
        )
    }
}

/// The page for a failed read, or a change that has no form to show: 404 for a toolset that is
/// gone, the reason as a 409 for what the owner can act on, a logged 500 for anything else.
fn failed(err: &ToolsetManagerError, title: &str) -> Response {
    match err {
        ToolsetManagerError::NotFound => error_page(
            StatusCode::NOT_FOUND,
            "No such MCP server",
            "That MCP server no longer exists. It may have been removed on another page. Go back to the list of MCP servers.",
        ),
        err if err.is_invalid_input() => error_page(StatusCode::CONFLICT, title, &err.to_string()),
        err => {
            error!(error = %err, "changing the toolsets failed");
            error_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Something went wrong",
                "Giles couldn't use its MCP servers. The error is in its log.",
            )
        }
    }
}

/// A time on the pages, in UTC.
fn when(at: Timestamp) -> String {
    at.strftime("%Y-%m-%d %H:%M:%S UTC").to_string()
}

/// How a toolset runs, as the list says it, and where.
fn describe_spec(name: &str, spec: &ToolsetSpec) -> (&'static str, String) {
    if let Some(url) = &spec.url {
        ("remote URL", url.clone())
    } else if let Some(command) = &spec.command {
        ("command", command.clone())
    } else {
        let component = spec.component.as_deref().unwrap_or(name);
        ("first-party plugin", format!("giles-toolset-{component}"))
    }
}

/// [`describe_spec`] for a toolset of `giles.toml`.
fn describe_instance(toolset: &ToolsetInstance) -> (&'static str, String) {
    match &toolset.transport {
        ToolsetTransport::Http(remote) => ("remote URL", remote.url.to_string()),
        ToolsetTransport::Stdio(server) => match &server.program {
            Program::Command(command) => ("command", command.clone()),
            Program::Plugin { binary, .. } => ("first-party plugin", binary.clone()),
        },
    }
}

/// How `view` runs and where: from its spec if it has one, else from the running configuration.
fn describe(view: &ToolsetView, config: &Config) -> (&'static str, String) {
    if let Some(spec) = &view.spec {
        return describe_spec(&view.name, spec);
    }
    match config
        .toolsets
        .values()
        .find(|toolset| toolset.name.as_str() == view.name)
    {
        Some(toolset) => describe_instance(toolset),
        None => ("unknown", String::new()),
    }
}

/// A toolset in the list.
struct ToolsetRow {
    name: String,
    /// Configured in `giles.toml`, so shown read-only.
    from_file: bool,
    transport: &'static str,
    location: String,
    state: String,
    class: &'static str,
    tools: Option<u32>,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "web/admin_toolsets.html")]
struct ToolsetsPage {
    toolsets: Vec<ToolsetRow>,
}

/// `GET /admin/mcp-servers`: every toolset with how it runs, its state, its tool count, and its last
/// error. Toolsets of `giles.toml` are listed without Edit and Remove.
async fn list_page(State(pages): State<ToolsetPages>, user: WebUser) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    let views = match pages.manager.list().await {
        Ok(views) => views,
        Err(err) => return failed(&err, "MCP servers unavailable"),
    };
    let mut plugins = match pages.plugins().await {
        Ok(plugins) => plugins,
        Err(response) => return *response,
    };
    let config = pages.config.get();
    let toolsets = views
        .into_iter()
        .map(|view| {
            let (transport, location) = describe(&view, &config);
            let plugin = plugins.remove(&view.name);
            let (state, class, tools, error) = match plugin {
                Some(plugin) => (
                    plugin.state_word.to_owned(),
                    plugin.class,
                    plugin.tools,
                    plugin.error,
                ),
                None => ("unknown".to_owned(), "unknown", None, None),
            };
            ToolsetRow {
                from_file: view.source == ToolsetSource::File,
                name: view.name,
                transport,
                location,
                state,
                class,
                tools,
                error: error.or(view.failure),
            }
        })
        .collect();
    http::render(&ToolsetsPage { toolsets })
}

/// One row of a toolset's settings: a label and what it is set to, one value per line.
struct Setting {
    label: &'static str,
    values: Vec<String>,
}

/// A tool in the toolset's tool list.
struct ToolRow {
    name: String,
    description: String,
}

/// The tools new conversations get from the toolset `name`, by name.
fn tools_of(registry: &Registry, name: &str) -> Vec<ToolRow> {
    registry
        .tools()
        .snapshot()
        .tools()
        .iter()
        .filter(|tool| {
            matches!(&tool.target, ToolTarget::Toolset { instance, .. } if instance.as_str() == name)
        })
        .map(|tool| ToolRow {
            name: tool.spec.name.clone(),
            description: tool.spec.description.clone(),
        })
        .collect()
}

#[derive(Template)]
#[template(path = "web/admin_toolset.html")]
struct ToolsetPage<'a> {
    csrf: &'a str,
    name: String,
    /// Stored in Giles, so it can be edited and removed.
    stored: bool,
    created: Option<String>,
    updated: Option<String>,
    transport: &'static str,
    plugin: Option<PluginRow>,
    error: Option<String>,
    restartable: bool,
    settings: Vec<Setting>,
    tools: Vec<ToolRow>,
}

/// `GET /admin/mcp-servers/<name>`: the settings (references by name), how it is doing, the tools it
/// offers now with their descriptions, and the buttons.
async fn detail_page(
    State(pages): State<ToolsetPages>,
    user: WebUser,
    Path(name): Path<String>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    let view = match pages.manager.get(&name).await {
        Ok(view) => view,
        Err(err) => return failed(&err, "MCP server unavailable"),
    };
    let mut plugins = match pages.plugins().await {
        Ok(plugins) => plugins,
        Err(response) => return *response,
    };
    let plugin = plugins.remove(&view.name);
    let (transport, _) = describe(&view, &pages.config.get());
    let (stored, created, updated) = match view.source {
        ToolsetSource::Stored {
            created_at,
            updated_at,
        } => (true, Some(when(created_at)), Some(when(updated_at))),
        ToolsetSource::File => (false, None, None),
    };
    let settings = view
        .spec
        .as_ref()
        .map(|spec| settings(&ToolsetForm::from_spec(&view.name, spec, None)))
        .unwrap_or_default();
    let error = plugin
        .as_ref()
        .and_then(|plugin| plugin.error.clone())
        .or_else(|| view.failure.clone());
    http::render(&ToolsetPage {
        csrf: &user.csrf,
        restartable: plugin.as_ref().is_some_and(|plugin| plugin.restartable),
        tools: tools_of(&pages.registry, &view.name),
        name: view.name,
        stored,
        created,
        updated,
        transport,
        plugin,
        error,
        settings,
    })
}

/// The form's fields, as posted (and shown again after an error), and as a stored spec fills them
/// in. Everything is the text the owner typed; [`spec`](Self::spec) turns it into a
/// [`ToolsetSpec`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolsetForm {
    /// The `updated_at` the form was filled from, to refuse an edit of a toolset that changed.
    updated_at: String,
    name: String,
    url: String,
    insecure: bool,
    /// `none` or `secret`.
    bearer: String,
    /// The name of the secret in `secrets.toml` that holds the token.
    bearer_secret: String,
    deadline: String,
    parallel: bool,
    /// One directory per line.
    skills: String,
}

/// Non-empty lines of `text`, trimmed.
fn trimmed_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// A duration as the largest whole unit that holds it: `5m`, `90s`, `1500ms`.
fn humanize(duration: Duration) -> String {
    let millis = duration.as_millis();
    for (unit, scale) in [
        ("d", 86_400_000),
        ("h", 3_600_000),
        ("m", 60_000),
        ("s", 1_000),
    ] {
        if millis >= scale && millis.is_multiple_of(scale) {
            return format!("{}{unit}", millis / scale);
        }
    }
    format!("{millis}ms")
}

/// The name of the secret a stored `auth` takes its bearer token from. A stored toolset has no
/// other kind of token.
fn bearer_secret(auth: Option<&toml::Value>) -> Option<String> {
    auth.and_then(toml::Value::as_table)
        .and_then(|table| table.get("bearer"))
        .and_then(toml::Value::as_table)
        .and_then(|bearer| bearer.get("secret"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
}

impl ToolsetForm {
    /// The form for a new toolset.
    fn blank() -> Self {
        Self {
            updated_at: String::new(),
            name: String::new(),
            url: String::new(),
            insecure: false,
            bearer: "none".to_owned(),
            bearer_secret: String::new(),
            deadline: String::new(),
            parallel: false,
            skills: String::new(),
        }
    }

    /// The form of the stored toolset `name` with `spec`, last changed at `updated_at`.
    fn from_spec(name: &str, spec: &ToolsetSpec, updated_at: Option<Timestamp>) -> Self {
        let secret = bearer_secret(spec.auth.as_ref());
        Self {
            updated_at: updated_at.map(|at| at.to_string()).unwrap_or_default(),
            name: name.to_owned(),
            url: spec.url.clone().unwrap_or_default(),
            insecure: spec.insecure == Some(true),
            bearer: if secret.is_some() { "secret" } else { "none" }.to_owned(),
            bearer_secret: secret.unwrap_or_default(),
            deadline: spec
                .deadline
                .map(|deadline| humanize(deadline.0))
                .unwrap_or_default(),
            parallel: spec.parallel == Some(true),
            skills: spec
                .skills
                .as_ref()
                .map(|skills| {
                    skills
                        .iter()
                        .map(|path| path.to_string_lossy())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default(),
        }
    }

    /// The form as posted. Names the form doesn't have are ignored, and a bearer choice it doesn't
    /// offer is no token.
    fn posted(fields: &HashMap<String, String>) -> Self {
        let text = |key: &str| fields.get(key).cloned().unwrap_or_default();
        let flag = |key: &str| fields.contains_key(key);
        let bearer = match text("bearer").as_str() {
            "secret" => "secret",
            _ => "none",
        };
        Self {
            updated_at: text("updated_at").trim().to_owned(),
            name: text("name").trim().to_owned(),
            url: text("url").trim().to_owned(),
            insecure: flag("insecure"),
            bearer: bearer.to_owned(),
            bearer_secret: text("bearer_secret").trim().to_owned(),
            deadline: text("deadline").trim().to_owned(),
            parallel: flag("parallel"),
            skills: text("skills"),
        }
    }

    /// Whether the URL is plain `http:`, the only case "Allow insecure connections" applies to:
    /// the form shows the box for such a URL, and the box counts for no other.
    fn plain_http(&self) -> bool {
        self.url
            .get(..5)
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("http:"))
    }

    /// The spec the form describes.
    ///
    /// # Errors
    ///
    /// The key and the reason, as validation gives them, for a setting that isn't valid.
    fn spec(&self) -> Result<ToolsetSpec, ToolsetError> {
        let text = |text: &str| Value::String(text.to_owned());
        let mut map = Map::new();
        if !self.url.is_empty() {
            map.insert("url".to_owned(), text(&self.url));
        }
        if self.bearer == "secret" {
            let mut bearer = Map::new();
            bearer.insert("secret".to_owned(), text(&self.bearer_secret));
            let mut auth = Map::new();
            auth.insert("bearer".to_owned(), Value::Object(bearer));
            map.insert("auth".to_owned(), Value::Object(auth));
        }
        if self.insecure && self.plain_http() {
            map.insert("insecure".to_owned(), Value::Bool(true));
        }
        if !self.deadline.is_empty() {
            map.insert("deadline".to_owned(), text(&self.deadline));
        }
        if self.parallel {
            map.insert("parallel".to_owned(), Value::Bool(true));
        }
        let skills = trimmed_lines(&self.skills);
        if !skills.is_empty() {
            map.insert(
                "skills".to_owned(),
                Value::Array(skills.into_iter().map(Value::String).collect()),
            );
        }
        ToolsetSpec::from_json(&Value::Object(map).to_string())
    }
}

/// The settings of the toolset the form describes, for its page: the ones the owner set. The
/// bearer token is shown by the name of its secret.
fn settings(form: &ToolsetForm) -> Vec<Setting> {
    fn add(out: &mut Vec<Setting>, label: &'static str, values: Vec<String>) {
        if !values.is_empty() {
            out.push(Setting { label, values });
        }
    }
    fn one(text: &str) -> Vec<String> {
        if text.is_empty() {
            Vec::new()
        } else {
            vec![text.to_owned()]
        }
    }
    let mut out = Vec::new();
    add(&mut out, "URL", one(&form.url));
    if form.bearer == "secret" {
        add(
            &mut out,
            "Authentication",
            vec![format!("secret {}", form.bearer_secret)],
        );
    }
    if form.insecure && form.plain_http() {
        add(&mut out, "Insecure connections", vec!["allowed".to_owned()]);
    }
    add(&mut out, "Deadline", one(&form.deadline));
    if form.parallel {
        add(&mut out, "Parallel tools", vec!["allowed".to_owned()]);
    }
    add(
        &mut out,
        "Skills",
        form.skills.lines().map(str::to_owned).collect(),
    );
    out
}

#[derive(Template)]
#[template(path = "web/admin_toolset_form.html")]
struct ToolsetFormPage<'a> {
    csrf: &'a str,
    title: String,
    action: String,
    submit: &'a str,
    /// Edits a stored toolset (else the form adds one).
    editing: bool,
    /// The toolset changed since the form was loaded.
    stale: bool,
    error: Option<String>,
    notice: Option<String>,
    form: &'a ToolsetForm,
}

/// What is wrong with a posted form, and the status that says so.
struct Problem {
    status: StatusCode,
    message: String,
    /// The toolset was changed since the form was loaded.
    stale: bool,
}

impl Problem {
    /// A setting that isn't valid: 400.
    fn invalid(error: &ToolsetError) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: error.to_string(),
            stale: false,
        }
    }

    /// What the manager refused, if the form can show it: 400 for a setting or a name that isn't
    /// valid, 409 for a name that is taken, a toolset that changed meanwhile, and a toolset the
    /// name of which is fine but that can't be used as a whole. `None` for the rest, which
    /// [`failed`] answers.
    fn of(err: &ToolsetManagerError, name: &str) -> Option<Self> {
        let status = match err {
            ToolsetManagerError::Invalid { key, .. }
                if key.is_root()
                    && InstanceName::new(name).is_ok()
                    && !name.eq_ignore_ascii_case(RESERVED_TOOLSET_NAME) =>
            {
                StatusCode::CONFLICT
            }
            ToolsetManagerError::Invalid { .. } => StatusCode::BAD_REQUEST,
            ToolsetManagerError::Exists | ToolsetManagerError::Stale => StatusCode::CONFLICT,
            _ => return None,
        };
        Some(Self {
            status,
            message: err.to_string(),
            stale: matches!(err, ToolsetManagerError::Stale),
        })
    }
}

/// The form for `form`, with `problem` if it is shown again after an error. `editing` says whether
/// it edits the stored toolset `form.name`; otherwise it adds one.
fn render_form(
    user: &WebUser,
    form: &ToolsetForm,
    editing: bool,
    problem: Option<Problem>,
    notice: Option<String>,
) -> Response {
    let (title, action, submit) = if editing {
        (
            format!("Edit {}", form.name),
            format!("/admin/mcp-servers/{}/edit", form.name),
            "Save",
        )
    } else {
        (
            "New MCP server".to_owned(),
            "/admin/mcp-servers".to_owned(),
            "Add",
        )
    };
    let status = problem.as_ref().map(|problem| problem.status);
    let mut response = http::render(&ToolsetFormPage {
        csrf: &user.csrf,
        title,
        action,
        submit,
        editing,
        stale: problem.as_ref().is_some_and(|problem| problem.stale),
        error: problem.map(|problem| problem.message),
        notice,
        form,
    });
    if let Some(status) = status
        && response.status().is_success()
    {
        *response.status_mut() = status;
    }
    response
}

/// `GET /admin/mcp-servers/new`: the form.
async fn new_page(user: WebUser) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    render_form(&user, &ToolsetForm::blank(), false, None, None)
}

/// `POST /admin/mcp-servers` (CSRF-checked): adds the toolset, then 303 to its page. A form that
/// isn't valid is shown again with the key and the reason; nothing is stored.
async fn create(
    State(pages): State<ToolsetPages>,
    user: WebUser,
    headers: HeaderMap,
    Form(fields): Form<HashMap<String, String>>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    if let Err(rejected) = check_csrf(&user, &headers, fields.get("csrf").map(String::as_str)) {
        return rejected.into_response();
    }
    let form = ToolsetForm::posted(&fields);
    let spec = match form.spec() {
        Ok(spec) => spec,
        Err(error) => {
            let problem = Problem::invalid(&error);
            return render_form(&user, &form, false, Some(problem), None);
        }
    };
    match pages.manager.add(&form.name, &spec).await {
        Ok(view) => Redirect::to(&format!("/admin/mcp-servers/{}", view.name)).into_response(),
        Err(err) => match Problem::of(&err, &form.name) {
            Some(problem) => render_form(&user, &form, false, Some(problem), None),
            None => failed(&err, "MCP server not added"),
        },
    }
}

/// The stored toolset `name` for a form, or the page that says why there is none: gone, or in
/// `giles.toml`.
async fn stored_view(
    pages: &ToolsetPages,
    name: &str,
) -> Result<(ToolsetView, Timestamp), Box<Response>> {
    let view = pages
        .manager
        .get(name)
        .await
        .map_err(|err| Box::new(failed(&err, "MCP server not changed")))?;
    match view.source {
        ToolsetSource::Stored { updated_at, .. } => Ok((view, updated_at)),
        ToolsetSource::File => Err(Box::new(failed(
            &ToolsetManagerError::Unsupported,
            "MCP server not changed",
        ))),
    }
}

/// `GET /admin/mcp-servers/<name>/edit`: the form, filled in from the stored spec, with the
/// `updated_at` it was read at.
async fn edit_page(
    State(pages): State<ToolsetPages>,
    user: WebUser,
    Path(name): Path<String>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    let (view, updated_at) = match stored_view(&pages, &name).await {
        Ok(found) => found,
        Err(response) => return *response,
    };
    let (form, notice) = match &view.spec {
        Some(spec) => (
            ToolsetForm::from_spec(&view.name, spec, Some(updated_at)),
            None,
        ),
        None => {
            let mut form = ToolsetForm::blank();
            form.name.clone_from(&view.name);
            form.updated_at = updated_at.to_string();
            let notice = view
                .failure
                .map(|reason| format!("{reason}. Fill in the form to replace it."));
            (form, notice)
        }
    };
    render_form(&user, &form, true, None, notice)
}

/// `POST /admin/mcp-servers/<name>/edit` (CSRF-checked): replaces the settings and restarts the
/// toolset with them, then 303 to its page. A form that isn't valid is shown again with the key
/// and the reason; one for a toolset that changed since the form was loaded is a 409 with a
/// link to reload it.
async fn update(
    State(pages): State<ToolsetPages>,
    user: WebUser,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(fields): Form<HashMap<String, String>>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    if let Err(rejected) = check_csrf(&user, &headers, fields.get("csrf").map(String::as_str)) {
        return rejected.into_response();
    }
    let (view, _) = match stored_view(&pages, &name).await {
        Ok(found) => found,
        Err(response) => return *response,
    };
    let mut form = ToolsetForm::posted(&fields);
    form.name.clone_from(&view.name);
    let expected = if form.updated_at.is_empty() {
        None
    } else {
        match form.updated_at.parse::<Timestamp>() {
            Ok(at) => Some(at),
            Err(_) => {
                let problem = Problem::invalid(&ToolsetError {
                    key: KeyPath::root(),
                    reason: "the form's updated_at isn't a time: reload the form".to_owned(),
                });
                return render_form(&user, &form, true, Some(problem), None);
            }
        }
    };
    let spec = match form.spec() {
        Ok(spec) => spec,
        Err(error) => {
            let problem = Problem::invalid(&error);
            return render_form(&user, &form, true, Some(problem), None);
        }
    };
    match pages.manager.update(&view.name, &spec, expected).await {
        Ok(updated) => {
            Redirect::to(&format!("/admin/mcp-servers/{}", updated.name)).into_response()
        }
        Err(err) => match Problem::of(&err, &view.name) {
            Some(problem) => render_form(&user, &form, true, Some(problem), None),
            None => failed(&err, "MCP server not changed"),
        },
    }
}

/// An agent that mentions the toolset's tools.
struct Mention {
    id: String,
    label: String,
    /// The `tools` and `ask` entries that start with `<toolset>__`.
    entries: Vec<String>,
}

/// The agents whose `tools` or `ask` mention tools of the toolset `name` (entries that start with
/// `<name>__`), by id.
fn mentions(config: &Config, name: &str) -> Vec<Mention> {
    let prefix = format!("{name}__");
    config
        .agents
        .values()
        .filter_map(|agent| {
            let entries: Vec<String> = agent
                .tools
                .iter()
                .chain(&agent.ask)
                .filter(|entry| entry.starts_with(&prefix))
                .cloned()
                .collect();
            (!entries.is_empty()).then(|| Mention {
                id: agent.id.to_string(),
                label: agent_label(config, &agent.id),
                entries,
            })
        })
        .collect()
}

#[derive(Template)]
#[template(path = "web/admin_toolset_remove.html")]
struct RemovePage<'a> {
    csrf: &'a str,
    name: String,
    agents: Vec<Mention>,
}

/// `GET /admin/mcp-servers/<name>/remove`: what removing does, the agents that mention the toolset's
/// tools, and the button. Nothing happens until the form is posted.
async fn remove_page(
    State(pages): State<ToolsetPages>,
    user: WebUser,
    Path(name): Path<String>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    let (view, _) = match stored_view(&pages, &name).await {
        Ok(found) => found,
        Err(response) => return *response,
    };
    http::render(&RemovePage {
        csrf: &user.csrf,
        agents: mentions(&pages.config.get(), &view.name),
        name: view.name,
    })
}

/// `POST /admin/mcp-servers/<name>/remove` (CSRF-checked): removes the toolset, then 303 to the list.
async fn remove(
    State(pages): State<ToolsetPages>,
    user: WebUser,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(fields): Form<HashMap<String, String>>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    if let Err(rejected) = check_csrf(&user, &headers, fields.get("csrf").map(String::as_str)) {
        return rejected.into_response();
    }
    let (view, _) = match stored_view(&pages, &name).await {
        Ok(found) => found,
        Err(response) => return *response,
    };
    match pages.manager.remove(&view.name).await {
        Ok(_) => Redirect::to("/admin/mcp-servers").into_response(),
        Err(err) => failed(&err, "MCP server not removed"),
    }
}

/// `POST /admin/mcp-servers/<name>/restart` (CSRF-checked): `RestartPlugin` for the toolset, then 303
/// back to its page. A name that isn't a toolset is a 404, whatever else has the name.
async fn restart(
    State(pages): State<ToolsetPages>,
    user: WebUser,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(fields): Form<HashMap<String, String>>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    if let Err(rejected) = check_csrf(&user, &headers, fields.get("csrf").map(String::as_str)) {
        return rejected.into_response();
    }
    let view = match pages.manager.get(&name).await {
        Ok(view) => view,
        Err(err) => return failed(&err, "MCP server not restarted"),
    };
    match pages
        .admin
        .restart_plugin(Request::new(pb::RestartPluginRequest {
            instance: view.name.clone(),
        }))
        .await
    {
        Ok(_) => Redirect::to(&format!("/admin/mcp-servers/{}", view.name)).into_response(),
        Err(status) => call_failed("RestartPlugin", "Restart failed", &status),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(json: &str) -> ToolsetSpec {
        ToolsetSpec::from_json(json).unwrap_or_else(|e| panic!("{e}"))
    }

    /// A spec filled into the form and built again is the same spec.
    #[test]
    fn a_spec_survives_the_form() {
        for json in [
            r#"{ "url": "http://example.com/mcp",
                 "auth": { "bearer": { "secret": "search" } }, "insecure": true,
                 "deadline": "90s", "parallel": true, "skills": ["/srv/skills"] }"#,
            r#"{ "url": "https://example.com/mcp" }"#,
        ] {
            let original = spec(json);
            let form = ToolsetForm::from_spec("search", &original, None);
            assert_eq!(form.spec().unwrap(), original, "{json}");
        }
    }

    /// What the browser posts for a form is what `from_spec` filled in.
    #[test]
    fn a_posted_form_reads_back_what_was_shown() {
        let original = spec(
            r#"{ "url": "https://example.com/mcp", "auth": { "bearer": { "secret": "t" } },
                 "deadline": "2m", "skills": ["/a", "/b"] }"#,
        );
        let form = ToolsetForm::from_spec("search", &original, None);
        let fields: HashMap<String, String> = [
            ("url", form.url.clone()),
            ("bearer", form.bearer.clone()),
            ("bearer_secret", form.bearer_secret.clone()),
            ("deadline", form.deadline.clone()),
            ("skills", form.skills.clone()),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect();
        let posted = ToolsetForm::posted(&fields);
        assert_eq!(posted.spec().unwrap(), original);
    }

    #[test]
    fn problems_with_a_value_name_the_key() {
        let mut form = ToolsetForm::blank();
        form.url = "https://example.com/mcp".to_owned();
        form.deadline = "soon".to_owned();
        assert_eq!(form.spec().unwrap_err().key.to_string(), "deadline");
    }

    #[test]
    fn durations_are_shown_in_the_largest_whole_unit() {
        for (millis, shown) in [
            (300_000, "5m"),
            (90_000, "90s"),
            (3_600_000, "1h"),
            (1_500, "1500ms"),
            (172_800_000, "2d"),
            (0, "0ms"),
        ] {
            assert_eq!(humanize(Duration::from_millis(millis)), shown);
        }
    }
}

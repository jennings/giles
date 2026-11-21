//! The identity provider pages, `/admin/identity-providers`: the OpenID Connect providers users
//! sign in with, and the forms that add, edit, disable, and remove them while Doozers runs. Next to
//! the other admin pages and like them: behind the login, admins only, every answer `no-store`,
//! every change a CSRF-checked form post. The sign-in routes they feed are in
//! [`oidc_pages`](super::oidc_pages), and the design in `Docs/technical/web-channel.md`.
//!
//! A provider's settings come from its discovery document: saving one fetches it fresh, and a
//! provider that can't be used (it can't be reached, names another issuer, has no code flow) is
//! refused with the reason, and nothing is stored. The client secret is typed into a write-only
//! field and goes to the secret store as `oidc/<id>/client-secret`; no page shows it, and a blank
//! field keeps the one stored. Everything a provider's document or an admin typed is escaped by
//! the templates.

use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use doozers_store::{OidcProvider, OidcProviderSettings, is_provider_id};
use serde::Deserialize;
use tracing::{error, warn};

use super::admin::{forbidden, may_administer};
use super::auth::{WebUser, check_csrf};
use super::oidc_pages::OidcPages;
use super::pages::{error_page, internal_error};
use super::shell::{Nav, Shell};
use crate::config::Secret;
use crate::http;
use crate::secrets::SecretPath;

/// The routes of the pages, with the state of the sign-in routes.
pub(super) fn routes() -> Router<OidcPages> {
    Router::new()
        .route("/admin/identity-providers", get(list_page).post(create))
        .route("/admin/identity-providers/new", get(new_page))
        .route("/admin/identity-providers/{id}", post(update))
        .route("/admin/identity-providers/{id}/edit", get(edit_page))
        .route("/admin/identity-providers/{id}/enabled", post(set_enabled))
        .route(
            "/admin/identity-providers/{id}/remove",
            get(remove_page).post(remove),
        )
}

/// The name of the client secret of provider `id` in the secret store.
fn secret_name(id: &str) -> String {
    format!("oidc/{id}/client-secret")
}

fn secret_path(name: &str) -> Option<SecretPath> {
    SecretPath::new(name).ok()
}

/// A provider in the list.
struct ProviderRow {
    id: String,
    display_name: String,
    issuer: String,
    client_id: String,
    secret_stored: bool,
    enabled: bool,
    redirect_uri: String,
    /// How many accounts are connected, as a sentence.
    connected: String,
}

#[derive(Template)]
#[template(path = "web/admin_oidc_providers.html")]
struct ListPage<'a> {
    shell: Shell,
    csrf: &'a str,
    providers: Vec<ProviderRow>,
    notice: Option<String>,
    /// Whether the channel has a `public_url`.
    has_public_url: bool,
    /// The address redirect URIs start with.
    base: String,
}

/// `?added=`, `?saved=`, or `?removed=`, after a change: the provider's name.
#[derive(Debug, Default, Deserialize)]
struct Notice {
    #[serde(default)]
    added: Option<String>,
    #[serde(default)]
    saved: Option<String>,
    #[serde(default)]
    removed: Option<String>,
}

fn connected_text(count: u64) -> String {
    match count {
        0 => "none".to_owned(),
        1 => "1 account".to_owned(),
        n => format!("{n} accounts"),
    }
}

/// `GET /admin/identity-providers`: every provider, with its redirect URI.
async fn list_page(
    State(pages): State<OidcPages>,
    user: WebUser,
    Query(notice): Query<Notice>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    let store = &pages.app.inner.store;
    let providers = match store.oidc_providers().await {
        Ok(providers) => providers,
        Err(err) => return internal_error("cannot list the identity providers", &err),
    };
    let mut rows = Vec::new();
    for provider in providers {
        let connected = match store
            .oidc_connection_count(&provider.id, &provider.issuer)
            .await
        {
            Ok(count) => count,
            Err(err) => return internal_error("cannot count the connected accounts", &err),
        };
        rows.push(ProviderRow {
            redirect_uri: pages.redirect_uri(&provider.id),
            secret_stored: provider.client_secret.is_some(),
            connected: connected_text(connected),
            id: provider.id,
            display_name: provider.display_name,
            issuer: provider.issuer,
            client_id: provider.client_id,
            enabled: provider.enabled,
        });
    }
    let notice = match notice {
        Notice {
            added: Some(name), ..
        } => Some(format!("Added {name}.")),
        Notice {
            saved: Some(name), ..
        } => Some(format!("Saved {name}.")),
        Notice {
            removed: Some(name),
            ..
        } => Some(format!("Removed {name}.")),
        _ => None,
    };
    let channel = &pages.app.inner.channel;
    http::render(&ListPage {
        shell: Shell::new(&user, Nav::AdminIdentityProviders),
        csrf: &user.csrf,
        providers: rows,
        notice,
        has_public_url: channel.config().public_url.is_some(),
        base: channel.link_base(),
    })
}

/// The form's fields, as posted (and shown again after an error).
#[derive(Debug, Default, Deserialize)]
struct ProviderForm {
    #[serde(default)]
    csrf: Option<String>,
    #[serde(default)]
    id: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    issuer: String,
    #[serde(default)]
    client_id: String,
    /// What was typed in the write-only field. Never shown again.
    #[serde(default)]
    client_secret: String,
    /// The checkbox "Remove the stored secret": present when ticked.
    #[serde(default)]
    clear_secret: Option<String>,
    #[serde(default)]
    scopes: String,
    /// The checkbox "Enabled": present when ticked.
    #[serde(default)]
    enabled: Option<String>,
}

/// What the form template shows: the fields, as strings, with `clear_secret` and `enabled` as
/// flags. It has no secret.
struct FormView {
    id: String,
    display_name: String,
    issuer: String,
    client_id: String,
    scopes: String,
    clear_secret: bool,
    enabled: bool,
}

impl FormView {
    fn of(form: &ProviderForm) -> Self {
        Self {
            id: form.id.clone(),
            display_name: form.display_name.clone(),
            issuer: form.issuer.clone(),
            client_id: form.client_id.clone(),
            scopes: form.scopes.clone(),
            clear_secret: form.clear_secret.is_some(),
            enabled: form.enabled.is_some(),
        }
    }

    fn blank() -> Self {
        Self {
            id: String::new(),
            display_name: String::new(),
            issuer: String::new(),
            client_id: String::new(),
            scopes: "openid".to_owned(),
            clear_secret: false,
            enabled: true,
        }
    }

    fn stored(provider: &OidcProvider) -> Self {
        Self {
            id: provider.id.clone(),
            display_name: provider.display_name.clone(),
            issuer: provider.issuer.clone(),
            client_id: provider.client_id.clone(),
            scopes: provider.scopes.join(" "),
            clear_secret: false,
            enabled: provider.enabled,
        }
    }
}

#[derive(Template)]
#[template(path = "web/admin_oidc_form.html")]
struct FormPage<'a> {
    shell: Shell,
    csrf: &'a str,
    title: &'a str,
    action: String,
    submit: &'a str,
    editing: bool,
    error: Option<String>,
    form: FormView,
    secret_stored: bool,
    redirect_uri: String,
}

/// Renders the form with `status`.
fn render_form(
    pages: &OidcPages,
    user: &WebUser,
    form: FormView,
    editing: bool,
    secret_stored: bool,
    error: Option<String>,
    status: StatusCode,
) -> Response {
    let (title, action, submit) = if editing {
        (
            "Edit identity provider",
            format!("/admin/identity-providers/{}", form.id),
            "Save",
        )
    } else {
        (
            "New identity provider",
            "/admin/identity-providers".to_owned(),
            "Add",
        )
    };
    let redirect_uri = if editing {
        pages.redirect_uri(&form.id)
    } else {
        pages.redirect_uri("<id>")
    };
    let mut response = http::render(&FormPage {
        shell: Shell::new(user, Nav::AdminIdentityProviders),
        csrf: &user.csrf,
        title,
        action,
        submit,
        editing,
        error,
        form,
        secret_stored,
        redirect_uri,
    });
    if response.status().is_success() {
        *response.status_mut() = status;
    }
    response
}

fn no_such_provider() -> Response {
    error_page(
        StatusCode::NOT_FOUND,
        "No such identity provider",
        "That identity provider doesn't exist. It may have been removed on another page. Go back to \
         the list.",
    )
}

/// `GET /admin/identity-providers/new`: the form.
async fn new_page(State(pages): State<OidcPages>, user: WebUser) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    render_form(
        &pages,
        &user,
        FormView::blank(),
        false,
        false,
        None,
        StatusCode::OK,
    )
}

/// The provider `id` of a path, if there is one.
async fn stored_provider(pages: &OidcPages, id: &str) -> Result<OidcProvider, Box<Response>> {
    if !is_provider_id(id) {
        return Err(Box::new(no_such_provider()));
    }
    match pages.app.inner.store.oidc_provider(id).await {
        Ok(Some(provider)) => Ok(provider),
        Ok(None) => Err(Box::new(no_such_provider())),
        Err(err) => Err(Box::new(internal_error(
            "cannot look up an identity provider",
            &err,
        ))),
    }
}

/// `GET /admin/identity-providers/<id>/edit`: the form, filled in with the provider.
async fn edit_page(
    State(pages): State<OidcPages>,
    user: WebUser,
    Path(id): Path<String>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    let provider = match stored_provider(&pages, &id).await {
        Ok(provider) => provider,
        Err(response) => return *response,
    };
    render_form(
        &pages,
        &user,
        FormView::stored(&provider),
        true,
        provider.client_secret.is_some(),
        None,
        StatusCode::OK,
    )
}

/// The scopes as typed, as the list to store: `openid` first, every other one once, in order.
fn parse_scopes(typed: &str) -> Vec<String> {
    let mut scopes = vec!["openid".to_owned()];
    for scope in typed.split_whitespace() {
        if !scopes.iter().any(|have| have == scope) {
            scopes.push(scope.to_owned());
        }
    }
    scopes
}

/// The settings `provider` has now.
fn settings_of(provider: &OidcProvider) -> OidcProviderSettings {
    OidcProviderSettings {
        display_name: provider.display_name.clone(),
        issuer: provider.issuer.clone(),
        client_id: provider.client_id.clone(),
        client_secret: provider.client_secret.clone(),
        scopes: provider.scopes.clone(),
        enabled: provider.enabled,
    }
}

/// A store message (`a provider's scopes must include openid`) as a sentence for a page.
fn sentence(message: &str) -> String {
    let mut chars = message.chars();
    let first = chars.next().map(|c| c.to_uppercase().collect::<String>());
    let mut text = first.unwrap_or_default() + chars.as_str();
    if !text.ends_with('.') {
        text.push('.');
    }
    text
}

/// What a valid form says, before the secret: the settings, with the secret's name left as it was
/// (`current`), checked as the store checks them, so nothing is changed that the store would then
/// refuse. `Err` is the sentence for the form.
fn settings(form: &ProviderForm, current: Option<String>) -> Result<OidcProviderSettings, String> {
    let text = |value: &str| {
        let value = value.trim();
        (!value.is_empty() && !value.chars().any(char::is_control)).then(|| value.to_owned())
    };
    let new = OidcProviderSettings {
        display_name: text(&form.display_name).ok_or("Give the provider a name.")?,
        issuer: text(&form.issuer).ok_or("Give the provider's issuer URL.")?,
        client_id: text(&form.client_id).ok_or("Give the client id the provider issued.")?,
        client_secret: current,
        scopes: parse_scopes(&form.scopes),
        enabled: form.enabled.is_some(),
    };
    match new.validate() {
        Ok(()) => Ok(new),
        Err(doozers_store::Error::InvalidRequest(why)) => Err(sentence(&why)),
        Err(err) => Err(sentence(&err.to_string())),
    }
}

/// Asks the provider for its discovery document, as a save does: `Err` is the reason, for the
/// form.
async fn check_provider(pages: &OidcPages, issuer: &str) -> Result<(), String> {
    pages
        .oidc
        .fetch(issuer, pages.app.inner.clock.now())
        .await
        .map(|_| ())
        .map_err(|err| format!("Doozers can't use this provider: {err}."))
}

/// `POST /admin/identity-providers` (CSRF-checked): adds the provider, then 303 to the list. An
/// invalid form, or a provider that can't be used, is shown again with the reason; nothing is
/// stored.
async fn create(
    State(pages): State<OidcPages>,
    user: WebUser,
    headers: HeaderMap,
    Form(form): Form<ProviderForm>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let again = |error: String, status| {
        render_form(
            &pages,
            &user,
            FormView::of(&form),
            false,
            false,
            Some(error),
            status,
        )
    };
    let id = form.id.trim().to_owned();
    if !is_provider_id(&id) {
        return again(
            "The id must be 1 to 32 lowercase letters, digits, and dashes, starting with a letter \
             or digit."
                .to_owned(),
            StatusCode::BAD_REQUEST,
        );
    }
    let secret = (!form.client_secret.is_empty()).then(|| Secret::new(form.client_secret.clone()));
    let new = match settings(&form, secret.as_ref().map(|_| secret_name(&id))) {
        Ok(new) => new,
        Err(why) => return again(why, StatusCode::BAD_REQUEST),
    };
    let store = &pages.app.inner.store;
    match store.oidc_provider(&id).await {
        Ok(None) => {}
        Ok(Some(_)) => {
            return again(
                format!("A provider with the id {id} exists already."),
                StatusCode::CONFLICT,
            );
        }
        Err(err) => return internal_error("cannot look up an identity provider", &err),
    }
    if let Err(why) = check_provider(&pages, &new.issuer).await {
        return again(why, StatusCode::UNPROCESSABLE_ENTITY);
    }
    let added = match store
        .oidc_provider_add(&id, new, pages.app.inner.clock.now())
        .await
    {
        Ok(added) => added,
        Err(doozers_store::Error::Conflict(_)) => {
            return again(
                format!("A provider with the id {id} exists already."),
                StatusCode::CONFLICT,
            );
        }
        Err(doozers_store::Error::InvalidRequest(why)) => {
            return again(sentence(&why), StatusCode::BAD_REQUEST);
        }
        Err(err) => return internal_error("cannot add an identity provider", &err),
    };
    if let (Some(secret), Some(name)) = (&secret, &added.client_secret) {
        let stored = match secret_path(name) {
            Some(path) => pages
                .secrets
                .put(&path, secret, Some(&user.user))
                .await
                .map(|_| ())
                .map_err(|err| err.to_string()),
            None => Err(format!("{name} is not a secret name")),
        };
        if let Err(err) = stored {
            // A provider whose secret isn't stored would fail every sign-in: take it back.
            if let Err(remove) = store.oidc_provider_remove(&id).await {
                error!(error = %remove, "cannot remove the identity provider whose secret wasn't stored");
            }
            error!(provider = %id, error = %err, "cannot store an identity provider's client secret");
            return error_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Something went wrong",
                "Doozers couldn't store the client secret, so it didn't add the provider. The error \
                 is in its log.",
            );
        }
    }
    Redirect::to(&format!(
        "/admin/identity-providers?added={}#provider-{id}",
        encode(&added.display_name)
    ))
    .into_response()
}

/// `POST /admin/identity-providers/<id>` (CSRF-checked): replaces the provider's settings. The id
/// can't change. A blank secret keeps the stored one, and "Remove the stored secret" deletes it.
async fn update(
    State(pages): State<OidcPages>,
    user: WebUser,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(mut form): Form<ProviderForm>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let provider = match stored_provider(&pages, &id).await {
        Ok(provider) => provider,
        Err(response) => return *response,
    };
    form.id = id.clone();
    let stored = provider.client_secret.is_some();
    let again = |error: String, status| {
        render_form(
            &pages,
            &user,
            FormView::of(&form),
            true,
            stored,
            Some(error),
            status,
        )
    };
    let typed = (!form.client_secret.is_empty()).then(|| Secret::new(form.client_secret.clone()));
    let clear = typed.is_none() && form.clear_secret.is_some();
    let name = if typed.is_some() {
        Some(secret_name(&id))
    } else if clear {
        None
    } else {
        provider.client_secret.clone()
    };
    let new = match settings(&form, name.clone()) {
        Ok(new) => new,
        Err(why) => return again(why, StatusCode::BAD_REQUEST),
    };
    if let Err(why) = check_provider(&pages, &new.issuer).await {
        return again(why, StatusCode::UNPROCESSABLE_ENTITY);
    }
    // The row first: the settings have been checked, so only a provider that was removed since
    // the form was shown refuses it, and then no secret is written for it.
    let store = &pages.app.inner.store;
    let previous = settings_of(&provider);
    let saved = match store
        .oidc_provider_update(&id, new, pages.app.inner.clock.now())
        .await
    {
        Ok(Some(saved)) => saved,
        Ok(None) => return no_such_provider(),
        Err(doozers_store::Error::InvalidRequest(why)) => {
            return again(sentence(&why), StatusCode::BAD_REQUEST);
        }
        Err(err) => return internal_error("cannot change an identity provider", &err),
    };
    if let (Some(secret), Some(name)) = (&typed, &name) {
        let stored = match secret_path(name) {
            Some(path) => pages
                .secrets
                .put(&path, secret, Some(&user.user))
                .await
                .map(|_| ())
                .map_err(|err| err.to_string()),
            None => Err(format!("{name} is not a secret name")),
        };
        if let Err(err) = stored {
            error!(provider = %id, error = %err, "cannot store an identity provider's client secret");
            // Back to what it was, so the provider doesn't name a secret that isn't there.
            if let Err(revert) = store
                .oidc_provider_update(&id, previous, pages.app.inner.clock.now())
                .await
            {
                error!(provider = %id, error = %revert, "cannot restore the identity provider whose secret wasn't stored");
            }
            return error_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Something went wrong",
                "Doozers couldn't store the client secret, so it didn't save the provider. The \
                 error is in its log.",
            );
        }
    }
    if clear
        && let Some(path) = provider.client_secret.as_deref().and_then(secret_path)
        && let Err(err) = pages.secrets.delete(&path).await
    {
        warn!(provider = %id, error = %err, "cannot delete an identity provider's client secret");
    }
    Redirect::to(&format!(
        "/admin/identity-providers?saved={}#provider-{id}",
        encode(&saved.display_name)
    ))
    .into_response()
}

/// `value` as a query value.
fn encode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

/// The fields of the one-button forms.
#[derive(Debug, Default, Deserialize)]
struct ActionForm {
    #[serde(default)]
    csrf: Option<String>,
    #[serde(default)]
    enabled: String,
}

/// `POST /admin/identity-providers/<id>/enabled` (CSRF-checked): enables or disables the provider
/// (`enabled=true` or `false`), then 303 to the list. Nothing is fetched: a provider can be turned
/// off while it is down.
async fn set_enabled(
    State(pages): State<OidcPages>,
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
    let enabled = match form.enabled.as_str() {
        "true" => true,
        "false" => false,
        _ => {
            return error_page(
                StatusCode::BAD_REQUEST,
                "Not allowed",
                "Say whether to enable or disable the provider.",
            );
        }
    };
    if !is_provider_id(&id) {
        return no_such_provider();
    }
    match pages
        .app
        .inner
        .store
        .oidc_provider_set_enabled(&id, enabled, pages.app.inner.clock.now())
        .await
    {
        Ok(true) => {
            Redirect::to(&format!("/admin/identity-providers#provider-{id}")).into_response()
        }
        Ok(false) => no_such_provider(),
        Err(err) => internal_error("cannot change an identity provider", &err),
    }
}

#[derive(Template)]
#[template(path = "web/admin_oidc_remove.html")]
struct RemovePage<'a> {
    shell: Shell,
    csrf: &'a str,
    provider: OidcProvider,
    connected: String,
}

/// `GET /admin/identity-providers/<id>/remove`: asks first, saying what goes with the provider.
async fn remove_page(
    State(pages): State<OidcPages>,
    user: WebUser,
    Path(id): Path<String>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    let provider = match stored_provider(&pages, &id).await {
        Ok(provider) => provider,
        Err(response) => return *response,
    };
    let connected = match pages
        .app
        .inner
        .store
        .oidc_provider_connection_count(&provider.id)
        .await
    {
        Ok(count) => connected_text(count),
        Err(err) => return internal_error("cannot count the connected accounts", &err),
    };
    http::render(&RemovePage {
        shell: Shell::new(&user, Nav::AdminIdentityProviders),
        csrf: &user.csrf,
        provider,
        connected,
    })
}

/// `POST /admin/identity-providers/<id>/remove` (CSRF-checked): removes the provider, its
/// connections, and its client secret, then 303 to the list.
async fn remove(
    State(pages): State<OidcPages>,
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
    let provider = match stored_provider(&pages, &id).await {
        Ok(provider) => provider,
        Err(response) => return *response,
    };
    match pages.app.inner.store.oidc_provider_remove(&id).await {
        Ok(true) => {}
        Ok(false) => return no_such_provider(),
        Err(err) => return internal_error("cannot remove an identity provider", &err),
    }
    if let Some(path) = provider.client_secret.as_deref().and_then(secret_path)
        && let Err(err) = pages.secrets.delete(&path).await
    {
        warn!(provider = %id, error = %err, "cannot delete a removed identity provider's client secret");
    }
    Redirect::to(&format!(
        "/admin/identity-providers?removed={}",
        encode(&provider.display_name)
    ))
    .into_response()
}

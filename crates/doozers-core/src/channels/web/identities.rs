//! The Identities page, `/account/identities`: the channel identities of the signed-in user, the
//! form that makes a link code for a channel, and the changes a user makes to their own identities
//! (the default, and removing one). The service behind it is [`Links`]; the design is in
//! `Docs/technical/web-channel.md#identities-page` and
//! `Docs/technical/identity-and-routing.md#linking-an-identity`.
//!
//! Like the other pages: behind the login, every answer `no-store`, every change a CSRF-checked form
//! post, and every form working without JavaScript. Everything is the signed-in user's own: no
//! path or field names another user. The page that makes a code is the one place the code is shown,
//! in the answer to the post that made it, so it is never in a URL or a redirect, and the list
//! afterwards says only whether a code is pending.
//!
//! The path is under `/account`, and the [account page](super::account) links to it.

use std::sync::Arc;

use askama::Template;
use axum::Router;
use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{from_fn_with_state, map_response};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use doozers_store::Verification;
use serde::Deserialize;
use tracing::error;

use super::auth::{WebUser, check_csrf};
use super::pages::{WebApp, error_page, no_store};
use super::shell::{Nav, Shell};
use super::toolsets::when;
use crate::config::Identity;
use crate::http::{self, RouterBuilder};
use crate::links::{CodeState, LinkCodeInfo, LinkError, Links, NewLinkCode, is_web_identity};
use crate::users::{User, UserError};

#[derive(Clone)]
struct IdentityPages {
    links: Arc<Links>,
}

impl WebApp {
    /// Adds the Identities page to `builder`, linking identities with `links`, behind the same
    /// authentication middleware as the chat pages. Every answer is `no-store`.
    pub fn install_identities(&self, builder: RouterBuilder, links: Arc<Links>) -> RouterBuilder {
        let auth = from_fn_with_state(self.clone(), super::login::authenticate);
        builder.merge(
            Router::new()
                .route("/account/identities", get(list_page))
                .route("/account/identities/link", post(start_link))
                .route("/account/identities/default", post(set_default))
                .route("/account/identities/remove", get(remove_page).post(remove))
                .with_state(IdentityPages { links })
                .route_layer(auth)
                .route_layer(map_response(async |response: Response| no_store(response))),
        )
    }
}

/// An identity on the page.
struct IdentityRow {
    text: String,
    /// `text` as a query value, for the link to the page that asks before removing it.
    query: String,
    how: &'static str,
    added: String,
    is_default: bool,
    removable: bool,
}

impl IdentityRow {
    fn of(user: &User, identity: &crate::users::UserIdentity) -> Self {
        let text = identity.identity.as_str().to_owned();
        let web = is_web_identity(&text);
        let how = match identity.verification {
            Verification::Link => "Linked by you with a code",
            Verification::Imported => "Imported from doozers.toml",
            Verification::Admin if web && text == format!("web:{}", user.id) => {
                "Signs you in to the web chat"
            }
            Verification::Admin if web => "Set by an admin for the trusted proxy",
            Verification::Admin => "Set by an admin",
        };
        Self {
            query: url::form_urlencoded::byte_serialize(text.as_bytes()).collect(),
            how,
            added: when(identity.verified_at),
            is_default: user.default_identity.as_ref() == Some(&identity.identity),
            removable: !web,
            text,
        }
    }
}

/// A code on the page, without the code.
struct CodeRow {
    namespace: String,
    state: &'static str,
    pill: &'static str,
    detail: String,
}

impl From<LinkCodeInfo> for CodeRow {
    fn from(code: LinkCodeInfo) -> Self {
        let (state, pill, detail) = match code.state {
            CodeState::Pending => (
                "Pending",
                "pill-ok",
                format!("works until {}, once", when(code.expires_at)),
            ),
            CodeState::Expired => (
                "Expired",
                "pill-neutral",
                format!("ran out at {}", when(code.expires_at)),
            ),
            CodeState::Cancelled => (
                "Cancelled",
                "pill-warn",
                "too many wrong codes were sent, so it stopped working".to_owned(),
            ),
        };
        Self {
            namespace: code.namespace,
            state,
            pill,
            detail,
        }
    }
}

/// The code just made, shown once.
struct NewCodeView {
    namespace: String,
    command: String,
    expires: String,
}

impl From<NewLinkCode> for NewCodeView {
    fn from(code: NewLinkCode) -> Self {
        Self {
            command: code.command(),
            expires: when(code.expires_at),
            namespace: code.namespace,
        }
    }
}

/// What the page shows besides the identities and the codes.
#[derive(Default)]
struct PageState {
    notice: Option<String>,
    error: Option<String>,
    new_code: Option<NewCodeView>,
    /// The channel the form has selected.
    selected: String,
}

#[derive(Template)]
#[template(path = "web/identities.html")]
struct ListPage<'a> {
    shell: Shell,
    csrf: &'a str,
    identities: Vec<IdentityRow>,
    codes: Vec<CodeRow>,
    namespaces: Vec<String>,
    selected: String,
    notice: Option<String>,
    error: Option<String>,
    new_code: Option<NewCodeView>,
}

#[derive(Template)]
#[template(path = "web/identity_remove.html")]
struct RemovePage<'a> {
    shell: Shell,
    csrf: &'a str,
    identity: &'a str,
    is_default: bool,
}

/// `?notice=<what happened>`, after a change.
#[derive(Deserialize)]
struct Notice {
    notice: Option<String>,
}

/// The sentence for `?notice=`: only the values the pages redirect with.
fn notice_text(notice: Option<&str>) -> Option<String> {
    let text = match notice? {
        "default" => "Changed your default identity.",
        "removed" => "Removed the identity.",
        _ => return None,
    };
    Some(text.to_owned())
}

/// The page for a call that failed for a reason the user can't fix by editing the form.
fn failed(err: &LinkError) -> Response {
    match err {
        LinkError::UserUnavailable => error_page(
            StatusCode::FORBIDDEN,
            "Not available",
            "Your account can't change its identities.",
        ),
        err => {
            error!(error = %err, "changing the identities failed");
            error_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Something went wrong",
                "Doozers couldn't use your identities. The error is in its log.",
            )
        }
    }
}

/// Renders the Identities page for `user` with `state`, answering `status`.
async fn render(
    pages: &IdentityPages,
    user: &WebUser,
    status: StatusCode,
    state: PageState,
) -> Response {
    let Some(account) = pages.links.user(&user.user) else {
        return failed(&LinkError::UserUnavailable);
    };
    let codes = match pages.links.codes(&user.user).await {
        Ok(codes) => codes,
        Err(err) => return failed(&err),
    };
    let namespaces = pages.links.namespaces();
    let selected = if namespaces.contains(&state.selected) {
        state.selected
    } else {
        namespaces.first().cloned().unwrap_or_default()
    };
    let mut response = http::render(&ListPage {
        shell: Shell::new(user, Nav::Account),
        csrf: &user.csrf,
        identities: account
            .identities
            .iter()
            .map(|identity| IdentityRow::of(&account, identity))
            .collect(),
        codes: codes.into_iter().map(CodeRow::from).collect(),
        namespaces,
        selected,
        notice: state.notice,
        error: state.error,
        new_code: state.new_code,
    });
    if response.status().is_success() {
        *response.status_mut() = status;
    }
    response
}

/// `GET /account/identities`: the user's identities, the form to add one, and the codes made.
async fn list_page(
    State(pages): State<IdentityPages>,
    user: WebUser,
    Query(notice): Query<Notice>,
) -> Response {
    let state = PageState {
        notice: notice_text(notice.notice.as_deref()),
        ..PageState::default()
    };
    render(&pages, &user, StatusCode::OK, state).await
}

#[derive(Debug, Default, Deserialize)]
struct LinkForm {
    #[serde(default)]
    csrf: Option<String>,
    #[serde(default)]
    namespace: String,
}

/// `POST /account/identities/link` (CSRF-checked): makes a code for the chosen channel and shows it
/// in the answer, once. A channel that isn't running is refused with the ones that are.
async fn start_link(
    State(pages): State<IdentityPages>,
    user: WebUser,
    headers: HeaderMap,
    Form(form): Form<LinkForm>,
) -> Response {
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    match pages.links.start(&user.user, &form.namespace).await {
        Ok(code) => {
            let state = PageState {
                selected: code.namespace.clone(),
                new_code: Some(code.into()),
                ..PageState::default()
            };
            render(&pages, &user, StatusCode::OK, state).await
        }
        Err(err @ (LinkError::UnknownNamespace { .. } | LinkError::WebNamespace)) => {
            let state = PageState {
                error: Some(err.to_string()),
                selected: form.namespace,
                ..PageState::default()
            };
            render(&pages, &user, StatusCode::BAD_REQUEST, state).await
        }
        Err(err) => failed(&err),
    }
}

#[derive(Debug, Default, Deserialize)]
struct IdentityForm {
    #[serde(default)]
    csrf: Option<String>,
    #[serde(default)]
    identity: String,
}

/// The identity a form or link names, if it is one.
fn identity_of(text: &str) -> Option<Identity> {
    Identity::new(text).ok()
}

fn no_such_identity() -> Response {
    error_page(
        StatusCode::NOT_FOUND,
        "No such identity",
        "You have no such identity. It may have been removed.",
    )
}

/// `POST /account/identities/default` (CSRF-checked): makes one of the user's identities the
/// default, then 303 to the page.
async fn set_default(
    State(pages): State<IdentityPages>,
    user: WebUser,
    headers: HeaderMap,
    Form(form): Form<IdentityForm>,
) -> Response {
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let Some(identity) = identity_of(&form.identity) else {
        return no_such_identity();
    };
    match pages
        .links
        .set_default_identity(&user.user, &identity)
        .await
    {
        Ok(_) => Redirect::to("/account/identities?notice=default").into_response(),
        Err(LinkError::Users(UserError::Invalid(_))) => no_such_identity(),
        Err(err) => failed(&err),
    }
}

/// `?identity=` on the page that asks before removing one.
#[derive(Deserialize)]
struct RemoveQuery {
    #[serde(default)]
    identity: String,
}

/// `GET /account/identities/remove?identity=`: asks before removing one of the user's identities.
/// An identity the user doesn't have, or the web sign-in, is a 404.
async fn remove_page(
    State(pages): State<IdentityPages>,
    user: WebUser,
    Query(query): Query<RemoveQuery>,
) -> Response {
    let Some(account) = pages.links.user(&user.user) else {
        return failed(&LinkError::UserUnavailable);
    };
    let Some(identity) = identity_of(&query.identity) else {
        return no_such_identity();
    };
    let Some(found) = account.identities.iter().find(|i| i.identity == identity) else {
        return no_such_identity();
    };
    if is_web_identity(found.identity.as_str()) {
        return no_such_identity();
    }
    http::render(&RemovePage {
        shell: Shell::new(&user, Nav::Account),
        csrf: &user.csrf,
        identity: identity.as_str(),
        is_default: account.default_identity.as_ref() == Some(&identity),
    })
}

/// `POST /account/identities/remove` (CSRF-checked): removes one of the user's identities, then
/// 303 to the page. The web sign-in can't be removed.
async fn remove(
    State(pages): State<IdentityPages>,
    user: WebUser,
    headers: HeaderMap,
    Form(form): Form<IdentityForm>,
) -> Response {
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let Some(identity) = identity_of(&form.identity) else {
        return no_such_identity();
    };
    match pages.links.remove_identity(&user.user, &identity).await {
        Ok(_) => Redirect::to("/account/identities?notice=removed").into_response(),
        Err(LinkError::WebNamespace | LinkError::Users(UserError::Invalid(_))) => {
            no_such_identity()
        }
        Err(err) => failed(&err),
    }
}

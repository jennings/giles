//! The Account page, `/account`: what a signed-in user manages about themselves. Design:
//! `Docs/technical/web-channel.md#account-page`.
//!
//! The page is the user's profile (display name and time zone), their ways to sign in
//! ([`account_methods`](super::account_methods)), a summary of their channel identities, which
//! [`identities`](super::identities) manages, and their logins
//! ([`account_sessions`](super::account_sessions)). Like the other pages: behind the
//! login, every answer `no-store`, every change a CSRF-checked form post, and every form working
//! without JavaScript. Everything is the signed-in user's own: no path or field names another user.

use std::sync::Arc;

use askama::Template;
use axum::Router;
use axum::extract::{Form, FromRef, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{from_fn_with_state, map_response};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use doozers_store::Role;
use serde::Deserialize;
use tracing::error;

use super::account_methods::MethodsView;
use super::account_sessions::SessionRow;
use super::auth::{WebUser, check_csrf};
use super::pages::{WebApp, error_page, internal_error, no_store};
use super::shell::{Nav, Shell};
use crate::http::{self, RouterBuilder};
use crate::users::{MAX_DISPLAY_NAME_CHARS, User, UserError, Users};

/// What the account routes need: the web app and the users, whose profile a user edits.
#[derive(Clone)]
pub(super) struct AccountPages {
    pub(super) app: WebApp,
    users: Arc<Users>,
}

impl AccountPages {
    /// The page again with `error` above the forms (400), for a form that was refused.
    pub(super) async fn render_error(&self, user: &WebUser, error: &str) -> Response {
        let state = PageState {
            error: Some(error.to_owned()),
            ..PageState::default()
        };
        render(&self.app, user, StatusCode::BAD_REQUEST, state).await
    }
}

impl FromRef<AccountPages> for WebApp {
    fn from_ref(pages: &AccountPages) -> Self {
        pages.app.clone()
    }
}

impl WebApp {
    /// Adds the Account page to `builder`, changing profiles through `users`, behind the same
    /// authentication middleware as the chat pages. Every answer is `no-store`.
    pub fn install_account(&self, builder: RouterBuilder, users: Arc<Users>) -> RouterBuilder {
        let auth = from_fn_with_state(self.clone(), super::login::authenticate);
        builder.merge(
            Router::new()
                .route("/account", get(page))
                .route("/account/profile", post(save_profile))
                .route(
                    "/account/sign-in/remove",
                    get(super::account_methods::remove_page).post(super::account_methods::remove),
                )
                .route(
                    "/account/sessions/{id}/revoke",
                    post(super::account_sessions::revoke),
                )
                .route(
                    "/account/sign-in/passkey/rename",
                    post(super::account_methods::rename_passkey),
                )
                .with_state(AccountPages {
                    app: self.clone(),
                    users,
                })
                .route_layer(auth)
                .route_layer(map_response(async |response: Response| no_store(response))),
        )
    }
}

/// A channel identity on the page.
struct IdentityRow {
    text: String,
    is_default: bool,
}

/// The profile form's fields.
#[derive(Default)]
struct ProfileForm {
    display_name: String,
    timezone: String,
}

impl ProfileForm {
    fn of(user: &User) -> Self {
        Self {
            display_name: user.display_name.clone(),
            timezone: user
                .timezone
                .as_ref()
                .and_then(|zone| zone.iana_name())
                .unwrap_or_default()
                .to_owned(),
        }
    }
}

/// What the page shows besides the user's own data.
#[derive(Default)]
struct PageState {
    notice: Option<String>,
    error: Option<String>,
    /// The profile fields to show instead of the stored ones, after a save that was refused.
    typed: Option<ProfileForm>,
}

#[derive(Template)]
#[template(path = "web/account.html")]
struct AccountPage<'a> {
    shell: Shell,
    csrf: &'a str,
    notice: Option<String>,
    error: Option<String>,
    username: &'a str,
    role: &'static str,
    display_name: String,
    timezone: String,
    max_display_name: usize,
    methods: MethodsView,
    sessions: Vec<SessionRow>,
    identities: Vec<IdentityRow>,
}

/// `?notice=<what happened>`, after a change.
#[derive(Deserialize)]
struct Notice {
    notice: Option<String>,
}

/// The sentence for `?notice=`: only the values the pages redirect with.
fn notice_text(notice: Option<&str>) -> Option<String> {
    let text = match notice? {
        "saved" => "Saved your profile.",
        "removed" => "Removed the way to sign in.",
        "renamed" => "Renamed the passkey.",
        "revoked" => "Ended that login.",
        "connected" => "Connected your account.",
        "reverified" => {
            "You confirmed it's you. For the next 10 minutes you can add or remove a way to \
             sign in, or change your password."
        }
        _ => return None,
    };
    Some(text.to_owned())
}

fn unavailable() -> Response {
    error_page(
        StatusCode::FORBIDDEN,
        "Not available",
        "Your account can't use this page.",
    )
}

/// Renders the page for `user` with `state`, answering `status`.
async fn render(app: &WebApp, user: &WebUser, status: StatusCode, state: PageState) -> Response {
    let methods = match app.methods_view(user).await {
        Ok(methods) => methods,
        Err(err) => return internal_error("cannot look up the sign-in methods", &err),
    };
    let sessions = match app.sessions_view(user).await {
        Ok(sessions) => sessions,
        Err(err) => return internal_error("cannot look up the web sessions", &err),
    };
    let directory = app.inner.channel.users().get();
    let Some(account) = directory.user(&user.user) else {
        return unavailable();
    };
    let typed = state.typed.unwrap_or_else(|| ProfileForm::of(account));
    let mut response = http::render(&AccountPage {
        shell: Shell::new(user, Nav::Account),
        csrf: &user.csrf,
        notice: state.notice,
        error: state.error,
        username: &account.username,
        role: match account.role {
            Role::Admin => "Admin",
            Role::Member => "Member",
        },
        display_name: typed.display_name,
        timezone: typed.timezone,
        max_display_name: MAX_DISPLAY_NAME_CHARS,
        methods,
        sessions,
        identities: account
            .identities
            .iter()
            .map(|identity| IdentityRow {
                text: identity.identity.as_str().to_owned(),
                is_default: account.default_identity.as_ref() == Some(&identity.identity),
            })
            .collect(),
    });
    if response.status().is_success() {
        *response.status_mut() = status;
    }
    response
}

/// `GET /account`.
async fn page(State(app): State<WebApp>, user: WebUser, Query(query): Query<Notice>) -> Response {
    let state = PageState {
        notice: notice_text(query.notice.as_deref()),
        ..PageState::default()
    };
    render(&app, &user, StatusCode::OK, state).await
}

#[derive(Debug, Default, Deserialize)]
struct ProfileFields {
    #[serde(default)]
    csrf: Option<String>,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    timezone: String,
}

/// `POST /account/profile` (CSRF-checked): sets the display name and time zone, then 303 to the
/// page. A name or time zone that isn't valid is the page again with why (400), and changes nothing.
async fn save_profile(
    State(pages): State<AccountPages>,
    user: WebUser,
    headers: HeaderMap,
    Form(form): Form<ProfileFields>,
) -> Response {
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let timezone = form.timezone.trim();
    let result = pages
        .users
        .set_profile(
            &user.user,
            &form.display_name,
            (!timezone.is_empty()).then_some(timezone),
            pages.app.inner.clock.now(),
        )
        .await;
    match result {
        Ok(_) => Redirect::to("/account?notice=saved").into_response(),
        Err(UserError::NoSuchUser(_)) => unavailable(),
        Err(UserError::Invalid(why)) => {
            render(
                &pages.app,
                &user,
                StatusCode::BAD_REQUEST,
                PageState {
                    error: Some(why),
                    typed: Some(ProfileForm {
                        display_name: form.display_name,
                        timezone: form.timezone,
                    }),
                    ..PageState::default()
                },
            )
            .await
        }
        Err(err) => {
            error!(error = %err, "cannot save a profile");
            error_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Something went wrong",
                "Doozers couldn't save your profile. The error is in its log.",
            )
        }
    }
}

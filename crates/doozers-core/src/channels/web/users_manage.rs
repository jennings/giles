//! Managing a user, from the Users pages: **Invite a user**, a user's own page with forms for
//! their details, role, and proxy identity, disabling and enabling them, and a recovery link. Next
//! to the list and **Remove** of [`users`](super::users) and like them: behind the login, admins
//! only, every answer `no-store`, every change a CSRF-checked form post that works without
//! JavaScript. Design: `Docs/technical/web-channel.md#user-pages`.
//!
//! Every handler checks the admin role first, then the CSRF token, then reads the id in the path
//! (an id that isn't a user is a 404). A form that is refused shows the user's page again with why
//! and what was typed, and changes nothing: the checks are [`Users`](crate::users::Users)', which
//! the last-active-admin rule belongs to, so the page only says what it answered. The link a user
//! is invited or recovered with is shown once, in the answer to the `POST` that made it, because
//! only its hash is stored.

use askama::Template;
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use doozers_store::{InviteKind, NewUser, Role, UserState};
use doozers_types::UserId;
use serde::Deserialize;
use tracing::{error, info};

use super::admin::{forbidden, may_administer};
use super::auth::{WebUser, check_csrf};
use super::pages::error_page;
use super::random_hex;
use super::shell::{Nav, Shell};
use super::toolsets::when;
use super::users::{CsrfForm, UserPages, no_such_user, user_id};
use crate::http;
use crate::users::{LoginUserError, MAX_DISPLAY_NAME_CHARS, MAX_USERNAME_CHARS, User, UserError};

/// The reason the last active admin can't be demoted, disabled, or removed.
const LAST_ADMIN: &str = "This is the last active admin, and Doozers keeps one. Make another user \
                          an admin first.";

fn role_name(role: Role) -> &'static str {
    match role {
        Role::Admin => "admin",
        Role::Member => "member",
    }
}

/// The fields of the forms on a user's page, as they are shown: the stored values, or what was
/// typed into a form that was refused.
#[derive(Default)]
struct Fields {
    username: String,
    display_name: String,
    timezone: String,
    role: String,
    proxy_identity: String,
}

impl Fields {
    fn of(user: &User) -> Self {
        Self {
            username: user.username.clone(),
            display_name: user.display_name.clone(),
            timezone: user
                .timezone
                .as_ref()
                .and_then(|zone| zone.iana_name())
                .unwrap_or_default()
                .to_owned(),
            role: role_name(user.role).to_owned(),
            proxy_identity: proxy_native_id(user).unwrap_or_default(),
        }
    }
}

/// The part after `web:` of the user's proxy identity.
fn proxy_native_id(user: &User) -> Option<String> {
    user.proxy_identity()
        .and_then(|held| held.identity.as_str().strip_prefix("web:"))
        .map(str::to_owned)
}

/// What a user's page shows besides the stored user.
#[derive(Default)]
struct Show {
    notice: Option<String>,
    error: Option<String>,
    /// The fields to show instead of the stored ones, after a form that was refused.
    typed: Option<Fields>,
}

#[derive(Template)]
#[template(path = "web/admin_user.html")]
struct UserPage<'a> {
    shell: Shell,
    csrf: &'a str,
    notice: Option<String>,
    error: Option<String>,
    id: String,
    username: &'a str,
    display_name: &'a str,
    /// The user looking at the page.
    you: bool,
    admin: bool,
    role: &'static str,
    active: bool,
    state: &'static str,
    last_sign_in: String,
    form: Fields,
    /// The proxy identity the user has now.
    proxy_identity: Option<String>,
    max_username: usize,
    max_display_name: usize,
}

/// `?notice=<what happened>`, after a change.
#[derive(Debug, Default, Deserialize)]
pub(super) struct Notice {
    #[serde(default)]
    notice: Option<String>,
}

/// The sentence for `?notice=`: only the values the pages redirect with.
fn notice_text(notice: Option<&str>) -> Option<String> {
    let text = match notice? {
        "saved" => "Saved the details.",
        "role" => "Saved the role.",
        "proxy" => "Saved the proxy identity.",
        "proxy-cleared" => "Removed the proxy identity.",
        "disabled" => "Disabled the user. Their logins ended.",
        "enabled" => "Enabled the user. They can sign in again.",
        _ => return None,
    };
    Some(text.to_owned())
}

/// The user in a path, after the checks every request makes: the admin role, and for a post the
/// CSRF token. The error is the page to answer with.
fn gate(
    user: &WebUser,
    post: Option<(&HeaderMap, Option<&str>)>,
    raw: &str,
) -> Result<UserId, Box<Response>> {
    if !may_administer(user) {
        return Err(Box::new(forbidden()));
    }
    if let Some((headers, csrf)) = post
        && let Err(rejected) = check_csrf(user, headers, csrf)
    {
        return Err(Box::new(rejected.into_response()));
    }
    user_id(raw).ok_or_else(|| Box::new(no_such_user()))
}

impl UserPages {
    /// The user `id` in the live list, or the 404.
    fn find(&self, id: &UserId) -> Result<User, Box<Response>> {
        self.app
            .inner
            .channel
            .users()
            .get()
            .user(id)
            .cloned()
            .ok_or_else(|| Box::new(no_such_user()))
    }

    /// When `id` last signed in, as the page says it. The router's list doesn't have it: the
    /// store does, as a session starting writes it there.
    pub(super) async fn last_sign_in(&self, id: &UserId) -> Result<String, Box<Response>> {
        match self.app.inner.store.user(id).await {
            Ok(Some(stored)) => Ok(sign_in_text(stored.last_sign_in_at)),
            Ok(None) => Err(Box::new(no_such_user())),
            Err(err) => Err(Box::new(super::pages::internal_error(
                "cannot read a user",
                &err,
            ))),
        }
    }

    /// The page of `target` answering `status`.
    async fn show(
        &self,
        admin: &WebUser,
        target: &User,
        status: StatusCode,
        show: Show,
    ) -> Response {
        let last_sign_in = match self.last_sign_in(&target.id).await {
            Ok(text) => text,
            Err(response) => return *response,
        };
        let mut response = http::render(&UserPage {
            shell: Shell::new(admin, Nav::Users),
            csrf: &admin.csrf,
            notice: show.notice,
            error: show.error,
            id: target.id.to_string(),
            username: &target.username,
            display_name: &target.display_name,
            you: target.id == admin.user,
            admin: target.role == Role::Admin,
            role: match target.role {
                Role::Admin => "Admin",
                Role::Member => "Member",
            },
            active: target.state == UserState::Active,
            state: match target.state {
                UserState::Active => "Active",
                UserState::Disabled => "Disabled",
            },
            last_sign_in,
            form: show.typed.unwrap_or_else(|| Fields::of(target)),
            proxy_identity: proxy_native_id(target).map(|native| format!("web:{native}")),
            max_username: MAX_USERNAME_CHARS,
            max_display_name: MAX_DISPLAY_NAME_CHARS,
        });
        if response.status().is_success() {
            *response.status_mut() = status;
        }
        response
    }

    /// The page again with `error` above the forms, `typed` in them, and `status`.
    async fn refuse(
        &self,
        admin: &WebUser,
        target: &User,
        status: StatusCode,
        error: String,
        typed: Fields,
    ) -> Response {
        let show = Show {
            error: Some(error),
            typed: Some(typed),
            ..Show::default()
        };
        self.show(admin, target, status, show).await
    }
}

/// When a user last signed in, or **Never**.
pub(super) fn sign_in_text(at: Option<doozers_types::Timestamp>) -> String {
    at.map_or_else(|| "Never".to_owned(), when)
}

/// The page for a change that failed for a reason that isn't the admin's to fix.
fn failed(error: &UserError, what: &str) -> Response {
    error!(%error, "cannot {what}");
    error_page(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Something went wrong",
        &format!("Doozers couldn't {what}. The error is in its log."),
    )
}

/// `GET /admin/users/<id>`: the user's page.
pub(super) async fn user_page(
    State(pages): State<UserPages>,
    user: WebUser,
    Path(id): Path<String>,
    Query(query): Query<Notice>,
) -> Response {
    let id = match gate(&user, None, &id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let target = match pages.find(&id) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    let show = Show {
        notice: notice_text(query.notice.as_deref()),
        ..Show::default()
    };
    pages.show(&user, &target, StatusCode::OK, show).await
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct DetailsForm {
    #[serde(default)]
    csrf: Option<String>,
    #[serde(default)]
    username: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    timezone: String,
}

/// `POST /admin/users/<id>/details` (CSRF-checked): sets the username, display name, and time
/// zone together, then 303 to the page. One that isn't valid, or a username another user has, is
/// the page again with why (400, 409) and what was typed, and changes nothing.
pub(super) async fn save_details(
    State(pages): State<UserPages>,
    user: WebUser,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<DetailsForm>,
) -> Response {
    let id = match gate(&user, Some((&headers, form.csrf.as_deref())), &id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let target = match pages.find(&id) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    let username = form.username.trim();
    let timezone = form.timezone.trim();
    let result = pages
        .users
        .set_details(
            &id,
            username,
            &form.display_name,
            (!timezone.is_empty()).then_some(timezone),
            pages.app.inner.clock.now(),
        )
        .await;
    let typed = || Fields {
        username: form.username.clone(),
        display_name: form.display_name.clone(),
        timezone: form.timezone.clone(),
        ..Fields::of(&target)
    };
    match result {
        Ok(_) => Redirect::to(&format!("/admin/users/{id}?notice=saved")).into_response(),
        Err(UserError::NoSuchUser(_)) => no_such_user(),
        Err(UserError::Invalid(why)) => {
            pages
                .refuse(&user, &target, StatusCode::BAD_REQUEST, why, typed())
                .await
        }
        Err(UserError::UsernameTaken) => {
            let why = "Another user has that username. Usernames are unique, whatever their case.";
            pages
                .refuse(
                    &user,
                    &target,
                    StatusCode::CONFLICT,
                    why.to_owned(),
                    typed(),
                )
                .await
        }
        Err(err) => failed(&err, "save the details"),
    }
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct RoleForm {
    #[serde(default)]
    csrf: Option<String>,
    #[serde(default)]
    role: String,
}

/// `POST /admin/users/<id>/role` (CSRF-checked): makes the user an admin or a member, then 303 to
/// the page, or to `/` for an admin who demoted themselves, who has lost the admin pages. The last
/// active admin can't be demoted: the page again with why (409).
pub(super) async fn save_role(
    State(pages): State<UserPages>,
    user: WebUser,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<RoleForm>,
) -> Response {
    let id = match gate(&user, Some((&headers, form.csrf.as_deref())), &id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let target = match pages.find(&id) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    let typed = || Fields {
        role: form.role.clone(),
        ..Fields::of(&target)
    };
    let role = match form.role.as_str() {
        "admin" => Role::Admin,
        "member" => Role::Member,
        _ => {
            let why = "Choose Member or Admin.".to_owned();
            return pages
                .refuse(&user, &target, StatusCode::BAD_REQUEST, why, typed())
                .await;
        }
    };
    match pages
        .users
        .set_role(&id, role, pages.app.inner.clock.now())
        .await
    {
        Ok(_) => {
            info!(admin = %user.user, user = %id, role = role_name(role), "changed a user's role");
            if id == user.user && role == Role::Member {
                return Redirect::to("/").into_response();
            }
            Redirect::to(&format!("/admin/users/{id}?notice=role")).into_response()
        }
        Err(UserError::NoSuchUser(_)) => no_such_user(),
        Err(UserError::LastAdmin) => {
            pages
                .refuse(
                    &user,
                    &target,
                    StatusCode::CONFLICT,
                    LAST_ADMIN.to_owned(),
                    typed(),
                )
                .await
        }
        Err(err) => failed(&err, "change the role"),
    }
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct ProxyForm {
    #[serde(default)]
    csrf: Option<String>,
    #[serde(default)]
    proxy_identity: String,
}

/// `POST /admin/users/<id>/proxy-identity` (CSRF-checked): sets the identity a trusted proxy logs
/// the user in as, or removes it when the field is empty, then 303 to the page. One that isn't
/// valid or that another user has is the page again with why (400, 409), and the user keeps the
/// one they had.
pub(super) async fn save_proxy_identity(
    State(pages): State<UserPages>,
    user: WebUser,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<ProxyForm>,
) -> Response {
    let id = match gate(&user, Some((&headers, form.csrf.as_deref())), &id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let target = match pages.find(&id) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    let native_id = form.proxy_identity.trim();
    let native_id = (!native_id.is_empty()).then_some(native_id);
    let typed = || Fields {
        proxy_identity: form.proxy_identity.clone(),
        ..Fields::of(&target)
    };
    // The field is what follows `web:`, so a value that starts with it would store `web:web:...`.
    if native_id.is_some_and(|value| value.to_ascii_lowercase().starts_with("web:")) {
        let why = "Leave off web:. Enter what the proxy sends, such as alice@example.com.";
        return pages
            .refuse(
                &user,
                &target,
                StatusCode::BAD_REQUEST,
                why.to_owned(),
                typed(),
            )
            .await;
    }
    match pages
        .users
        .set_proxy_identity(&id, native_id, pages.app.inner.clock.now())
        .await
    {
        Ok(_) => {
            info!(admin = %user.user, user = %id, "changed a user's proxy identity");
            let notice = if native_id.is_some() {
                "proxy"
            } else {
                "proxy-cleared"
            };
            Redirect::to(&format!("/admin/users/{id}?notice={notice}")).into_response()
        }
        Err(UserError::NoSuchUser(_)) => no_such_user(),
        Err(UserError::Invalid(why)) => {
            pages
                .refuse(&user, &target, StatusCode::BAD_REQUEST, why, typed())
                .await
        }
        Err(UserError::IdentityTaken(taken)) => {
            let why = format!("Another user has the identity {taken}.");
            pages
                .refuse(&user, &target, StatusCode::CONFLICT, why, typed())
                .await
        }
        Err(err) => failed(&err, "save the proxy identity"),
    }
}

/// `POST /admin/users/<id>/disable` (CSRF-checked): disables the user and ends every login they
/// have, their sessions and unspent links, then 303 to the page. Their open streams end with them
/// (`WebApp::login_ended`), their messages are dropped by the router, queued ones by the session
/// driver, and their scheduled jobs wait. An admin can't disable themselves, and the last active
/// admin can't be disabled: the page again with why (409).
pub(super) async fn disable(
    State(pages): State<UserPages>,
    user: WebUser,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<CsrfForm>,
) -> Response {
    let id = match gate(&user, Some((&headers, form.csrf.as_deref())), &id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let target = match pages.find(&id) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    if id == user.user {
        let why = "You can't disable yourself. Ask another admin to disable your account.";
        return pages
            .refuse(
                &user,
                &target,
                StatusCode::CONFLICT,
                why.to_owned(),
                Fields::of(&target),
            )
            .await;
    }
    match pages
        .users
        .set_state(&id, UserState::Disabled, pages.app.inner.clock.now())
        .await
    {
        Ok(_) => {}
        Err(UserError::NoSuchUser(_)) => return no_such_user(),
        Err(UserError::LastAdmin) => {
            return pages
                .refuse(
                    &user,
                    &target,
                    StatusCode::CONFLICT,
                    LAST_ADMIN.to_owned(),
                    Fields::of(&target),
                )
                .await;
        }
        Err(err) => return failed(&err, "disable the user"),
    }
    // The user is disabled, which already closes every login of theirs on its next request and
    // refuses a new session (`Store::web_session_create`), so a failure here leaves them locked
    // out. Enabling them deletes what is left first.
    match pages.app.inner.store.user_logins_revoke(&id).await {
        Ok((logins, links)) => {
            info!(admin = %user.user, user = %id, logins, links, "disabled a user");
            Redirect::to(&format!("/admin/users/{id}?notice=disabled")).into_response()
        }
        Err(err) => {
            error!(%err, user = %id, "cannot delete the logins of a disabled user");
            error_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "User disabled, logins not ended",
                "Doozers disabled the user, who can't sign in or use a login now, but couldn't \
                 delete their logins and links. Enabling the user deletes what is left first. \
                 The error is in the log.",
            )
        }
    }
}

/// `POST /admin/users/<id>/enable` (CSRF-checked): enables a disabled user, then 303 to the page.
/// Before it does, it deletes any login or link a failed disable left, so enabling brings none
/// back: the user signs in again. The change to the users wakes the scheduler, which starts the
/// jobs that waited.
pub(super) async fn enable(
    State(pages): State<UserPages>,
    user: WebUser,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<CsrfForm>,
) -> Response {
    let id = match gate(&user, Some((&headers, form.csrf.as_deref())), &id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let target = match pages.find(&id) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    if !target.is_active()
        && let Err(err) = pages.app.inner.store.user_logins_revoke(&id).await
    {
        error!(%err, user = %id, "cannot delete the logins left on a user being enabled");
        return error_page(
            StatusCode::INTERNAL_SERVER_ERROR,
            "User not enabled",
            "Doozers couldn't delete the logins the user had, which would work again if it \
             enabled them. The user is still disabled. The error is in the log.",
        );
    }
    match pages
        .users
        .set_state(&id, UserState::Active, pages.app.inner.clock.now())
        .await
    {
        Ok(_) => {
            info!(admin = %user.user, user = %id, "enabled a user");
            Redirect::to(&format!("/admin/users/{id}?notice=enabled")).into_response()
        }
        Err(UserError::NoSuchUser(_)) => no_such_user(),
        Err(err) => failed(&err, "enable the user"),
    }
}

#[derive(Template)]
#[template(path = "web/admin_user_link.html")]
struct LinkPage<'a> {
    shell: Shell,
    title: &'static str,
    id: String,
    username: &'a str,
    display_name: &'a str,
    url: String,
    expires: String,
    recovery: bool,
}

/// The page that shows a link once: `kind`'s link for `target`, which `invite` made.
fn link_page(
    admin: &WebUser,
    pages: &UserPages,
    target: &User,
    kind: InviteKind,
    invite: &crate::users::NewInvite,
) -> Response {
    let recovery = kind == InviteKind::Recovery;
    http::render(&LinkPage {
        shell: Shell::new(admin, Nav::Users),
        title: if recovery {
            "Recovery link"
        } else {
            "Invite link"
        },
        id: target.id.to_string(),
        username: &target.username,
        display_name: &target.display_name,
        url: pages.app.inner.channel.invite_url(&invite.token),
        expires: when(invite.expires_at),
        recovery,
    })
}

/// Why a link can't be made, as the user's page says it.
fn link_refusal(error: &LoginUserError, target: &User) -> Option<String> {
    match error {
        LoginUserError::Disabled(_) => Some(format!(
            "{} is disabled, and a disabled user can't sign in. Enable them first.",
            target.username
        )),
        LoginUserError::WebIdentityTaken { user, owner } => Some(format!(
            "The identity web:{user} belongs to the user {owner}, so {} can't sign in to the web \
             chat.",
            target.username
        )),
        _ => None,
    }
}

/// `POST /admin/users/<id>/recovery-link` (CSRF-checked): makes a recovery link for the user, as
/// `doozers admin login-link --user` does, and shows it once. A disabled user can't have one: the
/// page again with why (409).
pub(super) async fn recovery_link(
    State(pages): State<UserPages>,
    user: WebUser,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<CsrfForm>,
) -> Response {
    let id = match gate(&user, Some((&headers, form.csrf.as_deref())), &id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let target = match pages.find(&id) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    let made = pages
        .users
        .create_invite(
            &id,
            InviteKind::Recovery,
            Some(&user.user),
            pages.app.inner.clock.now(),
        )
        .await;
    match made {
        Ok(invite) => {
            info!(admin = %user.user, user = %id, "made a recovery link on the Users pages");
            link_page(&user, &pages, &target, InviteKind::Recovery, &invite)
        }
        Err(LoginUserError::NoSuchUser(_)) => no_such_user(),
        Err(err) => match link_refusal(&err, &target) {
            Some(why) => {
                pages
                    .refuse(
                        &user,
                        &target,
                        StatusCode::CONFLICT,
                        why,
                        Fields::of(&target),
                    )
                    .await
            }
            None => {
                error!(%err, "cannot make a recovery link");
                error_page(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Something went wrong",
                    "Doozers couldn't make the link. The error is in its log.",
                )
            }
        },
    }
}

#[derive(Template)]
#[template(path = "web/admin_user_invite.html")]
struct InvitePage<'a> {
    shell: Shell,
    csrf: &'a str,
    error: Option<String>,
    form: InviteFields,
    max_username: usize,
    max_display_name: usize,
}

/// The invite form's fields.
struct InviteFields {
    username: String,
    display_name: String,
    role: String,
}

impl Default for InviteFields {
    fn default() -> Self {
        Self {
            username: String::new(),
            display_name: String::new(),
            role: "member".to_owned(),
        }
    }
}

fn invite_form(
    user: &WebUser,
    status: StatusCode,
    error: Option<String>,
    form: InviteFields,
) -> Response {
    let mut response = http::render(&InvitePage {
        shell: Shell::new(user, Nav::Users),
        csrf: &user.csrf,
        error,
        form,
        max_username: MAX_USERNAME_CHARS,
        max_display_name: MAX_DISPLAY_NAME_CHARS,
    });
    if response.status().is_success() {
        *response.status_mut() = status;
    }
    response
}

/// `GET /admin/invite-user`: the form.
pub(super) async fn invite_page(user: WebUser) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    invite_form(&user, StatusCode::OK, None, InviteFields::default())
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct InviteForm {
    #[serde(default)]
    csrf: Option<String>,
    #[serde(default)]
    username: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    role: String,
}

/// `POST /admin/invite-user` (CSRF-checked): creates the user, with a fresh random id that no
/// user has had, and an invite link, which the answer shows once. A username that isn't valid or
/// that another user has, or a role that isn't one, is the form again with why (400, 409) and what
/// was typed, and creates nothing.
pub(super) async fn invite(
    State(pages): State<UserPages>,
    user: WebUser,
    headers: HeaderMap,
    Form(form): Form<InviteForm>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let typed = || InviteFields {
        username: form.username.clone(),
        display_name: form.display_name.clone(),
        role: form.role.clone(),
    };
    let role = match form.role.as_str() {
        "admin" => Role::Admin,
        "member" => Role::Member,
        _ => {
            let why = Some("Choose Member or Admin.".to_owned());
            return invite_form(&user, StatusCode::BAD_REQUEST, why, typed());
        }
    };
    let id = UserId::new(random_hex(16)).expect("32 hex digits are a valid user id");
    let now = pages.app.inner.clock.now();
    let new = NewUser {
        id: id.clone(),
        username: form.username.trim().to_owned(),
        display_name: form.display_name.trim().to_owned(),
        role,
        timezone: None,
        default_identity: None,
    };
    let created = match pages.users.create(new, Vec::new(), now).await {
        Ok(created) => created,
        Err(UserError::Invalid(why)) => {
            return invite_form(&user, StatusCode::BAD_REQUEST, Some(why), typed());
        }
        Err(UserError::UsernameTaken) => {
            let why = "Another user has that username. Usernames are unique, whatever their case.";
            return invite_form(&user, StatusCode::CONFLICT, Some(why.to_owned()), typed());
        }
        Err(err) => return failed(&err, "create the user"),
    };
    info!(admin = %user.user, user = %id, role = role_name(role), "invited a user");
    match pages
        .users
        .create_invite(&id, InviteKind::Invite, Some(&user.user), now)
        .await
    {
        Ok(link) => link_page(&user, &pages, &created, InviteKind::Invite, &link),
        Err(err) => {
            error!(%err, user = %id, "cannot make the invite link of a new user");
            error_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "User created without a link",
                &format!(
                    "Doozers created the user {} but couldn't make their link. Open the user's \
                     page and make a recovery link instead. The error is in the log.",
                    created.username
                ),
            )
        }
    }
}

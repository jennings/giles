//! The sign-in methods part of the Account page: what the user has (the password, passkeys, and
//! connections to identity providers), renaming a passkey, and removing a method. Design:
//! `Docs/technical/web-channel.md#account-page`.
//!
//! Adding and changing reuse the pages that already do it: `/setup/password`, `/setup/passkey`,
//! and `/setup/oidc/<id>`. The account page links to them, and they send the browser back to it
//! unless the login came from a link, which lands on the setup page. Removing a method needs the
//! same permission as adding one ([`WebApp::sign_in_change`]: a link's session, or one the user
//! re-verified lately), and the store refuses to remove the last method that can sign the user in
//! (`user_sign_in_method_remove`). Which methods count is the store's rule, with passkeys counted
//! only where this install can offer them (the relying party works). The proxy identity is not a
//! method of the user's: an admin sets it, and it works only behind a trusted proxy, so it doesn't
//! make the last method removable.
//!
//! Renaming a passkey needs no confirmation: it changes nothing about how anyone signs in.

use askama::Template;
use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use doozers_store::{Error, PasskeyId, SignInMethodRef};
use serde::Deserialize;

use super::account::AccountPages;
use super::auth::{Login, WebUser, check_csrf};
use super::pages::{WebApp, error_page, error_page_back, internal_error};
use super::passkey_pages::{kind_label, tidy_name};
use super::shell::{Nav, Shell};
use super::signin::{SignInChange, confirm_first};
use super::toolsets::when;
use crate::http;

/// A passkey on the page.
pub(super) struct PasskeyRow {
    pub(super) id: i64,
    pub(super) name: String,
    pub(super) kind: &'static str,
    pub(super) added: String,
    pub(super) last_used: String,
}

/// An identity provider on the page.
pub(super) struct ProviderRow {
    pub(super) id: String,
    pub(super) name: String,
    /// When the user connected an account, if they did.
    pub(super) connected: Option<String>,
    /// Whether the provider is enabled, so the page can connect and sign in with it.
    pub(super) enabled: bool,
    /// Whether the connection can sign the user in: the provider is enabled and still has the
    /// issuer the account was connected under. Meaningful only when `connected` is set.
    pub(super) works: bool,
    /// The link that starts a connection.
    pub(super) connect_href: String,
}

/// What the page shows of the user's sign-in methods.
pub(super) struct MethodsView {
    /// The login came from an invite or recovery link.
    pub(super) link: bool,
    /// The user confirmed it's them lately: until when.
    pub(super) until: Option<String>,
    /// A proxy login, which can't confirm anything.
    pub(super) proxy: bool,
    /// Whether the login may add or remove a method, or change the password, now.
    pub(super) can_change: bool,
    /// When the user set their password, if they have one.
    pub(super) password: Option<String>,
    pub(super) passkeys: Vec<PasskeyRow>,
    /// Why passkeys can't be added or used, if they can't.
    pub(super) passkeys_unavailable: Option<&'static str>,
    pub(super) providers: Vec<ProviderRow>,
    /// Whether exactly one method can sign the user in now.
    pub(super) only_one: bool,
}

impl WebApp {
    /// The user's sign-in methods as the account page shows them.
    pub(super) async fn methods_view(&self, user: &WebUser) -> doozers_store::Result<MethodsView> {
        let store = &self.inner.store;
        let change = self.sign_in_change(user).await?;
        let password = store.user_password(&user.user).await?;
        let passkeys = store.user_passkeys(&user.user).await?;
        let connections = store.user_oidc_connections(&user.user).await?;
        let providers = store.oidc_providers().await?;
        let relying_party = self.inner.passkeys.relying_party();
        let csrf: String = url::form_urlencoded::byte_serialize(user.csrf.as_bytes()).collect();

        let working = store
            .user_sign_in_methods_usable(&user.user, relying_party.is_ok())
            .await?;
        let rows: Vec<ProviderRow> = providers
            .iter()
            .filter_map(|provider| {
                let connection = connections.iter().find(|c| c.provider == provider.id);
                // A provider that is off is shown only for the connection the user has to it.
                if !provider.enabled && connection.is_none() {
                    return None;
                }
                Some(ProviderRow {
                    id: provider.id.clone(),
                    name: provider.display_name.clone(),
                    connected: connection.map(|c| when(c.connected_at)),
                    enabled: provider.enabled,
                    works: provider.enabled
                        && connection.is_some_and(|c| c.issuer == provider.issuer),
                    connect_href: format!("/setup/oidc/{}?csrf={csrf}", provider.id),
                })
            })
            .collect();

        Ok(MethodsView {
            link: change == SignInChange::Link,
            until: match change {
                SignInChange::Reverified { until } => Some(until.strftime("%H:%M UTC").to_string()),
                _ => None,
            },
            proxy: !matches!(user.login, Login::Cookie(_)),
            can_change: change != SignInChange::Refused,
            password: password.map(|stored| when(stored.changed_at)),
            passkeys: passkeys
                .into_iter()
                .map(|passkey| PasskeyRow {
                    id: passkey.id.get(),
                    name: passkey.name,
                    kind: kind_label(passkey.kind),
                    added: when(passkey.created_at),
                    last_used: passkey.last_used_at.map_or("never".to_owned(), when),
                })
                .collect(),
            passkeys_unavailable: relying_party.as_ref().err().map(|why| why.explain()),
            providers: rows,
            only_one: working == 1,
        })
    }
}

/// A method as a form or link names it: `password`, `passkey:<id>`, or `oidc:<provider id>`.
fn parse_method(text: &str) -> Option<SignInMethodRef> {
    if text == "password" {
        return Some(SignInMethodRef::Password);
    }
    let (kind, rest) = text.split_once(':')?;
    match kind {
        "passkey" => rest
            .parse::<i64>()
            .ok()
            .map(|id| SignInMethodRef::Passkey(PasskeyId::new(id))),
        "oidc" if doozers_store::is_provider_id(rest) => Some(SignInMethodRef::Oidc {
            provider: rest.to_owned(),
        }),
        _ => None,
    }
}

fn no_such_method() -> Response {
    error_page(
        StatusCode::NOT_FOUND,
        "No such way to sign in",
        "You have no such way to sign in. It may have been removed.",
    )
}

/// What the remove page says about a method the user has.
struct Described {
    label: String,
    consequence: String,
}

/// Describes `method` of `user`, or `None` if they don't have it.
async fn describe(
    app: &WebApp,
    user: &WebUser,
    method: &SignInMethodRef,
) -> doozers_store::Result<Option<Described>> {
    let store = &app.inner.store;
    Ok(match method {
        SignInMethodRef::Password => store.user_password(&user.user).await?.map(|_| Described {
            label: "your password".to_owned(),
            consequence: "You can no longer log in with your username and password.".to_owned(),
        }),
        SignInMethodRef::Passkey(id) => store
            .user_passkeys(&user.user)
            .await?
            .into_iter()
            .find(|passkey| passkey.id == *id)
            .map(|passkey| Described {
                consequence: format!(
                    "You can no longer log in with {}. Doozers can't delete the passkey from your \
                     device, so delete it there too.",
                    passkey.name
                ),
                label: format!("the passkey {}", passkey.name),
            }),
        SignInMethodRef::Oidc { provider } => {
            let connected = store
                .user_oidc_connections(&user.user)
                .await?
                .into_iter()
                .any(|connection| connection.provider == *provider);
            let name = store
                .oidc_provider(provider)
                .await?
                .map_or_else(|| provider.clone(), |found| found.display_name);
            connected.then(|| Described {
                consequence: format!(
                    "You can no longer sign in with {name}. You can connect it again later."
                ),
                label: format!("your {name} account"),
            })
        }
    })
}

#[derive(Template)]
#[template(path = "web/account_remove.html")]
struct RemovePage<'a> {
    shell: Shell,
    csrf: &'a str,
    method: &'a str,
    label: &'a str,
    consequence: &'a str,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct MethodQuery {
    #[serde(default)]
    method: String,
}

/// `GET /account/sign-in/remove?method=`: asks before removing a method of the user's. It needs
/// the permission to change methods, so the question doesn't invite a click that would be refused.
/// A method the user doesn't have is a 404.
pub(super) async fn remove_page(
    State(pages): State<AccountPages>,
    user: WebUser,
    Query(query): Query<MethodQuery>,
) -> Response {
    let app = &pages.app;
    match app.may_change_sign_in_methods(&user).await {
        Ok(true) => {}
        Ok(false) => return confirm_first(&user, "/account"),
        Err(err) => return internal_error("cannot look up a web session", &err),
    }
    let Some(method) = parse_method(&query.method) else {
        return no_such_method();
    };
    let described = match describe(app, &user, &method).await {
        Ok(Some(described)) => described,
        Ok(None) => return no_such_method(),
        Err(err) => return internal_error("cannot look up a sign-in method", &err),
    };
    http::render(&RemovePage {
        shell: Shell::new(&user, Nav::Account),
        csrf: &user.csrf,
        method: &query.method,
        label: &described.label,
        consequence: &described.consequence,
    })
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct RemoveForm {
    #[serde(default)]
    csrf: Option<String>,
    #[serde(default)]
    method: String,
}

/// `POST /account/sign-in/remove` (CSRF-checked, and allowed like adding a method): removes the
/// method, then 303 to the account page. The only method that can sign the user in is refused
/// (409), and nothing changes. A method the user doesn't have, another user's included, is a 404.
pub(super) async fn remove(
    State(pages): State<AccountPages>,
    user: WebUser,
    headers: HeaderMap,
    Form(form): Form<RemoveForm>,
) -> Response {
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let app = &pages.app;
    match app.may_change_sign_in_methods(&user).await {
        Ok(true) => {}
        Ok(false) => return confirm_first(&user, "/account"),
        Err(err) => return internal_error("cannot look up a web session", &err),
    }
    let Some(method) = parse_method(&form.method) else {
        return no_such_method();
    };
    let passkeys_work = app.inner.passkeys.relying_party().is_ok();
    match app
        .inner
        .store
        .user_sign_in_method_remove(&user.user, &method, passkeys_work)
        .await
    {
        Ok(true) => {
            tracing::info!(user = %user.user, method = %form.method, "removed a sign-in method");
            Redirect::to("/account?notice=removed").into_response()
        }
        Ok(false) => no_such_method(),
        Err(Error::LastSignInMethod) => error_page_back(
            StatusCode::CONFLICT,
            "That's your only way to sign in",
            "Doozers keeps at least one way for you to sign in. Add another way first, then \
             remove this one.",
            "/account",
            "Back to your account",
        ),
        Err(err) => internal_error("cannot remove a sign-in method", &err),
    }
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct RenameForm {
    #[serde(default)]
    csrf: Option<String>,
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
}

/// `POST /account/sign-in/passkey/rename` (CSRF-checked): renames one of the user's passkeys, then
/// 303 to the account page. An empty name is a 400, and a passkey that isn't the user's is a 404.
pub(super) async fn rename_passkey(
    State(pages): State<AccountPages>,
    user: WebUser,
    headers: HeaderMap,
    Form(form): Form<RenameForm>,
) -> Response {
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let Ok(id) = form.id.parse::<i64>().map(PasskeyId::new) else {
        return no_such_method();
    };
    let name = tidy_name(&form.name);
    if name.is_empty() {
        return pages
            .render_error(&user, "Type a name for the passkey.")
            .await;
    }
    match pages
        .app
        .inner
        .store
        .user_passkey_rename(&user.user, id, &name)
        .await
    {
        Ok(true) => Redirect::to("/account?notice=renamed").into_response(),
        Ok(false) => no_such_method(),
        Err(err) => internal_error("cannot rename a passkey", &err),
    }
}

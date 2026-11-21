//! Signing in with OpenID Connect: the three routes a sign-in or a connection goes through. The
//! pages where an admin adds the providers are in [`oidc_admin`](super::oidc_admin).
//! Design: `Docs/technical/web-channel.md`; the protocol is in [`oidc`](super::oidc).
//!
//! - `GET /login/oidc/<id>?nonce=` starts a sign-in from the login page. The link carries the
//!   page's sign-in nonce, which must match the browser's `SameSite=Strict` nonce cookie, so
//!   another site can't start sign-ins in a visitor's browser (they would use up the visitor's,
//!   and everyone behind the same address's, limit on starts).
//! - `GET /setup/oidc/<id>?csrf=` starts a connection from the setup page, for a user who signed
//!   in with an invite or recovery link, the only login that may add a way to sign in.
//! - `GET /auth/oidc/<id>/callback` is where the provider sends the browser back, for both. It is
//!   one address per provider, the one an admin registers there.
//!
//! The starts are `GET`s because they only send the browser on, and the page's `form-action
//! 'self'` would stop a form that redirects elsewhere. A start changes nothing but the table of
//! sign-ins in flight, which is bounded and rate limited. What makes the callback safe is on the
//! callback side, in three bindings:
//!
//! - The `state` names an entry the server made, once, for [`FLOW_TTL`]. Anything else, a forged
//!   or replayed or late `state`, is refused before the code is touched.
//! - The entry is bound to a cookie the start set in this browser ([`OIDC_COOKIE`]), so a
//!   callback URL that an attacker obtained from their own sign-in, and gets the victim to open,
//!   doesn't finish in the victim's browser (login CSRF). It is `SameSite=Lax`, not `Strict`,
//!   because the provider's redirect back is a cross-site navigation, which `Strict` cookies
//!   don't ride. A second tab, or a restart of the sign-in, reuses the binding the browser has.
//! - A connection is bound to the user and the login that started it too: the callback is
//!   finished only by that same logged-in browser, which must still have an invite or recovery
//!   session.
//!
//! Nobody is created by a sign-in. A sign-in signs in the user who connected that account, found
//! by (provider, issuer, subject) alone; any other account is refused with the same answer as a
//! disabled user, so the page says nothing about who has an account.

use std::sync::Arc;

use axum::Extension;
use axum::Router;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::header::{RETRY_AFTER, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{from_fn_with_state, map_response};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use doozers_store::{OidcProvider, SignInMethod, is_provider_id};
use serde::Deserialize;
use tracing::{debug, info, warn};

use super::auth::{
    OIDC_COOKIE, TrustedProxy, WebUser, binding_cookie, check_csrf, check_nonce, cookie, is_binding,
};
use super::limits::{Limited, client_key};
use super::login::{is_https, start_session};
use super::oidc::{Identity, Oidc, Purpose, Returned};
use super::pages::{WebApp, error_page, internal_error, no_store};
use super::random_hex;
use super::signin::{LoginOption, SetupOption, needs_a_link};
use crate::http::{PeerAddr, RouterBuilder};
use crate::secrets::{SecretPath, SecretProvider};

/// What the sign-in routes need: the web app, the secret store the providers' client secrets are
/// in, and the protocol state ([`Oidc`]).
#[derive(Clone)]
pub(super) struct OidcPages {
    pub(super) app: WebApp,
    pub(super) secrets: Arc<dyn SecretProvider>,
    pub(super) oidc: Arc<Oidc>,
}

impl WebApp {
    /// Adds the OpenID Connect sign-in routes and the identity provider pages to `builder`, with
    /// client secrets in `secrets`, behind the same authentication middleware as the chat pages.
    /// Every answer is `no-store`.
    pub fn install_oidc(
        &self,
        builder: RouterBuilder,
        secrets: Arc<dyn SecretProvider>,
    ) -> RouterBuilder {
        let auth = from_fn_with_state(self.clone(), super::login::authenticate);
        let pages = OidcPages {
            app: self.clone(),
            secrets,
            oidc: Arc::new(Oidc::default()),
        };
        builder.merge(
            Router::new()
                .route("/login/oidc/{id}", get(sign_in))
                .route("/setup/oidc/{id}", get(connect))
                .route("/auth/oidc/{id}/callback", get(callback))
                .merge(super::oidc_admin::routes())
                .with_state(pages)
                .route_layer(auth)
                .route_layer(map_response(async |response: Response| no_store(response))),
        )
    }
}

/// The path the provider sends the browser back to, for provider `id`.
pub(super) fn callback_path(id: &str) -> String {
    format!("/auth/oidc/{id}/callback")
}

impl OidcPages {
    /// The absolute callback URL of provider `id`, which the provider must have registered.
    pub(super) fn redirect_uri(&self, id: &str) -> String {
        format!(
            "{}{}",
            self.app.inner.channel.link_base(),
            callback_path(id)
        )
    }
}

impl WebApp {
    /// The login page's entries for the enabled providers. Each is the link that starts a
    /// sign-in, which carries the page's sign-in `nonce`.
    ///
    /// # Errors
    ///
    /// The store's error, if it can't say.
    pub(super) async fn oidc_login_options(
        &self,
        nonce: &str,
    ) -> doozers_store::Result<Vec<LoginOption>> {
        Ok(self
            .inner
            .store
            .oidc_providers()
            .await?
            .into_iter()
            .filter(|provider| provider.enabled)
            .map(|provider| LoginOption {
                label: format!("Sign in with {}", provider.display_name),
                href: format!("/login/oidc/{}?nonce={nonce}", provider.id),
            })
            .collect())
    }

    /// The setup page's entries for the enabled providers, if `user` may add a sign-in method.
    /// Each is the link that starts the connection, which carries the CSRF token of the login.
    ///
    /// # Errors
    ///
    /// The store's error, if it can't say.
    pub(super) async fn oidc_setup_options(
        &self,
        user: &WebUser,
    ) -> doozers_store::Result<Vec<SetupOption>> {
        if !self.may_add_sign_in_method(user).await? {
            return Ok(Vec::new());
        }
        let connections = self.inner.store.user_oidc_connections(&user.user).await?;
        let csrf: String = url::form_urlencoded::byte_serialize(user.csrf.as_bytes()).collect();
        Ok(self
            .inner
            .store
            .oidc_providers()
            .await?
            .into_iter()
            .filter(|provider| provider.enabled)
            .map(|provider| {
                let connected = connections
                    .iter()
                    .any(|c| c.provider == provider.id && c.issuer == provider.issuer);
                SetupOption {
                    description: if connected {
                        format!(
                            "You are connected. Choose this to connect a different {} account.",
                            provider.display_name
                        )
                    } else {
                        format!(
                            "Sign in with your {} account, so you don't need a link next time.",
                            provider.display_name
                        )
                    },
                    href: format!("/setup/oidc/{}?csrf={csrf}", provider.id),
                    label: provider.display_name,
                }
            })
            .collect())
    }
}

fn unavailable() -> Response {
    error_page(
        StatusCode::NOT_FOUND,
        "Not available",
        "That way of signing in isn't available. Go back and choose another.",
    )
}

/// The one answer to every sign-in that isn't let in: an account nobody connected, a user who
/// can't sign in, and one whose provider didn't vouch for them in the way it should.
fn refused(provider: &OidcProvider) -> Response {
    no_store(error_page(
        StatusCode::FORBIDDEN,
        "Couldn't sign you in",
        &format!(
            "Doozers couldn't sign you in with {}. Log in another way. To use {}, ask an admin for \
             an invite link and connect it on the page you land on.",
            provider.display_name, provider.display_name
        ),
    ))
}

fn out_of_date() -> Response {
    no_store(error_page(
        StatusCode::BAD_REQUEST,
        "Sign-in expired",
        "This sign-in took too long, was already used, or began in another browser. Go back and \
         start again.",
    ))
}

/// The provider `id` if it is there and enabled, else the page to answer with.
async fn enabled_provider(pages: &OidcPages, id: &str) -> Result<OidcProvider, Box<Response>> {
    if !is_provider_id(id) {
        return Err(Box::new(unavailable()));
    }
    match pages.app.inner.store.oidc_provider(id).await {
        Ok(Some(provider)) if provider.enabled => Ok(provider),
        Ok(_) => Err(Box::new(unavailable())),
        Err(err) => Err(Box::new(internal_error(
            "cannot look up an identity provider",
            &err,
        ))),
    }
}

/// Starts a sign-in for `purpose` at provider `id` and answers with the redirect to the provider
/// and the binding cookie.
async fn start(
    pages: &OidcPages,
    id: &str,
    purpose: Purpose,
    trusted: bool,
    peer: Option<&PeerAddr>,
    headers: &HeaderMap,
) -> Response {
    let app = &pages.app;
    let client = client_key(trusted, headers, peer);
    if let Err(Limited { retry_after, first }) =
        pages
            .oidc
            .starts
            .begin("oidc", &client, app.inner.clock.now())
    {
        if first {
            warn!(%client, "refusing OpenID Connect sign-in starts: too many");
        } else {
            debug!(%client, "refused an OpenID Connect sign-in start: too many");
        }
        let mut response = error_page(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many attempts",
            "Wait a few minutes and try again.",
        );
        response
            .headers_mut()
            .insert(RETRY_AFTER, HeaderValue::from(retry_after.as_secs().max(1)));
        return response;
    }
    let provider = match enabled_provider(pages, id).await {
        Ok(provider) => provider,
        Err(response) => return *response,
    };
    let binding = cookie(headers, OIDC_COOKIE)
        .filter(|value| is_binding(value))
        .map_or_else(|| random_hex(16), str::to_owned);
    let url = match pages
        .oidc
        .begin(
            &provider,
            &pages.redirect_uri(id),
            &binding,
            purpose,
            app.inner.clock.now(),
        )
        .await
    {
        Ok(url) => url,
        Err(err) => {
            warn!(provider = %provider.id, error = %err, "cannot start an OpenID Connect sign-in");
            return error_page(
                StatusCode::BAD_GATEWAY,
                "Couldn't reach the provider",
                &format!(
                    "Doozers couldn't start a sign-in with {}. Try again later, or ask an admin \
                     to check the provider's settings.",
                    provider.display_name
                ),
            );
        }
    };
    let https = is_https(&app.inner.channel, headers, trusted);
    let mut response = Redirect::to(url.as_str()).into_response();
    response
        .headers_mut()
        .append(SET_COOKIE, binding_cookie(&binding, https));
    response
}

/// `?nonce=` on the link that starts a sign-in.
#[derive(Debug, Deserialize)]
struct SignInQuery {
    #[serde(default)]
    nonce: Option<String>,
}

/// `GET /login/oidc/<id>?nonce=`: sends the browser to provider `id` to sign in. The nonce is
/// checked first, before the start is counted or anything is kept.
async fn sign_in(
    State(pages): State<OidcPages>,
    trusted: Option<Extension<TrustedProxy>>,
    peer: Option<Extension<ConnectInfo<PeerAddr>>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<SignInQuery>,
) -> Response {
    if let Err(rejected) = check_nonce(&headers, query.nonce.as_deref()) {
        return rejected.into_response();
    }
    start(
        &pages,
        &id,
        Purpose::SignIn,
        trusted.is_some(),
        peer.as_ref().map(|Extension(c)| &c.0),
        &headers,
    )
    .await
}

/// `?csrf=` on the link that starts a connection.
#[derive(Debug, Deserialize)]
struct ConnectQuery {
    #[serde(default)]
    csrf: Option<String>,
}

/// `GET /setup/oidc/<id>?csrf=`: sends the browser to provider `id` to connect the account to the
/// user, who must have signed in with an invite or recovery link.
async fn connect(
    State(pages): State<OidcPages>,
    user: WebUser,
    trusted: Option<Extension<TrustedProxy>>,
    peer: Option<Extension<ConnectInfo<PeerAddr>>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<ConnectQuery>,
) -> Response {
    if let Err(rejected) = check_csrf(&user, &headers, query.csrf.as_deref()) {
        return rejected.into_response();
    }
    match pages.app.may_add_sign_in_method(&user).await {
        Ok(true) => {}
        Ok(false) => return needs_a_link(),
        Err(err) => return internal_error("cannot look up a web session", &err),
    }
    start(
        &pages,
        &id,
        Purpose::Connect {
            user: user.user.clone(),
            csrf: user.csrf.clone(),
        },
        trusted.is_some(),
        peer.as_ref().map(|Extension(c)| &c.0),
        &headers,
    )
    .await
}

/// What the provider sends back: a code and the state, or an error.
#[derive(Debug, Deserialize)]
struct Callback {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    /// The issuer that answered, if the provider says (RFC 9207).
    #[serde(default)]
    iss: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// `GET /auth/oidc/<id>/callback`: finishes the sign-in or the connection, once ([module
/// docs](self)).
async fn callback(
    State(pages): State<OidcPages>,
    user: Option<Extension<WebUser>>,
    trusted: Option<Extension<TrustedProxy>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(callback): Query<Callback>,
) -> Response {
    let app = &pages.app;
    let now = app.inner.clock.now();
    // The sign-in is taken out of the table first, whatever else is wrong with the request, so
    // that it can't be tried again.
    let flow = match (&callback.state, cookie(&headers, OIDC_COOKIE)) {
        (Some(state), Some(binding)) => pages.oidc.take_flow(state, binding, now),
        _ => None,
    };
    let Some(flow) = flow.filter(|flow| flow.provider == id) else {
        debug!(provider = %id, "an OpenID Connect callback matched no sign-in");
        return out_of_date();
    };
    let provider = match enabled_provider(&pages, &id).await {
        Ok(provider) => provider,
        Err(response) => return *response,
    };
    if let Some(error) = &callback.error {
        info!(provider = %id, error = %error, "the provider didn't sign the user in");
        return no_store(error_page(
            StatusCode::BAD_REQUEST,
            "Not signed in",
            &format!(
                "{} didn't sign you in. Go back and try again.",
                provider.display_name
            ),
        ));
    }
    let Some(code) = &callback.code else {
        return out_of_date();
    };
    let user = user.map(|Extension(user)| user);
    let purpose = flow.purpose.clone();
    // A connection finishes only in the login that began it.
    if let Purpose::Connect { user: owner, csrf } = &purpose
        && !user
            .as_ref()
            .is_some_and(|user| user.user == *owner && user.csrf == *csrf)
    {
        warn!(provider = %id, "an OpenID Connect connection came back to another login");
        return out_of_date();
    }
    // And that login must still be one that may add a way to sign in.
    if matches!(purpose, Purpose::Connect { .. })
        && let Some(user) = &user
    {
        match app.may_add_sign_in_method(user).await {
            Ok(true) => {}
            Ok(false) => return needs_a_link(),
            Err(err) => return internal_error("cannot look up a web session", &err),
        }
    }

    let secret = match &provider.client_secret {
        None => None,
        Some(name) => {
            let secret = match SecretPath::new(name.clone()) {
                Ok(path) => pages
                    .secrets
                    .get(&path)
                    .await
                    .map_err(|err| err.to_string()),
                Err(err) => Err(err.to_string()),
            };
            match secret {
                Ok(Some(secret)) => Some(secret),
                Ok(None) | Err(_) => {
                    warn!(provider = %id, "the provider's client secret can't be read");
                    return error_page(
                        StatusCode::BAD_GATEWAY,
                        "Couldn't sign you in",
                        "Doozers can't read this provider's client secret. Ask an admin to check \
                         the provider's settings.",
                    );
                }
            }
        }
    };
    let identity = match pages
        .oidc
        .complete(
            &provider,
            secret.as_ref(),
            &pages.redirect_uri(&id),
            flow,
            Returned {
                code,
                issuer: callback.iss.as_deref(),
            },
            now,
        )
        .await
    {
        Ok(identity) => identity,
        Err(err) => {
            warn!(provider = %id, error = %err, "an OpenID Connect sign-in was refused");
            return refused(&provider);
        }
    };

    let https = is_https(&app.inner.channel, &headers, trusted.is_some());
    // The binding cookie stays: it only says which browser, the entry it bound is gone, and a
    // second tab's sign-in is still bound to it.
    match purpose {
        Purpose::SignIn => sign_in_user(&pages, &provider, &identity, user.as_ref(), https).await,
        Purpose::Connect { user: owner, .. } => {
            connect_user(&pages, &provider, &identity, &owner).await
        }
    }
}

/// Signs in the user who connected `identity`, replacing the login `previous`.
async fn sign_in_user(
    pages: &OidcPages,
    provider: &OidcProvider,
    identity: &Identity,
    previous: Option<&WebUser>,
    https: bool,
) -> Response {
    let app = &pages.app;
    let found = match app
        .inner
        .store
        .user_oidc_user(&provider.id, &identity.issuer, &identity.subject)
        .await
    {
        Ok(found) => found,
        Err(err) => return internal_error("cannot look up an OpenID Connect connection", &err),
    };
    let Some(user) = found else {
        info!(provider = %provider.id, "an OpenID Connect account nobody connected tried to sign in");
        return refused(provider);
    };
    if !app.inner.channel.can_sign_in(&user) {
        warn!(%user, provider = %provider.id, "a user who can't sign in tried to sign in with OpenID Connect");
        return refused(provider);
    }
    info!(%user, provider = %provider.id, method = SignInMethod::Oidc.as_str(), "signed in to the web chat");
    start_session(app, user, SignInMethod::Oidc, previous, https, "/").await
}

/// Connects `identity` to `owner`, then back to the setup page.
async fn connect_user(
    pages: &OidcPages,
    provider: &OidcProvider,
    identity: &Identity,
    owner: &doozers_types::UserId,
) -> Response {
    let app = &pages.app;
    match app
        .inner
        .store
        .user_oidc_connect(
            &provider.id,
            &identity.issuer,
            &identity.subject,
            owner,
            app.inner.clock.now(),
        )
        .await
    {
        Ok(_) => {
            info!(user = %owner, provider = %provider.id, "connected an OpenID Connect account");
            Redirect::to("/setup").into_response()
        }
        Err(doozers_store::Error::Conflict(_)) => no_store(error_page(
            StatusCode::CONFLICT,
            "Already connected",
            &format!(
                "That {} account is connected to another user. Sign in as that user, or connect \
                 another {} account.",
                provider.display_name, provider.display_name
            ),
        )),
        Err(err) => internal_error("cannot connect an OpenID Connect account", &err),
    }
}

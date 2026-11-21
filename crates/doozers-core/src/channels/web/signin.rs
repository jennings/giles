//! The sign-in methods the login and setup pages offer: the seam each method plugs into.
//!
//! A method that signs in through a link contributes an entry to [`WebApp::login_options`], the way
//! to use it on the login page, and every method that a user can add contributes one to
//! [`WebApp::setup_options`], the way to add it on the setup page. The password has no entry in the
//! first, because its form is on the login page itself
//! ([`password_collapsed`](WebApp::password_collapsed)); its setup page is in
//! [`password_pages`](super::password_pages). The login page always says how to use an invite or
//! recovery link.

use axum::http::StatusCode;
use axum::response::Response;
use doozers_store::SignInMethod;

use super::auth::{Login, WebUser};
use super::pages::{WebApp, error_page, no_store};

/// A sign-in method a user can add to their account, as the setup page lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SetupOption {
    /// Its name, such as `Passkey`.
    pub label: String,
    /// One sentence on what it is.
    pub description: String,
    /// The page that adds it.
    pub href: String,
}

/// A way to sign in, as the login page lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LoginOption {
    /// What the button says, such as `Sign in with a passkey`.
    pub label: String,
    /// The page that signs in with it.
    pub href: String,
}

/// The answer for a login that may not add or change a sign-in method.
pub(super) fn needs_a_link() -> Response {
    no_store(error_page(
        StatusCode::FORBIDDEN,
        "Open a link first",
        "You add or change a way to sign in from a login that came from an invite or recovery link. \
         Ask an admin for a link, or run `doozers admin login-link` if you run Doozers.",
    ))
}

impl WebApp {
    /// Whether `user` may add or change a sign-in method now: they have a cookie session that
    /// signed in with an invite or recovery link. Those are how a user proves who they are when
    /// they have no other way to prove it, and a 30-day session that came from anything else isn't
    /// enough to take over the account's way in.
    ///
    /// # Errors
    ///
    /// The store's error, if it can't look the session up.
    pub(super) async fn may_add_sign_in_method(
        &self,
        user: &WebUser,
    ) -> doozers_store::Result<bool> {
        let Login::Cookie(token) = &user.login else {
            return Ok(false);
        };
        let now = self.inner.clock.now();
        let session = self.inner.store.web_session(token, now).await?;
        Ok(session.is_some_and(|session| {
            matches!(
                session.method,
                SignInMethod::Invite | SignInMethod::Recovery
            )
        }))
    }

    /// The ways to sign in on the login page that are a link to another page. No method is one yet,
    /// so there are none.
    ///
    /// # Errors
    ///
    /// The store's error, if it can't say.
    pub(super) async fn login_options(&self) -> doozers_store::Result<Vec<LoginOption>> {
        Ok(Vec::new())
    }

    /// Whether the login page folds the password form away behind "Use a password instead", for
    /// a method that suits the browser better to take its place: true where passkeys are offered,
    /// which put their own sign-in above the form.
    pub(super) fn password_collapsed(&self) -> bool {
        self.inner.passkeys.relying_party().is_ok()
    }

    /// The sign-in methods `user` can add to their account now.
    ///
    /// # Errors
    ///
    /// The store's error, if it can't say.
    pub(super) async fn setup_options(
        &self,
        user: &WebUser,
    ) -> doozers_store::Result<Vec<SetupOption>> {
        let mut options = Vec::new();
        options.extend(self.passkey_setup_option(user).await?);
        options.extend(self.password_setup_option(user).await?);
        Ok(options)
    }
}

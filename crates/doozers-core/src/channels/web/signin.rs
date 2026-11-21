//! The sign-in methods the login and setup pages offer: the seam each method plugs into.
//!
//! A method contributes an entry to [`WebApp::login_options`], the way to use it on the login page,
//! and to [`WebApp::setup_options`], the way to add it on the setup page. Until one exists, the
//! login page says to open an invite or recovery link, and the setup page that the link the user
//! opened is the only way to sign in.

use super::auth::WebUser;
use super::pages::WebApp;

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

impl WebApp {
    /// The ways to sign in on the login page. No method exists yet, so there are none.
    pub(super) fn login_options(&self) -> Vec<LoginOption> {
        Vec::new()
    }

    /// The sign-in methods `user` can add to their account now. No method exists yet, so there are
    /// none.
    pub(super) fn setup_options(&self, _user: &WebUser) -> Vec<SetupOption> {
        Vec::new()
    }
}

//! The sign-in methods the setup page offers a user to add: the seam each method plugs into.
//!
//! A method contributes an entry to [`WebApp::setup_options`]. Until one exists, the setup page
//! says that the link the user opened is the only way to sign in.

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

impl WebApp {
    /// The sign-in methods `user` can add to their account now. No method exists yet, so there are
    /// none.
    pub(super) fn setup_options(&self, _user: &WebUser) -> Vec<SetupOption> {
        Vec::new()
    }
}

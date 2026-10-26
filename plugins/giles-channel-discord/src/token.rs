//! Whether Discord has rejected the bot token, shared between whoever calls REST and the session.
//!
//! Discord asks that an app stops making requests once its token is invalid (10 000 invalid
//! requests in 10 minutes get the IP banned), and the library refuses every request after the first
//! 401 anyway. The plugin goes further: a rejected token, whether Discord said so with a 401 on a
//! REST call or with close code 4004 on the Gateway, stops REST for good (until the plugin is
//! restarted with a new token), and a 401 on REST also ends the session the way a 4004 does, so the
//! `NeedsAuth` status is reported and nothing reconnects.

use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

/// The token's state: valid as far as anyone knows, or rejected.
#[derive(Debug, Default)]
pub struct TokenState {
    rejected: AtomicBool,
    wake: Notify,
}

impl TokenState {
    /// Whether Discord has rejected the token.
    pub fn is_rejected(&self) -> bool {
        self.rejected.load(Ordering::SeqCst)
    }

    /// Records that the Gateway closed with 4004. The session reports that itself, so it isn't
    /// woken; REST callers see [`is_rejected`](Self::is_rejected) and stop.
    pub fn note_rejected(&self) {
        self.rejected.store(true, Ordering::SeqCst);
    }

    /// Records that a REST call got a 401 and wakes the session, which reports `NeedsAuth` and
    /// stops reconnecting. The first call only: later ones change nothing.
    pub fn reject(&self) {
        if !self.rejected.swap(true, Ordering::SeqCst) {
            self.wake.notify_one();
        }
    }

    /// Completes after a [`reject`](Self::reject). A rejection that happened before the call is
    /// not lost: the wake-up is stored until someone waits for it.
    pub async fn rejection(&self) {
        self.wake.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_rejection_before_anyone_waits_is_not_lost() {
        let token = TokenState::default();
        assert!(!token.is_rejected());
        token.reject();
        assert!(token.is_rejected());
        tokio::time::timeout(Duration::from_secs(1), token.rejection())
            .await
            .expect("the stored wake-up");
    }

    #[tokio::test(start_paused = true)]
    async fn only_the_first_rejection_wakes_the_session() {
        let token = TokenState::default();
        token.reject();
        token.reject();
        token.rejection().await;
        assert!(
            tokio::time::timeout(Duration::from_secs(1), token.rejection())
                .await
                .is_err(),
            "one wake-up for one rejection"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_4004_stops_rest_without_waking_anyone() {
        let token = TokenState::default();
        token.note_rejected();
        assert!(token.is_rejected());
        // A 401 afterwards finds the token already rejected, and the session already final.
        token.reject();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), token.rejection())
                .await
                .is_err()
        );
    }
}

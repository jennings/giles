//! The seam between the channel's logic and the Discord client library.
//!
//! Everything the plugin does with Discord goes through [`Discord`], in the plugin's own terms.
//! `real` implements it on `twilight`; the tests implement it with a fake, so the plugin's logic
//! runs without a network, a token, or a bot. No library type crosses it, which is also what keeps
//! a library upgrade inside `real`.
//!
//! The design (`Docs/technical/discord.md`) lists six methods. This one has the two the
//! connection needs: the Gateway's events and a clean close. `create_message`, `create_dm`,
//! `messages_after`, and `download` arrive with the issues that use them (outbound messages,
//! catch-up, attachments), together with their real implementations, so that no method here is
//! defined and left unimplemented.

use giles_plugin_sdk::async_trait;

/// Something the Gateway connection reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayEvent {
    /// A new session is established (Identify): the first connection, a restart, or after Discord
    /// invalidated the session. Messages sent while no session existed are not replayed.
    Ready {
        /// The bot's own user id (a snowflake).
        user_id: u64,
    },
    /// The previous session resumed: Discord replayed what was missed, in order.
    Resumed,
    /// The connection ended and the client will try again, unless the close was final. `code` is
    /// the WebSocket close code, or `None` when no close frame came (a failed connect, a
    /// transport error, a heartbeat that went unanswered). Discord's request to reconnect and its
    /// invalid-session reply also end in a close.
    Closed {
        /// The close code, if the connection closed with one.
        code: Option<u16>,
    },
}

/// A Discord Gateway connection and the REST calls the plugin makes.
#[async_trait]
pub trait Discord: Send + Sync + 'static {
    /// The next event of one long-lived connection. Connecting, heartbeating, resuming, and
    /// re-identifying are the client's business and happen while this is polled: a connection
    /// that isn't polled doesn't reconnect, which is how the caller paces reconnects.
    ///
    /// `None` after a close the client won't recover from (a rejected token, intents or sharding
    /// Discord refuses); the caller doesn't poll again. Dropping the future loses nothing.
    async fn next_event(&self) -> Option<GatewayEvent>;

    /// Closes the connection cleanly (close code 1000, so the bot shows as offline and the
    /// session ends) and waits for Discord to answer. Called once, after the last
    /// [`next_event`](Self::next_event); the caller bounds how long it may take.
    async fn shutdown(&self);
}

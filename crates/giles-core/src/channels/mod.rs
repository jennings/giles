//! Built-in channels (PLAN sections 4.2, 10). Plugin channels are reached through the plugin host
//! instead.

pub mod chat_ids;
#[cfg(feature = "channel-web")]
pub mod web;

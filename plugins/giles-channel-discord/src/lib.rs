//! `giles-channel-discord`: the Giles Discord channel plugin. A bot account that receives and sends
//! direct messages over the Discord Gateway and REST API. Run by Giles, not by hand. See
//! `Docs/technical/discord.md`.
//!
//! The crate is a library and a binary so that its integration tests can reach the channel and the
//! fake Discord client; nothing here is an API for anyone else.

pub mod client;
pub mod config;
pub mod status;

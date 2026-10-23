//! Giles orchestration: the agent loop, routing, sessions, scheduling, and plugin supervision.

pub mod commands;
pub mod config;
mod front_matter;
pub mod http;
pub mod identity;
pub mod mcp;
pub mod memory;
pub mod net;
pub mod paths;
pub mod providers;
pub mod router;
pub mod sandbox;
pub mod skills;

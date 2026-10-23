//! Giles orchestration: the agent loop, routing, sessions, scheduling, and plugin supervision.

pub mod admin;
pub mod agent_loop;
pub mod approvals;
pub mod blobs;
pub mod channels;
pub mod commands;
pub mod config;
mod front_matter;
pub mod http;
pub mod identity;
pub mod mcp;
pub mod memory;
pub mod net;
pub mod paths;
pub mod plugin_host;
mod process_group;
pub mod providers;
pub mod registry;
pub mod router;
pub mod runtime;
pub mod sandbox;
pub mod sessions;
pub mod skills;
mod stderr_lines;

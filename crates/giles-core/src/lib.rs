//! Giles orchestration: the agent loop, routing, sessions, scheduling, and plugin supervision.

pub mod config;
mod front_matter;
pub mod http;
pub mod memory;
pub mod paths;
pub mod providers;
pub mod sandbox;
pub mod skills;

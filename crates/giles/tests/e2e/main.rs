//! End to end tests of the whole daemon.
//!
//! Each test runs the real `giles serve` binary on a temporary data root, with the test-only echo
//! channel plugin (`plugins/giles-channel-echo`, launched through the plugin host and, where the
//! host has one, the sandbox) and a scripted fake provider: a loopback server that speaks the
//! OpenAI Chat Completions protocol, reached through the daemon's real `openai` provider. The
//! tests are the owner: they write messages into the echo plugin's `inject/` directory and read
//! what the daemon sends back from its `sent.jsonl`. No production code knows about the tests.
//!
//! - `chat`: a message to its reply, a tool call with approval, and `/stop`.
//! - `recovery`: killing the daemon mid-turn and starting it again.
//! - `jobs`: a configured job delivered through the outbox.

#![cfg(feature = "provider-openai")]

mod chat;
mod harness;
mod jobs;
mod provider;
mod recovery;

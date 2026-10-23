//! Built-in model providers (PLAN section 4.2), each behind a Cargo feature, and the HTTP and
//! server-sent events plumbing they share.

#[cfg(feature = "provider-anthropic")]
pub mod http;
#[cfg(feature = "provider-anthropic")]
pub mod sse;

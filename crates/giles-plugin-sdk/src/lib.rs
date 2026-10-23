//! SDK for Giles plugins: serves a component trait implementation as an out-of-process gRPC
//! plugin.

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the plugin server uses it from the next commit on"
    )
)]
mod auth;
pub mod render;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the plugin server uses it from the next commit on"
    )
)]
mod startup;

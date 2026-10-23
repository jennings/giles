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
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the plugin server uses it from the next commit on"
    )
)]
mod base;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the plugin server uses it from the next commit on"
    )
)]
mod blobs;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the plugin server uses it from the next commit on"
    )
)]
mod dirs;
pub mod render;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the plugin server uses it from the next commit on"
    )
)]
mod startup;
#[cfg(test)]
mod test_support;

pub use blobs::Blobs;

//! Built-in model providers (PLAN section 4.2), each behind a Cargo feature, and the HTTP,
//! server-sent events, and wire-format plumbing they share.

use async_trait::async_trait;
use bytes::Bytes;
use giles_types::{MediaRef, Result};

#[cfg(feature = "provider-anthropic")]
pub mod anthropic;
#[cfg(feature = "provider-anthropic")]
pub mod http;
#[cfg(feature = "provider-anthropic")]
pub mod sse;
#[cfg(feature = "provider-anthropic")]
pub mod wire;

/// Where a provider gets the bytes of the attachments it sends inline (images, PDFs). In the
/// daemon this is the blob store (PLAN section 11.5).
///
/// It returns only bytes: the media type comes from the [`MediaRef`], which the core recorded
/// when it stored the blob. A provider calls it only for media types its model accepts, while
/// building a request, so an error fails that request before anything is sent.
#[async_trait]
pub trait MediaSource: Send + Sync {
    /// The content of `media`. [`giles_types::Error::NotFound`] if it no longer exists.
    async fn read(&self, media: &MediaRef) -> Result<Bytes>;
}

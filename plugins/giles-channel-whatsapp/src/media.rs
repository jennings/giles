//! Attachments in both directions, through the SDK's blob store.

use giles_plugin_sdk::giles_types::{MediaRef, Result};
use giles_plugin_sdk::{Blobs, async_trait};

/// Where attachments go and come from: the SDK's [`Blobs`], or a fake in tests.
#[async_trait]
pub(crate) trait MediaStore: Send + Sync {
    /// Stores an inbound attachment.
    async fn put(
        &self,
        bytes: Vec<u8>,
        media_type: String,
        name: Option<String>,
    ) -> Result<MediaRef>;

    /// Reads an outbound attachment.
    async fn read(&self, media: &MediaRef) -> Result<Vec<u8>>;
}

#[async_trait]
impl MediaStore for Blobs {
    async fn put(
        &self,
        bytes: Vec<u8>,
        media_type: String,
        name: Option<String>,
    ) -> Result<MediaRef> {
        Blobs::put(self, bytes, media_type, name).await
    }

    async fn read(&self, media: &MediaRef) -> Result<Vec<u8>> {
        Blobs::read(self, media).await
    }
}

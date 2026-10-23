//! Media between Giles and MCP servers (PLAN sections 5 and 7.10): images, audio, and blob
//! resources in tool results are stored in the blob store and become media blocks, and a stdio
//! server gets `{"$media": "<id>"}` inputs as files in its `TMPDIR` ([`super::files`]).

use std::path::PathBuf;

use base64::Engine as _;
use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use giles_store::{BlobMeta, BlobStore};
use giles_types::{Block, Timestamp, ToolCallEvent};
use tracing::warn;

use super::convert::{IncomingMedia, Part, ToolResult};
use super::files::MediaDir;
use crate::config::DEFAULT_MAX_BLOB;
use crate::media;

/// The largest media item a tool result may carry, decoded: 50 MiB, as for blobs the core fetches
/// from plugins. A larger one is dropped with a text placeholder.
pub const MAX_RESULT_MEDIA_BYTES: u64 = DEFAULT_MAX_BLOB;

/// Base64 as MCP sends it: the standard alphabet, with or without padding.
const BASE64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// Where one MCP connection's media goes: the blob store that keeps the media its tools return,
/// and for a stdio server, the directory `$media` inputs are copied into.
#[derive(Debug, Clone)]
pub struct McpMedia {
    pub(crate) blobs: BlobStore,
    /// `None`: the server takes no media inputs (a remote server, PLAN section 7.10).
    pub(crate) files: Option<MediaDir>,
}

impl McpMedia {
    /// Media in tool results is stored in `blobs`; media inputs are refused.
    pub fn new(blobs: BlobStore) -> Self {
        Self { blobs, files: None }
    }

    /// The media a call's input refers to (`ToolCall::media`) is copied from the blob store into
    /// `dir`, the stdio server's `TMPDIR`, for the length of the call, and each reference in the
    /// input is replaced with its file's path. The server must be able to read `dir`.
    #[must_use]
    pub fn files_in(mut self, dir: PathBuf) -> Self {
        self.files = Some(MediaDir::new(dir));
        self
    }
}

/// `result` as a [`ToolCallEvent::Result`], each media part stored in `blobs` and replaced with
/// its media block ([`media::block`]: `Image`, `Audio`, or `Document`). The agent loop puts the
/// media's reference before it when it stores the result. Media over `limit` bytes, invalid
/// base64, or media that can't be stored is replaced with a text placeholder saying so.
pub(crate) async fn store_result(
    result: ToolResult,
    blobs: &BlobStore,
    limit: u64,
) -> ToolCallEvent {
    let mut content = Vec::with_capacity(result.parts.len());
    for part in result.parts {
        content.push(match part {
            Part::Block(block) => block,
            Part::Media(incoming) => store(incoming, blobs, limit).await,
        });
    }
    ToolCallEvent::Result {
        content,
        is_error: result.is_error,
    }
}

async fn store(incoming: IncomingMedia, blobs: &BlobStore, limit: u64) -> Block {
    if incoming.decoded_len() > limit {
        return incoming.dropped(format_args!("over the {limit}-byte limit"));
    }
    let Ok(bytes) = BASE64.decode(incoming.data.as_bytes()) else {
        return incoming.dropped("invalid base64");
    };
    let meta = BlobMeta {
        media_type: incoming.media_type.clone(),
        name: incoming.name.clone(),
    };
    match blobs.put(&bytes[..], limit, meta, Timestamp::now()).await {
        Ok(stored) => media::block(stored),
        Err(error) => {
            warn!(%error, what = %incoming.what, "cannot store media from an MCP tool");
            incoming.dropped("it could not be stored")
        }
    }
}

#[cfg(test)]
mod tests {
    use giles_store::Store;
    use giles_types::MediaRef;

    use super::*;

    async fn blobs() -> (tempfile::TempDir, BlobStore) {
        let dir = tempfile::tempdir().unwrap();
        let db = Store::open(dir.path().join("giles.db")).await.unwrap();
        let blobs = BlobStore::open(dir.path().join("blobs"), db).await.unwrap();
        (dir, blobs)
    }

    fn incoming(what: &str, media_type: &str, name: Option<&str>, data: &str) -> Part {
        Part::Media(IncomingMedia {
            what: what.to_owned(),
            media_type: media_type.to_owned(),
            name: name.map(str::to_owned),
            data: data.to_owned(),
        })
    }

    async fn stored(blobs: &BlobStore, media: &MediaRef) -> Vec<u8> {
        use tokio::io::AsyncReadExt as _;
        let mut blob = blobs.get(&media.id).await.unwrap().unwrap();
        let mut bytes = Vec::new();
        blob.reader.read_to_end(&mut bytes).await.unwrap();
        bytes
    }

    #[tokio::test]
    async fn media_is_stored_as_blobs_and_becomes_media_blocks() {
        let (_dir, blobs) = blobs().await;
        let result = ToolResult {
            parts: vec![
                Part::Block(Block::text("Here:")),
                incoming("image", "image/png", None, "aGVsbG8="),
                // Without padding.
                incoming("audio", "audio/ogg", None, "aGVsbG8"),
                incoming(
                    "resource file:///q3.pdf",
                    "application/pdf",
                    Some("q3.pdf"),
                    "aGk=",
                ),
            ],
            is_error: true,
        };
        let ToolCallEvent::Result { content, is_error } = store_result(result, &blobs, 1024).await
        else {
            panic!("not a result");
        };
        assert!(is_error);
        assert_eq!(content.len(), 4);
        assert_eq!(content[0], Block::text("Here:"));
        let Block::Image { media: image } = &content[1] else {
            panic!("not an image: {:?}", content[1]);
        };
        assert_eq!((image.media_type.as_str(), image.size), ("image/png", 5));
        assert_eq!(stored(&blobs, image).await, b"hello");
        let Block::Audio { media: audio } = &content[2] else {
            panic!("not audio: {:?}", content[2]);
        };
        assert_eq!(audio.id, image.id, "the same bytes are one blob");
        assert_eq!(audio.media_type, "audio/ogg");
        let Block::Document { media: pdf, name } = &content[3] else {
            panic!("not a document: {:?}", content[3]);
        };
        assert_eq!(name.as_deref(), Some("q3.pdf"));
        assert_eq!(pdf.name.as_deref(), Some("q3.pdf"));
        assert_eq!(stored(&blobs, pdf).await, b"hi");
    }

    #[tokio::test]
    async fn oversized_or_invalid_media_becomes_a_placeholder() {
        let (_dir, blobs) = blobs().await;
        let result = ToolResult {
            parts: vec![
                // 9 bytes, over an 8-byte limit.
                incoming("image", "image/png", None, "aGVsbG8hISEh"),
                incoming("audio", "audio/wav", None, "not base64"),
            ],
            is_error: false,
        };
        let ToolCallEvent::Result { content, .. } = store_result(result, &blobs, 8).await else {
            panic!("not a result");
        };
        assert_eq!(
            content,
            [
                Block::text("[image (image/png, 9 bytes) dropped: over the 8-byte limit]"),
                Block::text("[audio (audio/wav, 7 bytes) dropped: invalid base64]"),
            ]
        );
    }
}

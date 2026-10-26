//! Attachments in both directions: the SDK's blob store, and the media types and file names that
//! go with the bytes.
//!
//! Both come from people (a file name Discord reports, a media type the sender's client guessed),
//! so they are cleaned before they go anywhere: a name before it is a blob name or a multipart
//! header, a media type before it is a header.

use giles_plugin_sdk::giles_types::{MediaRef, Result};
use giles_plugin_sdk::{Blobs, async_trait};
use tokio::io::AsyncRead;

/// The media type of a file nobody could name the type of.
pub const UNKNOWN_TYPE: &str = "application/octet-stream";
/// The longest name given to a blob or to an upload, in characters.
const NAME_LIMIT: usize = 100;
/// The name of an upload that has none (the extension of its media type is added when known).
const DEFAULT_NAME: &str = "attachment";

/// What an extension says about a media type. The first entry of a type is the extension an
/// upload of that type gets when it has no name.
const TYPES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("heic", "image/heic"),
    ("bmp", "image/bmp"),
    ("svg", "image/svg+xml"),
    ("pdf", "application/pdf"),
    ("txt", "text/plain"),
    ("md", "text/markdown"),
    ("csv", "text/csv"),
    ("html", "text/html"),
    ("json", "application/json"),
    ("xml", "application/xml"),
    ("zip", "application/zip"),
    ("gz", "application/gzip"),
    ("tar", "application/x-tar"),
    ("mp3", "audio/mpeg"),
    ("wav", "audio/wav"),
    ("ogg", "audio/ogg"),
    ("m4a", "audio/mp4"),
    ("flac", "audio/flac"),
    ("mp4", "video/mp4"),
    ("mov", "video/quicktime"),
    ("webm", "video/webm"),
    (
        "docx",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    ),
    (
        "xlsx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    ),
    (
        "pptx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    ),
];

/// Where attachments go and come from: the SDK's [`Blobs`], or a fake in tests.
#[async_trait]
pub trait MediaStore: Send + Sync + 'static {
    /// Stores an inbound attachment, streaming it from `reader` to disk. A reader that fails
    /// stores nothing.
    async fn put_reader(
        &self,
        reader: Box<dyn AsyncRead + Send + Unpin>,
        media_type: String,
        name: Option<String>,
    ) -> Result<MediaRef>;

    /// Reads an outbound attachment, which the core put for the `Send` that is running.
    async fn read(&self, media: &MediaRef) -> Result<Vec<u8>>;
}

#[async_trait]
impl MediaStore for Blobs {
    async fn put_reader(
        &self,
        reader: Box<dyn AsyncRead + Send + Unpin>,
        media_type: String,
        name: Option<String>,
    ) -> Result<MediaRef> {
        Blobs::put_reader(self, reader, media_type, name).await
    }

    async fn read(&self, media: &MediaRef) -> Result<Vec<u8>> {
        Blobs::read(self, media).await
    }
}

/// `type/subtype` of `media_type`, lowercased, with its parameters dropped; `None` unless both
/// are plain tokens (so it can't carry a header break or a comment) and the type is a specific
/// one: `application/octet-stream` says nothing.
fn base_type(media_type: &str) -> Option<String> {
    let base = media_type.split(';').next()?.trim().to_ascii_lowercase();
    let (kind, sub) = base.split_once('/')?;
    let token = |text: &str| {
        !text.is_empty()
            && text
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$&-^_.+".contains(&b))
    };
    (token(kind) && token(sub) && base != UNKNOWN_TYPE).then_some(base)
}

/// The media type of a file Discord calls `filename` and, maybe, `content_type`: what Discord says
/// when it is a media type, else what the extension says, else [`UNKNOWN_TYPE`].
pub fn media_type(content_type: Option<&str>, filename: &str) -> String {
    content_type
        .and_then(base_type)
        .or_else(|| {
            let (_, extension) = filename.rsplit_once('.')?;
            let extension = extension.to_ascii_lowercase();
            TYPES
                .iter()
                .find(|(known, _)| *known == extension)
                .map(|(_, media_type)| (*media_type).to_owned())
        })
        .unwrap_or_else(|| UNKNOWN_TYPE.to_owned())
}

/// `media_type` as it is sent in a part's `Content-Type`: `type/subtype` or [`UNKNOWN_TYPE`].
pub fn safe_media_type(media_type: &str) -> String {
    base_type(media_type).unwrap_or_else(|| UNKNOWN_TYPE.to_owned())
}

/// `name` without control characters and with path separators and quotes replaced, cut to
/// [`NAME_LIMIT`] characters, keeping a short extension. `None` when nothing is left.
fn clean(name: &str) -> Option<String> {
    let name: String = name
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| {
            if matches!(c, '/' | '\\' | '"') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let name = name.trim();
    if name.is_empty() || name.chars().all(|c| c == '.') {
        return None;
    }
    if name.chars().count() <= NAME_LIMIT {
        return Some(name.to_owned());
    }
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension)
        .filter(|extension| (1..=10).contains(&extension.chars().count()))
        .map_or(String::new(), |extension| format!(".{extension}"));
    let keep = NAME_LIMIT - extension.chars().count();
    let stem: String = name.chars().take(keep).collect();
    Some(format!("{stem}{extension}"))
}

/// The name of an inbound blob: Discord's file name, cleaned.
pub fn blob_name(filename: &str) -> Option<String> {
    clean(filename)
}

/// The name an upload goes under: `name` cleaned, or `attachment` with the extension of
/// `media_type` when it has no usable name.
pub fn upload_name(name: Option<&str>, media_type: &str) -> String {
    if let Some(name) = name.and_then(clean) {
        return name;
    }
    let extension = base_type(media_type).and_then(|base| {
        TYPES
            .iter()
            .find(|(_, known)| *known == base)
            .map(|(extension, _)| *extension)
    });
    match extension {
        Some(extension) => format!("{DEFAULT_NAME}.{extension}"),
        None => DEFAULT_NAME.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discords_media_type_wins_and_loses_its_parameters() {
        assert_eq!(media_type(Some("image/png"), "x.jpg"), "image/png");
        assert_eq!(
            media_type(Some("Text/Plain; charset=utf-8"), "x"),
            "text/plain"
        );
    }

    #[test]
    fn a_missing_or_useless_media_type_falls_back_to_the_extension() {
        assert_eq!(media_type(None, "Photo.JPG"), "image/jpeg");
        assert_eq!(media_type(Some(UNKNOWN_TYPE), "a.pdf"), "application/pdf");
        assert_eq!(media_type(Some(""), "a.mp3"), "audio/mpeg");
        assert_eq!(media_type(Some("not a type"), "a.zip"), "application/zip");
        // The last extension decides, and a name with none, or an unknown one, has no type.
        assert_eq!(media_type(None, "archive.tar.gz"), "application/gzip");
        assert_eq!(media_type(None, "README"), UNKNOWN_TYPE);
        assert_eq!(media_type(None, "x.unknownext"), UNKNOWN_TYPE);
    }

    #[test]
    fn a_media_type_with_a_header_break_is_never_passed_on() {
        for bad in [
            "image/png\r\nX-Evil: 1",
            "image/ png",
            "/png",
            "image/",
            "image",
            "a/b/c",
            "",
        ] {
            assert_eq!(safe_media_type(bad), UNKNOWN_TYPE, "{bad:?}");
            assert_eq!(media_type(Some(bad), "noext"), UNKNOWN_TYPE, "{bad:?}");
        }
        assert_eq!(safe_media_type("image/svg+xml"), "image/svg+xml");
        // Surrounding whitespace is trimmed, not passed on.
        assert_eq!(safe_media_type(" image/png\n"), "image/png");
        assert_eq!(safe_media_type("Audio/Ogg; codecs=opus"), "audio/ogg");
    }

    #[test]
    fn names_lose_what_could_break_a_header_or_a_path() {
        assert_eq!(blob_name("a\r\nb.png").as_deref(), Some("ab.png"));
        assert_eq!(
            blob_name("../../etc/passwd").as_deref(),
            Some(".._.._etc_passwd")
        );
        assert_eq!(
            blob_name("a\"; filename=\"b").as_deref(),
            Some("a_; filename=_b")
        );
        assert_eq!(blob_name("dir\\file.txt").as_deref(), Some("dir_file.txt"));
        assert_eq!(blob_name("  spaced.txt  ").as_deref(), Some("spaced.txt"));
        assert_eq!(
            blob_name("\u{202e}rtl.txt").as_deref(),
            Some("\u{202e}rtl.txt")
        );
        for empty in ["", "   ", "\n\t", "..", "."] {
            assert_eq!(blob_name(empty), None, "{empty:?}");
        }
    }

    #[test]
    fn a_long_name_is_cut_to_the_limit_and_keeps_its_extension() {
        let name = format!("{}.png", "x".repeat(300));
        let cut = blob_name(&name).unwrap();
        assert_eq!(cut.chars().count(), NAME_LIMIT);
        assert!(cut.ends_with(".png"));
        let name = "é".repeat(300);
        assert_eq!(blob_name(&name).unwrap().chars().count(), NAME_LIMIT);
        // Not an extension: too long to be one.
        let name = format!("{}.{}", "x".repeat(300), "y".repeat(50));
        assert_eq!(blob_name(&name).unwrap().chars().count(), NAME_LIMIT);
    }

    #[test]
    fn an_upload_without_a_name_gets_one_from_its_media_type() {
        assert_eq!(upload_name(None, "image/png"), "attachment.png");
        assert_eq!(
            upload_name(Some("   "), "application/pdf"),
            "attachment.pdf"
        );
        assert_eq!(upload_name(None, "image/jpeg; q=1"), "attachment.jpg");
        assert_eq!(upload_name(None, "x-unknown/thing"), "attachment");
        assert_eq!(upload_name(None, "garbage\r\n"), "attachment");
        assert_eq!(upload_name(Some("report.pdf"), "image/png"), "report.pdf");
    }
}

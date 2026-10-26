//! The `multipart/form-data` body of a message with files.
//!
//! `twilight-http` can upload files, but it writes each part with a file name and no
//! `Content-Type`, and the media type of an attachment is part of what the plugin promises
//! ("with the right file name and media type"). So the body is built here, to Discord's
//! documented shape (`payload_json` and `files[n]`,
//! <https://discord.com/developers/docs/reference#uploading-files>), and handed to the library
//! as a raw request, which keeps its rate limiting, authorization, and error handling.

use serde_json::json;
use twilight_model::channel::message::{AllowedMentions, MessageFlags};

use crate::client::{NewFile, NewMessage};

/// A built body and the `Content-Type` that names its boundary.
pub(super) struct Multipart {
    pub content_type: String,
    pub body: Vec<u8>,
}

/// The `payload_json` of `message`: its text (when there is any), nobody to ping, no embeds, and
/// the files as attachments `0..n`, matching `files[n]`.
fn payload(message: &NewMessage) -> Vec<u8> {
    let mut payload = json!({
        "allowed_mentions": AllowedMentions::default(),
        "flags": MessageFlags::SUPPRESS_EMBEDS.bits(),
        "attachments": message
            .files
            .iter()
            .enumerate()
            .map(|(index, file)| json!({"id": index, "filename": file.name}))
            .collect::<Vec<_>>(),
    });
    if !message.content.is_empty() {
        payload["content"] = json!(message.content);
    }
    serde_json::to_vec(&payload).expect("a JSON value always serializes")
}

/// A random boundary: 24 alphanumerics, so that it can't appear in the data except by a chance
/// of one in 62^24, which [`build`] also checks against.
fn boundary() -> String {
    std::iter::repeat_with(fastrand::alphanumeric)
        .take(24)
        .collect()
}

/// The body for creating `message`.
pub(super) fn build(message: &NewMessage) -> Multipart {
    let json = payload(message);
    let mut boundary = boundary();
    // A boundary inside the data would end a part early. Draw again in the (astronomically
    // unlikely) case it is: every byte of the data is checked once per draw.
    while contains(&json, &boundary) || message.files.iter().any(|f| contains(&f.bytes, &boundary))
    {
        boundary = self::boundary();
    }
    let size = json.len()
        + message
            .files
            .iter()
            .map(|file| file.bytes.len() + file.name.len() + file.media_type.len() + 200)
            .sum::<usize>()
        + 300;
    let mut body = Vec::with_capacity(size);
    part_header(
        &mut body,
        &boundary,
        "payload_json",
        None,
        "application/json",
    );
    body.extend_from_slice(&json);
    body.extend_from_slice(b"\r\n");
    for (index, file) in message.files.iter().enumerate() {
        file_part(&mut body, &boundary, index, file);
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Multipart {
        content_type: format!("multipart/form-data; boundary={boundary}"),
        body,
    }
}

fn file_part(body: &mut Vec<u8>, boundary: &str, index: usize, file: &NewFile) {
    part_header(
        body,
        boundary,
        &format!("files[{index}]"),
        Some(&file.name),
        &file.media_type,
    );
    body.extend_from_slice(&file.bytes);
    body.extend_from_slice(b"\r\n");
}

fn part_header(
    body: &mut Vec<u8>,
    boundary: &str,
    name: &str,
    filename: Option<&str>,
    media_type: &str,
) {
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"").as_bytes(),
    );
    if let Some(filename) = filename {
        body.extend_from_slice(format!("; filename=\"{filename}\"").as_bytes());
    }
    body.extend_from_slice(format!("\r\nContent-Type: {media_type}\r\n\r\n").as_bytes());
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    fn file(name: &str, media_type: &str, bytes: &[u8]) -> NewFile {
        NewFile {
            name: name.to_owned(),
            media_type: media_type.to_owned(),
            bytes: Bytes::copy_from_slice(bytes),
        }
    }

    /// The parts of `body` split at `boundary`: the header text, and the data.
    fn parts(multipart: &Multipart) -> Vec<(String, Vec<u8>)> {
        let boundary = multipart
            .content_type
            .strip_prefix("multipart/form-data; boundary=")
            .expect("the content type names the boundary");
        let delimiter = format!("--{boundary}");
        let text = &multipart.body;
        let mut parts = Vec::new();
        let mut rest = &text[..];
        let start = delimiter.as_bytes();
        assert!(rest.starts_with(start), "begins with the boundary");
        rest = &rest[start.len()..];
        loop {
            if rest.starts_with(b"--\r\n") {
                assert_eq!(rest, b"--\r\n", "nothing follows the closing boundary");
                return parts;
            }
            let rest_str = rest;
            assert!(rest_str.starts_with(b"\r\n"));
            let after = &rest_str[2..];
            let split = after
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .expect("headers end");
            let headers = String::from_utf8(after[..split].to_vec()).unwrap();
            let data = &after[split + 4..];
            let next = {
                let marker = format!("\r\n{delimiter}");
                data.windows(marker.len())
                    .position(|w| w == marker.as_bytes())
                    .expect("a part ends with the boundary")
            };
            parts.push((headers, data[..next].to_vec()));
            rest = &data[next + 2 + delimiter.len()..];
        }
    }

    #[test]
    fn the_body_has_the_payload_then_each_file_with_its_name_and_type() {
        let message = NewMessage {
            content: "here you go".to_owned(),
            files: vec![
                file("photo.png", "image/png", b"\x89PNG\r\n\x1a\n"),
                file("notes.txt", "text/plain", b"line one\r\nline two"),
            ],
        };
        let multipart = build(&message);
        let parts = parts(&multipart);
        assert_eq!(parts.len(), 3);
        assert_eq!(
            parts[0].0,
            "Content-Disposition: form-data; name=\"payload_json\"\r\n\
             Content-Type: application/json"
        );
        let payload: serde_json::Value = serde_json::from_slice(&parts[0].1).unwrap();
        assert_eq!(
            payload,
            json!({
                "content": "here you go",
                "allowed_mentions": {"parse": []},
                "flags": 4,
                "attachments": [
                    {"id": 0, "filename": "photo.png"},
                    {"id": 1, "filename": "notes.txt"},
                ],
            })
        );
        assert_eq!(
            parts[1].0,
            "Content-Disposition: form-data; name=\"files[0]\"; filename=\"photo.png\"\r\n\
             Content-Type: image/png"
        );
        assert_eq!(parts[1].1, b"\x89PNG\r\n\x1a\n");
        assert_eq!(
            parts[2].0,
            "Content-Disposition: form-data; name=\"files[1]\"; filename=\"notes.txt\"\r\n\
             Content-Type: text/plain"
        );
        assert_eq!(parts[2].1, b"line one\r\nline two");
    }

    #[test]
    fn a_message_with_no_text_has_no_content_field() {
        let multipart = build(&NewMessage {
            content: String::new(),
            files: vec![file("a.bin", "application/octet-stream", b"x")],
        });
        let payload: serde_json::Value = serde_json::from_slice(&parts(&multipart)[0].1).unwrap();
        assert!(payload.get("content").is_none(), "{payload}");
    }

    #[test]
    fn text_with_quotes_and_non_ascii_is_json_escaped_and_names_stay_whole() {
        let multipart = build(&NewMessage {
            content: "say \"hi\"\n\u{1f600}".to_owned(),
            files: vec![file("r\u{e9}sum\u{e9}.pdf", "application/pdf", b"%PDF")],
        });
        let parts = parts(&multipart);
        let payload: serde_json::Value = serde_json::from_slice(&parts[0].1).unwrap();
        assert_eq!(payload["content"], "say \"hi\"\n\u{1f600}");
        assert!(parts[1].0.contains("filename=\"r\u{e9}sum\u{e9}.pdf\""));
    }

    #[test]
    fn a_boundary_in_the_data_is_never_used() {
        // Every draw is checked against the data, so a file that contains a boundary-like run
        // can't end its part early: parse the body back and the file is whole.
        let data = b"--abcdefghijklmnopqrstuvwx\r\n--".repeat(10);
        let multipart = build(&NewMessage {
            content: String::new(),
            files: vec![file("x.bin", "application/octet-stream", &data)],
        });
        assert_eq!(parts(&multipart)[1].1, data);
    }

    #[test]
    fn boundaries_differ_between_messages() {
        let message = NewMessage {
            content: "x".to_owned(),
            files: Vec::new(),
        };
        assert_ne!(build(&message).content_type, build(&message).content_type);
    }
}

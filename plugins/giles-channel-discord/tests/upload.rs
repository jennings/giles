//! `RealDiscord::create_message` with files against a REST server of the test's own on loopback:
//! the multipart request that goes out (headers, authorization, the JSON part, each file with its
//! name and media type), and what comes back. Nothing here reaches Discord.

mod loopback;

use bytes::Bytes;
use giles_channel_discord::client::{Discord, NewFile, NewMessage, RestError};
use loopback::rest::{Request, Response, RestServer};
use loopback::{TOKEN, WAIT, rest_client};
use serde_json::{Value, json};

const CHANNEL: u64 = 319_674_150_115_610_528;

fn file(name: &str, media_type: &str, bytes: &[u8]) -> NewFile {
    NewFile {
        name: name.to_owned(),
        media_type: media_type.to_owned(),
        bytes: Bytes::copy_from_slice(bytes),
    }
}

async fn upload(
    client: &impl Discord,
    content: &str,
    files: Vec<NewFile>,
) -> Result<u64, RestError> {
    tokio::time::timeout(
        WAIT,
        client.create_message(
            CHANNEL,
            NewMessage {
                content: content.to_owned(),
                files,
            },
        ),
    )
    .await
    .expect("an answer in time")
}

/// One part of a multipart body: its header lines (lower-cased names) and its data.
#[derive(Debug)]
struct Part {
    headers: Vec<(String, String)>,
    data: Vec<u8>,
}

impl Part {
    fn header(&self, name: &str) -> &str {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map_or("", |(_, value)| value.as_str())
    }
}

/// Splits a `multipart/form-data` request on its boundary, as a server does, and checks the
/// framing on the way: the content type names the boundary, the body begins with it, every part
/// has its headers, a blank line, its data, and CRLF before the next boundary, and the closing
/// boundary ends the body.
fn parts(request: &Request) -> Vec<Part> {
    let content_type = request.header("content-type").expect("a content type");
    let boundary = content_type
        .strip_prefix("multipart/form-data; boundary=")
        .unwrap_or_else(|| panic!("not multipart: {content_type}"));
    let delimiter = format!("--{boundary}").into_bytes();
    let body = &request.body;
    assert!(body.starts_with(&delimiter), "begins with the boundary");
    let mut rest = &body[delimiter.len()..];
    let mut parts = Vec::new();
    loop {
        if rest == b"--\r\n" || rest == b"--" {
            return parts;
        }
        let after = rest.strip_prefix(b"\r\n").expect("CRLF after a boundary");
        let head_end = find(after, b"\r\n\r\n").expect("headers end");
        let headers = String::from_utf8(after[..head_end].to_vec())
            .unwrap()
            .split("\r\n")
            .filter_map(|line| line.split_once(": "))
            .map(|(name, value)| (name.to_ascii_lowercase(), value.to_owned()))
            .collect();
        let data = &after[head_end + 4..];
        let mut next = delimiter.clone();
        next.splice(0..0, *b"\r\n");
        let end = find(data, &next).expect("a part ends with the boundary");
        parts.push(Part {
            headers,
            data: data[..end].to_vec(),
        });
        rest = &data[end + next.len()..];
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

async fn accepting() -> RestServer {
    RestServer::start(|_, _| Response::json(200, &json!({"id": "1234567890123456789"}))).await
}

#[tokio::test]
async fn files_go_out_as_a_multipart_message_with_the_same_protections_as_text() {
    let server = accepting().await;
    let client = rest_client(&server.host);
    // Bytes that would break a naive framing: CRLFs, NULs, and something boundary-like.
    let binary: Vec<u8> = (0..=255_u8).chain(*b"\r\n--boundary\r\n\0\0").collect();
    let id = upload(
        &client,
        "here are **two**",
        vec![
            file("photo.png", "image/png", &binary),
            file("r\u{e9}sum\u{e9}.pdf", "application/pdf", b"%PDF-1.7"),
        ],
    )
    .await;
    assert_eq!(id, Ok(1_234_567_890_123_456_789));

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(
        request.target,
        format!("/api/v10/channels/{CHANNEL}/messages")
    );
    assert_eq!(
        request.header("authorization"),
        Some(format!("Bot {TOKEN}").as_str())
    );
    assert_eq!(
        request.header("content-length"),
        Some(request.body.len().to_string().as_str())
    );
    assert!(request.header("user-agent").is_some());

    let parts = parts(request);
    assert_eq!(parts.len(), 3);
    assert_eq!(
        parts[0].header("content-disposition"),
        "form-data; name=\"payload_json\""
    );
    assert_eq!(parts[0].header("content-type"), "application/json");
    // `parse: []` and `flags: 4` (SUPPRESS_EMBEDS) are in the JSON part, as in a text message.
    let payload: Value = serde_json::from_slice(&parts[0].data).unwrap();
    assert_eq!(
        payload,
        json!({
            "content": "here are **two**",
            "allowed_mentions": {"parse": []},
            "flags": 4,
            "attachments": [
                {"id": 0, "filename": "photo.png"},
                {"id": 1, "filename": "r\u{e9}sum\u{e9}.pdf"},
            ],
        })
    );

    assert_eq!(
        parts[1].header("content-disposition"),
        "form-data; name=\"files[0]\"; filename=\"photo.png\""
    );
    assert_eq!(parts[1].header("content-type"), "image/png");
    assert_eq!(parts[1].data, binary, "the file arrives byte for byte");
    assert_eq!(
        parts[2].header("content-disposition"),
        "form-data; name=\"files[1]\"; filename=\"r\u{e9}sum\u{e9}.pdf\""
    );
    assert_eq!(parts[2].header("content-type"), "application/pdf");
    assert_eq!(parts[2].data, b"%PDF-1.7");

    // The token is the header's and nobody else's.
    assert!(find(&request.body, TOKEN.as_bytes()).is_none());
}

#[tokio::test]
async fn a_message_of_files_alone_has_no_content_field() {
    let server = accepting().await;
    let client = rest_client(&server.host);
    upload(
        &client,
        "",
        vec![file("a.bin", "application/octet-stream", b"x")],
    )
    .await
    .unwrap();
    let parts = parts(&server.requests()[0]);
    let payload: Value = serde_json::from_slice(&parts[0].data).unwrap();
    assert!(payload.get("content").is_none(), "{payload}");
    assert_eq!(payload["allowed_mentions"], json!({"parse": []}));
    assert_eq!(payload["flags"], 4);
    assert_eq!(parts[1].header("content-type"), "application/octet-stream");
}

#[tokio::test]
async fn a_large_file_arrives_whole() {
    let server = accepting().await;
    let client = rest_client(&server.host);
    let big: Vec<u8> = (0..3 * 1024 * 1024_u32).map(|n| (n % 251) as u8).collect();
    upload(
        &client,
        "",
        vec![file("big.bin", "application/octet-stream", &big)],
    )
    .await
    .unwrap();
    let parts = parts(&server.requests()[0]);
    assert_eq!(parts[1].data.len(), big.len());
    assert!(parts[1].data == big);
}

#[tokio::test]
async fn a_text_message_is_still_json_and_a_file_message_is_not() {
    let server = accepting().await;
    let client = rest_client(&server.host);
    upload(&client, "just text", Vec::new()).await.unwrap();
    upload(&client, "", vec![file("a.txt", "text/plain", b"a")])
        .await
        .unwrap();
    let requests = server.requests();
    assert_eq!(requests[0].header("content-type"), Some("application/json"));
    assert!(
        requests[1]
            .header("content-type")
            .unwrap()
            .starts_with("multipart/form-data; boundary=")
    );
}

#[tokio::test]
async fn too_large_is_413_or_an_error_code_and_nothing_else_is_guessed() {
    let server = RestServer::start(|_, n| match n {
        0 => Response::json(
            413,
            &json!({"message": "Request entity too large", "code": 40005}),
        ),
        1 => Response::json(
            400,
            &json!({"message": "File exceeds maximum size", "code": 50045}),
        ),
        _ => Response::json(400, &json!({"message": "Invalid Form Body", "code": 50035})),
    })
    .await;
    let client = rest_client(&server.host);
    let one = || vec![file("a.bin", "application/octet-stream", b"x")];
    assert_eq!(upload(&client, "", one()).await, Err(RestError::TooLarge));
    assert_eq!(
        upload(&client, "", one()).await,
        Err(RestError::Api {
            status: 400,
            code: Some(50_045)
        })
    );
    assert_eq!(
        upload(&client, "", one()).await,
        Err(RestError::Api {
            status: 400,
            code: Some(50_035)
        })
    );
}

#[tokio::test]
async fn a_rejected_token_is_unauthorized_and_no_later_upload_is_sent() {
    let server = RestServer::start(|_, _| {
        Response::json(401, &json!({"message": "401: Unauthorized", "code": 0}))
    })
    .await;
    let client = rest_client(&server.host);
    let one = || vec![file("a.bin", "application/octet-stream", b"x")];
    assert_eq!(
        upload(&client, "", one()).await,
        Err(RestError::Unauthorized)
    );
    assert_eq!(
        upload(&client, "", one()).await,
        Err(RestError::Unauthorized)
    );
    assert_eq!(server.requests().len(), 1, "nothing more asked of Discord");
}

#[tokio::test]
async fn a_429_is_waited_out_and_the_same_upload_sent_again() {
    let server = RestServer::start(|_, n| {
        if n == 0 {
            Response::json(
                429,
                &json!({"message": "You are being rate limited.", "retry_after": 0.2, "global": false}),
            )
            .header("retry-after", "1")
            .header("x-ratelimit-scope", "user")
        } else {
            Response::json(200, &json!({"id": "42"}))
        }
    })
    .await;
    let client = rest_client(&server.host);
    let result = upload(
        &client,
        "x",
        vec![file("a.txt", "text/plain", b"same bytes")],
    )
    .await;
    assert_eq!(result, Ok(42));
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let first = parts(&requests[0]);
    let second = parts(&requests[1]);
    assert_eq!(first[1].data, second[1].data);
    assert_eq!(first[0].data, second[0].data);
}

#[tokio::test]
async fn a_server_that_is_not_there_is_a_transport_error() {
    let client = rest_client("127.0.0.1:9");
    assert_eq!(
        upload(&client, "", vec![file("a.txt", "text/plain", b"x")]).await,
        Err(RestError::Transport)
    );
}

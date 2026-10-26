//! Sending files: what is uploaded, in how many messages, in what order, what is refused before
//! anything is posted, and what a failure after some messages means.

use bytes::Bytes;
use giles_plugin_sdk::giles_types::MediaRef;

use super::*;
use crate::client::{NewFile, NewMessage};

const MIB: u64 = 1024 * 1024;

impl Harness {
    /// Stores `bytes` for a send, as the core's `PutBlob` does.
    fn stage(&self, bytes: &[u8], media_type: &str, name: Option<&str>) -> MediaRef {
        self.media.stage(bytes, media_type, name)
    }

    /// Like [`stage`](Self::stage), claiming to be `size` bytes long: the core's word for it,
    /// which the plan goes by, without holding that many bytes.
    fn stage_claiming(&self, size: u64, name: &str) -> MediaRef {
        let mut media = self.stage(b"x", "application/octet-stream", Some(name));
        media.size = size;
        media
    }

    async fn send_files(
        &self,
        target: SendTarget,
        markdown: &str,
        attachments: Vec<MediaRef>,
    ) -> Result<MessageId> {
        self.channel
            .send(OutboundMessage {
                target,
                markdown: markdown.to_owned(),
                attachments,
                reply_to: None,
            })
            .await
    }

    /// Every message the fake was asked to post: its text, and its files' names.
    fn shape(&self) -> Vec<(String, Vec<String>)> {
        self.client
            .posted()
            .into_iter()
            .map(|(_, message)| {
                (
                    message.content,
                    message.files.into_iter().map(|file| file.name).collect(),
                )
            })
            .collect()
    }
}

fn named(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

#[tokio::test(start_paused = true)]
async fn a_file_is_uploaded_with_its_name_type_and_bytes_and_the_text_is_its_body() {
    let h = Harness::start().await;
    let photo = h.stage(b"\x89PNG-bytes", "image/png", Some("photo.png"));
    let id = h
        .send_files(conversation(CHANNEL), "look at **this**", vec![photo])
        .await
        .unwrap();
    assert_eq!(id.as_str(), "5001");
    assert_eq!(
        h.client.posted(),
        [(
            CHANNEL,
            NewMessage {
                content: "look at **this**".to_owned(),
                files: vec![NewFile {
                    name: "photo.png".to_owned(),
                    media_type: "image/png".to_owned(),
                    bytes: Bytes::from_static(b"\x89PNG-bytes"),
                }],
            }
        )]
    );
}

#[tokio::test(start_paused = true)]
async fn a_file_with_no_text_is_a_message_of_files_only() {
    let h = Harness::start().await;
    let doc = h.stage(b"%PDF", "application/pdf", Some("report.pdf"));
    h.send_files(recipient_of(&USER.to_string()), "", vec![doc])
        .await
        .unwrap();
    assert_eq!(h.shape(), [(String::new(), named(&["report.pdf"]))]);
    assert_eq!(h.client.dms(), [USER]);
}

#[tokio::test(start_paused = true)]
async fn names_and_types_that_could_break_the_upload_are_made_safe() {
    let h = Harness::start().await;
    let files = vec![
        h.stage(b"a", "image/png", None),
        h.stage(
            b"b",
            "text/plain\r\nX-Evil: 1",
            Some("a\"; filename=\"b\r\n.txt"),
        ),
        h.stage(b"c", "who knows", Some("../../etc/passwd")),
        h.stage(b"d", "Image/JPEG; q=1", Some("   ")),
    ];
    h.send_files(conversation(CHANNEL), "", files)
        .await
        .unwrap();
    let message = &h.client.posted()[0].1;
    let seen: Vec<(&str, &str)> = message
        .files
        .iter()
        .map(|file| (file.name.as_str(), file.media_type.as_str()))
        .collect();
    assert_eq!(
        seen,
        [
            ("attachment.png", "image/png"),
            ("a_; filename=_b.txt", "application/octet-stream"),
            (".._.._etc_passwd", "application/octet-stream"),
            ("attachment.jpg", "image/jpeg"),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn ten_files_go_in_one_message_and_the_eleventh_in_a_follow_up() {
    let h = Harness::start().await;
    let files: Vec<_> = (0..11)
        .map(|n| h.stage(b"x", "image/png", Some(&format!("f{n:02}.png"))))
        .collect();
    let id = h
        .send_files(conversation(CHANNEL), "all of them", files)
        .await
        .unwrap();
    let shape = h.shape();
    assert_eq!(shape.len(), 2);
    assert_eq!(shape[0].0, "all of them");
    assert_eq!(
        shape[0].1,
        (0..10).map(|n| format!("f{n:02}.png")).collect::<Vec<_>>()
    );
    assert_eq!(shape[1], (String::new(), named(&["f10.png"])));
    assert_eq!(id.as_str(), "5002", "the id of the last message");
}

#[tokio::test(start_paused = true)]
async fn files_that_would_pass_the_request_limit_are_split_between_messages() {
    let h = Harness::start().await;
    let files = vec![
        h.stage_claiming(12 * MIB, "a.bin"),
        h.stage_claiming(12 * MIB, "b.bin"),
        h.stage_claiming(12 * MIB, "c.bin"),
    ];
    h.send_files(conversation(CHANNEL), "", files)
        .await
        .unwrap();
    assert_eq!(
        h.shape(),
        [
            (String::new(), named(&["a.bin", "b.bin"])),
            (String::new(), named(&["c.bin"]))
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn text_longer_than_a_message_goes_first_and_the_files_follow_it() {
    let h = Harness::start().await;
    let (text, parts) = long_text();
    let file = h.stage(b"x", "image/png", Some("end.png"));
    h.send_files(conversation(CHANNEL), &text, vec![file])
        .await
        .unwrap();
    let shape = h.shape();
    assert_eq!(shape.len(), parts.len() + 1);
    assert_eq!(
        shape[..parts.len()]
            .iter()
            .map(|(text, _)| text.as_str())
            .collect::<Vec<_>>(),
        parts
    );
    assert!(
        shape[..parts.len()]
            .iter()
            .all(|(_, files)| files.is_empty())
    );
    assert_eq!(shape[parts.len()], (String::new(), named(&["end.png"])));
}

#[tokio::test(start_paused = true)]
async fn a_file_over_discords_limit_refuses_the_whole_send_before_anything_is_posted() {
    let h = Harness::start().await;
    let fine = h.stage(b"x", "image/png", Some("fine.png"));
    let big = h.stage_claiming(20 * MIB + 1, "huge.mov");
    let error = h
        .send_files(
            recipient_of(&USER.to_string()),
            "with text",
            vec![fine, big],
        )
        .await
        .unwrap_err();
    let Error::InvalidRequest(message) = &error else {
        panic!("not InvalidRequest: {error:?}");
    };
    assert!(message.contains("huge.mov"), "{message}");
    assert!(
        message.contains(&(20 * MIB).to_string()),
        "the limit: {message}"
    );
    assert!(!error.is_retryable());
    assert!(h.client.posted().is_empty(), "not even the text");
    assert!(h.client.dms().is_empty(), "no DM opened for nothing");

    // A file of exactly the limit goes.
    let exact = h.stage_claiming(20 * MIB, "exact.bin");
    h.send_files(conversation(CHANNEL), "", vec![exact])
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn max_blob_below_discords_limit_is_the_limit() {
    let h = Harness::start_limited(2 * MIB).await;
    let big = h.stage_claiming(2 * MIB + 1, "big.bin");
    let error = h
        .send_files(conversation(CHANNEL), "", vec![big])
        .await
        .unwrap_err();
    let Error::InvalidRequest(message) = &error else {
        panic!("not InvalidRequest: {error:?}");
    };
    assert!(message.contains("max_blob"), "{message}");
    assert!(h.client.posted().is_empty());
    let ok = h.stage_claiming(2 * MIB, "ok.bin");
    h.send_files(conversation(CHANNEL), "", vec![ok])
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn the_bytes_are_checked_as_well_as_the_size_the_core_gave() {
    let h = Harness::start_limited(1024).await;
    let liar = h.stage(&[7; 2048], "application/octet-stream", Some("liar.bin"));
    let mut liar = liar;
    liar.size = 10;
    let error = h
        .send_files(conversation(CHANNEL), "", vec![liar])
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    assert!(h.client.posted().is_empty(), "nothing reached Discord");
}

#[tokio::test(start_paused = true)]
async fn a_blob_the_core_never_put_is_invalid_and_posts_nothing() {
    let h = Harness::start().await;
    let gone = MediaRef {
        id: giles_plugin_sdk::giles_types::MediaId::from_digest([9; 32]),
        media_type: "image/png".into(),
        size: 1,
        name: Some("gone.png".into()),
    };
    let error = h
        .send_files(conversation(CHANNEL), "caption", vec![gone])
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    assert!(h.client.posted().is_empty());
}

#[tokio::test(start_paused = true)]
async fn what_discord_says_about_a_too_large_upload_is_invalid_and_not_retried() {
    for refusal in [RestError::TooLarge, api(413, 40_005), api(400, 50_045)] {
        let h = Harness::start().await;
        let file = h.stage(b"x", "image/png", Some("a.png"));
        h.client.script_messages([Err(refusal.clone())]);
        let error = h
            .send_files(recipient_of(&USER.to_string()), "x", vec![file.clone()])
            .await
            .unwrap_err();
        assert!(
            matches!(error, Error::InvalidRequest(_)) && !error.is_retryable(),
            "{refusal:?}: {error:?}"
        );
        // Forgotten: the same send, once Discord takes it, goes out whole.
        h.send_files(recipient_of(&USER.to_string()), "x", vec![file])
            .await
            .unwrap();
        assert_eq!(h.shape().len(), 2, "{refusal:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_failure_midway_leaves_the_files_before_it_and_a_retry_carries_on() {
    let h = Harness::start().await;
    let files: Vec<_> = (0..11)
        .map(|n| h.stage(b"x", "image/png", Some(&format!("f{n:02}.png"))))
        .collect();
    // The text and ten files go out; the follow-up with the eleventh fails.
    h.client.script_messages([Ok(1), Err(RestError::Transport)]);
    let error = h
        .send_files(recipient_of(&USER.to_string()), "pics", files.clone())
        .await
        .unwrap_err();
    assert!(error.is_retryable());
    let message = unavailable(&error);
    assert!(
        message.contains("1 of 2 messages had already been sent"),
        "{message}"
    );
    assert!(
        message.contains("a retry carries on after them"),
        "{message}"
    );

    // The outbox sends it all again: only the follow-up is posted, and it carries the file it did
    // before.
    let id = h
        .send_files(recipient_of(&USER.to_string()), "pics", files.clone())
        .await
        .unwrap();
    let shape = h.shape();
    assert_eq!(
        shape.len(),
        3,
        "the first message once, the follow-up twice"
    );
    assert_eq!(shape[1], shape[2]);
    assert_eq!(shape[2], (String::new(), named(&["f10.png"])));
    assert_eq!(id.as_str(), "5001");

    // Finished: the same send again is a new message, and goes out in full.
    h.send_files(recipient_of(&USER.to_string()), "pics", files)
        .await
        .unwrap();
    assert_eq!(h.shape().len(), 5);
}

#[tokio::test(start_paused = true)]
async fn another_file_is_another_send_not_a_retry() {
    let h = Harness::start().await;
    let (text, parts) = long_text();
    let first = h.stage(b"one", "image/png", Some("same.png"));
    let other = h.stage(b"two", "image/png", Some("same.png"));
    // The first part goes out, the second fails.
    h.client.script_messages([Ok(1), Err(RestError::Transport)]);
    h.send_files(recipient_of(&USER.to_string()), &text, vec![first])
        .await
        .unwrap_err();
    // The same text with a different file (same name, other content) is not a retry of it: it
    // starts at the first part.
    h.send_files(recipient_of(&USER.to_string()), &text, vec![other])
        .await
        .unwrap();
    let texts: Vec<_> = h
        .client
        .posted()
        .into_iter()
        .map(|(_, m)| m.content)
        .collect();
    assert_eq!(texts[2..2 + parts.len()], parts[..]);
}

#[tokio::test(start_paused = true)]
async fn a_failed_reply_with_files_says_what_went_out_and_promises_no_retry() {
    let h = Harness::start().await;
    let (text, parts) = long_text();
    let file = h.stage(b"x", "image/png", Some("a.png"));
    h.client.script_messages([Ok(1), Err(RestError::Transport)]);
    let error = h
        .send_files(conversation(CHANNEL), &text, vec![file])
        .await
        .unwrap_err();
    let message = unavailable(&error);
    assert!(
        message.contains(&format!(
            "1 of {} messages had already been sent",
            parts.len() + 1
        )),
        "{message}"
    );
    assert!(!message.contains("a retry carries on"), "{message}");
}

#[tokio::test(start_paused = true)]
async fn the_messages_of_two_sends_with_files_never_interleave() {
    let h = Harness::start().await;
    h.client.delay_rest(Duration::from_secs(5));
    let files_a: Vec<_> = (0..11)
        .map(|n| h.stage(b"a", "image/png", Some(&format!("a{n:02}.png"))))
        .collect();
    let files_b: Vec<_> = (0..11)
        .map(|n| h.stage(b"b", "image/png", Some(&format!("b{n:02}.png"))))
        .collect();
    let (a, b) = tokio::join!(
        h.send_files(conversation(CHANNEL), "a", files_a),
        h.send_files(conversation(CHANNEL), "b", files_b),
    );
    a.unwrap();
    b.unwrap();
    let first_letters: Vec<char> = h
        .shape()
        .iter()
        .map(|(_, files)| files[0].chars().next().unwrap())
        .collect();
    assert!(
        first_letters == ['a', 'a', 'b', 'b'] || first_letters == ['b', 'b', 'a', 'a'],
        "{first_letters:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_401_on_an_upload_rejects_the_token_and_nothing_more_is_sent() {
    let h = Harness::start().await;
    let file = h.stage(b"x", "image/png", Some("a.png"));
    h.client.script_messages([Err(RestError::Unauthorized)]);
    let error = h
        .send_files(conversation(CHANNEL), "x", vec![file.clone()])
        .await
        .unwrap_err();
    assert!(unavailable(&error).contains("rejected the bot token"));
    h.send_files(conversation(CHANNEL), "x", vec![file])
        .await
        .unwrap_err();
    assert_eq!(h.client.posted().len(), 1, "nothing more after a 401");
}

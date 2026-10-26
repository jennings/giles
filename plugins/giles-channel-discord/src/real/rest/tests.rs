//! The REST glue's pure parts: a page of history. Its errors need a response from a server, in
//! `tests/rest.rs`.

use serde_json::json;

use super::parse_page;
use crate::client::{Page, RestError};

fn message(id: u64, content: &str) -> serde_json::Value {
    json!({
        "id": id.to_string(), "channel_id": "319674150115610528", "type": 0,
        "content": content, "timestamp": "2026-10-24T12:00:00.000000+00:00",
        "edited_timestamp": null, "tts": false, "mention_everyone": false, "mentions": [],
        "mention_roles": [], "pinned": false, "flags": 0, "embeds": [], "attachments": [],
        "author": {
            "id": "11111111111111111", "username": "alice", "discriminator": "0",
            "global_name": null, "avatar": null
        }
    })
}

#[test]
fn a_page_comes_newest_first_on_the_wire_and_oldest_first_out() {
    let body = json!([message(30, "c"), message(20, "b"), message(10, "a")]).to_string();
    let page = parse_page(body.as_bytes()).unwrap();
    assert_eq!(
        page.messages
            .iter()
            .map(|m| (m.id, m.content.as_str()))
            .collect::<Vec<_>>(),
        [(10, "a"), (20, "b"), (30, "c")]
    );
}

#[test]
fn a_message_that_does_not_deserialize_costs_only_itself() {
    let body = json!([message(30, "c"), {"id": "20", "content": "no author or channel"}, message(10, "a")])
        .to_string();
    let page = parse_page(body.as_bytes()).unwrap();
    assert_eq!(
        page.messages.iter().map(|m| m.id).collect::<Vec<_>>(),
        [10, 30]
    );
    // But the page was three long, and that is what paging goes by.
    assert_eq!((page.received, page.last), (3, Some(30)));
}

#[test]
fn the_highest_id_of_a_page_counts_even_if_that_message_cannot_be_read() {
    let body =
        json!([{"id": "40", "content": "no author or channel"}, message(30, "c")]).to_string();
    let page = parse_page(body.as_bytes()).unwrap();
    assert_eq!(page.messages.len(), 1);
    assert_eq!((page.received, page.last), (2, Some(40)));
}

#[test]
fn an_empty_page_is_a_page() {
    assert_eq!(parse_page(b"[]").unwrap(), Page::default());
}

#[test]
fn a_body_that_is_not_a_list_is_a_transport_error() {
    assert_eq!(parse_page(b"<html>").unwrap_err(), RestError::Transport);
    assert_eq!(
        parse_page(br#"{"message": "nope"}"#).unwrap_err(),
        RestError::Transport
    );
}

#[tokio::test(start_paused = true)]
async fn a_call_held_back_for_a_minute_is_rate_limited() {
    let started = tokio::time::Instant::now();
    let held = super::bounded(std::future::pending::<Result<(), RestError>>()).await;
    assert_eq!(held, Err(RestError::RateLimited));
    assert_eq!(started.elapsed(), super::CALL_TIMEOUT);
    // A call that finishes in time is left alone, error or not.
    assert_eq!(super::bounded(async { Ok(1) }).await, Ok(1));
    assert_eq!(
        super::bounded(async { Err::<(), _>(RestError::Unauthorized) }).await,
        Err(RestError::Unauthorized)
    );
}

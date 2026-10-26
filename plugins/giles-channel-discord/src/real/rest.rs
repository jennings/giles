//! The REST side of [`RealDiscord`](super::RealDiscord): requests on `twilight-http`, and its
//! errors in the plugin's terms.
//!
//! `twilight-http` queues every request behind Discord's rate limits and sends one that got a 429
//! again, with no retry count, so a call can take as long as Discord keeps limiting. [`bounded`]
//! is the plugin's bound.

use std::future::Future;
use std::time::Duration;

use serde::Deserialize;
use twilight_http::Client;
use twilight_http::api_error::ApiError;
use twilight_http::error::{Error, ErrorType};
use twilight_http::response::Response;
use twilight_model::channel::Message;
use twilight_model::channel::message::{AllowedMentions, MessageFlags};
use twilight_model::id::Id;

use super::message;
use crate::client::{DiscordMessage, NewMessage, Page, RestError};

/// How long one REST call may take, queueing behind a rate limit included.
pub(super) const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Runs `call` for at most [`CALL_TIMEOUT`]. A call that outlasts it was held back by a rate limit
/// (the library has its own, shorter timeout for a request in flight), so it is
/// [`RestError::RateLimited`].
pub(super) async fn bounded<T>(
    call: impl Future<Output = Result<T, RestError>>,
) -> Result<T, RestError> {
    tokio::time::timeout(CALL_TIMEOUT, call)
        .await
        .unwrap_or(Err(RestError::RateLimited))
}

/// The plugin's view of a library error. The error's own text is not logged: some variants carry
/// the response body.
pub(super) fn rest_error(error: &Error) -> RestError {
    let mapped = match error.kind() {
        ErrorType::Unauthorized => RestError::Unauthorized,
        ErrorType::Response { error, status, .. } => {
            if status.get() == 401 {
                RestError::Unauthorized
            } else if status.get() == 413 {
                RestError::TooLarge
            } else {
                RestError::Api {
                    status: status.get(),
                    code: match error {
                        ApiError::General(general) => u32::try_from(general.code).ok(),
                        _ => None,
                    },
                }
            }
        }
        // The library refused the request itself (a text over its limit): what Discord answers
        // to an invalid form body.
        ErrorType::Validation => RestError::Api {
            status: 400,
            code: Some(50_035),
        },
        _ => RestError::Transport,
    };
    tracing::debug!("a Discord REST call failed: {mapped}");
    mapped
}

/// `GET /channels/{channel}/messages?after={after}&limit={limit}`, oldest first.
///
/// A message that doesn't deserialize (the library's models are strict) is skipped with a
/// warning that names no content; the rest of the page is kept.
pub(super) async fn messages_after(
    http: &Client,
    channel: u64,
    after: u64,
    limit: u8,
) -> Result<Page, RestError> {
    // Id 0 is not a valid `Id`; Discord accepts 0 as the beginning of time, and 1 is as good.
    let (Some(channel), Some(after)) = (Id::new_checked(channel), Id::new_checked(after.max(1)))
    else {
        return Err(RestError::Api {
            status: 404,
            code: Some(10003),
        });
    };
    bounded(async {
        let response = http
            .channel_messages(channel)
            .after(after)
            .limit(u16::from(limit))
            .await
            .map_err(|e| rest_error(&e))?;
        let body = response.bytes().await.map_err(|_| RestError::Transport)?;
        parse_page(&body)
    })
    .await
}

/// A page as Discord sends it, newest first, in the plugin's types, oldest first, with the count
/// and the highest id of what Discord sent, read from the raw list.
fn parse_page(body: &[u8]) -> Result<Page, RestError> {
    let values: Vec<serde_json::Value> = serde_json::from_slice(body).map_err(|_| {
        tracing::warn!("a page of Discord messages wasn't a JSON list");
        RestError::Transport
    })?;
    let mut skipped = 0_usize;
    let received = values.len();
    let last = values
        .iter()
        .filter_map(|value| value.get("id")?.as_str()?.parse::<u64>().ok())
        .max();
    let mut messages: Vec<DiscordMessage> = values
        .into_iter()
        .filter_map(|value| match serde_json::from_value::<Message>(value) {
            Ok(message) => Some(message::convert(&message)),
            Err(_) => {
                skipped += 1;
                None
            }
        })
        .collect();
    if skipped > 0 {
        tracing::warn!("{skipped} Discord messages in a page couldn't be deserialized; skipped");
    }
    messages.sort_by_key(|message| message.id);
    Ok(Page {
        messages,
        received,
        last,
    })
}

/// The part of a created message or channel the plugin reads. Discord sends ids as strings.
#[derive(Deserialize)]
struct Created {
    id: String,
}

/// `POST /channels/{channel}/messages`, with no mention allowed to ping and no embed.
///
/// Only the id is read from the response: the library's models are strict, and a message Discord
/// accepted must not become an error (and a duplicate on retry) because another field changed.
pub(super) async fn create_message(
    http: &Client,
    channel: u64,
    message: &NewMessage,
) -> Result<u64, RestError> {
    let Some(channel) = Id::new_checked(channel) else {
        return Err(RestError::Api {
            status: 404,
            code: Some(10003),
        });
    };
    // No mention can ping: nothing to parse, no user or role listed, no reply to notify.
    let mentions = AllowedMentions::default();
    bounded(async {
        let response = http
            .create_message(channel)
            .content(&message.content)
            .allowed_mentions(Some(&mentions))
            .flags(MessageFlags::SUPPRESS_EMBEDS)
            .await
            .map_err(|e| rest_error(&e))?;
        created(response).await
    })
    .await
}

/// `POST /users/@me/channels`.
pub(super) async fn create_dm(http: &Client, user: u64) -> Result<u64, RestError> {
    let Some(user) = Id::new_checked(user) else {
        return Err(RestError::Api {
            status: 404,
            code: Some(10013),
        });
    };
    bounded(async {
        let response = http
            .create_private_channel(user)
            .await
            .map_err(|e| rest_error(&e))?;
        created(response).await
    })
    .await
}

/// The id in a successful response.
async fn created<T>(response: Response<T>) -> Result<u64, RestError> {
    let body = response.bytes().await.map_err(|_| RestError::Transport)?;
    serde_json::from_slice::<Created>(&body)
        .ok()
        .and_then(|created| created.id.parse().ok())
        .ok_or_else(|| {
            // Never the body: it is a message.
            tracing::warn!("a Discord REST response had no usable id");
            RestError::Transport
        })
}

#[cfg(test)]
mod tests;

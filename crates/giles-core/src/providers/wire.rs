//! Wire-format helpers shared by the built-in providers: media types, numbers in request bodies,
//! and the loop that turns a response's server-sent events into [`CompletionEvent`]s. Media
//! references are [`crate::media::reference`].

use std::collections::VecDeque;

use futures::{Stream, StreamExt};
use giles_types::{CompletionEvent, Error, Result};
use serde_json::{Number, Value};

use crate::providers::sse::SseEvent;

/// The `type/subtype` part of a media type, trimmed and lowercased: `Audio/OGG; codecs=opus`
/// gives `audio/ogg`.
pub fn media_essence(media_type: &str) -> String {
    media_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// A temperature as its shortest decimal form (`0.7`, not `0.699999988079071`).
/// [`Error::InvalidRequest`] for NaN or infinity.
pub fn temperature(temperature: f32) -> Result<Value> {
    temperature
        .to_string()
        .parse::<f64>()
        .ok()
        .and_then(Number::from_f64)
        .map(Value::Number)
        .ok_or_else(|| Error::InvalidRequest(format!("invalid temperature {temperature}")))
}

/// Maps the server-sent events of one streamed response to [`CompletionEvent`]s.
pub trait EventMapper: Send {
    /// Maps one event. An `Err` ends the stream.
    fn event(&mut self, event: &SseEvent) -> Result<Vec<CompletionEvent>>;

    /// Whether the response is complete: `Stop` was returned, and later events are ignored.
    fn is_finished(&self) -> bool;

    /// Called when the body ends before [`EventMapper::is_finished`]: the last events (ending
    /// with `Stop`), or the error for a response that was cut off.
    fn end(&mut self) -> Result<Vec<CompletionEvent>>;
}

/// Maps a stream of SSE events to completion events with `mapper`. The stream ends after `Stop`
/// without reading the rest of the body, or after the first error.
pub fn completion_events<S, M>(
    events: S,
    mapper: M,
) -> impl Stream<Item = Result<CompletionEvent>> + Send
where
    S: Stream<Item = Result<SseEvent>> + Send,
    M: EventMapper,
{
    struct State<S, M> {
        events: S,
        mapper: M,
        ready: VecDeque<Result<CompletionEvent>>,
        done: bool,
    }
    let state = State {
        events: Box::pin(events),
        mapper,
        ready: VecDeque::new(),
        done: false,
    };
    futures::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(item) = state.ready.pop_front() {
                return Some((item, state));
            }
            if state.done {
                return None;
            }
            let result = match state.events.next().await {
                Some(Ok(event)) => state.mapper.event(&event),
                Some(Err(error)) => Err(error),
                None => {
                    state.done = true;
                    state.mapper.end()
                }
            };
            match result {
                Ok(events) => {
                    state.ready.extend(events.into_iter().map(Ok));
                    state.done |= state.mapper.is_finished();
                }
                Err(error) => {
                    state.done = true;
                    state.ready.push_back(Err(error));
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use giles_types::StopReason;

    use super::*;

    #[test]
    fn media_essence_is_the_lowercased_type_without_parameters() {
        assert_eq!(media_essence(" Audio/OGG; codecs=opus"), "audio/ogg");
    }

    #[test]
    fn temperature_is_the_shortest_decimal() {
        assert_eq!(temperature(0.7).unwrap().to_string(), "0.7");
        assert_eq!(temperature(1.0).unwrap().to_string(), "1.0");
        assert!(matches!(
            temperature(f32::NAN),
            Err(Error::InvalidRequest(_))
        ));
        assert!(temperature(f32::INFINITY).is_err());
    }

    /// Stops on the event `stop`; fails on `fail`; at the end of the body, stops if `lenient`.
    struct Mapper {
        finished: bool,
        lenient: bool,
    }

    impl EventMapper for Mapper {
        fn event(&mut self, event: &SseEvent) -> Result<Vec<CompletionEvent>> {
            match event.data.as_str() {
                "stop" => {
                    self.finished = true;
                    Ok(vec![CompletionEvent::Stop(StopReason::EndTurn)])
                }
                "fail" => Err(Error::other("failed")),
                _ => Ok(vec![CompletionEvent::Usage(Default::default())]),
            }
        }

        fn is_finished(&self) -> bool {
            self.finished
        }

        fn end(&mut self) -> Result<Vec<CompletionEvent>> {
            if self.lenient {
                Ok(vec![CompletionEvent::Stop(StopReason::MaxTokens)])
            } else {
                Err(Error::unavailable("cut off"))
            }
        }
    }

    async fn run(data: &[&str], lenient: bool) -> Vec<Result<CompletionEvent>> {
        let events = data.iter().map(|data| {
            Ok(SseEvent {
                event: "message".into(),
                data: (*data).into(),
                id: None,
            })
        });
        let mapper = Mapper {
            finished: false,
            lenient,
        };
        completion_events(futures::stream::iter(events), mapper)
            .collect()
            .await
    }

    #[tokio::test]
    async fn stops_after_stop_or_the_first_error() {
        let items = run(&["a", "stop", "a"], false).await;
        assert_eq!(items.len(), 2);
        assert!(matches!(
            items[1],
            Ok(CompletionEvent::Stop(StopReason::EndTurn))
        ));

        let items = run(&["a", "fail", "a"], false).await;
        assert_eq!(items.len(), 2);
        assert!(matches!(&items[1], Err(Error::Other(_))));
    }

    #[tokio::test]
    async fn the_end_of_the_body_asks_the_mapper() {
        let items = run(&["a"], false).await;
        assert_eq!(items.len(), 2);
        assert!(items[1].as_ref().unwrap_err().is_retryable());

        let items = run(&["a"], true).await;
        assert!(matches!(
            items[..],
            [Ok(_), Ok(CompletionEvent::Stop(StopReason::MaxTokens))]
        ));
    }
}

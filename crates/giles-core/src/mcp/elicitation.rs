//! MCP elicitation (PLAN sections 6.4 and 7.10): a server's `elicitation/create` becomes a
//! [`ToolCallEvent::ApprovalRequest`](giles_types::ToolCallEvent::ApprovalRequest) of the call
//! that asked, and the answer on the call's `answers` goes back as the `ElicitResult`.
//!
//! MCP has no field saying which request a server's question belongs to. Over streamable HTTP the
//! question arrives on the response stream of the `tools/call` that asked it (`rmcp` records that
//! as [`InboundStreamOrigin::OutboundRequest`]); over stdio nothing tells calls apart. So a
//! question goes to the call whose stream it came on, else to the only call running; with no call
//! or several, it is declined, since asking the wrong conversation would let one person answer
//! another's question.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use giles_types::{ApprovalAnswer, ApprovalDecision, ApprovalId, ToolCallEvent};
use rmcp::model::{ElicitRequestParams, ElicitResult, ElicitationAction, RequestId};
use rmcp::service::{InboundStreamOrigin, RequestContext, RoleClient};
use serde_json::Value;
use tokio::sync::{Notify, mpsc, oneshot};

/// Questions one call may have waiting for it to pick them up.
const QUESTION_BUFFER: usize = 4;

/// A server's question, on its way to the call that asked it.
#[derive(Debug)]
pub(crate) struct Question {
    /// What to ask.
    pub(crate) message: String,
    /// The requested schema, as JSON.
    pub(crate) schema: Option<Value>,
    /// Where the answer goes. Dropping it answers `cancel`.
    pub(crate) reply: oneshot::Sender<ElicitResult>,
}

/// The calls of one connection that a question may be for.
#[derive(Clone, Default)]
pub(crate) struct CallRouter {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    calls: Mutex<Calls>,
    /// Notified when a call learns its request id or ends.
    changed: Notify,
}

#[derive(Default)]
struct Calls {
    next: u64,
    slots: HashMap<u64, Slot>,
}

struct Slot {
    /// The `tools/call` request id, once sent.
    request: Option<RequestId>,
    questions: mpsc::Sender<Question>,
}

/// Where a question goes.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    To(u64),
    /// The call it came for may be one still being sent.
    Wait,
    Nobody,
}

/// The stream a question came on, as `rmcp` recorded it.
#[derive(Debug)]
enum Origin {
    /// The response stream of the request with this id (streamable HTTP).
    Response(RequestId),
    /// A stream tied to no request: HTTP's standalone stream, whose messages are unrelated to
    /// any running request.
    Unassociated,
    /// The transport doesn't tell streams apart (stdio, in-process pipes).
    Unknown,
}

/// The call a question from `origin` is for, among `slots`: the call whose stream it came on, or
/// without stream information the lone running call; never a call on HTTP's standalone stream.
fn decide(slots: &HashMap<u64, Slot>, origin: &Origin) -> Route {
    match origin {
        Origin::Response(origin) => {
            let sent = slots
                .iter()
                .find(|(_, slot)| slot.request.as_ref() == Some(origin));
            match sent {
                Some((key, _)) => Route::To(*key),
                None if slots.values().any(|slot| slot.request.is_none()) => Route::Wait,
                None => Route::Nobody,
            }
        }
        Origin::Unassociated => Route::Nobody,
        Origin::Unknown => {
            let mut keys = slots.keys();
            match (keys.next(), keys.next()) {
                (Some(key), None) => Route::To(*key),
                _ => Route::Nobody,
            }
        }
    }
}

impl CallRouter {
    fn lock(&self) -> std::sync::MutexGuard<'_, Calls> {
        self.inner
            .calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds a call, before its request is sent, so a question that comes at once finds it. The
    /// call stops taking questions when the returned slot is dropped.
    pub(crate) fn register(&self) -> CallSlot {
        let (questions, receiver) = mpsc::channel(QUESTION_BUFFER);
        let mut calls = self.lock();
        let key = calls.next;
        calls.next += 1;
        calls.slots.insert(
            key,
            Slot {
                request: None,
                questions,
            },
        );
        CallSlot {
            router: self.clone(),
            key,
            receiver,
        }
    }

    /// The call a question is for ([`decide`]), waiting while one that may be it is being sent.
    async fn route(&self, origin: &Origin) -> Option<mpsc::Sender<Question>> {
        loop {
            let changed = self.inner.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let calls = self.lock();
                match decide(&calls.slots, origin) {
                    Route::To(key) => return Some(calls.slots[&key].questions.clone()),
                    Route::Nobody => return None,
                    Route::Wait => {}
                }
            }
            changed.await;
        }
    }

    /// Answers `elicitation/create`: hands the question to its call and waits for the answer.
    /// Declined at once: URL-mode questions (not declared), questions whose schema requires
    /// fields (a v1 answer is only yes or no, so it can't supply them), and questions no single
    /// call asked. A question whose call ends first, or that the server cancels, is cancelled.
    pub(crate) async fn ask(
        &self,
        instance: &str,
        request: ElicitRequestParams,
        context: RequestContext<RoleClient>,
    ) -> ElicitResult {
        let decline = || ElicitResult::new(ElicitationAction::Decline);
        let cancel = || ElicitResult::new(ElicitationAction::Cancel);
        let (message, schema) = match request {
            ElicitRequestParams::FormElicitationParams {
                message,
                requested_schema,
                ..
            } => {
                if requested_schema
                    .required
                    .as_ref()
                    .is_some_and(|r| !r.is_empty())
                {
                    tracing::info!(
                        %instance,
                        "declined an MCP elicitation whose schema requires fields: Giles answers \
                         only yes or no"
                    );
                    return decline();
                }
                (message, serde_json::to_value(requested_schema).ok())
            }
            _ => {
                tracing::warn!(%instance, "declined an MCP elicitation: only form mode is supported");
                return decline();
            }
        };
        let origin = match context.extensions.get::<InboundStreamOrigin>() {
            Some(InboundStreamOrigin::OutboundRequest(id)) => Origin::Response(id.clone()),
            Some(InboundStreamOrigin::Unassociated) => Origin::Unassociated,
            None => Origin::Unknown,
        };
        let Some(call) = self.route(&origin).await else {
            tracing::warn!(
                %instance,
                ?origin,
                "declined an MCP elicitation: no single running call it can be for"
            );
            return decline();
        };
        let (reply, answer) = oneshot::channel();
        let question = Question {
            message,
            schema,
            reply,
        };
        if call.send(question).await.is_err() {
            return cancel();
        }
        tokio::select! {
            answer = answer => answer.unwrap_or_else(|_| cancel()),
            () = context.ct.cancelled() => cancel(),
        }
    }
}

/// One call's place in the [`CallRouter`].
pub(crate) struct CallSlot {
    router: CallRouter,
    key: u64,
    /// The questions for this call.
    pub(crate) receiver: mpsc::Receiver<Question>,
}

impl CallSlot {
    /// The call's request is sent, as `id`.
    pub(crate) fn sent(&self, id: RequestId) {
        if let Some(slot) = self.router.lock().slots.get_mut(&self.key) {
            slot.request = Some(id);
        }
        self.router.inner.changed.notify_waiters();
    }
}

impl Drop for CallSlot {
    fn drop(&mut self) {
        self.router.lock().slots.remove(&self.key);
        self.router.inner.changed.notify_waiters();
    }
}

/// The `ElicitResult` for a person's decision (MCP 2025-06-18): approved is `accept` with the
/// answer's content object, or `{}` without one (v1's `yes`; questions whose schema requires
/// fields never get here, [`CallRouter::ask`]); denied, expired, or superseded is `decline`.
/// Content that isn't an object can't be an answer, so it is `cancel`.
fn elicit_result(decision: ApprovalDecision) -> ElicitResult {
    match decision {
        ApprovalDecision::Approve { content: None } => ElicitResult::new(ElicitationAction::Accept)
            .with_content(Value::Object(Default::default())),
        ApprovalDecision::Approve {
            content: Some(content @ Value::Object(_)),
        } => ElicitResult::new(ElicitationAction::Accept).with_content(content),
        ApprovalDecision::Approve { content: Some(_) } => {
            ElicitResult::new(ElicitationAction::Cancel)
        }
        _ => ElicitResult::new(ElicitationAction::Decline),
    }
}

/// One call's questions waiting for an answer, by the approval id each was given.
#[derive(Debug, Default)]
pub(crate) struct Pending {
    waiting: HashMap<ApprovalId, oneshot::Sender<ElicitResult>>,
    asked: u64,
    /// No more answers will come.
    closed: bool,
}

impl Pending {
    /// The approval request that asks `question`, or `None` if no more answers will come: then
    /// it is declined at once.
    pub(crate) fn ask(&mut self, question: Question) -> Option<ToolCallEvent> {
        if self.closed {
            let _ = question
                .reply
                .send(ElicitResult::new(ElicitationAction::Decline));
            return None;
        }
        self.asked += 1;
        let id =
            ApprovalId::new(format!("elicitation-{}", self.asked)).expect("the id is not empty");
        self.waiting.insert(id.clone(), question.reply);
        Some(ToolCallEvent::ApprovalRequest {
            id,
            message: question.message,
            schema: question.schema,
        })
    }

    /// Sends `answer` back to its question. `None` (the answers channel closed) declines every
    /// question waiting, and every later one.
    pub(crate) fn answer(&mut self, answer: Option<ApprovalAnswer>) {
        let Some(answer) = answer else {
            self.closed = true;
            for (_, reply) in self.waiting.drain() {
                let _ = reply.send(ElicitResult::new(ElicitationAction::Decline));
            }
            return;
        };
        match self.waiting.remove(&answer.id) {
            Some(reply) => {
                let _ = reply.send(elicit_result(answer.decision));
            }
            None => tracing::debug!(id = %answer.id, "an answer to no waiting question"),
        }
    }

    /// Whether no more answers will come.
    pub(crate) fn closed(&self) -> bool {
        self.closed
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn slots(requests: &[Option<i64>]) -> HashMap<u64, Slot> {
        (0..)
            .zip(requests)
            .map(|(key, request)| {
                let (questions, _) = mpsc::channel(1);
                let request = request.map(RequestId::Number);
                (key, Slot { request, questions })
            })
            .collect()
    }

    #[test]
    fn a_question_goes_to_the_call_whose_stream_it_came_on() {
        let origin = Origin::Response(RequestId::Number(7));
        assert_eq!(decide(&slots(&[Some(3), Some(7)]), &origin), Route::To(1));
        // One of the calls may be the one, its id not known yet.
        assert_eq!(decide(&slots(&[Some(3), None]), &origin), Route::Wait);
        // The stream is of some other request (not a call).
        assert_eq!(decide(&slots(&[Some(3)]), &origin), Route::Nobody);
        assert_eq!(decide(&slots(&[]), &origin), Route::Nobody);
    }

    #[test]
    fn without_stream_information_only_a_lone_call_gets_the_question() {
        let unknown = Origin::Unknown;
        assert_eq!(decide(&slots(&[Some(3)]), &unknown), Route::To(0));
        assert_eq!(decide(&slots(&[None]), &unknown), Route::To(0));
        assert_eq!(decide(&slots(&[Some(3), Some(4)]), &unknown), Route::Nobody);
        assert_eq!(decide(&slots(&[]), &unknown), Route::Nobody);
    }

    #[test]
    fn a_question_on_the_standalone_stream_is_for_nobody() {
        // Its messages are unrelated to running requests, even with one call running.
        let origin = Origin::Unassociated;
        assert_eq!(decide(&slots(&[Some(3)]), &origin), Route::Nobody);
        assert_eq!(decide(&slots(&[None]), &origin), Route::Nobody);
    }

    #[tokio::test]
    async fn routing_waits_for_a_call_being_sent() {
        let router = CallRouter::default();
        let other = router.register();
        other.sent(RequestId::Number(1));
        let call = router.register();
        let origin = RequestId::Number(2);
        let from = Origin::Response(origin.clone());
        let route = router.route(&from);
        tokio::pin!(route);
        assert!(futures::poll!(route.as_mut()).is_pending());
        call.sent(origin.clone());
        assert!(route.await.is_some());

        // A call that ends while a question waits for it: nobody.
        let late = router.register();
        let from = Origin::Response(RequestId::Number(3));
        let route = router.route(&from);
        tokio::pin!(route);
        assert!(futures::poll!(route.as_mut()).is_pending());
        drop(late);
        assert!(route.await.is_none());
        drop((other, call));
        assert!(router.lock().slots.is_empty());
    }

    #[test]
    fn decisions_map_to_accept_decline_or_cancel() {
        let accept = elicit_result(ApprovalDecision::Approve { content: None });
        assert_eq!(
            (accept.action, accept.content),
            (ElicitationAction::Accept, Some(json!({})))
        );
        let content = json!({"name": "Ada"});
        let accept = elicit_result(ApprovalDecision::Approve {
            content: Some(content.clone()),
        });
        assert_eq!(
            (accept.action, accept.content),
            (ElicitationAction::Accept, Some(content))
        );
        let odd = elicit_result(ApprovalDecision::Approve {
            content: Some(json!("Ada")),
        });
        assert_eq!((odd.action, odd.content), (ElicitationAction::Cancel, None));
        let deny = elicit_result(ApprovalDecision::Deny);
        assert_eq!(
            (deny.action, deny.content),
            (ElicitationAction::Decline, None)
        );
    }

    fn question(message: &str) -> (Question, oneshot::Receiver<ElicitResult>) {
        let (reply, answer) = oneshot::channel();
        let question = Question {
            message: message.to_owned(),
            schema: None,
            reply,
        };
        (question, answer)
    }

    #[test]
    fn questions_get_ids_and_their_answers() {
        let mut pending = Pending::default();
        let (first, mut first_answer) = question("Deploy?");
        let (second, mut second_answer) = question("Really?");
        let id = |n: u32| ApprovalId::new(format!("elicitation-{n}")).unwrap();
        assert_eq!(
            pending.ask(first),
            Some(ToolCallEvent::ApprovalRequest {
                id: id(1),
                message: "Deploy?".to_owned(),
                schema: None,
            })
        );
        assert!(pending.ask(second).is_some());
        // An answer to no question changes nothing.
        pending.answer(Some(ApprovalAnswer {
            id: id(9),
            decision: ApprovalDecision::Deny,
        }));
        pending.answer(Some(ApprovalAnswer {
            id: id(2),
            decision: ApprovalDecision::Approve { content: None },
        }));
        assert_eq!(
            second_answer.try_recv().unwrap().action,
            ElicitationAction::Accept
        );
        assert!(first_answer.try_recv().is_err());

        // No more answers: the waiting question and later ones are declined.
        pending.answer(None);
        assert!(pending.closed());
        assert_eq!(
            first_answer.try_recv().unwrap().action,
            ElicitationAction::Decline
        );
        let (late, mut late_answer) = question("Now?");
        assert_eq!(pending.ask(late), None);
        assert_eq!(
            late_answer.try_recv().unwrap().action,
            ElicitationAction::Decline
        );
    }
}

//! Killing the daemon mid-turn and starting it again (PLAN section 6.3, "Startup recovery").

use serde_json::json;

use crate::harness::{Daemon, Home, wait_until};
use crate::provider::{FakeProvider, Reply, texts_with_role, tool_result};

/// What the requester is told once, for each turn the restart cut off.
const NOTICE: &str = "Interrupted by a restart; resend if needed.";

#[test]
fn a_crash_with_an_approval_pending_is_reported_and_the_call_never_runs() {
    let provider = FakeProvider::start();
    provider.script([Reply::call(
        "call_1",
        "memory_add",
        json!({"content": "Takes tea at four.", "core": true}),
    )]);
    let home = Home::new(
        &provider,
        "tools = [\"memory_add\"]\nask = [\"memory_add\"]\n",
    );
    let daemon = Daemon::start(&home);

    home.say("Remember that I take tea at four.");
    home.wait_approval_code(0, "memory_add");
    assert_eq!(home.inbox_states(), ["started"]);
    daemon.kill();
    // The crash left the turn's message started and the call waiting for an answer.
    assert_eq!(home.inbox_states(), ["started"]);

    let seen = home.sent().len();
    let daemon = Daemon::start(&home);
    // The requester is told, and the message is recorded as unknown, never replayed.
    home.wait_sent_after(seen, NOTICE);
    assert_eq!(home.inbox_states(), ["unknown"]);
    assert_eq!(provider.requests().len(), 1);

    // The conversation goes on, and the model learns what became of the call.
    provider.script([Reply::text("Back online, sir.")]);
    home.say("Hello?");
    home.wait_sent("Back online, sir.");
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(tool_result(&requests[1], "call_1"), "Error: interrupted");
    assert!(
        home.memories().is_empty(),
        "the call that was waiting for approval must not run after a restart"
    );
    // The reply is sent before the turn is finished; the message is done once it is.
    wait_until("the new message to be done", || {
        (home.inbox_states() == ["unknown", "done"]).then_some(())
    });

    daemon.stop();
    provider.assert_no_surprises();
}

#[test]
fn a_crash_while_the_model_is_answering_is_reported_and_not_replayed() {
    let provider = FakeProvider::start();
    provider.script([Reply::Hold]);
    let home = Home::new(&provider, "");
    let daemon = Daemon::start(&home);

    home.say("Take your time.");
    provider.wait_requests(1);
    daemon.kill();
    assert_eq!(home.inbox_states(), ["started"]);

    let seen = home.sent().len();
    let daemon = Daemon::start(&home);
    home.wait_sent_after(seen, NOTICE);
    assert_eq!(home.inbox_states(), ["unknown"]);

    // The interrupted message stays in the history, unanswered, and is not asked again: the
    // only requests are the one that was cut off and the one for the new message.
    provider.script([Reply::text("Forgive the delay, sir.")]);
    home.say("Hello?");
    home.wait_sent("Forgive the delay, sir.");
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let users = texts_with_role(&requests[1], "user");
    assert!(users[0].ends_with("Take your time."), "{users:?}");
    assert!(users[1].ends_with("Hello?"), "{users:?}");

    daemon.stop();
    provider.assert_no_surprises();
}

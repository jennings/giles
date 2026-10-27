//! A message to its reply, a tool call that needs approval, and `/stop`.

use serde_json::json;

use crate::harness::{CONVERSATION, Daemon, Home, wait_until};
use crate::provider::{FakeProvider, Reply, texts_with_role, tool_result};

/// Input for the `memory_add` core tool: remembers `content` as a core memory.
fn remember(content: &str) -> serde_json::Value {
    json!({"content": content, "core": true})
}

/// Config for an agent that may use `memory_add` but must ask first.
const ASKS_FIRST: &str = "tools = [\"memory_add\"]\nask = [\"memory_add\"]\n";

#[test]
fn a_message_reaches_the_model_and_the_reply_reaches_the_channel() {
    let provider = FakeProvider::start();
    provider.script([Reply::text("Good evening, sir."), Reply::text("Tea, sir.")]);
    let home = Home::new(&provider, "");
    let daemon = Daemon::start(&home);

    home.say("Hello, Jeeves.");
    let reply = home.wait_sent("Good evening, sir.");
    // Answered where the message came from.
    assert_eq!(
        reply["message"]["target"],
        json!({"conversation": CONVERSATION})
    );
    let requests = provider.wait_requests(1);
    assert_eq!(requests[0]["model"], "test-model");
    assert_eq!(requests[0]["stream"], true);
    assert!(
        !texts_with_role(&requests[0], "system")[0].is_empty(),
        "the agent's prompt is sent"
    );
    let users = texts_with_role(&requests[0], "user");
    assert_eq!(users.len(), 1, "{users:?}");
    assert!(users[0].ends_with("Hello, Jeeves."), "{users:?}");
    // The plugin is told the core has the message, once it's stored.
    wait_until("the ack of the first message", || {
        (home.acked() == ["e1"]).then_some(())
    });

    // The session goes on: the second request carries the whole conversation.
    home.say("Anything else?");
    home.wait_sent("Tea, sir.");
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        texts_with_role(&requests[1], "assistant"),
        ["Good evening, sir."]
    );
    let users = texts_with_role(&requests[1], "user");
    assert!(users[0].ends_with("Hello, Jeeves."), "{users:?}");
    assert!(users[1].ends_with("Anything else?"), "{users:?}");
    assert_eq!(home.texts(), ["Good evening, sir.", "Tea, sir."]);

    daemon.stop();
    provider.assert_no_surprises();
}

#[test]
fn an_approved_tool_call_waits_for_the_owner_and_then_runs() {
    let provider = FakeProvider::start();
    provider.script([
        Reply::call("call_1", "memory_add", remember("Takes tea at four.")),
        Reply::text("Noted, sir."),
    ]);
    let home = Home::new(&provider, ASKS_FIRST);
    let daemon = Daemon::start(&home);

    home.say("Remember that I take tea at four.");
    let code = home.wait_approval_code(0, "memory_add");
    let prompt = home.wait_sent("Allow `memory_add`");
    // The prompt shows the call's input, and goes to the conversation that asked.
    let text = prompt["message"]["markdown"].as_str().unwrap();
    assert!(text.contains("Takes tea at four."), "{text}");
    assert_eq!(
        prompt["message"]["target"],
        json!({"conversation": CONVERSATION})
    );
    // Nothing ran, and the model hasn't been asked again, while the owner hasn't answered.
    assert!(home.memories().is_empty());
    assert_eq!(provider.requests().len(), 1);

    home.say(&format!("yes {code}"));
    home.wait_sent("Noted, sir.");
    let memories = home.memories();
    assert_eq!(memories.len(), 1, "{memories:?}");
    assert_eq!(memories[0].0, "Takes tea at four.");
    assert!(memories[0].1, "the memory was added as core: {memories:?}");
    // The model got the tool's result, not a denial.
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let result = tool_result(&requests[1], "call_1");
    assert!(!result.contains("denied"), "{result}");
    // The `yes` answered the prompt; it never reached the model as a message.
    assert_eq!(texts_with_role(&requests[1], "user").len(), 1);

    daemon.stop();
    provider.assert_no_surprises();
}

#[test]
fn a_denied_tool_call_never_runs_and_the_model_is_told() {
    let provider = FakeProvider::start();
    provider.script([
        Reply::call("call_1", "memory_add", remember("Dislikes coffee.")),
        Reply::text("Understood, sir."),
        Reply::call("call_2", "memory_add", remember("Dislikes tea.")),
        Reply::text("As you wish, sir."),
    ]);
    let home = Home::new(&provider, ASKS_FIRST);
    let daemon = Daemon::start(&home);

    // `no <code>` denies.
    home.say("Remember I dislike coffee.");
    let code = home.wait_approval_code(0, "memory_add");
    home.say(&format!("no {code}"));
    home.wait_sent("Understood, sir.");
    assert_eq!(
        tool_result(&provider.requests()[1], "call_1"),
        "Error: denied"
    );

    // So does any other message while the prompt waits: the model sees it.
    let seen = home.sent().len();
    home.say("Remember I dislike tea.");
    home.wait_approval_code(seen, "memory_add");
    home.say("Actually, never mind.");
    home.wait_sent("As you wish, sir.");
    let requests = provider.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(
        tool_result(&requests[3], "call_2"),
        "Error: Not approved. The user said: Actually, never mind."
    );

    assert!(home.memories().is_empty(), "a denied call must not run");
    daemon.stop();
    provider.assert_no_surprises();
}

#[test]
fn stop_cancels_a_turn_that_is_waiting_for_the_model() {
    let provider = FakeProvider::start();
    provider.script([Reply::Hold]);
    let home = Home::new(&provider, "");
    let daemon = Daemon::start(&home);

    home.say("Take your time.");
    provider.wait_requests(1);
    home.say("/stop");
    // The daemon drops the request: the model is no longer being waited on.
    provider.wait_hang_ups(1);
    daemon.wait_for_log("turn stopped by /stop");

    // The conversation is usable at once, and what was cancelled left no half answer behind.
    provider.script([Reply::text("Back at once, sir.")]);
    home.say("Hello?");
    home.wait_sent("Back at once, sir.");
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(texts_with_role(&requests[1], "assistant").is_empty());
    let users = texts_with_role(&requests[1], "user");
    assert!(users[0].ends_with("Take your time."), "{users:?}");
    assert!(users[1].ends_with("Hello?"), "{users:?}");
    // Nothing was said about the cancelled turn.
    assert_eq!(home.texts(), ["Back at once, sir."]);

    daemon.stop();
    provider.assert_no_surprises();
}

#[test]
fn stop_cancels_a_pending_approval_and_the_call_never_runs() {
    let provider = FakeProvider::start();
    provider.script([Reply::call(
        "call_1",
        "memory_add",
        remember("Takes tea at four."),
    )]);
    let home = Home::new(&provider, ASKS_FIRST);
    let daemon = Daemon::start(&home);

    home.say("Remember that I take tea at four.");
    let code = home.wait_approval_code(0, "memory_add");
    home.say("/stop");
    daemon.wait_for_log("turn stopped by /stop");

    provider.script([Reply::text("Very good, sir.")]);
    home.say("Hello?");
    home.wait_sent("Very good, sir.");
    // The call's slot was filled, so the transcript stays valid for the next request.
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(tool_result(&requests[1], "call_1"), "Error: cancelled");
    assert!(home.memories().is_empty());

    // A late `yes` doesn't resurrect the call: it's told the request ended.
    let seen = home.sent().len();
    home.say(&format!("yes {code}"));
    home.wait_sent_after(seen, "cancelled");
    assert!(home.memories().is_empty());
    assert_eq!(provider.requests().len(), 2);

    daemon.stop();
    provider.assert_no_surprises();
}

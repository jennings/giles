//! Approvals end to end (PLAN section 6.4): prompts sent through the channel, `yes`/`no`/`yes!`
//! replies consumed by the session manager, expiry, denial by a new message, and questions a
//! running call asks (MCP elicitation), on paused time.

#[path = "support/turns.rs"]
mod turns;

use std::time::Duration;

use serde_json::json;
use turns::{Daemon, PREFIX, calls, text};

/// The code of an approval prompt: the `<code>` in "Reply `yes <code>` or `no <code>`."
fn code(prompt: &str) -> String {
    let (_, rest) = prompt
        .rsplit_once("`no ")
        .unwrap_or_else(|| panic!("not a prompt: {prompt}"));
    rest.trim_end_matches("`.").to_owned()
}

fn prompt(tool: &str, input: &str, code: &str) -> String {
    format!("Allow `{tool}` (`{input}`)? Reply `yes {code}` or `no {code}`.")
}

#[tokio::test(start_paused = true)]
async fn an_ask_call_waits_for_yes_and_the_reply_never_reaches_the_agent() {
    let mut d = Daemon::start("ask = [\"met__quick\"]\n").await;
    d.provider
        .push(calls(&[("t1", "met__quick", json!({"path": "/tmp"}))]));
    d.provider.push(text("Cleaned."));
    d.say("Clean up");
    let sent = d.next_sent().await;
    let c = code(&sent);
    assert_eq!(c.len(), 4, "{c}");
    assert_eq!(sent, prompt("met__quick", r#"{"path":"/tmp"}"#, &c));

    // Nothing runs while it waits.
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert!(d.tool_events().is_empty());
    d.say(&format!("Yes {c}"));
    assert_eq!(d.next_tool_event().await, "start t1");
    assert_eq!(d.next_sent().await, "Cleaned.");
    d.idle().await;
    assert_eq!(
        d.transcript().await,
        [
            format!("user: {PREFIX} Clean up"),
            "assistant: <met__quick t1>".to_owned(),
            "result t1: quick ran with {\"path\":\"/tmp\"}".to_owned(),
            "assistant: Cleaned.".to_owned(),
        ]
    );
    assert_eq!(d.provider.requests().len(), 2);
    d.stop().await;
}

#[tokio::test(start_paused = true)]
async fn no_denies_the_call() {
    let mut d = Daemon::start("ask = [\"met__*\"]\n").await;
    d.provider.push(calls(&[("t1", "met__quick", json!({}))]));
    d.provider.push(text("Understood."));
    d.say("Clean up");
    let c = code(&d.next_sent().await);
    d.say(&format!("no {c}"));
    assert_eq!(d.next_sent().await, "Understood.");
    d.idle().await;
    assert_eq!(
        d.transcript().await[1..3],
        ["assistant: <met__quick t1>", "result! t1: denied"]
    );
    assert!(d.tool_events().is_empty());
    d.stop().await;
}

#[tokio::test(start_paused = true)]
async fn yes_bang_allows_the_tool_for_the_rest_of_the_session() {
    let mut d = Daemon::start("ask = [\"met__quick\", \"met__block\"]\n").await;
    d.provider
        .push(calls(&[("t1", "met__quick", json!({"n": 1}))]));
    d.provider
        .push(calls(&[("t2", "met__quick", json!({"n": 2}))]));
    d.provider.push(text("Twice."));
    d.say("Go");
    let c = code(&d.next_sent().await);
    d.say(&format!("yes! {c}"));
    assert_eq!(d.next_sent().await, "Twice.");
    d.idle().await;

    // The next turn: still allowed; another `ask` tool still asks.
    d.provider.push(calls(&[
        ("t3", "met__quick", json!({"n": 3})),
        ("t4", "met__block", json!({})),
    ]));
    d.say("Again");
    let sent = d.next_sent().await;
    assert_eq!(sent, prompt("met__block", "{}", &code(&sent)));
    assert_eq!(
        d.tool_events(),
        [
            "start t1", "end t1", "start t2", "end t2", "start t3", "end t3"
        ]
    );
    d.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_bare_yes_with_two_requests_waiting_is_refused() {
    let mut d = Daemon::start("ask = [\"par__quick\"]\n").await;
    d.provider.push(calls(&[
        ("t1", "par__quick", json!({"n": 1})),
        ("t2", "par__quick", json!({"n": 2})),
    ]));
    d.provider.push(text("One of two."));
    d.say("Go");
    let first = d.next_sent().await;
    let second = d.next_sent().await;
    let (c1, c2) = (code(&first), code(&second));
    assert_ne!(c1, c2);
    assert_eq!(first, prompt("par__quick", r#"{"n":1}"#, &c1));

    d.say("yes");
    let mut codes = [c1.clone(), c2.clone()];
    codes.sort();
    assert_eq!(
        d.next_sent().await,
        format!(
            "More than one approval request is waiting for you (`{}`, `{}`). Reply with its \
             code, e.g. `yes {}`.",
            codes[0], codes[1], codes[0]
        )
    );
    // A code that isn't waiting is refused too.
    d.say("yes 0000");
    assert_eq!(
        d.next_sent().await,
        format!(
            "No approval request `0000` is waiting for you (waiting: `{}`, `{}`).",
            codes[0], codes[1]
        )
    );
    Daemon::settle().await;
    assert!(d.tool_events().is_empty());

    d.say(&format!("yes {c1}"));
    assert_eq!(d.next_tool_event().await, "start t1");
    d.say(&format!("no {c2}"));
    assert_eq!(d.next_sent().await, "One of two.");
    d.idle().await;
    assert_eq!(
        d.transcript().await[1..4],
        [
            "assistant: <par__quick t1> <par__quick t2>",
            "result t1: quick ran with {\"n\":1}",
            "result! t2: denied",
        ]
    );
    // None of the replies reached the agent.
    assert_eq!(d.provider.requests().len(), 2);
    d.stop().await;
}

#[tokio::test(start_paused = true)]
async fn yes_bang_also_allows_the_same_tools_other_waiting_calls() {
    let mut d = Daemon::start("ask = [\"par__quick\"]\n").await;
    d.provider.push(calls(&[
        ("t1", "par__quick", json!({"n": 1})),
        ("t2", "par__quick", json!({"n": 2})),
    ]));
    d.provider.push(text("Both."));
    d.say("Go");
    let c1 = code(&d.next_sent().await);
    let _ = d.next_sent().await;
    d.say(&format!("yes! {c1}"));
    assert_eq!(d.next_sent().await, "Both.");
    d.idle().await;
    let mut events = d.tool_events();
    events.sort();
    assert_eq!(events, ["end t1", "end t2", "start t1", "start t2"]);
    d.stop().await;
}

#[tokio::test(start_paused = true)]
async fn an_unanswered_request_expires_denied_and_a_late_reply_is_told() {
    let mut d = Daemon::start("ask = [\"met__quick\"]\napproval_timeout = \"1m\"\n").await;
    d.provider.push(calls(&[("t1", "met__quick", json!({}))]));
    d.provider.push(text("No answer, so I didn't."));
    d.say("Go");
    let c = code(&d.next_sent().await);
    tokio::time::sleep(Duration::from_secs(59)).await;
    assert!(d.sent().is_empty());
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(d.next_sent().await, "No answer, so I didn't.");
    d.idle().await;
    assert_eq!(
        d.transcript().await[2],
        "result! t1: denied (the approval request expired)"
    );

    d.say(&format!("yes {c}"));
    assert_eq!(
        d.next_sent().await,
        format!("Approval request `{c}` expired and was denied. Ask again if you still want it.")
    );
    Daemon::settle().await;
    // Consumed: no turn, nothing ran.
    assert_eq!(d.provider.requests().len(), 2);
    assert!(d.tool_events().is_empty());
    d.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_new_message_denies_the_request_and_the_model_sees_it() {
    for busy_mode in ["steer", "queue"] {
        let mut d = Daemon::start(&format!(
            "ask = [\"met__quick\"]\nbusy_mode = \"{busy_mode}\"\n"
        ))
        .await;
        d.provider.push(calls(&[
            ("t1", "met__quick", json!({})),
            ("t2", "met__block", json!({})),
        ]));
        d.provider.push(text("I won't."));
        d.say("Go");
        let _ = code(&d.next_sent().await);
        d.say("Wait, not that one");
        if busy_mode == "queue" {
            // The batch goes on in queue mode.
            assert_eq!(d.next_tool_event().await, "start t2");
            d.met.release("t2");
        }
        assert_eq!(d.next_sent().await, "I won't.", "{busy_mode}");
        d.idle().await;
        let t2 = if busy_mode == "steer" {
            "result! t2: Skipped to read a new message"
        } else {
            "result t2: block ran with {}"
        };
        assert_eq!(
            d.transcript().await,
            [
                format!("user: {PREFIX} Go"),
                "assistant: <met__quick t1> <met__block t2>".to_owned(),
                "result! t1: Not approved. The user said: Wait, not that one".to_owned(),
                t2.to_owned(),
                format!("user: {PREFIX} Wait, not that one"),
                "assistant: I won't.".to_owned(),
            ],
            "{busy_mode}"
        );
        // The message joined the turn: no second turn.
        assert_eq!(d.provider.requests().len(), 2, "{busy_mode}");
        d.stop().await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_message_that_denied_a_request_survives_a_stop_and_starts_the_next_turn() {
    let mut d = Daemon::start("ask = [\"met__quick\"]\nbusy_mode = \"queue\"\n").await;
    d.provider.push(calls(&[
        ("t1", "met__quick", json!({})),
        ("t2", "met__block", json!({})),
    ]));
    d.provider.push(text("Something else, then."));
    d.say("Go");
    let _ = code(&d.next_sent().await);
    d.say("not that");
    // Denied; the batch goes on in queue mode.
    assert_eq!(d.next_tool_event().await, "start t2");
    d.say("/stop");
    assert_eq!(d.next_tool_event().await, "drop t2");

    assert_eq!(d.next_sent().await, "Something else, then.");
    d.idle().await;
    assert_eq!(
        d.transcript().await,
        [
            format!("user: {PREFIX} Go"),
            "assistant: <met__quick t1> <met__block t2>".to_owned(),
            "result! t1: Not approved. The user said: not that".to_owned(),
            "result! t2: cancelled".to_owned(),
            format!("user: {PREFIX} not that"),
            "assistant: Something else, then.".to_owned(),
        ]
    );
    d.stop().await;
}

#[tokio::test(start_paused = true)]
async fn in_queue_mode_a_message_sent_before_the_prompt_does_not_deny_it() {
    let mut d = Daemon::start("ask = [\"met__quick\"]\nbusy_mode = \"queue\"\n").await;
    d.provider.push(calls(&[
        ("t1", "met__block", json!({})),
        ("t2", "met__quick", json!({})),
    ]));
    d.provider.push(text("Both done."));
    d.provider.push(text("About your question."));
    d.say("Go");
    assert_eq!(d.next_tool_event().await, "start t1");
    d.say("a question for later");
    Daemon::settle().await;
    d.met.release("t1");
    let c = code(&d.next_sent().await);
    Daemon::settle().await;
    // Still waiting: the earlier message didn't deny it.
    d.say(&format!("yes {c}"));
    assert_eq!(d.next_sent().await, "Both done.");
    assert_eq!(d.next_sent().await, "About your question.");
    d.idle().await;
    assert_eq!(
        d.transcript().await[2..],
        [
            "result t1: block ran with {}".to_owned(),
            "result t2: quick ran with {}".to_owned(),
            "assistant: Both done.".to_owned(),
            format!("user: {PREFIX} a question for later"),
            "assistant: About your question.".to_owned(),
        ]
    );
    d.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_question_from_a_running_call_is_asked_and_answered() {
    let mut d = Daemon::start("").await;
    d.provider.push(calls(&[("t1", "met__confirm", json!({}))]));
    d.provider.push(text("Deployed."));
    d.provider.push(calls(&[("t2", "met__confirm", json!({}))]));
    d.provider.push(text("Not deployed."));
    d.say("Deploy");
    assert_eq!(d.next_tool_event().await, "start t1");
    let sent = d.next_sent().await;
    let c = code(&sent);
    assert_eq!(
        sent,
        format!(
            "The tool `met__confirm` asks: `Deploy to production?` Reply `yes {c}` or `no {c}`."
        )
    );
    d.say(&format!("yes {c}"));
    assert_eq!(d.next_sent().await, "Deployed.");
    d.idle().await;

    d.say("Deploy again");
    let c = code(&d.next_sent().await);
    d.say(&format!("no {c}"));
    assert_eq!(d.next_sent().await, "Not deployed.");
    d.idle().await;
    let transcript = d.transcript().await;
    assert_eq!(transcript[2], "result t1: answer Approve { content: None }");
    assert_eq!(transcript[6], "result t2: answer Deny");
    d.stop().await;
}

#[tokio::test(start_paused = true)]
async fn stop_cancels_a_waiting_request() {
    let mut d = Daemon::start("ask = [\"met__quick\"]\n").await;
    d.provider.push(calls(&[("t1", "met__quick", json!({}))]));
    d.say("Go");
    let c = code(&d.next_sent().await);
    d.say("/stop");
    d.idle().await;
    assert_eq!(d.transcript().await[2], "result! t1: cancelled");

    d.say(&format!("yes {c}"));
    assert_eq!(
        d.next_sent().await,
        format!("Approval request `{c}` was cancelled.")
    );
    Daemon::settle().await;
    assert!(d.tool_events().is_empty());
    assert_eq!(d.provider.requests().len(), 1);
    d.stop().await;
}

#[tokio::test(start_paused = true)]
async fn waiting_for_approval_does_not_use_up_the_calls_deadline() {
    // `slow` has a 1 s deadline.
    let mut d = Daemon::start("ask = [\"slow__quick\"]\n").await;
    d.provider.push(calls(&[("t1", "slow__quick", json!({}))]));
    d.provider.push(text("Done."));
    d.say("Go");
    let c = code(&d.next_sent().await);
    tokio::time::sleep(Duration::from_secs(30)).await;
    d.say(&format!("yes {c}"));
    assert_eq!(d.next_sent().await, "Done.");
    d.idle().await;
    assert_eq!(d.transcript().await[2], "result t1: quick ran with {}");
    d.stop().await;
}

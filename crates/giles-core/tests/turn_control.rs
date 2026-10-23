//! Turn control end to end through the session manager (PLAN section 6.3): `/stop` reaching a
//! running tool call, and the requester's new messages in `steer` and `queue` mode.

#[path = "support/turns.rs"]
mod turns;

use serde_json::json;
use turns::{Daemon, PREFIX, calls, text};

#[tokio::test(start_paused = true)]
async fn stop_cancels_the_running_call_at_its_toolset_and_fills_every_slot() {
    let mut d = Daemon::start("").await;
    d.provider.push(calls(&[
        ("t1", "met__block", json!({})),
        ("t2", "met__quick", json!({})),
    ]));
    d.say("Go");
    assert_eq!(d.next_tool_event().await, "start t1");

    d.say("/stop");
    // Dropping the call's events stream is the toolset's cancellation (MCP
    // `notifications/cancelled`).
    assert_eq!(d.next_tool_event().await, "drop t1");
    d.idle().await;
    assert_eq!(
        d.transcript().await,
        [
            format!("user: {PREFIX} Go"),
            "assistant: <met__block t1> <met__quick t2>".to_owned(),
            "result! t1: cancelled".to_owned(),
            "result! t2: cancelled".to_owned(),
        ]
    );

    // The next message starts a new turn; the cancelled batch's other call never ran.
    d.provider.push(text("Stopped."));
    d.say("Never mind");
    assert_eq!(d.next_sent().await, "Stopped.");
    assert!(d.tool_events().is_empty());
    d.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_message_that_skipped_calls_survives_a_stop_and_starts_the_next_turn() {
    let mut d = Daemon::start("").await;
    d.provider.push(calls(&[
        ("t1", "met__block", json!({})),
        ("t2", "met__quick", json!({})),
    ]));
    d.provider.push(text("About your note."));
    d.say("Go");
    assert_eq!(d.next_tool_event().await, "start t1");
    // The note skips `t2`; `t1` keeps running.
    d.say("note");
    Daemon::settle().await;
    d.say("/stop");
    assert_eq!(d.next_tool_event().await, "drop t1");

    // The note is answered in the next turn.
    assert_eq!(d.next_sent().await, "About your note.");
    d.idle().await;
    assert_eq!(
        d.transcript().await,
        [
            format!("user: {PREFIX} Go"),
            "assistant: <met__block t1> <met__quick t2>".to_owned(),
            "result! t1: cancelled".to_owned(),
            "result! t2: Skipped to read a new message".to_owned(),
            format!("user: {PREFIX} note"),
            "assistant: About your note.".to_owned(),
        ]
    );
    d.stop().await;
}

#[tokio::test(start_paused = true)]
async fn in_steer_mode_a_new_message_skips_unstarted_calls_and_joins_the_turn() {
    let mut d = Daemon::start("").await;
    d.provider.push(calls(&[
        ("t1", "met__block", json!({"n": 1})),
        ("t2", "met__quick", json!({})),
    ]));
    d.provider.push(text("Reading your note."));
    d.say("Go");
    assert_eq!(d.next_tool_event().await, "start t1");

    d.say("Also check the cellar");
    Daemon::settle().await;
    // The running call finishes.
    d.met.release("t1");
    assert_eq!(d.next_sent().await, "Reading your note.");
    d.idle().await;

    let transcript = [
        format!("user: {PREFIX} Go"),
        "assistant: <met__block t1> <met__quick t2>".to_owned(),
        "result t1: block ran with {\"n\":1}".to_owned(),
        "result! t2: Skipped to read a new message".to_owned(),
        format!("user: {PREFIX} Also check the cellar"),
        "assistant: Reading your note.".to_owned(),
    ];
    assert_eq!(d.transcript().await, transcript);
    assert_eq!(d.tool_events(), ["end t1"]);
    // The second request saw the message after the results.
    let requests = d.provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].messages.len(), 5);
    // One turn: the message didn't start another.
    d.stop().await;
}

#[tokio::test(start_paused = true)]
async fn in_queue_mode_a_new_message_waits_for_the_next_turn() {
    let mut d = Daemon::start("busy_mode = \"queue\"\n").await;
    d.provider.push(calls(&[
        ("t1", "met__block", json!({})),
        ("t2", "met__quick", json!({})),
    ]));
    d.provider.push(text("Done."));
    d.provider.push(text("Cellar checked."));
    d.say("Go");
    assert_eq!(d.next_tool_event().await, "start t1");

    d.say("Also check the cellar");
    Daemon::settle().await;
    d.met.release("t1");
    assert_eq!(d.next_sent().await, "Done.");
    assert_eq!(d.next_sent().await, "Cellar checked.");
    d.idle().await;

    assert_eq!(
        d.transcript().await,
        [
            format!("user: {PREFIX} Go"),
            "assistant: <met__block t1> <met__quick t2>".to_owned(),
            "result t1: block ran with {}".to_owned(),
            "result t2: quick ran with {}".to_owned(),
            "assistant: Done.".to_owned(),
            format!("user: {PREFIX} Also check the cellar"),
            "assistant: Cellar checked.".to_owned(),
        ]
    );
    assert_eq!(d.tool_events(), ["end t1", "start t2", "end t2"]);
    d.stop().await;
}

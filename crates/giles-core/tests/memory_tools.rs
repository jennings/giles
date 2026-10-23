//! The `memory` and `memory_search` core tools end to end, registered by `Runtime::start` and
//! called by the model through the agent loop (PLAN section 6.7).

#[path = "support/turns.rs"]
mod turns;

use giles_types::{Block, CompletionRequest};
use serde_json::json;
use turns::{Daemon, calls, text};

fn system(request: &CompletionRequest) -> &str {
    match request.system.as_slice() {
        [Block::Text { text }] => text,
        other => panic!("system is not one text block: {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn writes_show_in_the_result_and_reach_the_prompt_in_the_next_session() {
    let mut d = Daemon::start("").await;
    d.provider.push(text("Noted."));
    d.say("I saw a giraffe today");
    d.idle().await;

    d.provider.push(calls(&[
        (
            "t1",
            "memory",
            json!({"scope": "user", "operations": [{"op": "add", "content": "Likes giraffes."}]}),
        ),
        (
            "t2",
            "memory",
            json!({"scope": "agent", "operations": [{"op": "add", "content": "Zoo diary."}]}),
        ),
        ("t3", "memory_search", json!({"query": "giraffes"})),
    ]));
    d.provider.push(text("Remembered."));
    d.say("Remember that");
    d.idle().await;

    let transcript = d.transcript().await.join("\n");
    for result in [
        "result t1: Saved. User memory is now [0% — 15/2,200]:\n\nLikes giraffes.",
        "result t2: Saved. Agent memory is now [0% — 10/2,200]:\n\nZoo diary.",
        "result t3: 1 match, best first (matches in **bold**):\n\n\
         [Thu 2026-10-22 17:30 BST] user on fake: I saw a **giraffe** today",
    ] {
        assert!(
            transcript.contains(result),
            "{result:?} not in {transcript}"
        );
    }
    let root = &d.runtime.root;
    let read = |path| std::fs::read_to_string(path).unwrap();
    assert_eq!(
        read(giles_core::memory::user_memory_file(root, "jeeves", "owner").unwrap()),
        "Likes giraffes.\n"
    );
    assert_eq!(
        read(giles_core::memory::agent_memory_file(root, "jeeves").unwrap()),
        "Zoo diary.\n"
    );

    // Startup registered both tools, and the session kept its prompt: the snapshot from its
    // start, without the new entries.
    let requests = d.provider.requests();
    assert_eq!(requests.len(), 3);
    let tools: Vec<&str> = requests[0].tools.iter().map(|t| t.name.as_str()).collect();
    assert!(
        tools.contains(&"memory") && tools.contains(&"memory_search"),
        "{tools:?}"
    );
    assert_eq!(requests[2].system, requests[0].system);
    assert!(!system(&requests[2]).contains("giraffes"));

    // A new session loads the memory as it is now.
    d.say("/new");
    d.idle().await;
    d.provider.push(text("Hello again."));
    d.say("Hi");
    d.idle().await;
    let requests = d.provider.requests();
    let fresh = system(requests.last().unwrap());
    assert!(fresh.contains("Likes giraffes."), "{fresh}");
    assert!(fresh.contains("Zoo diary."), "{fresh}");
    d.stop().await;
}

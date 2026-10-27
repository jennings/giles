//! The memory core tools (`memory_add`, `memory_edit`, `memory_forget`, `memory_recall`) and
//! `memory_search` end to end, registered by `Runtime::start` and called by the model through the
//! agent loop (PLAN section 6.7). What the model remembers is checked in the agent's memory
//! database.

#[path = "support/turns.rs"]
mod turns;

use giles_core::identity::owner;
use giles_store::{MemoryId, MemoryOwner, MemoryViewer};
use giles_types::{AgentId, Block, CompletionRequest};
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

    // The tools run in order, so the ids are 1 to 4. `memory_recall` is parallel-safe, so it
    // gets its own round, after the writes.
    d.provider.push(calls(&[
        (
            "t1",
            "memory_add",
            json!({"content": "Likes giraffes.", "core": true}),
        ),
        (
            "t2",
            "memory_add",
            json!({"content": "Zoo diary.", "core": true, "shared": true}),
        ),
        (
            "t3",
            "memory_add",
            json!({"content": "Saw a giraffe on Thursday."}),
        ),
        (
            "t4",
            "memory_add",
            json!({"content": "Temporary note.", "core": true}),
        ),
        ("t5", "memory_search", json!({"query": "giraffes"})),
    ]));
    d.provider.push(calls(&[(
        "t6",
        "memory_recall",
        json!({"query": "giraffe"}),
    )]));
    d.provider.push(calls(&[
        ("t7", "memory_forget", json!({"id": 4})),
        (
            "t8",
            "memory_edit",
            json!({"id": 1, "content": "Loves giraffes."}),
        ),
    ]));
    d.provider.push(text("Remembered."));
    d.say("Remember that");
    d.idle().await;

    let transcript = d.transcript().await.join("\n");
    for result in [
        "result t1: Added #1 ",
        "Likes giraffes.\nYour core memories [0% — 15/2,200]",
        "result t2: Added #2 ",
        "Zoo diary.\nShared core memories [0% — 10/2,200]",
        "result t3: Added #3 ",
        "Saw a giraffe on Thursday.\nresult t4: Added #4 ",
        "Temporary note.\nYour core memories [1% — 30/2,200]",
        "result t5: 1 match, best first (matches in **bold**):\n\n\
         [Thu 2026-10-22 17:30 BST] user on fake: I saw a **giraffe** today",
        "result t6: 2 memories, best first:",
        "result t7: Forgot #4 ",
        "Temporary note.\nYour core memories [0% — 15/2,200]",
        "result t8: Updated #1 ",
        "Loves giraffes.\nYour core memories [0% — 15/2,200]",
    ] {
        assert!(
            transcript.contains(result),
            "{result:?} not in {transcript}"
        );
    }
    // The recall found both giraffe memories, the core one and the one that isn't core.
    let recall = transcript.split("result t6:").nth(1).unwrap();
    let recall = recall.split("result t7:").next().unwrap();
    assert!(recall.contains("Likes giraffes."), "{recall}");
    assert!(recall.contains("Saw a giraffe on Thursday."), "{recall}");
    assert!(!recall.contains("Zoo diary."), "{recall}");

    // What the model wrote is in the agent's memory database.
    let jeeves = AgentId::new("jeeves").unwrap();
    let db = d.runtime.memory_dbs.get(&jeeves).await.unwrap();
    let viewer = MemoryViewer::new(MemoryOwner::User(owner()), true);
    let updated = db.get(&viewer, MemoryId::new(1)).await.unwrap();
    assert_eq!(updated.content, "Loves giraffes.");
    assert!(updated.core && !updated.shared);
    assert_eq!(updated.owner, MemoryOwner::User(owner()));
    let shared = db.get(&viewer, MemoryId::new(2)).await.unwrap();
    assert!(shared.core && shared.shared);
    let plain = db.get(&viewer, MemoryId::new(3)).await.unwrap();
    assert!(!plain.core && !plain.shared);
    assert!(db.get(&viewer, MemoryId::new(4)).await.is_err());
    let core = db.core_memories(&viewer).await.unwrap();
    let contents = |scope: &giles_store::CoreScope| -> Vec<String> {
        scope.memories.iter().map(|m| m.content.clone()).collect()
    };
    assert_eq!(contents(&core.shared), ["Zoo diary."]);
    assert_eq!(contents(&core.own), ["Loves giraffes."]);
    // Nothing is written to the old memory files.
    let agent_dir = d.runtime.root.agent_dir("jeeves").unwrap();
    assert!(!agent_dir.join("MEMORY.md").exists());
    assert!(!agent_dir.join("users/owner/MEMORY.md").exists());
    drop(db);

    // Startup registered the tools, and the session kept its prompt: the snapshot from its
    // start, without the new memories.
    let requests = d.provider.requests();
    assert_eq!(requests.len(), 5);
    let tools: Vec<&str> = requests[0].tools.iter().map(|t| t.name.as_str()).collect();
    for name in [
        "memory_add",
        "memory_edit",
        "memory_forget",
        "memory_recall",
        "memory_search",
    ] {
        assert!(tools.contains(&name), "{name} not in {tools:?}");
    }
    assert!(!tools.contains(&"memory"), "{tools:?}");
    for request in &requests[1..] {
        assert_eq!(request.system, requests[0].system);
    }
    assert!(!system(&requests[4]).contains("giraffes"));

    // A new session loads the core memories as they are now, and only those.
    d.say("/new");
    d.idle().await;
    d.provider.push(text("Hello again."));
    d.say("Hi");
    d.idle().await;
    let requests = d.provider.requests();
    let fresh = system(requests.last().unwrap());
    assert!(fresh.contains("Loves giraffes."), "{fresh}");
    assert!(fresh.contains("Zoo diary."), "{fresh}");
    assert!(!fresh.contains("Saw a giraffe on Thursday."), "{fresh}");
    assert!(!fresh.contains("Temporary note."), "{fresh}");
    d.stop().await;
}

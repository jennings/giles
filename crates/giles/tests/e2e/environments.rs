//! An agent with an environment: it creates a workspace and works in it, and `/stop` cancels a
//! running command. The environment is the echo environment plugin
//! (`plugins/giles-environment-echo`): workspaces and files live in its memory, and its commands
//! are `echo <text>`, `fail`, and `hang`.

use std::fs;

use serde_json::json;

use crate::harness::{Daemon, Home, wait_until};
use crate::provider::{FakeProvider, Reply, tool_names, tool_result};

/// The agent may use the environment and every core tool.
const WITH_ENVIRONMENT: &str = "tools = [\"*\"]\nenvironments = [\"box\"]\n";

const ENVIRONMENT_TOOLS: [&str; 11] = [
    "bash",
    "edit",
    "environment_create",
    "environment_delete",
    "environment_plugins",
    "environment_reset",
    "environment_status",
    "find",
    "grep",
    "read",
    "write",
];

#[test]
fn an_agent_creates_a_workspace_and_works_in_it() {
    let provider = FakeProvider::start();
    provider.script([
        Reply::call("call_1", "bash", json!({"command": "echo too early"})),
        Reply::call("call_2", "environment_plugins", json!({})),
        Reply::call(
            "call_3",
            "environment_create",
            json!({"plugin": "box", "options": {"note": "scratch"}}),
        ),
        Reply::call(
            "call_4",
            "write",
            json!({"path": "notes.txt", "content": "first line\nsecond line\n"}),
        ),
        Reply::call("call_5", "read", json!({"path": "notes.txt"})),
        Reply::call("call_6", "bash", json!({"command": "echo from the box"})),
        Reply::text("All done, sir."),
    ]);
    let home = Home::with_environment(&provider, WITH_ENVIRONMENT);
    let daemon = Daemon::start(&home);

    home.say("Make some notes in a workspace.");
    home.wait_sent("All done, sir.");

    let requests = provider.requests();
    assert_eq!(requests.len(), 7);
    // The agent has an environment, so it is offered the environment tools.
    let offered = tool_names(&requests[0]);
    for name in ENVIRONMENT_TOOLS {
        assert!(offered.iter().any(|offered| offered == name), "{name}");
    }
    // Before it has a workspace, a workspace tool says how to get one.
    let early = tool_result(&requests[1], "call_1");
    assert!(
        early.starts_with("Error: This conversation has no environment yet"),
        "{early}"
    );
    assert!(early.contains("`box`"), "{early}");
    assert!(early.contains("environment_create"), "{early}");
    // The plugin's options schema reaches the model.
    let plugins = tool_result(&requests[2], "call_2");
    assert!(plugins.contains("## `box`"), "{plugins}");
    assert!(plugins.contains("Reachable: yes"), "{plugins}");
    assert!(plugins.contains("\"note\""), "{plugins}");
    // Create, write, read, and run.
    let created = tool_result(&requests[3], "call_3");
    assert!(created.starts_with("Created the workspace."), "{created}");
    assert!(created.contains("Working directory: /work"), "{created}");
    assert_eq!(
        tool_result(&requests[4], "call_4"),
        "Wrote 23 bytes to notes.txt."
    );
    assert_eq!(
        tool_result(&requests[5], "call_5"),
        "     1\tfirst line\n     2\tsecond line\n"
    );
    assert_eq!(
        tool_result(&requests[6], "call_6"),
        "Exit code: 0\n\nstdout:\nfrom the box\n"
    );

    daemon.stop();
    provider.assert_no_surprises();
}

#[test]
fn an_agent_without_an_environment_is_not_offered_the_environment_tools() {
    let provider = FakeProvider::start();
    provider.script([Reply::text("Very good, sir.")]);
    let home = Home::new(&provider, "tools = [\"*\"]\n");
    let daemon = Daemon::start(&home);

    home.say("Hello.");
    home.wait_sent("Very good, sir.");
    let offered = tool_names(&provider.requests()[0]);
    assert!(
        offered.iter().any(|name| name == "memory_add"),
        "{offered:?}"
    );
    for name in ENVIRONMENT_TOOLS {
        assert!(!offered.iter().any(|offered| offered == name), "{name}");
    }

    daemon.stop();
    provider.assert_no_surprises();
}

#[test]
fn stop_cancels_a_running_command_in_the_environment() {
    let provider = FakeProvider::start();
    provider.script([
        Reply::call("call_1", "environment_create", json!({"plugin": "box"})),
        Reply::call("call_2", "bash", json!({"command": "hang"})),
    ]);
    let home = Home::with_environment(&provider, WITH_ENVIRONMENT);
    let daemon = Daemon::start(&home);

    home.say("Run something long.");
    // The command is running in the plugin, and the model waits for its result.
    let started = home.environment_dir().join("exec-started");
    wait_until("the command to start", || started.is_file().then_some(()));
    let cancelled = home.environment_dir().join("exec-cancelled");
    assert!(!cancelled.exists());

    home.say("/stop");
    daemon.wait_for_log("turn stopped by /stop");
    // The core dropped the command's stream, and the plugin was told to kill it.
    wait_until("the plugin to cancel the command", || {
        cancelled.is_file().then_some(())
    });
    assert_eq!(fs::read_to_string(&cancelled).unwrap(), "cancelled");

    // The conversation goes on, and the model sees that the call was cancelled.
    provider.script([Reply::text("Back, sir.")]);
    home.say("Hello?");
    home.wait_sent("Back, sir.");
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(tool_result(&requests[2], "call_2"), "Error: cancelled");

    daemon.stop();
    provider.assert_no_surprises();
}

#[test]
fn env_reset_replaces_the_workspace_and_new_keeps_it() {
    let provider = FakeProvider::start();
    let home = Home::with_environment(&provider, WITH_ENVIRONMENT);
    let daemon = Daemon::start(&home);

    // No workspace yet: the command says so, and the model isn't involved.
    home.say("/env reset");
    home.wait_sent("This conversation has no workspace.");
    assert!(provider.requests().is_empty());

    provider.script([
        Reply::call("call_1", "environment_create", json!({"plugin": "box"})),
        Reply::call(
            "call_2",
            "write",
            json!({"path": "notes.txt", "content": "keep me\n"}),
        ),
        Reply::text("Saved, sir."),
    ]);
    home.say("Save a note.");
    home.wait_sent("Saved, sir.");

    // `/new` starts a fresh session, and the workspace and its files stay.
    home.say("/new");
    provider.script([
        Reply::call("call_3", "read", json!({"path": "notes.txt"})),
        Reply::text("Still there, sir."),
    ]);
    home.say("Is the note still there?");
    home.wait_sent("Still there, sir.");
    let requests = provider.requests();
    assert_eq!(requests.len(), 5);
    assert_eq!(tool_result(&requests[4], "call_3"), "     1\tkeep me\n");

    // `/env reset` deletes the workspace and creates a new, empty one.
    let seen = home.sent().len();
    home.say("/env reset");
    home.wait_sent_after(seen, "new, empty one");
    provider.script([
        Reply::call("call_4", "read", json!({"path": "notes.txt"})),
        Reply::text("Gone, sir."),
    ]);
    home.say("And now?");
    home.wait_sent("Gone, sir.");
    let requests = provider.requests();
    assert_eq!(requests.len(), 7);
    let gone = tool_result(&requests[6], "call_4");
    assert!(gone.starts_with("Error: "), "{gone}");
    assert!(gone.contains("no file notes.txt"), "{gone}");

    daemon.stop();
    provider.assert_no_surprises();
}

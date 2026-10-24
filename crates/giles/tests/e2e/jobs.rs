//! A configured job fires in the scheduler and its reply is delivered through the outbox.

use jiff::{SignedDuration, Timestamp};
use serde_json::json;

use crate::harness::{AGENT, Daemon, Home, OUTBOX_WAIT, wait_until_within};
use crate::provider::{FakeProvider, Reply, texts_with_role};

/// A job whose only time is midnight on 1 January. The test makes that time "missed while the core
/// was down" (see [`seed_last_run`]), so the scheduler runs it once at startup, deterministically
/// and without waiting for a clock.
const JOB: &str =
    "[[jobs]]\nid = \"digest\"\ncron = \"0 0 1 1 *\"\nprompt = \"Summarize the day.\"\n";

/// Records that the job last ran 400 days ago, so a 1 January has passed since: the scheduler
/// coalesces the missed time into one run at the next start. Creates the database the daemon will
/// open (the scheduler's own record, `job_runs`, is what it reads).
fn seed_last_run(home: &Home) {
    std::fs::create_dir_all(home.root().join("data")).unwrap();
    let long_ago = Timestamp::now()
        .checked_sub(SignedDuration::from_hours(24 * 400))
        .unwrap();
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            let store = giles_store::Store::open(home.root().join("data/giles.db"))
                .await
                .unwrap();
            store
                .job_set_last_run(
                    giles_types::AgentId::new(AGENT).unwrap(),
                    giles_store::JobKey::Config("digest".to_owned()),
                    long_ago,
                )
                .await
                .unwrap();
        });
}

#[test]
fn a_job_fires_and_its_reply_is_delivered_to_the_owner_by_recipient() {
    let provider = FakeProvider::start();
    provider.script([Reply::text("Your summary, sir.")]);
    let home = Home::new(&provider, JOB);
    seed_last_run(&home);
    let daemon = Daemon::start(&home);

    // No conversation exists: the reply is a proactive send to the owner's identity
    // (`echo:owner`), which the channel owning that namespace gets as a recipient.
    let sent = home.wait_sent_within(OUTBOX_WAIT, 0, "Your summary, sir.");
    assert_eq!(sent["message"]["target"], json!({"recipient": "owner"}));
    // The job ran as a turn with its prompt, labelled as scheduled.
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let users = texts_with_role(&requests[0], "user");
    assert_eq!(users.len(), 1, "{users:?}");
    assert!(users[0].contains("Scheduled job `digest`"), "{users:?}");
    assert!(users[0].contains("Summarize the day."), "{users:?}");
    // Delivered, so nothing waits in the outbox.
    wait_until_within(OUTBOX_WAIT, "an empty outbox", || {
        home.outbox_rows().is_empty().then_some(())
    });

    daemon.stop();
    provider.assert_no_surprises();
}

#[test]
fn a_reply_the_channel_cannot_take_waits_in_the_outbox_through_a_restart() {
    let provider = FakeProvider::start();
    provider.script([Reply::text("Your summary, sir.")]);
    let home = Home::new(&provider, JOB);
    // The echo channel refuses sends to the owner, as a messaging service that is down would.
    home.configure_echo("unavailable = [\"owner\"]\n");
    seed_last_run(&home);
    let daemon = Daemon::start(&home);

    // (Attempts before the plugin is up fail with a different error.)
    wait_until_within(OUTBOX_WAIT, "the channel refusing the reply", || {
        home.outbox_rows().into_iter().find(|(_, error)| {
            error
                .as_deref()
                .is_some_and(|error| error.contains("not reachable"))
        })
    });
    assert!(home.sent().is_empty());
    daemon.stop();
    // Still queued after the daemon is gone.
    assert_eq!(home.outbox_rows().len(), 1);

    // The channel is back at the next start; the queued reply is delivered, once, and the job,
    // which already ran, does not run again.
    home.configure_echo("");
    let daemon = Daemon::start(&home);
    let sent = home.wait_sent_within(OUTBOX_WAIT, 0, "Your summary, sir.");
    assert_eq!(sent["message"]["target"], json!({"recipient": "owner"}));
    wait_until_within(OUTBOX_WAIT, "an empty outbox", || {
        home.outbox_rows().is_empty().then_some(())
    });
    assert_eq!(home.texts(), ["Your summary, sir."]);
    assert_eq!(provider.requests().len(), 1);

    daemon.stop();
    provider.assert_no_surprises();
}

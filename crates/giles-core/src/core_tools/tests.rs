//! The `memory_add`, `memory_edit`, `memory_forget`, `memory_recall`, `memory_search`, and
//! `schedule` tools through [`CoreTool::call`], against a temporary data root and databases. The
//! agent `jeeves` has a 60-character memory cap and one config job.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use giles_store::{
    JournalMode, MemoryEdit, MemoryId, MemoryOwner, MemoryStore, MemoryViewer, Session, SessionId,
    SessionKey, SessionKind, Store,
};
use giles_types::{
    AgentId, Block, CallContext, ConversationId, Message, MessageMeta, Role, Timestamp, UserId,
};
use serde_json::{Value, json};
use tempfile::TempDir;

use super::*;
use super::{memory_add, memory_edit, memory_forget, memory_recall, memory_search, schedule};
use crate::config::{Config, LiveConfig, LoadOptions};
use crate::identity::owner;
use crate::memory_dbs::MemoryDbs;
use crate::paths::DataRoot;
use crate::scheduler::JobChanges;

const AGENT: &str = "jeeves";
const CAP: usize = 60;

struct Setup {
    _dir: TempDir,
    config: LiveConfig,
    store: Store,
    memory_dbs: MemoryDbs,
    clock: Arc<TickClock>,
}

/// Each reading is a minute after the last, starting at [`schedule_now`], so memories are made
/// and changed at distinct, known times.
#[derive(Default)]
struct TickClock(AtomicI64);

impl crate::sessions::Clock for TickClock {
    fn now(&self) -> Timestamp {
        schedule_now() + jiff::SignedDuration::from_secs(60 * self.0.fetch_add(1, Ordering::SeqCst))
    }
}

impl Setup {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path(), Path::new("/tmp")).unwrap();
        root.create_layout().unwrap();
        let write = |relative: &str, contents: &str| {
            let path = dir.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        };
        write(
            "giles.toml",
            "[owner]\nidentities = [\"fake:owner\"]\ntimezone = \"Europe/London\"\n\
             [provider.anthropic]\nconfig = { api_key = \"sk-test\" }\n",
        );
        write(
            &format!("agents/{AGENT}/agent.toml"),
            &format!(
                "models = [\"anthropic/test-model\"]\nmemory_cap = {CAP}\n\
                 [[jobs]]\nid = \"check-in\"\ncron = \"0 9 * * *\"\nsession = \"dm\"\n\
                 prompt = \"Anything to tell me?\"\n"
            ),
        );
        write(
            "agents/other/agent.toml",
            "models = [\"anthropic/test-model\"]\n",
        );
        let config = Config::load_with(
            &root,
            &LoadOptions {
                env: &|_| None,
                giles_exe: None,
                ..LoadOptions::default()
            },
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let store = Store::open(root.database_file()).await.unwrap();
        Self {
            _dir: dir,
            config: LiveConfig::new(Arc::new(config)),
            store,
            memory_dbs: MemoryDbs::new(root, JournalMode::Wal),
            clock: Arc::default(),
        }
    }

    fn search(&self) -> MemorySearchTool {
        MemorySearchTool::new(self.store.clone(), self.config.clone())
    }

    /// `jeeves`'s memory database.
    async fn db(&self) -> MemoryStore {
        self.memory_dbs
            .get(&AgentId::new(AGENT).unwrap())
            .await
            .unwrap()
    }

    async fn add(&self, input: Value, who: &Actor) -> ToolOutput {
        let tool = MemoryAddTool::new(
            self.memory_dbs.clone(),
            self.config.clone(),
            self.clock.clone(),
        );
        call(&tool, input, who.requester.clone(), who.session.clone()).await
    }

    async fn edit(&self, input: Value, who: &Actor) -> ToolOutput {
        let tool = MemoryEditTool::new(
            self.memory_dbs.clone(),
            self.config.clone(),
            self.clock.clone(),
        );
        call(&tool, input, who.requester.clone(), who.session.clone()).await
    }

    async fn forget(&self, input: Value, who: &Actor) -> ToolOutput {
        let tool = MemoryForgetTool::new(
            self.memory_dbs.clone(),
            self.config.clone(),
            self.clock.clone(),
        );
        call(&tool, input, who.requester.clone(), who.session.clone()).await
    }

    async fn recall(&self, input: Value, who: &Actor) -> ToolOutput {
        let tool = MemoryRecallTool::new(
            self.memory_dbs.clone(),
            self.config.clone(),
            self.clock.clone(),
        );
        call(&tool, input, who.requester.clone(), who.session.clone()).await
    }

    /// Adds a memory as `who` and returns its id; the call must succeed.
    async fn remember(&self, who: &Actor, content: &str, core: bool, shared: bool) -> i64 {
        let output = self
            .add(
                json!({"content": content, "core": core, "shared": shared}),
                who,
            )
            .await;
        id_of(&output)
    }

    /// The text of a successful recall of `query` as `who`.
    async fn recalled(&self, query: &str, who: &Actor) -> String {
        ok(&self.recall(json!({"query": query}), who).await).to_owned()
    }
}

fn user(name: &str) -> UserId {
    UserId::new(name).unwrap()
}

/// A session of `jeeves` (not stored).
fn session(kind: SessionKind, user: Option<UserId>) -> Session {
    let channel = (kind != SessionKind::Job).then(|| "fake".to_owned());
    Session {
        id: SessionId::new(1),
        agent: AgentId::new(AGENT).unwrap(),
        kind,
        conversation: channel
            .as_ref()
            .map(|_| ConversationId::new("chat").unwrap()),
        channel,
        user,
        started_at: Timestamp::UNIX_EPOCH,
        ended_at: None,
        last_message_at: None,
    }
}

fn dm() -> Session {
    session(SessionKind::Direct, Some(owner()))
}

/// Calls `tool` in a turn of `requester` in `session` (a job run if it's a job session).
async fn call(
    tool: &dyn CoreTool,
    input: Value,
    requester: Option<UserId>,
    session: Session,
) -> ToolOutput {
    let job = session.kind == SessionKind::Job;
    call_as(tool, input, requester, session, job).await
}

/// Calls `tool` in a turn of `requester` in `session`, a job run if `job`.
async fn call_as(
    tool: &dyn CoreTool,
    input: Value,
    requester: Option<UserId>,
    session: Session,
    job: bool,
) -> ToolOutput {
    tool.call(CoreCall {
        id: "t1".to_owned(),
        input,
        context: CallContext {
            agent: session.agent.clone(),
            requester,
            conversation: None,
        },
        turn: Arc::new(TurnContext {
            session,
            reply: None,
            skills: Arc::default(),
            job,
        }),
        media: Arc::default(),
    })
    .await
}

fn text(output: &ToolOutput) -> &str {
    match output.content.as_slice() {
        [Block::Text { text }] => text,
        other => panic!("not one text block: {other:?}"),
    }
}

fn ok(output: &ToolOutput) -> &str {
    assert!(!output.is_error, "error: {}", text(output));
    text(output)
}

fn err(output: &ToolOutput) -> &str {
    assert!(output.is_error, "not an error: {}", text(output));
    text(output)
}

/// Who makes a call: the turn's requester and the session it runs in.
#[derive(Clone)]
struct Actor {
    requester: Option<UserId>,
    session: Session,
}

impl Actor {
    /// The owner in their DM.
    fn owner() -> Self {
        Self {
            requester: Some(owner()),
            session: dm(),
        }
    }

    /// The guest `ann` in her DM.
    fn ann() -> Self {
        Self {
            requester: Some(user("ann")),
            session: session(SessionKind::Direct, Some(user("ann"))),
        }
    }

    /// `requester` speaking in the group conversation `conversation` of channel `fake`.
    fn group(requester: &UserId, conversation: &str) -> Self {
        let mut session = session(SessionKind::Group, None);
        session.conversation = Some(ConversationId::new(conversation).unwrap());
        Self {
            requester: Some(requester.clone()),
            session,
        }
    }

    /// A config job: no requester, in the session of the owner's job.
    fn config_job() -> Self {
        Self {
            requester: None,
            session: session(SessionKind::Job, Some(owner())),
        }
    }

    /// A runtime job the owner created: it runs as the owner.
    fn owner_job() -> Self {
        Self {
            requester: Some(owner()),
            session: session(SessionKind::Job, Some(owner())),
        }
    }
}

/// The owner's view of their own memories, for checking the database directly.
fn owner_viewer() -> MemoryViewer {
    MemoryViewer::new(MemoryOwner::User(owner()), true)
}

/// The id in a result that starts with a memory's line (`Added #12 ...`).
fn id_of(output: &ToolOutput) -> i64 {
    let rest = ok(output).split_once('#').unwrap().1;
    rest.chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap()
}

#[tokio::test]
async fn memories_are_added_edited_and_forgotten_by_id_and_results_show_them() {
    let s = Setup::new().await;
    let me = Actor::owner();

    let added = s.add(json!({"content": "Tea at four."}), &me).await;
    let tea = id_of(&added);
    let shown = ok(&added);
    assert!(shown.starts_with(&format!("Added #{tea} ")), "{shown}");
    assert!(shown.contains("Tea at four."), "{shown}");
    // Not core: no usage line.
    assert!(!shown.contains("core memories"), "{shown}");
    let stored = s
        .db()
        .await
        .get(&owner_viewer(), MemoryId::new(tea))
        .await
        .unwrap();
    assert_eq!(stored.content, "Tea at four.");
    assert!(!stored.core && !stored.shared);
    assert_eq!(stored.source_session, Some(SessionId::new(1)));
    assert_eq!(stored.owner, MemoryOwner::User(owner()));

    let second = s.add(json!({"content": "Jazz on Fridays."}), &me).await;
    assert_ne!(id_of(&second), tea);

    let edited = s
        .edit(json!({"id": tea, "content": "Tea at five."}), &me)
        .await;
    let shown = ok(&edited);
    assert!(shown.starts_with(&format!("Updated #{tea} ")), "{shown}");
    assert!(shown.contains("Tea at five."), "{shown}");
    assert!(!shown.contains("Tea at four."), "{shown}");
    let stored = s
        .db()
        .await
        .get(&owner_viewer(), MemoryId::new(tea))
        .await
        .unwrap();
    assert_eq!(stored.content, "Tea at five.");
    assert!(stored.updated_at > stored.created_at);

    let forgotten = s.forget(json!({"id": tea}), &me).await;
    let shown = ok(&forgotten);
    assert!(shown.starts_with(&format!("Forgot #{tea} ")), "{shown}");
    assert!(shown.contains("Tea at five."), "{shown}");
    assert_eq!(
        s.db()
            .await
            .get(&owner_viewer(), MemoryId::new(tea))
            .await
            .unwrap_err()
            .to_string(),
        format!("unknown memory {tea}")
    );
    // Forgetting it again is "no such memory", and the other memory is untouched.
    assert_eq!(
        err(&s.forget(json!({"id": tea}), &me).await),
        format!("no memory #{tea}")
    );
    assert_eq!(
        err(&s.edit(json!({"id": tea, "core": true}), &me).await),
        format!("no memory #{tea}")
    );
    assert!(s.recalled("jazz", &me).await.contains("Jazz on Fridays."));
}

#[tokio::test]
async fn core_changes_show_the_usage_of_each_scope_they_touch() {
    let s = Setup::new().await;
    let me = Actor::owner();

    let jazz = s
        .add(json!({"content": "Likes jazz.", "core": true}), &me)
        .await;
    let id = id_of(&jazz);
    let shown = ok(&jazz);
    assert!(shown.starts_with("Added #"), "{shown}");
    assert!(
        shown.ends_with("\nYour core memories [18% — 11/60]"),
        "{shown}"
    );

    // Sharing a core memory moves it from the owner's scope into the shared one: both show.
    let shared = s.edit(json!({"id": id, "shared": true}), &me).await;
    assert!(
        ok(&shared)
            .ends_with("\nYour core memories [0% — 0/60]\nShared core memories [18% — 11/60]"),
        "{}",
        text(&shared)
    );

    // Demoting shows the scope it left.
    let demoted = s.edit(json!({"id": id, "core": false}), &me).await;
    assert!(
        ok(&demoted).ends_with("\nShared core memories [0% — 0/60]"),
        "{}",
        text(&demoted)
    );

    // A change to a memory that is not core and stays so shows no usage.
    let plain = s
        .edit(json!({"id": id, "content": "Likes jazz a lot."}), &me)
        .await;
    assert!(!ok(&plain).contains("core memories"), "{}", text(&plain));

    let promoted = s.edit(json!({"id": id, "core": true}), &me).await;
    assert!(
        ok(&promoted).ends_with("\nShared core memories [28% — 17/60]"),
        "{}",
        text(&promoted)
    );
    let forgotten = s.forget(json!({"id": id}), &me).await;
    assert!(
        ok(&forgotten).ends_with("\nShared core memories [0% — 0/60]"),
        "{}",
        text(&forgotten)
    );

    // A group's scope is its own, and a config job's memories are shared.
    let group = Actor::group(&user("ann"), "chat");
    let noted = s
        .add(
            json!({"content": "Meets on Mondays.", "core": true}),
            &group,
        )
        .await;
    assert!(
        ok(&noted).ends_with("\nThis group's core memories [28% — 17/60]"),
        "{}",
        text(&noted)
    );
    let job = s
        .add(
            json!({"content": "Mind the cat.", "core": true, "shared": true}),
            &Actor::config_job(),
        )
        .await;
    assert!(
        ok(&job).ends_with("\nShared core memories [21% — 13/60]"),
        "{}",
        text(&job)
    );
    // A memory that isn't core never shows usage.
    let plain = s.add(json!({"content": "Rains a lot."}), &group).await;
    assert!(!ok(&plain).contains("core memories"), "{}", text(&plain));
}

#[tokio::test]
async fn an_over_cap_core_write_lists_the_scope_and_changes_nothing() {
    let s = Setup::new().await;
    let me = Actor::owner();
    let bees = s
        .remember(&me, "Keeps bees on the roof of the shed.", true, false)
        .await;
    let tea = s.remember(&me, "Tea at four.", true, false).await;
    // Another scope's core memories don't count against the owner's: this one is shared.
    s.remember(&me, &"s".repeat(60), true, true).await;

    // 47 characters now; 26 more is 73, 13 over.
    let long = json!({"content": "Likes jazz and long walks.", "core": true});
    let output = s.add(long.clone(), &me).await;
    let shown = err(&output);
    assert!(
        shown.starts_with(
            "not saved: your core memories would use 73/60 characters, 13 over the cap; \
             nothing was changed."
        ),
        "{shown}"
    );
    assert!(shown.contains("memory_edit core=false"), "{shown}");
    assert!(shown.contains("forgetting"), "{shown}");
    assert!(
        shown.contains("Your core memories [78% — 47/60]:"),
        "{shown}"
    );
    assert!(shown.contains(&format!("#{bees} ")), "{shown}");
    assert!(
        shown.contains("Keeps bees on the roof of the shed. — 35 chars"),
        "{shown}"
    );
    assert!(shown.contains(&format!("#{tea} ")), "{shown}");
    assert!(shown.contains("Tea at four. — 12 chars"), "{shown}");
    let core = s.db().await.core_memories(&owner_viewer()).await.unwrap();
    assert_eq!(core.own.chars, 47);
    assert_eq!(core.own.memories.len(), 2);
    assert!(
        !s.recalled("walks", &me)
            .await
            .contains("Likes jazz and long walks."),
    );

    // The shared scope is full too, and its error names it.
    let shared = s
        .add(json!({"content": "y", "core": true, "shared": true}), &me)
        .await;
    assert!(
        err(&shared).starts_with(
            "not saved: the shared core memories would use 61/60 characters, 1 over the cap;"
        ),
        "{}",
        text(&shared)
    );

    // The same memory that isn't core fits, and so does a core one after demoting another.
    ok(&s
        .add(json!({"content": "Likes jazz and long walks."}), &me)
        .await);
    ok(&s.edit(json!({"id": bees, "core": false}), &me).await);
    let fits = s.add(long, &me).await;
    assert!(
        ok(&fits).ends_with("\nYour core memories [63% — 38/60]"),
        "{}",
        text(&fits)
    );

    // An edit that grows a core memory past the cap changes nothing.
    let grown = s
        .edit(json!({"id": tea, "content": "t".repeat(40)}), &me)
        .await;
    assert!(err(&grown).contains("over the cap"), "{}", text(&grown));
    let stored = s
        .db()
        .await
        .get(&owner_viewer(), MemoryId::new(tea))
        .await
        .unwrap();
    assert_eq!(stored.content, "Tea at four.");
    // So does making a memory core that doesn't fit (the bees are 35 characters).
    let promote = s.edit(json!({"id": bees, "core": true}), &me).await;
    assert!(
        err(&promote).starts_with(
            "not saved: your core memories would use 73/60 characters, 13 over the cap;"
        ),
        "{}",
        text(&promote)
    );
    assert!(
        !s.db()
            .await
            .get(&owner_viewer(), MemoryId::new(bees))
            .await
            .unwrap()
            .core
    );
}

#[tokio::test]
async fn each_scope_has_its_own_core_cap_from_the_agent() {
    let s = Setup::new().await;
    let fill = |shared: bool| json!({"content": "x".repeat(CAP), "core": true, "shared": shared});
    let one = json!({"content": "y", "core": true});
    let me = Actor::owner();
    let group = Actor::group(&owner(), "chat");
    let other_group = Actor::group(&owner(), "other-room");
    ok(&s.add(fill(true), &me).await);
    ok(&s.add(fill(false), &me).await);
    ok(&s.add(fill(false), &group).await);
    ok(&s.add(fill(false), &other_group).await);
    ok(&s.add(fill(false), &Actor::ann()).await);
    for who in [&me, &group, &other_group, &Actor::ann()] {
        assert!(
            err(&s.add(one.clone(), who).await).contains("1 over the cap"),
            "full scope accepts more"
        );
    }
}

#[tokio::test]
async fn content_is_limited_in_characters() {
    let s = Setup::new().await;
    let me = Actor::owner();
    let too_long = s.add(json!({"content": "z".repeat(1001)}), &me).await;
    assert_eq!(
        err(&too_long),
        "the memory is 1001 characters; the limit is 1000. Shorten it, or split it into \
         several memories"
    );
    // A thousand multi-byte characters fit.
    ok(&s.add(json!({"content": "é".repeat(1000)}), &me).await);
    // An edit is held to the same limit.
    let id = s.remember(&me, "short", false, false).await;
    let edit = s
        .edit(json!({"id": id, "content": "z".repeat(1001)}), &me)
        .await;
    assert!(err(&edit).contains("1001 characters"), "{}", text(&edit));
}

#[tokio::test]
async fn private_memories_are_visible_only_in_their_owners_sessions() {
    let s = Setup::new().await;
    let me = Actor::owner();
    let ann = Actor::ann();
    let group = Actor::group(&user("ann"), "chat");
    let other_group = Actor::group(&user("ann"), "other-room");
    let job = Actor::config_job();

    let mine = s.remember(&me, "alpha owner diary", false, false).await;
    s.remember(&me, "bravo shared rule", false, true).await;
    s.remember(&ann, "charlie ann diary", false, false).await;
    let note = s.remember(&group, "delta group note", false, false).await;
    // A job the owner created runs as the owner, in the owner's scope.
    s.remember(&Actor::owner_job(), "echo owner job diary", false, false)
        .await;

    let found = s.recalled("diary", &me).await;
    assert!(found.contains("alpha owner diary"), "{found}");
    assert!(found.contains("echo owner job diary"), "{found}");
    assert!(!found.contains("charlie"), "{found}");
    let found = s.recalled("diary", &ann).await;
    assert!(found.contains("charlie ann diary"), "{found}");
    assert!(
        !found.contains("alpha") && !found.contains("echo"),
        "{found}"
    );

    // A group never sees a participant's DM memories, and DMs never see the group's.
    assert_eq!(
        s.recalled("diary", &group).await,
        memory_recall::NO_MEMORIES
    );
    assert!(
        s.recalled("note", &group)
            .await
            .contains("delta group note")
    );
    assert_eq!(
        s.recalled("note", &other_group).await,
        memory_recall::NO_MEMORIES
    );
    assert_eq!(s.recalled("note", &me).await, memory_recall::NO_MEMORIES);
    assert_eq!(s.recalled("note", &ann).await, memory_recall::NO_MEMORIES);
    assert_eq!(
        err(&s.edit(json!({"id": note, "content": "x"}), &me).await),
        format!("no memory #{note}")
    );
    assert_eq!(
        err(&s.forget(json!({"id": note}), &ann).await),
        format!("no memory #{note}")
    );
    // A group's participants share its memories.
    let owner_in_group = Actor::group(&owner(), "chat");
    assert!(
        s.recalled("note", &owner_in_group)
            .await
            .contains("delta group note")
    );
    ok(&s
        .edit(
            json!({"id": note, "content": "delta group note, revised"}),
            &owner_in_group,
        )
        .await);

    // A config job has no requester and so no private scope: it sees shared memories only.
    assert_eq!(s.recalled("diary", &job).await, memory_recall::NO_MEMORIES);
    assert_eq!(s.recalled("note", &job).await, memory_recall::NO_MEMORIES);
    assert!(s.recalled("rule", &job).await.contains("bravo shared rule"));
    // Every session sees a shared memory, and the others' recall doesn't leak what it can't see.
    for who in [&me, &ann, &group, &other_group, &job] {
        let found = s.recalled("bravo", who).await;
        assert!(found.contains("bravo shared rule"), "{found}");
    }
    let everything = s.recalled("alpha bravo charlie delta echo", &ann).await;
    assert!(
        everything.contains("bravo") && everything.contains("charlie"),
        "{everything}"
    );
    assert!(
        !everything.contains("alpha")
            && !everything.contains("delta")
            && !everything.contains("echo"),
        "{everything}"
    );
    assert!(
        everything.starts_with("2 memories, best first:"),
        "{everything}"
    );

    // The owner's memory is still there.
    assert_eq!(
        s.db()
            .await
            .get(&owner_viewer(), MemoryId::new(mine))
            .await
            .unwrap()
            .content,
        "alpha owner diary"
    );
}

#[tokio::test]
async fn shared_memories_are_written_only_in_owner_turns_and_config_jobs() {
    let s = Setup::new().await;
    let me = Actor::owner();
    let ann = Actor::ann();
    let forbidden = |output: &ToolOutput| {
        assert!(
            err(output).contains("only in the owner's turns and in config jobs"),
            "{}",
            text(output)
        );
    };

    // A guest can't create a shared memory, nor mark one of hers shared.
    forbidden(
        &s.add(json!({"content": "Plant", "shared": true}), &ann)
            .await,
    );
    let plum = s.remember(&ann, "Ann likes plums", false, false).await;
    forbidden(&s.edit(json!({"id": plum, "shared": true}), &ann).await);
    let stored = s
        .db()
        .await
        .get(
            &MemoryViewer::new(MemoryOwner::User(user("ann")), false),
            MemoryId::new(plum),
        )
        .await
        .unwrap();
    assert!(!stored.shared);

    // She sees the owner's shared memory but can't change or forget it, nor make it core.
    let rule = s.remember(&me, "House rule: shoes off", false, true).await;
    assert!(
        s.recalled("shoes", &ann)
            .await
            .contains("House rule: shoes off")
    );
    forbidden(
        &s.edit(json!({"id": rule, "content": "Shoes on"}), &ann)
            .await,
    );
    forbidden(&s.edit(json!({"id": rule, "core": true}), &ann).await);
    forbidden(&s.edit(json!({"id": rule, "shared": false}), &ann).await);
    forbidden(&s.forget(json!({"id": rule}), &ann).await);
    assert!(s.recalled("shoes", &me).await.contains("shoes off"));

    // Nor can a group, whoever speaks, unless it's the owner.
    let ann_in_group = Actor::group(&user("ann"), "chat");
    forbidden(
        &s.add(json!({"content": "Plant", "shared": true}), &ann_in_group)
            .await,
    );
    let owner_in_group = Actor::group(&owner(), "chat");
    let from_group = s
        .remember(&owner_in_group, "Owner said so", false, true)
        .await;
    forbidden(&s.forget(json!({"id": from_group}), &ann_in_group).await);
    // A shared memory keeps its author as owner but is visible to all.
    assert!(s.recalled("said", &me).await.contains("Owner said so"));

    // A config job writes, changes, and forgets shared memories, with no owner.
    let job = Actor::config_job();
    let reminder = s.remember(&job, "Water the ferns", false, true).await;
    let stored = s
        .db()
        .await
        .get(&owner_viewer(), MemoryId::new(reminder))
        .await
        .unwrap();
    assert!(stored.shared);
    assert_eq!(stored.owner, MemoryOwner::None);
    ok(&s
        .edit(
            json!({"id": reminder, "content": "Water the ferns daily"}),
            &job,
        )
        .await);
    ok(&s
        .edit(json!({"id": rule, "content": "Shoes off, please"}), &job)
        .await);
    ok(&s.forget(json!({"id": rule}), &job).await);
    // The owner can change what a job wrote.
    ok(&s
        .edit(
            json!({"id": reminder, "content": "Water the ferns weekly"}),
            &me,
        )
        .await);
    // But a config job's private memory could never be read back, so it must be shared.
    let private = s.add(json!({"content": "Secret"}), &job).await;
    assert!(
        err(&private).starts_with("invalid input: a memory with no owner must be shared"),
        "{}",
        text(&private)
    );
    // And it can't reach anyone's private memories.
    assert_eq!(
        err(&s.edit(json!({"id": plum, "content": "x"}), &job).await),
        format!("no memory #{plum}")
    );
    assert_eq!(
        err(&s.forget(json!({"id": plum}), &job).await),
        format!("no memory #{plum}")
    );
}

#[tokio::test]
async fn an_invisible_memory_reads_like_one_that_does_not_exist() {
    let s = Setup::new().await;
    let me = Actor::owner();
    let ann = Actor::ann();
    let secret = s.remember(&me, "alpha secret", true, false).await;
    let missing = secret + 1000;

    for (input, tool) in [
        (json!({"id": secret, "content": "x"}), "edit"),
        (json!({"id": missing, "content": "x"}), "edit"),
        (json!({"id": secret}), "forget"),
        (json!({"id": missing}), "forget"),
    ] {
        let output = match tool {
            "edit" => s.edit(input.clone(), &ann).await,
            _ => s.forget(input.clone(), &ann).await,
        };
        let id = input["id"].as_i64().unwrap();
        assert_eq!(err(&output), format!("no memory #{id}"));
    }
    // Nothing changed, and no usage or content leaked.
    let stored = s
        .db()
        .await
        .get(&owner_viewer(), MemoryId::new(secret))
        .await
        .unwrap();
    assert_eq!(stored.content, "alpha secret");
    assert_eq!(s.recalled("secret", &ann).await, memory_recall::NO_MEMORIES);

    // A guest can't use the owner's DM scope even by speaking in the owner's session.
    let intruder = Actor {
        requester: Some(user("ann")),
        session: dm(),
    };
    for output in [
        s.add(json!({"content": "x"}), &intruder).await,
        s.edit(json!({"id": secret, "content": "x"}), &intruder)
            .await,
        s.forget(json!({"id": secret}), &intruder).await,
        s.recall(json!({"query": "secret"}), &intruder).await,
    ] {
        assert!(!err(&output).contains("alpha"), "{}", text(&output));
    }
    assert_eq!(
        s.db()
            .await
            .core_memories(&owner_viewer())
            .await
            .unwrap()
            .own
            .chars,
        "alpha secret".len()
    );
}

#[tokio::test]
async fn recall_ranks_by_relevance_then_recency_and_shows_flags_and_dates() {
    let s = Setup::new().await;
    let me = Actor::owner();
    let both = s.remember(&me, "alpha beta", false, false).await;
    let one = s.remember(&me, "alpha", false, false).await;
    s.remember(&me, "gamma", false, false).await;

    let found = s.recalled("alpha beta", &me).await;
    assert!(found.starts_with("2 memories, best first:"), "{found}");
    let first = found.find(&format!("#{both} ")).unwrap();
    let second = found.find(&format!("#{one} ")).unwrap();
    assert!(first < second, "{found}");
    assert!(!found.contains("gamma"), "{found}");

    // Equal matches: the most recently updated first.
    let one_old = s.remember(&me, "delta one", false, false).await;
    let two_old = s.remember(&me, "delta two", false, false).await;
    let found = s.recalled("delta", &me).await;
    assert!(
        found.find(&format!("#{two_old} ")).unwrap() < found.find(&format!("#{one_old} ")).unwrap(),
        "{found}"
    );
    // Editing the older one makes it the newest.
    ok(&s
        .edit(json!({"id": one_old, "content": "delta uno"}), &me)
        .await);
    let found = s.recalled("delta", &me).await;
    assert!(
        found.find(&format!("#{one_old} ")).unwrap() < found.find(&format!("#{two_old} ")).unwrap(),
        "{found}"
    );

    // Word endings are ignored, as in transcript search.
    assert!(s.recalled("betas", &me).await.contains("alpha beta"));

    assert_eq!(s.recalled("zebra", &me).await, memory_recall::NO_MEMORIES);
}

#[tokio::test]
async fn recall_shows_each_memory_as_the_prompt_lists_it_with_its_dates() {
    let s = Setup::new().await;
    let me = Actor::owner();
    // The clock reads 17:00 BST for the add, 17:01 for the edit, then 17:02 for the recall.
    let id = s.remember(&me, "Likes giraffes", true, true).await;
    ok(&s
        .edit(json!({"id": id, "content": "Likes giraffes a lot"}), &me)
        .await);
    let found = s.recalled("giraffe", &me).await;
    let stored = s
        .db()
        .await
        .get(&owner_viewer(), MemoryId::new(id))
        .await
        .unwrap();
    assert!(
        found.starts_with(&format!(
            "1 memory, best first:\n\n{}\n",
            crate::memory::line(&stored)
        )),
        "{found}"
    );
    assert!(
        found.ends_with("created Thu 2026-10-22 17:00 BST, updated Thu 2026-10-22 17:01 BST"),
        "{found}"
    );
}

#[tokio::test]
async fn recall_limits_its_hits_and_matches_query_syntax_as_text() {
    let s = Setup::new().await;
    let me = Actor::owner();
    for content in ["delta one", "delta two", "delta three"] {
        s.remember(&me, content, false, false).await;
    }
    let count = |text: &str| text.split_once(", best first").unwrap().0.to_owned();
    assert_eq!(count(&s.recalled("delta", &me).await), "3 memories");
    let limited = s.recall(json!({"query": "delta", "limit": 2}), &me).await;
    assert_eq!(count(ok(&limited)), "2 memories");
    // Out-of-range limits are clamped into 1..=50 rather than refused.
    let zero = s.recall(json!({"query": "delta", "limit": 0}), &me).await;
    assert_eq!(count(ok(&zero)), "1 memory");
    let huge = s
        .recall(json!({"query": "delta", "limit": 1000}), &me)
        .await;
    assert_eq!(count(ok(&huge)), "3 memories");

    // FTS syntax in the query is text, not syntax: it neither fails nor changes what matches.
    for query in [
        "\"delta",
        "delta*",
        "delta OR (",
        "NEAR(delta two)",
        "delta:one",
        "-delta",
        "delta AND NOT two",
    ] {
        let found = s.recall(json!({"query": query}), &me).await;
        assert!(
            ok(&found).starts_with(|c: char| c.is_ascii_digit()),
            "{query}: {}",
            text(&found)
        );
    }
    // A query without words finds nothing, and one that is blank is invalid.
    let punctuation = s.recall(json!({"query": "\"*():-"}), &me).await;
    assert_eq!(ok(&punctuation), memory_recall::NO_MEMORIES);
    let blank = s.recall(json!({"query": "  "}), &me).await;
    assert_eq!(err(&blank), "invalid input: query is empty");
}

#[test]
fn input_is_parsed_strictly() {
    use memory_add::Add;

    assert_eq!(
        memory_add::parse_input(json!({"content": "a"})),
        Ok(Add {
            content: "a".to_owned(),
            core: false,
            shared: false
        })
    );
    assert_eq!(
        memory_add::parse_input(json!({"content": "a", "core": true, "shared": true})),
        Ok(Add {
            content: "a".to_owned(),
            core: true,
            shared: true
        })
    );
    for (input, expected) in [
        (json!({}), "invalid input: missing field `content`"),
        (json!({"content": "  "}), "invalid input: content is empty"),
        (
            json!({"content": "a", "core": "yes"}),
            "invalid input: invalid type: string \"yes\"",
        ),
        (
            json!({"content": "a", "scope": "agent"}),
            "invalid input: unknown field `scope`",
        ),
    ] {
        let error = memory_add::parse_input(input).unwrap_err();
        assert!(error.starts_with(expected), "{error}");
    }

    assert_eq!(
        memory_edit::parse_input(json!({"id": 3, "content": "x"})),
        Ok((
            MemoryId::new(3),
            MemoryEdit {
                content: Some("x".to_owned()),
                core: None,
                shared: None,
                expected_updated_at: None,
            }
        ))
    );
    assert_eq!(
        memory_edit::parse_input(json!({"id": 3, "core": false, "shared": true})),
        Ok((
            MemoryId::new(3),
            MemoryEdit {
                content: None,
                core: Some(false),
                shared: Some(true),
                expected_updated_at: None,
            }
        ))
    );
    for (input, expected) in [
        (json!({"content": "x"}), "invalid input: missing field `id`"),
        (
            json!({"id": 3}),
            "invalid input: give at least one of content, core, or shared",
        ),
        (
            json!({"id": 3, "content": ""}),
            "invalid input: content is empty; use memory_forget",
        ),
        (
            json!({"id": "3", "content": "x"}),
            "invalid input: invalid type: string \"3\"",
        ),
        (
            json!({"id": 3, "content": "x", "old_text": "y"}),
            "invalid input: unknown field `old_text`",
        ),
    ] {
        let error = memory_edit::parse_input(input).unwrap_err();
        assert!(error.starts_with(expected), "{error}");
    }

    assert_eq!(
        memory_forget::parse_input(json!({"id": 7})),
        Ok(MemoryId::new(7))
    );
    for input in [
        json!({}),
        json!({"id": 1.5}),
        json!({"id": 1, "core": true}),
    ] {
        let error = memory_forget::parse_input(input).unwrap_err();
        assert!(error.starts_with("invalid input: "), "{error}");
    }

    let recall = memory_recall::parse_input;
    assert_eq!(
        recall(json!({"query": "tea"})),
        Ok(("tea".to_owned(), DEFAULT_LIMIT))
    );
    assert_eq!(
        recall(json!({"query": "tea", "limit": 7})),
        Ok(("tea".to_owned(), 7))
    );
    assert_eq!(
        recall(json!({"query": "tea", "limit": 0})),
        Ok(("tea".to_owned(), 1))
    );
    assert_eq!(
        recall(json!({"query": "tea", "limit": 500})),
        Ok(("tea".to_owned(), MAX_LIMIT))
    );
    for input in [
        json!({"query": " "}),
        json!({}),
        json!({"query": "a", "limit": -1}),
        json!({"query": "a", "scope": "all"}),
    ] {
        let error = recall(input).unwrap_err();
        assert!(error.starts_with("invalid input: "), "{error}");
    }
}

#[tokio::test]
async fn invalid_input_and_unknown_agents_are_error_results() {
    let s = Setup::new().await;
    let me = Actor::owner();
    assert!(
        err(&s.add(json!({"core": true}), &me).await)
            .starts_with("invalid input: missing field `content`")
    );
    assert!(
        err(&s.edit(json!({"id": 1}), &me).await).starts_with("invalid input: give at least one")
    );
    assert!(err(&s.forget(json!({}), &me).await).starts_with("invalid input: missing field `id`"));
    assert!(
        err(&s.recall(json!({"query": ""}), &me).await)
            .starts_with("invalid input: query is empty")
    );

    let mut ghost = Actor::owner();
    ghost.session.agent = AgentId::new("ghost").unwrap();
    assert_eq!(
        err(&s.add(json!({"content": "x"}), &ghost).await),
        "agent `ghost` is not configured"
    );
}

#[test]
fn specs_name_the_reserved_tools_with_closed_schemas() {
    let specs = [
        (memory_add::spec(), MEMORY_ADD, json!(["content"])),
        (memory_edit::spec(), MEMORY_EDIT, json!(["id"])),
        (memory_forget::spec(), MEMORY_FORGET, json!(["id"])),
        (memory_recall::spec(), MEMORY_RECALL, json!(["query"])),
    ];
    for (spec, name, required) in specs {
        assert_eq!(spec.name, name);
        assert!(crate::registry::is_core_tool(&spec.name), "{name}");
        let schema = &spec.input_schema;
        assert_eq!(schema["type"], json!("object"), "{name}");
        assert_eq!(schema["required"], required, "{name}");
        assert_eq!(schema["additionalProperties"], json!(false), "{name}");
        for (property, definition) in schema["properties"].as_object().unwrap() {
            let description = definition["description"].as_str().unwrap_or_default();
            assert!(
                !description.is_empty(),
                "{name}.{property} has no description"
            );
            assert!(definition["type"].is_string(), "{name}.{property}");
        }
        assert!(!spec.description.is_empty(), "{name}");
    }
    // The old single `memory` tool is gone.
    assert!(!crate::registry::is_core_tool("memory"));

    let add = memory_add::spec();
    let properties = &add.input_schema["properties"];
    assert_eq!(
        properties["content"]["maxLength"],
        json!(giles_store::MAX_MEMORY_CHARS)
    );
    assert_eq!(properties["core"]["type"], json!("boolean"));
    assert_eq!(properties["shared"]["type"], json!("boolean"));
    for needle in [
        "core",
        "shared",
        "memory_recall",
        "memory_edit",
        "memory_forget",
        "snapshot",
    ] {
        assert!(add.description.contains(needle), "memory_add: {needle}");
    }
    let edit = memory_edit::spec();
    assert_eq!(
        edit.input_schema["properties"]["id"]["type"],
        json!("integer")
    );
    assert!(edit.description.contains("whole old content"));
    let recall = memory_recall::spec();
    let limit = &recall.input_schema["properties"]["limit"];
    assert_eq!(limit["minimum"], json!(1));
    assert_eq!(limit["maximum"], json!(MAX_LIMIT));
    assert_eq!(limit["default"], json!(DEFAULT_LIMIT));
}

#[test]
fn only_recall_is_parallel_safe() {
    use crate::agent_loop::PARALLEL_SAFE_CORE_TOOLS;

    for name in [MEMORY_ADD, MEMORY_EDIT, MEMORY_FORGET, MEMORY_RECALL] {
        assert_eq!(
            PARALLEL_SAFE_CORE_TOOLS.contains(&name),
            name == MEMORY_RECALL,
            "{name}"
        );
    }
}

/// Stores `texts` (alternating user and assistant, starting with the user) in a new direct
/// session of `user` with `agent` on `channel`.
async fn transcript(store: &Store, agent: &str, channel: &str, user: &UserId, texts: &[&str]) {
    let key = SessionKey {
        agent: AgentId::new(agent).unwrap(),
        channel: channel.to_owned(),
        conversation: ConversationId::new(user.as_str()).unwrap(),
    };
    let at: Timestamp = "2026-10-22T16:30:00Z".parse().unwrap();
    let (session, _) = store
        .open_session(key, SessionKind::Direct, Some(user.clone()), at)
        .await
        .unwrap();
    let messages = texts
        .iter()
        .zip([Role::User, Role::Assistant].into_iter().cycle())
        .map(|(text, role)| Message {
            role,
            content: vec![Block::text(*text)],
            meta: MessageMeta::new(at),
        })
        .collect();
    store.append_messages(session.id, messages).await.unwrap();
}

#[tokio::test]
async fn search_covers_the_users_direct_sessions_with_this_agent_on_every_channel() {
    let s = Setup::new().await;
    let owner = owner();
    transcript(
        &s.store,
        AGENT,
        "fake",
        &owner,
        &[
            "[Thu 2026-10-22 17:30 BST] I saw a giraffe today",
            "Lovely.",
        ],
    )
    .await;
    transcript(
        &s.store,
        AGENT,
        "web",
        &owner,
        &["Hello", "Did the giraffe eat?"],
    )
    .await;
    // Not in scope: another user's DM, another agent's, an isolated job run.
    transcript(&s.store, AGENT, "fake", &user("ann"), &["My giraffe"]).await;
    transcript(&s.store, "other", "fake", &owner, &["Other giraffe"]).await;
    let job = s
        .store
        .create_job_session(
            AgentId::new(AGENT).unwrap(),
            Some(owner.clone()),
            Timestamp::UNIX_EPOCH,
        )
        .await
        .unwrap();
    s.store
        .append_messages(
            job.id,
            vec![Message {
                role: Role::Assistant,
                content: vec![Block::text("Job giraffe")],
                meta: MessageMeta::new(Timestamp::UNIX_EPOCH),
            }],
        )
        .await
        .unwrap();

    let output = call(
        &s.search(),
        json!({"query": "giraffes"}),
        Some(owner.clone()),
        dm(),
    )
    .await;
    let found = ok(&output);
    assert!(
        found.starts_with("2 matches, best first (matches in **bold**):\n\n"),
        "{found}"
    );
    let lines: Vec<&str> = found.lines().skip(2).filter(|l| !l.is_empty()).collect();
    let mut sorted = lines.clone();
    sorted.sort_unstable();
    assert_eq!(
        sorted,
        [
            "[Thu 2026-10-22 17:30 BST] user on fake: I saw a **giraffe** today",
            "[Thu 2026-10-22 17:30 BST] you on web: Did the **giraffe** eat?",
        ]
    );

    // A job run delivering to the owner searches the owner's DMs too.
    let from_job = call(
        &s.search(),
        json!({"query": "giraffe", "limit": 1}),
        None,
        session(SessionKind::Job, Some(owner)),
    )
    .await;
    assert!(ok(&from_job).starts_with("1 match, best first"));

    let none = call(&s.search(), json!({"query": "zebra"}), None, dm()).await;
    assert_eq!(ok(&none), NO_MATCHES);

    // A query word that only matches inside the stored time prefix: the prefix is shown clean,
    // once.
    let in_prefix = call(&s.search(), json!({"query": "BST"}), None, dm()).await;
    assert_eq!(
        ok(&in_prefix),
        "1 match, best first (matches in **bold**):\n\n\
         [Thu 2026-10-22 17:30 BST] user on fake: I saw a giraffe today"
    );
}

#[test]
fn a_snippets_time_prefix_is_split_off_without_match_markers() {
    assert_eq!(
        memory_search::split_sent_prefix("[Thu 2026-**10**-22 **10**:15 BST] hello **10** there"),
        Some((
            "[Thu 2026-10-22 10:15 BST]".to_owned(),
            "hello **10** there"
        ))
    );
    assert_eq!(
        memory_search::split_sent_prefix("[Thu 2026-10-22 10:15 BST] hi"),
        Some(("[Thu 2026-10-22 10:15 BST]".to_owned(), "hi"))
    );
    // Cut snippets and other brackets are left alone.
    assert_eq!(
        memory_search::split_sent_prefix("…saw a **giraffe** today"),
        None
    );
    assert_eq!(memory_search::split_sent_prefix("[**note**] giraffe"), None);
}

#[tokio::test]
async fn search_input_and_scope_errors() {
    let s = Setup::new().await;
    for (input, expected) in [
        (json!({"query": "  "}), "query is empty"),
        (
            json!({"query": "a", "limit": 0}),
            "limit must be between 1 and 50",
        ),
        (
            json!({"query": "a", "limit": 51}),
            "limit must be between 1 and 50",
        ),
    ] {
        let output = call(&s.search(), input, Some(owner()), dm()).await;
        assert_eq!(err(&output), expected);
    }
    let extra = call(
        &s.search(),
        json!({"query": "a", "scope": "all"}),
        None,
        dm(),
    )
    .await;
    assert!(err(&extra).starts_with("invalid input: unknown field `scope`"));
    let nobody = call(
        &s.search(),
        json!({"query": "a"}),
        None,
        session(SessionKind::Job, None),
    )
    .await;
    assert_eq!(
        err(&nobody),
        "this conversation has no transcripts to search"
    );

    assert_eq!(
        memory_search::parse_input(json!({"query": "tea"})),
        Ok(("tea".to_owned(), DEFAULT_LIMIT))
    );
    let group = session(SessionKind::Group, None);
    assert_eq!(
        memory_search::scope(&group),
        Ok(giles_store::SearchScope::conversation(
            AgentId::new(AGENT).unwrap(),
            "fake".to_owned(),
            ConversationId::new("chat").unwrap(),
        ))
    );
    assert_eq!(
        memory_search::scope(&dm()),
        Ok(giles_store::SearchScope::direct(
            AgentId::new(AGENT).unwrap(),
            owner()
        ))
    );
}

#[test]
fn search_spec_is_read_only_and_parallel_safe() {
    let spec = memory_search::spec();
    assert_eq!(spec.name, MEMORY_SEARCH);
    assert!(crate::agent_loop::PARALLEL_SAFE_CORE_TOOLS.contains(&MEMORY_SEARCH));
    assert_eq!(spec.input_schema["required"], json!(["query"]));
    assert_eq!(
        spec.input_schema["properties"]["limit"]["maximum"],
        json!(MAX_LIMIT)
    );
    assert_eq!(spec.input_schema["additionalProperties"], json!(false));
}

/// 17:00 in London (BST), the owner's time zone in these tests.
fn schedule_now() -> Timestamp {
    "2026-10-22T16:00:00Z".parse().unwrap()
}

struct FixedClock;

impl crate::sessions::Clock for FixedClock {
    fn now(&self) -> Timestamp {
        schedule_now()
    }
}

#[test]
fn schedule_input_is_parsed_strictly() {
    use crate::config::JobSession;
    use crate::scheduler::RuntimeSpec;

    let london = jiff::tz::TimeZone::get("Europe/London").unwrap();
    let parse = |input: Value| schedule::parse_input(input, schedule_now(), &london);
    let once = |at: &str, prompt: &str| {
        Ok(Request::Create(RuntimeSpec {
            cron: None,
            at: Some(at.parse().unwrap()),
            prompt: prompt.to_owned(),
            session: JobSession::Isolated,
        }))
    };
    assert_eq!(
        parse(json!({"action": "create", "prompt": "Tea.", "in": "20m"})),
        once("2026-10-22T16:20:00Z", "Tea.")
    );
    // Without an offset, in the user's zone (BST); with one, as given.
    assert_eq!(
        parse(json!({"action": "create", "prompt": "Tea.", "at": "2026-10-22T18:30"})),
        once("2026-10-22T17:30:00Z", "Tea.")
    );
    assert_eq!(
        parse(json!({"action": "create", "prompt": "Tea.", "at": "2026-10-22T18:30:00Z"})),
        once("2026-10-22T18:30:00Z", "Tea.")
    );
    assert_eq!(
        parse(
            json!({"action": "create", "prompt": "Notes?", "cron": "0 9 * * 1-5",
                     "session": "dm"})
        ),
        Ok(Request::Create(RuntimeSpec {
            cron: Some("0 9 * * 1-5".into()),
            at: None,
            prompt: "Notes?".into(),
            session: JobSession::Dm,
        }))
    );
    assert_eq!(parse(json!({"action": "list"})), Ok(Request::List));
    assert_eq!(
        parse(json!({"action": "cancel", "id": 3})),
        Ok(Request::Cancel(giles_store::RuntimeJobId::new(3)))
    );

    for (input, error) in [
        (json!({"action": "create", "prompt": "p"}), "exactly one of"),
        (
            json!({"action": "create", "prompt": "p", "in": "1h", "cron": "0 9 * * *"}),
            "exactly one of",
        ),
        (
            json!({"action": "create", "in": "1h"}),
            "`prompt` is required",
        ),
        (
            json!({"action": "create", "prompt": " ", "in": "1h"}),
            "`prompt` is required",
        ),
        (
            json!({"action": "create", "prompt": "p", "at": "2026-10-22T16:59"}),
            "has passed",
        ),
        (
            json!({"action": "create", "prompt": "p", "at": "tomorrow"}),
            "invalid time",
        ),
        (
            json!({"action": "create", "prompt": "p", "in": "soon"}),
            "invalid duration",
        ),
        (
            json!({"action": "create", "prompt": "p", "cron": "0 9 * *"}),
            "invalid cron",
        ),
        (
            json!({"action": "create", "prompt": "p", "cron": "0 0 30 2 *"}),
            "never matches again",
        ),
        (
            json!({"action": "create", "prompt": "p", "in": "1h", "id": 1}),
            "only for `cancel`",
        ),
        (json!({"action": "list", "prompt": "p"}), "takes no"),
        (json!({"action": "list", "id": 1}), "only for `cancel`"),
        (json!({"action": "cancel"}), "`id` is required"),
        (json!({"action": "pause"}), "unknown action"),
        (json!({"action": "list", "extra": 1}), "unknown field"),
        (
            json!({"action": "create", "prompt": "p", "in": "1h", "session": "group"}),
            "unknown variant",
        ),
    ] {
        let got = parse(input.clone()).unwrap_err();
        assert!(got.contains(error), "{input}: {got}");
    }
}

#[tokio::test]
async fn schedule_creates_lists_and_cancels_only_the_requesters_jobs() {
    let s = Setup::new().await;
    let changes = JobChanges::default();
    let mut seen = changes.subscribe();
    let tool = ScheduleTool::new(
        s.store.clone(),
        s.config.clone(),
        changes.clone(),
        Arc::new(FixedClock),
    );
    let jeeves = AgentId::new(AGENT).unwrap();

    let once = call(
        &tool,
        json!({"action": "create", "prompt": "Call the dentist.", "in": "1h"}),
        Some(owner()),
        dm(),
    )
    .await;
    assert_eq!(
        ok(&once),
        "Scheduled job #1: once, Thu 2026-10-22 18:00 BST."
    );
    assert!(seen.has_changed().unwrap(), "the scheduler wasn't told");
    seen.borrow_and_update();
    let jobs = s.store.jobs(None, None).await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].agent, jeeves);
    assert_eq!(jobs[0].creator, owner());
    assert_eq!(
        jobs[0].spec,
        json!({"at": "2026-10-22T17:00:00Z", "prompt": "Call the dentist.", "session": "isolated"})
    );
    let repeating = call(
        &tool,
        json!({"action": "create", "prompt": "Stand-up notes?", "cron": "0 9 * * 1-5",
               "session": "dm"}),
        Some(owner()),
        dm(),
    )
    .await;
    assert_eq!(
        ok(&repeating),
        "Scheduled job #2: every `0 9 * * 1-5`, next Fri 2026-10-23 09:00 BST."
    );
    assert!(seen.has_changed().unwrap());
    seen.borrow_and_update();
    // Someone else's job, and another agent's.
    let theirs = s
        .store
        .job_create(
            jeeves.clone(),
            user("ann"),
            json!({"at": "2026-10-23T09:00:00Z", "prompt": "Theirs."}),
            schedule_now(),
        )
        .await
        .unwrap();
    s.store
        .job_create(
            AgentId::new("other").unwrap(),
            owner(),
            json!({"at": "2026-10-23T09:00:00Z", "prompt": "Elsewhere."}),
            schedule_now(),
        )
        .await
        .unwrap();

    let listed = call(&tool, json!({"action": "list"}), Some(owner()), dm()).await;
    assert_eq!(
        ok(&listed),
        "Jobs scheduled for this user (cancel by number):\n\
         - #1: once, Thu 2026-10-22 18:00 BST (on its own): Call the dentist.\n\
         - #2: every `0 9 * * 1-5`, next Fri 2026-10-23 09:00 BST (in the DM): Stand-up notes?\n\n\
         Configured jobs (read-only):\n\
         - `check-in`: every `0 9 * * *`, next Fri 2026-10-23 09:00 BST (in the DM): Anything to \
         tell me?"
    );

    // Not theirs to cancel; their own, once.
    let other = json!({"action": "cancel", "id": theirs.id.get()});
    let refused = call(&tool, other, Some(owner()), dm()).await;
    assert_eq!(err(&refused), "no job #3 of this user's to cancel");
    assert!(s.store.job(theirs.id).await.unwrap().is_some());
    assert!(!seen.has_changed().unwrap());
    let cancel = json!({"action": "cancel", "id": 1});
    assert_eq!(
        ok(&call(&tool, cancel.clone(), Some(owner()), dm()).await),
        "Cancelled job #1."
    );
    assert!(seen.has_changed().unwrap());
    assert!(err(&call(&tool, cancel, Some(owner()), dm()).await).contains("no job #1"));

    // A config job's turn has no requester: it can list the configured jobs only.
    let job_session = session(SessionKind::Job, Some(owner()));
    let create = json!({"action": "create", "prompt": "p", "in": "1h"});
    assert!(
        err(&call(&tool, create, None, job_session.clone()).await).contains("no requesting user")
    );
    let listed = call(&tool, json!({"action": "list"}), None, job_session).await;
    assert!(ok(&listed).starts_with("Runtime jobs belong to users; this turn has none.\n\n"));
}

#[test]
fn schedule_spec_is_strict() {
    let spec = schedule::spec();
    assert_eq!(spec.name, SCHEDULE);
    assert_eq!(spec.input_schema["additionalProperties"], json!(false));
    assert_eq!(spec.input_schema["required"], json!(["action"]));
    assert_eq!(
        spec.input_schema["properties"]["action"]["enum"],
        json!(["create", "list", "cancel"])
    );
}

#[tokio::test]
async fn schedule_refuses_jobs_from_job_runs_and_past_the_cap() {
    let s = Setup::new().await;
    let tool = ScheduleTool::new(
        s.store.clone(),
        s.config.clone(),
        JobChanges::default(),
        Arc::new(FixedClock),
    );
    let create = json!({"action": "create", "prompt": "Again.", "in": "1h"});

    // A runtime job's run (requester = its creator), in its own session or the DM: no new jobs,
    // but it may list and cancel.
    let refused = call_as(&tool, create.clone(), Some(owner()), dm(), true).await;
    assert!(err(&refused).contains("can't be scheduled from a scheduled job's run"));
    let job_session = session(SessionKind::Job, Some(owner()));
    let refused = call(&tool, create.clone(), Some(owner()), job_session).await;
    assert!(err(&refused).contains("can't be scheduled from a scheduled job's run"));
    assert!(s.store.jobs(None, None).await.unwrap().is_empty());
    ok(&call_as(&tool, json!({"action": "list"}), Some(owner()), dm(), true).await);

    // At most MAX_RUNTIME_JOBS_PER_USER per user and agent.
    let jeeves = AgentId::new(AGENT).unwrap();
    for _ in 1..MAX_RUNTIME_JOBS_PER_USER {
        s.store
            .job_create(
                jeeves.clone(),
                owner(),
                json!({"cron": "0 9 * * *", "prompt": "p"}),
                schedule_now(),
            )
            .await
            .unwrap();
    }
    ok(&call(&tool, create.clone(), Some(owner()), dm()).await);
    let full = call(&tool, create.clone(), Some(owner()), dm()).await;
    assert_eq!(
        err(&full),
        "too many scheduled jobs (at most 20); cancel one first"
    );
    // Another user has their own allowance.
    let ann = session(SessionKind::Direct, Some(user("ann")));
    ok(&call(&tool, create, Some(user("ann")), ann).await);
}

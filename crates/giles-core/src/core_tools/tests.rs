//! The `memory`, `memory_search`, and `schedule` tools through [`CoreTool::call`], against a
//! temporary data root and database. The agent `jeeves` has a 60-character memory cap and one
//! config job.

use std::path::Path;
use std::sync::Arc;

use giles_store::{Session, SessionId, SessionKey, SessionKind, Store};
use giles_types::{
    AgentId, Block, CallContext, ConversationId, Message, MessageMeta, Role, Timestamp, UserId,
};
use serde_json::{Value, json};
use tempfile::TempDir;

use super::memory::{self, Scope, Target, parse_input, target};
use super::*;
use super::{memory_search, schedule};
use crate::config::{Config, LiveConfig, LoadOptions};
use crate::identity::owner;
use crate::paths::DataRoot;
use crate::scheduler::JobChanges;

const AGENT: &str = "jeeves";
const CAP: usize = 60;

struct Setup {
    dir: TempDir,
    root: DataRoot,
    config: LiveConfig,
    store: Store,
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
            dir,
            root,
            config: LiveConfig::new(Arc::new(config)),
            store,
        }
    }

    fn memory(&self) -> MemoryTool {
        MemoryTool::new(self.root.clone(), self.config.clone())
    }

    fn search(&self) -> MemorySearchTool {
        MemorySearchTool::new(self.store.clone(), self.config.clone())
    }

    fn read(&self, relative: &str) -> Option<String> {
        std::fs::read_to_string(self.dir.path().join(relative)).ok()
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.dir.path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
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

fn ops(scope: &str, operations: Value) -> Value {
    json!({"scope": scope, "operations": operations})
}

const AGENT_FILE: &str = "agents/jeeves/MEMORY.md";
const OWNER_FILE: &str = "agents/jeeves/users/owner/MEMORY.md";

#[tokio::test]
async fn each_operation_changes_agent_memory_and_the_result_shows_it() {
    let s = Setup::new().await;
    let tool = s.memory();
    let owner = Some(owner());

    let added = call(
        &tool,
        ops(
            "agent",
            json!([{"op": "add", "content": "Tea at four."}, {"op": "add", "content": "Jazz."}]),
        ),
        owner.clone(),
        dm(),
    )
    .await;
    assert_eq!(
        ok(&added),
        "Saved. Agent memory is now [33% — 20/60]:\n\nTea at four.\n§\nJazz."
    );
    assert_eq!(s.read(AGENT_FILE).unwrap(), "Tea at four.\n§\nJazz.\n");

    let patched = call(
        &tool,
        ops(
            "agent",
            json!([{"op": "patch", "old_text": "four", "content": "five"}]),
        ),
        owner.clone(),
        dm(),
    )
    .await;
    assert_eq!(
        ok(&patched),
        "Saved. Agent memory is now [33% — 20/60]:\n\nTea at five.\n§\nJazz."
    );

    let removed = call(
        &tool,
        ops("agent", json!([{"op": "remove", "old_text": "Tea"}])),
        owner.clone(),
        dm(),
    )
    .await;
    assert_eq!(
        ok(&removed),
        "Saved. Agent memory is now [8% — 5/60]:\n\nJazz."
    );

    let emptied = call(
        &tool,
        ops("agent", json!([{"op": "remove", "old_text": "Jazz"}])),
        owner,
        dm(),
    )
    .await;
    assert_eq!(
        ok(&emptied),
        "Saved. Agent memory is now [0% — 0/60]:\n\n(empty)"
    );
    assert_eq!(s.read(AGENT_FILE).unwrap(), "");
}

#[tokio::test]
async fn a_failing_operation_rejects_the_whole_batch_and_names_itself() {
    let s = Setup::new().await;
    s.write(AGENT_FILE, "Tea at four.\n§\nTea in the garden.\n");
    let output = call(
        &s.memory(),
        ops(
            "agent",
            json!([
                {"op": "add", "content": "Jazz."},
                {"op": "remove", "old_text": "Tea"},
            ]),
        ),
        Some(owner()),
        dm(),
    )
    .await;
    assert_eq!(
        err(&output),
        "operations[1] (remove): old_text matches 2 entries; include more of the text so it \
         matches exactly one"
    );
    assert_eq!(
        s.read(AGENT_FILE).unwrap(),
        "Tea at four.\n§\nTea in the garden.\n"
    );
}

#[tokio::test]
async fn an_over_cap_batch_changes_nothing_and_shows_the_current_entries() {
    let s = Setup::new().await;
    s.write(AGENT_FILE, "Tea at four.\n§\nLikes jazz and long walks.\n");
    let output = call(
        &s.memory(),
        ops(
            "agent",
            json!([{"op": "add", "content": "Keeps bees on the roof of the shed."}]),
        ),
        Some(owner()),
        dm(),
    )
    .await;
    // 12 + 3 + 26 = 41 now; adding 3 + 35 makes 79, 19 over.
    assert_eq!(
        err(&output),
        "the batch would bring memory to 79/60 characters, 19 over the cap; nothing was changed. \
         Make room by merging, shortening, or removing entries, in one batch\n\n\
         Agent memory is unchanged [68% — 41/60]:\n\nTea at four.\n§\nLikes jazz and long walks."
    );
    assert_eq!(
        s.read(AGENT_FILE).unwrap(),
        "Tea at four.\n§\nLikes jazz and long walks.\n"
    );

    // Consolidating in one batch fits.
    let output = call(
        &s.memory(),
        ops(
            "agent",
            json!([
                {"op": "remove", "old_text": "jazz"},
                {"op": "add", "content": "Keeps bees on the roof of the shed."},
            ]),
        ),
        Some(owner()),
        dm(),
    )
    .await;
    assert!(ok(&output).starts_with("Saved. Agent memory is now [83% — 50/60]"));
}

#[tokio::test]
async fn agent_memory_is_written_only_in_owner_turns_and_config_jobs() {
    let s = Setup::new().await;
    let add = || ops("agent", json!([{"op": "add", "content": "Fact."}]));

    let guest = call(&s.memory(), add(), Some(user("ann")), dm()).await;
    assert_eq!(
        err(&guest),
        "not permitted for ann: agent-wide memory is written only in the owner's turns and in \
         config jobs"
    );
    assert_eq!(s.read(AGENT_FILE), None);

    // A config job has no requester, in its own session or the owner's DM.
    let job = call(
        &s.memory(),
        add(),
        None,
        session(SessionKind::Job, Some(owner())),
    )
    .await;
    ok(&job);
    let dm_job = call(&s.memory(), add(), None, dm()).await;
    ok(&dm_job);
    // A runtime job the owner created runs as the owner, as does a group turn of the owner.
    let runtime_job = call(
        &s.memory(),
        add(),
        Some(owner()),
        session(SessionKind::Job, Some(owner())),
    )
    .await;
    ok(&runtime_job);
    let group = call(
        &s.memory(),
        add(),
        Some(owner()),
        session(SessionKind::Group, None),
    )
    .await;
    ok(&group);
    assert_eq!(
        s.read(AGENT_FILE).unwrap(),
        "Fact.\n§\nFact.\n§\nFact.\n§\nFact.\n"
    );
}

#[tokio::test]
async fn user_memory_is_the_session_users_and_never_written_in_a_group() {
    let s = Setup::new().await;
    let add = |content: &str| ops("user", json!([{"op": "add", "content": content}]));

    let output = call(&s.memory(), add("Owns a cat."), Some(owner()), dm()).await;
    assert_eq!(
        ok(&output),
        "Saved. User memory is now [18% — 11/60]:\n\nOwns a cat."
    );
    assert_eq!(s.read(OWNER_FILE).unwrap(), "Owns a cat.\n");
    assert_eq!(s.read(AGENT_FILE), None);

    // A job delivering to the owner writes the owner's file.
    let job = call(
        &s.memory(),
        add("Up at six."),
        None,
        session(SessionKind::Job, Some(owner())),
    )
    .await;
    ok(&job);
    assert_eq!(s.read(OWNER_FILE).unwrap(), "Owns a cat.\n§\nUp at six.\n");

    let group = call(
        &s.memory(),
        add("x"),
        Some(owner()),
        session(SessionKind::Group, None),
    )
    .await;
    assert_eq!(
        err(&group),
        "the `user` memory scope is only available in direct conversations"
    );
    let job_without_user = call(&s.memory(), add("x"), None, session(SessionKind::Job, None)).await;
    err(&job_without_user);

    // Someone else's turn can't write the session user's memory.
    let other = call(&s.memory(), add("x"), Some(user("ann")), dm()).await;
    assert_eq!(
        err(&other),
        "not permitted for ann: this conversation's user memory belongs to owner"
    );
    assert_eq!(s.read(OWNER_FILE).unwrap(), "Owns a cat.\n§\nUp at six.\n");
}

#[tokio::test]
async fn each_scope_has_its_own_cap_from_the_agent() {
    let s = Setup::new().await;
    let long = "x".repeat(CAP);
    let fill = |scope: &str| ops(scope, json!([{"op": "add", "content": long}]));
    ok(&call(&s.memory(), fill("agent"), Some(owner()), dm()).await);
    ok(&call(&s.memory(), fill("user"), Some(owner()), dm()).await);
    let over = call(
        &s.memory(),
        ops("user", json!([{"op": "add", "content": "y"}])),
        Some(owner()),
        dm(),
    )
    .await;
    assert!(err(&over).contains("4 over the cap"), "{}", text(&over));
}

#[test]
fn input_is_checked_before_anything_is_written() {
    let parse = |input: Value| parse_input(input).map(|(scope, ops)| (scope, ops.len()));
    assert_eq!(
        parse(ops("user", json!([{"op": "add", "content": "a"}]))),
        Ok((Scope::User, 1))
    );
    assert_eq!(
        parse(ops("group", json!([{"op": "add", "content": "a"}]))),
        Err("the `group` memory scope isn't available".to_owned())
    );
    assert_eq!(
        parse(ops("team", json!([{"op": "add", "content": "a"}]))),
        Err("unknown memory scope \"team\": use \"agent\" or \"user\"".to_owned())
    );
    assert_eq!(
        parse(ops("agent", json!([]))),
        Err("operations is empty: give at least one operation".to_owned())
    );
    let bad_op = parse(ops(
        "agent",
        json!([{"op": "add", "content": "a"}, {"op": "replace", "content": "b"}]),
    ))
    .unwrap_err();
    assert!(
        bad_op.starts_with("operations[1]: unknown variant `replace`"),
        "{bad_op}"
    );
    // A whole-entry replace dressed as `remove` is refused, not half applied.
    let replace = parse(ops(
        "agent",
        json!([{"op": "remove", "old_text": "a", "content": "b"}]),
    ))
    .unwrap_err();
    assert!(
        replace.starts_with("operations[0]: unknown field `content`"),
        "{replace}"
    );
    let add_extra = parse(ops(
        "agent",
        json!([{"op": "add", "content": "x", "old_text": "y"}]),
    ))
    .unwrap_err();
    assert!(
        add_extra.starts_with("operations[0]: unknown field `old_text`"),
        "{add_extra}"
    );
    let missing = parse(ops("agent", json!([{"op": "patch", "old_text": "a"}]))).unwrap_err();
    assert!(
        missing.starts_with("operations[0]: missing field `content`"),
        "{missing}"
    );
    let extra = parse(json!({"scope": "agent", "operations": [], "read": true})).unwrap_err();
    assert!(
        extra.starts_with("invalid input: unknown field `read`"),
        "{extra}"
    );
    let no_scope = parse(json!({"operations": []})).unwrap_err();
    assert!(
        no_scope.starts_with("invalid input: missing field `scope`"),
        "{no_scope}"
    );
}

#[tokio::test]
async fn invalid_input_is_an_error_result() {
    let s = Setup::new().await;
    let output = call(&s.memory(), json!({"scope": "agent"}), Some(owner()), dm()).await;
    assert!(err(&output).starts_with("invalid input: missing field `operations`"));
}

#[test]
fn scope_rules() {
    let owner_dm = dm();
    assert_eq!(
        target(Scope::Agent, Some(&owner()), &owner_dm),
        Ok(Target::Agent)
    );
    assert_eq!(target(Scope::Agent, None, &owner_dm), Ok(Target::Agent));
    assert!(target(Scope::Agent, Some(&user("ann")), &owner_dm).is_err());
    assert_eq!(
        target(Scope::User, Some(&owner()), &owner_dm),
        Ok(Target::User(owner()))
    );
    let ann_dm = session(SessionKind::Direct, Some(user("ann")));
    assert_eq!(
        target(Scope::User, Some(&user("ann")), &ann_dm),
        Ok(Target::User(user("ann")))
    );
    assert!(target(Scope::User, Some(&owner()), &ann_dm).is_err());
    assert!(
        target(
            Scope::User,
            Some(&owner()),
            &session(SessionKind::Group, None)
        )
        .is_err()
    );
}

#[test]
fn specs_name_the_reserved_tools_with_closed_schemas() {
    let memory = memory::spec();
    assert_eq!(memory.name, MEMORY);
    assert!(crate::registry::is_core_tool(&memory.name));
    let schema = &memory.input_schema;
    assert_eq!(schema["required"], json!(["scope", "operations"]));
    assert_eq!(schema["additionalProperties"], json!(false));
    assert_eq!(
        schema["properties"]["scope"]["enum"],
        json!(["agent", "user"])
    );
    let variants = schema["properties"]["operations"]["items"]["oneOf"]
        .as_array()
        .unwrap();
    let names: Vec<&Value> = variants
        .iter()
        .map(|v| &v["properties"]["op"]["enum"][0])
        .collect();
    assert_eq!(names, [&json!("add"), &json!("patch"), &json!("remove")]);
    assert!(
        variants
            .iter()
            .all(|v| v["additionalProperties"] == json!(false))
    );
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

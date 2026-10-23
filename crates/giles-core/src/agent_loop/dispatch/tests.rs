//! The policy dispatcher against fake toolsets, on tokio's paused clock: `par` is `parallel =
//! true` with the default deadline, `serial` has a 1 s deadline, and `ghost` is configured but
//! never registered.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use giles_store::SessionId;
use giles_types::{AgentId, Error, Result, Skill, SkillSummary, ToolCallHandle, ToolSpec, Toolset};
use serde_json::json;
use tempfile::TempDir;

use super::*;
use crate::approvals::{ApprovalBoard, UNATTENDED_RESULT};
use crate::config::{InstanceName, LoadOptions};
use crate::paths::DataRoot;

/// What the fake toolsets saw: `start <id>`, `end <id>` (answered), `drop <id>` (dropped
/// before answering), in order, and how many calls ran at once.
#[derive(Default)]
struct Probe {
    log: Mutex<Vec<String>>,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
}

impl Probe {
    fn push(&self, entry: String) {
        self.log.lock().unwrap().push(entry);
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    /// Where `entry` is in the log.
    fn at(&self, entry: &str) -> usize {
        self.log()
            .iter()
            .position(|e| e == entry)
            .unwrap_or_else(|| panic!("{entry:?} not in {:?}", self.log()))
    }
}

/// One running call: logs its end (or drop) and leaves the in-flight count.
struct Running {
    probe: Arc<Probe>,
    id: String,
    answered: bool,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.probe.in_flight.fetch_sub(1, Ordering::SeqCst);
        let what = if self.answered { "end" } else { "drop" };
        self.probe.push(format!("{what} {}", self.id));
    }
}

/// Tools `run` (answers after `input.ms` milliseconds, or never without `ms`) and `gone`.
struct FakeToolset {
    probe: Arc<Probe>,
}

#[async_trait]
impl Toolset for FakeToolset {
    async fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        Ok(vec![spec("gone"), spec("run")])
    }

    async fn list_skills(&self) -> Result<Vec<SkillSummary>> {
        Ok(Vec::new())
    }

    async fn get_skill(&self, name: &str) -> Result<Skill> {
        Err(Error::NotFound(name.to_owned()))
    }

    async fn call(&self, call: ToolCall, _ctx: CallContext) -> Result<ToolCallHandle> {
        self.probe.push(format!("start {}", call.id));
        let now = self.probe.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.probe.max_in_flight.fetch_max(now, Ordering::SeqCst);
        let mut running = Running {
            probe: self.probe.clone(),
            id: call.id.clone(),
            answered: false,
        };
        let ms = call.input["ms"].as_u64();
        let events = stream::once(async move {
            match ms {
                Some(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
                None => std::future::pending().await,
            }
            running.answered = true;
            drop(running);
            Ok(ToolCallEvent::Result {
                content: vec![Block::text(format!("{} done", call.id))],
                is_error: false,
            })
        })
        .boxed();
        let (answers, _) = tokio::sync::mpsc::channel(1);
        Ok(ToolCallHandle { events, answers })
    }
}

fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        description: format!("The {name} tool."),
        input_schema: json!({"type": "object"}),
    }
}

fn instance(name: &str) -> InstanceName {
    InstanceName::new(name).unwrap()
}

struct Setup {
    _dir: TempDir,
    registry: Arc<Registry>,
    probe: Arc<Probe>,
}

impl Setup {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("giles.toml"),
            "[owner]\nidentities = [\"fake:owner\"]\n\
             [toolset.par]\nurl = \"https://example.com/mcp\"\nparallel = true\n\
             [toolset.serial]\nurl = \"https://example.com/mcp\"\ndeadline = \"1s\"\n\
             [toolset.ghost]\nurl = \"https://example.com/mcp\"\nparallel = true\n",
        )
        .unwrap();
        let root = DataRoot::new(dir.path(), Path::new("/tmp")).unwrap();
        let config = Config::load_with(
            &root,
            &LoadOptions {
                env: &|_| None,
                giles_exe: None,
                ..LoadOptions::default()
            },
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let registry = Arc::new(Registry::new(root, Arc::new(config)));
        let probe = Arc::new(Probe::default());
        for name in ["par", "serial"] {
            let toolset = Arc::new(FakeToolset {
                probe: probe.clone(),
            });
            registry
                .register_toolset(&instance(name), toolset)
                .await
                .unwrap();
        }
        for name in ["memory", "memory_search"] {
            registry.tools().set_core_tool(spec(name)).unwrap();
        }
        Self {
            _dir: dir,
            registry,
            probe,
        }
    }

    /// The session's tools: the registry's tools now, filtered by `allow`.
    fn tools(&self, allow: &[&str]) -> Arc<SessionTools> {
        Arc::new(SessionTools::from_snapshot(
            &self.registry.tools().snapshot(),
            allow,
        ))
    }

    fn dispatch(
        &self,
        allow: &[&str],
        calls: &[(&str, &str, Value)],
    ) -> BoxStream<'static, (usize, ToolOutput)> {
        self.dispatch_with(allow, calls, CancellationToken::new())
    }

    fn dispatch_with(
        &self,
        allow: &[&str],
        calls: &[(&str, &str, Value)],
        skip: CancellationToken,
    ) -> BoxStream<'static, (usize, ToolOutput)> {
        self.dispatch_unattended(allow, &[], calls, skip)
    }

    /// Dispatches `calls` in a turn nobody attends, with `ask` globs.
    fn dispatch_unattended(
        &self,
        allow: &[&str],
        ask: &[&str],
        calls: &[(&str, &str, Value)],
        skip: CancellationToken,
    ) -> BoxStream<'static, (usize, ToolOutput)> {
        let calls = calls
            .iter()
            .map(|(id, name, input)| ToolRequest {
                id: (*id).to_owned(),
                name: (*name).to_owned(),
                input: input.clone(),
            })
            .collect();
        let approvals = TurnApprovals::new(
            Arc::new(ApprovalBoard::default()),
            SessionId::new(1),
            ask.iter().map(|glob| (*glob).to_owned()).collect(),
            Duration::from_secs(600),
            None,
        );
        PolicyDispatcher::new(self.registry.clone()).dispatch(ToolBatch {
            calls,
            tools: self.tools(allow),
            context: CallContext {
                agent: AgentId::new("jeeves").unwrap(),
                requester: None,
                conversation: None,
            },
            media: Arc::default(),
            skip,
            approvals: Arc::new(approvals),
        })
    }
}

/// The outputs by call index; each call answered exactly once.
async fn outputs(stream: BoxStream<'static, (usize, ToolOutput)>, calls: usize) -> Vec<ToolOutput> {
    let mut outputs: Vec<Option<ToolOutput>> = vec![None; calls];
    let mut stream = stream;
    while let Some((index, output)) = stream.next().await {
        assert!(
            outputs[index].replace(output).is_none(),
            "{index} answered twice"
        );
    }
    outputs
        .into_iter()
        .enumerate()
        .map(|(index, output)| output.unwrap_or_else(|| panic!("{index} not answered")))
        .collect()
}

fn ok(text: &str) -> ToolOutput {
    ToolOutput {
        content: vec![Block::text(text)],
        is_error: false,
    }
}

#[tokio::test(start_paused = true)]
async fn parallel_safe_calls_run_together_up_to_eight_and_others_alone() {
    let s = Setup::new().await;
    let mut calls = vec![("s0", "serial__run", json!({"ms": 5}))];
    let ids: Vec<String> = (1..=12).map(|i| format!("p{i}")).collect();
    // p1 finishes first, so p9 can only start then; p2..=p10 take longer.
    calls.push((ids[0].as_str(), "par__run", json!({"ms": 10})));
    for id in &ids[1..10] {
        calls.push((id.as_str(), "par__run", json!({"ms": 100})));
    }
    calls.extend([
        // A read-only core tool joins the parallel run (it has no implementation yet).
        ("m", "memory_search", json!({})),
        ("s1", "serial__run", json!({"ms": 5})),
        ("s2", "serial__run", json!({"ms": 5})),
        (ids[10].as_str(), "par__run", json!({"ms": 10})),
        (ids[11].as_str(), "par__run", json!({"ms": 10})),
    ]);
    let results = outputs(s.dispatch(&["*"], &calls), calls.len()).await;

    for (output, (id, name, _)) in results.iter().zip(&calls) {
        if *name == "memory_search" {
            assert_eq!(*output, ToolOutput::error(TOOL_UNAVAILABLE));
        } else {
            assert_eq!(*output, ok(&format!("{id} done")));
        }
    }
    let p = &s.probe;
    // Every call starts in model order.
    let starts: Vec<String> = p
        .log()
        .into_iter()
        .filter_map(|e| e.strip_prefix("start ").map(str::to_owned))
        .collect();
    let toolset_ids: Vec<&str> = calls
        .iter()
        .filter(|(_, name, _)| *name != "memory_search")
        .map(|(id, _, _)| *id)
        .collect();
    assert_eq!(starts, toolset_ids);
    // A barrier runs alone: the run after it waits for it.
    assert!(p.at("end s0") < p.at("start p1"));
    // Eight at once: p1..=p8 start before any of them ends; p9 waits for p1.
    assert!(p.at("start p8") < p.at("end p1"));
    assert!(p.at("end p1") < p.at("start p9"));
    assert_eq!(p.max_in_flight.load(Ordering::SeqCst), MAX_PARALLEL_CALLS);
    // A barrier waits for everything before it; consecutive barriers run one by one.
    for id in &ids[..10] {
        assert!(p.at(&format!("end {id}")) < p.at("start s1"), "{id}");
    }
    assert!(p.at("end s1") < p.at("start s2"));
    assert!(p.at("end s2") < p.at("start p11"));
    // The last run is parallel again.
    assert!(p.at("start p12") < p.at("end p11"));
}

#[tokio::test(start_paused = true)]
async fn a_call_past_its_toolsets_deadline_is_dropped_and_times_out() {
    let s = Setup::new().await;
    let started = tokio::time::Instant::now();
    let calls = [
        // `serial` has a 1 s deadline; this one never answers.
        ("hang", "serial__run", json!({})),
        // `par` keeps the 5 min default: 2 s is fine.
        ("slow", "par__run", json!({"ms": 2000})),
    ];
    let results = outputs(s.dispatch(&["*"], &calls), calls.len()).await;

    assert_eq!(
        results,
        [ToolOutput::error("timed out after 1s"), ok("slow done")]
    );
    // The timed-out call was dropped (its toolset saw the cancellation) before the next began.
    assert_eq!(
        s.probe.log(),
        ["start hang", "drop hang", "start slow", "end slow"]
    );
    // 1 s for the deadline, then 2 s for the next call.
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_secs(3) && elapsed < Duration::from_secs(4),
        "{elapsed:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn calls_are_checked_against_the_session_and_registry_as_they_start() {
    let s = Setup::new().await;
    let calls = [
        ("first", "serial__run", json!({"ms": 10})),
        // Dropped by its toolset while `first` runs.
        ("gone", "par__gone", json!({})),
        // Registered, but outside the allowlist.
        ("mem", "memory", json!({})),
        ("nope", "nope__run", json!({})),
        ("last", "par__run", json!({"ms": 10})),
    ];
    let mut stream = s.dispatch(&["par__*", "serial__*", "memory_search"], &calls);
    assert_eq!(stream.next().await, Some((0, ok("first done"))));
    s.registry
        .tools()
        .set_toolset_tools(&instance("par"), vec![spec("run")]);
    let mut rest: Vec<(usize, ToolOutput)> = stream.collect().await;
    rest.sort_by_key(|(index, _)| *index);

    let unavailable = ToolOutput::error(TOOL_UNAVAILABLE);
    assert_eq!(
        rest,
        [
            (1, unavailable.clone()),
            (2, unavailable.clone()),
            (3, unavailable),
            (4, ok("last done")),
        ]
    );
    assert_eq!(
        s.probe.log(),
        ["start first", "end first", "start last", "end last"]
    );
}

#[tokio::test(start_paused = true)]
async fn a_tool_whose_toolset_is_not_registered_is_unavailable() {
    let s = Setup::new().await;
    // Its tools are known (as from the tool-list cache), but no toolset serves them.
    s.registry
        .tools()
        .set_toolset_tools(&instance("ghost"), vec![spec("run")]);
    let calls = [
        ("ghost", "ghost__run", json!({"ms": 1})),
        ("par", "par__run", json!({"ms": 1})),
    ];
    let results = outputs(s.dispatch(&["*"], &calls), calls.len()).await;

    assert_eq!(
        results,
        [ToolOutput::error(TOOL_UNAVAILABLE), ok("par done")]
    );
    // Nothing started for the ghost call.
    assert_eq!(s.probe.log(), ["start par", "end par"]);
}

#[tokio::test(start_paused = true)]
async fn dropping_the_stream_cancels_running_calls_and_starts_no_more() {
    let s = Setup::new().await;
    let calls = [
        ("a", "par__run", json!({})),
        ("b", "par__run", json!({})),
        ("c", "serial__run", json!({"ms": 1})),
    ];
    let mut stream = s.dispatch(&["*"], &calls);
    // Neither parallel call answers within the minute (and `par` allows 5).
    let waited = tokio::time::timeout(Duration::from_secs(60), stream.next()).await;
    assert!(waited.is_err());
    assert_eq!(s.probe.log(), ["start a", "start b"]);
    drop(stream);

    let mut log = s.probe.log();
    log.sort();
    assert_eq!(log, ["drop a", "drop b", "start a", "start b"]);
    assert_eq!(s.probe.in_flight.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn after_the_skip_signal_waiting_calls_are_skipped_and_running_ones_finish() {
    let s = Setup::new().await;
    let calls = [
        ("a", "par__run", json!({"ms": 100})),
        ("b", "par__run", json!({"ms": 300})),
        // Waits for `a` and `b`: skipped while they run.
        ("c", "serial__run", json!({"ms": 1})),
        ("d", "par__run", json!({"ms": 1})),
    ];
    let skip = CancellationToken::new();
    let mut stream = s.dispatch_with(&["*"], &calls, skip.clone());
    assert_eq!(stream.next().await, Some((0, ok("a done"))));
    skip.cancel();
    // The waiting calls are answered at once, before `b` finishes.
    let skipped = ToolOutput::error(SKIPPED_FOR_MESSAGE_RESULT);
    assert_eq!(stream.next().await, Some((2, skipped.clone())));
    assert_eq!(stream.next().await, Some((3, skipped)));
    assert_eq!(s.probe.log(), ["start a", "start b", "end a"]);
    assert_eq!(stream.next().await, Some((1, ok("b done"))));
    assert_eq!(stream.next().await, None);
    assert_eq!(s.probe.log(), ["start a", "start b", "end a", "end b"]);
}

#[tokio::test(start_paused = true)]
async fn a_skip_before_the_first_poll_starts_nothing() {
    let s = Setup::new().await;
    let calls = [
        ("a", "serial__run", json!({"ms": 1})),
        ("b", "par__run", json!({"ms": 1})),
    ];
    let skip = CancellationToken::new();
    skip.cancel();
    let results = outputs(s.dispatch_with(&["*"], &calls, skip), calls.len()).await;

    let skipped = ToolOutput::error(SKIPPED_FOR_MESSAGE_RESULT);
    assert_eq!(results, [skipped.clone(), skipped]);
    assert!(s.probe.log().is_empty());
}

#[tokio::test(start_paused = true)]
async fn in_an_unattended_turn_ask_tools_are_denied_before_they_start() {
    let s = Setup::new().await;
    let calls = [
        ("a", "serial__run", json!({"ms": 1})),
        ("b", "par__run", json!({"ms": 1})),
        // Unavailable tools are reported as such, not as denied.
        ("c", "serial__gone2", json!({})),
    ];
    let stream = s.dispatch_unattended(&["*"], &["serial__*"], &calls, CancellationToken::new());
    let results = outputs(stream, calls.len()).await;

    assert_eq!(
        results,
        [
            ToolOutput::error(UNATTENDED_RESULT),
            ok("b done"),
            ToolOutput::error(TOOL_UNAVAILABLE),
        ]
    );
    assert_eq!(s.probe.log(), ["start b", "end b"]);
}

#[tokio::test]
async fn a_calls_policy_comes_from_its_toolset_or_the_core_tool_list() {
    let s = Setup::new().await;
    let tools = s.tools(&["par__*", "serial__*", "memory*"]);
    let config = s.registry.config();
    let policy = |name: &str| CallPolicy::of(&tools, config, name);
    let five_minutes = Duration::from_secs(300);
    let of = |parallel, deadline| CallPolicy { parallel, deadline };

    assert_eq!(policy("par__run"), of(true, five_minutes));
    assert_eq!(policy("serial__run"), of(false, Duration::from_secs(1)));
    assert_eq!(policy("memory_search"), of(true, five_minutes));
    assert_eq!(policy("memory"), of(false, five_minutes));
    // Not offered: answered at once, so it holds nothing up.
    assert_eq!(policy("other__run"), of(true, five_minutes));
}

#[test]
fn a_timeout_names_the_deadline_in_its_largest_whole_unit() {
    assert_eq!(timed_out(Duration::from_secs(300)), "timed out after 5m");
    assert_eq!(timed_out(Duration::from_secs(90)), "timed out after 90s");
    assert_eq!(timed_out(Duration::from_secs(7200)), "timed out after 2h");
    assert_eq!(
        timed_out(Duration::from_millis(1500)),
        "timed out after 1500ms"
    );
}

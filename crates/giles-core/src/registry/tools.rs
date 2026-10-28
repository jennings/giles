//! The prefixed tool index (PLAN sections 6.5 and 7.9).
//!
//! Toolsets report bare tool names; the index offers each as `<instance>__<tool>`. Core tools
//! (memory, skills, scheduling, attachments) keep bare, reserved names, which never contain `__`
//! and so can't collide with a toolset's. Every name is checked when its list is loaded: a bad
//! name is logged and skipped, and never reaches a model.
//!
//! A session takes a [`ToolSnapshot`] when it starts and keeps it, so a toolset whose list
//! changes later (`tool_changes`, MCP `notifications/tools/list_changed`) only affects new
//! sessions.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use futures::StreamExt;
use futures::stream::BoxStream;
use giles_types::ToolSpec;
use tokio::task::AbortHandle;
use tracing::{debug, warn};

use crate::config::InstanceName;

/// The reserved names of the core tools (PLAN section 6.5), sorted. Only these may be added
/// with [`ToolIndex::set_core_tool`].
pub const CORE_TOOLS: [&str; 8] = [
    "load_skill",
    "memory_add",
    "memory_edit",
    "memory_forget",
    "memory_recall",
    "memory_search",
    "schedule",
    "send_attachment",
];

/// The longest tool name offered to a model, prefix included, in characters. Both v1 provider
/// APIs reject longer names.
pub const MAX_TOOL_NAME_CHARS: usize = 64;

/// What separates the instance prefix from the bare tool name.
pub const PREFIX_SEPARATOR: &str = "__";

/// Whether `name` is one of the [`CORE_TOOLS`].
pub fn is_core_tool(name: &str) -> bool {
    CORE_TOOLS.contains(&name)
}

/// The name a toolset's tool is offered under: `<instance>__<tool>`.
pub fn prefixed_name(instance: &InstanceName, tool: &str) -> String {
    format!("{instance}{PREFIX_SEPARATOR}{tool}")
}

/// Why a tool was left out of the index.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ToolNameError {
    /// The toolset reported a tool with an empty name.
    #[error("a tool has an empty name")]
    Empty,
    /// The name has a character model APIs don't accept.
    #[error("tool name {0:?} may only contain letters, digits, `_`, and `-`")]
    InvalidChars(String),
    /// The prefixed name is longer than [`MAX_TOOL_NAME_CHARS`].
    #[error("tool name {name:?} is {chars} characters long; the limit is {MAX_TOOL_NAME_CHARS}")]
    TooLong {
        /// The prefixed name.
        name: String,
        /// Its length in characters.
        chars: usize,
    },
    /// The toolset listed the same name twice; the first one is kept.
    #[error("tool name {0:?} is listed more than once; only the first is kept")]
    Duplicate(String),
    /// [`ToolIndex::set_core_tool`] was given a name that isn't reserved for a core tool.
    #[error(
        "{name:?} is not a core tool name (core tools: {core})",
        name = .0,
        core = CORE_TOOLS.join(", ")
    )]
    NotCoreTool(String),
}

/// Checks the characters of a bare or full tool name.
fn check_chars(name: &str) -> Result<(), ToolNameError> {
    if name.is_empty() {
        return Err(ToolNameError::Empty);
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(ToolNameError::InvalidChars(name.to_owned()));
    }
    Ok(())
}

/// What a tool name in the index dispatches to.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ToolTarget {
    /// A core tool; the offered name is its bare name.
    Core,
    /// A toolset's tool: call `tool` on the toolset registered as `instance`.
    Toolset {
        /// The toolset instance.
        instance: InstanceName,
        /// The bare name the toolset reported.
        tool: String,
    },
}

/// One tool in a [`ToolSnapshot`].
#[derive(Debug, Clone, PartialEq)]
pub struct IndexedTool {
    /// The tool as offered to the model: `spec.name` is the prefixed (or core) name.
    pub spec: ToolSpec,
    /// Where calls to it go.
    pub target: ToolTarget,
}

/// Every tool offered at one point in time, sorted by name. Sessions hold one for their whole
/// life.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolSnapshot {
    tools: Vec<IndexedTool>,
}

impl ToolSnapshot {
    /// A snapshot of `tools`, sorted by name.
    fn new(mut tools: Vec<IndexedTool>) -> Self {
        tools.sort_by(|a, b| a.spec.name.cmp(&b.spec.name));
        Self { tools }
    }

    /// Every tool, sorted by name.
    pub fn tools(&self) -> &[IndexedTool] {
        &self.tools
    }

    /// Every tool's spec, sorted by name, as a [`giles_types::CompletionRequest`] takes them.
    pub fn specs(&self) -> impl Iterator<Item = &ToolSpec> {
        self.tools.iter().map(|t| &t.spec)
    }

    /// The tool offered as `name` (prefixed, or a core tool's bare name).
    pub fn get(&self, name: &str) -> Option<&IndexedTool> {
        self.tools
            .binary_search_by(|t| t.spec.name.as_str().cmp(name))
            .ok()
            .map(|i| &self.tools[i])
    }

    /// The number of tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether there are no tools.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

/// Prefixes a toolset's bare tool list with `instance`, dropping names that fail the rules.
/// Returns the tools kept, in the toolset's order, and one error per tool dropped.
pub fn prefix_tools(
    instance: &InstanceName,
    tools: Vec<ToolSpec>,
) -> (Vec<IndexedTool>, Vec<ToolNameError>) {
    let mut kept: Vec<IndexedTool> = Vec::with_capacity(tools.len());
    let mut rejected = Vec::new();
    for mut spec in tools {
        if let Err(e) = check_chars(&spec.name) {
            rejected.push(e);
            continue;
        }
        let name = prefixed_name(instance, &spec.name);
        let chars = name.chars().count();
        if chars > MAX_TOOL_NAME_CHARS {
            rejected.push(ToolNameError::TooLong { name, chars });
            continue;
        }
        if kept.iter().any(|t| t.spec.name == name) {
            rejected.push(ToolNameError::Duplicate(spec.name));
            continue;
        }
        let tool = std::mem::replace(&mut spec.name, name);
        kept.push(IndexedTool {
            spec,
            target: ToolTarget::Toolset {
                instance: instance.clone(),
                tool,
            },
        });
    }
    (kept, rejected)
}

/// The live tool index: core tools plus every registered toolset's current list. Cheap
/// [`snapshot`](Self::snapshot)s are rebuilt on every change.
#[derive(Debug, Default)]
pub struct ToolIndex {
    state: Arc<Mutex<State>>,
}

#[derive(Debug, Default)]
struct State {
    core: BTreeMap<String, ToolSpec>,
    toolsets: BTreeMap<InstanceName, Entry>,
    next_generation: u64,
    snapshot: Arc<ToolSnapshot>,
}

/// One toolset's tools. The generation changes each time the instance is registered again, so
/// updates meant for a replaced toolset object are dropped.
#[derive(Debug)]
struct Entry {
    generation: u64,
    tools: Vec<IndexedTool>,
    watcher: Option<AbortHandle>,
}

impl Drop for Entry {
    fn drop(&mut self) {
        if let Some(watcher) = self.watcher.take() {
            watcher.abort();
        }
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

impl State {
    fn rebuild(&mut self) {
        let core = self.core.values().map(|spec| IndexedTool {
            spec: spec.clone(),
            target: ToolTarget::Core,
        });
        let toolsets = self.toolsets.values().flat_map(|e| e.tools.iter().cloned());
        self.snapshot = Arc::new(ToolSnapshot::new(core.chain(toolsets).collect()));
    }

    /// Replaces `instance`'s tools if its generation is still `generation` (`None`: any).
    /// Returns the rejected names, or `None` if the update was stale: the instance was
    /// registered again, or removed, since.
    fn apply(
        &mut self,
        instance: &InstanceName,
        generation: Option<u64>,
        tools: Vec<ToolSpec>,
    ) -> Option<Vec<ToolNameError>> {
        if generation.is_some() && !self.toolsets.contains_key(instance) {
            return None;
        }
        let next = &mut self.next_generation;
        let entry = self.toolsets.entry(instance.clone()).or_insert_with(|| {
            *next += 1;
            Entry {
                generation: *next,
                tools: Vec::new(),
                watcher: None,
            }
        });
        if generation.is_some_and(|g| g != entry.generation) {
            return None;
        }
        let (kept, rejected) = prefix_tools(instance, tools);
        for error in &rejected {
            warn!(toolset = %instance, %error, "tool skipped");
        }
        debug!(toolset = %instance, tools = kept.len(), "tool list loaded");
        entry.tools = kept;
        self.rebuild();
        Some(rejected)
    }
}

impl ToolIndex {
    /// An empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// The tools offered right now. Later changes don't affect a snapshot already taken.
    pub fn snapshot(&self) -> Arc<ToolSnapshot> {
        lock(&self.state).snapshot.clone()
    }

    /// Adds (or replaces) a core tool. Its name must be one of [`CORE_TOOLS`].
    ///
    /// # Errors
    ///
    /// [`ToolNameError::NotCoreTool`] for any other name.
    pub fn set_core_tool(&self, spec: ToolSpec) -> Result<(), ToolNameError> {
        if !is_core_tool(&spec.name) {
            return Err(ToolNameError::NotCoreTool(spec.name));
        }
        let mut state = lock(&self.state);
        state.core.insert(spec.name.clone(), spec);
        state.rebuild();
        Ok(())
    }

    /// Replaces `instance`'s tool list with `tools` (bare names, as the toolset reports them),
    /// e.g. from the tool-list cache. Each rejected tool is logged and skipped; the errors are
    /// returned too.
    pub fn set_toolset_tools(
        &self,
        instance: &InstanceName,
        tools: Vec<ToolSpec>,
    ) -> Vec<ToolNameError> {
        lock(&self.state)
            .apply(instance, None, tools)
            .unwrap_or_default()
    }

    /// Removes `instance`'s tools and stops following its changes.
    pub fn remove_toolset(&self, instance: &InstanceName) {
        let mut state = lock(&self.state);
        if state.toolsets.remove(instance).is_some() {
            state.rebuild();
        }
    }

    /// Starts a new generation for `instance` (a newly registered toolset object): stops
    /// following the previous object's changes and returns the generation to pass to
    /// [`apply`](Self::apply) and [`watch`](Self::watch). Keeps its current tools until the new
    /// object's list arrives.
    pub(crate) fn begin(&self, instance: &InstanceName) -> u64 {
        let mut state = lock(&self.state);
        state.next_generation += 1;
        let generation = state.next_generation;
        let entry = state
            .toolsets
            .entry(instance.clone())
            .or_insert_with(|| Entry {
                generation,
                tools: Vec::new(),
                watcher: None,
            });
        entry.generation = generation;
        if let Some(watcher) = entry.watcher.take() {
            watcher.abort();
        }
        generation
    }

    /// Replaces `instance`'s tools unless the instance was registered again since `generation`.
    pub(crate) fn apply(&self, instance: &InstanceName, generation: u64, tools: Vec<ToolSpec>) {
        lock(&self.state).apply(instance, Some(generation), tools);
    }

    /// Applies every list `changes` yields to `instance` until the stream ends, the instance is
    /// registered again, or the index is dropped. Must be called inside a Tokio runtime.
    pub(crate) fn watch(
        &self,
        instance: &InstanceName,
        generation: u64,
        mut changes: BoxStream<'static, Vec<ToolSpec>>,
    ) {
        let weak: Weak<Mutex<State>> = Arc::downgrade(&self.state);
        let name = instance.clone();
        let task = tokio::spawn(async move {
            while let Some(tools) = changes.next().await {
                let Some(state) = weak.upgrade() else {
                    return;
                };
                if lock(&state).apply(&name, Some(generation), tools).is_none() {
                    return;
                }
            }
        });
        let mut state = lock(&self.state);
        match state.toolsets.get_mut(instance) {
            Some(entry) if entry.generation == generation => {
                entry.watcher = Some(task.abort_handle());
            }
            _ => task.abort(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::channel::mpsc;
    use futures::stream;
    use serde_json::json;

    use super::*;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_owned(),
            description: format!("{name} does things"),
            input_schema: json!({"type": "object"}),
        }
    }

    fn instance(name: &str) -> InstanceName {
        InstanceName::new(name).unwrap()
    }

    fn names(snapshot: &ToolSnapshot) -> Vec<&str> {
        snapshot.specs().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn core_tool_names_are_reserved_bare_and_sorted() {
        let mut sorted = CORE_TOOLS;
        sorted.sort_unstable();
        assert_eq!(sorted, CORE_TOOLS);
        for name in CORE_TOOLS {
            assert!(check_chars(name).is_ok(), "{name}");
            assert!(!name.contains(PREFIX_SEPARATOR), "{name}");
            assert!(is_core_tool(name));
        }
        assert!(!is_core_tool("github__memory_add"));
    }

    #[test]
    fn toolset_tools_are_prefixed_with_the_instance() {
        let (kept, rejected) = prefix_tools(&instance("github"), vec![spec("create_issue")]);
        assert!(rejected.is_empty());
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].spec.name, "github__create_issue");
        assert_eq!(kept[0].spec.description, "create_issue does things");
        assert_eq!(
            kept[0].target,
            ToolTarget::Toolset {
                instance: instance("github"),
                tool: "create_issue".into()
            }
        );
    }

    #[test]
    fn the_64_character_limit_counts_the_prefix() {
        let gh = instance("gh"); // "gh__" is 4 characters.
        let fits = "a".repeat(60);
        let too_long = "a".repeat(61);
        let (kept, rejected) = prefix_tools(&gh, vec![spec(&fits), spec(&too_long)]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].spec.name.len(), MAX_TOOL_NAME_CHARS);
        assert_eq!(
            rejected,
            [ToolNameError::TooLong {
                name: format!("gh__{too_long}"),
                chars: 65
            }]
        );
    }

    #[test]
    fn invalid_empty_and_duplicate_names_are_skipped() {
        let (kept, rejected) = prefix_tools(
            &instance("fs"),
            vec![
                spec("read"),
                spec(""),
                spec("read file"),
                spec("read.file"),
                spec("lire_é"),
                spec("read"),
                spec("write-file__v2"),
            ],
        );
        let kept: Vec<_> = kept.iter().map(|t| t.spec.name.as_str()).collect();
        assert_eq!(kept, ["fs__read", "fs__write-file__v2"]);
        assert_eq!(
            rejected,
            [
                ToolNameError::Empty,
                ToolNameError::InvalidChars("read file".into()),
                ToolNameError::InvalidChars("read.file".into()),
                ToolNameError::InvalidChars("lire_é".into()),
                ToolNameError::Duplicate("read".into()),
            ]
        );
    }

    #[test]
    fn only_reserved_names_can_be_core_tools() {
        let index = ToolIndex::new();
        index.set_core_tool(spec("memory_add")).unwrap();
        assert_eq!(
            index.set_core_tool(spec("shell")),
            Err(ToolNameError::NotCoreTool("shell".into()))
        );
        assert_eq!(
            index.set_core_tool(spec("github__memory_add")),
            Err(ToolNameError::NotCoreTool("github__memory_add".into()))
        );
        let snapshot = index.snapshot();
        assert_eq!(names(&snapshot), ["memory_add"]);
        assert_eq!(snapshot.get("memory_add").unwrap().target, ToolTarget::Core);
    }

    #[test]
    fn snapshots_are_sorted_and_resolve_prefixed_names() {
        let index = ToolIndex::new();
        index.set_toolset_tools(&instance("zed"), vec![spec("b"), spec("a")]);
        index.set_core_tool(spec("schedule")).unwrap();
        index.set_core_tool(spec("load_skill")).unwrap();
        index.set_toolset_tools(&instance("github"), vec![spec("search"), spec("create")]);
        let snapshot = index.snapshot();
        assert_eq!(
            names(&snapshot),
            [
                "github__create",
                "github__search",
                "load_skill",
                "schedule",
                "zed__a",
                "zed__b"
            ]
        );
        assert_eq!(
            snapshot.get("zed__b").unwrap().target,
            ToolTarget::Toolset {
                instance: instance("zed"),
                tool: "b".into()
            }
        );
        assert!(snapshot.get("b").is_none());
        assert!(snapshot.get("github__").is_none());
        assert!(snapshot.get("memory_add").is_none());
    }

    #[test]
    fn a_snapshot_keeps_its_tools_after_changes() {
        let index = ToolIndex::new();
        let fs = instance("fs");
        index.set_toolset_tools(&fs, vec![spec("read")]);
        let before = index.snapshot();
        index.set_toolset_tools(&fs, vec![spec("write")]);
        assert_eq!(names(&before), ["fs__read"]);
        assert_eq!(names(&index.snapshot()), ["fs__write"]);
        index.remove_toolset(&fs);
        assert!(index.snapshot().is_empty());
        assert_eq!(before.len(), 1);
    }

    #[tokio::test]
    async fn watched_changes_update_the_index_until_registered_again() {
        let index = ToolIndex::new();
        let fs = instance("fs");
        let first = index.begin(&fs);
        index.apply(&fs, first, vec![spec("read")]);
        let (tx, rx) = mpsc::unbounded();
        index.watch(&fs, first, rx.boxed());

        tx.unbounded_send(vec![spec("read"), spec("write")])
            .unwrap();
        wait_for(&index, &["fs__read", "fs__write"]).await;

        // A new registration: the old object's updates no longer apply.
        let second = index.begin(&fs);
        assert_eq!(names(&index.snapshot()), ["fs__read", "fs__write"]);
        index.apply(&fs, first, vec![spec("stale")]);
        assert_eq!(names(&index.snapshot()), ["fs__read", "fs__write"]);
        index.apply(&fs, second, vec![spec("list")]);
        assert_eq!(names(&index.snapshot()), ["fs__list"]);
        // The old watcher was aborted, dropping its receiver.
        for _ in 0..500 {
            if tx.is_closed() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(tx.is_closed(), "the old watcher should stop");

        // A watcher for an already-replaced generation is never started.
        index.watch(&fs, first, stream::iter([vec![spec("stale")]]).boxed());
        tokio::task::yield_now().await;
        assert_eq!(names(&index.snapshot()), ["fs__list"]);
    }

    #[tokio::test]
    async fn a_removed_instance_stops_its_watcher_and_ignores_late_updates() {
        let index = ToolIndex::new();
        let fs = instance("fs");
        let generation = index.begin(&fs);
        index.apply(&fs, generation, vec![spec("read")]);
        let (tx, rx) = mpsc::unbounded();
        index.watch(&fs, generation, rx.boxed());
        assert_eq!(names(&index.snapshot()), ["fs__read"]);

        index.remove_toolset(&fs);
        assert!(index.snapshot().is_empty());
        // An update the old object still had in flight neither lists tools nor brings the
        // instance back.
        index.apply(&fs, generation, vec![spec("late")]);
        assert!(index.snapshot().is_empty());
        for _ in 0..500 {
            if tx.is_closed() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(tx.is_closed(), "the watcher should stop");
    }

    async fn wait_for(index: &ToolIndex, expected: &[&str]) {
        for _ in 0..500 {
            if names(&index.snapshot()) == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "tool list is {:?}, expected {expected:?}",
            names(&index.snapshot())
        );
    }
}

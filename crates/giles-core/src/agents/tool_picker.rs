//! The tool picker of the agent forms: a checkbox for each core tool and one for each configured
//! toolset, in place of typing the `tools` list.
//!
//! `agent.toml` keeps `tools` as a list of names and globs. A checkbox stands for one entry:
//! a core tool's name (`memory_add`), or `<instance>__*` for a toolset as a whole. Any other entry
//! (`github__create_*`, `memory*`, a toolset that is no longer configured) has no checkbox. It
//! stays in the form's "Other tools" text, so saving never drops it.
//!
//! A checkbox that an entry in "Other tools" already covers is [`ToolState::Via`]: shown ticked
//! and disabled, since unticking it would change nothing. [`compose`] leaves such a tool out of
//! the checkbox part of the list, so the entry that allows it is the only one.

use std::collections::BTreeSet;

use crate::agent_loop::glob;
use crate::config::Config;
use crate::registry::prefixed_name;

/// What each core tool does, for the form. Sorted like
/// [`CORE_TOOLS`](crate::registry::CORE_TOOLS), which a test checks.
const CORE_TOOL_NOTES: [(&str, &str); 8] = [
    ("load_skill", "Load a skill's instructions."),
    ("memory_add", "Remember something new."),
    ("memory_edit", "Change a memory."),
    ("memory_forget", "Delete a memory."),
    ("memory_recall", "Search the memories it can see."),
    ("memory_search", "Search the text of earlier conversations."),
    ("schedule", "Schedule prompts for itself to run later."),
    ("send_attachment", "Send a file to the conversation."),
];

/// What a toolset's checkbox says about it.
const TOOLSET_NOTE: &str = "Every tool of this toolset.";

/// Whether a tool is allowed, as its checkbox shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolState {
    /// Not allowed.
    Off,
    /// Allowed by its own checkbox.
    On,
    /// Allowed by this entry of "Other tools", which the checkbox can't undo.
    Via(String),
}

/// One checkbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolChoice {
    /// The `tools` entry the checkbox stands for: `memory_add`, or `github__*`.
    pub value: String,
    /// What the tool, or the toolset's tools, do.
    pub note: &'static str,
    /// Whether it is allowed.
    pub state: ToolState,
}

impl ToolChoice {
    /// Whether the checkbox is ticked: the tool is allowed, by this box or another entry.
    pub fn is_on(&self) -> bool {
        self.state != ToolState::Off
    }

    /// The entry of "Other tools" that allows the tool, if it is the reason it is ticked.
    pub fn via(&self) -> Option<&str> {
        match &self.state {
            ToolState::Via(glob) => Some(glob),
            _ => None,
        }
    }
}

/// The checkboxes of a form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPicker {
    /// The core tools, sorted by name.
    pub core: Vec<ToolChoice>,
    /// The configured toolsets, sorted by instance name.
    pub toolsets: Vec<ToolChoice>,
}

/// The entries of the core tools' checkboxes, with their notes.
fn core_entries() -> impl Iterator<Item = (String, &'static str)> {
    CORE_TOOL_NOTES
        .iter()
        .map(|(name, note)| ((*name).to_owned(), *note))
}

/// The entries of the toolsets' checkboxes: one `<instance>__*` for each configured toolset.
fn toolset_entries(config: &Config) -> impl Iterator<Item = (String, &'static str)> {
    config
        .toolsets
        .keys()
        .map(|instance| (prefixed_name(instance, "*"), TOOLSET_NOTE))
}

/// Every entry that has a checkbox, in the picker's order.
fn entries(config: &Config) -> impl Iterator<Item = (String, &'static str)> {
    core_entries().chain(toolset_entries(config))
}

impl ToolPicker {
    /// The checkboxes for `picked` (the ticked entries) and `other` (the "Other tools" entries).
    /// `picked` holds entries as [`ToolChoice::value`] has them; any that has no checkbox is
    /// ignored.
    pub fn new(config: &Config, picked: &BTreeSet<String>, other: &[String]) -> Self {
        let choose = |(value, note): (String, &'static str)| {
            let state = match other.iter().find(|entry| glob::matches(entry, &value)) {
                Some(entry) => ToolState::Via(entry.clone()),
                None if picked.contains(&value) => ToolState::On,
                None => ToolState::Off,
            };
            ToolChoice { value, note, state }
        };
        Self {
            core: core_entries().map(choose).collect(),
            toolsets: toolset_entries(config).map(choose).collect(),
        }
    }
}

/// Splits an agent's `tools` into the entries that have a checkbox and the rest, in the order
/// they were written.
pub fn split(config: &Config, tools: &[String]) -> (BTreeSet<String>, Vec<String>) {
    let known: BTreeSet<String> = entries(config).map(|(value, _)| value).collect();
    let mut picked = BTreeSet::new();
    let mut other: Vec<String> = Vec::new();
    for tool in tools {
        if known.contains(tool.as_str()) {
            picked.insert(tool.clone());
        } else if !other.contains(tool) {
            other.push(tool.clone());
        }
    }
    (picked, other)
}

/// The `tools` list for the ticked checkboxes and the "Other tools" entries: the checkboxes in
/// the picker's order (a tool that an "Other tools" entry covers already is left out), then the
/// other entries as written, without repeats.
pub fn compose(config: &Config, picked: &BTreeSet<String>, other: &[String]) -> Vec<String> {
    let mut tools: Vec<String> = entries(config)
        .map(|(value, _)| value)
        .filter(|value| {
            picked.contains(value) && !other.iter().any(|entry| glob::matches(entry, value))
        })
        .collect();
    for entry in other {
        if !tools.contains(entry) {
            tools.push(entry.clone());
        }
    }
    tools
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LoadOptions;
    use crate::paths::DataRoot;
    use crate::registry::CORE_TOOLS;
    use std::path::Path;

    /// A configuration with these toolsets and nothing else.
    fn config(toolsets: &[&str]) -> Config {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path(), Path::new("/tmp")).unwrap();
        root.create_layout().unwrap();
        let mut text = String::from("[owner]\nidentities = [\"web:owner\"]\n[channel.web]\n");
        for name in toolsets {
            text.push_str(&format!("[toolset.{name}]\ncommand = \"/x\"\n"));
        }
        std::fs::write(root.config_file(), text).unwrap();
        Config::load_with(
            &root,
            &LoadOptions {
                env: &|_| None,
                giles_exe: None,
                ..LoadOptions::default()
            },
        )
        .unwrap_or_else(|e| panic!("{e}"))
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn every_core_tool_has_one_note_in_order() {
        let names: Vec<&str> = CORE_TOOL_NOTES.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, CORE_TOOLS);
    }

    #[test]
    fn split_keeps_what_has_no_checkbox() {
        let config = config(&["github"]);
        let tools = strings(&[
            "memory_add",
            "github__create_*",
            "github__*",
            "memory*",
            "gone__*",
            "memory*",
        ]);
        let (picked, other) = split(&config, &tools);
        assert_eq!(picked, set(&["memory_add", "github__*"]));
        assert_eq!(other, ["github__create_*", "memory*", "gone__*"]);
    }

    #[test]
    fn composing_what_was_split_gives_the_tools_back() {
        let config = config(&["github", "files"]);
        let tools = strings(&["memory_add", "files__*", "github__create_*", "gone__*"]);
        let (picked, other) = split(&config, &tools);
        assert_eq!(
            compose(&config, &picked, &other),
            ["memory_add", "files__*", "github__create_*", "gone__*"]
        );
    }

    #[test]
    fn an_entry_that_covers_a_checkbox_wins() {
        let config = config(&["github"]);
        let other = strings(&["memory*", "*"]);
        let picker = ToolPicker::new(&config, &set(&["memory_add", "schedule"]), &other[..1]);
        let state = |name: &str| {
            picker
                .core
                .iter()
                .find(|choice| choice.value == name)
                .unwrap()
                .state
                .clone()
        };
        assert_eq!(state("memory_add"), ToolState::Via("memory*".into()));
        assert_eq!(state("memory_search"), ToolState::Via("memory*".into()));
        assert_eq!(state("schedule"), ToolState::On);
        assert_eq!(state("load_skill"), ToolState::Off);
        assert_eq!(picker.toolsets[0].state, ToolState::Off);

        // `*` covers a toolset as a whole, `github__create_*` doesn't.
        let all = ToolPicker::new(&config, &set(&[]), &other[1..]);
        assert_eq!(all.toolsets[0].state, ToolState::Via("*".into()));
        let some = ToolPicker::new(&config, &set(&[]), &strings(&["github__create_*"]));
        assert_eq!(some.toolsets[0].state, ToolState::Off);

        assert_eq!(
            compose(&config, &set(&["memory_add", "schedule"]), &other[..1]),
            ["schedule", "memory*"]
        );
    }

    #[test]
    fn compose_ignores_entries_with_no_checkbox_and_repeats() {
        let config = config(&[]);
        let picked = set(&["memory_edit", "evil__*", "load_skill"]);
        assert_eq!(
            compose(&config, &picked, &strings(&["a", "a", "b"])),
            ["load_skill", "memory_edit", "a", "b"]
        );
    }
}

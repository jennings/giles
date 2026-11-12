//! The environment picker of the agent forms: a checkbox for each configured
//! `[environment.<instance>]`, in place of typing the `environments` list.
//!
//! A checkbox stands for one instance name. An instance named in the agent's `environments` that
//! isn't configured any more still gets a checkbox, ticked and marked, so the form shows what
//! `agent.toml` says (as the Models field does) and saving never drops it silently: saving with it
//! ticked is refused like a hand-written `agent.toml` naming it, and unticking it removes it.
//!
//! The field is shown only if there is something to show: a configured environment, or a name
//! already ticked.
//!
//! An agent is offered the environment tools
//! ([`ENVIRONMENT_TOOLS`](crate::registry::ENVIRONMENT_TOOLS)) only with an environment, so the
//! form also tells the owner when the two disagree ([`EnvironmentNotes`]). The notes inform; the
//! form still saves.

use std::collections::BTreeSet;

use super::tool_picker::ToolPicker;
use crate::config::Config;

/// One checkbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentChoice {
    /// The instance name: the entry in `environments`.
    pub name: String,
    /// What the environment is for, as `giles.toml` describes it; empty if it doesn't.
    pub description: String,
    /// Whether `[environment.<name>]` is configured.
    pub configured: bool,
    /// Whether the box is ticked.
    pub checked: bool,
}

/// The checkboxes of a form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentPicker {
    /// The configured instances by name, then the ticked names that aren't configured, sorted.
    pub choices: Vec<EnvironmentChoice>,
}

/// Which of the environment notes a form shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvironmentNotes {
    /// An environment is ticked, but no environment tool is allowed, so the agent can't use it.
    pub no_tools: bool,
    /// An environment tool is allowed, but no environment is ticked, so it isn't offered.
    pub no_environment: bool,
}

impl EnvironmentPicker {
    /// The checkboxes for `picked` (the ticked names), over the environments `config` has.
    pub fn new(config: &Config, picked: &BTreeSet<String>) -> Self {
        let configured = config.environments.values().map(|instance| {
            let name = instance.name.as_str().to_owned();
            EnvironmentChoice {
                checked: picked.contains(&name),
                description: instance.description.clone(),
                name,
                configured: true,
            }
        });
        let gone = picked
            .iter()
            .filter(|name| !config.environments.contains_key(name.as_str()))
            .map(|name| EnvironmentChoice {
                name: name.clone(),
                description: String::new(),
                configured: false,
                checked: true,
            });
        Self {
            choices: configured.chain(gone).collect(),
        }
    }

    /// Whether the form has no field for environments: none is configured, and none is ticked.
    pub fn is_empty(&self) -> bool {
        self.choices.is_empty()
    }

    /// Whether any box is ticked.
    pub fn any_ticked(&self) -> bool {
        self.choices.iter().any(|choice| choice.checked)
    }

    /// The notes for these boxes and the tool `tools` shows. None while the field is hidden: with
    /// no environment configured, the environment tools' own notes say what they need.
    pub fn notes(&self, tools: &ToolPicker) -> EnvironmentNotes {
        let ticked = self.any_ticked();
        let allowed = tools
            .core
            .iter()
            .any(|tool| tool.needs_environment() && tool.is_on());
        EnvironmentNotes {
            no_tools: ticked && !allowed,
            no_environment: !self.is_empty() && !ticked && allowed,
        }
    }
}

/// The `environments` list for the ticked names: sorted, which is the order of the configured
/// instances. A name that is configured and one that isn't are alike here: validation tells them
/// apart.
pub fn compose(picked: &BTreeSet<String>) -> Vec<String> {
    picked.iter().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LoadOptions;
    use crate::paths::DataRoot;
    use std::path::Path;

    /// A configuration with environments `box` (described) and `far`, and nothing else.
    fn config(with_environments: bool) -> Config {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path(), Path::new("/tmp")).unwrap();
        root.create_layout().unwrap();
        let mut text = String::from("[owner]\nidentities = [\"web:owner\"]\n[channel.web]\n");
        if with_environments {
            text.push_str(
                "[environment.far]\ncomponent = \"docker\"\n\
                 [environment.box]\ncomponent = \"docker\"\ndescription = \"A scratch box\"\n",
            );
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

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    fn choice(name: &str, description: &str, configured: bool, checked: bool) -> EnvironmentChoice {
        EnvironmentChoice {
            name: name.into(),
            description: description.into(),
            configured,
            checked,
        }
    }

    #[test]
    fn one_box_for_each_configured_environment_by_name() {
        let picker = EnvironmentPicker::new(&config(true), &set(&["far"]));
        assert_eq!(
            picker.choices,
            [
                choice("box", "A scratch box", true, false),
                choice("far", "", true, true),
            ]
        );
        assert!(!picker.is_empty());
        assert!(picker.any_ticked());
    }

    #[test]
    fn the_field_is_hidden_without_environments_but_a_name_that_is_not_configured_shows() {
        let none = config(false);
        assert!(EnvironmentPicker::new(&none, &set(&[])).is_empty());
        let gone = EnvironmentPicker::new(&none, &set(&["gone"]));
        assert_eq!(gone.choices, [choice("gone", "", false, true)]);
        let both = EnvironmentPicker::new(&config(true), &set(&["gone", "box"]));
        assert_eq!(
            both.choices,
            [
                choice("box", "A scratch box", true, true),
                choice("far", "", true, false),
                choice("gone", "", false, true),
            ]
        );
    }

    #[test]
    fn compose_gives_the_ticked_names_sorted() {
        assert_eq!(compose(&set(&["far", "box"])), ["box", "far"]);
        assert!(compose(&set(&[])).is_empty());
    }

    #[test]
    fn the_notes_tell_when_the_environment_and_its_tools_disagree() {
        let config = config(true);
        let notes = |envs: &[&str], tools: &[&str], other: &[&str]| {
            let other: Vec<String> = other.iter().map(|s| (*s).to_owned()).collect();
            EnvironmentPicker::new(&config, &set(envs)).notes(&ToolPicker::new(
                &config,
                &set(tools),
                &other,
            ))
        };
        let none = EnvironmentNotes {
            no_tools: false,
            no_environment: false,
        };
        // An environment, and none of its tools.
        assert_eq!(
            notes(&["box"], &["memory_add"], &[]),
            EnvironmentNotes {
                no_tools: true,
                ..none
            }
        );
        // Environment tools, and no environment.
        assert_eq!(
            notes(&[], &["bash", "memory_add"], &[]),
            EnvironmentNotes {
                no_environment: true,
                ..none
            }
        );
        // Both, or neither: nothing to say.
        assert_eq!(notes(&["box"], &["read"], &[]), none);
        assert_eq!(notes(&[], &["memory_add"], &[]), none);
        // A glob in Other tools allows them too.
        assert_eq!(notes(&["box"], &[], &["*"]), none);
        assert!(notes(&["box"], &[], &["memory*"]).no_tools);
        // No environment is configured at all: the tools' notes say so, the form doesn't nag.
        let bare = self::config(false);
        let picker = EnvironmentPicker::new(&bare, &set(&[]));
        assert_eq!(
            picker.notes(&ToolPicker::new(&bare, &set(&["bash"]), &[])),
            none
        );
    }
}

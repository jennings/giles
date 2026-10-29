//! Text edits for `giles instance rename`: rename one instance in `giles.toml` and in the agents'
//! `agent.toml` files while keeping everything else as the operator wrote it (comments, blank
//! lines, key order, quoting). `toml_edit` does the editing; nothing here touches the file system.

use std::cell::RefCell;
use std::collections::BTreeMap;

use toml_edit::{DocumentMut, Item, Key, Table, TableLike, Value};

use crate::agent_loop::glob;
use crate::config::Kind;

/// A file can't be edited.
#[derive(Debug, thiserror::Error)]
pub(crate) enum EditError {
    /// Not valid TOML.
    #[error("not valid TOML: {0}")]
    Syntax(String),
    /// The instance is written in a form this command doesn't edit.
    #[error("{0}")]
    Unsupported(String),
}

/// Parses a TOML document for editing.
pub(super) fn parse(source: &str) -> Result<DocumentMut, EditError> {
    source
        .parse()
        .map_err(|e: toml_edit::TomlError| EditError::Syntax(e.to_string()))
}

/// The instances `giles.toml` configures: every key of `[provider]`, `[channel]`, and
/// `[toolset]`, with its kind, except a `[toolset.<name>]` with `url`, which the configuration
/// loader ignores (remote toolsets are stored in the database). Names are unique across kinds in
/// a valid configuration; if one isn't, the later kind in `provider`, `channel`, `toolset` order
/// wins.
pub(super) fn configured(doc: &DocumentMut) -> BTreeMap<String, Kind> {
    let mut names = BTreeMap::new();
    for kind in Kind::ALL {
        if let Some(table) = doc.get(kind.as_str()).and_then(Item::as_table_like) {
            for (name, item) in table.iter() {
                let remote = kind == Kind::Toolset
                    && item.as_table_like().is_some_and(|t| t.contains_key("url"));
                if !remote {
                    names.insert(name.to_owned(), kind);
                }
            }
        }
    }
    names
}

/// What [`rename_in_giles_toml`] changed.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct GilesEdit {
    /// The edited document.
    pub text: String,
    /// Whether `[<kind>.<old>]` was renamed (it isn't when only `new` is configured, because the
    /// operator already renamed the table by hand).
    pub table_renamed: bool,
    /// `[[routes]]` entries whose `channel` was `old`.
    pub routes: usize,
}

/// Renames `[<kind>.<old>]` to `[<kind>.<new>]` (with its sub-tables, comments, and position),
/// and for a channel every `[[routes]]` `channel` that names `old`. If the table is already
/// `new` (the operator renamed it by hand), changes nothing but `routes`. A `[<kind>.<new>]` that
/// is there too (a `[toolset.<new>]` with `url`, which loading ignores) is refused rather than
/// overwritten.
pub(super) fn rename_in_giles_toml(
    source: &str,
    kind: Kind,
    old: &str,
    new: &str,
) -> Result<GilesEdit, EditError> {
    let mut doc = parse(source)?;
    let mut table_renamed = false;
    let old_component = component_of(&doc, kind, old);
    if let Some(parent) = doc.get_mut(kind.as_str())
        && parent.as_table_like().is_some_and(|t| t.contains_key(old))
    {
        if parent.as_table_like().is_some_and(|t| t.contains_key(new)) {
            return Err(EditError::Unsupported(format!(
                "giles.toml already has a [{kind}.{new}] table: remove it, then run the command \
                 again"
            )));
        }
        // Without a `component` key a provider or channel runs the component named like the
        // instance, so the new name would run another one (or none). Keep the old one. A toolset
        // runs no component.
        let pin = kind != Kind::Toolset
            && parent
                .as_table_like()
                .and_then(|t| t.get(old))
                .and_then(Item::as_table_like)
                .is_some_and(|t| !t.contains_key("component"));
        match parent {
            Item::Table(table) => rekey_table(table, old, new),
            Item::Value(Value::InlineTable(table)) => {
                let names: Vec<String> = table.iter().map(|(k, _)| k.to_owned()).collect();
                let entries: Vec<_> = names
                    .iter()
                    .filter_map(|name| table.remove_entry(name))
                    .collect();
                for (key, value) in entries {
                    let key = if key.get() == old {
                        rekey(&key, new)
                    } else {
                        key
                    };
                    table.insert_formatted(&key, value);
                }
            }
            _ => {
                return Err(EditError::Unsupported(format!(
                    "`{}` is not a table",
                    kind.as_str()
                )));
            }
        }
        if pin
            && let Some(table) = parent
                .as_table_like_mut()
                .and_then(|t| t.get_mut(new))
                .and_then(Item::as_table_like_mut)
        {
            table.insert("component", toml_edit::value(old));
        }
        table_renamed = true;
        let new_component = component_of(&doc, kind, new);
        if new_component != old_component {
            return Err(EditError::Unsupported(format!(
                "[{kind}.{old}] runs the component {old_component:?}, but [{kind}.{new}] would \
                 run {new_component:?}: it is written in a form this command can't pin"
            )));
        }
    }
    let mut routes = 0;
    if kind == Kind::Channel
        && let Some(item) = doc.get_mut("routes")
    {
        for table in table_likes_mut(item) {
            if let Some(channel) = table.get_mut("channel") {
                routes += usize::from(map_string(channel, &|s| (s == old).then(|| new.to_owned())));
            }
        }
    }
    Ok(GilesEdit {
        text: doc.to_string(),
        table_renamed,
        routes,
    })
}

/// The component instance `name` of `kind` runs: its `component` key, else its own name. `None`
/// for a toolset, which runs no component, and if there is no such instance.
fn component_of(doc: &DocumentMut, kind: Kind, name: &str) -> Option<String> {
    if kind == Kind::Toolset {
        return None;
    }
    let table = doc
        .get(kind.as_str())
        .and_then(Item::as_table_like)
        .and_then(|t| t.get(name))
        .and_then(Item::as_table_like)?;
    Some(
        table
            .get("component")
            .and_then(Item::as_str)
            .map_or_else(|| name.to_owned(), str::to_owned),
    )
}

/// Renames the references to the instance `old` in the text of an `agent.toml`: for a provider,
/// the `<old>/<model id>` models (`models`, `compaction_model`, each job's `model`); for a
/// toolset, the tool globs that start with `<old>__` (`tools`, `ask`), since a toolset's name is
/// its tools' prefix ([`rewrite_toolset_globs`]). Channels aren't referenced from agents. Returns
/// the new text and the number of changes, or `None` if there was nothing to change.
pub(super) fn rename_in_agent_toml(
    source: &str,
    kind: Kind,
    old: &str,
    new: &str,
) -> Result<Option<(String, usize)>, EditError> {
    if kind == Kind::Channel {
        return Ok(None);
    }
    let mut doc = parse(source)?;
    let mut count = 0;
    match kind {
        Kind::Provider => {
            let (from, to) = (format!("{old}/"), format!("{new}/"));
            let swap = |s: &str| s.strip_prefix(&from).map(|rest| format!("{to}{rest}"));
            for key in ["models", "compaction_model"] {
                if let Some(item) = doc.get_mut(key) {
                    count += map_strings(item, &swap);
                }
            }
            if let Some(jobs) = doc.get_mut("jobs") {
                for job in table_likes_mut(jobs) {
                    if let Some(model) = job.get_mut("model") {
                        count += map_strings(model, &swap);
                    }
                }
            }
        }
        _ => {
            return match rewrite_toolset_globs(source, old, new)? {
                ToolsetGlobs::Unchanged => Ok(None),
                ToolsetGlobs::Ambiguous(globs) => Err(EditError::Unsupported(format!(
                    "these globs match `{old}__` tools without starting with `{old}__`, so they \
                     would stop matching after the rename to {new:?}: {}; rewrite them by hand \
                     (for example to `{old}__*`), then run the command again",
                    globs.join(", ")
                ))),
                ToolsetGlobs::Rewritten { text, changes } => Ok(Some((text, changes.len()))),
            };
        }
    }
    Ok((count > 0).then(|| (doc.to_string(), count)))
}

/// One tool glob of an `agent.toml` that a toolset rename rewrites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobChange {
    /// The list the glob is in: `tools` or `ask`.
    pub key: &'static str,
    /// The glob as written, which starts with `<old>__`.
    pub from: String,
    /// The glob after the rename, which starts with `<new>__`.
    pub to: String,
}

/// What renaming a toolset does to the text of an `agent.toml`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ToolsetGlobs {
    /// No glob names the toolset.
    Unchanged,
    /// These globs match `<old>__` tools without starting with `<old>__` (a glob like `github*`):
    /// nothing can rewrite them safely, and left alone they would stop matching (an `ask` entry
    /// then stops asking), so the operator decides what each should say. Each is described as
    /// `tools entry "github*"`.
    Ambiguous(Vec<String>),
    /// The file with every glob that starts with `<old>__` rewritten, and what changed, in file
    /// order (`tools` first).
    Rewritten {
        /// The new text.
        text: String,
        /// The globs that changed.
        changes: Vec<GlobChange>,
    },
}

/// Rewrites the `<old>__` globs of the `tools` and `ask` lists of an `agent.toml` to `<new>__`,
/// as a toolset rename needs, and keeps everything else as written (comments, key order). A file
/// with CRLF line ends is edited as LF and written back with CRLF, as the agent edit form does.
///
/// # Errors
///
/// [`EditError::Syntax`] if `source` isn't valid TOML.
pub(crate) fn rewrite_toolset_globs(
    source: &str,
    old: &str,
    new: &str,
) -> Result<ToolsetGlobs, EditError> {
    let crlf = source.contains("\r\n");
    let lf;
    let text = if crlf {
        lf = source.replace("\r\n", "\n");
        lf.as_str()
    } else {
        source
    };
    let mut doc = parse(text)?;
    let (from, to) = (format!("{old}__"), format!("{new}__"));
    // A glob like `github*` matches the toolset's tools by the old name without saying
    // `github__`: nothing can rewrite it safely, and leaving it would stop it matching.
    let mut ambiguous = Vec::new();
    for key in ["tools", "ask"] {
        if let Some(item) = doc.get(key) {
            for glob in strings_of(item) {
                if !glob.starts_with(&from) && depends_on_name(&glob, old, new) {
                    ambiguous.push(format!("{key} entry {glob:?}"));
                }
            }
        }
    }
    if !ambiguous.is_empty() {
        return Ok(ToolsetGlobs::Ambiguous(ambiguous));
    }
    let changes = RefCell::new(Vec::new());
    for key in ["tools", "ask"] {
        let swap = |glob: &str| {
            glob.strip_prefix(&from).map(|rest| {
                let renamed = format!("{to}{rest}");
                changes.borrow_mut().push(GlobChange {
                    key,
                    from: glob.to_owned(),
                    to: renamed.clone(),
                });
                renamed
            })
        };
        if let Some(item) = doc.get_mut(key) {
            map_strings(item, &swap);
        }
    }
    let changes = changes.into_inner();
    if changes.is_empty() {
        return Ok(ToolsetGlobs::Unchanged);
    }
    let text = if crlf {
        doc.to_string().replace('\n', "\r\n")
    } else {
        doc.to_string()
    };
    Ok(ToolsetGlobs::Rewritten { text, changes })
}

/// The strings of a string or an array of strings.
fn strings_of(item: &Item) -> Vec<String> {
    match item {
        Item::Value(Value::Array(values)) => values
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        Item::Value(value) => value.as_str().map(str::to_owned).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// Whether the tool glob matches some tool of the toolset `old` but not the same tool under
/// `new`: it depends on the old name in a way a rename doesn't carry over.
///
/// The probe tool names are a few fixed ones and, for each position in the glob, the rest of the
/// glob with `*` dropped and `?` read as `x`, so globs with literal text after the name
/// (`github*delete*`, `git*__merge*`) are tried with a tool name that fits them.
fn depends_on_name(glob: &str, old: &str, new: &str) -> bool {
    let derived = glob.char_indices().map(|(i, _)| {
        glob[i..]
            .chars()
            .filter(|&c| c != '*')
            .map(|c| if c == '?' { 'x' } else { c })
            .collect::<String>()
    });
    ["", "x", "create_issue"]
        .into_iter()
        .map(str::to_owned)
        .chain(derived)
        .any(|tool| {
            glob::matches(glob, &format!("{old}__{tool}"))
                && !glob::matches(glob, &format!("{new}__{tool}"))
        })
}

/// A key for `name` that prints like `key`: same spacing around it, as a header or a dotted key.
fn rekey(key: &Key, name: &str) -> Key {
    let mut renamed = Key::new(name);
    *renamed.leaf_decor_mut() = key.leaf_decor().clone();
    *renamed.dotted_decor_mut() = key.dotted_decor().clone();
    renamed
}

/// Renames the key `old` of `table` to `new`, keeping the entry's value (so its sub-tables, their
/// positions, and its comments) and the order of `table`'s entries.
fn rekey_table(table: &mut Table, old: &str, new: &str) {
    let names: Vec<String> = table.iter().map(|(k, _)| k.to_owned()).collect();
    let entries: Vec<_> = names
        .iter()
        .filter_map(|name| table.remove_entry(name))
        .collect();
    for (key, item) in entries {
        let key = if key.get() == old {
            rekey(&key, new)
        } else {
            key
        };
        table.insert_formatted(&key, item);
    }
}

/// The tables in an array of tables (`[[routes]]`) or an array of inline tables.
fn table_likes_mut(item: &mut Item) -> Vec<&mut dyn TableLike> {
    match item {
        Item::ArrayOfTables(tables) => tables
            .iter_mut()
            .map(|table| table as &mut dyn TableLike)
            .collect(),
        Item::Value(Value::Array(values)) => values
            .iter_mut()
            .filter_map(Value::as_inline_table_mut)
            .map(|table| table as &mut dyn TableLike)
            .collect(),
        _ => Vec::new(),
    }
}

/// Replaces a string value with `f`'s result, keeping the whitespace and comments around it.
fn map_string(item: &mut Item, f: &dyn Fn(&str) -> Option<String>) -> bool {
    item.as_value_mut().is_some_and(|value| replace(value, f))
}

fn replace(value: &mut Value, f: &dyn Fn(&str) -> Option<String>) -> bool {
    let Some(text) = value.as_str().and_then(f) else {
        return false;
    };
    let decor = value.decor().clone();
    *value = Value::from(text);
    *value.decor_mut() = decor;
    true
}

/// [`map_string`] on a string or on each string of an array; returns how many changed.
fn map_strings(item: &mut Item, f: &dyn Fn(&str) -> Option<String>) -> usize {
    match item.as_value_mut() {
        Some(Value::Array(values)) => values.iter_mut().map(|v| usize::from(replace(v, f))).sum(),
        Some(value) => usize::from(replace(value, f)),
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GILES: &str = r##"# Giles configuration
[core]
http_listen = "127.0.0.1:8080"   # local only

[owner]
identities = ["irc:libera:account:me"]

# The Libera network.
[channel.libera]   # trailing comment
component = "irc"
config = { server = "irc.libera.chat" }

[channel.libera.sandbox]
network = ["tcp:6697"]

[channel.web]

[toolset.files]
command = "/bin/true"

[[routes]]
channel = "libera"   # DMs
agent = "jeeves"

[[routes]]
channel = "web"
agent = "jeeves"
"##;

    #[test]
    fn renames_a_table_in_place_and_keeps_everything_else_byte_for_byte() {
        let edit = rename_in_giles_toml(GILES, Kind::Channel, "libera", "libera-chat").unwrap();
        assert!(edit.table_renamed);
        assert_eq!(edit.routes, 1);
        // Only the three places that name the instance differ.
        let expected = GILES
            .replace("[channel.libera]", "[channel.libera-chat]")
            .replace("[channel.libera.sandbox]", "[channel.libera-chat.sandbox]")
            .replace("channel = \"libera\"", "channel = \"libera-chat\"");
        assert_eq!(edit.text, expected);
    }

    #[test]
    fn the_renamed_document_means_what_the_old_one_did() {
        let edit = rename_in_giles_toml(GILES, Kind::Channel, "libera", "libera-chat").unwrap();
        let before: toml::Table = toml::from_str(GILES).unwrap();
        let after: toml::Table = toml::from_str(&edit.text).unwrap();
        let mut expected = before.clone();
        let channels = expected["channel"].as_table_mut().unwrap();
        let libera = channels.remove("libera").unwrap();
        channels.insert("libera-chat".into(), libera);
        expected["routes"].as_array_mut().unwrap()[0]["channel"] = "libera-chat".into();
        assert_eq!(after, expected);
    }

    #[test]
    fn other_instances_and_routes_are_untouched() {
        let edit = rename_in_giles_toml(GILES, Kind::Toolset, "files", "docs").unwrap();
        assert_eq!(edit.routes, 0);
        assert_eq!(
            edit.text,
            GILES.replace("[toolset.files]", "[toolset.docs]")
        );
    }

    #[test]
    fn an_already_renamed_table_is_left_alone() {
        let hand_edited = GILES.replace("libera", "libera-chat");
        let edit =
            rename_in_giles_toml(&hand_edited, Kind::Channel, "libera", "libera-chat").unwrap();
        assert!(!edit.table_renamed);
        assert_eq!(edit.routes, 0);
        assert_eq!(edit.text, hand_edited);
    }

    #[test]
    fn dotted_and_inline_spellings_are_renamed() {
        let dotted = "[channel]\nlibera.component = \"irc\"\nweb = {}\n";
        let edit = rename_in_giles_toml(dotted, Kind::Channel, "libera", "oftc").unwrap();
        assert_eq!(edit.text, "[channel]\noftc.component = \"irc\"\nweb = {}\n");

        let inline = "channel = { libera = { component = \"irc\" }, web = {} }\n";
        let edit = rename_in_giles_toml(inline, Kind::Channel, "libera", "oftc").unwrap();
        assert_eq!(
            edit.text,
            "channel = { oftc = { component = \"irc\" }, web = {} }\n"
        );

        let routes = "routes = [{ channel = \"libera\", agent = \"a\" }]\n[channel.libera]\n";
        let edit = rename_in_giles_toml(routes, Kind::Channel, "libera", "oftc").unwrap();
        assert_eq!(
            edit.text,
            "routes = [{ channel = \"oftc\", agent = \"a\" }]\n[channel.oftc]\ncomponent = \"libera\"\n"
        );
    }

    #[test]
    fn invalid_toml_is_a_syntax_error() {
        let err = rename_in_giles_toml("[channel.x", Kind::Channel, "x", "y").unwrap_err();
        assert!(matches!(err, EditError::Syntax(_)), "{err}");
    }

    #[test]
    fn configured_lists_every_kind_but_ignored_toolset_tables() {
        let doc = parse(GILES).unwrap();
        let names = configured(&doc);
        assert_eq!(names["libera"], Kind::Channel);
        assert_eq!(names["web"], Kind::Channel);
        assert_eq!(names["files"], Kind::Toolset);
        assert_eq!(names.len(), 3);

        // Loading ignores a toolset table with `url`, so it configures nothing.
        let doc = parse(&format!("{GILES}[toolset.github]\nurl = \"https://x\"\n")).unwrap();
        assert!(!configured(&doc).contains_key("github"));
    }

    #[test]
    fn a_table_already_under_the_new_name_is_refused_not_overwritten() {
        let source = format!("{GILES}[toolset.docs]\nurl = \"https://x\"\n");
        let err = rename_in_giles_toml(&source, Kind::Toolset, "files", "docs").unwrap_err();
        assert!(matches!(err, EditError::Unsupported(_)), "{err}");
        assert!(err.to_string().contains("[toolset.docs]"), "{err}");
    }

    const AGENT: &str = r##"name = "Jeeves"
# Fallbacks follow.
models = ["openrouter/anthropic/claude-sonnet-4.5", "ollama/qwen3:8b"]   # in order
compaction_model = "ollama/qwen3:8b"
tools = ["github__*", "memory*"]
ask = ["github__create_issue", "shell__exec"]

[[jobs]]
id = "check-in"
cron = "*/30 * * * *"
model = "ollama/llama3"
prompt = "Hello"

[[jobs]]
id = "other"
cron = "0 * * * *"
model = "anthropic/claude"
prompt = "Hi"
"##;

    #[test]
    fn a_provider_rename_rewrites_the_models_that_use_it() {
        let (text, count) = rename_in_agent_toml(AGENT, Kind::Provider, "ollama", "local")
            .unwrap()
            .unwrap();
        assert_eq!(count, 3);
        assert_eq!(
            text,
            AGENT
                .replace("ollama/qwen3:8b", "local/qwen3:8b")
                .replace("ollama/llama3", "local/llama3")
        );
    }

    #[test]
    fn a_provider_name_is_matched_whole_not_as_a_prefix() {
        // `openrouter/...` must not match `open`, and a model id mentioning the name is not a
        // reference.
        let source = "models = [\"openrouter/open/x\"]\n";
        assert_eq!(
            rename_in_agent_toml(source, Kind::Provider, "open", "new").unwrap(),
            None
        );
    }

    #[test]
    fn a_toolset_rename_rewrites_the_globs_with_its_prefix() {
        let (text, count) = rename_in_agent_toml(AGENT, Kind::Toolset, "github", "gh")
            .unwrap()
            .unwrap();
        assert_eq!(count, 2);
        assert_eq!(
            text,
            AGENT
                .replace("github__*", "gh__*")
                .replace("github__create_issue", "gh__create_issue")
        );
    }

    #[test]
    fn agents_have_nothing_to_rename_for_channels_or_unrelated_names() {
        assert_eq!(
            rename_in_agent_toml(AGENT, Kind::Channel, "ollama", "x").unwrap(),
            None
        );
        assert_eq!(
            rename_in_agent_toml(AGENT, Kind::Provider, "nothing", "x").unwrap(),
            None
        );
        assert_eq!(
            rename_in_agent_toml(AGENT, Kind::Toolset, "shell", "sh")
                .unwrap()
                .unwrap()
                .1,
            1
        );
    }

    /// The component `[<kind>.<name>]` of `source` runs, as the config loader resolves it.
    fn component(source: &str, kind: Kind, name: &str) -> Option<String> {
        component_of(&parse(source).unwrap(), kind, name)
    }

    #[test]
    fn a_table_without_component_keeps_running_its_component_under_the_new_name() {
        // `[channel.web]` is the built-in web channel because its name says so; `[channel.chat]`
        // would look for the plugin giles-channel-chat.
        let source = "# the web chat\n[channel.web]\n\n[channel.irc]\n[provider.anthropic]\n";
        for (kind, old, new) in [
            (Kind::Channel, "web", "chat"),
            (Kind::Channel, "irc", "libera"),
            (Kind::Provider, "anthropic", "claude"),
        ] {
            let edit = rename_in_giles_toml(source, kind, old, new).unwrap();
            assert_eq!(component(&edit.text, kind, new).as_deref(), Some(old));
            assert!(
                edit.text.contains(&format!("component = \"{old}\"")),
                "{}",
                edit.text
            );
        }
        let edit = rename_in_giles_toml(source, Kind::Channel, "web", "chat").unwrap();
        assert_eq!(
            edit.text,
            "# the web chat\n[channel.chat]\ncomponent = \"web\"\n\n[channel.irc]\n[provider.anthropic]\n"
        );
        // Inline and dotted spellings get it too.
        let edit =
            rename_in_giles_toml("channel = { web = {} }\n", Kind::Channel, "web", "chat").unwrap();
        assert_eq!(
            component(&edit.text, Kind::Channel, "chat").as_deref(),
            Some("web")
        );
        let edit =
            rename_in_giles_toml("[channel]\nweb.x = 1\n", Kind::Channel, "web", "chat").unwrap();
        assert_eq!(
            component(&edit.text, Kind::Channel, "chat").as_deref(),
            Some("web")
        );
    }

    #[test]
    fn a_component_that_is_written_stays_as_written_and_toolsets_get_none() {
        let source = "[channel.libera]\ncomponent = \"irc\"\n";
        let edit = rename_in_giles_toml(source, Kind::Channel, "libera", "oftc").unwrap();
        assert_eq!(edit.text, "[channel.oftc]\ncomponent = \"irc\"\n");

        // A toolset runs no component, so nothing is added.
        let source = "[toolset.github]\ncommand = \"/bin/x\"\n";
        let edit = rename_in_giles_toml(source, Kind::Toolset, "github", "gh").unwrap();
        assert_eq!(edit.text, "[toolset.gh]\ncommand = \"/bin/x\"\n");
    }

    #[test]
    fn globs_that_match_by_the_old_name_without_saying_so_are_refused() {
        let agent = |tools: &str, ask: &str| format!("tools = {tools}\nask = {ask}\n");
        for (tools, ask, shown) in [
            ("[\"*\"]", "[\"github*\"]", "ask entry \"github*\""),
            ("[\"git*\"]", "[]", "tools entry \"git*\""),
            ("[\"github?_*\"]", "[]", "\"github?_*\""),
            ("[\"*hub__*\"]", "[]", "\"*hub__*\""),
            ("[\"github*delete*\"]", "[]", "\"github*delete*\""),
            ("[\"*hub__delete_*\"]", "[]", "\"*hub__delete_*\""),
            ("[\"git*__merge*\"]", "[]", "\"git*__merge*\""),
        ] {
            let err = rename_in_agent_toml(&agent(tools, ask), Kind::Toolset, "github", "gh")
                .unwrap_err();
            let message = err.to_string();
            assert!(message.contains(shown), "{message}");
            assert!(message.contains("rewrite them by hand"), "{message}");
        }
        // Globs that mean the same under both names are fine, and so are the rewritable ones.
        let ok = agent(
            "[\"*\", \"memory*\", \"*__create_issue\", \"github__*\"]",
            "[\"gh*\"]",
        );
        let (text, count) = rename_in_agent_toml(&ok, Kind::Toolset, "github", "gh")
            .unwrap()
            .unwrap();
        assert_eq!(count, 1);
        assert!(text.contains("\"gh__*\""), "{text}");
        // `git*` matches `gh__x`? No; but it does when the new name starts the same way.
        assert!(
            rename_in_agent_toml(
                &agent("[\"git*\"]", "[]"),
                Kind::Toolset,
                "github",
                "gitlab"
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn the_rewritten_globs_are_listed_with_their_old_and_new_text() {
        let source = "# Jeeves\ntools = [\"github__*\", \"memory*\"]  # kept\nask = [\"github__create_issue\", \"x\"]\n";
        let ToolsetGlobs::Rewritten { text, changes } =
            rewrite_toolset_globs(source, "github", "gh").unwrap()
        else {
            panic!("nothing was rewritten");
        };
        assert_eq!(
            text,
            source.replace("github__", "gh__"),
            "only the globs change"
        );
        let listed: Vec<(&str, &str, &str)> = changes
            .iter()
            .map(|c| (c.key, c.from.as_str(), c.to.as_str()))
            .collect();
        assert_eq!(
            listed,
            [
                ("tools", "github__*", "gh__*"),
                ("ask", "github__create_issue", "gh__create_issue"),
            ]
        );

        // Nothing names it, or the globs can't be rewritten.
        assert_eq!(
            rewrite_toolset_globs("tools = [\"memory*\"]\n", "github", "gh").unwrap(),
            ToolsetGlobs::Unchanged
        );
        assert_eq!(
            rewrite_toolset_globs(
                "tools = [\"git*\", \"x\"]\nask = [\"github*\"]\n",
                "github",
                "gh"
            )
            .unwrap(),
            ToolsetGlobs::Ambiguous(vec![
                "tools entry \"git*\"".to_owned(),
                "ask entry \"github*\"".to_owned()
            ])
        );
        assert!(rewrite_toolset_globs("tools = [", "github", "gh").is_err());
    }

    #[test]
    fn crlf_line_ends_survive_a_glob_rewrite() {
        let source = "models = [\"a/b\"]\r\ntools = [\"github__*\"]\r\n";
        let ToolsetGlobs::Rewritten { text, .. } =
            rewrite_toolset_globs(source, "github", "gh").unwrap()
        else {
            panic!("nothing was rewritten");
        };
        assert_eq!(text, "models = [\"a/b\"]\r\ntools = [\"gh__*\"]\r\n");
    }
}

//! The workspace tools (PLAN section 8.8): `environment_plugins`, `environment_create`,
//! `environment_status`, `environment_reset`, `environment_delete`, `bash`, `read`, `write`,
//! `edit`, `find`, and `grep`. They are offered only to agents whose `environments` is not empty
//! ([`SessionTools::for_agent`](crate::agent_loop::SessionTools::for_agent)), and each call checks
//! that the environment it touches is one of the agent's.
//!
//! The environment plugin provides primitives ([`Environment`]); the tools build everything the
//! model sees on them, so the output caps, the line numbers, and the edit rules are the same for
//! every plugin. A workspace tool opens the conversation's workspace through
//! [`Workspaces::open`], which leases it for the turn and starts it if it was stopped. A notice
//! from the open (the workspace was started again, or recreated) leads the result. A call with
//! no workspace fails with guidance: which environments the agent may use, and, after a
//! deletion, when and why and what the workspace was. Nothing is created automatically.
//!
//! The lifecycle tools are in [`lifecycle`], `bash` in [`bash`], the file tools in [`files`], and
//! `find` and `grep` in [`search`].

pub(super) mod bash;
pub(super) mod files;
pub(super) mod lifecycle;
pub(super) mod search;

#[cfg(test)]
mod tests;

use std::fmt::Write as _;
use std::sync::{Arc, Weak};

use giles_types::Block;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::CoreCall;
use crate::agent_loop::ToolOutput;
use crate::registry::{EnvironmentEntry, Registry, RegistryError};
use crate::runtime::Runtime;
use crate::workspaces::{Opened, WorkspaceError, WorkspaceOwner, WorkspaceRecord, Workspaces};

/// Registers the workspace tools with `runtime`'s registry.
///
/// # Errors
///
/// [`RegistryError::ToolName`] if a tool's name isn't reserved for core tools (a bug).
pub(super) fn register(runtime: &Runtime) -> Result<(), RegistryError> {
    let env = Env::new(runtime.workspaces.clone(), &runtime.registry);
    let registry = &runtime.registry;
    registry.register_core_tool(Arc::new(lifecycle::EnvironmentPluginsTool(env.clone())))?;
    registry.register_core_tool(Arc::new(lifecycle::EnvironmentCreateTool(env.clone())))?;
    registry.register_core_tool(Arc::new(lifecycle::EnvironmentStatusTool(env.clone())))?;
    registry.register_core_tool(Arc::new(lifecycle::EnvironmentResetTool(env.clone())))?;
    registry.register_core_tool(Arc::new(lifecycle::EnvironmentDeleteTool(env.clone())))?;
    registry.register_core_tool(Arc::new(bash::BashTool(env.clone())))?;
    registry.register_core_tool(Arc::new(files::ReadTool(env.clone())))?;
    registry.register_core_tool(Arc::new(files::WriteTool(env.clone())))?;
    registry.register_core_tool(Arc::new(files::EditTool(env.clone())))?;
    registry.register_core_tool(Arc::new(search::FindTool(env.clone())))?;
    registry.register_core_tool(Arc::new(search::GrepTool(env)))?;
    Ok(())
}

/// What the workspace tools share: the workspace service and the registry, which says which
/// environments exist and which an agent may use. Cheap to clone. The registry is held weakly: it
/// owns the tools, so a strong reference would keep it (and the database, and the plugins) alive
/// for ever.
#[derive(Clone)]
pub(super) struct Env {
    workspaces: Workspaces,
    registry: Weak<Registry>,
}

impl std::fmt::Debug for Env {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Env").finish_non_exhaustive()
    }
}

/// What a call is about: the environments its agent may use and the owner of its workspace.
pub(super) struct Scope {
    /// The agent's environments, in the order of its `environments`. Never empty.
    pub(super) allowed: Vec<EnvironmentEntry>,
    pub(super) owner: WorkspaceOwner,
}

impl Env {
    pub(super) fn new(workspaces: Workspaces, registry: &Arc<Registry>) -> Self {
        Self {
            workspaces,
            registry: Arc::downgrade(registry),
        }
    }

    /// The scope of `call`.
    ///
    /// # Errors
    ///
    /// The error result to return: the agent isn't configured or has no environments.
    pub(super) fn scope(&self, call: &CoreCall) -> Result<Scope, ToolOutput> {
        let agent = &call.context.agent;
        let Some(registry) = self.registry.upgrade() else {
            return Err(ToolOutput::error("Giles is shutting down"));
        };
        let config = registry.config();
        let Some(agent_config) = config.agents.get(agent) else {
            return Err(ToolOutput::error(format!(
                "agent `{agent}` is not configured"
            )));
        };
        let allowed: Vec<EnvironmentEntry> = agent_config
            .environments
            .iter()
            .filter_map(|name| registry.environment(name.as_str()))
            .collect();
        if allowed.is_empty() {
            return Err(ToolOutput::error(format!(
                "agent `{agent}` has no environments"
            )));
        }
        Ok(Scope {
            allowed,
            owner: WorkspaceOwner::of_session(&call.turn.session),
        })
    }

    /// Opens the conversation's workspace for a call: checks that its environment is one of the
    /// agent's, leases it, and starts it if needed.
    ///
    /// # Errors
    ///
    /// The error result to return, with the guidance the model needs when it has no workspace.
    pub(super) async fn open(&self, call: &CoreCall) -> Result<Open, ToolOutput> {
        let scope = self.scope(call)?;
        match self.workspaces.current(&scope.owner).await {
            Ok(Some(current)) => {
                scope.check(&current.environment)?;
            }
            Ok(None) => return Err(self.no_workspace(&scope).await),
            Err(error) => return Err(workspace_error(&error)),
        }
        match self.workspaces.open(&scope.owner, &call.turn.lease).await {
            Ok(Opened {
                record,
                client,
                notice,
            }) => Ok(Open {
                record,
                client,
                notice,
            }),
            Err(WorkspaceError::NoWorkspace) => Err(self.no_workspace(&scope).await),
            Err(error) => Err(workspace_error(&error)),
        }
    }

    /// The error result for a call that needs a workspace the conversation doesn't have: what
    /// happened to the last one, if there was one, and where a new one can be created.
    pub(super) async fn no_workspace(&self, scope: &Scope) -> ToolOutput {
        let mut text = String::new();
        match self.workspaces.last_deleted(&scope.owner).await {
            Ok(Some(gone)) => {
                text.push_str("This conversation's workspace was deleted");
                if let Some(at) = gone.deleted_at {
                    let _ = write!(text, " at {at}");
                }
                if let Some(reason) = gone.delete_reason {
                    let _ = write!(text, " ({reason})");
                }
                let _ = write!(
                    text,
                    ". It was in the environment `{}` with the options {}. Nothing was created in \
                     its place.",
                    gone.environment, gone.options
                );
            }
            _ => text.push_str("This conversation has no environment yet."),
        }
        text.push_str("\n\nEnvironments you can create a workspace in:\n");
        for entry in &scope.allowed {
            let _ = writeln!(text, "{}", entry_line(entry));
        }
        text.push_str(
            "\nCall environment_plugins to see their options, then environment_create to create \
             a workspace.",
        );
        ToolOutput::error(text)
    }
}

impl Scope {
    /// Whether the agent may use the environment `name`.
    ///
    /// # Errors
    ///
    /// The error result to return, naming the environments the agent may use.
    pub(super) fn check(&self, name: &str) -> Result<&EnvironmentEntry, ToolOutput> {
        self.allowed
            .iter()
            .find(|entry| entry.name.as_str() == name)
            .ok_or_else(|| {
                let names: Vec<String> = self
                    .allowed
                    .iter()
                    .map(|entry| format!("`{}`", entry.name))
                    .collect();
                ToolOutput::error(format!(
                    "the environment `{name}` is not one you may use; your environments are {}",
                    names.join(", ")
                ))
            })
    }
}

/// `- `name`: description`, with a note if the environment isn't reachable.
fn entry_line(entry: &EnvironmentEntry) -> String {
    let mut line = format!("- `{}`", entry.name);
    if !entry.description.is_empty() {
        let _ = write!(line, ": {}", entry.description);
    }
    if !entry.reachable() {
        line.push_str(" (not reachable right now)");
    }
    line
}

/// A workspace opened for a call.
pub(super) struct Open {
    pub(super) record: WorkspaceRecord,
    pub(super) client: Arc<dyn giles_types::Environment>,
    notice: Option<String>,
}

impl Open {
    /// `output`, led by the open's lifecycle notice if it had one.
    pub(super) fn finish(self, output: ToolOutput) -> ToolOutput {
        lead(self.notice, output)
    }

    /// An error result from the plugin, led by the notice.
    pub(super) fn plugin_error(self, error: &giles_types::Error) -> ToolOutput {
        self.finish(plugin_error(error))
    }
}

/// `output` with `notice` as its first paragraph.
fn lead(notice: Option<String>, mut output: ToolOutput) -> ToolOutput {
    let Some(notice) = notice else {
        return output;
    };
    match output.content.first_mut() {
        Some(Block::Text { text }) => *text = format!("{notice}\n\n{text}"),
        _ => output.content.insert(0, Block::text(notice)),
    }
    output
}

/// A successful result with one text block.
pub(super) fn ok(text: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: vec![Block::text(text)],
        is_error: false,
    }
}

/// The error result of a [`WorkspaceError`], whose message is written for the model.
pub(super) fn workspace_error(error: &WorkspaceError) -> ToolOutput {
    ToolOutput::error(error.to_string())
}

/// The error result of a failed environment call: a refused request or a missing path says what
/// the plugin said; anything else says the environment failed.
pub(super) fn plugin_error(error: &giles_types::Error) -> ToolOutput {
    match error {
        giles_types::Error::InvalidRequest(message) | giles_types::Error::NotFound(message) => {
            ToolOutput::error(message.clone())
        }
        other => ToolOutput::error(format!("the environment failed: {other}")),
    }
}

/// Parses a call's input.
///
/// # Errors
///
/// The error result to return.
pub(super) fn parse<T: DeserializeOwned>(input: Value) -> Result<T, ToolOutput> {
    serde_json::from_value(input)
        .map_err(|error| ToolOutput::error(format!("invalid input: {error}")))
}

/// The workspace as the model reads it: environment, options, and what the plugin said about it.
pub(super) fn describe(record: &WorkspaceRecord) -> String {
    let mut text = format!(
        "Environment: `{}`\nOptions: {}\n",
        record.environment, record.options
    );
    if let Some(description) = &record.description {
        let _ = writeln!(text, "Description: {description}");
    }
    if let Some(directory) = &record.working_directory {
        let _ = writeln!(text, "Working directory: {directory}");
    }
    if let Some(persistence) = &record.persistence {
        let _ = writeln!(text, "What persists: {persistence}");
    }
    text
}

/// `bytes` in the largest binary unit that keeps the number at least 1, e.g. `1.5 MiB`.
pub(super) fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

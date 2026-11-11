//! The tools that manage the conversation's workspace: `environment_plugins`,
//! `environment_create`, `environment_status`, `environment_reset`, and `environment_delete`.
//!
//! A conversation has at most one workspace. `environment_create` fails if there is one,
//! naming it; `environment_reset` and `environment_delete` free the slot at once (the plugin is
//! told in the background if it can't be reached), so the model can create another in the same
//! turn. None of them needs approval unless the operator puts it in the agent's `ask` globs.
//! `environment_plugins` and `environment_status` only read, so they're parallel-safe
//! ([`PARALLEL_SAFE_CORE_TOOLS`](crate::agent_loop::PARALLEL_SAFE_CORE_TOOLS)).

use std::fmt::Write as _;

use async_trait::async_trait;
use giles_store::{DeleteReason, WorkspaceState};
use giles_types::{ToolSpec, WorkspaceState as PluginState};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Env, Scope, describe, format_bytes, ok, parse, workspace_error};
use crate::agent_loop::ToolOutput;
use crate::core_tools::{CoreCall, CoreTool};
use crate::workspaces::{WorkspaceError, WorkspaceRecord};

/// The name of `environment_plugins`.
pub(in crate::core_tools) const ENVIRONMENT_PLUGINS: &str = "environment_plugins";
/// The name of `environment_create`.
pub(in crate::core_tools) const ENVIRONMENT_CREATE: &str = "environment_create";
/// The name of `environment_status`.
pub(in crate::core_tools) const ENVIRONMENT_STATUS: &str = "environment_status";
/// The name of `environment_reset`.
pub(in crate::core_tools) const ENVIRONMENT_RESET: &str = "environment_reset";
/// The name of `environment_delete`.
pub(in crate::core_tools) const ENVIRONMENT_DELETE: &str = "environment_delete";

/// The `environment_plugins` tool.
#[derive(Debug, Clone)]
pub(in crate::core_tools) struct EnvironmentPluginsTool(pub(in crate::core_tools) Env);

/// The `environment_create` tool.
#[derive(Debug, Clone)]
pub(in crate::core_tools) struct EnvironmentCreateTool(pub(in crate::core_tools) Env);

/// The `environment_status` tool.
#[derive(Debug, Clone)]
pub(in crate::core_tools) struct EnvironmentStatusTool(pub(in crate::core_tools) Env);

/// The `environment_reset` tool.
#[derive(Debug, Clone)]
pub(in crate::core_tools) struct EnvironmentResetTool(pub(in crate::core_tools) Env);

/// The `environment_delete` tool.
#[derive(Debug, Clone)]
pub(in crate::core_tools) struct EnvironmentDeleteTool(pub(in crate::core_tools) Env);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NoInput {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateInput {
    plugin: String,
    options: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResetInput {
    plugin: Option<String>,
    options: Option<Value>,
}

#[async_trait]
impl CoreTool for EnvironmentPluginsTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: ENVIRONMENT_PLUGINS.to_owned(),
            description: "List the environment plugins you can create a workspace in: each one's \
                          name, what it is for, whether it is reachable right now, and the JSON \
                          Schema of the options environment_create takes for it. Read this \
                          before environment_create if you need to choose options."
                .to_owned(),
            input_schema: no_input(),
        }
    }

    async fn call(&self, call: CoreCall) -> ToolOutput {
        if let Err(error) = parse::<NoInput>(call.input.clone()) {
            return error;
        }
        let scope = match self.0.scope(&call) {
            Ok(scope) => scope,
            Err(output) => return output,
        };
        let mut text = String::from("Environment plugins you can use:\n");
        for entry in &scope.allowed {
            let _ = write!(text, "\n## `{}`\n", entry.name);
            if !entry.description.is_empty() {
                let _ = writeln!(text, "{}", entry.description);
            }
            let _ = writeln!(
                text,
                "Reachable: {}",
                if entry.reachable() { "yes" } else { "no" }
            );
            match &entry.options_schema {
                Some(schema) => {
                    let pretty = serde_json::to_string_pretty(schema).unwrap_or_default();
                    let _ = write!(text, "Options schema:\n```json\n{pretty}\n```\n");
                }
                None => text.push_str("Options schema: not known until it first connects\n"),
            }
        }
        ok(text)
    }
}

#[async_trait]
impl CoreTool for EnvironmentCreateTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: ENVIRONMENT_CREATE.to_owned(),
            description: "Create this conversation's workspace: an isolated environment where \
                          bash, read, write, edit, find, and grep work. A conversation has one \
                          workspace, and its files stay for later turns and sessions. The \
                          result gives the working directory and what persists. If the \
                          environment refuses the options, the error says why. See \
                          environment_plugins for the environments and their options."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "plugin": {
                        "type": "string",
                        "description": "The environment plugin, by name, from environment_plugins.",
                    },
                    "options": {
                        "type": "object",
                        "default": {},
                        "description": "The options for the plugin, matching the options \
                                        schema environment_plugins shows. Leave out for none.",
                    },
                },
                "required": ["plugin"],
                "additionalProperties": false,
            }),
        }
    }

    async fn call(&self, call: CoreCall) -> ToolOutput {
        let input: CreateInput = match parse(call.input.clone()) {
            Ok(input) => input,
            Err(output) => return output,
        };
        let scope = match self.0.scope(&call) {
            Ok(scope) => scope,
            Err(output) => return output,
        };
        if let Err(output) = scope.check(&input.plugin) {
            return output;
        }
        let options = input.options.unwrap_or_else(|| json!({}));
        match self
            .0
            .workspaces
            .create(&scope.owner, &input.plugin, options)
            .await
        {
            Ok(record) => ok(format!("Created the workspace.\n{}", describe(&record))),
            Err(WorkspaceError::AlreadyHasWorkspace { current }) => {
                ToolOutput::error(already_has(&current))
            }
            Err(error) => workspace_error(&error),
        }
    }
}

#[async_trait]
impl CoreTool for EnvironmentStatusTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: ENVIRONMENT_STATUS.to_owned(),
            description: "Show this conversation's workspace: its environment, options, state, \
                          when it was created and last used, and the disk it uses. Says so if \
                          there is none, and what became of the last one."
                .to_owned(),
            input_schema: no_input(),
        }
    }

    async fn call(&self, call: CoreCall) -> ToolOutput {
        if let Err(error) = parse::<NoInput>(call.input.clone()) {
            return error;
        }
        let scope = match self.0.scope(&call) {
            Ok(scope) => scope,
            Err(output) => return output,
        };
        let record = match self.0.workspaces.current(&scope.owner).await {
            Ok(Some(record)) => record,
            Ok(None) => return self.0.no_workspace(&scope).await,
            Err(error) => return workspace_error(&error),
        };
        if let Err(output) = scope.check(&record.environment) {
            return output;
        }
        let mut text = format!("Workspace `{}`\n{}", record.id, describe(&record));
        let (state, disk) = self.plugin_view(&scope, &record).await;
        let _ = writeln!(text, "State: {state}");
        let _ = writeln!(text, "Created: {}", record.created_at);
        let _ = writeln!(text, "Last used: {}", record.last_used_at);
        if let Some(bytes) = disk {
            let _ = writeln!(text, "Disk used: {}", format_bytes(bytes));
        }
        ok(text)
    }
}

impl EnvironmentStatusTool {
    /// The workspace's run state and disk use as its environment reports them now. Asking the
    /// environment never starts the workspace.
    async fn plugin_view(&self, scope: &Scope, record: &WorkspaceRecord) -> (String, Option<u64>) {
        if record.state == WorkspaceState::Creating {
            return ("being created".to_owned(), None);
        }
        let Ok(entry) = scope.check(&record.environment) else {
            return ("unknown".to_owned(), None);
        };
        let Some(client) = &entry.client else {
            return (
                "unknown (the environment is not reachable right now)".to_owned(),
                None,
            );
        };
        match client.list_workspaces().await {
            Ok(list) => match list.workspaces.iter().find(|held| held.id == record.id) {
                Some(held) => (
                    match held.state {
                        PluginState::Running => "running".to_owned(),
                        PluginState::Stopped => {
                            "stopped (the next workspace tool call starts it)".to_owned()
                        }
                        _ => "unknown".to_owned(),
                    },
                    held.disk_used_bytes,
                ),
                None => ("missing from the environment".to_owned(), None),
            },
            Err(_) => (
                "unknown (the environment failed to answer)".to_owned(),
                None,
            ),
        }
    }
}

#[async_trait]
impl CoreTool for EnvironmentResetTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: ENVIRONMENT_RESET.to_owned(),
            description: "Delete this conversation's workspace, with everything in it, and \
                          create a new one. With no input the new one has the same environment \
                          and options as the old one; plugin and options change that (options \
                          default to the old ones only if the plugin stays the same). The \
                          result says what was discarded. If creating the new workspace fails \
                          you are left with none.\n\n\
                          Go ahead if the user clearly wants a fresh start or the files gone. \
                          If you aren't sure they want to lose what is in the workspace, ask \
                          them first."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "plugin": {
                        "type": "string",
                        "description": "The environment plugin for the new workspace. Default: \
                                        the current one.",
                    },
                    "options": {
                        "type": "object",
                        "description": "The options for the new workspace. Default: the \
                                        current options, if the plugin stays the same, \
                                        otherwise none.",
                    },
                },
                "additionalProperties": false,
            }),
        }
    }

    async fn call(&self, call: CoreCall) -> ToolOutput {
        let input: ResetInput = match parse(call.input.clone()) {
            Ok(input) => input,
            Err(output) => return output,
        };
        let scope = match self.0.scope(&call) {
            Ok(scope) => scope,
            Err(output) => return output,
        };
        let current = match self.0.workspaces.current(&scope.owner).await {
            Ok(current) => current,
            Err(error) => return workspace_error(&error),
        };
        // Everything that can be refused is checked before anything is deleted.
        let plugin = match (&input.plugin, &current) {
            (Some(plugin), _) => plugin.clone(),
            (None, Some(current)) => current.environment.clone(),
            (None, None) => return self.0.no_workspace(&scope).await,
        };
        let entry = match scope.check(&plugin) {
            Ok(entry) => entry,
            Err(output) => return output,
        };
        if let Some(current) = &current
            && let Err(output) = scope.check(&current.environment)
        {
            return output;
        }
        if !entry.reachable() {
            return workspace_error(&WorkspaceError::Unreachable {
                environment: plugin,
            });
        }
        let options = match (input.options, &current) {
            (Some(options), _) => options,
            (None, Some(current)) if current.environment == plugin => current.options.clone(),
            _ => json!({}),
        };
        if !options.is_object() {
            return ToolOutput::error("the options must be a JSON object ({} for none)");
        }

        let discarded = match self
            .0
            .workspaces
            .delete(&scope.owner, DeleteReason::ModelReset)
            .await
        {
            Ok(Some(record)) => Some(record),
            Ok(None) => None,
            Err(error) => return workspace_error(&error),
        };
        let gone = match &discarded {
            Some(record) => format!(
                "Deleted the previous workspace (environment `{}`, options {}); everything in \
                 it is gone.{}",
                record.environment,
                record.options,
                pending_note(record)
            ),
            None => "There was no workspace to discard.".to_owned(),
        };
        match self
            .0
            .workspaces
            .create(&scope.owner, &plugin, options)
            .await
        {
            Ok(record) => ok(format!(
                "{gone}\n\nCreated a new workspace.\n{}",
                describe(&record)
            )),
            Err(error) => ToolOutput::error(format!(
                "{gone}\n\nCreating the new workspace failed: {error}\nYou have no workspace now; \
                 call environment_create to try again."
            )),
        }
    }
}

#[async_trait]
impl CoreTool for EnvironmentDeleteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: ENVIRONMENT_DELETE.to_owned(),
            description: "Delete this conversation's workspace, with everything in it. Workspace \
                          tools then fail until you create another with environment_create.\n\n\
                          Go ahead if the user clearly wants the files gone. If you aren't sure \
                          they want to lose what is in the workspace, ask them first."
                .to_owned(),
            input_schema: no_input(),
        }
    }

    async fn call(&self, call: CoreCall) -> ToolOutput {
        if let Err(error) = parse::<NoInput>(call.input.clone()) {
            return error;
        }
        let scope = match self.0.scope(&call) {
            Ok(scope) => scope,
            Err(output) => return output,
        };
        match self.0.workspaces.current(&scope.owner).await {
            Ok(Some(current)) => {
                if let Err(output) = scope.check(&current.environment) {
                    return output;
                }
            }
            Ok(None) => return self.0.no_workspace(&scope).await,
            Err(error) => return workspace_error(&error),
        }
        match self
            .0
            .workspaces
            .delete(&scope.owner, DeleteReason::ModelDelete)
            .await
        {
            Ok(Some(record)) => ok(format!(
                "Deleted the workspace (environment `{}`, options {}); everything in it is \
                 gone.{}",
                record.environment,
                record.options,
                pending_note(&record)
            )),
            Ok(None) => self.0.no_workspace(&scope).await,
            Err(error) => workspace_error(&error),
        }
    }
}

/// What to add when the environment couldn't confirm a delete: Giles keeps trying.
fn pending_note(record: &WorkspaceRecord) -> &'static str {
    if record.state == WorkspaceState::Deleted {
        ""
    } else {
        " The environment couldn't be reached to remove it; Giles will keep trying."
    }
}

/// The error for `environment_create` when the conversation has a workspace.
fn already_has(current: &WorkspaceRecord) -> String {
    format!(
        "This conversation already has a workspace in the environment `{}` with the options {}. \
         Use it, or call environment_reset or environment_delete first.",
        current.environment, current.options
    )
}

/// The schema of a tool that takes nothing.
fn no_input() -> Value {
    json!({"type": "object", "properties": {}, "additionalProperties": false})
}

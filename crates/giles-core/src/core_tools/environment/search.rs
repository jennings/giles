//! The `find` and `grep` tools. They are calls of the environment protocol ([`Environment::find`],
//! [`Environment::grep`]), which the plugin implements however suits it. The core sets the limits
//! and shapes the result, so they look the same for every plugin. Both only read, so they're
//! parallel-safe ([`PARALLEL_SAFE_CORE_TOOLS`](crate::agent_loop::PARALLEL_SAFE_CORE_TOOLS)).

use std::fmt::Write as _;

use async_trait::async_trait;
use giles_types::{FindRequest, GrepRequest, ToolSpec};
use serde::Deserialize;
use serde_json::json;

use super::{Env, ok, parse};
use crate::agent_loop::ToolOutput;
use crate::core_tools::{CoreCall, CoreTool};

/// The name of `find`.
pub const FIND: &str = "find";
/// The name of `grep`.
pub const GREP: &str = "grep";

/// The most paths `find` and the most matching lines `grep` return.
pub const RESULT_LIMIT: u32 = 200;

/// The longest matching line `grep` shows, in characters. A longer one is cut.
pub const GREP_LINE_CHARS: usize = 500;

/// The `find` tool.
#[derive(Debug, Clone)]
pub(in crate::core_tools) struct FindTool(pub(in crate::core_tools) Env);

/// The `grep` tool.
#[derive(Debug, Clone)]
pub(in crate::core_tools) struct GrepTool(pub(in crate::core_tools) Env);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FindInput {
    pattern: String,
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GrepInput {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    #[serde(default)]
    case_insensitive: bool,
}

#[async_trait]
impl CoreTool for FindTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: FIND.to_owned(),
            description: format!(
                "Find files in this conversation's workspace by a glob on their path, e.g. \
                 `**/*.rs`. `**` crosses directories. Skips `.git` directories and files that \
                 `.gitignore` excludes. Returns at most {RESULT_LIMIT} paths, sorted, relative \
                 to the searched directory; narrow the pattern or the path if the result says \
                 it was cut."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "minLength": 1,
                        "description": "A glob matched against paths relative to `path`.",
                    },
                    "path": {
                        "type": "string",
                        "description": "The directory to search: absolute, or relative to the \
                                        working directory. Default: the working directory.",
                    },
                },
                "required": ["pattern"],
                "additionalProperties": false,
            }),
        }
    }

    async fn call(&self, call: CoreCall) -> ToolOutput {
        let input: FindInput = match parse(call.input.clone()) {
            Ok(input) => input,
            Err(output) => return output,
        };
        if input.pattern.is_empty() {
            return ToolOutput::error("invalid input: pattern is empty");
        }
        let open = match self.0.open(&call).await {
            Ok(open) => open,
            Err(output) => return output,
        };
        let request = FindRequest {
            pattern: input.pattern,
            path: input.path,
            limit: RESULT_LIMIT,
        };
        let output = match open.client.find(&open.record.id, request).await {
            Ok(found) if found.paths.is_empty() => ok("No files matched."),
            Ok(found) => {
                let mut text = found.paths.join("\n");
                text.push('\n');
                if found.truncated {
                    let _ = write!(
                        text,
                        "\n[Showing the first {} matches; more matched. Narrow the pattern or \
                         the path.]",
                        found.paths.len()
                    );
                }
                ok(text)
            }
            Err(error) => super::plugin_error(&error),
        };
        open.finish(output)
    }
}

#[async_trait]
impl CoreTool for GrepTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: GREP.to_owned(),
            description: format!(
                "Search file contents in this conversation's workspace with a regular \
                 expression (Rust regex syntax). Returns `path:line:text` for each matching \
                 line, at most {RESULT_LIMIT}, with lines over {GREP_LINE_CHARS} characters \
                 cut; narrow the search if the result says it was cut. Skips binary files, \
                 `.git` directories, and files that `.gitignore` excludes."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The regular expression to search for.",
                    },
                    "path": {
                        "type": "string",
                        "description": "The file or directory to search: absolute, or relative \
                                        to the working directory. Default: the working \
                                        directory.",
                    },
                    "glob": {
                        "type": "string",
                        "description": "Only search files whose path matches this glob, e.g. \
                                        `**/*.rs`.",
                    },
                    "case_insensitive": {
                        "type": "boolean",
                        "default": false,
                        "description": "Match letters regardless of case.",
                    },
                },
                "required": ["pattern"],
                "additionalProperties": false,
            }),
        }
    }

    async fn call(&self, call: CoreCall) -> ToolOutput {
        let input: GrepInput = match parse(call.input.clone()) {
            Ok(input) => input,
            Err(output) => return output,
        };
        if input.pattern.is_empty() {
            return ToolOutput::error("invalid input: pattern is empty");
        }
        let open = match self.0.open(&call).await {
            Ok(open) => open,
            Err(output) => return output,
        };
        let request = GrepRequest {
            pattern: input.pattern,
            path: input.path,
            glob: input.glob,
            case_insensitive: input.case_insensitive,
            limit: RESULT_LIMIT,
        };
        let output = match open.client.grep(&open.record.id, request).await {
            Ok(found) if found.matches.is_empty() => ok("No lines matched."),
            Ok(found) => {
                let mut text = String::new();
                for line in &found.matches {
                    let _ = writeln!(text, "{}:{}:{}", line.path, line.line, cut_line(&line.text));
                }
                if found.truncated {
                    let _ = write!(
                        text,
                        "\n[Showing the first {} matching lines; more matched. Narrow the \
                         pattern, the path, or the glob.]",
                        found.matches.len()
                    );
                }
                ok(text)
            }
            Err(error) => super::plugin_error(&error),
        };
        open.finish(output)
    }
}

/// `line`, or its first [`GREP_LINE_CHARS`] characters and a note if it is longer.
fn cut_line(line: &str) -> String {
    match line.char_indices().nth(GREP_LINE_CHARS) {
        None => line.to_owned(),
        Some((end, _)) => format!(
            "{} [line cut at {GREP_LINE_CHARS} characters]",
            &line[..end]
        ),
    }
}

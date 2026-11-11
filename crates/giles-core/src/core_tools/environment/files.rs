//! The file tools: `read`, `write`, and `edit`. They use the environment's file calls, so the
//! rules here are the same for every plugin: a path is absolute or relative to the working
//! directory (the plugin resolves it), `read` shows numbered lines and refuses binary files,
//! `write` replaces the whole file, and `edit` is a read, a replacement the core makes, and a
//! write.
//!
//! `read` is parallel-safe ([`PARALLEL_SAFE_CORE_TOOLS`](crate::agent_loop::PARALLEL_SAFE_CORE_TOOLS));
//! `write` and `edit` run alone.

use std::fmt::Write as _;

use async_trait::async_trait;
use giles_types::ToolSpec;
use serde::Deserialize;
use serde_json::json;

use super::{Env, format_bytes, ok, parse};
use crate::agent_loop::ToolOutput;
use crate::core_tools::{CoreCall, CoreTool};

/// The name of `read`.
pub const READ: &str = "read";
/// The name of `write`.
pub const WRITE: &str = "write";
/// The name of `edit`.
pub const EDIT: &str = "edit";

/// The most bytes `read` asks the environment for.
pub const READ_MAX_BYTES: u64 = 256 * 1024;

/// How many lines `read` shows when the call sets no `limit`.
pub const READ_DEFAULT_LINES: u64 = 2000;

/// The largest file `edit` changes. A bigger one is refused, since writing back what was read
/// would cut it short.
pub const EDIT_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// How many bytes at the start of a file `read` looks at for a NUL byte, which marks it binary.
const BINARY_PROBE_BYTES: usize = 8192;

/// The `read` tool.
#[derive(Debug, Clone)]
pub(in crate::core_tools) struct ReadTool(pub(in crate::core_tools) Env);

/// The `write` tool.
#[derive(Debug, Clone)]
pub(in crate::core_tools) struct WriteTool(pub(in crate::core_tools) Env);

/// The `edit` tool.
#[derive(Debug, Clone)]
pub(in crate::core_tools) struct EditTool(pub(in crate::core_tools) Env);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadInput {
    path: String,
    offset: Option<u64>,
    limit: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteInput {
    path: String,
    content: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditInput {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

/// What a text file's bytes make, for `read` and `edit`.
enum Text<'a> {
    Text(&'a str),
    Binary,
}

/// The bytes as text. `cut` says the bytes are the start of a longer file, so a character cut in
/// two at the end is fine, and dropped.
fn decode(bytes: &[u8], cut: bool) -> Text<'_> {
    if bytes[..bytes.len().min(BINARY_PROBE_BYTES)].contains(&0) {
        return Text::Binary;
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => Text::Text(text),
        Err(error) if cut && error.error_len().is_none() => {
            // `valid_up_to` is a character boundary inside `bytes`.
            Text::Text(std::str::from_utf8(&bytes[..error.valid_up_to()]).unwrap_or_default())
        }
        Err(_) => Text::Binary,
    }
}

#[async_trait]
impl CoreTool for ReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: READ.to_owned(),
            description: format!(
                "Read a text file in this conversation's workspace. Each line is shown with its \
                 line number. Shows up to {READ_DEFAULT_LINES} lines from `offset` (default: the \
                 start) and the first {} KiB of the file at most; the result says when it left \
                 something out, and how to continue. A binary file is refused: use bash to \
                 inspect it. The path is absolute or relative to the working directory.",
                READ_MAX_BYTES / 1024
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The file: absolute, or relative to the working directory.",
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 1,
                        "default": 1,
                        "description": "The line to start at, counting from 1.",
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "default": READ_DEFAULT_LINES,
                        "description": "The most lines to show.",
                    },
                },
                "required": ["path"],
                "additionalProperties": false,
            }),
        }
    }

    async fn call(&self, call: CoreCall) -> ToolOutput {
        let input: ReadInput = match parse(call.input.clone()) {
            Ok(input) => input,
            Err(output) => return output,
        };
        let offset = input.offset.unwrap_or(1);
        let limit = input.limit.unwrap_or(READ_DEFAULT_LINES);
        if offset == 0 || limit == 0 {
            return ToolOutput::error("invalid input: offset and limit count from 1");
        }
        let open = match self.0.open(&call).await {
            Ok(open) => open,
            Err(output) => return output,
        };
        let file = match open
            .client
            .read_file(&open.record.id, &input.path, READ_MAX_BYTES)
            .await
        {
            Ok(file) => file,
            Err(error) => return open.plugin_error(&error),
        };
        let cut = file.size > file.content.len() as u64;
        let output = match decode(&file.content, cut) {
            Text::Binary => ToolOutput::error(format!(
                "{} looks like a binary file ({}), which read doesn't show. Use bash to inspect \
                 it, for example with `file` or `xxd | head`.",
                input.path,
                format_bytes(file.size)
            )),
            Text::Text(text) => match numbered(text, offset, limit, cut, file.size, &input.path) {
                Ok(shown) => ok(shown),
                Err(message) => ToolOutput::error(message),
            },
        };
        open.finish(output)
    }
}

/// The lines of `text` from `offset` (counting from 1), at most `limit`, numbered like `cat -n`,
/// and what the model needs to know about what was left out. `cut` says `text` is only the start
/// of a file of `size` bytes.
///
/// # Errors
///
/// The message for the model if `offset` is past the lines there are.
fn numbered(
    text: &str,
    offset: u64,
    limit: u64,
    cut: bool,
    size: u64,
    path: &str,
) -> Result<String, String> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return Ok(if cut {
            format!(
                "{path} has no text in its first {} KiB.",
                READ_MAX_BYTES / 1024
            )
        } else {
            format!("{path} is empty.")
        });
    }
    let start = usize::try_from(offset - 1).unwrap_or(usize::MAX);
    if start >= lines.len() {
        return Err(if cut {
            format!(
                "offset {offset} is past the part of {path} that could be read: its first {} \
                 KiB, which hold {} lines. The file is {}.",
                READ_MAX_BYTES / 1024,
                lines.len(),
                format_bytes(size)
            )
        } else {
            format!(
                "offset {offset} is past the end of {path}, which has {} lines.",
                lines.len()
            )
        });
    }
    let end = start
        .saturating_add(usize::try_from(limit).unwrap_or(usize::MAX))
        .min(lines.len());
    let mut out = String::new();
    for (index, line) in lines[start..end].iter().enumerate() {
        let _ = writeln!(out, "{:>6}\t{line}", start + index + 1);
    }
    if end < lines.len() {
        let _ = write!(
            out,
            "\n[Showed lines {}-{end}. Continue with offset {}.]",
            start + 1,
            end + 1
        );
    }
    if cut {
        // The last line of the part read may be cut in two.
        let _ = write!(
            out,
            "\n[Truncated: the file is {}, and only its first {} KiB were read, which end in \
             line {}. Use bash (`sed -n`, `tail`) to see the rest.]",
            format_bytes(size),
            READ_MAX_BYTES / 1024,
            lines.len()
        );
    }
    Ok(out)
}

#[async_trait]
impl CoreTool for WriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: WRITE.to_owned(),
            description: "Write a file in this conversation's workspace: the whole file, \
                          replacing what was there, creating missing directories. To change part \
                          of a file use edit. The path is absolute or relative to the working \
                          directory."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The file: absolute, or relative to the working directory.",
                    },
                    "content": {
                        "type": "string",
                        "description": "The whole new content of the file.",
                    },
                },
                "required": ["path", "content"],
                "additionalProperties": false,
            }),
        }
    }

    async fn call(&self, call: CoreCall) -> ToolOutput {
        let input: WriteInput = match parse(call.input.clone()) {
            Ok(input) => input,
            Err(output) => return output,
        };
        if input.path.is_empty() {
            return ToolOutput::error("invalid input: path is empty");
        }
        let open = match self.0.open(&call).await {
            Ok(open) => open,
            Err(output) => return output,
        };
        let bytes = input.content.into_bytes();
        let size = bytes.len() as u64;
        let output = match open
            .client
            .write_file(&open.record.id, &input.path, bytes)
            .await
        {
            Ok(()) => ok(format!("Wrote {} to {}.", format_bytes(size), input.path)),
            Err(error) => super::plugin_error(&error),
        };
        open.finish(output)
    }
}

#[async_trait]
impl CoreTool for EditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: EDIT.to_owned(),
            description: "Replace exact text in a file in this conversation's workspace. \
                          `old_string` must match the file exactly, including whitespace and \
                          line breaks, and must appear once: include enough surrounding lines to \
                          make it unique, or set `replace_all` to change every occurrence. The \
                          error says how many matches there were. Read the file first so you \
                          know its exact text."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The file: absolute, or relative to the working directory.",
                    },
                    "old_string": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The exact text to replace.",
                    },
                    "new_string": {
                        "type": "string",
                        "description": "What to put in its place. Must differ from old_string.",
                    },
                    "replace_all": {
                        "type": "boolean",
                        "default": false,
                        "description": "Replace every occurrence of old_string, not only a \
                                        unique one.",
                    },
                },
                "required": ["path", "old_string", "new_string"],
                "additionalProperties": false,
            }),
        }
    }

    async fn call(&self, call: CoreCall) -> ToolOutput {
        let input: EditInput = match parse(call.input.clone()) {
            Ok(input) => input,
            Err(output) => return output,
        };
        if input.old_string.is_empty() {
            return ToolOutput::error("invalid input: old_string is empty");
        }
        if input.old_string == input.new_string {
            return ToolOutput::error("invalid input: old_string and new_string are the same");
        }
        let open = match self.0.open(&call).await {
            Ok(open) => open,
            Err(output) => return output,
        };
        let id = &open.record.id;
        let file = match open.client.read_file(id, &input.path, EDIT_MAX_BYTES).await {
            Ok(file) => file,
            Err(error) => return open.plugin_error(&error),
        };
        if file.size > file.content.len() as u64 {
            let output = ToolOutput::error(format!(
                "{} is {}, too large for edit (the limit is {}). Use bash, for example `sed`.",
                input.path,
                format_bytes(file.size),
                format_bytes(EDIT_MAX_BYTES)
            ));
            return open.finish(output);
        }
        let Text::Text(text) = decode(&file.content, false) else {
            let output = ToolOutput::error(format!(
                "{} is not a text file, so edit can't change it. Use bash.",
                input.path
            ));
            return open.finish(output);
        };
        let matches = text.matches(input.old_string.as_str()).count();
        if matches == 0 {
            let output = ToolOutput::error(format!(
                "old_string was not found in {} (0 matches). It must match the file's text \
                 exactly, including whitespace; read the file to check.",
                input.path
            ));
            return open.finish(output);
        }
        if matches > 1 && !input.replace_all {
            let output = ToolOutput::error(format!(
                "old_string matches {matches} places in {}. Add surrounding lines to make it \
                 unique, or set replace_all to change all {matches}.",
                input.path
            ));
            return open.finish(output);
        }
        let changed = text.replace(input.old_string.as_str(), &input.new_string);
        let output = match open
            .client
            .write_file(id, &input.path, changed.into_bytes())
            .await
        {
            Ok(()) => ok(format!(
                "Replaced {matches} occurrence{} in {}.",
                if matches == 1 { "" } else { "s" },
                input.path
            )),
            Err(error) => super::plugin_error(&error),
        };
        open.finish(output)
    }
}

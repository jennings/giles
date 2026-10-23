//! The `load_skill` core tool (PLAN section 6.6): a skill's instructions, and its resource files
//! handed over like any other media.
//!
//! - `load_skill(name)` answers with the body of `SKILL.md` and the list of the skill's resource
//!   files ([`list_resources`]).
//! - `load_skill(name, file)` reads one of those files ([`read_resource`]: it must stay inside
//!   the skill's directory), stores it in the blob store, and answers with its text (when it's
//!   UTF-8 and at most [`MAX_INLINE_TEXT_BYTES`]) and the file as media, with its reference, so
//!   the model can pass it to a tool as `{"$media": "<id>"}` or send it with `send_attachment`.
//!
//! Skills come from the session's prompt ([`TurnContext::skills`](super::TurnContext)): the model
//! can load exactly the skills it was shown. It only reads (storing a blob is idempotent), so it's
//! parallel-safe ([`PARALLEL_SAFE_CORE_TOOLS`](crate::agent_loop::PARALLEL_SAFE_CORE_TOOLS)).

use std::fmt::Write as _;

use async_trait::async_trait;
use giles_store::{BlobMeta, BlobStore};
use giles_types::{Block, Timestamp, ToolSpec};
use serde::Deserialize;
use serde_json::json;
use tracing::warn;

use super::{CoreCall, CoreTool};
use crate::agent_loop::ToolOutput;
use crate::media;
use crate::skills::{MAX_RESOURCE_BYTES, list_resources, read_resource};

pub use crate::agent_loop::prompt::LOAD_SKILL;

/// The largest text file whose contents the result shows; larger ones are only attached.
pub const MAX_INLINE_TEXT_BYTES: usize = 64 * 1024;

/// The `load_skill` tool.
#[derive(Debug, Clone)]
pub struct LoadSkillTool {
    blobs: BlobStore,
}

impl LoadSkillTool {
    /// Stores the skill files it hands over in `blobs`.
    pub fn new(blobs: BlobStore) -> Self {
        Self { blobs }
    }
}

/// The tool's input.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    name: String,
    file: Option<String>,
}

#[async_trait]
impl CoreTool for LoadSkillTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: LOAD_SKILL.to_owned(),
            description: "Load a skill's instructions, by the name the system prompt lists. The \
                          answer ends with the skill's files; pass `file` to read one (it's also \
                          attached as media, for tools that take `{\"$media\": \"<id>\"}`)."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "The skill's name."},
                    "file": {
                        "type": "string",
                        "description": "A file of the skill, as its file list shows it, e.g. \
                                        `scripts/run.py`.",
                    },
                },
                "required": ["name"],
                "additionalProperties": false,
            }),
        }
    }

    async fn call(&self, call: CoreCall) -> ToolOutput {
        let input: Input = match serde_json::from_value(call.input) {
            Ok(input) => input,
            Err(error) => return ToolOutput::error(format!("invalid input: {error}")),
        };
        let Some(skill) = call.turn.skills.get(&input.name) else {
            return ToolOutput::error(format!("unknown skill `{}`", input.name));
        };
        let dir = skill.dir.clone();
        match input.file {
            None => {
                let listed = tokio::task::spawn_blocking(move || list_resources(&dir)).await;
                let files = match listed {
                    Ok(Ok(list)) => Some(list),
                    Ok(Err(error)) => {
                        warn!(skill = %skill.name, %error, "cannot list the skill's files");
                        None
                    }
                    Err(error) => {
                        warn!(skill = %skill.name, %error, "listing the skill's files failed");
                        None
                    }
                };
                ToolOutput {
                    content: vec![Block::text(body_text(&skill.body, files.as_ref()))],
                    is_error: false,
                }
            }
            Some(file) => self.file(&skill.name, dir, file).await,
        }
    }
}

impl LoadSkillTool {
    /// Resource `requested` of the skill `name` in `dir`, stored as a blob.
    async fn file(&self, name: &str, dir: std::path::PathBuf, requested: String) -> ToolOutput {
        let read = tokio::task::spawn_blocking(move || read_resource(&dir, &requested)).await;
        let resource = match read {
            Ok(Ok(resource)) => resource,
            Ok(Err(error)) => return ToolOutput::error(error.to_string()),
            Err(error) => return ToolOutput::error(format!("reading the file failed: {error}")),
        };
        let media_type = media::media_type_for(resource.name(), &resource.bytes);
        let meta = BlobMeta {
            media_type: media_type.to_owned(),
            name: Some(resource.name().to_owned()),
        };
        let stored = self
            .blobs
            .put(
                &resource.bytes[..],
                MAX_RESOURCE_BYTES,
                meta,
                Timestamp::now(),
            )
            .await;
        let stored = match stored {
            Ok(stored) => stored,
            Err(error) => {
                warn!(skill = %name, file = %resource.path, %error, "cannot store a skill file");
                return ToolOutput::error(format!("cannot store `{}`", resource.path));
            }
        };
        let size = resource.bytes.len();
        let text = std::str::from_utf8(&resource.bytes)
            .ok()
            .filter(|text| text.len() <= MAX_INLINE_TEXT_BYTES && !text.contains('\0'));
        let heading = format!("`{}` ({media_type}, {size} bytes)", resource.path);
        let mut content = vec![Block::text(match text {
            Some(text) => format!("{heading}:\n\n{text}"),
            None => format!("{heading} is attached."),
        })];
        content.extend(media::attachment(stored));
        ToolOutput {
            content,
            is_error: false,
        }
    }
}

/// The skill's body, then its files (if any), one per line.
fn body_text(body: &str, files: Option<&crate::skills::ResourceList>) -> String {
    let mut text = body.to_owned();
    if let Some(list) = files.filter(|list| !list.files.is_empty()) {
        text.push_str("\n\nFiles of this skill (read one with `load_skill` and `file`):\n");
        for file in &list.files {
            let _ = writeln!(text, "- {file}");
        }
        if list.truncated {
            text.push_str("- (more files not listed)\n");
        }
        text.truncate(text.trim_end().len());
    }
    text
}

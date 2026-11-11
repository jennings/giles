//! File access: `read_file`, `write_file`, `find`, and `grep`, all run by the helper in the
//! container. A relative path is relative to `/workspace`, because every exec starts there.

use std::time::Duration;

use serde_json::Value;

use crate::error::{Error, Result};
use crate::exec::Collected;
use crate::types::{FindRequest, FindResult, GrepMatch, GrepRequest, GrepResult, ReadFile};
use crate::{DockerEnvironment, check_workspace_id};

/// How long a helper call may take. Long enough for a search of a big tree.
const HELPER_TIMEOUT: Duration = Duration::from_secs(60);

/// Room for the helper's `{"size":N}` line ahead of a file's bytes.
const READ_HEADER_ROOM: u64 = 128;

/// The helper's exit status for a path that doesn't exist.
const EXIT_NOT_FOUND: i32 = 66;

impl DockerEnvironment {
    /// Reads up to `max_bytes` from the start of the file at `path`.
    pub async fn read_file(&self, id: &str, path: &str, max_bytes: u64) -> Result<ReadFile> {
        check_workspace_id(id)?;
        let path = file_arg(path)?;
        let container = self.running_container(id).await?;
        let max = max_bytes.to_string();
        let out = self
            .run_helper(
                container,
                &["read", "--max", &max, &path],
                None,
                HELPER_TIMEOUT,
                max_bytes.saturating_add(READ_HEADER_ROOM),
            )
            .await?;
        let out = succeeded(out, &format!("reading {path}"))?;
        let newline = out
            .stdout
            .iter()
            .position(|b| *b == b'\n')
            .ok_or_else(|| Error::other("the helper's reply to read had no header"))?;
        let header: Value = serde_json::from_slice(&out.stdout[..newline])
            .map_err(|e| Error::other(format!("the helper's read header isn't JSON: {e}")))?;
        let size = header
            .get("size")
            .and_then(Value::as_u64)
            .ok_or_else(|| Error::other("the helper's read header has no size"))?;
        let mut content = out.stdout;
        content.drain(..=newline);
        // The helper stops at `max`; this is only a guard against a misbehaving one.
        content.truncate(usize::try_from(max_bytes).unwrap_or(usize::MAX));
        Ok(ReadFile { content, size })
    }

    /// Writes `content` to the file at `path`, creating missing parent directories. An existing
    /// file keeps its permissions.
    pub async fn write_file(&self, id: &str, path: &str, content: Vec<u8>) -> Result<()> {
        check_workspace_id(id)?;
        let path = file_arg(path)?;
        let container = self.running_container(id).await?;
        let out = self
            .run_helper(
                container,
                &["write", &path],
                Some(content),
                HELPER_TIMEOUT,
                64 * 1024,
            )
            .await?;
        succeeded(out, &format!("writing {path}")).map(|_| ())
    }

    /// Lists files whose path under the searched directory matches a glob.
    pub async fn find(&self, id: &str, request: FindRequest) -> Result<FindResult> {
        check_workspace_id(id)?;
        let limit = request.limit.to_string();
        let mut args = vec!["find", "--pattern", &request.pattern, "--limit", &limit];
        let path;
        if let Some(dir) = request.path.as_deref().filter(|p| !p.is_empty()) {
            path = file_arg(dir)?;
            args.extend(["--path", &path]);
        }
        let container = self.running_container(id).await?;
        let out = self
            .run_helper(container, &args, None, HELPER_TIMEOUT, SEARCH_OUTPUT_LIMIT)
            .await?;
        let (lines, truncated) = json_lines(succeeded(out, "searching for files")?)?;
        let paths = lines
            .iter()
            .map(|line| text_field(line, "path"))
            .collect::<Result<Vec<_>>>()?;
        Ok(FindResult { paths, truncated })
    }

    /// Searches file contents with a regular expression.
    pub async fn grep(&self, id: &str, request: GrepRequest) -> Result<GrepResult> {
        check_workspace_id(id)?;
        let limit = request.limit.to_string();
        let mut args = vec!["grep", "--pattern", &request.pattern, "--limit", &limit];
        let path;
        if let Some(target) = request.path.as_deref().filter(|p| !p.is_empty()) {
            path = file_arg(target)?;
            args.extend(["--path", &path]);
        }
        if let Some(glob) = request.glob.as_deref() {
            args.extend(["--glob", glob]);
        }
        if request.case_insensitive {
            args.push("--ignore-case");
        }
        let container = self.running_container(id).await?;
        let out = self
            .run_helper(container, &args, None, HELPER_TIMEOUT, SEARCH_OUTPUT_LIMIT)
            .await?;
        let (lines, truncated) = json_lines(succeeded(out, "searching file contents")?)?;
        let matches = lines
            .iter()
            .map(|line| {
                Ok(GrepMatch {
                    path: text_field(line, "path")?,
                    line: line
                        .get("line")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| Error::other("a grep result has no line number"))?,
                    text: text_field(line, "text")?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(GrepResult { matches, truncated })
    }
}

/// The most output a find or grep may produce: `limit` lines of text, which the helper bounds, but
/// a single line can be long.
const SEARCH_OUTPUT_LIMIT: u64 = 64 * 1024 * 1024;

/// A path as a helper argument: not empty, and never mistaken for a flag.
fn file_arg(path: &str) -> Result<String> {
    if path.is_empty() {
        return Err(Error::invalid("a path can't be empty"));
    }
    if path.contains('\0') {
        return Err(Error::invalid("a path can't contain a NUL byte"));
    }
    Ok(if path.starts_with('-') {
        format!("./{path}")
    } else {
        path.to_owned()
    })
}

/// Turns the helper's failures into errors: 66 is a missing path, 1 a refused request (the
/// message is the helper's), and anything else, including a timeout, is unexpected.
fn succeeded(out: Collected, what: &str) -> Result<Collected> {
    if out.timed_out {
        return Err(Error::Unavailable(format!("{what} timed out")));
    }
    if out.code == 0 {
        return Ok(out);
    }
    let message = String::from_utf8_lossy(&out.stderr).trim().to_owned();
    let message = message
        .strip_prefix("giles-workspace-helper: ")
        .unwrap_or(&message)
        .to_owned();
    match out.code {
        EXIT_NOT_FOUND => Err(Error::not_found(message)),
        1 => Err(Error::invalid(message)),
        code => Err(Error::other(format!(
            "{what} failed (the helper exited {code}): {message}"
        ))),
    }
}

/// Splits the helper's JSON lines into the entries and the final `{"truncated":bool}`.
fn json_lines(out: Collected) -> Result<(Vec<Value>, bool)> {
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            serde_json::from_str::<Value>(l).map_err(|e| {
                Error::other(format!("the helper printed a line that isn't JSON: {e}"))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let truncated = lines
        .pop()
        .and_then(|last| last.get("truncated").and_then(Value::as_bool))
        .ok_or_else(|| Error::other("the helper's reply was cut short"))?;
    Ok((lines, truncated))
}

fn text_field(line: &Value, name: &str) -> Result<String> {
    line.get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| Error::other(format!("a helper result has no {name}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collected(code: i32, stdout: &str, stderr: &str) -> Collected {
        Collected {
            code,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
            timed_out: false,
            dropped: 0,
        }
    }

    #[test]
    fn paths_that_look_like_flags_get_a_dot_slash() {
        assert_eq!(file_arg("src/a.rs").unwrap(), "src/a.rs");
        assert_eq!(file_arg("/abs").unwrap(), "/abs");
        assert_eq!(file_arg("-rf").unwrap(), "./-rf");
        assert_eq!(file_arg("--max").unwrap(), "./--max");
        assert!(file_arg("").is_err());
        assert!(file_arg("a\0b").is_err());
    }

    #[test]
    fn helper_failures_map_to_error_kinds() {
        let missing = succeeded(
            collected(
                66,
                "",
                "giles-workspace-helper: nope: No such file or directory\n",
            ),
            "x",
        );
        assert!(
            matches!(missing, Err(Error::NotFound(m)) if m == "nope: No such file or directory")
        );
        let refused = succeeded(
            collected(1, "", "giles-workspace-helper: invalid glob\n"),
            "x",
        );
        assert!(matches!(refused, Err(Error::InvalidRequest(m)) if m == "invalid glob"));
        let odd = succeeded(collected(64, "", "usage"), "reading a");
        assert!(matches!(odd, Err(Error::Other(_))));
        let mut slow = collected(137, "", "");
        slow.timed_out = true;
        assert!(
            matches!(succeeded(slow, "searching"), Err(Error::Unavailable(m)) if m.contains("timed out"))
        );
        assert!(succeeded(collected(0, "", ""), "x").is_ok());
    }

    #[test]
    fn json_lines_end_with_the_truncated_flag() {
        let (lines, truncated) = json_lines(collected(
            0,
            "{\"path\":\"a\"}\n{\"path\":\"b\"}\n{\"truncated\":true}\n",
            "",
        ))
        .unwrap();
        assert_eq!(lines.len(), 2);
        assert!(truncated);
        assert!(json_lines(collected(0, "{\"path\":\"a\"}\n", "")).is_err());
        assert!(json_lines(collected(0, "", "")).is_err());
        assert!(json_lines(collected(0, "not json\n", "")).is_err());
    }
}

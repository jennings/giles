//! `find`: list files whose relative path matches a glob.

use std::ffi::OsStr;
use std::io::Write;

use serde_json::json;

use crate::{Failure, Outcome, walk};

/// Prints `{"path":...}` lines for the first `limit` matches in sorted order, then
/// `{"truncated":bool}`.
pub fn run(pattern: &str, path: Option<&OsStr>, limit: u64) -> Outcome<()> {
    let matcher = walk::glob(pattern)?;
    let root = walk::root_or_cwd(path);
    let matches: Vec<String> = walk::files(&root)?
        .into_iter()
        .filter(|p| matcher.is_match(p))
        .collect();
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    let mut out = std::io::stdout().lock();
    for found in matches.iter().take(limit) {
        writeln!(out, "{}", json!({ "path": found })).map_err(write_failed)?;
    }
    writeln!(out, "{}", json!({ "truncated": matches.len() > limit })).map_err(write_failed)?;
    Ok(())
}

pub fn write_failed(error: std::io::Error) -> Failure {
    Failure::other(format!("writing output: {error}"))
}

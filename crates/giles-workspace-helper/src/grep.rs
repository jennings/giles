//! `grep`: search file contents with a regular expression.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use regex::bytes::RegexBuilder;
use serde_json::json;

use crate::find::write_failed;
use crate::{Failure, Outcome, walk};

/// How much of a file's start is checked for NUL bytes, which mark a binary file.
const BINARY_SNIFF: usize = 8 * 1024;

pub struct Request<'a> {
    pub pattern: &'a str,
    pub path: Option<&'a OsStr>,
    pub glob: Option<&'a str>,
    pub ignore_case: bool,
    pub limit: u64,
}

/// Prints `{"path":...,"line":N,"text":...}` lines for the first `limit` matching lines, in path
/// order and then line order, then `{"truncated":bool}`.
pub fn run(request: &Request<'_>) -> Outcome<()> {
    let regex = RegexBuilder::new(request.pattern)
        .case_insensitive(request.ignore_case)
        .build()
        .map_err(|e| Failure::other(format!("invalid regular expression: {e}")))?;
    let glob = request.glob.map(walk::glob).transpose()?;
    let root = walk::root_or_cwd(request.path);
    let limit = usize::try_from(request.limit).unwrap_or(usize::MAX);

    // A single file is searched as itself and reported under its own name.
    let metadata = std::fs::metadata(&root).map_err(|e| Failure::io(&root, &e))?;
    let (base, relative): (PathBuf, Vec<String>) = if metadata.is_dir() {
        (root.clone(), walk::files(&root)?)
    } else {
        let name = root
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let parent = root.parent().map_or_else(PathBuf::new, Path::to_path_buf);
        (parent, vec![name])
    };

    let mut out = std::io::stdout().lock();
    let mut count = 0usize;
    let mut truncated = false;
    'files: for rel in relative {
        if glob.as_ref().is_some_and(|g| !g.is_match(&rel)) {
            continue;
        }
        let Ok(file) = File::open(base.join(&rel)) else {
            continue;
        };
        let mut reader = BufReader::new(file);
        match reader.fill_buf() {
            Ok(head) if head[..head.len().min(BINARY_SNIFF)].contains(&0) => continue,
            Ok(_) => {}
            Err(_) => continue,
        }
        let mut line = Vec::new();
        let mut number = 0u64;
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            number += 1;
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if !regex.is_match(&line) {
                continue;
            }
            if count == limit {
                truncated = true;
                break 'files;
            }
            count += 1;
            writeln!(
                out,
                "{}",
                json!({ "path": rel, "line": number, "text": String::from_utf8_lossy(&line) })
            )
            .map_err(write_failed)?;
        }
    }
    writeln!(out, "{}", json!({ "truncated": truncated })).map_err(write_failed)?;
    Ok(())
}

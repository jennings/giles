//! The directory walk `find` and `grep` share.

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

use crate::{Failure, Outcome};

/// The default search root: the working directory.
pub fn root_or_cwd(path: Option<&std::ffi::OsStr>) -> PathBuf {
    match path {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from("."),
    }
}

/// Every non-directory under `root`, as paths relative to it with `/` separators, sorted.
///
/// Skips `.git` directories and files that `.gitignore` files (in `root`, below it, and in its
/// parents) exclude, whether or not a `.git` directory exists. Hidden files are included.
pub fn files(root: &Path) -> Outcome<Vec<String>> {
    let metadata = std::fs::metadata(root).map_err(|e| Failure::io(root, &e))?;
    if !metadata.is_dir() {
        return Err(Failure::other(format!(
            "{}: not a directory",
            root.display()
        )));
    }
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .ignore(false)
        .git_global(false)
        .require_git(false)
        .filter_entry(|entry| entry.depth() == 0 || entry.file_name() != ".git")
        .build();
    let mut paths = Vec::new();
    for entry in walker {
        // An unreadable directory or a broken ignore file shouldn't hide everything else.
        let Ok(entry) = entry else { continue };
        if entry.depth() == 0 || entry.file_type().is_none_or(|t| t.is_dir()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        paths.push(
            relative
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/"),
        );
    }
    paths.sort_unstable();
    Ok(paths)
}

/// A matcher for a glob over `/`-separated relative paths. `*` stays within a path component and
/// `**` crosses directories.
pub fn glob(pattern: &str) -> Outcome<globset::GlobMatcher> {
    globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|g| g.compile_matcher())
        .map_err(|e| Failure::other(format!("invalid glob {pattern:?}: {}", e.kind())))
}

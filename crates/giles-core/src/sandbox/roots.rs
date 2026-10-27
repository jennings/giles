//! Resolving grants without following links the command planted, shared by the backends.
//!
//! Each backend resolves every grant in one walk of its own (`macos/paths.rs`,
//! `linux/paths.rs`) that follows no link at or below a "stop", a writable root where the
//! command can create links. This module has what the walks share: how a path splits into
//! components, the link limit, and [`writable_roots`], which finds the stops themselves. See
//! "Writable roots and links" in `Docs/technical/sandbox.md`.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use super::SandboxError;

/// How many links one walk follows before giving up, like the kernel's `MAXSYMLINKS`. A loop
/// never resolves, so whatever it guards fails closed.
pub(super) const MAX_LINKS: usize = 32;

/// The components to walk: names and `..`.
pub(super) fn names(path: &Path) -> VecDeque<OsString> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_owned()),
            Component::ParentDir => Some(OsString::from("..")),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => None,
        })
        .collect()
}

/// What [`writable_roots`] needs from one backend's walk of a path.
pub(super) trait Walk {
    /// The real path the walk ended at (missing components appended as spelled).
    fn real(&self) -> &Path;
    /// Where the links the walk followed are, as real paths.
    fn links(&self) -> &[PathBuf];
}

/// Resolves the writable roots with `resolve(path, stops)` so that none of them follows a link
/// strictly inside any of them.
///
/// The command can create links anywhere strictly inside a root (not the root's own entry,
/// which the backends protect), so a root nested in another root (a `write` grant inside the
/// data directory, say) must not follow links there. Which roots contain which is only known
/// once they're resolved, so this resolves them all, and while some root's walk followed a link
/// strictly inside another root, resolves them again with that root as a stop. Every round adds
/// a stop, so there are at most as many rounds as roots; a set that still doesn't settle (links
/// changing under it) is refused.
pub(super) fn writable_roots<W: Walk>(
    roots: &[&Path],
    resolve: impl Fn(&Path, &[PathBuf]) -> W,
) -> Result<Vec<W>, SandboxError> {
    let mut stops: Vec<PathBuf> = Vec::new();
    let mut planted: Option<(PathBuf, PathBuf)> = None;
    for _ in 0..=roots.len() {
        let resolved: Vec<W> = roots.iter().map(|root| resolve(root, &stops)).collect();
        let inside = |root: &W| {
            resolved.iter().zip(roots).find_map(|(other, path)| {
                other
                    .links()
                    .iter()
                    .find(|link| link.as_path() != root.real() && link.starts_with(root.real()))
                    .map(|link| (path.to_path_buf(), link.clone()))
            })
        };
        let mut new_stops = Vec::new();
        for root in &resolved {
            if let Some(found) = inside(root) {
                planted = Some(found);
                new_stops.push(root.real().to_owned());
            }
        }
        if new_stops.is_empty() {
            return Ok(resolved);
        }
        stops.extend(new_stops);
    }
    let (path, link) = planted.unwrap_or_default();
    Err(SandboxError::WritableLink { path, link })
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::BTreeMap;

    use super::*;

    /// A walk result from a fake filesystem.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Fake {
        real: PathBuf,
        links: Vec<PathBuf>,
    }

    impl Walk for Fake {
        fn real(&self) -> &Path {
            &self.real
        }
        fn links(&self) -> &[PathBuf] {
            &self.links
        }
    }

    /// A fake resolver with links at `links` (location -> target, both real and absolute),
    /// following none at or below a stop.
    fn resolver(links: &BTreeMap<&str, &str>) -> impl Fn(&Path, &[PathBuf]) -> Fake {
        move |path, stops| {
            let mut real = PathBuf::from("/");
            let mut followed = Vec::new();
            let mut pending = names(path);
            while let Some(name) = pending.pop_front() {
                let below_stop = stops.iter().any(|stop| real.starts_with(stop));
                let next = real.join(&name);
                match links.get(next.to_str().unwrap()) {
                    Some(target) if !below_stop => {
                        followed.push(next);
                        real = PathBuf::from("/");
                        for name in names(Path::new(target)).into_iter().rev() {
                            pending.push_front(name);
                        }
                    }
                    _ => real = next,
                }
            }
            Fake {
                real,
                links: followed,
            }
        }
    }

    #[test]
    fn components_are_names_and_parents() {
        assert_eq!(names(Path::new("/a/./b/../c/")), ["a", "b", "..", "c"]);
        assert!(names(Path::new("/")).is_empty());
    }

    #[test]
    fn a_root_nested_in_another_does_not_follow_its_links() {
        // `/data/nested` is a link the command planted; `/tmp` is the operator's.
        let links = BTreeMap::from([("/data/nested", "/home"), ("/tmp", "/private/tmp")]);
        let resolve = resolver(&links);
        let roots = writable_roots(
            &[
                Path::new("/data"),
                Path::new("/data/nested/x"),
                Path::new("/tmp/w"),
            ],
            &resolve,
        )
        .unwrap();
        let reals: Vec<_> = roots.iter().map(|r| r.real.clone()).collect();
        assert_eq!(
            reals,
            [
                PathBuf::from("/data"),
                PathBuf::from("/data/nested/x"),
                PathBuf::from("/private/tmp/w")
            ]
        );
        // Without the data directory as a root, the link is followed.
        assert_eq!(
            resolve(Path::new("/data/nested/x"), &[]).real,
            Path::new("/home/x")
        );
    }

    #[test]
    fn roots_that_never_settle_are_refused() {
        // Each round resolves differently, as if a racing child kept flipping links.
        let round = Cell::new(0);
        let resolve = |path: &Path, _: &[PathBuf]| {
            round.set(round.get() + 1);
            Fake {
                real: path.to_owned(),
                links: vec![PathBuf::from(format!("/data/link{}", round.get()))],
            }
        };
        let err = writable_roots(&[Path::new("/data")], resolve).unwrap_err();
        assert!(
            matches!(&err, SandboxError::WritableLink { path, .. } if path == Path::new("/data")),
            "{err}"
        );
    }
}

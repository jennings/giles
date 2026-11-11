//! Image references and the allowlist that decides which images a workspace may start from.
//!
//! References normalize the way Docker's do: the registry defaults to `docker.io`, a
//! single-component Docker Hub name is under `library/`, the first component is a registry only if
//! it contains `.` or `:` or is `localhost`, and the tag defaults to `latest`. Allowlist patterns
//! use the same syntax, `[registry/]image[:tag]`, and a tag may be `*` to match any tag.

use std::fmt;

/// The registry Docker uses when a reference names none.
pub const DOCKER_HUB: &str = "docker.io";

/// The patterns allowed when `default_images` is on.
pub const DEFAULT_PATTERNS: &[&str] = &[
    "ubuntu:*",
    "debian:*",
    "alpine:*",
    "python:*",
    "node:*",
    "golang:*",
    "rust:*",
    "ruby:*",
    "eclipse-temurin:*",
];

/// Why a reference or pattern was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ParseError(String);

fn error<T>(message: impl Into<String>) -> Result<T, ParseError> {
    Err(ParseError(message.into()))
}

/// A normalized image reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// The registry host, e.g. `docker.io` or `ghcr.io` or `localhost:5000`.
    pub registry: String,
    /// The repository path, e.g. `library/ubuntu`.
    pub repository: String,
    /// The tag. `latest` when the reference gave none and no digest. Kept alongside a digest if
    /// the reference had both.
    pub tag: Option<String>,
    /// The content digest, e.g. `sha256:...`.
    pub digest: Option<String>,
}

impl Reference {
    /// Parses a concrete image reference (no wildcard tag).
    pub fn parse(text: &str) -> Result<Self, ParseError> {
        let parsed = parse(text)?;
        if parsed.tag.as_deref() == Some("*") {
            return error(format!("{text:?}: a concrete image can't have a `*` tag"));
        }
        Ok(parsed)
    }

    /// The repository with its registry, e.g. `docker.io/library/ubuntu`.
    pub fn name(&self) -> String {
        format!("{}/{}", self.registry, self.repository)
    }

    /// Whether the reference pins a digest.
    pub fn is_pinned(&self) -> bool {
        self.digest.is_some()
    }
}

impl fmt::Display for Reference {
    /// The full normalized form, e.g. `docker.io/library/ubuntu:latest` or
    /// `docker.io/library/ubuntu@sha256:...`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.registry, self.repository)?;
        match (&self.digest, &self.tag) {
            (Some(digest), _) => write!(f, "@{digest}"),
            (None, Some(tag)) => write!(f, ":{tag}"),
            (None, None) => Ok(()),
        }
    }
}

fn parse(text: &str) -> Result<Reference, ParseError> {
    if text.is_empty() {
        return error("an image reference can't be empty");
    }
    if text.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return error(format!(
            "{text:?}: an image reference can't contain whitespace"
        ));
    }
    let (name_and_tag, digest) = match text.split_once('@') {
        Some((name, digest)) => (name, Some(parse_digest(text, digest)?)),
        None => (text, None),
    };
    // A ':' after the last '/' starts the tag; one before it ends a registry's port.
    let last_slash = name_and_tag.rfind('/');
    let (name, tag) = match name_and_tag.rfind(':') {
        Some(at) if last_slash.is_none_or(|slash| at > slash) => {
            (&name_and_tag[..at], Some(&name_and_tag[at + 1..]))
        }
        _ => (name_and_tag, None),
    };
    if let Some(tag) = tag {
        validate_tag(text, tag)?;
    }

    let mut components: Vec<&str> = name.split('/').collect();
    let registry = if components.len() > 1 && looks_like_registry(components[0]) {
        normalize_registry(components.remove(0))?
    } else {
        DOCKER_HUB.to_owned()
    };
    if registry == DOCKER_HUB && components.len() == 1 {
        components.insert(0, "library");
    }
    for component in &components {
        validate_path_component(text, component)?;
    }
    let tag = match (tag, &digest) {
        (Some(tag), _) => Some(tag.to_owned()),
        (None, None) => Some("latest".to_owned()),
        (None, Some(_)) => None,
    };
    Ok(Reference {
        registry,
        repository: components.join("/"),
        tag,
        digest,
    })
}

/// The first component of a reference is a registry only if it looks like a host.
fn looks_like_registry(component: &str) -> bool {
    component.contains('.') || component.contains(':') || component == "localhost"
}

/// Lowercases a registry host and folds Docker Hub's alternate names into `docker.io`.
pub fn normalize_registry(host: &str) -> Result<String, ParseError> {
    let lower = host.to_ascii_lowercase();
    let valid = !lower.is_empty()
        && lower
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'))
        && !lower.starts_with(['-', '.']);
    if !valid {
        return error(format!("{host:?} isn't a valid registry host"));
    }
    Ok(match lower.as_str() {
        "index.docker.io" | "registry-1.docker.io" | "registry.hub.docker.com" => {
            DOCKER_HUB.to_owned()
        }
        _ => lower,
    })
}

fn parse_digest(whole: &str, digest: &str) -> Result<String, ParseError> {
    let valid = digest.split_once(':').is_some_and(|(algorithm, hex)| {
        !algorithm.is_empty()
            && algorithm
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'+')
            && hex.len() >= 32
            && hex.bytes().all(|b| b.is_ascii_hexdigit())
    });
    if valid {
        Ok(digest.to_owned())
    } else {
        error(format!(
            "{whole:?}: {digest:?} isn't a valid digest (expected sha256:<hex>)"
        ))
    }
}

fn validate_tag(whole: &str, tag: &str) -> Result<(), ParseError> {
    if tag == "*" {
        return Ok(());
    }
    if tag.contains('*') {
        return error(format!("{whole:?}: `*` can only be the whole tag"));
    }
    let valid = !tag.is_empty()
        && tag.len() <= 128
        && tag
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
        && tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    if valid {
        Ok(())
    } else {
        error(format!("{whole:?}: {tag:?} isn't a valid tag"))
    }
}

fn validate_path_component(whole: &str, component: &str) -> Result<(), ParseError> {
    if component.contains('*') {
        return error(format!("{whole:?}: `*` can only be the whole tag"));
    }
    let bytes = component.as_bytes();
    let edge_ok = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let valid = !bytes.is_empty()
        && edge_ok(bytes[0])
        && edge_ok(bytes[bytes.len() - 1])
        && bytes
            .iter()
            .all(|&b| edge_ok(b) || matches!(b, b'.' | b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        error(format!(
            "{whole:?}: {component:?} isn't a valid image name (lowercase letters, digits, \
             \".\", \"_\", \"-\"; starting and ending with a letter or digit)"
        ))
    }
}

/// How a pattern matches tags.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TagPattern {
    Any,
    Exact(String),
}

/// One allowlist entry: a normalized repository and a tag or `*`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pattern {
    /// As the operator wrote it, for messages.
    text: String,
    registry: String,
    repository: String,
    tag: TagPattern,
}

/// The images a workspace may start from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allowlist {
    patterns: Vec<Pattern>,
}

impl Allowlist {
    /// Builds an allowlist from pattern texts, in order. Duplicates are dropped.
    pub fn new<'a>(patterns: impl IntoIterator<Item = &'a str>) -> Result<Self, ParseError> {
        let mut parsed: Vec<Pattern> = Vec::new();
        for text in patterns {
            let pattern = parse_pattern(text)?;
            if !parsed.iter().any(|p| {
                (&p.registry, &p.repository, &p.tag)
                    == (&pattern.registry, &pattern.repository, &pattern.tag)
            }) {
                parsed.push(pattern);
            }
        }
        Ok(Self { patterns: parsed })
    }

    /// The patterns as written, in order.
    pub fn patterns(&self) -> impl Iterator<Item = &str> {
        self.patterns.iter().map(|p| p.text.as_str())
    }

    /// The patterns, comma-separated, for messages. Empty if nothing is allowed.
    pub fn describe(&self) -> String {
        if self.patterns.is_empty() {
            return "none".to_owned();
        }
        self.patterns().collect::<Vec<_>>().join(", ")
    }

    /// Whether `image` may be used. An image pinned by digest matches only a pattern whose tag is
    /// `*`, since a digest can't be compared with a tag.
    pub fn allows(&self, image: &Reference) -> bool {
        self.patterns.iter().any(|p| {
            p.registry == image.registry
                && p.repository == image.repository
                && match &p.tag {
                    TagPattern::Any => true,
                    TagPattern::Exact(tag) => {
                        !image.is_pinned() && image.tag.as_deref() == Some(tag)
                    }
                }
        })
    }
}

fn parse_pattern(text: &str) -> Result<Pattern, ParseError> {
    if text.contains('@') {
        return error(format!(
            "{text:?}: an allowed image can't pin a digest; use `name:*`"
        ));
    }
    let reference = parse(text)?;
    let tag = match reference.tag.as_deref() {
        Some("*") => TagPattern::Any,
        Some(tag) => TagPattern::Exact(tag.to_owned()),
        None => unreachable!("a reference without a digest always has a tag"),
    };
    Ok(Pattern {
        text: text.to_owned(),
        registry: reference.registry,
        repository: reference.repository,
        tag,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(text: &str) -> Reference {
        Reference::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"))
    }

    fn full(text: &str) -> String {
        reference(text).to_string()
    }

    #[test]
    fn single_component_hub_names_go_under_library() {
        assert_eq!(full("ubuntu"), "docker.io/library/ubuntu:latest");
        assert_eq!(full("ubuntu:24.04"), "docker.io/library/ubuntu:24.04");
        assert_eq!(full("docker.io/ubuntu"), "docker.io/library/ubuntu:latest");
        assert_eq!(
            full("docker.io/library/ubuntu:22.04"),
            "docker.io/library/ubuntu:22.04"
        );
    }

    #[test]
    fn multi_component_hub_names_keep_their_namespace() {
        assert_eq!(full("jennings/tools"), "docker.io/jennings/tools:latest");
        assert_eq!(full("jennings/tools:v2"), "docker.io/jennings/tools:v2");
        assert_eq!(
            full("docker.io/jennings/tools"),
            "docker.io/jennings/tools:latest"
        );
    }

    #[test]
    fn the_first_component_is_a_registry_only_if_it_looks_like_a_host() {
        assert_eq!(
            full("ghcr.io/jennings/tools"),
            "ghcr.io/jennings/tools:latest"
        );
        assert_eq!(full("localhost/tools"), "localhost/tools:latest");
        assert_eq!(full("localhost:5000/tools:1"), "localhost:5000/tools:1");
        assert_eq!(full("registry:5000/a/b"), "registry:5000/a/b:latest");
        // No dot, colon, or `localhost`: a Docker Hub namespace.
        assert_eq!(full("myorg/tools"), "docker.io/myorg/tools:latest");
        // A tag's colon isn't a registry port.
        assert_eq!(reference("myorg/tools:v1").registry, "docker.io");
    }

    #[test]
    fn a_registry_is_not_needed_to_be_lowercase_and_hub_aliases_fold() {
        assert_eq!(
            full("GHCR.io/jennings/tools"),
            "ghcr.io/jennings/tools:latest"
        );
        assert_eq!(
            full("index.docker.io/library/ubuntu"),
            "docker.io/library/ubuntu:latest"
        );
        assert_eq!(
            full("registry-1.docker.io/jennings/tools"),
            "docker.io/jennings/tools:latest"
        );
        assert_eq!(
            full("index.docker.io/ubuntu"),
            "docker.io/library/ubuntu:latest"
        );
    }

    #[test]
    fn digests_are_kept_and_replace_the_default_tag() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let pinned = reference(&format!("ubuntu@{digest}"));
        assert_eq!(pinned.digest.as_deref(), Some(digest.as_str()));
        assert_eq!(pinned.tag, None);
        assert_eq!(
            pinned.to_string(),
            format!("docker.io/library/ubuntu@{digest}")
        );
        assert!(pinned.is_pinned());
        let both = reference(&format!("ubuntu:24.04@{digest}"));
        assert_eq!(both.tag.as_deref(), Some("24.04"));
        assert_eq!(
            both.to_string(),
            format!("docker.io/library/ubuntu@{digest}")
        );
        assert_eq!(
            reference(&format!("localhost:5000/a/b@{digest}")).registry,
            "localhost:5000"
        );
    }

    #[test]
    fn name_is_the_repository_with_its_registry() {
        assert_eq!(reference("ubuntu:1").name(), "docker.io/library/ubuntu");
    }

    #[test]
    fn rejects_malformed_references() {
        for bad in [
            "",
            " ubuntu",
            "ubuntu:",
            "UPPER/case",
            "a//b",
            "/ubuntu",
            "ubuntu/",
            "-bad",
            "ubuntu:-tag",
            "ubuntu:t ag",
            "ubuntu@sha256:xyz",
            "ubuntu@nothex",
            "ubuntu@sha256:abc",
            "a/b/../c",
            "ubuntu:*",
            "ub*ntu",
            "ubuntu:2*",
        ] {
            assert!(Reference::parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    fn allow(patterns: &[&str]) -> Allowlist {
        Allowlist::new(patterns.iter().copied()).unwrap()
    }

    fn default_list() -> Allowlist {
        Allowlist::new(DEFAULT_PATTERNS.iter().copied()).unwrap()
    }

    #[test]
    fn a_star_tag_allows_any_tag_of_that_image() {
        let list = allow(&["ubuntu:*"]);
        assert!(list.allows(&reference("ubuntu")));
        assert!(list.allows(&reference("ubuntu:24.04")));
        assert!(list.allows(&reference("docker.io/library/ubuntu:rolling")));
        assert!(!list.allows(&reference("debian:12")));
        assert!(!list.allows(&reference("jennings/ubuntu:24.04")));
        assert!(!list.allows(&reference("ghcr.io/library/ubuntu:24.04")));
        assert!(!list.allows(&reference("ubuntu-extra:1")));
    }

    #[test]
    fn a_literal_tag_allows_only_that_tag_and_none_means_latest() {
        let list = allow(&["python:3.12", "alpine"]);
        assert!(list.allows(&reference("python:3.12")));
        assert!(!list.allows(&reference("python:3.13")));
        assert!(!list.allows(&reference("python")));
        assert!(list.allows(&reference("alpine")));
        assert!(list.allows(&reference("alpine:latest")));
        assert!(!list.allows(&reference("alpine:3.20")));
    }

    #[test]
    fn registries_and_namespaces_are_part_of_the_pattern() {
        let list = allow(&[
            "ghcr.io/acme/tools:*",
            "localhost:5000/dev:1",
            "jennings/tools:*",
        ]);
        assert!(list.allows(&reference("ghcr.io/acme/tools:v3")));
        assert!(!list.allows(&reference("acme/tools:v3")));
        assert!(!list.allows(&reference("ghcr.io/acme/other:v3")));
        assert!(list.allows(&reference("localhost:5000/dev:1")));
        assert!(!list.allows(&reference("localhost:5001/dev:1")));
        assert!(list.allows(&reference("docker.io/jennings/tools:x")));
    }

    #[test]
    fn patterns_normalize_like_references() {
        // `index.docker.io/library/ubuntu` is the same repository as `ubuntu`.
        let list = allow(&["index.docker.io/library/ubuntu:*"]);
        assert!(list.allows(&reference("ubuntu:24.04")));
        let list = allow(&["docker.io/ubuntu:20.04"]);
        assert!(list.allows(&reference("ubuntu:20.04")));
    }

    #[test]
    fn a_digest_matches_a_star_tag_pattern_only() {
        let digest = format!("sha256:{}", "b".repeat(64));
        let pinned = reference(&format!("ubuntu@{digest}"));
        assert!(allow(&["ubuntu:*"]).allows(&pinned));
        assert!(!allow(&["ubuntu:24.04"]).allows(&pinned));
        assert!(!allow(&["ubuntu"]).allows(&pinned));
        // Even with a tag alongside the digest, an exact-tag pattern can't vouch for the content.
        let tagged = reference(&format!("ubuntu:24.04@{digest}"));
        assert!(!allow(&["ubuntu:24.04"]).allows(&tagged));
        assert!(allow(&["ubuntu:*"]).allows(&tagged));
        assert!(!allow(&["debian:*"]).allows(&pinned));
    }

    #[test]
    fn the_default_list_covers_the_common_languages() {
        let list = default_list();
        for image in [
            "ubuntu:24.04",
            "debian:bookworm",
            "alpine:3.20",
            "python:3.12-slim",
            "node:22",
            "golang:1.23",
            "rust:1.90",
            "ruby:3.3",
            "eclipse-temurin:21-jdk",
        ] {
            assert!(list.allows(&reference(image)), "{image}");
        }
        assert!(!list.allows(&reference("busybox")));
        assert!(!list.allows(&reference("ghcr.io/evil/ubuntu:latest")));
    }

    #[test]
    fn patterns_reject_a_star_anywhere_but_the_whole_tag() {
        for bad in [
            "ub*:1",
            "ubuntu:2*",
            "ubuntu:*1",
            "*/ubuntu:1",
            "gh*.io/a/b:1",
            "*",
        ] {
            assert!(Allowlist::new([bad]).is_err(), "{bad:?} should be rejected");
        }
        let digest = format!("ubuntu@sha256:{}", "c".repeat(64));
        assert!(Allowlist::new([digest.as_str()]).is_err());
        assert!(Allowlist::new(["not a pattern"]).is_err());
    }

    #[test]
    fn describing_lists_patterns_as_written_without_duplicates() {
        let list = allow(&["ubuntu:*", "docker.io/library/ubuntu:*", "ghcr.io/a/b:1"]);
        assert_eq!(list.describe(), "ubuntu:*, ghcr.io/a/b:1");
        assert_eq!(list.patterns().count(), 2);
        assert_eq!(Allowlist::new([]).unwrap().describe(), "none");
    }
}

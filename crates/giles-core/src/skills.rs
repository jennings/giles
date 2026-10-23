//! Agent Skills (PLAN section 6.6): parsing `SKILL.md` and discovering skill directories.
//!
//! A skill is a directory holding `SKILL.md` (YAML front-matter with `name` and `description`, then
//! a Markdown body) and optional resource files, as the
//! [Agent Skills specification](https://agentskills.io/specification) describes. Front-matter
//! fields other than `name` and `description` are allowed and ignored.
//!
//! [`discover`] reads an ordered list of [`SkillSource`]s, each a directory whose subdirectories
//! are skills. A later source overrides an earlier one's skill with the same name, so an agent's
//! skills ([`agent_sources`]) override global ones. A skill that fails to load is skipped and
//! reported in [`Discovery::warnings`]; it never keeps the other skills from loading.
//!
//! Everything here does blocking file I/O; async callers run it on a blocking thread.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use giles_types::{Skill, SkillSummary};
use serde::Deserialize;

use crate::front_matter;
use crate::paths::{DataRoot, PathsError};

/// The file that makes a directory a skill.
pub const SKILL_FILE: &str = "SKILL.md";

/// The longest skill name, in characters.
pub const MAX_NAME_CHARS: usize = 64;

/// The longest skill description, in characters.
pub const MAX_DESCRIPTION_CHARS: usize = 1024;

/// Why a skill could not be loaded. Every variant names the file or directory at fault.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SkillError {
    /// A file or directory could not be read (including a skill directory without `SKILL.md`).
    #[error("cannot read {}: {error}", .path.display())]
    Io {
        /// The file or directory.
        path: PathBuf,
        /// The underlying error.
        error: io::Error,
    },
    /// A skill directory's name is not valid UTF-8, so it cannot match a skill name.
    #[error("{}: directory name is not valid UTF-8", .path.display())]
    NonUtf8DirName {
        /// The skill directory.
        path: PathBuf,
    },
    /// `SKILL.md` does not start with a front-matter block.
    #[error(
        "{}: missing YAML front-matter (a `---` line, the YAML, then another `---` line)",
        .path.display()
    )]
    NoFrontMatter {
        /// The `SKILL.md` file.
        path: PathBuf,
    },
    /// The front-matter is not valid YAML, is not a mapping, or a field has the wrong type.
    #[error("{}: invalid YAML front-matter: {message}", .path.display())]
    Yaml {
        /// The `SKILL.md` file.
        path: PathBuf,
        /// The parser's message; line numbers count from the top of the file.
        message: String,
    },
    /// A required front-matter field is absent or null.
    #[error("{}: missing required front-matter field `{field}`", .path.display())]
    MissingField {
        /// The `SKILL.md` file.
        path: PathBuf,
        /// `name` or `description`.
        field: &'static str,
    },
    /// The `name` breaks the specification's rules ([`validate_skill_name`]).
    #[error("{}: invalid skill name {name:?}: {reason}", .path.display())]
    InvalidName {
        /// The `SKILL.md` file.
        path: PathBuf,
        /// The rejected name.
        name: String,
        /// Which rule it breaks.
        reason: &'static str,
    },
    /// The `name` differs from the skill directory's name.
    #[error(
        "{}: skill name {name:?} does not match its directory name {dir_name:?}",
        .path.display()
    )]
    NameMismatch {
        /// The `SKILL.md` file.
        path: PathBuf,
        /// The name in the front-matter.
        name: String,
        /// The directory's name.
        dir_name: String,
    },
    /// The `description` is empty or too long.
    #[error("{}: invalid description: {reason}", .path.display())]
    InvalidDescription {
        /// The `SKILL.md` file.
        path: PathBuf,
        /// Which rule it breaks.
        reason: &'static str,
    },
}

/// The front-matter fields Giles reads. Others (`license`, `metadata`, `allowed-tools`, ...) are
/// ignored.
#[derive(Debug, Default, Deserialize)]
struct FrontMatter {
    name: Option<String>,
    description: Option<String>,
}

/// Checks a skill name against the Agent Skills rules: 1 to 64 characters, only lowercase
/// alphanumeric characters and hyphens, no leading, trailing, or consecutive hyphens.
///
/// Such a name never contains `__`, so a bare skill name can't look like a prefixed toolset skill
/// (`<instance>__<skill>`, PLAN section 6.5).
///
/// # Errors
///
/// The rule the name breaks, as a short phrase.
pub fn validate_skill_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("it is empty");
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err("it is longer than 64 characters");
    }
    let allowed =
        |c: char| c == '-' || (c.is_alphanumeric() && c.to_lowercase().eq(std::iter::once(c)));
    if !name.chars().all(allowed) {
        return Err("only lowercase letters, digits, and hyphens are allowed");
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err("it must not start or end with a hyphen");
    }
    if name.contains("--") {
        return Err("it must not contain consecutive hyphens");
    }
    Ok(())
}

/// Parses the contents of `dir/SKILL.md`.
///
/// The front-matter must hold a valid `name` equal to the directory's name and a non-empty
/// `description` (trimmed, at most 1,024 characters). The body is the rest of the file with line
/// endings normalized to `\n`, leading blank lines and trailing whitespace removed.
///
/// # Errors
///
/// A [`SkillError`] naming `dir/SKILL.md`.
pub fn parse_skill(dir: &Path, text: &str) -> Result<Skill, SkillError> {
    let path = || dir.join(SKILL_FILE);
    let split =
        front_matter::split(text).ok_or_else(|| SkillError::NoFrontMatter { path: path() })?;
    let fields: FrontMatter = if split.yaml.trim().is_empty() {
        FrontMatter::default()
    } else {
        // Without snippets the message is one line, fit for a log warning.
        let mut options = serde_saphyr::Options::default();
        options.with_snippet = false;
        serde_saphyr::from_str_with_options(split.document, options).map_err(|e| {
            SkillError::Yaml {
                path: path(),
                message: e.to_string(),
            }
        })?
    };

    let name = fields.name.ok_or_else(|| SkillError::MissingField {
        path: path(),
        field: "name",
    })?;
    validate_skill_name(&name).map_err(|reason| SkillError::InvalidName {
        path: path(),
        name: name.clone(),
        reason,
    })?;
    let dir_name = dir_name(dir)?;
    if dir_name != name {
        return Err(SkillError::NameMismatch {
            path: path(),
            name,
            dir_name: dir_name.to_owned(),
        });
    }

    let description = fields.description.ok_or_else(|| SkillError::MissingField {
        path: path(),
        field: "description",
    })?;
    let description = description.trim();
    if description.is_empty() {
        return Err(SkillError::InvalidDescription {
            path: path(),
            reason: "it is empty",
        });
    }
    if description.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(SkillError::InvalidDescription {
            path: path(),
            reason: "it is longer than 1,024 characters",
        });
    }

    let body = split.body.replace("\r\n", "\n");
    Ok(Skill {
        name,
        description: description.to_owned(),
        body: body.trim_start_matches('\n').trim_end().to_owned(),
        dir: dir.to_owned(),
    })
}

/// Reads and parses `dir/SKILL.md` ([`parse_skill`]).
///
/// # Errors
///
/// [`SkillError::Io`] if the file can't be read or isn't UTF-8, else as [`parse_skill`].
pub fn load_skill(dir: &Path) -> Result<Skill, SkillError> {
    let path = dir.join(SKILL_FILE);
    let text = fs::read_to_string(&path).map_err(|error| SkillError::Io { path, error })?;
    parse_skill(dir, &text)
}

/// The directory's last component as UTF-8.
fn dir_name(dir: &Path) -> Result<&str, SkillError> {
    dir.file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| SkillError::NonUtf8DirName {
            path: dir.to_owned(),
        })
}

/// A directory whose subdirectories are skills.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillSource {
    /// The directory, e.g. `<root>/skills/`. A missing directory holds no skills.
    pub dir: PathBuf,
    /// When set, each skill from this source is named `<prefix>__<name>`: the toolset instance
    /// that lists the directory (PLAN sections 6.5 and 7.10). `None` for local skills.
    pub prefix: Option<String>,
}

impl SkillSource {
    /// A source of bare-named skills.
    pub fn local(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            prefix: None,
        }
    }
}

/// The local skill sources for `agent`, lowest precedence first: `<root>/skills/`, then
/// `agents/<agent>/skills/`.
///
/// # Errors
///
/// [`PathsError::InvalidName`] if `agent` is not a valid name.
pub fn agent_sources(root: &DataRoot, agent: &str) -> Result<Vec<SkillSource>, PathsError> {
    Ok(vec![
        SkillSource::local(root.skills_dir()),
        SkillSource::local(root.agent_dir(agent)?.join("skills")),
    ])
}

/// The skills available to an agent, by name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillSet {
    skills: BTreeMap<String, Skill>,
}

impl SkillSet {
    /// The skill with this (possibly prefixed) name.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    /// Every skill, sorted by name.
    pub fn iter(&self) -> impl Iterator<Item = &Skill> {
        self.skills.values()
    }

    /// Every skill's name and description, sorted by name, for the system prompt.
    pub fn summaries(&self) -> Vec<SkillSummary> {
        self.iter()
            .map(|s| SkillSummary {
                name: s.name.clone(),
                description: s.description.clone(),
            })
            .collect()
    }

    /// The number of skills.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// Whether there are no skills.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
}

/// The result of [`discover`].
#[derive(Debug, Default)]
pub struct Discovery {
    /// The skills that loaded.
    pub skills: SkillSet,
    /// One error per skill (or unreadable source directory) that was skipped, for the caller to
    /// log.
    pub warnings: Vec<SkillError>,
}

/// Loads every skill from `sources`, in order; a skill overrides an earlier one with the same name.
///
/// In each source, every subdirectory is a skill, except hidden ones (names starting with `.`).
/// Symbolic links are followed; plain files are ignored. A missing source directory is not an
/// error. Subdirectories are read in name order, so warnings come out in a stable order.
///
/// A skill that fails to load is skipped with a warning and does not override anything, so a
/// broken agent skill leaves a global skill with the same name in place.
pub fn discover(sources: &[SkillSource]) -> Discovery {
    let mut found = Discovery::default();
    for source in sources {
        let dirs = match skill_dirs(&source.dir) {
            Ok(dirs) => dirs,
            Err(warning) => {
                found.warnings.push(warning);
                continue;
            }
        };
        for dir in dirs {
            let mut skill = match dir.and_then(|dir| load_skill(&dir)) {
                Ok(skill) => skill,
                Err(warning) => {
                    found.warnings.push(warning);
                    continue;
                }
            };
            if let Some(prefix) = &source.prefix {
                skill.name = format!("{prefix}__{}", skill.name);
            }
            found.skills.skills.insert(skill.name.clone(), skill);
        }
    }
    found
}

/// The candidate skill directories in `source`, sorted by name. An error for one entry is returned
/// in its place so the others still load.
fn skill_dirs(source: &Path) -> Result<Vec<Result<PathBuf, SkillError>>, SkillError> {
    let io_error = |path: &Path, error| SkillError::Io {
        path: path.to_owned(),
        error,
    };
    let entries = match fs::read_dir(source) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_error(source, e)),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| io_error(source, e))?;
        if entry.file_name().as_encoded_bytes().starts_with(b".") {
            continue;
        }
        paths.push(entry.path());
    }
    paths.sort();

    let mut dirs = Vec::new();
    for path in paths {
        match fs::metadata(&path) {
            Ok(meta) if meta.is_dir() => dirs.push(Ok(path)),
            Ok(_) => {}
            Err(e) => dirs.push(Err(io_error(&path, e))),
        }
    }
    Ok(dirs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(dir_name: &str, text: &str) -> Result<Skill, SkillError> {
        parse_skill(&Path::new("/skills").join(dir_name), text)
    }

    #[test]
    fn parses_minimal_skill() {
        let skill = parse(
            "pdf",
            "---\nname: pdf\ndescription: Read PDFs.\n---\n# PDF\n\nSteps.\n",
        )
        .unwrap();
        assert_eq!(
            skill,
            Skill {
                name: "pdf".into(),
                description: "Read PDFs.".into(),
                body: "# PDF\n\nSteps.".into(),
                dir: PathBuf::from("/skills/pdf"),
            }
        );
    }

    #[test]
    fn ignores_other_front_matter_fields() {
        let text = "---\nname: pdf-processing\ndescription: Extract PDF text.\nlicense: Apache-2.0\n\
                    metadata:\n  author: example-org\n  version: \"1.0\"\n\
                    allowed-tools: Bash(git:*) Read\ncompatibility: Requires git\n---\nBody\n";
        let skill = parse("pdf-processing", text).unwrap();
        assert_eq!(skill.name, "pdf-processing");
        assert_eq!(skill.description, "Extract PDF text.");
    }

    #[test]
    fn body_drops_leading_blank_lines_and_trailing_whitespace_only() {
        let text = "---\nname: a\ndescription: d\n---\n\n\n    indented code\n\nmore  \n\n\n";
        assert_eq!(parse("a", text).unwrap().body, "    indented code\n\nmore");
        assert_eq!(
            parse("a", "---\nname: a\ndescription: d\n---")
                .unwrap()
                .body,
            ""
        );
    }

    #[test]
    fn crlf_files_parse_and_body_uses_lf() {
        let text = "---\r\nname: a\r\ndescription: >\r\n  Folded\r\n  text.\r\n---\r\nline 1\r\nline 2\r\n";
        let skill = parse("a", text).unwrap();
        assert_eq!(skill.description, "Folded text.");
        assert_eq!(skill.body, "line 1\nline 2");
    }

    #[test]
    fn description_is_trimmed_and_may_span_lines() {
        let text = "---\nname: a\ndescription: |\n  Line one.\n  Line two.\n---\n";
        assert_eq!(
            parse("a", text).unwrap().description,
            "Line one.\nLine two."
        );
    }

    #[test]
    fn rejects_missing_or_unclosed_front_matter() {
        for text in ["# Just Markdown\n", "---\nname: a\ndescription: d\n", ""] {
            let err = parse("a", text).unwrap_err();
            assert!(
                matches!(err, SkillError::NoFrontMatter { .. }),
                "{text:?}: {err}"
            );
            assert!(err.to_string().starts_with("/skills/a/SKILL.md: "), "{err}");
        }
    }

    #[test]
    fn rejects_invalid_yaml_naming_the_file() {
        let cases = [
            ("---\nname: [unclosed\ndescription: d\n---\n", "line 2"),
            ("---\n- a list\n- not a mapping\n---\n", "line 2"),
            ("---\nname: a\ndescription:\n  nested: map\n---\n", "line 4"),
            ("---\nname: a\nname: b\ndescription: d\n---\n", "line 3"),
        ];
        for (text, line) in cases {
            let err = parse("a", text).unwrap_err();
            assert!(matches!(err, SkillError::Yaml { .. }), "{text:?}: {err}");
            let message = err.to_string();
            assert!(
                message.starts_with("/skills/a/SKILL.md: invalid YAML"),
                "{message}"
            );
            // Line numbers count from the top of the file, and the message is one line.
            assert!(message.contains(line), "{message}");
            assert!(!message.contains('\n'), "{message}");
        }
    }

    #[test]
    fn rejects_missing_fields() {
        let cases = [
            ("---\ndescription: d\n---\n", "name"),
            ("---\nname: a\n---\n", "description"),
            ("---\nname: a\ndescription:\n---\n", "description"),
            ("---\n---\n", "name"),
        ];
        for (text, missing) in cases {
            match parse("a", text) {
                Err(SkillError::MissingField { field, path }) => {
                    assert_eq!(field, missing, "{text:?}");
                    assert_eq!(path, Path::new("/skills/a/SKILL.md"));
                }
                other => panic!("{text:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn description_rules() {
        let with = |d: &str| parse("a", &format!("---\nname: a\ndescription: \"{d}\"\n---\n"));
        assert!(matches!(
            with("   "),
            Err(SkillError::InvalidDescription { .. })
        ));
        assert!(with(&"x".repeat(MAX_DESCRIPTION_CHARS)).is_ok());
        assert!(matches!(
            with(&"x".repeat(MAX_DESCRIPTION_CHARS + 1)),
            Err(SkillError::InvalidDescription { .. })
        ));
        // Characters, not bytes.
        assert!(with(&"é".repeat(MAX_DESCRIPTION_CHARS)).is_ok());
    }

    #[test]
    fn name_rules() {
        let max = "a".repeat(MAX_NAME_CHARS);
        let too_long = "a".repeat(MAX_NAME_CHARS + 1);
        let valid = [
            "a",
            "pdf-processing",
            "v2",
            "x-1-y",
            max.as_str(),
            "café",
            "数据",
        ];
        for name in valid {
            assert_eq!(validate_skill_name(name), Ok(()), "{name:?}");
        }
        let invalid = [
            "",
            too_long.as_str(),
            "PDF",
            "Café",
            "a_b",
            "a__b",
            "a b",
            "a.b",
            "-a",
            "a-",
            "a--b",
            "a/b",
        ];
        for name in invalid {
            assert!(validate_skill_name(name).is_err(), "{name:?}");
        }
    }

    #[test]
    fn rejects_invalid_name_before_comparing_with_directory() {
        let err = parse("Bad", "---\nname: Bad\ndescription: d\n---\n").unwrap_err();
        assert!(matches!(err, SkillError::InvalidName { .. }), "{err}");
    }

    #[test]
    fn name_must_match_directory() {
        let err = parse("pdf", "---\nname: pdf-tools\ndescription: d\n---\n").unwrap_err();
        assert_eq!(
            err.to_string(),
            "/skills/pdf/SKILL.md: skill name \"pdf-tools\" does not match its directory name \
             \"pdf\""
        );
    }

    fn write_skill(dir: &Path, name: &str, description: &str) {
        let dir = dir.join(name);
        fs::create_dir_all(&dir).unwrap();
        let text = format!("---\nname: {name}\ndescription: {description}\n---\nBody of {name}.\n");
        fs::write(dir.join(SKILL_FILE), text).unwrap();
    }

    #[test]
    fn discovers_with_agent_override_sorted_and_skips_bad_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let root = DataRoot::new(tmp.path(), tmp.path()).unwrap();
        let global = root.skills_dir();
        let agent = root.agent_dir("jeeves").unwrap().join("skills");

        write_skill(&global, "zeta", "Global zeta.");
        write_skill(&global, "shared", "Global version.");
        write_skill(&global, "alpha", "Global alpha.");
        write_skill(&agent, "shared", "Agent version.");
        write_skill(&agent, "gamma", "Agent gamma.");
        // A broken agent skill doesn't hide the global one of the same name.
        write_skill(&global, "beta", "Global beta.");
        fs::create_dir_all(agent.join("beta")).unwrap();
        fs::write(agent.join("beta").join(SKILL_FILE), "no front-matter").unwrap();
        // A directory without SKILL.md is reported; files and hidden directories are ignored.
        fs::create_dir_all(agent.join("empty")).unwrap();
        fs::write(agent.join("README.md"), "not a skill").unwrap();
        fs::create_dir_all(agent.join(".git")).unwrap();

        let found = discover(&agent_sources(&root, "jeeves").unwrap());

        let names: Vec<_> = found.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["alpha", "beta", "gamma", "shared", "zeta"]);
        let shared = found.skills.get("shared").unwrap();
        assert_eq!(shared.description, "Agent version.");
        assert_eq!(shared.dir, agent.join("shared"));
        assert_eq!(shared.body, "Body of shared.");
        assert_eq!(found.skills.get("beta").unwrap().dir, global.join("beta"));
        assert_eq!(
            found.skills.summaries()[0],
            SkillSummary {
                name: "alpha".into(),
                description: "Global alpha.".into()
            }
        );

        assert_eq!(found.warnings.len(), 2, "{:?}", found.warnings);
        assert!(
            matches!(&found.warnings[0], SkillError::NoFrontMatter { path }
            if *path == agent.join("beta").join(SKILL_FILE))
        );
        assert!(matches!(&found.warnings[1], SkillError::Io { path, error }
            if *path == agent.join("empty").join(SKILL_FILE)
                && error.kind() == io::ErrorKind::NotFound));
    }

    #[test]
    fn missing_sources_are_empty_without_warnings() {
        let tmp = tempfile::tempdir().unwrap();
        let found = discover(&[SkillSource::local(tmp.path().join("nope"))]);
        assert!(found.skills.is_empty());
        assert!(found.warnings.is_empty());
    }

    #[test]
    fn prefixed_sources_name_skills_with_the_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "review", "Review code.");
        let source = SkillSource {
            dir: tmp.path().to_owned(),
            prefix: Some("github".into()),
        };
        let found = discover(&[source]);
        let skill = found.skills.get("github__review").unwrap();
        assert_eq!(skill.dir, tmp.path().join("review"));
        assert_eq!(found.skills.len(), 1);
    }

    #[test]
    fn follows_symlinked_skill_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let elsewhere = tmp.path().join("elsewhere");
        write_skill(&elsewhere, "linked", "Linked skill.");
        let skills = tmp.path().join("skills");
        fs::create_dir_all(&skills).unwrap();
        std::os::unix::fs::symlink(elsewhere.join("linked"), skills.join("linked")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("missing"), skills.join("dangling")).unwrap();

        let found = discover(&[SkillSource::local(&skills)]);
        assert_eq!(
            found.skills.get("linked").unwrap().dir,
            skills.join("linked")
        );
        assert_eq!(found.warnings.len(), 1);
        assert!(matches!(&found.warnings[0], SkillError::Io { path, .. }
            if *path == skills.join("dangling")));
    }
}

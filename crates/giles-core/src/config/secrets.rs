//! Secrets: `secrets.toml`, and `{ secret = "name" }` and `{ env = "NAME" }` references (PLAN
//! section 7.7).
//!
//! A resolved value is held as a [`Secret`], whose `Debug` and `Display` print `<redacted>`.
//! Errors name the key path and the secret or variable name, never a value.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use super::error::{ConfigError, KeyPath};

/// What a [`Secret`] prints instead of its value.
pub const REDACTED: &str = "<redacted>";

/// A credential: a value from `secrets.toml`, the environment, or a credential key such as `auth`.
/// `Debug` and `Display` print `<redacted>`; [`Secret::expose`] is the only way to the value.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wraps `value`.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The value. Never log it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

/// Secret values shorter than this many bytes are left alone by [`scrub`]. A value that short
/// (a port, a flag, `1`) would turn every matching character or number in ordinary text into
/// `<redacted>`, garbling the message, and a value that short protects nothing anyway.
pub const MIN_SCRUB_LEN: usize = 4;

/// `text` with every occurrence of any of `secrets` replaced by `<redacted>`: for text built from
/// secret values that Giles logs or shows, such as a plugin's `Configure` error or a plugin's or
/// MCP server's stderr (whose credentials are `auth`, toolset `env`, and `config` values).
///
/// One pass from left to right; at each position the longest matching value wins, so a value
/// containing another is replaced whole and an inserted `<redacted>` is never matched again.
/// Values shorter than [`MIN_SCRUB_LEN`] are skipped.
pub fn scrub(text: &str, secrets: &[&Secret]) -> String {
    let mut values: Vec<&str> = secrets
        .iter()
        .map(|s| s.expose())
        .filter(|v| v.len() >= MIN_SCRUB_LEN)
        .collect();
    if values.is_empty() {
        return text.to_owned();
    }
    values.sort_unstable_by_key(|v| std::cmp::Reverse(v.len()));
    values.dedup();
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if let Some(value) = values.iter().find(|v| rest.starts_with(**v)) {
            out.push_str(REDACTED);
            rest = &rest[value.len()..];
        } else {
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

/// The contents of `secrets.toml`: a flat table of names to strings, which `{ secret = "name" }`
/// references resolve against. A value is read when the file is loaded, so loading again picks up
/// a changed file.
#[derive(Debug, Default)]
pub struct Secrets {
    file: PathBuf,
    exists: bool,
    values: BTreeMap<String, Secret>,
}

impl Secrets {
    /// Loads `file` if it exists. It must be a regular file that neither group nor others can
    /// access (no bits of mode 0o077 set).
    ///
    /// # Errors
    ///
    /// The problem with the file, as a [`ConfigError`] naming `file`; never quotes a value.
    pub fn load(file: &Path) -> Result<Self, ConfigError> {
        let meta = match fs::metadata(file) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(Self {
                    file: file.to_path_buf(),
                    ..Self::default()
                });
            }
            Err(e) => return Err(ConfigError::io(file, "inspect", e)),
        };
        if !meta.is_file() {
            return Err(ConfigError::file(file, "not a regular file"));
        }
        let mode = meta.mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(ConfigError::file(
                file,
                format!(
                    "group or others can access it (mode {mode:04o}); run `chmod 600 {}`",
                    file.display()
                ),
            ));
        }
        let source = super::read_file(file)?;
        let table: toml::Table =
            toml::from_str(&source).map_err(|e| ConfigError::syntax(file, &source, &e))?;
        let mut values = BTreeMap::new();
        for (name, value) in table {
            match value {
                toml::Value::String(value) => {
                    values.insert(name, Secret(value));
                }
                // Say what's wrong without the value, which may hold the secret.
                other => {
                    return Err(ConfigError::at(
                        file,
                        KeyPath::root().key(name),
                        format!("must be a string, not {}", other.type_str()),
                    )
                    .located(file, &source));
                }
            }
        }
        Ok(Self {
            file: file.to_path_buf(),
            exists: true,
            values,
        })
    }

    #[cfg(test)]
    pub(crate) fn from_values(values: &[(&str, &str)]) -> Self {
        Self {
            file: PathBuf::from("/h/secrets.toml"),
            exists: true,
            values: values
                .iter()
                .map(|(k, v)| ((*k).to_owned(), Secret::new(*v)))
                .collect(),
        }
    }
}

/// A `{ secret = "name" }` or `{ env = "NAME" }` reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reference<'a> {
    /// A name in `secrets.toml`.
    Secret(&'a str),
    /// An environment variable of the `giles` process.
    Env(&'a str),
}

impl<'a> Reference<'a> {
    /// Whether `value` is a reference: an inline table with exactly one key, `secret` or `env`.
    /// `Some(Err(reason))` if it has that shape but doesn't name a secret or variable.
    pub(crate) fn of(value: &'a toml::Value) -> Option<Result<Self, String>> {
        let table = value.as_table()?;
        if table.len() != 1 {
            return None;
        }
        let (key, name) = table.iter().next()?;
        let reference: fn(&'a str) -> Self = match key.as_str() {
            "secret" => Self::Secret,
            "env" => Self::Env,
            _ => return None,
        };
        Some(match name.as_str() {
            Some(name) if !name.is_empty() => Ok(reference(name)),
            _ => Err(format!(
                "`{{ {key} = ... }}` must name a {} as a non-empty string",
                if key == "secret" {
                    "secret"
                } else {
                    "variable"
                }
            )),
        })
    }
}

/// Resolves references for one load.
pub(crate) struct Resolver<'a> {
    pub secrets: &'a Secrets,
    pub env: &'a dyn Fn(&str) -> Option<OsString>,
}

impl Resolver<'_> {
    /// Resolves `reference`, found at `path` in `file`.
    pub(crate) fn resolve(
        &self,
        file: &Path,
        path: &KeyPath,
        reference: Reference<'_>,
    ) -> Result<Secret, ConfigError> {
        let err = |reason: String| ConfigError::at(file, path.clone(), reason);
        match reference {
            Reference::Secret(name) => match self.secrets.values.get(name) {
                Some(value) => Ok(value.clone()),
                None if self.secrets.exists => Err(err(format!(
                    "no secret {name:?} in {}",
                    self.secrets.file.display()
                ))),
                None => Err(err(format!(
                    "no secret {name:?}: {} does not exist",
                    self.secrets.file.display()
                ))),
            },
            Reference::Env(name) => match (self.env)(name) {
                Some(value) => value
                    .into_string()
                    .map(Secret)
                    .map_err(|_| err(format!("environment variable {name} is not valid UTF-8"))),
                None => Err(err(format!("environment variable {name} is not set"))),
            },
        }
    }

    /// A credential value: a string, or a reference to one.
    pub(crate) fn value(
        &self,
        file: &Path,
        path: &KeyPath,
        value: &toml::Value,
    ) -> Result<Secret, ConfigError> {
        match Reference::of(value) {
            Some(Ok(reference)) => self.resolve(file, path, reference),
            Some(Err(reason)) => Err(ConfigError::at(file, path.clone(), reason)),
            None => match value {
                toml::Value::String(s) => Ok(Secret::new(s.as_str())),
                other => Err(ConfigError::at(
                    file,
                    path.clone(),
                    format!(
                        "must be a string, {{ secret = \"name\" }}, or {{ env = \"NAME\" }}, \
                         not {}",
                        other.type_str()
                    ),
                )),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn write(dir: &Path, contents: &str, mode: u32) -> PathBuf {
        let file = dir.join("secrets.toml");
        // Replace rather than overwrite: an earlier case may have left it read-only.
        let _ = fs::remove_file(&file);
        fs::write(&file, contents).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(mode)).unwrap();
        file
    }

    #[test]
    fn scrub_replaces_in_one_pass_longest_first() {
        let s = |v: &str| Secret::new(v);
        let (long, short, inner, tiny) = (s("sk-long-value"), s("long"), s("abcd"), s("1"));
        let all = [&long, &short, &inner, &tiny];
        let cases = [
            ("key sk-long-value!", "key <redacted>!"),
            // The longer value wins where both match; the shorter still matches elsewhere.
            ("sk-long-value and long", "<redacted> and <redacted>"),
            // Adjacent and repeated values.
            ("abcdabcd long", "<redacted><redacted> <redacted>"),
            // `<redacted>` itself is never rescanned, and short values are left alone.
            ("port 1, count 11", "port 1, count 11"),
            ("é abcd é", "é <redacted> é"),
            ("nothing here", "nothing here"),
        ];
        for (text, expected) in cases {
            assert_eq!(scrub(text, &all), expected, "{text}");
        }
        // A value that is a substring of `<redacted>` doesn't garble earlier replacements.
        let red = s("dact");
        assert_eq!(scrub("dact abcd", &[&inner, &red]), "<redacted> <redacted>");
        assert_eq!(scrub("x", &[]), "x");
    }

    #[test]
    fn secret_redacts_debug_and_display() {
        let secret = Secret::new("sk-live-123");
        assert_eq!(format!("{secret:?}"), "<redacted>");
        assert_eq!(format!("{secret}"), "<redacted>");
        assert_eq!(format!("{:?}", Some(&secret)), "Some(<redacted>)");
        assert_eq!(secret.expose(), "sk-live-123");
    }

    #[test]
    fn secrets_file_permissions() {
        let dir = tempfile::tempdir().unwrap();
        for mode in [0o600, 0o400, 0o700] {
            let file = write(dir.path(), "a = \"x\"", mode);
            let secrets = Secrets::load(&file).unwrap();
            assert_eq!(secrets.values["a"].expose(), "x");
        }
        for mode in [
            0o640, 0o604, 0o644, 0o660, 0o620, 0o602, 0o610, 0o601, 0o777,
        ] {
            let file = write(dir.path(), "a = \"x\"", mode);
            let err = Secrets::load(&file).unwrap_err();
            assert_eq!(err.file, file);
            assert!(err.reason.contains("group or others"), "{mode:o}: {err}");
            assert!(err.reason.contains(&format!("{:04o}", mode)), "{err}");
            assert!(err.reason.contains("chmod 600"), "{err}");
        }
    }

    #[test]
    fn missing_secrets_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Secrets::load(&dir.path().join("secrets.toml")).unwrap();
        assert!(!secrets.exists);
        assert!(secrets.values.is_empty());
    }

    #[test]
    fn a_directory_is_not_a_secrets_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("secrets.toml");
        fs::create_dir(&file).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o700)).unwrap();
        let err = Secrets::load(&file).unwrap_err();
        assert!(err.reason.contains("regular file"), "{err}");
    }

    #[test]
    fn secrets_file_errors_never_show_values() {
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            (
                "token = 12345678\n",
                "token",
                "must be a string, not integer",
            ),
            (
                "token = { v = \"hunter2-12345678\" }\n",
                "token",
                "must be a string, not table",
            ),
            (
                "token = [\"hunter2-12345678\"]\n",
                "token",
                "must be a string, not array",
            ),
            ("token = \"hunter2-12345678\n", "", "invalid TOML"),
            ("token = 'hunter2-12345678\n", "", "invalid TOML"),
            ("token = \"\"\"hunter2-12345678\n", "", "invalid TOML"),
            ("token = hunter2-12345678\n", "", "invalid TOML"),
            ("token = \"hunter2-\\q12345678\"\n", "", "invalid TOML"),
            (
                "hunter2 = \"a\"\nhunter2 = \"12345678\"\n",
                "",
                "invalid TOML",
            ),
        ];
        for (contents, key, reason) in cases {
            let file = write(dir.path(), contents, 0o600);
            let err = Secrets::load(&file).unwrap_err();
            let shown = format!("{err} {err:?}");
            assert_eq!(err.key.to_string(), key, "{shown}");
            assert!(err.reason.contains(reason), "{shown}");
            assert!(!shown.contains("12345678"), "{shown}");
        }
    }

    #[test]
    fn references() {
        let t =
            |src: &str| -> toml::Value { toml::from_str::<toml::Table>(src).unwrap()["v"].clone() };
        let secret = t("v = { secret = \"github\" }");
        assert_eq!(
            Reference::of(&secret),
            Some(Ok(Reference::Secret("github")))
        );
        let env = t("v = { env = \"GITHUB_TOKEN\" }");
        assert_eq!(
            Reference::of(&env),
            Some(Ok(Reference::Env("GITHUB_TOKEN")))
        );
        for not_a_reference in [
            "v = \"plain\"",
            "v = {}",
            "v = { secret = \"a\", other = 1 }",
            "v = { secrets = \"a\" }",
            "v = [{ secret = \"a\" }]",
        ] {
            assert_eq!(
                Reference::of(&t(not_a_reference)),
                None,
                "{not_a_reference}"
            );
        }
        for bad in [
            "v = { secret = 1 }",
            "v = { env = \"\" }",
            "v = { env = [\"A\"] }",
        ] {
            let err = Reference::of(&t(bad)).unwrap().unwrap_err();
            assert!(err.contains("non-empty string"), "{bad}: {err}");
        }
    }

    #[test]
    fn resolution() {
        let secrets = Secrets::from_values(&[("github", "ghp-secret")]);
        let env = |name: &str| match name {
            "TOKEN" => Some(OsString::from("env-secret")),
            "BINARY" => Some(std::os::unix::ffi::OsStringExt::from_vec(vec![0xff])),
            _ => None,
        };
        let resolver = Resolver {
            secrets: &secrets,
            env: &env,
        };
        let file = Path::new("/h/giles.toml");
        let path = KeyPath::root().key("x");
        let resolve = |r| resolver.resolve(file, &path, r);
        assert_eq!(
            resolve(Reference::Secret("github")).unwrap().expose(),
            "ghp-secret"
        );
        assert_eq!(
            resolve(Reference::Env("TOKEN")).unwrap().expose(),
            "env-secret"
        );

        let err = resolve(Reference::Secret("gitlab")).unwrap_err();
        assert_eq!(err.key, path);
        assert!(
            err.reason
                .contains("no secret \"gitlab\" in /h/secrets.toml"),
            "{err}"
        );
        let err = resolve(Reference::Env("UNSET")).unwrap_err();
        assert!(err.reason.contains("UNSET is not set"), "{err}");
        let err = resolve(Reference::Env("BINARY")).unwrap_err();
        assert!(err.reason.contains("not valid UTF-8"), "{err}");

        let none = Secrets {
            file: PathBuf::from("/h/secrets.toml"),
            ..Secrets::default()
        };
        let resolver = Resolver {
            secrets: &none,
            env: &env,
        };
        let err = resolver
            .resolve(file, &path, Reference::Secret("github"))
            .unwrap_err();
        assert!(
            err.reason.contains("/h/secrets.toml does not exist"),
            "{err}"
        );
    }

    #[test]
    fn credential_values() {
        let secrets = Secrets::from_values(&[("s", "from-file")]);
        let resolver = Resolver {
            secrets: &secrets,
            env: &|_| None,
        };
        let file = Path::new("/h/giles.toml");
        let path = KeyPath::root().key("auth").key("bearer");
        let value = |src: &str| {
            let v = toml::from_str::<toml::Table>(src).unwrap()["v"].clone();
            resolver.value(file, &path, &v)
        };
        assert_eq!(value("v = \"literal\"").unwrap().expose(), "literal");
        assert_eq!(
            value("v = { secret = \"s\" }").unwrap().expose(),
            "from-file"
        );
        let err = value("v = 123456789").unwrap_err();
        assert!(err.reason.contains("not integer"), "{err}");
        assert!(!err.to_string().contains("123456789"), "{err}");
        let err = value("v = { secret = \"s\", x = 1 }").unwrap_err();
        assert!(err.reason.contains("not table"), "{err}");
    }
}

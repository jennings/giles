//! An instance's free-form `config` table, which the core passes to the component as JSON in
//! `Configure` (PLAN section 7.7). The core checks nothing in it except that it converts to JSON
//! and that its secret references resolve; the component validates the rest.

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;

use serde_json::{Map, Number, Value};

use super::error::{ConfigError, KeyPath, Segment};
use super::secrets::{REDACTED, Reference, Resolver, Secret, scrub};

/// An instance's `config` table as JSON, with every `{ secret = ... }` and `{ env = ... }`
/// reference replaced by its value.
///
/// The paths that held references are remembered: `Debug` and [`PluginConfig::redacted`] show
/// `<redacted>` there. Only [`PluginConfig::to_json`], for `Configure`, has the values.
#[derive(Clone, PartialEq, Default)]
pub struct PluginConfig {
    value: Map<String, Value>,
    secret_paths: BTreeSet<KeyPath>,
    /// The values the references resolved to.
    secrets: Vec<Secret>,
}

impl PluginConfig {
    /// Converts the `config` value at `path` in `file`, resolving references.
    pub(crate) fn from_toml(
        file: &Path,
        path: &KeyPath,
        value: Option<toml::Value>,
        resolver: &Resolver<'_>,
    ) -> Result<Self, ConfigError> {
        let table = match value {
            None => toml::Table::new(),
            Some(toml::Value::Table(table)) => table,
            Some(other) => {
                return Err(ConfigError::at(
                    file,
                    path.clone(),
                    format!("must be a table, not {}", other.type_str()),
                ));
            }
        };
        let mut converter = Converter {
            file,
            root: path,
            resolver,
            secret_paths: BTreeSet::new(),
            secrets: Vec::new(),
        };
        let value = converter.table(&KeyPath::root(), table)?;
        Ok(Self {
            value,
            secret_paths: converter.secret_paths,
            secrets: converter.secrets,
        })
    }

    /// The table as a JSON object with secret values in place, for `Configure` only.
    pub fn to_json(&self) -> Value {
        Value::Object(self.value.clone())
    }

    /// The table as a JSON object with `"<redacted>"` at every secret path, safe to log or show.
    pub fn redacted(&self) -> Value {
        let mut value = Value::Object(self.value.clone());
        for path in &self.secret_paths {
            if let Some(slot) = lookup_mut(&mut value, path) {
                *slot = Value::String(REDACTED.to_owned());
            }
        }
        value
    }

    /// The paths, relative to the `config` table, whose values came from references.
    pub fn secret_paths(&self) -> impl Iterator<Item = &KeyPath> {
        self.secret_paths.iter()
    }

    /// The values that came from references, for scrubbing other text ([`scrub`]).
    pub fn secrets(&self) -> impl Iterator<Item = &Secret> {
        self.secrets.iter()
    }

    /// `message` with this config's secret values replaced by `<redacted>` ([`scrub`]): for text
    /// the component built from its config, such as a `Configure` validation error, before it is
    /// logged or shown.
    pub fn scrub(&self, message: &str) -> String {
        scrub(message, &self.secrets.iter().collect::<Vec<_>>())
    }
}

impl fmt::Debug for PluginConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PluginConfig({})", self.redacted())
    }
}

fn lookup_mut<'v>(value: &'v mut Value, path: &KeyPath) -> Option<&'v mut Value> {
    path.segments()
        .iter()
        .try_fold(value, |value, segment| match segment {
            Segment::Key(key) => value.get_mut(key.as_str()),
            Segment::Index(index) => value.get_mut(*index),
        })
}

struct Converter<'a> {
    file: &'a Path,
    /// The path of the `config` table in the file.
    root: &'a KeyPath,
    resolver: &'a Resolver<'a>,
    secret_paths: BTreeSet<KeyPath>,
    secrets: Vec<Secret>,
}

impl Converter<'_> {
    fn err(&self, path: &KeyPath, reason: impl Into<String>) -> ConfigError {
        ConfigError::at(self.file, self.root.join(path), reason)
    }

    fn table(
        &mut self,
        path: &KeyPath,
        table: toml::Table,
    ) -> Result<Map<String, Value>, ConfigError> {
        table
            .into_iter()
            .map(|(key, value)| {
                let value = self.value(&path.key(&key), value)?;
                Ok((key, value))
            })
            .collect()
    }

    /// Converts one TOML value. A reference becomes its value; datetimes become their TOML text
    /// (RFC 3339 for offset datetimes); infinite and NaN floats, which JSON can't represent, are
    /// errors.
    fn value(&mut self, path: &KeyPath, value: toml::Value) -> Result<Value, ConfigError> {
        match Reference::of(&value) {
            Some(Ok(reference)) => {
                let secret = self
                    .resolver
                    .resolve(self.file, &self.root.join(path), reference)?;
                self.secret_paths.insert(path.clone());
                let value = Value::String(secret.expose().to_owned());
                self.secrets.push(secret);
                return Ok(value);
            }
            Some(Err(reason)) => return Err(self.err(path, reason)),
            None => {}
        }
        Ok(match value {
            toml::Value::String(s) => Value::String(s),
            toml::Value::Integer(i) => Value::Number(i.into()),
            toml::Value::Float(f) => {
                Value::Number(Number::from_f64(f).ok_or_else(|| {
                    self.err(path, "infinite and NaN floats can't be sent as JSON")
                })?)
            }
            toml::Value::Boolean(b) => Value::Bool(b),
            toml::Value::Datetime(d) => Value::String(d.to_string()),
            toml::Value::Array(items) => Value::Array(
                items
                    .into_iter()
                    .enumerate()
                    .map(|(i, item)| self.value(&path.index(i), item))
                    .collect::<Result<_, _>>()?,
            ),
            toml::Value::Table(table) => Value::Object(self.table(path, table)?),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use serde_json::json;

    use super::super::secrets::Secrets;
    use super::*;

    fn convert(src: &str) -> Result<PluginConfig, ConfigError> {
        let secrets = Secrets::from_values(&[("anthropic", "sk-ant-secret")]);
        let env = |name: &str| (name == "TOKEN").then(|| OsString::from("env-secret"));
        let resolver = Resolver {
            secrets: &secrets,
            env: &env,
        };
        let table: toml::Table = toml::from_str(src).unwrap();
        let path = KeyPath::root().key("channel").key("irc").key("config");
        PluginConfig::from_toml(
            Path::new("giles.toml"),
            &path,
            Some(toml::Value::Table(table)),
            &resolver,
        )
    }

    #[test]
    fn toml_converts_to_json() {
        let json = convert(
            r##"
            server = "irc.libera.chat:6697"
            port = 6697
            ratio = 0.5
            tls = true
            channels = ["#giles", "#rust"]
            since = 1979-05-27T07:32:00Z
            day = 1979-05-27
            nested = { deep = [{ x = 1 }] }
            "##,
        )
        .unwrap()
        .to_json();
        assert_eq!(
            json,
            json!({
                "server": "irc.libera.chat:6697",
                "port": 6697,
                "ratio": 0.5,
                "tls": true,
                "channels": ["#giles", "#rust"],
                "since": "1979-05-27T07:32:00Z",
                "day": "1979-05-27",
                "nested": { "deep": [{ "x": 1 }] },
            })
        );
        assert_eq!(convert("").unwrap().to_json(), json!({}));
    }

    #[test]
    fn non_finite_floats_are_errors_at_their_path() {
        let err = convert("a = { b = [1.0, inf] }").unwrap_err();
        assert_eq!(err.key.to_string(), "channel.irc.config.a.b[1]");
        assert!(err.reason.contains("NaN"), "{err}");
        assert!(convert("x = nan").is_err());
    }

    #[test]
    fn references_resolve_anywhere_and_are_redacted() {
        let config = convert(
            r#"
            api_key = { secret = "anthropic" }
            servers = [{ password = { env = "TOKEN" } }, { password = "plain" }]
            not_a_ref = { secret = "anthropic", note = "two keys" }
            "#,
        )
        .unwrap();
        assert_eq!(
            config.to_json(),
            json!({
                "api_key": "sk-ant-secret",
                "servers": [{ "password": "env-secret" }, { "password": "plain" }],
                "not_a_ref": { "secret": "anthropic", "note": "two keys" },
            })
        );
        let redacted = json!({
            "api_key": "<redacted>",
            "servers": [{ "password": "<redacted>" }, { "password": "plain" }],
            "not_a_ref": { "secret": "anthropic", "note": "two keys" },
        });
        assert_eq!(config.redacted(), redacted);
        let paths: Vec<String> = config.secret_paths().map(ToString::to_string).collect();
        assert_eq!(paths, ["api_key", "servers[0].password"]);

        let debug = format!("{config:?}");
        assert!(!debug.contains("sk-ant-secret"), "{debug}");
        assert!(!debug.contains("env-secret"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
    }

    #[test]
    fn scrub_removes_secret_values_from_messages() {
        let config = convert(
            r#"
            api_key = { secret = "anthropic" }
            short = { env = "TOKEN" }
            plain = "sk-ant"
            "#,
        )
        .unwrap();
        let message = "api_key sk-ant-secret rejected; token env-secret; prefix sk-ant";
        assert_eq!(
            config.scrub(message),
            "api_key <redacted> rejected; token <redacted>; prefix sk-ant"
        );
        assert_eq!(convert("").unwrap().scrub("nothing"), "nothing");
        let secrets: Vec<&str> = config.secrets().map(Secret::expose).collect();
        assert_eq!(secrets, ["sk-ant-secret", "env-secret"]);
    }

    #[test]
    fn bad_references_are_errors_at_their_path() {
        let cases = [
            (
                "a = { secret = \"missing\" }",
                "channel.irc.config.a",
                "no secret \"missing\"",
            ),
            (
                "a = [{ env = \"UNSET\" }]",
                "channel.irc.config.a[0]",
                "UNSET is not set",
            ),
            (
                "a = { b = { secret = 5 } }",
                "channel.irc.config.a.b",
                "non-empty string",
            ),
        ];
        for (src, key, reason) in cases {
            let err = convert(src).unwrap_err();
            assert_eq!(err.key.to_string(), key, "{err}");
            assert!(err.reason.contains(reason), "{err}");
        }
    }
}

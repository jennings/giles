//! An instance's free-form `config` table, which the core passes to the component as JSON in
//! `Configure` (PLAN section 7.7). The core checks nothing in it except that it converts to JSON;
//! the component validates it.

use std::path::Path;

use serde_json::{Map, Number, Value};

use super::error::{ConfigError, KeyPath};

/// An instance's `config` table as JSON.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PluginConfig {
    value: Map<String, Value>,
}

impl PluginConfig {
    /// Converts the `config` table at `path` in `file`.
    pub(crate) fn from_toml(
        file: &Path,
        path: &KeyPath,
        table: toml::Table,
    ) -> Result<Self, ConfigError> {
        Ok(Self {
            value: table_to_json(file, path, table)?,
        })
    }

    /// The table as a JSON object, as sent to the component.
    pub fn to_json(&self) -> Value {
        Value::Object(self.value.clone())
    }
}

fn table_to_json(
    file: &Path,
    path: &KeyPath,
    table: toml::Table,
) -> Result<Map<String, Value>, ConfigError> {
    table
        .into_iter()
        .map(|(key, value)| {
            let value = to_json(file, &path.key(&key), value)?;
            Ok((key, value))
        })
        .collect()
}

/// Converts one TOML value. Datetimes become their TOML text (RFC 3339 for offset datetimes);
/// infinite and NaN floats, which JSON can't represent, are errors.
fn to_json(file: &Path, path: &KeyPath, value: toml::Value) -> Result<Value, ConfigError> {
    Ok(match value {
        toml::Value::String(s) => Value::String(s),
        toml::Value::Integer(i) => Value::Number(i.into()),
        toml::Value::Float(f) => Value::Number(Number::from_f64(f).ok_or_else(|| {
            ConfigError::at(
                file,
                path.clone(),
                "infinite and NaN floats can't be sent as JSON",
            )
        })?),
        toml::Value::Boolean(b) => Value::Bool(b),
        toml::Value::Datetime(d) => Value::String(d.to_string()),
        toml::Value::Array(items) => Value::Array(
            items
                .into_iter()
                .enumerate()
                .map(|(i, item)| to_json(file, &path.index(i), item))
                .collect::<Result<_, _>>()?,
        ),
        toml::Value::Table(table) => Value::Object(table_to_json(file, path, table)?),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn convert(src: &str) -> Result<Value, ConfigError> {
        let table: toml::Table = toml::from_str(src).unwrap();
        let path = KeyPath::root().key("channel").key("irc").key("config");
        PluginConfig::from_toml(Path::new("giles.toml"), &path, table).map(|c| c.to_json())
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
        .unwrap();
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
        assert_eq!(convert("").unwrap(), json!({}));
    }

    #[test]
    fn non_finite_floats_are_errors_at_their_path() {
        let err = convert("a = { b = [1.0, inf] }").unwrap_err();
        assert_eq!(err.key.to_string(), "channel.irc.config.a.b[1]");
        assert!(err.reason.contains("NaN"), "{err}");
        assert!(convert("x = nan").is_err());
    }
}

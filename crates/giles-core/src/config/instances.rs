//! Component instances: `[provider.<instance>]`, `[channel.<instance>]`, and
//! `[toolset.<instance>]` (PLAN sections 7.2, 7.6, 7.10, 11.2).
//!
//! Every key at the instance level belongs to the core; only `config` goes to the component. A
//! key that has no effect on an instance of its shape (e.g. `sandbox` on a built-in channel) is an
//! error, not silently ignored. A key the core doesn't know at all is ignored and reported
//! (`Config::unknown_keys`), which serde finds for the typed tables and `auth` finds by hand.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use giles_plugin_sdk::config::{ByteSize, HumanDuration};
use serde::Deserialize;
use serde::Serialize;
use serde::ser::Serializer;
use url::{Host, Url};

use super::error::{ConfigError, KeyPath, Position};
use super::hash::ConfigHash;
use super::names::{Builtins, Component, InstanceName, Kind, invalid_name_reason, is_valid_name};
use super::plugin_config::PluginConfig;
use super::sandbox::{self, Grants, Protected, SandboxSpec};
use super::secrets::{REDACTED, Resolver, Secret};
use super::toolset::ToolsetError;
use crate::paths::DataRoot;
use crate::sandbox::DERIVED_ENV;

/// The largest blob the core fetches from a plugin when the instance sets no `max_blob`: 50 MiB
/// (PLAN section 8.3).
pub const DEFAULT_MAX_BLOB: u64 = 50 << 20;

/// A toolset's per-call deadline when the instance sets no `deadline`: 5 minutes.
pub const DEFAULT_TOOL_DEADLINE: Duration = Duration::from_secs(5 * 60);

/// A `[provider.<instance>]`. v1 providers are all built in (PLAN section 4.1).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ProviderInstance {
    /// The instance name.
    pub name: InstanceName,
    /// The built-in provider component, e.g. `openai`.
    pub component: String,
    /// The `config` table for the component.
    pub config: PluginConfig,
    /// The hash of the instance's table as written (PLAN section 7.9).
    pub config_hash: ConfigHash,
}

/// A `[channel.<instance>]`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ChannelInstance {
    /// The instance name.
    pub name: InstanceName,
    /// The component name, e.g. `irc`. Remote channels have one too: identity namespaces must
    /// start with it (PLAN section 3.2).
    pub component: String,
    /// Where the component runs.
    pub runtime: ChannelRuntime,
    /// The largest blob the core fetches from the plugin, in bytes (PLAN section 8.3).
    /// [`DEFAULT_MAX_BLOB`] unless set; unused for built-ins.
    pub max_blob: u64,
    /// The `config` table for the component.
    pub config: PluginConfig,
    /// The hash of the instance's table as written, for the `Describe` cache (PLAN section 7.9).
    pub config_hash: ConfigHash,
}

/// Where a channel runs.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ChannelRuntime {
    /// Compiled into the core.
    BuiltIn,
    /// A plugin binary the core launches.
    Plugin(Launch),
    /// A plugin someone else runs, reached over TCP (PLAN section 7.6).
    Remote(Remote),
}

/// A plugin binary the core launches under the sandbox.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Launch {
    /// The binary name, `giles-<kind>-<component>`, found in `<root>/plugins/` or on `$PATH`.
    pub binary: String,
    /// The sandbox it runs in.
    pub sandbox: Sandbox,
}

/// How a launched plugin or stdio toolset is sandboxed (PLAN section 12).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Sandbox {
    /// `unsandboxed = true`: launch without a sandbox, with a warning at every launch. `grants`
    /// is then the default and unused.
    pub unsandboxed: bool,
    /// The `sandbox` grants, with the kind's network default applied.
    pub grants: Grants,
}

/// A remote component: a gRPC plugin at `address`, or an MCP server at `url` (PLAN section 7.6).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Remote {
    /// `http` or `https`. Plain `http` to a host other than loopback requires `insecure`.
    pub url: Url,
    /// How the core authenticates to it, if at all.
    pub auth: Option<Auth>,
    /// `insecure = true`: allow plain text to a non-loopback host.
    pub insecure: bool,
}

/// How the core authenticates to a remote component: `auth = { bearer = ... }` or
/// `auth = { oauth = { ... } }`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Auth {
    /// A static bearer token.
    Bearer(Secret),
    /// OAuth 2.0 client credentials; remote channels only.
    OAuth(Box<OAuthClientCredentials>),
}

/// OAuth 2.0 client credentials grant settings.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OAuthClientCredentials {
    /// The token endpoint (`https`, or `http` to loopback).
    pub token_url: Url,
    /// The client id.
    pub client_id: String,
    /// The client secret.
    pub client_secret: Secret,
    /// Scopes to request; none if empty.
    pub scopes: Vec<String>,
}

/// A `[toolset.<instance>]`: one MCP server (PLAN section 7.10).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ToolsetInstance {
    /// The instance name, also the tool prefix: `[toolset.github]` gives `github__create_issue`.
    pub name: InstanceName,
    /// How the core reaches the server.
    pub transport: ToolsetTransport,
    /// How long one tool call may run. [`DEFAULT_TOOL_DEADLINE`] unless set.
    pub deadline: Duration,
    /// `parallel = true`: every tool of this instance is parallel-safe (PLAN section 6.3).
    pub parallel: bool,
    /// Skill directories the toolset offers (absolute paths).
    pub skills: Vec<PathBuf>,
    /// The hash of the instance's table as written, for the tool-list cache (PLAN section 7.9).
    pub config_hash: ConfigHash,
}

/// How the core reaches a toolset.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ToolsetTransport {
    /// A server the core launches and talks to over stdin and stdout.
    Stdio(StdioServer),
    /// A remote server over streamable HTTP.
    Http(Remote),
}

/// A stdio MCP server the core launches under the sandbox.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct StdioServer {
    /// `command`: an absolute path, or a bare name to look up on the operator's `$PATH`.
    pub command: String,
    /// Arguments, passed unchanged.
    pub args: Vec<String>,
    /// Environment variables to add, by name. Values may come from secret references.
    pub env: BTreeMap<String, Secret>,
    /// Stop the server after this long without calls; off unless set (PLAN section 7.5).
    pub idle_stop: Option<Duration>,
    /// The sandbox it runs in.
    pub sandbox: Sandbox,
}

// `config`, `auth`, and `env` may hold credentials, so they're taken as raw TOML values and
// checked by hand: serde's type errors quote the offending value.

#[derive(Debug, Deserialize)]
pub(crate) struct RawProvider {
    component: Option<String>,
    config: Option<toml::Value>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawChannel {
    component: Option<String>,
    config: Option<toml::Value>,
    address: Option<String>,
    auth: Option<toml::Value>,
    insecure: Option<bool>,
    max_blob: Option<ByteSize>,
    sandbox: Option<SandboxSpec>,
    unsandboxed: Option<bool>,
}

/// One toolset as written: the keys of a `[toolset.<name>]` table, deserializable from TOML and
/// from JSON (the form the web UI stores) with the same keys.
///
/// Nothing is checked yet beyond each value's type. [`validate_toolset`](super::validate_toolset)
/// turns it into a [`ToolsetInstance`], resolving credentials.
///
/// Credentials stay unresolved. `auth` is `{ bearer = <credential> }`, and the values of `env`
/// are credentials; a credential is a plain string or a reference, `{ secret = "<name>" }` (from
/// `secrets.toml`) or `{ env = "<VAR>" }`. `env` and `auth` are therefore raw values that
/// validation checks by hand: serde's type errors quote the offending value. `Debug` leaves both
/// out.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolsetSpec {
    /// `command`: the stdio server to run, an absolute path or a program name on `$PATH`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// `args`: arguments for the stdio server, passed unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    /// `env`: a table of environment variable names to credentials, for the stdio server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<toml::Value>,
    /// `url`: the address of a remote server, which makes the toolset remote.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// `auth`: how to authenticate to a remote server, `{ bearer = <credential> }`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<toml::Value>,
    /// `insecure`: allow plain `http` to a host other than loopback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub insecure: Option<bool>,
    /// `deadline`: how long one tool call may run, e.g. `"90s"`.
    #[serde(
        serialize_with = "serialize_duration",
        skip_serializing_if = "Option::is_none"
    )]
    pub deadline: Option<HumanDuration>,
    /// `idle_stop`: stop the stdio server after this long without calls.
    #[serde(
        serialize_with = "serialize_duration",
        skip_serializing_if = "Option::is_none"
    )]
    pub idle_stop: Option<HumanDuration>,
    /// `parallel`: every tool of the toolset is parallel-safe.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel: Option<bool>,
    /// `skills`: absolute paths of skill directories the toolset offers.
    #[serde(
        serialize_with = "serialize_paths",
        skip_serializing_if = "Option::is_none"
    )]
    pub skills: Option<Vec<PathBuf>>,
    /// `sandbox`: what the stdio server may reach.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxSpec>,
    /// `unsandboxed`: run the stdio server without a sandbox.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsandboxed: Option<bool>,
}

impl ToolsetSpec {
    /// Parses a spec from JSON, with the same keys as a `[toolset.<name>]` table. Keys the spec
    /// doesn't have are ignored, as in `giles.toml`.
    ///
    /// # Errors
    ///
    /// The key whose value has the wrong type, or the syntax error at the root. The reason can
    /// quote values of the keys that aren't credentials (`env` and `auth` take any value here,
    /// and validation reports them without quoting).
    pub fn from_json(json: &str) -> Result<Self, ToolsetError> {
        let mut deserializer = serde_json::Deserializer::from_str(json);
        serde_path_to_error::deserialize(&mut deserializer).map_err(|e| ToolsetError {
            key: KeyPath::from(e.path()),
            reason: e.inner().to_string(),
        })
    }

    /// Parses a spec from TOML text: the keys of a `[toolset.<name>]` table, at the top level of
    /// the text (as [`to_toml`](Self::to_toml) writes them). Unlike `giles.toml`, where an
    /// unknown key is a warning, a key the spec doesn't have is an error, so a typo in a file
    /// given to `giles toolset set` isn't silently dropped.
    ///
    /// # Errors
    ///
    /// The key whose value has the wrong type or that the spec doesn't have, or the syntax error
    /// at the root, with its line and column. As for [`from_json`](Self::from_json), the reason
    /// can quote values of the keys that aren't credentials.
    pub fn from_toml(text: &str) -> Result<Self, ToolsetError> {
        let position = |error: &toml::de::Error| {
            error
                .span()
                .map(|span| Position::of_offset(text, span.start))
                .map(|at| format!("line {}, column {}", at.line, at.column))
        };
        let deserializer = toml::Deserializer::parse(text).map_err(|error| ToolsetError {
            key: KeyPath::root(),
            reason: match position(&error) {
                Some(at) => format!("invalid TOML at {at}: {}", error.message()),
                None => format!("invalid TOML: {}", error.message()),
            },
        })?;
        let mut unknown = Vec::new();
        let mut record = |path: serde_ignored::Path<'_>| unknown.push(KeyPath::from(&path));
        let spec = serde_path_to_error::deserialize(serde_ignored::Deserializer::new(
            deserializer,
            &mut record,
        ))
        .map_err(
            |error: serde_path_to_error::Error<toml::de::Error>| ToolsetError {
                key: KeyPath::from(error.path()),
                reason: match position(error.inner()) {
                    Some(at) => format!("{} (at {at})", error.inner().message()),
                    None => error.inner().message().to_owned(),
                },
            },
        )?;
        match unknown.into_iter().next() {
            Some(key) => Err(ToolsetError {
                key,
                reason: "unknown key".to_owned(),
            }),
            None => Ok(spec),
        }
    }

    /// The spec as TOML text, the keys of a `[toolset.<name>]` table at the top level, with
    /// credentials as references, as they were written ([`from_toml`](Self::from_toml) reads it
    /// back as the same spec). Durations are whole milliseconds (`"300000ms"`).
    ///
    /// # Errors
    ///
    /// The spec can't be written as TOML: an `env` or `auth` that isn't a table with values TOML
    /// can hold. A validated spec can.
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        // Through a table, which writes the values of each table before its sub-tables, however
        // the spec's fields are ordered.
        toml::to_string(&toml::Table::try_from(self)?)
    }

    /// The hash of the spec for the tool-list cache (PLAN section 7.9), over the spec's keys with
    /// credentials as written, as references, so it never depends on a resolved secret value.
    ///
    /// It doesn't depend on key order, or on how a duration is spelled (`"5m"` and `"300s"` hash
    /// alike), but only on what the spec says: unlike the hash of a `giles.toml` table, it knows
    /// no keys the spec lacks.
    pub fn config_hash(&self) -> ConfigHash {
        let table = toml::Table::try_from(self)
            .expect("a spec serializes to a TOML table: durations and paths become strings");
        ConfigHash::of(&table)
    }
}

impl fmt::Debug for ToolsetSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolsetSpec")
            .field("command", &self.command)
            .field("args", &self.args)
            .field("env", &self.env.as_ref().map(|_| REDACTED))
            .field("url", &self.url)
            .field("auth", &self.auth.as_ref().map(|_| REDACTED))
            .field("insecure", &self.insecure)
            .field("deadline", &self.deadline)
            .field("idle_stop", &self.idle_stop)
            .field("parallel", &self.parallel)
            .field("skills", &self.skills)
            .field("sandbox", &self.sandbox)
            .field("unsandboxed", &self.unsandboxed)
            .finish()
    }
}

/// A duration as whole milliseconds (`"300000ms"`), so equal durations serialize alike however
/// they were spelled, and the result reads back as the same duration.
fn serialize_duration<S: Serializer>(
    duration: &Option<HumanDuration>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match duration {
        Some(HumanDuration(duration)) => {
            serializer.serialize_str(&format!("{}ms", duration.as_millis()))
        }
        None => serializer.serialize_none(),
    }
}

/// Paths as strings. TOML and JSON carry only UTF-8, so the lossy conversion changes only a path
/// built in code.
fn serialize_paths<S: Serializer>(
    paths: &Option<Vec<PathBuf>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match paths {
        Some(paths) => serializer.collect_seq(paths.iter().map(|path| path.to_string_lossy())),
        None => serializer.serialize_none(),
    }
}

/// What instance validation needs to know besides the raw table.
pub(crate) struct Ctx<'a> {
    pub file: &'a Path,
    pub root: &'a DataRoot,
    pub builtins: &'a Builtins,
    pub resolver: &'a Resolver<'a>,
    /// The keys found in `auth` tables that nothing recognizes: ignored, and reported by the
    /// caller (the rest are found by serde, in `parse_toml_lenient`).
    pub unknown: RefCell<Vec<KeyPath>>,
}

impl Ctx<'_> {
    fn err(&self, path: KeyPath, reason: impl Into<String>) -> ConfigError {
        ConfigError::at(self.file, path, reason)
    }

    /// Fails if `key` is set, saying why it doesn't apply.
    fn forbid(&self, path: &KeyPath, key: &str, set: bool, why: &str) -> Result<(), ConfigError> {
        if set {
            Err(self.err(path.key(key), format!("not allowed {why}")))
        } else {
            Ok(())
        }
    }

    /// The instance name (validated) and its path.
    fn name(&self, kind: Kind, name: &str) -> Result<(InstanceName, KeyPath), ConfigError> {
        let path = KeyPath::root().key(kind.as_str()).key(name);
        let name = InstanceName::new(name).map_err(|reason| self.err(path.clone(), reason))?;
        Ok((name, path))
    }

    /// The `component` key (default: the instance name), validated and resolved.
    fn component(
        &self,
        kind: Kind,
        path: &KeyPath,
        name: &InstanceName,
        component: Option<String>,
    ) -> Result<Component, ConfigError> {
        let component = component.unwrap_or_else(|| name.as_str().to_owned());
        if !is_valid_name(&component) {
            return Err(self.err(
                path.key("component"),
                invalid_name_reason("component", &component),
            ));
        }
        Ok(self.builtins.resolve(kind, &component))
    }

    fn config(
        &self,
        path: &KeyPath,
        config: Option<toml::Value>,
    ) -> Result<PluginConfig, ConfigError> {
        PluginConfig::from_toml(self.file, &path.key("config"), config, self.resolver)
    }

    pub fn provider(
        &self,
        name: &str,
        raw: RawProvider,
        config_hash: ConfigHash,
    ) -> Result<ProviderInstance, ConfigError> {
        let (name, path) = self.name(Kind::Provider, name)?;
        let component = match self.component(Kind::Provider, &path, &name, raw.component)? {
            Component::BuiltIn { name } => name,
            other => {
                return Err(self.err(
                    path.key("component"),
                    format!(
                        "no built-in provider {:?} (built-in: {}); provider plugins are not \
                         supported yet",
                        other.name(),
                        self.builtins.providers.join(", "),
                    ),
                ));
            }
        };
        Ok(ProviderInstance {
            config: self.config(&path, raw.config)?,
            name,
            component,
            config_hash,
        })
    }

    pub fn channel(
        &self,
        name: &str,
        raw: RawChannel,
        config_hash: ConfigHash,
    ) -> Result<ChannelInstance, ConfigError> {
        let (name, path) = self.name(Kind::Channel, name)?;
        let component = self.component(Kind::Channel, &path, &name, raw.component)?;
        let runtime = match (&component, raw.address) {
            (Component::BuiltIn { .. }, address) => {
                let why = "on a built-in channel";
                self.forbid(&path, "address", address.is_some(), why)?;
                self.forbid(&path, "auth", raw.auth.is_some(), why)?;
                self.forbid(&path, "insecure", raw.insecure.is_some(), why)?;
                self.forbid(&path, "sandbox", raw.sandbox.is_some(), why)?;
                self.forbid(&path, "unsandboxed", raw.unsandboxed.is_some(), why)?;
                self.forbid(&path, "max_blob", raw.max_blob.is_some(), why)?;
                ChannelRuntime::BuiltIn
            }
            (Component::Plugin { .. }, Some(address)) => {
                let why = "on a remote channel (one with `address`)";
                self.forbid(&path, "sandbox", raw.sandbox.is_some(), why)?;
                self.forbid(&path, "unsandboxed", raw.unsandboxed.is_some(), why)?;
                ChannelRuntime::Remote(self.remote(
                    &path,
                    "address",
                    &address,
                    raw.auth,
                    raw.insecure,
                    true,
                )?)
            }
            (Component::Plugin { binary, .. }, None) => {
                let why = "without `address`";
                self.forbid(&path, "auth", raw.auth.is_some(), why)?;
                self.forbid(&path, "insecure", raw.insecure.is_some(), why)?;
                ChannelRuntime::Plugin(Launch {
                    binary: binary.clone(),
                    sandbox: self.sandbox(
                        Kind::Channel,
                        &path,
                        &name,
                        raw.sandbox,
                        raw.unsandboxed,
                    )?,
                })
            }
        };
        let max_blob = raw.max_blob.map_or(DEFAULT_MAX_BLOB, |s| s.0);
        if max_blob == 0 {
            return Err(self.err(path.key("max_blob"), "must be more than 0 bytes"));
        }
        Ok(ChannelInstance {
            config: self.config(&path, raw.config)?,
            component: component.name().to_owned(),
            name,
            runtime,
            max_blob,
            config_hash,
        })
    }

    pub fn toolset(
        &self,
        name: &str,
        raw: ToolsetSpec,
        config_hash: ConfigHash,
    ) -> Result<ToolsetInstance, ConfigError> {
        let (name, path) = self.name(Kind::Toolset, name)?;
        let transport = if let Some(url) = raw.url {
            let why = "on a remote toolset (one with `url`)";
            self.forbid(&path, "command", raw.command.is_some(), why)?;
            self.forbid(&path, "args", raw.args.is_some(), why)?;
            self.forbid(&path, "env", raw.env.is_some(), why)?;
            self.forbid(&path, "idle_stop", raw.idle_stop.is_some(), why)?;
            self.forbid(&path, "sandbox", raw.sandbox.is_some(), why)?;
            self.forbid(&path, "unsandboxed", raw.unsandboxed.is_some(), why)?;
            ToolsetTransport::Http(self.remote(
                &path,
                "url",
                &url,
                raw.auth,
                raw.insecure,
                false,
            )?)
        } else {
            let why = "without `url`";
            self.forbid(&path, "auth", raw.auth.is_some(), why)?;
            self.forbid(&path, "insecure", raw.insecure.is_some(), why)?;
            let Some(command) = raw.command else {
                return Err(self.err(
                    path,
                    "missing `command`: set it to the MCP server's program, an absolute path or a \
                     name to look up on $PATH",
                ));
            };
            let command = self.command(&path, command)?;
            let idle_stop = raw.idle_stop.map(|d| d.0);
            if idle_stop == Some(Duration::ZERO) {
                return Err(self.err(path.key("idle_stop"), "must be longer than 0s"));
            }
            ToolsetTransport::Stdio(StdioServer {
                command,
                args: raw.args.unwrap_or_default(),
                env: self.env(&path.key("env"), raw.env)?,
                idle_stop,
                sandbox: self.sandbox(Kind::Toolset, &path, &name, raw.sandbox, raw.unsandboxed)?,
            })
        };
        let deadline = raw.deadline.map_or(DEFAULT_TOOL_DEADLINE, |d| d.0);
        if deadline.is_zero() {
            return Err(self.err(path.key("deadline"), "must be longer than 0s"));
        }
        let skills = raw.skills.unwrap_or_default();
        for (i, dir) in skills.iter().enumerate() {
            if !dir.is_absolute() {
                return Err(self.err(path.key("skills").index(i), "must be an absolute path"));
            }
        }
        Ok(ToolsetInstance {
            name,
            transport,
            deadline,
            parallel: raw.parallel.unwrap_or(false),
            skills,
            config_hash,
        })
    }

    /// A launched instance's `sandbox` and `unsandboxed`. Grants may not overlap the data root
    /// except the instance's own data directory, nor the socket directory.
    fn sandbox(
        &self,
        kind: Kind,
        path: &KeyPath,
        name: &InstanceName,
        raw: Option<SandboxSpec>,
        unsandboxed: Option<bool>,
    ) -> Result<Sandbox, ConfigError> {
        let unsandboxed = unsandboxed.unwrap_or(false);
        if unsandboxed && raw.is_some() {
            return Err(self.err(
                path.key("sandbox"),
                "has no effect with `unsandboxed = true`",
            ));
        }
        let own = self
            .root
            .plugin_data_dir(kind, name.as_str())
            .map_err(|e| self.err(path.clone(), e.to_string()))?;
        let protected = Protected::new(self.root.root(), self.root.socket_dir(), &own);
        Ok(Sandbox {
            unsandboxed,
            grants: sandbox::grants(
                self.file,
                &path.key("sandbox"),
                raw,
                kind == Kind::Toolset,
                &protected,
            )?,
        })
    }

    /// A stdio toolset's `command`: an absolute path or a bare program name.
    fn command(&self, path: &KeyPath, command: String) -> Result<String, ConfigError> {
        let path = path.key("command");
        if command.is_empty() {
            return Err(self.err(path, "must not be empty"));
        }
        if command.contains('/') && !Path::new(&command).is_absolute() {
            return Err(self.err(
                path,
                "must be an absolute path or a program name to look up on $PATH",
            ));
        }
        Ok(command)
    }

    /// A remote component's `address` or `url`, and `auth`. `oauth` says whether OAuth client
    /// credentials are supported (remote channels) or only bearer tokens (remote toolsets).
    fn remote(
        &self,
        path: &KeyPath,
        key: &str,
        text: &str,
        auth: Option<toml::Value>,
        insecure: Option<bool>,
        oauth: bool,
    ) -> Result<Remote, ConfigError> {
        let insecure = insecure.unwrap_or(false);
        let url =
            parse_remote_url(text, insecure).map_err(|reason| self.err(path.key(key), reason))?;
        let auth = auth
            .map(|auth| self.auth(&path.key("auth"), &auth, oauth))
            .transpose()?;
        Ok(Remote {
            url,
            auth,
            insecure,
        })
    }

    /// `auth = { bearer = <credential> }` or
    /// `auth = { oauth = { token_url, client_id, client_secret = <credential>, scopes } }`.
    ///
    /// Exactly one of `bearer` and `oauth` must be there: without a method, ignoring the table
    /// would silently leave the remote unauthenticated. Other keys are unknown and ignored.
    fn auth(&self, path: &KeyPath, auth: &toml::Value, oauth: bool) -> Result<Auth, ConfigError> {
        let shape = if oauth {
            "must be { bearer = ... } or { oauth = { ... } }"
        } else {
            "must be { bearer = ... }"
        };
        let table = auth
            .as_table()
            .ok_or_else(|| self.err(path.clone(), shape))?;
        let mut methods = table
            .iter()
            .filter(|(key, _)| matches!(key.as_str(), "bearer" | "oauth"));
        let (method, value) = match (methods.next(), methods.next()) {
            (Some(method), None) => method,
            (None, None) if table.len() == 1 => {
                let key = table.keys().next().expect("one entry");
                return Err(self.err(path.key(key), format!("unknown auth method; {shape}")));
            }
            _ => return Err(self.err(path.clone(), shape)),
        };
        self.unknown.borrow_mut().extend(
            table
                .keys()
                .filter(|key| *key != method)
                .map(|key| path.key(key)),
        );
        let at = path.key(method);
        match method.as_str() {
            "bearer" => Ok(Auth::Bearer(self.resolver.value(self.file, &at, value)?)),
            "oauth" if oauth => Ok(Auth::OAuth(Box::new(self.oauth(&at, value)?))),
            _ => Err(self.err(at, "remote toolsets support only `bearer` auth")),
        }
    }

    fn oauth(
        &self,
        path: &KeyPath,
        value: &toml::Value,
    ) -> Result<OAuthClientCredentials, ConfigError> {
        let table = value.as_table().ok_or_else(|| {
            self.err(
                path.clone(),
                "must be a table with token_url, client_id, client_secret",
            )
        })?;
        self.unknown.borrow_mut().extend(
            table
                .keys()
                .filter(|key| {
                    !matches!(
                        key.as_str(),
                        "token_url" | "client_id" | "client_secret" | "scopes"
                    )
                })
                .map(|key| path.key(key)),
        );
        let string = |key: &str| match table.get(key) {
            Some(toml::Value::String(s)) => Ok(s.clone()),
            Some(_) => Err(self.err(path.key(key), "must be a string")),
            None => Err(self.err(path.clone(), format!("missing field `{key}`"))),
        };
        let token_url = string("token_url")?;
        let token_url = parse_remote_url(&token_url, false)
            .map_err(|reason| self.err(path.key("token_url"), reason))?;
        let client_id = string("client_id")?;
        let client_secret = match table.get("client_secret") {
            Some(value) => self
                .resolver
                .value(self.file, &path.key("client_secret"), value)?,
            None => return Err(self.err(path.clone(), "missing field `client_secret`")),
        };
        let scopes = match table.get("scopes") {
            None => Vec::new(),
            Some(toml::Value::Array(items)) => items
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    item.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| self.err(path.key("scopes").index(i), "must be a string"))
                })
                .collect::<Result<_, _>>()?,
            Some(_) => return Err(self.err(path.key("scopes"), "must be an array of strings")),
        };
        Ok(OAuthClientCredentials {
            token_url,
            client_id,
            client_secret,
            scopes,
        })
    }

    /// A stdio toolset's `env`: a table of variable names to credentials.
    fn env(
        &self,
        path: &KeyPath,
        env: Option<toml::Value>,
    ) -> Result<BTreeMap<String, Secret>, ConfigError> {
        let Some(env) = env else {
            return Ok(BTreeMap::new());
        };
        let table = env.as_table().ok_or_else(|| {
            self.err(
                path.clone(),
                format!("must be a table, not {}", env.type_str()),
            )
        })?;
        table
            .iter()
            .map(|(name, value)| {
                let at = path.key(name);
                if name.is_empty() || name.contains(['=', '\0']) {
                    return Err(self.err(at, "invalid environment variable name"));
                }
                if DERIVED_ENV.contains(&name.as_str()) {
                    return Err(self.err(
                        at,
                        format!("{name} is set by Giles (the server's data directory or PATH)"),
                    ));
                }
                Ok((name.clone(), self.resolver.value(self.file, &at, value)?))
            })
            .collect()
    }
}

/// Parses a remote component's URL: `http` or `https`, with a host, and no user name or password
/// (credentials go in `auth`). Plain `http` to a host other than loopback (`localhost`,
/// `127.0.0.0/8`, `::1`) requires `insecure` (PLAN section 7.6).
///
/// Errors never quote the URL, which might carry credentials; the key path locates it.
pub(crate) fn parse_remote_url(text: &str, insecure: bool) -> Result<Url, String> {
    let url = Url::parse(text).map_err(|e| format!("invalid URL: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("invalid URL: the scheme must be http or https".to_owned());
    }
    let Some(host) = url.host() else {
        return Err("invalid URL: no host".to_owned());
    };
    if !url.username().is_empty() || url.password().is_some() {
        return Err("the URL must not contain credentials; use `auth`".to_owned());
    }
    let loopback = match host {
        Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
        Host::Ipv4(ip) => ip.is_loopback(),
        Host::Ipv6(ip) => ip.is_loopback(),
    };
    if url.scheme() == "http" && !loopback && !insecure {
        return Err(format!(
            "plain-text http to {host} lets anyone on the path inject messages or tool results; \
             use https, or set `insecure = true`"
        ));
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_urls() {
        let ok = [
            ("https://example.com/mcp", false),
            ("https://example.com:8443", false),
            ("http://localhost:9000", false),
            ("http://LOCALHOST:9000", false),
            ("http://127.0.0.1:9000", false),
            ("http://127.8.9.10", false),
            ("http://[::1]:9000", false),
            ("http://example.com", true),
            ("http://10.0.0.5:9000", true),
        ];
        for (text, insecure) in ok {
            assert!(parse_remote_url(text, insecure).is_ok(), "{text}");
        }
        let bad = [
            ("example.com:443", "scheme"),
            ("ftp://example.com", "scheme"),
            ("grpc://example.com", "scheme"),
            ("not a url", "invalid URL"),
            ("http://example.com", "insecure = true"),
            ("http://10.0.0.5:9000", "insecure = true"),
            ("http://[::2]", "insecure = true"),
            ("http://localhost.example.com", "insecure = true"),
            ("https://user:pw@example.com", "credentials"),
            ("https://user@example.com", "credentials"),
        ];
        for (text, reason) in bad {
            let err = parse_remote_url(text, false).unwrap_err();
            assert!(err.contains(reason), "{text}: {err}");
        }
        // Credentials are never echoed, even when the URL is invalid.
        for text in [
            "https://user:hunter2@example.com",
            "user:hunter2@example.com",
            "https://user:hunter2@exa mple.com",
        ] {
            let err = parse_remote_url(text, false).unwrap_err();
            assert!(!err.contains("hunter2"), "{err}");
        }
    }
}

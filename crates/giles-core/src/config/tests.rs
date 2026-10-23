//! Loading whole configurations from temporary data roots.

use std::path::{Path, PathBuf};
use std::time::Duration;

use giles_types::ConversationKind;
use tempfile::TempDir;

use super::*;

fn agent_id(name: &str) -> AgentId {
    AgentId::new(name).unwrap()
}

/// The smallest valid `giles.toml`.
const MINIMAL: &str = "[owner]\nidentities = [\"web:owner\"]\n";

/// A temporary data root.
struct Home {
    dir: TempDir,
    root: DataRoot,
}

impl Home {
    fn new(giles_toml: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path(), Path::new("/tmp")).unwrap();
        let home = Self { dir, root };
        home.write("giles.toml", giles_toml);
        home
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    fn write(&self, relative: &str, contents: &str) -> &Self {
        let path = self.path(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
        self
    }

    fn agent(&self, name: &str, agent_toml: &str) -> &Self {
        self.write(&format!("agents/{name}/agent.toml"), agent_toml)
    }

    /// Loads with the v1 built-ins and [`test_env`] as the environment.
    fn load(&self) -> Result<Config, ConfigError> {
        Config::load_with(
            &self.root,
            &LoadOptions {
                env: &test_env,
                ..LoadOptions::default()
            },
        )
    }

    /// Writes `secrets.toml` with `mode`.
    fn secrets(&self, contents: &str, mode: u32) -> &Self {
        self.write("secrets.toml", contents);
        std::fs::set_permissions(
            self.path("secrets.toml"),
            std::os::unix::fs::PermissionsExt::from_mode(mode),
        )
        .unwrap();
        self
    }

    fn load_ok(&self) -> Config {
        self.load().unwrap_or_else(|e| panic!("{e}"))
    }
}

/// The environment tests load with: only `GITHUB_TOKEN` is set.
fn test_env(name: &str) -> Option<std::ffi::OsString> {
    (name == "GITHUB_TOKEN").then(|| "env-github-token".into())
}

/// Asserts that loading fails in `file` (relative to the root) at `key` with a reason containing
/// `reason`.
#[track_caller]
fn assert_error(home: &Home, file: &str, key: &str, reason: &str) {
    let err = home
        .load()
        .expect_err("the configuration should be invalid");
    let shown = err.to_string();
    assert_eq!(err.file, home.path(file), "{shown}");
    assert_eq!(err.key.to_string(), key, "{shown}");
    assert!(
        err.reason.contains(reason),
        "{shown}\nexpected reason: {reason}"
    );
    // The display starts with the file, and names the key.
    assert!(
        shown.starts_with(&home.path(file).display().to_string()),
        "{shown}"
    );
    assert!(
        shown.contains(&format!(": {key}: ")) || key.is_empty(),
        "{shown}"
    );
}

#[test]
fn minimal_config_gets_defaults() {
    let config = Home::new(MINIMAL).load_ok();
    assert_eq!(config.core.http_listen, DEFAULT_HTTP_LISTEN);
    assert_eq!(config.core.http_listen.to_string(), "127.0.0.1:8080");
    assert_eq!(
        config.owner.identities,
        [Identity::new("web:owner").unwrap()]
    );
    assert_eq!(config.owner.default.as_str(), "web:owner");
    assert!(config.owner.timezone.is_none());
    assert!(config.providers.is_empty());
    assert!(config.channels.is_empty());
    assert!(config.toolsets.is_empty());
    assert!(config.routes.is_empty());
    assert!(config.agents.is_empty());
}

#[test]
fn missing_giles_toml_is_an_error() {
    let home = Home::new(MINIMAL);
    std::fs::remove_file(home.path("giles.toml")).unwrap();
    assert_error(&home, "giles.toml", "", "not found");
}

#[test]
fn invalid_utf8_is_an_error_with_a_position() {
    let home = Home::new(MINIMAL);
    std::fs::write(
        home.path("giles.toml"),
        b"[owner]\nidentities = [\"\xff\"]\n",
    )
    .unwrap();
    let err = home.load().unwrap_err();
    assert!(err.reason.contains("UTF-8"), "{err}");
    assert_eq!(
        err.position,
        Some(Position {
            line: 2,
            column: 16
        })
    );
}

#[test]
fn syntax_errors_have_a_position() {
    let home = Home::new("[owner]\nidentities = [\"web:owner\"\n");
    let err = home.load().unwrap_err();
    assert!(err.reason.starts_with("invalid TOML"), "{err}");
    assert_eq!(err.position.map(|p| p.line), Some(2), "{err}");
}

#[test]
fn type_errors_have_a_key_path_and_position() {
    let home = Home::new(&format!("{MINIMAL}[core]\nhttp_listen = 8080\n"));
    assert_error(&home, "giles.toml", "core.http_listen", "invalid type");
    let err = home.load().unwrap_err();
    assert_eq!(
        err.position,
        Some(Position {
            line: 4,
            column: 15
        }),
        "{err}"
    );
}

#[test]
fn errors_found_after_parsing_have_positions() {
    let pos = |line, column| Some(Position { line, column });
    // A key that's present: its position.
    let home = Home::new(
        "[owner]\nidentities = [\"a:b\", \"c:d\"]\n  default = \"e:f\"\n\n\
         [[routes]]\nagent = \"jeeves\"\n",
    );
    let err = home.load().unwrap_err();
    assert_eq!(err.key.to_string(), "owner.default");
    assert_eq!(err.position, pos(3, 3), "{err}");
    // An array item.
    home.write("giles.toml", "[owner]\nidentities = [\"a:b\", \"a:b\"]\n");
    let err = home.load().unwrap_err();
    assert_eq!(err.position, pos(2, 22), "{err}");
    // A missing key: the deepest part of its path that's there.
    home.write("giles.toml", "[owner]\nidentities = [\"a:b\", \"c:d\"]\n");
    let err = home.load().unwrap_err();
    assert_eq!(err.key.to_string(), "owner.default");
    assert_eq!(err.position, pos(1, 2), "{err}");
    // Through arrays of tables.
    home.write(
        "giles.toml",
        &format!("{MINIMAL}[[routes]]\nagent = \"x\"\n\n[[routes]]\n\nagent = \"y\"\n"),
    );
    let err = home.load().unwrap_err();
    assert_eq!(err.key.to_string(), "routes[0].agent");
    assert_eq!(err.position, pos(4, 1), "{err}");
    // In agent.toml too.
    let home = with_provider();
    home.agent(
        "jeeves",
        "models = [\"anthropic/m\"]\n[[jobs]]\nid = \"j\"\ncron = \"0 8 * *\"\nprompt = \"p\"\n",
    );
    let err = home.load().unwrap_err();
    assert_eq!(err.key.to_string(), "jobs[0].cron");
    assert_eq!(err.position, pos(4, 1), "{err}");
}

#[test]
fn owner_rules() {
    let cases = [
        ("", "owner", "missing"),
        (
            "[owner]\nidentities = []",
            "owner.identities",
            "at least one",
        ),
        (
            "[owner]\nidentities = [\"whatsapp\"]",
            "owner.identities[0]",
            "<namespace>:<native id>",
        ),
        (
            "[owner]\nidentities = [\"a:b\", \"a:b\"]",
            "owner.identities[1]",
            "listed twice",
        ),
        (
            "[owner]\nidentities = [\"a:b\", \"c:d\"]",
            "owner.default",
            "missing",
        ),
        (
            "[owner]\nidentities = [\"a:b\", \"c:d\"]\ndefault = \"e:f\"",
            "owner.default",
            "not one of",
        ),
        (
            "[owner]\nidentities = [\"a:b\"]\ntimezone = \"PST\"",
            "owner.timezone",
            "unknown time zone",
        ),
        (
            "[owner]\nidentities = [\"a:b\"]\nname = \"me\"",
            "owner.name",
            "unknown field `name`",
        ),
        (
            "[owner]\nidentity = [\"a:b\"]",
            "owner.identity",
            "unknown field `identity`",
        ),
    ];
    for (toml, key, reason) in cases {
        assert_error(&Home::new(toml), "giles.toml", key, reason);
    }

    let config = Home::new(
        "[owner]\nidentities = [\"whatsapp:+1555\", \"irc:libera.chat:account:me\"]\n\
         default = \"irc:libera.chat:account:me\"\ntimezone = \"America/Los_Angeles\"",
    )
    .load_ok();
    assert_eq!(config.owner.identities.len(), 2);
    assert_eq!(config.owner.default.as_str(), "irc:libera.chat:account:me");
    let tz = config.owner.timezone.unwrap();
    assert_eq!(tz.iana_name(), Some("America/Los_Angeles"));
}

#[test]
fn unknown_keys_are_rejected_everywhere_but_config_tables() {
    let cases = [
        ("[cores]\nx = 1", "cores", "unknown field `cores`"),
        (
            "[core]\nhttp = \"127.0.0.1:1\"",
            "core.http",
            "unknown field `http`",
        ),
        (
            "[provider.anthropic]\napi_key = \"x\"",
            "provider.anthropic.api_key",
            "unknown field `api_key`",
        ),
        (
            "[channel.irc]\nfoo = 1",
            "channel.irc.foo",
            "unknown field `foo`",
        ),
        (
            "[toolset.files]\nconfig = {}",
            "toolset.files.config",
            "unknown field `config`",
        ),
        (
            "[[routes]]\nagent = \"a\"\nuser = \"me\"",
            "routes[0].user",
            "unknown field `user`",
        ),
    ];
    for (toml, key, reason) in cases {
        let home = Home::new(&format!("{MINIMAL}{toml}\n"));
        assert_error(&home, "giles.toml", key, reason);
    }
    // Anything goes inside `config`.
    let home = Home::new(&format!(
        "{MINIMAL}[channel.irc]\nconfig = {{ anything = {{ at = [\"all\", 1] }} }}\n"
    ));
    let config = home.load_ok();
    let irc = &config.channels["irc"];
    assert_eq!(irc.config.to_json()["anything"]["at"][1], 1);
}

#[test]
fn instance_names_and_components() {
    let cases = [
        (
            "[channel.irc_libera]",
            "channel.irc_libera",
            "letters, digits, and `-`",
        ),
        (
            "[channel.\"a b\"]",
            "channel.\"a b\"",
            "letters, digits, and `-`",
        ),
        (
            "[channel.irc]\ncomponent = \"irc.v2\"",
            "channel.irc.component",
            "invalid component name",
        ),
        (
            "[provider.x]\ncomponent = \"\"",
            "provider.x.component",
            "invalid component name",
        ),
        (
            "[provider.ollama]",
            "provider.ollama.component",
            "provider plugins are not supported",
        ),
        (
            "[provider.web]\ncomponent = \"openai\"\n[channel.web]",
            "channel.web",
            "`[provider.web]` also uses \"web\"",
        ),
        (
            "[channel.github]\n[toolset.github]\nurl = \"https://x\"",
            "toolset.github",
            "unique across kinds",
        ),
        (
            "[channel.Libera]\ncomponent = \"irc\"\n[toolset.libera]\nurl = \"https://x\"",
            "toolset.libera",
            "`[channel.Libera]` also uses \"libera\"",
        ),
        (
            "[channel.IRC]\n[channel.irc]",
            "channel.irc",
            "ignoring case",
        ),
    ];
    for (toml, key, reason) in cases {
        let home = Home::new(&format!("{MINIMAL}{toml}\n"));
        assert_error(&home, "giles.toml", key, reason);
    }
}

#[test]
fn providers_are_built_in() {
    let config = Home::new(&format!(
        "{MINIMAL}[provider.anthropic]\n\
         [provider.openrouter]\ncomponent = \"openai\"\n\
         config = {{ base_url = \"https://openrouter.ai/api/v1\" }}\n"
    ))
    .load_ok();
    let anthropic = &config.providers["anthropic"];
    assert_eq!(anthropic.component, "anthropic");
    assert_eq!(anthropic.config.to_json(), serde_json::json!({}));
    let openrouter = &config.providers["openrouter"];
    assert_eq!(openrouter.component, "openai");
    assert_eq!(
        openrouter.config.to_json()["base_url"],
        "https://openrouter.ai/api/v1"
    );

    // With other built-ins, the same name is a plugin, which providers can't be yet.
    let home = Home::new(&format!("{MINIMAL}[provider.anthropic]\n"));
    let builtins = Builtins {
        providers: &["openai"],
        channels: &[],
    };
    let options = LoadOptions {
        builtins,
        ..LoadOptions::default()
    };
    let err = Config::load_with(&home.root, &options).unwrap_err();
    assert_eq!(err.key.to_string(), "provider.anthropic.component");
    assert!(err.reason.contains("built-in: openai"), "{err}");
}

#[test]
fn channels() {
    let config = Home::new(&format!(
        "{MINIMAL}[channel.web]\n\
         [channel.libera]\ncomponent = \"irc\"\nmax_blob = \"10 MiB\"\nunsandboxed = true\n\
         config = {{ server = \"irc.libera.chat:6697\" }}\n\
         [channel.irc]\n\
         [channel.mail]\ncomponent = \"email\"\naddress = \"https://mail.example.com:7443\"\n\
         [channel.local]\ncomponent = \"irc\"\naddress = \"http://127.0.0.1:7000\"\n\
         [channel.lan]\ncomponent = \"irc\"\naddress = \"http://10.0.0.2:7000\"\ninsecure = true\n"
    ))
    .load_ok();

    let web = &config.channels["web"];
    assert_eq!(web.component, "web");
    assert!(matches!(web.runtime, ChannelRuntime::BuiltIn));

    let libera = &config.channels["libera"];
    assert_eq!(libera.component, "irc");
    assert_eq!(libera.max_blob, 10 << 20);
    let ChannelRuntime::Plugin(launch) = &libera.runtime else {
        panic!("{:?}", libera.runtime)
    };
    assert_eq!(launch.binary, "giles-channel-irc");
    assert!(launch.sandbox.unsandboxed);

    let irc = &config.channels["irc"];
    assert_eq!(irc.max_blob, DEFAULT_MAX_BLOB);
    let ChannelRuntime::Plugin(launch) = &irc.runtime else {
        panic!("{:?}", irc.runtime)
    };
    assert_eq!(launch.binary, "giles-channel-irc");
    assert!(!launch.sandbox.unsandboxed);

    let mail = &config.channels["mail"];
    assert_eq!(mail.component, "email");
    let ChannelRuntime::Remote(remote) = &mail.runtime else {
        panic!("{:?}", mail.runtime)
    };
    assert_eq!(remote.url.as_str(), "https://mail.example.com:7443/");
    assert!(!remote.insecure);
    assert!(matches!(
        &config.channels["local"].runtime,
        ChannelRuntime::Remote(_)
    ));
    let ChannelRuntime::Remote(lan) = &config.channels["lan"].runtime else {
        panic!()
    };
    assert!(lan.insecure);
}

#[test]
fn channel_rules() {
    let cases = [
        (
            "[channel.web]\naddress = \"https://x\"",
            "channel.web.address",
            "built-in",
        ),
        (
            "[channel.web]\nmax_blob = 1",
            "channel.web.max_blob",
            "built-in",
        ),
        (
            "[channel.web]\nunsandboxed = true",
            "channel.web.unsandboxed",
            "built-in",
        ),
        (
            "[channel.web]\ninsecure = true",
            "channel.web.insecure",
            "built-in",
        ),
        (
            "[channel.irc]\ninsecure = true",
            "channel.irc.insecure",
            "without `address`",
        ),
        (
            "[channel.irc]\naddress = \"https://x\"\nunsandboxed = true",
            "channel.irc.unsandboxed",
            "remote channel",
        ),
        (
            "[channel.irc]\naddress = \"http://irc.example.com\"",
            "channel.irc.address",
            "insecure = true",
        ),
        (
            "[channel.irc]\naddress = \"irc.example.com:7000\"",
            "channel.irc.address",
            "http or https",
        ),
        (
            "[channel.irc]\nmax_blob = 0",
            "channel.irc.max_blob",
            "more than 0",
        ),
        (
            "[channel.irc]\nmax_blob = \"50MB\"",
            "channel.irc.max_blob",
            "invalid size",
        ),
        (
            "[channel.irc]\nmax_blob = -5",
            "channel.irc.max_blob",
            "negative",
        ),
        (
            "[channel.irc]\nconfig = { x = nan }",
            "channel.irc.config.x",
            "NaN",
        ),
        (
            "[channel.irc]\nconfig = 5",
            "channel.irc.config",
            "must be a table, not integer",
        ),
    ];
    for (toml, key, reason) in cases {
        let home = Home::new(&format!("{MINIMAL}{toml}\n"));
        assert_error(&home, "giles.toml", key, reason);
    }
}

#[test]
fn toolsets() {
    let config = Home::new(&format!(
        "{MINIMAL}[toolset.github]\nurl = \"https://api.githubcopilot.com/mcp/\"\n\
         deadline = \"2m\"\nparallel = true\n\
         [toolset.files]\ncommand = \"/opt/homebrew/bin/mcp-server-filesystem\"\n\
         args = [\"/Users/me/Documents\"]\nidle_stop = \"30m\"\nskills = [\"/opt/skills\"]\n\
         [toolset.fetch]\ncommand = \"uvx\"\n\
         [toolset.shell]\n\
         [toolset.sh2]\ncomponent = \"shell\"\n"
    ))
    .load_ok();

    let github = &config.toolsets["github"];
    assert_eq!(github.deadline, Duration::from_secs(120));
    assert!(github.parallel);
    let ToolsetTransport::Http(remote) = &github.transport else {
        panic!("{:?}", github.transport)
    };
    assert_eq!(remote.url.as_str(), "https://api.githubcopilot.com/mcp/");

    let files = &config.toolsets["files"];
    assert_eq!(files.deadline, DEFAULT_TOOL_DEADLINE);
    assert!(!files.parallel);
    assert_eq!(files.skills, [PathBuf::from("/opt/skills")]);
    let ToolsetTransport::Stdio(stdio) = &files.transport else {
        panic!("{:?}", files.transport)
    };
    assert_eq!(
        stdio.program,
        Program::Command("/opt/homebrew/bin/mcp-server-filesystem".into())
    );
    assert_eq!(stdio.args, ["/Users/me/Documents"]);
    assert_eq!(stdio.idle_stop, Some(Duration::from_secs(1800)));

    let program = |name: &str| match &config.toolsets[name].transport {
        ToolsetTransport::Stdio(stdio) => stdio.program.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(program("fetch"), Program::Command("uvx".into()));
    let plugin = |component: &str| Program::Plugin {
        component: component.into(),
        binary: format!("giles-toolset-{component}"),
    };
    assert_eq!(program("shell"), plugin("shell"));
    assert_eq!(program("sh2"), plugin("shell"));
}

#[test]
fn toolset_rules() {
    let cases = [
        (
            "url = \"https://x\"\ncommand = \"/bin/x\"",
            "command",
            "remote toolset",
        ),
        (
            "url = \"https://x\"\ncomponent = \"x\"",
            "component",
            "remote toolset",
        ),
        ("url = \"https://x\"\nargs = []", "args", "remote toolset"),
        (
            "url = \"https://x\"\nidle_stop = \"1m\"",
            "idle_stop",
            "remote toolset",
        ),
        (
            "url = \"https://x\"\nunsandboxed = true",
            "unsandboxed",
            "remote toolset",
        ),
        ("url = \"http://example.com/mcp\"", "url", "insecure = true"),
        ("insecure = true", "insecure", "without `url`"),
        (
            "command = \"/bin/x\"\ncomponent = \"x\"",
            "component",
            "together with `command`",
        ),
        ("command = \"bin/x\"", "command", "absolute path"),
        ("command = \"\"", "command", "not be empty"),
        ("component = \"a_b\"", "component", "invalid component name"),
        ("deadline = \"5\"", "deadline", "invalid duration"),
        ("deadline = \"0s\"", "deadline", "longer than 0s"),
        ("idle_stop = \"0ms\"", "idle_stop", "longer than 0s"),
        ("env = { HOME = \"x\" }", "env.HOME", "HOME is set by Giles"),
        ("env = { PATH = \"x\" }", "env.PATH", "PATH is set by Giles"),
        (
            "env = { GILES_PLUGIN_DATA = \"x\" }",
            "env.GILES_PLUGIN_DATA",
            "is set by Giles",
        ),
        (
            "skills = [\"/ok\", \"relative\"]",
            "skills[1]",
            "absolute path",
        ),
        ("parallel = \"yes\"", "parallel", "invalid type"),
        ("max_blob = 5", "max_blob", "unknown field `max_blob`"),
    ];
    for (toml, key, reason) in cases {
        let home = Home::new(&format!("{MINIMAL}[toolset.t]\n{toml}\n"));
        let key = if key.is_empty() {
            "toolset.t".to_owned()
        } else {
            format!("toolset.t.{key}")
        };
        assert_error(&home, "giles.toml", &key, reason);
    }
}

const PROVIDER: &str = "[provider.anthropic]\n[provider.ollama]\ncomponent = \"openai\"\n";

fn with_provider() -> Home {
    Home::new(&format!("{MINIMAL}{PROVIDER}"))
}

#[test]
fn routes() {
    let home = Home::new(&format!(
        "{MINIMAL}{PROVIDER}[channel.web]\n[channel.libera]\ncomponent = \"irc\"\n\
         [[routes]]\nchannel = \"libera\"\nconversation = \"#giles\"\nagent = \"jeeves\"\n\
         [[routes]]\nkind = \"direct\"\nagent = \"jeeves\"\n\
         [[routes]]\nagent = \"bunter\"\n"
    ));
    home.agent("jeeves", "models = [\"anthropic/claude-sonnet-4-5\"]")
        .agent("bunter", "models = [\"ollama/qwen3:8b\"]");
    let config = home.load_ok();
    let routes = &config.routes;
    assert_eq!(routes.len(), 3);
    assert_eq!(routes[0].channel.as_ref().unwrap().as_str(), "libera");
    assert_eq!(routes[0].conversation.as_deref(), Some("#giles"));
    assert_eq!(routes[0].kind, None);
    assert_eq!(routes[0].agent.as_str(), "jeeves");
    assert_eq!(routes[1].channel, None);
    assert_eq!(routes[1].kind, Some(ConversationKind::Direct));
    assert_eq!(routes[2].agent.as_str(), "bunter");

    let cases = [
        (
            "channel = \"irc\"\nagent = \"jeeves\"",
            "routes[0].channel",
            "no channel instance",
        ),
        (
            "channel = \"a_b\"\nagent = \"jeeves\"",
            "routes[0].channel",
            "invalid instance name",
        ),
        (
            "agent = \"wooster\"",
            "routes[0].agent",
            "no agent \"wooster\"",
        ),
        ("agent = \"a.b\"", "routes[0].agent", "invalid agent name"),
        (
            "kind = \"dm\"\nagent = \"jeeves\"",
            "routes[0].kind",
            "unknown variant `dm`",
        ),
        (
            "conversation = \"\"\nagent = \"jeeves\"",
            "routes[0].conversation",
            "not be empty",
        ),
        ("channel = \"web\"", "routes[0]", "missing field `agent`"),
    ];
    for (toml, key, reason) in cases {
        let home = Home::new(&format!(
            "{MINIMAL}{PROVIDER}[channel.web]\n[[routes]]\n{toml}\n"
        ));
        home.agent("jeeves", "models = [\"anthropic/claude-sonnet-4-5\"]");
        assert_error(&home, "giles.toml", key, reason);
    }
}

#[test]
fn agent_defaults() {
    let home = with_provider();
    home.agent("jeeves", "models = [\"anthropic/claude-sonnet-4-5\"]");
    let config = home.load_ok();
    let agent = &config.agents[&agent_id("jeeves")];
    assert_eq!(agent.id.as_str(), "jeeves");
    assert_eq!(agent.dir, home.path("agents/jeeves"));
    assert_eq!(agent.name, None);
    assert_eq!(agent.models[0].to_string(), "anthropic/claude-sonnet-4-5");
    assert_eq!(agent.compaction_model, None);
    assert!(agent.tools.is_empty());
    assert!(agent.ask.is_empty());
    assert_eq!(agent.max_iterations, None);
    assert_eq!(agent.max_iterations_jobs, DEFAULT_MAX_ITERATIONS_JOBS);
    assert_eq!(agent.max_iterations_jobs, 25);
    assert_eq!(agent.busy_mode, BusyMode::Steer);
    assert_eq!(agent.memory_cap, 2200);
    assert_eq!(agent.approval_timeout, Duration::from_secs(600));
    assert!(agent.timezone.is_none());
    assert_eq!(agent.idle_reset, None);
    assert!(agent.jobs.is_empty());
}

#[test]
fn validate_agent_checks_a_source_as_loading_does() {
    let home = with_provider();
    let config = home.load_ok();
    let agent = config
        .validate_agent(
            &home.root,
            "bunter",
            "name = \"Bunter\"\nmodels = [\"anthropic/claude-sonnet-4-5\"]\n",
        )
        .unwrap();
    assert_eq!(agent.id.as_str(), "bunter");
    assert_eq!(agent.dir, home.path("agents/bunter"));
    assert_eq!(agent.name.as_deref(), Some("Bunter"));
    // Nothing was written, and the loaded configuration is unchanged.
    assert!(!home.path("agents/bunter").exists());
    assert!(config.agents.is_empty());

    let error = |id: &str, source: &str| config.validate_agent(&home.root, id, source).unwrap_err();
    let bad_id = error("../x", "models = [\"anthropic/m\"]");
    assert_eq!(bad_id.file, home.path("agents"));
    assert!(bad_id.reason.contains("agent"), "{bad_id}");
    let unknown = error("bunter", "models = [\"nope/m\"]");
    assert_eq!(unknown.file, home.path("agents/bunter/agent.toml"));
    assert_eq!(unknown.key.to_string(), "models[0]");
    assert!(unknown.reason.contains("no provider instance"), "{unknown}");
    // Positioned in the source, like a file loaded at startup.
    assert!(unknown.position.is_some(), "{unknown}");
    let glob = error("bunter", "models = [\"anthropic/m\"]\ntools = [\"a b\"]");
    assert_eq!(glob.key.to_string(), "tools[0]");
    let unknown_key = error("bunter", "models = [\"anthropic/m\"]\nowner = \"x\"");
    assert!(
        unknown_key.reason.contains("unknown field"),
        "{unknown_key}"
    );
}

#[test]
fn live_config_adds_agents_without_changing_earlier_snapshots() {
    let home = with_provider();
    home.agent("jeeves", "models = [\"anthropic/claude-sonnet-4-5\"]");
    let config = home.load_ok();
    let agent = |id: &str| {
        config
            .validate_agent(&home.root, id, "models = [\"anthropic/m\"]")
            .unwrap()
    };
    let live = LiveConfig::new(std::sync::Arc::new(config.clone()));
    let before = live.get();
    let after = live.add_agent(agent("bunter")).unwrap();
    assert_eq!(after.agents.len(), 2);
    assert!(std::sync::Arc::ptr_eq(&after, &live.get()));
    assert_eq!(
        before.agents.len(),
        1,
        "an earlier snapshot keeps its agents"
    );
    // A clone shares the configuration.
    assert!(live.clone().get().agents.contains_key(&agent_id("bunter")));

    // Ids are taken ignoring case; a refusal changes nothing.
    for (id, taken) in [("bunter", "bunter"), ("JEEVES", "jeeves")] {
        assert_eq!(live.add_agent(agent(id)).unwrap_err().0.as_str(), taken);
    }
    assert!(std::sync::Arc::ptr_eq(&after, &live.get()));
}

#[test]
fn agent_settings() {
    let home = with_provider();
    home.write("agents/jeeves/morning.md", "Good morning.")
        .agent(
            "jeeves",
            r#"
            name = "Jeeves"
            models = ["anthropic/claude-sonnet-4-5", "ollama/anthropic/claude"]
            compaction_model = "ollama/qwen3:8b"
            tools = ["memory", "github__*"]
            ask = ["github__create_*"]
            max_iterations = 50
            max_iterations_jobs = 10
            busy_mode = "queue"
            memory_cap = 4000
            approval_timeout = "5m"
            timezone = "Europe/London"
            idle_reset = "12h"

            [[jobs]]
            id = "morning"
            cron = "0 8 * * 1-5"
            prompt_file = "morning.md"

            [[jobs]]
            id = "check-in"
            cron = "*/30 * * * *"
            session = "dm"
            idle_only = true
            active_hours = "08:00-22:00"
            model = "ollama/qwen3:8b"
            prompt = "Anything to tell me?"
            ask_to = "deliver_to"

            [[jobs]]
            id = "digest"
            cron = "0 18 * * *"
            prompt = "Summarize the day."
            dm_excerpt = 20
            "#,
        );
    let config = home.load_ok();
    let agent = &config.agents[&agent_id("jeeves")];
    assert_eq!(agent.name.as_deref(), Some("Jeeves"));
    assert_eq!(agent.models[1].provider.as_str(), "ollama");
    assert_eq!(agent.models[1].model, "anthropic/claude");
    assert_eq!(agent.compaction_model.as_ref().unwrap().model, "qwen3:8b");
    assert_eq!(agent.tools, ["memory", "github__*"]);
    assert_eq!(agent.ask, ["github__create_*"]);
    assert_eq!(agent.max_iterations, Some(50));
    assert_eq!(agent.max_iterations_jobs, 10);
    assert_eq!(agent.busy_mode, BusyMode::Queue);
    assert_eq!(agent.memory_cap, 4000);
    assert_eq!(agent.approval_timeout, Duration::from_secs(300));
    assert_eq!(
        agent.timezone.as_ref().unwrap().iana_name(),
        Some("Europe/London")
    );
    assert_eq!(agent.idle_reset, Some(Duration::from_secs(12 * 3600)));

    let [morning, check_in, digest] = &agent.jobs[..] else {
        panic!("{:?}", agent.jobs)
    };
    assert_eq!(morning.id, "morning");
    assert_eq!(morning.session, JobSession::Isolated);
    assert!(!morning.idle_only);
    assert_eq!(morning.active_hours, None);
    assert_eq!(morning.model, None);
    assert_eq!(
        morning.prompt,
        JobPrompt::File(home.path("agents/jeeves/morning.md"))
    );
    assert_eq!(morning.ask_to, None);
    assert_eq!(morning.dm_excerpt, None);

    assert_eq!(check_in.session, JobSession::Dm);
    assert!(check_in.idle_only);
    assert_eq!(
        check_in.active_hours,
        Some(ActiveHours::parse("08:00-22:00").unwrap())
    );
    assert_eq!(
        check_in.model.as_ref().unwrap().to_string(),
        "ollama/qwen3:8b"
    );
    assert_eq!(
        check_in.prompt,
        JobPrompt::Inline("Anything to tell me?".into())
    );
    assert_eq!(check_in.ask_to, Some(AskTo::DeliverTo));

    assert_eq!(digest.dm_excerpt, Some(20));
}

#[test]
fn agent_rules() {
    let model = "models = [\"anthropic/m\"]";
    let job = "[[jobs]]\nid = \"j\"\ncron = \"0 8 * * *\"\nprompt = \"p\"";
    let cases: Vec<(String, &str, &str)> = vec![
        ("name = \"x\"".into(), "", "missing field `models`"),
        ("models = []".into(), "models", "at least one model"),
        (
            "models = [\"claude\"]".into(),
            "models[0]",
            "<provider instance>/<model id>",
        ),
        (
            "models = [\"anthropic/m\", \"openai/m\"]".into(),
            "models[1]",
            "no provider instance \"openai\"",
        ),
        (
            format!("{model}\ncompaction_model = \"x/y\""),
            "compaction_model",
            "no provider instance",
        ),
        (
            format!("{model}\nmax_iterations = 0"),
            "max_iterations",
            "at least 1",
        ),
        (
            format!("{model}\nmax_iterations_jobs = 0"),
            "max_iterations_jobs",
            "at least 1",
        ),
        (
            format!("{model}\nmax_iterations = -1"),
            "max_iterations",
            "invalid value",
        ),
        (
            format!("{model}\nmemory_cap = 0"),
            "memory_cap",
            "at least 1",
        ),
        (
            format!("{model}\nbusy_mode = \"wait\""),
            "busy_mode",
            "unknown variant `wait`",
        ),
        (
            format!("{model}\napproval_timeout = \"10\""),
            "approval_timeout",
            "invalid duration",
        ),
        (
            format!("{model}\napproval_timeout = \"0s\""),
            "approval_timeout",
            "longer than 0s",
        ),
        (
            format!("{model}\nidle_reset = \"0m\""),
            "idle_reset",
            "longer than 0s",
        ),
        (
            format!("{model}\ntimezone = \"Nowhere\""),
            "timezone",
            "unknown time zone",
        ),
        (
            format!("{model}\ntools = [\"memory\", \"\"]"),
            "tools[1]",
            "invalid tool glob",
        ),
        (
            format!("{model}\nask = [\"a b\"]"),
            "ask[0]",
            "invalid tool glob",
        ),
        (
            format!("{model}\nowner = \"stephen\""),
            "owner",
            "unknown field `owner`",
        ),
        (
            format!("{model}\n[[jobs]]\ncron = \"0 8 * * *\"\nprompt = \"p\""),
            "jobs[0]",
            "missing field `id`",
        ),
        (
            format!("{model}\n{job}\n{job}"),
            "jobs[1].id",
            "already has id \"j\"",
        ),
        (
            format!("{model}\n[[jobs]]\nid = \"a_b\"\ncron = \"0 8 * * *\"\nprompt = \"p\""),
            "jobs[0].id",
            "invalid job name",
        ),
        (
            format!("{model}\n[[jobs]]\nid = \"j\"\ncron = \"0 8 * *\"\nprompt = \"p\""),
            "jobs[0].cron",
            "invalid cron pattern",
        ),
        (
            format!("{model}\n[[jobs]]\nid = \"j\"\ncron = \"0 8 * * *\""),
            "jobs[0]",
            "missing `prompt` or `prompt_file`",
        ),
        (
            format!("{model}\n{job}\nprompt_file = \"p.md\""),
            "jobs[0].prompt_file",
            "not both",
        ),
        (
            format!("{model}\n[[jobs]]\nid = \"j\"\ncron = \"0 8 * * *\"\nprompt = \" \""),
            "jobs[0].prompt",
            "not be empty",
        ),
        (
            format!(
                "{model}\n[[jobs]]\nid = \"j\"\ncron = \"0 8 * * *\"\nprompt_file = \"none.md\""
            ),
            "jobs[0].prompt_file",
            "does not exist",
        ),
        (
            format!(
                "{model}\n[[jobs]]\nid = \"j\"\ncron = \"0 8 * * *\"\nprompt_file = \"../x.md\""
            ),
            "jobs[0].prompt_file",
            "no `..`",
        ),
        (
            format!("{model}\n{job}\nactive_hours = \"8-22\""),
            "jobs[0].active_hours",
            "HH:MM-HH:MM",
        ),
        (
            format!("{model}\n{job}\nsession = \"dm\"\ndm_excerpt = 5"),
            "jobs[0].dm_excerpt",
            "only for",
        ),
        (
            format!("{model}\n{job}\nsession = \"group\""),
            "jobs[0].session",
            "unknown variant `group`",
        ),
        (
            format!("{model}\n{job}\nask_to = \"owner\""),
            "jobs[0].ask_to",
            "unknown variant `owner`",
        ),
        (
            format!("{model}\n{job}\nmodel = \"nope/m\""),
            "jobs[0].model",
            "no provider instance",
        ),
        (
            format!("{model}\n{job}\ndeliver_to = \"stephen\""),
            "jobs[0].deliver_to",
            "unknown field `deliver_to`",
        ),
    ];
    for (toml, key, reason) in cases {
        let home = with_provider();
        home.agent("jeeves", &toml);
        assert_error(&home, "agents/jeeves/agent.toml", key, reason);
    }
}

#[test]
fn agent_directories() {
    let home = with_provider();
    home.agent("jeeves", "models = [\"anthropic/m\"]")
        .write("agents/README.md", "ignored")
        .write(".hidden/agent.toml", "ignored")
        .write("agents/.trash/agent.toml", "not even TOML");
    let config = home.load_ok();
    assert_eq!(
        config
            .agents
            .keys()
            .map(AgentId::as_str)
            .collect::<Vec<_>>(),
        ["jeeves"]
    );

    let home = with_provider();
    home.agent("my_agent", "models = [\"anthropic/m\"]");
    assert_error(&home, "agents/my_agent", "", "invalid agent name");

    let home = with_provider();
    std::fs::create_dir_all(home.path("agents/empty")).unwrap();
    assert_error(&home, "agents/empty", "", "needs an agent.toml");
}

/// The `toml` code blocks of a Markdown document whose first line is `# ~/.giles/<path>`
/// (optionally followed by a note), as (path, contents).
fn documented_files(markdown: &str) -> Vec<(&str, String)> {
    let mut files = Vec::new();
    let mut lines = markdown.lines();
    while let Some(line) = lines.next() {
        if line != "```toml" {
            continue;
        }
        let block: Vec<&str> = lines.by_ref().take_while(|l| *l != "```").collect();
        let path = block
            .first()
            .and_then(|l| l.strip_prefix("# ~/.giles/"))
            .and_then(|l| l.split_whitespace().next());
        if let Some(path) = path {
            files.push((path, block.join("\n")));
        }
    }
    files
}

#[test]
fn the_documented_full_example_is_valid() {
    let doc = include_str!("../../../../Docs/user/configuration.md");
    let files = documented_files(doc);
    let paths: Vec<&str> = files.iter().map(|(path, _)| *path).collect();
    assert_eq!(
        paths,
        ["giles.toml", "secrets.toml", "agents/jeeves/agent.toml"]
    );
    let home = Home::new("");
    for (path, contents) in &files {
        home.write(path, contents);
    }
    home.secrets(&files[1].1, 0o600);
    home.write("agents/jeeves/morning.md", "Plan my day.");
    let config = home.load_ok();
    assert_eq!(config.agents.len(), 1);
    assert_eq!(config.routes.len(), 2);
    assert_eq!(config.agents[&agent_id("jeeves")].jobs.len(), 2);
    let libera = config.channels["libera"].config.to_json();
    assert_eq!(libera["sasl"]["password"], "correct horse battery staple");
}

/// Instances whose credentials come from `secrets.toml`, the environment, and literals.
const WITH_SECRETS: &str = r#"
[provider.anthropic]
config = { api_key = { secret = "anthropic" }, region = "us" }

[channel.mail]
component = "email"
address = "https://mail.example.com:7443"
auth = { oauth = { token_url = "https://auth.example.com/token", client_id = "giles", client_secret = { secret = "mail" }, scopes = ["chat", "admin"] } }

[channel.relay]
component = "irc"
address = "https://relay.example.com"
auth = { bearer = "literal-bearer" }

[toolset.github]
url = "https://api.githubcopilot.com/mcp/"
auth = { bearer = { env = "GITHUB_TOKEN" } }

[toolset.files]
command = "/bin/files"
env = { FILES_TOKEN = { secret = "files" }, MODE = "ro" }
"#;

const SECRETS: &str =
    "anthropic = \"sk-ant-1\"\nmail = \"mail-secret\"\nfiles = \"files-secret\"\n";

#[test]
fn secret_references_resolve() {
    let home = Home::new(&format!("{MINIMAL}{WITH_SECRETS}"));
    home.secrets(SECRETS, 0o600);
    let config = home.load_ok();

    let anthropic = &config.providers["anthropic"].config;
    assert_eq!(anthropic.to_json()["api_key"], "sk-ant-1");
    assert_eq!(anthropic.redacted()["api_key"], "<redacted>");
    assert_eq!(anthropic.redacted()["region"], "us");

    let ChannelRuntime::Remote(mail) = &config.channels["mail"].runtime else {
        panic!()
    };
    let Some(Auth::OAuth(oauth)) = &mail.auth else {
        panic!("{:?}", mail.auth)
    };
    assert_eq!(oauth.token_url.as_str(), "https://auth.example.com/token");
    assert_eq!(oauth.client_id, "giles");
    assert_eq!(oauth.client_secret.expose(), "mail-secret");
    assert_eq!(oauth.scopes, ["chat", "admin"]);

    let ChannelRuntime::Remote(relay) = &config.channels["relay"].runtime else {
        panic!()
    };
    assert_eq!(
        relay.auth,
        Some(Auth::Bearer(Secret::new("literal-bearer")))
    );

    let ToolsetTransport::Http(github) = &config.toolsets["github"].transport else {
        panic!()
    };
    assert_eq!(
        github.auth,
        Some(Auth::Bearer(Secret::new("env-github-token")))
    );

    let ToolsetTransport::Stdio(files) = &config.toolsets["files"].transport else {
        panic!()
    };
    assert_eq!(files.env["FILES_TOKEN"].expose(), "files-secret");
    assert_eq!(files.env["MODE"].expose(), "ro");

    // Nothing that holds a credential shows it in `Debug`.
    let debug = format!("{config:?}");
    for value in [
        "sk-ant-1",
        "mail-secret",
        "files-secret",
        "env-github-token",
        "literal-bearer",
    ] {
        assert!(!debug.contains(value), "{value} in {debug}");
    }
    assert!(debug.contains("<redacted>"));
}

#[test]
fn secrets_toml_must_be_private() {
    for mode in [0o644, 0o640, 0o604, 0o660] {
        let home = Home::new(MINIMAL);
        home.secrets(SECRETS, mode);
        assert_error(&home, "secrets.toml", "", "group or others");
    }
    // Checked even when nothing refers to it.
    let home = Home::new(MINIMAL);
    home.secrets("", 0o644);
    assert_error(&home, "secrets.toml", "", "chmod 600");
}

#[test]
fn secret_resolution_errors() {
    let cases = [
        (
            "[provider.anthropic]\nconfig = { api_key = { secret = \"nope\" } }",
            "provider.anthropic.config.api_key",
            "no secret \"nope\" in",
        ),
        (
            "[toolset.t]\nurl = \"https://x\"\nauth = { bearer = { env = \"UNSET\" } }",
            "toolset.t.auth.bearer",
            "environment variable UNSET is not set",
        ),
        (
            "[toolset.t]\ncommand = \"/x\"\nenv = { A = { secret = \"nope\" } }",
            "toolset.t.env.A",
            "no secret",
        ),
        (
            "[provider.anthropic]\nconfig = { api_key = { env = 1 } }",
            "provider.anthropic.config.api_key",
            "non-empty string",
        ),
    ];
    for (toml, key, reason) in cases {
        let home = Home::new(&format!("{MINIMAL}{toml}\n"));
        home.secrets(SECRETS, 0o600);
        assert_error(&home, "giles.toml", key, reason);
    }
    let home = Home::new(&format!(
        "{MINIMAL}[provider.anthropic]\nconfig = {{ api_key = {{ secret = \"anthropic\" }} }}\n"
    ));
    let missing = format!("{} does not exist", home.path("secrets.toml").display());
    assert_error(
        &home,
        "giles.toml",
        "provider.anthropic.config.api_key",
        &missing,
    );
}

#[test]
fn auth_and_env_rules() {
    let remote = "[channel.x]\ncomponent = \"irc\"\naddress = \"https://x\"";
    let cases = [
        (
            "[channel.web]\nauth = { bearer = \"t\" }",
            "channel.web.auth",
            "built-in",
        ),
        (
            "[channel.x]\nauth = { bearer = \"t\" }",
            "channel.x.auth",
            "without `address`",
        ),
        (
            "[toolset.t]\ncommand = \"/x\"\nauth = { bearer = \"t\" }",
            "toolset.t.auth",
            "without `url`",
        ),
        (
            "[toolset.t]\nurl = \"https://x\"\nenv = { A = \"b\" }",
            "toolset.t.env",
            "remote toolset",
        ),
        (
            "[toolset.t]\nurl = \"https://x\"\nauth = { oauth = {} }",
            "toolset.t.auth.oauth",
            "only `bearer`",
        ),
        (
            "[toolset.t]\nurl = \"https://x\"\nauth = \"t\"",
            "toolset.t.auth",
            "must be { bearer = ... }",
        ),
        (
            &format!("{remote}\nauth = {{}}"),
            "channel.x.auth",
            "or { oauth",
        ),
        (
            &format!("{remote}\nauth = {{ bearer = \"a\", oauth = {{}} }}"),
            "channel.x.auth",
            "must be",
        ),
        (
            &format!("{remote}\nauth = {{ basic = \"a\" }}"),
            "channel.x.auth.basic",
            "unknown auth",
        ),
        (
            &format!("{remote}\nauth = {{ bearer = [] }}"),
            "channel.x.auth.bearer",
            "not array",
        ),
        (
            &format!(
                "{remote}\nauth = {{ oauth = {{ token_url = \"https://t\", client_id = \"c\" }} }}"
            ),
            "channel.x.auth.oauth",
            "missing field `client_secret`",
        ),
        (
            &format!(
                "{remote}\nauth = {{ oauth = {{ token_url = \"http://t.example.com\", \
                 client_id = \"c\", client_secret = \"s\" }} }}"
            ),
            "channel.x.auth.oauth.token_url",
            "insecure = true",
        ),
        (
            &format!(
                "{remote}\nauth = {{ oauth = {{ token_url = \"https://t\", client_id = 5, \
                 client_secret = \"s\" }} }}"
            ),
            "channel.x.auth.oauth.client_id",
            "must be a string",
        ),
        (
            &format!(
                "{remote}\nauth = {{ oauth = {{ token_url = \"https://t\", client_id = \"c\", \
                 client_secret = \"s\", scope = [] }} }}"
            ),
            "channel.x.auth.oauth.scope",
            "unknown field `scope`",
        ),
        (
            &format!(
                "{remote}\nauth = {{ oauth = {{ token_url = \"https://t\", client_id = \"c\", \
                 client_secret = \"s\", scopes = [1] }} }}"
            ),
            "channel.x.auth.oauth.scopes[0]",
            "must be a string",
        ),
        (
            "[toolset.t]\ncommand = \"/x\"\nenv = \"A=b\"",
            "toolset.t.env",
            "must be a table",
        ),
        (
            "[toolset.t]\ncommand = \"/x\"\nenv = { \"A=B\" = \"c\" }",
            "toolset.t.env.\"A=B\"",
            "variable name",
        ),
        (
            "[toolset.t]\ncommand = \"/x\"\nenv = { A = 1 }",
            "toolset.t.env.A",
            "not integer",
        ),
        (
            "[provider.anthropic]\nconfig = \"sk-123\"",
            "provider.anthropic.config",
            "must be a table",
        ),
    ];
    for (toml, key, reason) in cases {
        let home = Home::new(&format!("{MINIMAL}{toml}\n"));
        assert_error(&home, "giles.toml", key, reason);
    }
}

#[test]
fn errors_never_show_credential_values() {
    // Values at credential paths, even literal ones of the wrong type, stay out of errors.
    let cases = [
        "[channel.x]\ncomponent = \"irc\"\naddress = \"https://x\"\nauth = { bearer = 98765432 }",
        "[channel.x]\ncomponent = \"irc\"\naddress = \"https://x\"\nauth = \"98765432\"",
        "[toolset.t]\ncommand = \"/x\"\nenv = { TOKEN = 98765432 }",
        "[toolset.t]\ncommand = \"/x\"\nenv = \"98765432\"",
        "[provider.anthropic]\nconfig = \"98765432\"",
        "[provider.anthropic]\nconfig = 98765432",
    ];
    for toml in cases {
        let home = Home::new(&format!("{MINIMAL}{toml}\n"));
        let err = home.load().unwrap_err();
        let shown = format!("{err} {err:?}");
        assert!(!shown.contains("98765432"), "{shown}");
    }
    let home = Home::new(MINIMAL);
    home.secrets("token = 98765432\n", 0o600);
    let err = home.load().unwrap_err();
    let shown = format!("{err} {err:?}");
    assert_eq!(err.key.to_string(), "token");
    assert!(!shown.contains("98765432"), "{shown}");
}

#[test]
fn sandbox_grants() {
    let home = Home::new(MINIMAL);
    let own = home.path("data/plugins/files");
    home.write(
        "giles.toml",
        &format!(
            "{MINIMAL}[channel.irc]\n\
             [channel.libera]\ncomponent = \"irc\"\n\
             sandbox = {{ network = [\"tcp:6697\"], read = [\"/opt/homebrew/\"], min_abi = 4 }}\n\
             [toolset.files]\ncommand = \"/bin/files\"\n\
             sandbox = {{ read = [\"/Users/me/Documents\"], write = [{own:?}, \"{own}/./tmp\"], \
             unix = [\"/var/run/docker.sock\"] }}\n\
             [toolset.fetch]\ncommand = \"/bin/fetch\"\nsandbox = {{ network = true }}\n\
             [toolset.shell]\ncommand = \"/bin/sh\"\nunsandboxed = true\n",
            own = own.display()
        ),
    );
    let config = home.load_ok();
    let launch = |name: &str| match &config.channels[name].runtime {
        ChannelRuntime::Plugin(launch) => launch.sandbox.clone(),
        other => panic!("{other:?}"),
    };
    let stdio = |name: &str| match &config.toolsets[name].transport {
        ToolsetTransport::Stdio(stdio) => stdio.sandbox.clone(),
        other => panic!("{other:?}"),
    };

    // Channels get the network by default, stdio toolsets don't.
    assert_eq!(
        launch("irc").grants,
        Grants {
            network: NetworkGrant::All,
            ..Grants::default()
        }
    );
    let libera = launch("libera").grants;
    assert_eq!(libera.network, NetworkGrant::TcpPorts(vec![6697]));
    assert_eq!(libera.read, [PathBuf::from("/opt/homebrew")]);
    assert_eq!(libera.min_abi, Some(4));

    let files = stdio("files");
    assert!(!files.unsandboxed);
    assert_eq!(files.grants.network, NetworkGrant::None);
    assert_eq!(files.grants.read, [PathBuf::from("/Users/me/Documents")]);
    assert_eq!(files.grants.write, [own.clone(), own.join("tmp")]);
    assert_eq!(files.grants.unix, [PathBuf::from("/var/run/docker.sock")]);
    assert_eq!(files.grants.min_abi, None);
    assert_eq!(stdio("fetch").grants.network, NetworkGrant::All);
    let shell = stdio("shell");
    assert!(shell.unsandboxed);
    assert_eq!(shell.grants.network, NetworkGrant::None);
}

#[test]
fn sandbox_rules() {
    let shapes = [
        ("[channel.web]", "channel.web", "built-in"),
        (
            "[channel.x]\ncomponent = \"irc\"\naddress = \"https://x\"",
            "channel.x",
            "remote channel",
        ),
        (
            "[toolset.t]\nurl = \"https://x\"",
            "toolset.t",
            "remote toolset",
        ),
        (
            "[toolset.t]\ncommand = \"/x\"\nunsandboxed = true",
            "toolset.t",
            "no effect",
        ),
    ];
    for (toml, key, reason) in shapes {
        let home = Home::new(&format!("{MINIMAL}{toml}\nsandbox = {{}}\n"));
        assert_error(&home, "giles.toml", &format!("{key}.sandbox"), reason);
    }

    let grants = [
        ("read = [\"relative\"]", "read[0]", "not an absolute path"),
        ("write = [\"/a/../b\"]", "write[0]", "`..`"),
        ("network = \"on\"", "network", "true, false, or a list"),
        ("network = [\"udp:53\"]", "network", "tcp:<port>"),
        ("min_abi = 0", "min_abi", "at least 1"),
        ("exec = []", "exec", "unknown field `exec`"),
    ];
    for (toml, key, reason) in grants {
        let home = Home::new(&format!(
            "{MINIMAL}[toolset.t]\ncommand = \"/x\"\nsandbox = {{ {toml} }}\n"
        ));
        assert_error(
            &home,
            "giles.toml",
            &format!("toolset.t.sandbox.{key}"),
            reason,
        );
    }
}

#[test]
fn grants_must_not_overlap_the_data_root() {
    let home = Home::new(MINIMAL);
    let root = home.root.root().to_path_buf();
    let parent = root.parent().unwrap().to_path_buf();
    let overlapping = [
        root.clone(),
        root.join("giles.toml"),
        root.join("secrets.toml"),
        root.join("plugins"),
        root.join("agents/jeeves"),
        root.join("data"),
        root.join("data/plugins"),
        root.join("data/plugins/other"),
        root.join("data/plugins/t-other"),
        root.join("run/admin.sock"),
        parent.clone(),
        PathBuf::from("/"),
    ];
    let grant = |key: &str, paths: &[&Path]| {
        let paths: Vec<String> = paths.iter().map(|p| format!("{p:?}")).collect();
        home.write(
            "giles.toml",
            &format!(
                "{MINIMAL}[toolset.t]\ncommand = \"/x\"\nsandbox = {{ {key} = [{}] }}\n",
                paths.join(", ")
            ),
        );
    };
    for key in ["read", "write", "unix"] {
        for path in &overlapping {
            grant(key, &[path]);
            assert_error(
                &home,
                "giles.toml",
                &format!("toolset.t.sandbox.{key}[0]"),
                "overlaps the data root",
            );
        }
    }
    // The instance's own data directory, and anything in it, may be granted.
    let own = root.join("data/plugins/t");
    grant("write", &[&own, &own.join("cache")]);
    home.load_ok();
    // A sibling of the root that shares a name prefix is not inside it.
    grant("read", &[Path::new(&format!("{}-backup", root.display()))]);
    home.load_ok();
}

#[test]
fn grants_through_a_symlink_into_the_data_root_are_rejected() {
    let home = Home::new(MINIMAL);
    let link = home.dir.path().with_extension("link");
    std::os::unix::fs::symlink(home.root.root(), &link).unwrap();
    let grant = link.join("secrets.toml");
    home.write(
        "giles.toml",
        &format!("{MINIMAL}[toolset.t]\ncommand = \"/x\"\nsandbox = {{ read = [{grant:?}] }}\n"),
    );
    let result = home.load();
    std::fs::remove_file(&link).unwrap();
    let err = result.unwrap_err();
    assert_eq!(err.key.to_string(), "toolset.t.sandbox.read[0]");
    assert!(err.reason.contains("overlaps the data root"), "{err}");
}

#[test]
fn check_grant_applies_the_same_rule() {
    let home = Home::new(MINIMAL);
    let name = InstanceName::new("files").unwrap();
    assert!(check_grant(&home.root, &name, Path::new("/opt/homebrew")).is_ok());
    assert!(check_grant(&home.root, &name, &home.path("data/plugins/files/x")).is_ok());
    let err = check_grant(&home.root, &name, &home.path("plugins")).unwrap_err();
    assert!(err.contains("overlaps the data root"), "{err}");
}

#[test]
fn grants_must_not_write_where_binaries_live() {
    use std::os::unix::fs::PermissionsExt;

    let tools = tempfile::tempdir().unwrap();
    let tools = tools.path();
    let executable = |relative: &str| {
        let path = tools.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    };
    let giles = executable("giles-bin/giles");
    executable("path/giles-channel-irc");
    executable("path/uvx");
    let server = executable("venv/bin/server");
    let real_shell = executable("real/giles-toolset-shell");
    std::fs::create_dir_all(tools.join("other")).unwrap();
    std::os::unix::fs::symlink(&real_shell, tools.join("path/giles-toolset-shell")).unwrap();
    let path_var = tools.join("path").into_os_string();
    let env = move |name: &str| (name == "PATH").then(|| path_var.clone());

    let home = Home::new(MINIMAL);
    let load = |instance: &str, sandbox: &str, giles_exe: Option<PathBuf>| {
        let mut toml = format!(
            "{MINIMAL}[channel.irc]\n[toolset.venv]\ncommand = {server:?}\n\
             [toolset.uv]\ncommand = \"uvx\"\n[toolset.shell]\n"
        );
        toml = toml.replace(
            &format!("[{instance}]\n"),
            &format!("[{instance}]\nsandbox = {{ {sandbox} }}\n"),
        );
        home.write("giles.toml", &toml);
        let options = LoadOptions {
            env: &env,
            giles_exe,
            ..LoadOptions::default()
        };
        Config::load_with(&home.root, &options)
    };
    let dir = |relative: &str| format!("{:?}", tools.join(relative));

    let rejected = [
        ("channel.irc", "write", "giles-bin", "the giles binary"),
        (
            "channel.irc",
            "write",
            "giles-bin/giles",
            "the giles binary",
        ),
        ("channel.irc", "write", "path", "channel irc's plugin"),
        (
            "toolset.venv",
            "unix",
            "path/plugin.sock",
            "channel irc's plugin",
        ),
        ("toolset.shell", "write", "venv", "toolset venv's server"),
        (
            "toolset.venv",
            "write",
            "venv/bin/cache",
            "toolset venv's server",
        ),
        ("toolset.venv", "write", "real", "toolset shell's server"),
    ];
    for (instance, key, grant, what) in rejected {
        let err = load(
            instance,
            &format!("{key} = [{}]", dir(grant)),
            Some(giles.clone()),
        )
        .unwrap_err();
        assert_eq!(
            err.key.to_string(),
            format!("{instance}.sandbox.{key}[0]"),
            "{err}"
        );
        assert!(err.reason.contains(what), "{grant}: {err}");
        assert!(err.reason.contains("may read there"), "{err}");
        assert!(err.position.is_some(), "{err}");
    }
    // A directory holding only `uvx`, found on PATH, is protected too.
    std::fs::remove_file(tools.join("path/giles-channel-irc")).unwrap();
    std::fs::remove_file(tools.join("path/giles-toolset-shell")).unwrap();
    let err = load("toolset.venv", &format!("write = [{}]", dir("path")), None).unwrap_err();
    assert!(err.reason.contains("toolset uv's server"), "{err}");

    // Reading is fine, and so is writing elsewhere.
    for (instance, sandbox) in [
        (
            "channel.irc",
            format!("read = [{}, {}]", dir("path"), dir("giles-bin")),
        ),
        ("toolset.venv", format!("write = [{}]", dir("other"))),
    ] {
        load(instance, &sandbox, Some(giles.clone())).unwrap_or_else(|e| panic!("{e}"));
    }
    // Without a known giles binary, its directory isn't checked.
    load(
        "channel.irc",
        &format!("write = [{}]", dir("giles-bin")),
        None,
    )
    .unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn toolset_skill_directories_must_not_be_writable_by_plugins() {
    let outside = tempfile::tempdir().unwrap();
    let skills = outside.path().join("skills");
    let home = Home::new(MINIMAL);
    let write = |toml: &str| {
        home.write("giles.toml", &format!("{MINIMAL}{toml}"));
    };

    // A write grant on the skills, or on a directory holding them.
    for grant in [
        skills.clone(),
        outside.path().to_owned(),
        skills.join("demo"),
    ] {
        write(&format!(
            "[toolset.docs]\nurl = \"https://example.com/mcp\"\nskills = [{skills:?}]\n\
             [channel.irc]\nsandbox = {{ write = [{grant:?}] }}\n"
        ));
        assert_error(
            &home,
            "giles.toml",
            "channel.irc.sandbox.write[0]",
            "toolset docs's skills",
        );
    }

    // Skills inside the plugins' data directories.
    let data = home.root.plugin_data_dir("irc").unwrap().join("skills");
    write(&format!(
        "[toolset.docs]\nurl = \"https://example.com/mcp\"\nskills = [{data:?}]\n"
    ));
    assert_error(
        &home,
        "giles.toml",
        "toolset.docs.skills[0]",
        "overlaps the plugins' data directories",
    );

    // Read grants are fine.
    write(&format!(
        "[toolset.docs]\nurl = \"https://example.com/mcp\"\nskills = [{skills:?}]\n\
         [channel.irc]\nsandbox = {{ read = [{skills:?}] }}\n"
    ));
    home.load_ok();
}

#[test]
fn config_hashes_follow_the_instance_table_as_written() {
    let hashes = |giles_toml: &str, secrets: &str| {
        let home = Home::new(&format!("{MINIMAL}{giles_toml}"));
        home.secrets(secrets, 0o600);
        let config = home.load_ok();
        (
            config.providers["anthropic"].config_hash,
            config.channels["libera"].config_hash,
            config.toolsets["files"].config_hash,
        )
    };
    let base = hashes(
        r##"
[provider.anthropic]
config = { api_key = { secret = "anthropic" } }

[channel.libera]
component = "irc"
sandbox = { network = ["tcp:6697"] }
config = { server = "irc.libera.chat:6697", channels = ["#giles"] }

[toolset.files]
command = "/bin/files"
args = ["/docs"]
env = { TOKEN = { secret = "files" } }
"##,
        "anthropic = \"one\"\nfiles = \"two\"\n",
    );

    // Reordered, reformatted, other instances added, and different secret values: same hashes.
    let same = hashes(
        r##"
[toolset.files]   # comment
env.TOKEN.secret = "files"
args = [ "/docs", ]
command = '/bin/files'

[toolset.other]
command = "/bin/other"

[channel.libera.config]
channels = ["#giles"]
server = "irc.libera.chat:6697"

[channel.libera]
component = "irc"
sandbox.network = ["tcp:6697"]

[provider.anthropic.config.api_key]
secret = "anthropic"
"##,
        "anthropic = \"changed\"\nfiles = \"changed too\"\n",
    );
    assert_eq!(same, base);

    // A changed value changes that instance's hash and no other.
    let changed = hashes(
        r##"
[provider.anthropic]
config = { api_key = { secret = "anthropic" } }

[channel.libera]
component = "irc"
sandbox = { network = ["tcp:6697"] }
config = { server = "irc.libera.chat:6697", channels = ["#giles", "#rust"] }

[toolset.files]
command = "/bin/files"
args = ["/docs"]
env = { TOKEN = { secret = "files" } }
"##,
        "anthropic = \"one\"\nfiles = \"two\"\n",
    );
    assert_eq!(changed.0, base.0);
    assert_ne!(changed.1, base.1);
    assert_eq!(changed.2, base.2);
}

#[test]
fn an_empty_instance_table_has_the_empty_table_hash() {
    let config = Home::new(&format!("{MINIMAL}[channel.web]\n")).load_ok();
    assert_eq!(
        config.channels["web"].config_hash,
        ConfigHash::of(&toml::Table::new())
    );
}

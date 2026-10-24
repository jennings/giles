# Registry and Startup

`giles_core::registry` maps configured instance names to the trait objects that serve them
(PLAN sections 4.1, 7.2, and 13.3), and holds the prefixed tool index, the per-agent skill index,
and model resolution. `giles_core::runtime` builds it at startup together with the stores.

## Startup

`Runtime::start(root, Arc<Config>)` runs after the configuration is loaded and the sandbox host
check has run ([The `giles` Binary](daemon.md#giles-serve-lifecycle)):

1. Open the database, `data/giles.db` (`giles_store::Store::open`, which migrates it).
2. Open the blob store, `data/blobs` (`BlobStore::open`).
3. `Registry::from_config(root, config, media)` with `media` a `giles_core::blobs::BlobMedia` over
   the blob store.

`serve` then registers the toolsets (the [toolset host](toolsets.md#lifecycle)) and calls
`Registry::log_summary()`: one `instance not started` line per pending instance, then
`registry ready` with the counts of providers, channels, toolsets, tools, and pending instances.

`Runtime` is `Clone` and holds `root`, `store`, `blobs`, `registry: Arc<Registry>`, and `outbox`
(built here, its sender started by `start_outbox`; [Outbox](outbox.md)). Errors are a
`StartError` (`Store`, `Blobs`, `Registry`, and later `Sessions`, `Outbox`); `giles serve` prints
it and exits.

## The Running Configuration

The registry holds the configuration as a `config::LiveConfig`: the `Config` loaded at startup, plus
agents created since (the web UI's agent pages, TASKS.md 5.1). `Registry::config()` returns a
snapshot, `Arc<Config>`; a caller keeps one snapshot for the work at hand (a turn, a tool call, a
page). `Registry::live_config()` is the shared handle: the core tools and the session manager (its
`AgentPolicies`) hold clones, so they see a new agent at their next lookup.
`LiveConfig::add_agent(AgentConfig)` swaps in a copy of the configuration with the agent added and
refuses an id already taken, ignoring ASCII case (`AgentExists`). Nothing else changes while the
core runs: instances and the owner are fixed until a restart (reload is deferred, D28). Routes
are stored and change at runtime ([Identity and Routing](identity-and-routing.md#routing)).

## Built-in Components

`from_config` constructs every built-in instance. Providers are always built in (provider plugins
are deferred, PLAN section 4.1); a channel is built in when its runtime is
`ChannelRuntime::BuiltIn`. Each builder is a `match` on the component name with one arm per
built-in, so adding one is one arm:

- provider `anthropic`: `AnthropicConfig::from_json(config.to_json())`, then
  `AnthropicProvider::new` (feature `provider-anthropic`; without it, an error naming the feature).
- provider `openai`: `OpenAiConfig::from_json(config.to_json())`, then `OpenAiProvider::new`
  (feature `provider-openai`; without it, an error naming the feature).
- channel `web`: `WebConfig::from_json(config.to_json())`, then `WebChannel` (feature
  `channel-web`; without it, an error naming the feature). At most one instance may use it: a
  second is an error at its `component` key. The channel is also kept typed,
  `Registry::web_channel() -> Option<Arc<WebChannel>>`, for its pages and approval prompts (see
  [Web Channel](web-channel.md)).

Every failure is a `ConfigError` on `giles.toml` at `<kind>.<name>.component` (missing component)
or `<kind>.<name>.config` (rejected config). On that error path only, `giles.toml` is read again
to find the key's line and column, so it prints as `file:line:col: key: reason` like any other
configuration error. Messages from the component pass through `PluginConfig::scrub`, so they never
show a secret.

Config loading already rejects a model whose provider instance isn't configured, and
`from_config` either registers every configured provider or fails, so every agent model resolves
afterwards.

## Registering Components Later

Components built outside the registry are added with:

- `register_channel(&InstanceName, Arc<dyn Channel>)`: plugin channels, launched or remote (the
  plugin host's `RemoteChannel`, see [Plugin Host](plugin-host.md#remotechannel)), or a built-in
  constructed elsewhere.
- `register_toolset(&InstanceName, Arc<dyn Toolset>).await`: every MCP toolset, once at startup,
  by the toolset host ([Toolsets](toolsets.md#lifecycle)).
- `register_provider(&InstanceName, Arc<dyn Provider>)`: future provider plugins and fakes in
  tests.

Each requires `[<kind>.<name>]` to be configured (else `RegistryError::NotConfigured`) and replaces
any object registered earlier under that name, returning it, so a restarted plugin's new adapter can
take over. The maps sit behind `RwLock`s and lookups (`provider`, `channel`, `toolset`, and the
`providers()`/`channels()`/`toolsets()` lists) clone an `Arc`, so nothing holds a lock across an
`await`. Consuming a channel's events is the caller's job.

`pending()` lists configured channels and toolsets with nothing registered, with what will start
them (the plugin host once the plugin is up, or a task still to land); `log_summary()` logs each as
`instance not started`. Toolsets are never pending once `serve` has started the toolset host, which
registers them all before the summary.

`Registry::new(root, config)` makes an empty registry for tests, which register fakes.

## Tool Index

`registry.tools()` is the `ToolIndex` (PLAN section 6.5):

- Toolsets report bare names; the index offers `<instance>__<tool>`. A tool is skipped, with a
  warning, if its bare name is empty or has a character outside `[A-Za-z0-9_-]`, if the prefixed
  name is over 64 characters (`MAX_TOOL_NAME_CHARS`, both v1 provider APIs' limit), or if the
  toolset lists the name twice (the first is kept). Instance names can't contain `_`, so a prefixed
  name never collides with another instance's or with a core tool.
- Core tools use the bare, reserved `CORE_TOOLS`: `load_skill`, `memory`, `memory_search`,
  `schedule`, `send_attachment`. `Registry::register_core_tool(Arc<dyn CoreTool>)` keeps the
  implementation (`Registry::core_tool(name)`) and adds its `ToolSpec` with
  `ToolIndex::set_core_tool`, which rejects any other name. A spec added with `set_core_tool`
  alone (tests) is offered, but calls to it get `tool unavailable`.
- `snapshot()` returns an `Arc<ToolSnapshot>`: every tool sorted by name (so the prompt prefix is
  stable for caching), `get(name)` by binary search giving an `IndexedTool { spec, target }` where
  `target` is `ToolTarget::Core` or `ToolTarget::Toolset { instance, tool }` (the bare name to
  call). A session keeps the snapshot it started with; changes reach new sessions only.

`register_toolset` subscribes to `Toolset::tool_changes()` first, then awaits `list_tools()`, and
only then, with no further `await`, installs the object in the map, starts a new generation, indexes
the list, and spawns a task applying each later list from the stream. It is therefore cancel-safe:
dropping the future during `list_tools` (a timeout around a hung server) changes nothing, and the
previous object and its watcher stay in place. A failed `list_tools` is logged and leaves the
instance's previous tools in place (none at first) until a change arrives. Each new generation
aborts the previous object's watcher, and any update tagged with an old generation is dropped, so a
replaced object can't overwrite its successor's list. Watchers hold only a weak reference to the
index and stop when it is dropped.

`set_toolset_tools(instance, tools)` replaces an instance's list directly (tests use it to offer
tools without a toolset object), with the same rules; `remove_toolset` removes it. The toolset
host feeds the index through `register_toolset` instead: each instance's `ManagedToolset` reports
the cached and fresh lists through `tool_changes` ([Toolsets](toolsets.md#lifecycle)).

## Skill Index

`skill_sources(agent)` lists an agent's skill directories, lowest precedence first: each toolset's
`skills` directories (by instance name, in listed order, as
`SkillSource { prefix: Some(instance) }`, so their skills are `<instance>__<skill>`), then
`<root>/skills/`, then `agents/<agent>/skills/` (PLAN section 6.6). `agent_skills(agent).await` runs
`skills::discover` over them on a blocking thread, logs each skipped skill, and returns the
`SkillSet`. A toolset also serves its own directories, bare-named, through `Toolset::list_skills`
and `get_skill` ([Toolsets](toolsets.md#lifecycle)); the index reads the configuration directly,
so a stopped server's skills are listed too.

## Models

`resolve_model(&ModelRef)` gives a `ResolvedModel { instance, provider: Arc<dyn Provider>, model }`,
or `RegistryError::UnknownProvider`. `agent_model(&AgentConfig)` is the first of `models` (v1 has
no fallback, PLAN section 1.1); `compaction_model(&AgentConfig)` is `compaction_model`, else the
agent model.

## Blob Media

`giles_core::blobs::BlobMedia` implements `providers::MediaSource` over the blob store. A read
opens the blob (`NotFound` if it isn't stored), reads at most one byte more than its recorded size
(enough to reach end of file, where the reader checks the SHA-256, without reading an unbounded
amount), and fails unless it got exactly the recorded size.

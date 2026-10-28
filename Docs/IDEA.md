# Giles - Idea

## Purpose

OpenClaw is useful but uses too many resources for what it does. It uses over 2 GB of memory even
when it's only making API calls and handling web requests, without running local tools. The goal is
a Rust port with a small, efficient core that ships only the features needed now and leaves room to
add the rest later.

This doc lists the features and explains roughly how each one works. The detailed design is
`Docs/PLAN.md`; where the two differ, `PLAN.md` wins. Decisions still open are tracked in
`Docs/DECISIONS.md`.

## Design principles

- Small core. The core handles orchestration: identity, routing, sessions, the agent loop,
  scheduling, policy, storage, and supervising plugins. Work that needs isolation, a long-lived
  connection, or a heavy protocol library runs in a separate plugin process. Plain HTTP clients (the
  v1 providers), the web channel, and the core tools are built in.
- One trait per component kind. The core talks to a provider, channel, or toolset only through its
  trait, so it can't tell a built-in from a plugin. New features fit in later as plugins without
  changes to the core.
- One source of truth. A plugin describes itself at runtime. There is no manifest to keep in sync by
  hand.
- Don't trust plugins by default. The core sandboxes every plugin it launches.
- Settings belong in the web UI, not in config files. `giles.toml` holds only what Giles needs to
  start and what makes Giles run a program on the host: the address the web interface listens on,
  the default time zone for users who don't set one, security-model requirements such as whether a
  plugin may be configured to run unsandboxed, and the programs Giles launches (stdio toolsets and
  plugins) with their arguments, environment, and sandbox grants. Installing and running code on the
  host is the operator's act, so a web login can't choose any of it. The operator writes the file
  and Giles never edits it. Giles manages everything else so it can be changed in the web UI: users
  (an admin adds them), agents and their files (`agent.toml`, `SOUL.md`, per-user memories), routes
  from channel identities to users and agents, remote MCP servers (toolsets Giles connects to, not
  runs), and, once designed, the provider and channel instances that launch no program. Whether each
  kind of data lives in SQLite or in Markdown, JSONL, or other text files is decided as it's built.
  Routes and remote toolsets are already managed in the web UI; v1 still reads the owner and the
  provider and channel instances from `giles.toml`, and those that launch no program move out as
  their web UI lands (`PLAN.md` section 11.2).

## v1 scope

v1 is used by one person, the owner, who talks to agents on several channels.

- One implicit user, the owner, whose identities are declared in `giles.toml`. Messages from any
  other identity are dropped and logged. No user records, roles, or pairing.
- Group conversations are ignored.
- Each agent uses one model, with no fallback.
- Config is read at startup; changes need a restart. Agent Markdown files are read on every turn.

The multi-user, group-chat, and fallback designs in `PLAN.md` are kept for later.

## Feature: channels

Channels are how users talk to an agent. OpenClaw calls these "channels".

v1 channels:

| Channel  | Packaging                | Receive                   | Send |
| -------- | ------------------------ | ------------------------- | ---- |
| Web      | Built-in                 | HTTP and server-sent events | Same |
| IRC      | `giles-channel-irc`      | IRC connection            | IRC  |
| Email    | `giles-channel-email`    | IMAP with IDLE            | SMTP |
| WhatsApp | `giles-channel-whatsapp` | WhatsApp Web multi-device | Same |

- Other services (Telegram and so on) are added later as channel plugins.
- The agent writes Markdown. Each channel renders it into its own format.
- Each channel uses an account dedicated to the agent: its own WhatsApp number, mailbox, and IRC
  nick.
- WhatsApp has no official route for a general-purpose chatbot, so the plugin uses the unofficial
  linked-device protocol (`whatsapp-rust`, with `whatsmeow` in Go as the fallback).
- Delivery from a channel to the core is at least once. The core stores each event before
  acknowledging it, so a crash or restart loses no message.

Identity and access control:

- A channel reports each sender's native identifier (IRC services account, email address, WhatsApp
  phone number) only after verifying it: DMARC or aligned DKIM for email, IRCv3 account tags for
  IRC, the protocol itself for WhatsApp. Unverified senders are dropped.
- The core maps identities to users and enforces policy, so policy logic isn't rewritten in every
  channel.
- In v1 the owner's identities are listed in config. The multi-user design (users, roles, pairing
  codes, self-linking) is in `PLAN.md` section 3.

## Feature: providers

Providers are how the system calls an LLM.

- Two built-in clients: one for the OpenAI-compatible API, which covers OpenAI, OpenRouter, Ollama,
  and most others, and a native Anthropic client.
- Local models run out of process, for example through Ollama, never inside the core.
- Giles has its own message model, patterned on Anthropic's content blocks. Each provider translates
  to and from its wire format.
- Provider plugins are deferred. The `Provider` trait stays, so they can be added later.

## Feature: agents

An agent is the personality the user talks to. Agents are managed by Giles itself, not through
plugins, and are meant to be edited in the web UI.

Each agent has:

- A name
- The models it can use (v1 uses the first one)
- The tools it may call, and the tools that need the owner's approval before each call
- Its own files: `SOUL.md` (who it is) and the user's `USER.md` (who its user is)
- Its memories, in a database of its own: short facts it adds, edits, forgets, and recalls through
  tools. Each is private to one user or group, or shared by all its conversations, and the agent
  marks the important ones as core. Only core memories are loaded into every prompt, under a size
  cap, so the rest costs nothing until the agent searches for it.
- Scheduled jobs

Routing rules decide which agent handles which channel and conversation. Like agents, they're
managed by Giles and meant to be edited in the web UI.

## Feature: sessions (v1)

- Every conversation is its own session with its own history: each WhatsApp chat, email thread, IRC
  query, and web chat.
- Knowledge crosses channels through memory, not history. Core memories are loaded into every
  session that may see them, the agent can recall its other memories by search, and it can search
  past transcripts on all channels.
- At most one turn runs per session. New messages during a turn are either shown to the model
  between tool calls or queued for the next turn.
- `/new` starts a fresh session, and `/stop` cancels the running turn.
- When a session nears the model's context limit, older history is summarized.

## Feature: scheduling (v1)

There is one scheduler and one kind of job.

- Cron-style jobs, defined in an agent's config or created at runtime by the agent ("remind me at
  5").
- The heartbeat, a job that wakes the agent without a user message, is just another job.
- A job's reply goes to the owner. A reply of exactly `NO_REPLY` sends nothing.
- Missed runs while the core was down are coalesced into one run at startup.

## Feature: tools and skills

Tools come from toolsets. A toolset is an MCP server; one server exposes many tools.

- Core tools are built in because they operate on core state: memory (add, edit, forget, recall),
  transcript search, loading skills, scheduling, and sending attachments.
- Every other tool comes from an MCP server: a local stdio command, launched and sandboxed by the
  core, or a remote HTTP server. Any MCP server works as a toolset unchanged.
- Tool names are prefixed with the toolset instance name, e.g. `github__create_issue`.
- The core starts every configured toolset at boot. A toolset may stop when idle and restart on its
  next call; its cached tool list keeps its tools available to the model meanwhile.

Tool call flow:

1. Toolsets expose tools to the core.
2. The core gives the model the tools the agent allows.
3. The model asks to call a tool.
4. The core checks policy, asks the owner for approval if the tool needs it, and starts the toolset
   if it's stopped.
5. The toolset does the work and returns a result.
6. The core passes the result to the model for its next turn.

Skills use the Agent Skills format: a directory with a `SKILL.md`. They come from the global and
per-agent skill directories and from directories a toolset lists. Each skill's name and description
go into the prompt, and the model loads the full skill on demand.

## Feature: MCP support

- The core is an MCP client, built on the official Rust SDK (`rmcp`).
- Transports: stdio and streamable HTTP. The deprecated HTTP+SSE transport is not supported; current
  `rmcp` releases don't provide it either.
- MCP elicitation becomes a mid-call question or approval, asked through the channel.
- A first-party toolset is an MCP server written with `rmcp`.

## Feature: plugins

Plugins come in three kinds with little overlap. One plugin never mixes kinds.

| Kind     | What it is                       | Protocol |
| -------- | -------------------------------- | -------- |
| Provider | `giles-provider-<name>` binary    | gRPC     |
| Channel  | `giles-channel-<name>` binary     | gRPC     |
| Toolset  | Any MCP server                   | MCP      |

gRPC plugins:

- Each implements a small shared base service (describe, configure, shutdown, blob transfer, health)
  plus exactly one kind service.
- The core launches each configured instance as its own process and hands it one end of a private
  socket pair, so no other process can reach it. The plugin prints a one-line handshake, similar to
  HashiCorp go-plugin, and serves gRPC on that socket.
- The core controls each launched plugin's lifetime: it restarts crashed plugins with backoff and
  stops them on shutdown. A plugin exits on its own only when the core has gone away.
- Remote plugins run elsewhere, managed by the user. The core connects over TCP, with TLS
  recommended over a network, and authenticates with a bearer token or OAuth client credentials.
- Proto packages are versioned, and changes within a major version are additive only.
- A plugin SDK (`giles-plugin-sdk`) handles the plugin side for plugins written in Rust.

## Feature: sandboxing

There are two layers:

- Core level: The core sandboxes every plugin and stdio MCP server it launches. The sandbox is
  mandatory: if it can't be applied, the plugin doesn't start, unless `giles.toml` allows opting out
  and the instance opts out. Plugins get a scrubbed environment and a single writable data
  directory. Provider and channel plugins have network access by default; stdio MCP servers get it
  only when granted.
  - Linux: Landlock and seccomp, with no namespaces.
  - macOS: a generated deny-by-default Seatbelt profile.
- Plugin level: Plugins that run risky things, like a shell or curl tool, can add their own
  isolation (for example Docker) for the commands they run.

The core itself never runs shell commands, curl, or similar tools directly. Built-in components run
in the core unsandboxed.

## Platforms

Linux and macOS in v1. Transport and sandbox sit behind internal traits so Windows can be added
later without changing plugin contracts. A container image ships the core and the first-party
plugins.

## Out of scope for v1

- A ClawHub-style registry for plugins and skills. It's wanted eventually, and the design assumes it
  will exist later: it would install into `<root>/plugins/`.
- Channels beyond web, WhatsApp, email, and IRC (they come later as plugins).
- Provider plugins.
- More than one user, and group chats.
- Model fallback.
- Config reload, and managing users and instances in the web UI. (Creating agents and managing
  routes in the web UI were added later.)
- MCP servers that only speak the deprecated HTTP+SSE transport.

## Decisions

The questions this doc used to leave open are settled in `PLAN.md`:

| Question                  | Answer                                                                                  | `PLAN.md` |
| ------------------------- | --------------------------------------------------------------------------------------- | --------- |
| Remote plugin connections | TLS optional but recommended; bearer token or OAuth client credentials; no OAuth authorization code grant in v1; an unreachable plugin leaves Giles running degraded, retrying with backoff | 7.6 |
| Plugin interface shape    | A shared base service plus one service per kind                                         | 8.2       |
| Tool discovery            | Query at every start; cache keyed on the binary's hash, the reported version, and the instance config | 7.9 |
| Identity mapping          | v1 declares the owner's identities in `giles.toml`; the multi-user design is deferred  | 1.1, 3    |
| Config ownership          | `giles.toml` holds startup settings and what runs a program on the host (stdio toolsets, plugins, sandbox grants); Giles never edits it and manages the rest, remote MCP servers included, in the web UI | 11.2 |
| Core sandboxing           | Landlock and seccomp on Linux, Seatbelt on macOS, mandatory by default                  | 12        |
| Windows support           | Not in v1                                                                               | 12        |
| Email receiving           | IMAP with IDLE                                                                          | 10        |
| Skill format              | Agent Skills (`SKILL.md`), loaded on demand with `load_skill`                           | 6.6       |
| Plugin protocol for tools | MCP, not a custom gRPC protocol; skills come from directories the toolset lists         | 7.10      |

Deferred decisions, each with what v1 does meanwhile, are listed in `Docs/DECISIONS.md`.

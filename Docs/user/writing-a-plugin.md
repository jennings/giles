# Writing a Plugin

Channels other than the built-in web chat are plugins: separate programs that Giles starts, one
process per configured instance. A channel plugin is a binary named `giles-channel-<name>`, found
in `<GILES_HOME>/plugins/` or on `PATH`. This page shows how to write one in Rust with the
`giles-plugin-sdk` crate. (Model providers are built in for now; toolsets are ordinary MCP servers
and need no SDK.)

## What Giles does for you

Giles launches the plugin inside a sandbox whose only writable directory is the instance's data
directory, `<GILES_HOME>/data/plugins/<instance>/`. It talks to the plugin over a private
connection that no other process can reach, asks it what it is, and sends it the instance's
`config` table with secrets already filled in. A plugin never reads `giles.toml` or `secrets.toml`
and never listens on a socket.

The SDK does the plugin's side of all that. You write a type that implements the `Channel` trait
from `giles-types`: say what the channel can do, produce inbound events, take acks, and send
messages.

## A minimal channel

`Cargo.toml`:

```toml
[package]
name = "giles-channel-example"
version = "0.1.0"
edition = "2024"

[dependencies]
futures = "0.3"
giles-plugin-sdk = "0.1"
serde = { version = "1", features = ["derive"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

`src/main.rs`:

```rust
use std::process::ExitCode;

use futures::stream::{self, BoxStream, StreamExt as _};
use giles_plugin_sdk::giles_types::{
    Channel, ChannelEvent, ChannelInfo, EventId, MessageId, OutboundMessage, OutputMode, Result,
};
use giles_plugin_sdk::{ChannelPlugin, Configure, async_trait};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    server: String,
}

struct Example {
    info: ChannelInfo,
}

#[async_trait]
impl Channel for Example {
    fn info(&self) -> &ChannelInfo {
        &self.info
    }

    async fn events(&self) -> Result<BoxStream<'static, Result<ChannelEvent>>> {
        Ok(stream::pending().boxed()) // connect and yield inbound messages here
    }

    async fn ack(&self, _event: &EventId) -> Result<()> {
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<MessageId> {
        // render msg.markdown for the service and deliver it
        Ok(MessageId::new("1")?)
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    ChannelPlugin::new("example", env!("CARGO_PKG_VERSION"))
        .output(OutputMode::Blocks)
        .serve(|ctx: Configure| async move {
            let config: Config = ctx.parse_config()?;
            Ok(Example {
                info: ChannelInfo {
                    identity_namespace: format!("example:{}", config.server),
                    output: OutputMode::Blocks,
                    can_initiate: false,
                },
            })
        })
        .await
}
```

Install the binary where Giles finds it and configure an instance:

```toml
[channel.home]
component = "example"
config = { server = "chat.example.com" }
```

Run by hand, the binary only says that it's a Giles plugin and exits.

## The pieces

- `ChannelPlugin::new(name, version)`: `name` is the `<name>` in `giles-channel-<name>`. Set
  `output` (`Blocks` for chat services without message edits, `Deltas` for live streaming) and
  `can_initiate` (whether the channel can message an identity that hasn't written first, needed
  for scheduled jobs and login codes). Optionally `requested_grants`: sandbox grants to suggest to
  the operator, shown in `giles status` and never applied by themselves.
- `serve(configure)`: call it first thing in `main` and return its exit code. `configure` runs
  when Giles sends the instance config. Return `Error::InvalidRequest` for a bad config; Giles
  shows the message and doesn't start the instance. `ctx.parse_config()` does this for you, naming
  the offending key.
- The identity namespace your channel reports must be the plugin's name itself or the name, a
  colon, and more: `irc` or `irc:libera.chat`, but not `ircx`. Each `:`-separated part must be
  non-empty, without spaces. Instances in the same namespace share identities.
- `ctx.data_dir()` is the instance's own directory, kept across restarts and upgrades. Keep your
  state there (session files, cursors); the `giles-sdk/` subdirectory belongs to the SDK.

## Inbound events

Each event needs an id that is unique within the instance and, where the service allows, the same
after a restart (a message id from the service, not a counter). Giles stores every event before
acknowledging it, and drops repeats by id, so delivery is at least once and never twice into a
conversation.

You don't need to store events yourself: the SDK writes each event to the data directory before
sending it to Giles and sends it again until Giles acks it, even after your plugin crashed. The SDK
also stores an event before it asks your stream for the next one, so once your stream is polled
again you may forget the previous event. A source that can replay (an IMAP mailbox) can instead
keep a cursor and advance it in `ack`.

Report the connection's state with `ChannelEvent::Status`: `Connected`, `Error`, or `NeedsAuth`
with a QR code, pair code, or URL that `giles status` and the admin pages show to the operator.

## Attachments

Attachments never travel inside messages. For an inbound attachment, store the bytes with
`ctx.blobs().put(bytes, media_type, name)` (or `put_reader`) before emitting the event, and put the
returned `MediaRef` in the message's `attachments`; Giles fetches it and then tells the plugin to
delete it. For an outbound attachment, read it in `send` with `blobs.read(&media)`, `open`, or
`path`; it's only valid until `send` returns.

## Sending

`send` gets the reply as CommonMark. The SDK's `render` module converts it for the service:
`render::html`, `render::plain_text`, `render::irc` (split into lines that fit), and
`render::whatsapp`. If a recipient can't be reached right now (for instance an IRC account that
isn't online), return `Error::Unavailable`: Giles retries later rather than giving up.

## TLS

The SDK's `tls` module (on by default) gives `rustls` client settings that trust the Mozilla root
certificates built into the binary, the same ones Giles itself uses, so connections work the same
inside the sandbox on every host.

## Lifecycle

The SDK answers Giles' health checks. When Giles stops the plugin, it first asks it to shut down:
the SDK lets open calls finish for a few seconds and returns from `serve`. The SDK also returns when
Giles itself goes away, so a plugin never outlives the daemon; these are the only times it exits on
its own. Events not yet acknowledged are already on disk, so a plugin that is killed loses nothing.

Log to stderr (for example with `tracing-subscriber`); Giles records each line under the instance's
name. Anything written to stdout is discarded.

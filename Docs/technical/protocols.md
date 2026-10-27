# gRPC Protocols

The core talks to provider and channel plugins over gRPC, and serves an admin service to the CLI and
the web admin pages (PLAN section 8). Toolsets speak MCP instead and have no `.proto` contract. The
contracts are Protobuf files under `proto/`, checked with `buf` and compiled into the `giles-proto`
crate.

## Packages

Each package lives in `proto/giles/<package>/v1/<package>.proto`; the `buf` module root is `proto/`.

| Package             | Contents                                                              | Rust module                   |
| ------------------- | --------------------------------------------------------------------- | ----------------------------- |
| `giles.types.v1`    | Shared types mirroring `giles-types`: messages, blocks, `BlobRef`     | `giles_proto::types::v1`      |
| `giles.plugin.v1`   | `PluginService` (`Describe`, `Configure`, `Shutdown`, blob transfer)  | `giles_proto::plugin::v1`     |
| `giles.provider.v1` | `ProviderCapabilities` only; `ProviderService` comes with provider plugins | `giles_proto::provider::v1` |
| `giles.channel.v1`  | `ChannelService` (`Events`, `Send`) and `ChannelCapabilities`          | `giles_proto::channel::v1`    |
| `giles.admin.v1`    | `AdminService`, served by the core on `run/admin.sock`                | `giles_proto::admin::v1`      |

Every gRPC plugin serves `giles.plugin.v1.PluginService`, `grpc.health.v1.Health` (from the
`tonic-health` crate, not from these files), and exactly one kind service. The core is always the
client.

`giles.admin.v1` has the RPCs of v1: `Status`, `ListPlugins`, `RestartPlugin`, `ListSessions`,
`ResetSession`, `CreateLoginLink`, `UsageReport`, and `Backup`. The user and pairing RPCs of PLAN
section 8.7 belong to multi-user support and are added when it is built. Beyond PLAN section 8.7's
sketch, `StatusResponse` also reports the host's sandbox (`SandboxHost`) and every configured
channel with its last status (`ChannelInstance`), so built-in channels show up too. The core's side
is described in [Admin Service](admin.md).

## Conventions

- `buf` `STANDARD` lint, with no exceptions:
  - Service names end in `Service`, and every RPC has its own `<Rpc>Request` and `<Rpc>Response`
    messages, streaming RPCs included. Shared payloads are fields of those messages, e.g.
    `PutBlobRequest.chunk` and `GetBlobResponse.chunk` both carry a `BlobChunk`.
  - Enum values carry the enum's name as a prefix, and value 0 is `<ENUM>_UNSPECIFIED`. Where a
    value is required, an unspecified or unknown one is invalid (`INVALID_ARGUMENT`), except
    `CacheRetention`, whose unspecified value means the default, `SHORT`.
- `optional` scalars stand exactly where `giles-types` has an `Option`, so `prost` generates the
  same `Option` and set-but-empty stays distinct from unset. Message-typed fields are always
  optional in proto3; the ones the domain requires say "Always set" in their comment, and a missing
  one is `INVALID_ARGUMENT`.
- Ids are plain strings on the wire. The receiver validates them when converting to `giles-types`
  (non-empty, or SHA-256 hex for media).
- JSON values are JSON text in `string` fields named `*_json`: tool input, tool input schemas,
  `Opaque` data, instance config. `google.protobuf.Struct` would not do: it holds only objects and
  turns every number into a double, and `Opaque` data has to round-trip exactly.
- Times are `google.protobuf.Timestamp`.
- Attachments travel as `giles.types.v1.BlobRef`, never as bytes inside messages. On the wire the id
  is a plugin-local handle; the `Remote*` adapter moves the bytes with `GetBlob` and `PutBlob` and
  swaps the handle for the core's SHA-256 `MediaId` (PLAN section 8.3). A `BlobChunk` carries at
  most 1 MiB of data.
- Messages are limited to 16 MiB instead of `tonic`'s 4 MiB default. Clients and servers set it from
  `giles_proto::MAX_MESSAGE_SIZE`.

## Launch handshake

Before any gRPC call, the core and a plugin agree on a few fixed values (PLAN section 7.4). Both
sides take them from `giles_proto::handshake`, so they can't drift apart:

| Constant                       | Value                                                        |
| ------------------------------ | ------------------------------------------------------------ |
| `MAGIC_COOKIE_ENV`             | `GILES_PLUGIN_MAGIC`, set to `MAGIC_COOKIE`                  |
| `MAGIC_COOKIE`                 | `giles-plugin:5b2f0c7e9a1d4836`; never changes               |
| `PROTOCOLS_ENV`                | `GILES_PLUGIN_PROTOCOLS`: the majors the core speaks, comma-separated, e.g. `1` |
| `DATA_ENV`                     | `GILES_PLUGIN_DATA`: the instance's data directory           |
| `PLUGIN_FD`                    | 3, the plugin's end of the socket pair                       |
| `PROTOCOL_MAJOR`               | 1, the major of the `v1` packages                            |
| `HANDSHAKE_VERSION`            | 1, the first field of the handshake line                     |
| `EXIT_NO_SHARED_PROTOCOL`      | 76 (`EX_PROTOCOL`): the plugin shares no protocol major; don't retry until the binary changes |

`HandshakeLine` formats and parses the line the plugin prints on stdout once it serves,
`1|<protocol>|fd|3|grpc`. The parser accepts one trailing `\n` or `\r\n` and nothing else: no
spaces, no leading zeros or signs in the protocol, no other transport. Protocol majors are plain
decimal (`parse_major`). [Plugin SDK](plugin-sdk.md) describes the plugin side.

## Mapping to `giles-types`

`giles-proto` holds only the wire types. The plugin SDK and the core's `Remote*` adapters convert
between them and `giles-types`, moving fields.

| `giles-types`                                | `giles.types.v1`                                          |
| -------------------------------------------- | --------------------------------------------------------- |
| `Message { role, content, meta }`            | `Message { role, content, meta }`                          |
| `Role::{User, Assistant}`                    | `Role` `ROLE_USER`, `ROLE_ASSISTANT`                       |
| `MessageMeta`                                | `MessageMeta`; `at` always set, `sender` is the user id   |
| `Block`                                      | `Block`, with a `oneof block` of one message per variant  |
| `Block::Text { text }`                       | `TextBlock { text }`                                      |
| `Block::Image`, `Block::Audio { media }`     | `ImageBlock`, `AudioBlock { media: BlobRef }`             |
| `Block::Document { media, name }`            | `DocumentBlock { media, optional name }`                  |
| `Block::ToolUse { id, name, input }`         | `ToolUseBlock { id, name, input_json }`                   |
| `Block::ToolResult { tool_use_id, content, is_error }` | `ToolResultBlock` with the same fields          |
| `Block::Reasoning { text, opaque }`          | `ReasoningBlock { optional text, opaque }`                |
| `Block::Opaque(Opaque { api, data })`        | `Block.opaque`: `Opaque { api, data_json }`               |
| `BlockKind`                                  | `BlockKind`                                               |
| `MediaRef { id, media_type, size, name }`    | `BlobRef { id, media_type, size, optional name }`         |
| `Usage`                                      | `Usage`, the same four counts                             |
| `StopReason`                                 | `StopReason`                                              |
| `ToolSpec { name, description, input_schema }` | `ToolSpec { name, description, input_schema_json }`     |
| `GenerationParams`                           | `GenerationParams`, `optional` numbers                    |
| `CacheRetention::{Short, Long}`              | `CacheRetention`; `UNSPECIFIED` means `SHORT`             |
| `ConversationRef { id, kind, title }`        | `ConversationRef { id, kind, optional title }`            |
| `ConversationKind::{Direct, Group}`          | `ConversationKind`                                        |

The channel types map onto `giles.channel.v1`:

| `giles-types`                                  | `giles.channel.v1`                                          |
| ---------------------------------------------- | ----------------------------------------------------------- |
| `ChannelInfo { output, can_initiate }`         | `ChannelCapabilities`, in `DescribeResponse.channel`        |
| `ChannelInfo.identity_namespace`               | `giles.plugin.v1.ConfigureResponse.identity_namespace`      |
| `OutputMode`                                   | `OutputMode`                                                |
| `Channel::events`                              | `Events`: `EventsResponse.event` from plugin to core        |
| `Channel::ack`                                 | `Events`: `EventsRequest.ack` from core to plugin           |
| `ChannelEvent::{Message, Status} { id, .. }`   | `ChannelEvent { event_id, oneof event }`                    |
| `ChannelStatus::{Connected, NeedsAuth, Error}` | `ChannelStatus` `oneof state`; `Error`'s message is `error` |
| `AuthChallenge::{QrCode, PairCode, Url}`       | `NeedsAuth` `oneof challenge`: `qr_code`, `pair_code`, `url` |
| `InboundMessage`                               | `InboundMessage`; attachments are `BlobRef`s                |
| `Channel::send(OutboundMessage)`               | `Send(SendRequest)`; returns `SendResponse.message_id`      |
| `SendTarget::{Conversation, Recipient}`        | `SendRequest` `oneof target`: `conversation_id`, `recipient` |

`giles.provider.v1.ProviderCapabilities { media_types, reasoning }` describes a provider plugin as a
whole; the per-model `ModelInfo` comes with `ProviderService`.

## Versioning

The package name carries the major version (PLAN section 7.8). Within `v1`, changes are additive
only, and `buf breaking` enforces it:

- Add fields, messages, enum values, `oneof` variants, and RPCs. Never renumber, rename, retype, or
  remove one, and never move a field into or out of a `oneof`.
- A removed field's number (and name) is `reserved`. Numbers that the design set aside are reserved
  already, e.g. `DescribeResponse` 4 (config schema) and 12 (toolsets).
- Receivers ignore unknown fields. An unknown `oneof` variant decodes as no variant set: the core
  acks such a `ChannelEvent` by id and ignores it, so the plugin doesn't resend it forever.
- More generally, a `ChannelEvent` that fails conversion on the `Events` stream (an unknown nested
  variant or enum value, a missing "Always set" message, an empty id) is logged, acked by
  `event_id`, and dropped. A stream item can't be answered with `INVALID_ARGUMENT`, and leaving it
  unacked would make the plugin resend it forever.
- New optional behavior is announced, not assumed: plugins report typed capabilities in `Describe`,
  and the core sends its protocol minor version and feature names in `DescribeRequest`.
- A breaking change is a new package, `giles.<package>.v2`, in its own directory, served next to
  `v1` for as long as the core supports both.

## Code generation

`crates/giles-proto/build.rs` compiles every package with `tonic-prost-build`. Since `tonic` 0.14,
`prost` support lives in separate crates: `tonic-prost-build` generates the code, and the generated
code uses `tonic-prost`'s codec at run time.

- `protoc` comes from `PROTOC` if set, else from the `protoc-bin-vendored` crate, which bundles
  Google's binaries for Linux and macOS on x86-64 and ARM64. Building needs no system `protoc`. The
  well-known types (`google/protobuf/timestamp.proto`) come from the vendored include directory
  either way.
- Messages, and a client and a server for every service, are generated into `OUT_DIR` and included
  in one module per package, `giles_proto::<package>::v1`, marked
  `#[allow(missing_docs, clippy::all, clippy::pedantic)]`. Comments in the `.proto` files become the
  Rust docs. Well-known types map to `prost-types`.
- `giles_proto::FILE_DESCRIPTOR_SET` is the encoded descriptor set of every package, with its
  imports, for gRPC server reflection.
- `crates/giles-proto/proto` is a symlink to the repository's `proto/`. Cargo follows it when
  packaging, so the published crate contains the `.proto` files and builds on its own.

## Checks

`buf.yaml` at the repository root declares the module (`proto/`), the `STANDARD` lint rules, and the
`FILE` breaking rules, the strictest set, which also catches changes that only break generated code,
such as a renamed field. From the repository root:

```sh
buf lint
buf format --diff --exit-code   # `buf format -w` fixes it
buf breaking --against '<base>' # e.g. a directory or '.git#ref=<commit>'
```

CI runs all three in the Protobuf job, comparing each pull request against its base and each push to
`main` against the previous tip of `main` (see [Continuous Integration](ci.md)).

## Deviations from the PLAN snippets

The PLAN's Protobuf snippets (sections 8.2 to 8.7) were written before `buf` lint; they're updated
to match these files. The changes:

- Service names end in `Service`: `Plugin` became `PluginService`, `Admin` became `AdminService`.
- Streaming RPCs have their own request and response messages:
  `PutBlob(stream PutBlobRequest) returns (PutBlobResponse)`,
  `GetBlob(GetBlobRequest) returns (stream GetBlobResponse)`, both carrying a `BlobChunk`, and
  `Events(stream EventsRequest) returns (stream EventsResponse)`, with the `EventAck` in
  `EventsRequest`'s `oneof request` and the `ChannelEvent` in `EventsResponse`. Besides satisfying
  the lint, each stream direction can now gain fields independently.
- `InboundMessage.conversation` is a `giles.types.v1.ConversationRef`, which the admin service's
  sessions use too.
- Fields that are `Option`s in `giles-types` are `optional`: `BlobRef.name`,
  `InboundMessage.sender_display_name`, `reply_to`, and `message_id`, and `SendRequest.reply_to`.
- `DescribeResponse` reserves 12 (toolsets) at message level, since a `oneof` can't hold
  reservations, and `SandboxGrants` is defined in `giles.plugin.v1`.
- The deferred provider sketch (PLAN section 8.4) follows the same rules: `Complete` streams
  `CompleteResponse` messages, and `GenerationParams` is `giles.types.v1.GenerationParams`.

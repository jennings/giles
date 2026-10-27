# Outbox

`giles_core::outbox` delivers proactive sends: messages the core starts rather than replies in a
conversation ([Plan](../PLAN.md) §6.11, §8.5). They're addressed to an identity, such as the owner's
default identity (`[owner] default`, §1.1), and wait in the `outbox` table ([Storage](storage.md))
until the channel owning that identity accepts them or they expire. So the reply of a job that fires
before WhatsApp has reconnected isn't lost.

| Item                    | What it is                                                              |
| ----------------------- | ----------------------------------------------------------------------- |
| `Outbox`                | `new(store, registry, clock)`, `enqueue`, `remove`, `start`, `closed`; cheap to clone |
| `OutboxItem`            | `recipient: Identity`, `kind`, `user`, `markdown`, `attachments`, `expires_at` |
| `OutboxItem::new`       | To an identity, no user, no attachments, default expiry                 |
| `OutboxItem::to_owner`  | To the owner's default identity, `user` = `owner`                       |
| `OutboxKind`            | `JobReply`, `ApprovalPrompt`, `LoginCode`, `Notice` (`job_reply`, ...)  |
| `OutboxError`           | `Unowned`, `CannotInitiate`, `MissingExpiry`, `Expired`, `Store`        |
| `DEFAULT_EXPIRY`        | 24 hours                                                                |
| `BACKOFF_INITIAL`, `BACKOFF_MAX`, `RETRY_AFTER_MAX` | 1 s, 60 s, 1 h                              |
| `SEND_TIMEOUT`          | 60 s per send                                                           |

## Wiring

`Runtime::start` builds the outbox (`runtime.outbox`), so the scheduler and the approvals code can
enqueue as soon as the runtime exists. `giles serve` calls `Runtime::start_outbox(shutdown)` before
`start_sessions`, and waits for `runtime.outbox.closed()` after the session manager has stopped.
Rows enqueued before the sender starts just wait.

## Enqueueing

`enqueue(item)` stores one row and wakes the sender, returning the row's `OutboxId`:

- `recipient` is the whole identity, `<namespace>:<native id>` (the `recipient` column holds it as
  written; the sender splits it at each attempt, since only a registered channel knows where its
  namespace ends).
- `kind` is stored as `job_reply`, `approval_prompt`, `login_code`, or `notice`.
- `payload` is `{"markdown": ..., "attachments": [MediaRef, ...]}` (`attachments` omitted when
  empty). Blobs referenced there must outlive the row; the blob sweep scans outbox payloads for
  references (see [Storage](storage.md#sweep)).
- `expires_at`: approval prompts and login codes must set it (their approval's deadline, the code's
  expiry), else `MissingExpiry`; for the rest `None` means 24 hours after enqueueing. An expiry that
  isn't in the future is `Expired`.
- `instance` is the channel instance the identity routes to at enqueue time (see below), for the
  logs. When several stopped instances could own it, it's their names joined with `,`.

Routing at enqueue also refuses rows nothing could ever deliver: `Unowned` when no configured
channel can own the identity, `CannotInitiate` when its channel is registered with
`can_initiate = false` (the built-in web channel, for one). Those are the caller's to report.

## Routing

At each attempt the sender routes the identity again over the configured channel instances (sorted
by name) and the channels registered for them now, so a restarted plugin's channel is picked up:

1. A registered channel owns the identity when its `ChannelInfo::identity_namespace` followed by `:`
   is a prefix of the identity and a non-empty native id is left: `irc:libera.chat:account:stephen`
   belongs to namespace `irc:libera.chat` with native id `account:stephen`. The longest such
   namespace wins, then a channel that can initiate, then the first instance by name. If it can
   initiate, the row is sent to it as `SendTarget::Recipient(native id)`; otherwise it's dropped.
2. Otherwise the instances with no channel registered whose component is the identity's first
   segment may own it (a namespace is its component or starts with `<component>:`, as
   `validate_namespace` enforces), so the row waits for them: retried with backoff.
3. Otherwise no configured channel can own it (the configuration changed since it was enqueued):
   dropped.

## The Sender

One task, because rows aren't leased. Each pass:

1. `outbox_expire(now)`: drops expired rows, each logged at `warn` with id, instance, recipient,
   kind, attempts, and last error.
2. Up to 32 times, `outbox_due(now, 1)` with a fresh `now`: the earliest due row, fetched just
   before it's sent, so a row removed or expired while earlier ones were being sent is never sent.
   The row is sent (with a 60 s timeout), then
   - accepted: `outbox_delete`, logged at `info`;
   - a definite rejection of the message (`Error::InvalidRequest`, `InvalidId`, `NotFound`), a
     channel that can't initiate, or an unreadable payload: deleted and logged at `warn`;
   - anything else (`Error::Unavailable`, any other error, a timeout, no channel running yet):
     `outbox_retry` with the next attempt after `retry_after` if the channel gave one (clamped to 1
     s .. 1 h), else 1 s doubled per earlier failure up to 60 s. The first failure is logged at
     `info`, later ones at `debug`. A row removed meanwhile is left alone. A plugin that dies or
     drains for a restart mid-send fails it with `UNKNOWN` or `CANCELLED`, which `RemoteChannel`
     reports as `Unavailable` ([Plugin Host](plugin-host.md)); either way the row is retried until
     it expires.

   After a send times out, the pass doesn't send to that instance again: its other due rows get a
   retry without an attempt, so a wedged channel holds up the rest for at most 60 s per pass.
3. A full batch starts the next pass at once; otherwise the sender sleeps until `outbox_next_wakeup`
   (the earliest next attempt or expiry, rounded up to the millisecond), at most 5 minutes, or until
   `enqueue` wakes it. A database error is logged and the pass retried after 5 s.

Shutdown abandons the pass at once, even in the middle of a send, so `closed()` returns promptly and
`serve` goes on to stop the plugins. The row being sent stays as it was and is tried again at the
next start. Sends are one at a time; sending to each instance concurrently would keep one slow
channel from delaying the others at all, but isn't needed while the timeout rule above caps it.

The backoff stops at a minute because a plugin that comes up doesn't tell the outbox, so a minute is
the longest a reply waits after its channel is back. Delivery is at least once: a send that times
out, or is cut off by shutdown, after reaching the channel is sent again. Logs never include the
message's content.

## Expiry

- Approval prompts (job turns with `ask_to = "deliver_to"`) expire at their approval's deadline.
  When an approval resolves first, `remove(id)` deletes its prompt (`false` if it was already sent
  or gone); a send already under way still completes. Pending approvals live in memory and die with
  the process, so `start` first drops every `approval_prompt` row (`outbox_expire_kind`), logged
  like an expiry. Approval prompts must therefore be enqueued only after `start` (turns run only
  after `start_sessions`, which `serve` calls later).
- Login codes expire with the code. The web login code flow isn't built (v1 has links only), so
  nothing enqueues them yet.
- Everything else expires 24 hours after it was enqueued.

## Restart Notices

The "Interrupted by a restart" notices of startup recovery ([Sessions and Turns](sessions.md)) don't
go through the outbox: they're replies into the conversation the turn ran in
(`SendTarget::Conversation`), not sends to an identity, and recovery already holds them per instance
until a plugin channel attaches.

## Tests

`outbox/route.rs` unit tests: namespace prefix boundaries, namespaces with colons, longest
namespace, tie-breaking, channels that can't initiate, waiting for stopped instances of the
component, unowned identities, and the backoff and `retry_after` bounds. `outbox/tests.rs`, with a
real store and registry, fake channels, and paused Tokio time: delivery to the owner's default
identity on enqueue (row contents, attachments, recipient split); backoff 0, 1, 3, 7 s until the
channel accepts; each failure recorded on the row; `retry_after` honored; waiting for a channel that
registers later; dropping rows the channel rejects (`InvalidRequest`, `NotFound`) but retrying other
errors; a timed-out send retried, with the instance's other rows deferred to the next pass; shutdown
abandoning a hung send and keeping its row; rows removed or expired while an earlier send of the
pass hangs never sent; expiry at the deadline (tried 1 ms before, dropped at it) and after 24 hours;
missing and past expiries refused; approval prompts dropped at startup; a removed prompt never sent;
routing between two IRC instances, including while both are stopped; and identities whose channel
can't initiate (refused at enqueue, dropped when the channel turns out so).
`plugin_host/remote_channel/tests.rs` checks that `CANCELLED`, `UNKNOWN`, and `DEADLINE_EXCEEDED`
from `Send` are `Unavailable`.

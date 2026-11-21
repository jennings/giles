# Web interface design

This page is the design system of Doozers' web interface: how a page is laid out, which colors,
type, and spacing it uses, which components it's built from, and how it behaves. Build every new
page from it. The [web channel](web-channel.md) page covers routes, handlers, and templates; the
[HTTP server](http.md) page covers the layout template, assets, and the Content Security Policy.

## Sources

The design is the "Agent Console UI" canvas: one board per page at 1280 px wide, two phone boards at
390 px, a login board, and the sidebar component. Two boards on its "Proposed" page (My agents, My
routes) need per-user identity and aren't built.

The implementation differs from the boards on purpose in four ways:

- The boards load Inter and Space Grotesk from Google Fonts. Doozers uses the system font stack: the
  CSP allows no third-party requests, and we don't vendor fonts.
- The boards are dark only. Doozers also has a light theme, derived from the same roles, for an OS
  that prefers light.
- The sidebar footer has an initial badge next to the user's name.
- The boards style with inline `style=""`. Doozers can't: the CSP blocks inline styles, so every
  rule is a class in `crates/doozers-core/assets/doozers.css`.

## Principles

- **The server renders HTML.** Pages are askama templates. There is no client-side rendering, no
  client-side routing, and no JSON API behind the pages.
- **Native HTML first.** Use the element the browser already has before writing script or building a
  widget out of `<div>`s: `<dialog>`, `<details>`, `<form>`, `<button>`, `<select>`, `<datalist>`,
  `popover`, invoker commands, and the built-in form validation.
- **Modern browsers only.** Use current Baseline features freely: CSS nesting, `:has()`,
  `light-dark()`, `color-mix()`, `<dialog>` with `closedby`, `command` and `commandfor`, and
  `popover`. Don't add fallbacks for old browsers.
- **Script is the last resort.** A page must work with no script except where it streams (the chat
  page). The small per-form scripts only add conveniences, such as filling an id from a name.
- **Quiet and dense.** Dark surfaces a step apart, thin borders, one accent color, and text that
  tells the owner what each control does.

## Interaction model

### Navigation and actions

- Every navigation is a full page load through a real `<a href>`. There's no client-side history, no
  infinite scroll, and no pane that loads by script.
- Every change is a `<form method="post">` carrying the CSRF token. The handler answers
  `303 See Other` back to the page the change belongs to (POST, then redirect, then GET), so reload
  never posts twice.
- A form that fails validation renders again with what was typed and an error banner at the top
  (`role="alert"`). Nothing is stored.
- After a change worth reporting, the page shows a status banner (`role="status"`), such as "You're
  signed in to github." Otherwise the changed list is the feedback. There are no toasts.
- Edit forms carry the version they were loaded from. A save that would overwrite someone else's
  change is refused, with a link that reloads the current values.

### Confirmations

How much an action asks first depends on what it costs:

- **One click.** A cheap or reversible action runs at once: Disable and Enable a job, Up and Down on
  a route, Restart, Sign out, Stop an idle workspace, Archive and Unarchive a chat. A few deletions
  that the owner expects to be immediate (Delete job, Remove route, Forget memory) are one click
  too. The user docs say so.
- **A confirmation page.** An action whose consequences the server has to work out first goes
  through a page: Remove agent (its routes, jobs, and trash path), Remove and Rename MCP server (the
  agents it touches), Delete workspace, and Stop a workspace that is in use. The page is a GET. It
  shows a confirmation card that asks a question, lists what happens, warns about anything in
  progress, and has the danger button (a POST) and a Cancel link back.
- **A dialog.** A confirmation that needs nothing beyond the page already shown is a `<dialog>`, not
  a page and not `window.confirm()`, such as Delete chat. Open it with an invoker button and close
  it with HTML:

  ```html
  <button type="button" class="button button-danger" command="show-modal" commandfor="delete-x">Delete</button>
  <dialog id="delete-x" class="card confirm" closedby="any" aria-labelledby="delete-x-title">
    <h2 id="delete-x-title">Delete x?</h2>
    <p>It can't be brought back.</p>
    <div class="form-actions">
      <form method="post" action="/things/x/delete">
        <input type="hidden" name="csrf" value="…">
        <button type="submit" class="button button-danger-solid">Delete x</button>
      </form>
      <form method="dialog"><button class="button-quiet">Cancel</button></form>
    </div>
  </dialog>
  ```

  `showModal()` gives the backdrop, the focus trap, Escape to close, and an inert page behind it.
  `closedby="any"` closes it on a backdrop click.

### Native elements

Use these instead of script or custom widgets:

| Need                                   | Element                                                                   |
| -------------------------------------- | ------------------------------------------------------------------------- |
| A modal (navigation drawer, confirm)   | `<dialog>`, opened by `command="show-modal"` `commandfor="<id>"`          |
| Closing a dialog                       | `<form method="dialog">`, `command="close"`, or `closedby="any"`          |
| A menu or popup that isn't modal       | `popover` with `popovertarget`                                            |
| Show and hide (Advanced, Edit, output) | `<details>` and `<summary>`                                               |
| Suggestions in a text field            | `<datalist>`                                                              |
| Required fields, formats, ranges       | `required`, `pattern`, `type="number"` with `min`, `type="url"`           |
| Dates and times                        | `type="datetime-local"`, and `<time datetime>` for display                |
| State that depends on a control        | CSS `:has()`, `:checked`, `:user-invalid`                                 |

### Live updates

- The chat page is the only page that updates without a reload. htmx posts the compose form and the
  approval buttons, and the page follows the chat's server-sent events. Both forms also work as
  plain posts.
- The status page reloads itself (`<meta http-equiv="refresh">`) only while a channel is waiting to
  be linked, so the QR code stays current.
- Every other page shows the state at the time it loaded. Reload to see newer state.

## Layout

### App shell

Every page for a signed-in user has two columns: the sidebar and the content.

- The sidebar is 232 px wide, full height, and stays put while the content scrolls.
- The content (`<main class="page">`) is padded 28 px top and bottom and 32 px at the sides. Its
  blocks stack with a 20 px gap, or 24 px on list and status pages.
- Content doesn't center. It starts at the sidebar's edge, and long text and forms have a maximum
  width instead.

The login, login-required, invite, setup, error, and not-found pages have no sidebar. They center
one card on the page.

### Sidebar

From top to bottom:

1. The brand: a 30 px square badge with radius 8 px, the accent fill, and a white "G", then
   "Doozers" at 18 px, weight 600. It links to `/`.
2. Navigation groups. Each has a label (11 px, uppercase, 0.08em tracking, faint) and links 40 px
   tall with 12 px side padding and radius 8 px. The current page's link has the "nav current" fill,
   white text, weight 600, and `aria-current="page"`. The others are secondary text with no fill.
   - **You**: Chats, Scheduled jobs, Connections, SSH keys.
   - **Admin**: Status, Plugins, Agents, Routes, MCP servers, Workspaces, SSH keys, Known SSH hosts.
     It's shown only to admins.
3. The user footer, pushed to the bottom with a top border: a 30 px initial badge, the user's id,
   and **Log out**. Log out appears only for a cookie login, since a proxy login can't log out.
   Today the id is always `owner`. This is where user information goes once there are other users.

Pages below a link in the sidebar mark that link current: the job form marks Scheduled jobs, and an
agent's edit page marks Agents.

### Narrow screens

Below 760 px:

- The sidebar is hidden. A 64 px top bar replaces it, with a 44 px menu button and the page title.
- The menu button opens the navigation in a `<dialog class="nav-dialog">` with
  `command="show-modal"`. The dialog is a 300 px drawer on the left over a scrim, with a close
  button and the same groups and footer. Links are 44 px tall. `closedby="any"` closes it on a
  backdrop click.
- The content's padding drops to 20 px 16 px.
- Two-column field grids become one column.
- Nothing scrolls sideways. A table becomes a stacked list: one block per row, with the row's name
  and state on its first line, the details in short lines under it, and its buttons last
  ([Tables](#tables)).
- The stat cards become one card of label and value lines, and a QR code to scan hides behind
  **Show QR code**, since the page is likely on the phone that would scan it.
- All touch targets are at least 44 px tall.

### Page patterns

**List page** (Scheduled jobs, Agents, Routes, MCP servers):

1. The page head: the `h1` on the left and the primary action (such as "New job") on the right, in
   one row that wraps.
2. Optionally, a lede: one paragraph of muted text, at most about 800 px wide, saying what the list
   is and how it behaves.
3. Banners, if any.
4. The items, as a table in a card or as a card grid. If there are none, a sentence that says so and
   how to add one.

**Detail page** (an agent, an MCP server):

1. A back link to the parent list, named after it ("Agents").
2. The page head: the `h1` and a row of action buttons (primary first, then secondary, then the
   danger outline).
3. A facts card: a `<dl>` of labels and values.
4. Sections, each with an `h2`.

**Form page** (new job, new route, the agent and MCP server forms):

1. A back link to where Cancel goes.
2. The `h1`, such as "New job" or "Edit Jeeves".
3. The error banner, if the last post failed.
4. The form: one card (at most 680 px wide, 24 px padding), or for long forms a stack of cards and
   fieldsets (at most 820 px), each grouping related fields.
5. The form actions: the primary submit, then a quiet Cancel link.

**Confirmation page** (remove an agent, delete a workspace): a back link, then a confirmation card
at most 620 px wide with the warn border. In it: an `h1` that asks a question ("Delete workspace
`ws-7f3a9c`?"), what happens, a warning banner if something is in progress, then the solid danger
button and Cancel.

**Chats** (`/`, `/chats/archived`, and `/chats/<id>`): two panes beside the sidebar.

- The list pane is at least 280 px wide on the pane background, with a right border and 24 px 16 px
  padding. It holds the `h1` "Chats" (22 px) with **New chat**, the "Chat with" picker card, the
  list, and an "Archived chats" link at the foot. Each item is a block link with radius 8 px: the
  title (14 px, 600), then the agent and the time in a 12 px muted row. The open chat's item has the
  selected fill and `aria-current="page"`.
- The archived list pane has a "Chats" back link and the `h1` "Archived chats" instead, without
  **New chat** or the picker. An archived chat shows beside it.
- The chat pane has a header with a bottom border (18 px 28 px): the chat title as an `h2` at 20 px,
  then "with Jeeves" muted, then, pushed to the end, small **Archive** (or **Unarchive**) and
  **Delete** buttons once the chat has something stored. Delete opens the confirmation dialog. The
  messages and the compose form sit in a centered column at most 880 px wide. An archived chat has
  an info banner at the top of the column.
- With no chat open, the chat pane shows a muted "Pick a chat or start one" message ("Pick a chat"
  beside the archived list).
- On narrow screens only one pane shows. `/` shows the list, and a chat shows the chat with an "All
  chats" link in its header.

## Color

Colors are custom properties on `:root`, each set with `light-dark(<light>, <dark>)`. `:root` has
`color-scheme: light dark`, so the OS setting picks the theme. Pages use the roles, never a hex
value.

| Token                | Dark      | Light     | Use                                                       |
| -------------------- | --------- | --------- | --------------------------------------------------------- |
| `--bg`               | `#0A1020` | `#F4F6FB` | Page background                                           |
| `--bg-pane`          | `#0C1426` | `#EDF1F8` | The chat list pane                                        |
| `--bg-sidebar`       | `#0D1528` | `#FFFFFF` | Sidebar, top bar, navigation drawer                       |
| `--surface`          | `#111A2E` | `#FFFFFF` | Cards, table cards, the assistant's bubbles               |
| `--surface-inset`    | `#0D1528` | `#F6F8FC` | Fields, `<pre>` inside a card, tool calls, attachments    |
| `--surface-selected` | `#16213A` | `#E3EAF7` | The open chat in the list                                 |
| `--border`           | `#1E2A44` | `#DCE3EF` | Card borders, dividers, table rows                        |
| `--border-control`   | `#24314F` | `#C3CEE2` | Fields, secondary buttons                                 |
| `--text`             | `#EEF2FA` | `#0E1626` | Body text, headings                                       |
| `--text-secondary`   | `#C3CEE4` | `#35425C` | Field labels, sidebar links, secondary table cells        |
| `--text-muted`       | `#9FB0CF` | `#55637E` | Ledes, help text, meta                                    |
| `--text-faint`       | `#8E9FC2` | `#5F6D88` | Table headers, `<dt>`, nav group labels, row numbers      |
| `--code`             | `#D6E2FF` | `#1D3B7A` | `<code>` and `<pre>` text                                 |
| `--link`             | `#8FB4FF` | `#2556C4` | Links                                                     |
| `--link-hover`       | `#B9D0FF` | `#1B3A7A` | Hovered links                                             |
| `--accent`           | `#2F63D6` | `#2F63D6` | Primary buttons, the user's bubbles, badges, focus ring   |
| `--on-accent`        | `#FFFFFF` | `#FFFFFF` | Text on the accent                                        |
| `--nav-current`      | `#1B3A7A` | `#DCE6FB` | The current sidebar link                                  |
| `--on-nav-current`   | `#FFFFFF` | `#163378` | Text of the current sidebar link                          |
| `--info-bg`          | `#13294F` | `#DCE6FB` | Status banners, the "ok" pill                             |
| `--info-text`        | `#CFE0FF` | `#163378` | Status banner text                                        |
| `--ok-text`          | `#9CC0FF` | `#1B3A7A` | "ok" pill text, a good stat value                         |
| `--neutral-bg`       | `#1A2440` | `#E8ECF3` | Neutral pill                                              |
| `--neutral-text`     | `#B5C2DC` | `#4A5670` | Neutral pill text                                         |
| `--approval-bg`      | `#0F1D3A` | `#EEF3FD` | An approval request                                       |
| `--warn-border`      | `#6B4420` | `#E3B98C` | Danger outline buttons, confirmation cards, alerts        |
| `--warn-text`        | `#F5B979` | `#8A4510` | Errors, warnings, danger button text                      |
| `--warn-bg`          | `#2A1C10` | `#FDF1E4` | Alert and warning banners                                 |
| `--warn-body`        | `#F3DCC2` | `#5A3210` | Text inside a warning banner                              |
| `--warn-pill-bg`     | `#3A2614` | `#FBE6CF` | "warn" pill                                               |
| `--danger`           | `#9A4E12` | `#9A4E12` | The solid danger button                                   |
| `--disabled-bg`      | `#0B1222` | `#EEF1F6` | Disabled fields                                           |
| `--scrim`            | `#050912B3` | `#0E162666` | A modal dialog's backdrop                               |

The palette has no red and no green. Good is blue, a problem is orange, and neutral is grey. Blue
and orange stay distinct with any common color vision deficiency, so keep it that way: don't
introduce a green "running" or a red "failed".

## Type

All text uses the system UI stack (`--font`:
`system-ui, -apple-system, "Segoe UI", Roboto, sans-serif`). Headings use `--font-display`, which is
the same stack today at weight 600. Code uses `--font-mono`:
`ui-monospace, SFMono-Regular, Menlo, Consolas, monospace`. Sizes are in `rem` in the stylesheet.
This table gives them in px at the default 16 px root.

| Role                                   | Size  | Weight | Other                                |
| -------------------------------------- | ----- | ------ | ------------------------------------ |
| Page title (`h1`)                      | 26 px | 600    | Display font                         |
| Chats pane title, confirmation title   | 22 px | 600    | Display font                         |
| Stat value                             | 22 px | 600    | Display font                         |
| Chat title (`h2` in the chat header)   | 20 px | 600    | Display font                         |
| Section heading (`h2`)                 | 18 px | 600    | Display font                         |
| Card title                             | 16 px | 600    | 17 px on plugin cards                |
| Fieldset legend                        | 15 px | 600    |                                      |
| Body, table cells, buttons             | 14 px | 400    | Line height 1.5                      |
| Lede                                   | 14 px | 400    | Line height 1.55, muted              |
| Field label                            | 13 px | 600    | Secondary                            |
| Help text, code in tables              | 13 px | 400    | Muted                                |
| Meta, pills                            | 12 px | 400    | Pills are 600                        |
| Table header                           | 12 px | 600    | Uppercase, 0.06em tracking, faint    |
| Nav group label                        | 11 px | 600    | Uppercase, 0.08em tracking, faint    |

Values that are names, such as ids, models, tool names, globs, paths, cron patterns, and URLs, are
`<code>`. Fields that take such a value use the monospace font (`class="mono"`).

## Spacing and shape

Spacing comes in a few steps: 2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 24, and 28 px. Use the steps the
patterns above name and don't invent new ones.

| Thing                         | Value                                                     |
| ----------------------------- | --------------------------------------------------------- |
| Content padding               | 28 px 32 px (narrow: 20 px 16 px)                         |
| Gap between page blocks       | 20 px (list and status pages: 24 px)                      |
| Card padding                  | 18 px 20 px (form cards: 24 px; small items: 14 px 18 px) |
| Gap inside a card             | 14 px between groups, 8 px in a `<dl>` row                |
| Field: label to control       | 6 px                                                      |
| Between fields                | 16 to 18 px                                               |
| Table cell                    | 12 to 14 px by 16 px                                      |
| Card grid                     | `repeat(auto-fill, minmax(420px, 1fr))`, gap 16 px        |
| Stat grid                     | `repeat(auto-fit, minmax(220px, 1fr))`, gap 16 px         |
| Lede width                    | at most 820 px                                            |
| Form width                    | 680 px, 760 px for the MCP server form, 820 px for agents |
| Confirmation card width       | 620 px                                                    |
| Chat column width             | 880 px                                                    |
| Standalone card width         | 440 px, 32 px padding, radius 16 px                       |

| Radius   | Used on                                                                |
| -------- | ---------------------------------------------------------------------- |
| 8 px     | Buttons, fields, nav links, chat list items, `<pre>` in cards          |
| 10 px    | Banners, the compose field, tool calls, approvals, attachments         |
| 12 px    | Cards and table cards                                                  |
| 14 px    | Chat bubbles, with a 4 px corner on the speaker's side at the bottom   |
| 16 px    | The standalone card                                                    |
| 999 px   | Pills                                                                  |

Borders are 1 px. There are no shadows except the navigation drawer's edge.

Control heights:

| Control                                                    | Height |
| ---------------------------------------------------------- | ------ |
| Fields, selects, header buttons on detail pages            | 40 px  |
| A page's primary action, form submits, Cancel              | 44 px  |
| Card buttons (Restart, Stop, Sign out)                     | 36 px  |
| Table row buttons (`button-small`)                         | 34 px  |
| The standalone page's button                               | 48 px  |

## Components

Each component is a class in `doozers.css`. Reuse these. Add a class only for something new, and
document it here.

### Shell and page

- `app.html` lays out `.app`: the `.sidebar` (with `.brand` and its `.brand-mark`), the `.topbar` of
  narrow screens with its `.icon-button`, and the `.nav-dialog`. `nav.html` holds the `.nav`
  (`.nav-label` and `.nav-list` per group) and the `.account` footer with its `.avatar`.
- `<main class="page">` is the content column. In it, `.page-head` is the title row, `.actions` a
  row of buttons, `.back` the back link, `.lede` the intro paragraph, and `.section` a titled
  section (a column with a 12 px gap).
- Text helpers: `.muted` (muted color), `.meta` (12 px muted), `.warn-text`, `.mono`, and `.sr-only`
  (read out but not shown).

### Buttons

`<button>` and `<a>` share the button classes, and a bare `<button>` is a secondary button.

| Class                  | Look                                                       | Use                                    |
| ---------------------- | ---------------------------------------------------------- | -------------------------------------- |
| `button`               | Transparent, control border, text color                    | Secondary actions                      |
| `button-primary`       | Accent fill, white text, weight 600, no border             | The one main action of a page or form  |
| `button-danger`        | Transparent, warn border, warn text                        | Delete and Remove                      |
| `button-danger-solid`  | Danger fill, white text, weight 600                        | The final button of a confirmation     |
| `button-quiet`         | Link color, no border or fill, same height                 | Cancel                                 |
| `button-small`         | 34 px tall, 13 px text, 12 px padding                      | Buttons in table rows and memory items |

Buttons are 8 px radius with 14 to 22 px side padding. A page has at most one primary button in
view, except the chat page's approvals. Inside a table, every action is a `button-small`, links
(Edit, Chat) included: outlined, with the `button-danger` action (Delete, Remove) last.

### Fields

- `.form` is a form card (24 px padding, at most 680 px wide). `.form-wide` widens it to 760 px.
  `.form-sections` is a long form's stack of cards and fieldsets, at most 820 px wide.
- `.field` is a column: the `<label>` (13 px, 600, secondary), the control, then `.help` (13 px,
  muted). A value that can't be edited here uses `<span class="field-label">` and
  `<span class="field-value">` instead.
- Controls are 40 px tall with 12 px side padding, radius 8 px, the control border, the inset
  surface, and the text color. Textareas resize vertically. A disabled control uses the disabled
  surface and faint text.
- Every one-line control has the same fixed `height: 40px`, `<select>` included, so a select next to
  a text field lines up with it. Selects use `appearance: none`, because some browsers ignore
  `height` on a native select, and draw their chevron with background gradients in `--text-muted`.
  Don't give one kind of control its own height or padding.
- `.input-group` attaches an icon button to a field's right edge (`.input-group-button`, 44 px wide,
  the inset surface), such as the copy button of a public key. The field loses its right corners
  only while the button is shown.
- `.field-row` puts two fields side by side (a two-column grid with a 16 px gap). It becomes one
  column on narrow screens.
- `.control-row` puts two controls under one label, such as the authentication choice and its token.
- `.check` is a checkbox row: an 18 px box, a 10 px gap, then the label text. `.check-grid` puts
  checkbox rows in two columns.
- `<fieldset class="card">` groups checkboxes. Its `<legend>` is 15 px, 600.
- `.form-actions` is the row of the submit button and Cancel.
- `.search` is an inline search form: label, field, and button in one row.
- A field that fails the browser's checks (`:user-invalid`) gets the warn border.

### Surfaces

- `.card`: surface fill, 1 px border, radius 12 px, 18 px 20 px padding, a column with a 14 px gap.
  `.card-head` puts the card title on the left and its buttons on the right.
- `.card-grid`: a grid of cards at least 420 px wide (Plugins, Workspaces). `.card-grid-narrow`
  makes them at least 300 px (Channels).
- `.chips` is a row of `.chip`s: small bordered facts above a list, such as each environment's free
  space.
- `details.disclosure` is a card's "show more" below a top border, with a link-colored summary.
- `details.qr-toggle` holds something only narrow screens fold away: its summary ("Show QR code")
  shows below 760 px, and above that the summary is hidden and the content always shown
  (`::details-content`).
- `.stats` holds `.stat` cards, each with a `.stat-label` (12 px, uppercase, faint) and a
  `.stat-value` (22 px, display font). Below 760 px they become one card of label and value lines.
- `.confirm`: a card with the warn border, at most 620 px wide. As a `<dialog>` it has the scrim
  backdrop, and it's hidden while closed, since `.card` sets `display`.
- `.subtitle`: the quieter second half of a heading, such as "doozers-channel-irc, wasm" after a
  plugin's name.
- `.highlight`: a card with the accent border, for one that needs attention now (a channel waiting
  to be linked).
- `.item`: a smaller card (14 px 18 px padding, at most 780 px wide) in a list of them, such as a
  memory.
- `.inset`: a `<pre>` block inside a card, on the inset surface, at most 180 px tall and scrolling.
- `.facts`: a `<dl>` laid out in two columns, `max-content` then the rest. `<dt>` is faint, and long
  values wrap anywhere.

### Tables

A table sits in `.table-wrap`: a card with no padding. The table has `class="table"`, the full
width, and a minimum width of 600 px that keeps its columns readable; on a wide screen too narrow
for that, the card scrolls sideways. `.table-wide` raises the minimum to 980 px for a table with
many columns, such as the jobs.

- Header cells: 12 px uppercase faint, 12 px 16 px padding, left aligned.
- Body rows have a top border, with 14 px 16 px cells, aligned top.
- The actions column's header is `<span class="sr-only">Actions</span>`, and its cells are
  `.row-actions`: right aligned, not wrapping.
- Every other cell has `data-label` set to its column's name, such as `<td data-label="Last run">`.

Below 760 px a table becomes a stacked list. The header row is hidden, and each row becomes a block
whose cells flow into lines by an `m-*` class on each cell. Give every cell one:

| Class         | Line                    | Look                                                          |
| ------------- | ----------------------- | ------------------------------------------------------------- |
| `.m-lead`     | 1, before the title     | Faint, such as a route's position                             |
| `.m-title`    | 1                       | 15 px, 600: what the row is, such as the job or server name   |
| `.m-aside`    | 1, at the right         | The row's state: a pill, or the next run                      |
| `.m-meta`     | 2                       | 13 px secondary, values joined by " · "                       |
| `.m-chip`     | 2                       | A small chip reading "`data-label`: value", such as a route's keys |
| `.m-body`     | a line of its own       | Body text, such as a prompt, a location, or an error          |
| `.m-foot`     | after the body lines    | 12 px muted, values joined by " · "                           |
| `.row-actions`| last                    | The buttons in a row, the danger action at the far end        |

- `.m-label` puts "`data-label`: " before the value, such as "Last run: Never".
- `.m-split` hands a cell's parts to different lines: the cell disappears and its children take
  `m-*` classes, such as a sign-in cell's pill (`.m-aside`) and its detail (`.m-meta`).
- `.m-hide` leaves a cell out on narrow screens. Empty cells are always left out.
- A cell with no `m-*` class gets a body line of its own, so nothing is lost.
- `.secondary` cells use the secondary text color, and `.num` cells (a route's position) the faint
  one. A `.long` cell holds free text that may be long, such as a prompt or an error: 200 to 280 px
  wide, wrapping anywhere.
- An empty filter value reads "any" in muted italics (`.any`).
- A problem with a row goes in a following `.problem-row` that spans the columns, in warn text.

### Pills and banners

- `.pill` is 12 px, weight 600, radius 999 px, and 4 px 10 px padding. Its first letter is a
  capital, so a server's state word ("running") reads "Running". Add `.pill-ok` (running, signed
  in), `.pill-neutral` (stopped, not signed in), or `.pill-warn` (failed, in use, waiting). The
  admin pages' state classes map onto these: `.pill-unknown` is neutral and `.pill-bad` is warn.
- `.banner` is 14 px text, radius 10 px, and 12 px 16 px padding:
  - `.banner-info` (`role="status"`): the result of an action.
  - `.banner-warn`: something the owner should fix, such as leftover `[[routes]]` in doozers.toml.
  - `.banner-error` (`role="alert"`): a failed post or a configuration error. It has the warn border
    and can start with a `<strong>` title in warn text.

### Chat

- `.chats` is the two-pane grid: `.chat-list-pane` (with the `.agent-picker` card, the `.chat-list`
  of `.chat-item-title` and `.chat-item-meta`, and the `.archived-link`) and `.chat-pane` (with the
  `.chat-head` and its `.chat-actions`, then a `.chat-column` of `.messages` and the compose form,
  or `.chat-empty`). The container has `.has-chat` when a chat is open, which picks the pane narrow
  screens show.
- `.chat-notice` is a centered muted line among the messages, such as "Older messages aren't shown."
- `.msg.user`: right aligned, at most 75% wide, accent fill, white text, radius 14 14 4 14.
- `.msg.assistant`: left aligned, at most 80% wide, surface fill and border, radius 14 14 14 4.
- `.tool`: full width, inset surface, no border (so it reads apart from a reply), radius 10 px. The
  whole collapsed bubble is a `<summary>`: the name (13 px, 600) and status, the input's first line
  cut with an ellipsis, and a link-colored hint ("Output, 14 lines"). Opened, `.tool-body` shows the
  whole input and output as `<pre>`.
- `.approval`: full width, accent border, approval fill. It has the prompt, the call's input, and
  the buttons Allow (primary), Allow for this session, and Deny.
- `.attachment`: a 10 px 14 px chip with a file icon and the name. Images show inline.
- `.compose`: a textarea that fills the row, then Send (primary, 44 px), under a top border.

### Standalone pages

`<main class="standalone">` centers one `.card.solo`: the brand (34 px badge, 20 px name), the `h1`
(24 px), the text, and a full-width 48 px primary button.

## Writing

- Sentence case everywhere: headings, buttons, labels, and table headers.
- Buttons name the action, and the confirming button names the object: "Delete workspace", "Remove
  agent".
- A confirmation's title is a question.
- Help text goes under its field, says what the field does, and gives an example in `<code>`.
- Say what happens, not how it works internally.

## Accessibility

- Every control has a `<label>`. A control without a visible label gets an `aria-label`.
- A repeated button in a table names its row: `aria-label="Delete job Check CI"`.
- The current navigation link has `aria-current="page"`, and so does the open chat in the list.
- Error banners are `role="alert"`, and result banners are `role="status"`.
- Focus is always visible: a 2 px accent outline with a 2 px offset (`:focus-visible`).
- Icons are decorative SVGs with `aria-hidden="true"` next to text, or inside a button with an
  `aria-label`.
- Text meets 4.5:1 contrast on its surface in both themes.

## Adding a page

1. Pick the pattern: list, detail, form, confirmation, or standalone.
2. Extend `web/app.html`, pass the page's `Shell` (the signed-in user and the current navigation
   link), and put the content in `{% block content %}`.
3. Build it from the components above. Don't add inline `style`, inline script, or `on*` attributes.
4. Choose how each action confirms, following [Confirmations](#confirmations).
5. Check it in both themes, and below 760 px wide.

# Agent instructions

- Docs/IDEA.md contains a the general idea for this project
- Docs/PLAN.md contains design notes for this project

## Build

- Cargo is the repository build system.
- Create documentation with mdBook in the Docs directory
  - User documentation describes what the system is, how to use it, how to configure it, etc.
  - Technical documentation describes significant design decisions of the implementation:
    Architecture overview, wire protocols, major subsystems, etc.

## Development process

- Update tests in the same commit that changes behavior.
- Update documentation in the same commit when the system changed:
  - Update user documentation for any user-facing change.
  - Update technical documentation for implementation changes.

## Writing user documentation

The reader is someone installing and running Giles. Implementation detail belongs in
`Docs/technical/`; link there instead of explaining it.

- Cut anything the reader wouldn't act on:
  - Mechanism. "Giles listens on loopback, so bind to all interfaces", not why the port can't be
    reached.
  - Internals: env vars the system sets, account names, internal paths, signal handling, startup
    order, retry schedules, log lines nobody looks for.
  - Build, release, CI, and licensing details.
  - "Does nothing" statements ("inert until configured", "unknown keys warn").
  - Results nobody acts on ("Giles creates the rest on first start").
  - Rare environments. Keep only the one setting that works around them.
- Before cutting a fact a reader could act on, check that `Docs/technical/` or another page covers
  it. Otherwise keep it.
- Keep commands and config verbatim in code blocks, hard requirements, how to check that it worked,
  and one escape hatch per failure.
- State safety warnings plainly, bolded once ("**Do not expose it to the public Internet**").
- Give one concrete recommendation, not a menu of options.
- State the default before the alternative.
- Open with one sentence saying what the page is for.
- Order sections the way the reader does the steps.
- Make a series of three or more items a bullet list.
- Keep paragraphs to one to three sentences, one idea each.
- Give each page one Diátaxis type (tutorial, how-to, reference, or explanation).
- Headings name the topic or what the reader does ("Create a data volume").
- Every heading in `Docs/`, user or technical, and every `SUMMARY.md` entry uses sentence case:
  capitalize only the first word, proper nouns, and acronyms.
- Write in the second person; contractions are fine. Use plain subject–verb sentences: no semicolon
  chains, "X: Y" constructions, or "so … so …" links.
- Avoid "just as", "at the cost of", "as long as", "simply", and needless contrast ("X, not Y").
- Put links inline on the noun (`[The web interface](…) does not handle TLS`), not as parenthetical
  `([Title](page.md#anchor))` asides.
- Name a single config key by its dotted path, such as `channel.<name>.unsandboxed = true`.
- Wrap prose at 100 columns. Don't split inline code across lines.
- When renaming or removing a heading, fix every `<page>.md#<anchor>` link to it. mdBook doesn't
  check anchors.
- Some pages' TOML blocks are parsed by tests (`include_str!` of `Docs/user/*.md`). Keep the blocks
  those tests read.

## Tracking work

- Outstanding work is in GitHub issues at jennings/giles (use `gh`). Do not leave TODO or FIXME
  notes or checklists in the repository for work that remains: file an issue.
- One issue per unit of work: something that lands as one reviewable commit or one small stack of
  commits. Larger work gets a parent issue with sub-issues (GitHub sub-issues). Add one with:
  `gh api -X POST repos/jennings/giles/issues/<parent>/sub_issues -F sub_issue_id=<id>`, where
  `<id>` is the child's database id, from `gh api repos/jennings/giles/issues/<n> --jq .id`.
- Use the existing labels: `bug`, `enhancement`, `documentation`, `question`, `help wanted`.
- An issue says what and why, what done looks like, and any decisions the owner needs to make (label
  those `question`).
- Before starting work, find or file its issue. Work the owner asked for that has no issue gets one
  first.
- Commit messages reference the issue, so it closes when the work reaches `main` (closing keywords
  act when the commit lands on the default branch):
  - Put `Fixes #N` (or `Closes #N`) as a footer line on the commit that completes the issue.
  - Use `Refs #N` on earlier commits of a multi-commit issue, and for partial work.
  - For a parent issue, its sub-issues' commits close the sub-issues; close the parent by hand when
    all of them are done.
  - Subject lines stay Conventional Commits.
- Record findings (design decisions, follow-ups found during the work, flaky tests) as issue
  comments or new issues, not in commit messages or files.

## Commits

- Every commit must build and pass all tests.
- Every commit must be properly formatted.
- Each commit should contain an atomic unit of work.
- Do not mix refactoring and functionality changes in a single commit. First refactor, then change
  functionality.
- Commits should be sized to be human reviewable, whenever reasonably possible. (Exception:
  Generated files, data)
- Commit messages should follow the Conventional Commits style.

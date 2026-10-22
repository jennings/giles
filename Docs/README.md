# Giles

Giles is a small, long-running agent daemon written in Rust. The owner talks to LLM agents over
several channels (web, IRC, email, WhatsApp); the agents call model providers, use tools, and run
scheduled jobs. The core does orchestration only: identity, routing, sessions, the agent loop,
scheduling, policy, storage, and supervising plugins. Providers, channels, and toolsets are
components behind one Rust trait each, either built into the core or run as sandboxed plugin
processes over gRPC.

Giles is under construction. Chapters are added as the features they describe land.

## How this book is organized

- **User Guide**: what Giles does, how to install, configure, and run it.
- **Technical**: how Giles is built: architecture, crates, wire protocols, major subsystems, and the
  development workflow.
- **Design Background**: the documents the implementation starts from. [Idea](IDEA.md) explains what
  Giles is for and roughly how each feature works; [Plan](PLAN.md) is the detailed design. Where the
  two differ, the plan wins, and where the implementation differs from the plan, the User Guide and
  Technical chapters describe what was actually built.

The book is built with [mdBook](https://rust-lang.github.io/mdBook/) from the `Docs/` directory of
the repository:

```sh
mdbook build Docs   # output in Docs/book/
mdbook serve Docs   # live preview at http://localhost:3000
```

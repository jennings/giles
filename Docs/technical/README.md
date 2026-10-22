# Technical Overview

This part of the book describes how Giles is implemented: the significant design decisions, the
architecture, the wire protocols, and the major subsystems. It records what was built; the design
it started from is in [Plan](../PLAN.md).

The shape of the system, in brief:

- One `giles` daemon process holds the core: identity, routing, sessions, the agent loop,
  scheduling, policy, storage, and plugin supervision.
- Providers, channels, and toolsets are each one Rust trait, defined in `giles-types`. The core
  talks only to the trait, so a built-in component and an out-of-process plugin look the same.
- Plugins are separate executables that the core launches in a sandbox and talks to over gRPC
  (`giles-proto`); `giles-plugin-sdk` serves a trait implementation as a plugin.
- State lives in SQLite and a content-addressed blob store under `GILES_HOME` (`giles-store`).

# User Guide

This part of the book is for the person running Giles: what it does, how to install and configure
it, and how to talk to its agents.

Giles v1 serves one person, the owner. The owner's identities on each channel are declared in
`giles.toml`; messages from anyone else are dropped and logged. Each agent has its own files
(`SOUL.md`, `MEMORY.md`), model, tools, and scheduled jobs. Every conversation (a WhatsApp chat, an
email thread, an IRC query, a web chat) is its own session with its own history.

All of Giles' state lives under one data directory, `GILES_HOME` (default `~/.giles`): the config
files, agent directories, the SQLite database, and blobs.

Giles does not run yet. Chapters on installing, configuring, and operating it are added here as
those features are built; until then, [Idea](../IDEA.md) describes the intended behavior.

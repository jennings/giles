# User Guide

This part of the book is for the person running Giles. It covers installing and configuring Giles,
and talking to its agents.

Giles serves one person, the owner. You list your identities on each channel in
[`giles.toml`](configuration.md#owner). Giles drops messages from anyone else.

Each agent has its own:

- files (`SOUL.md`, skills)
- memories
- model
- tools
- scheduled jobs

Every conversation is its own session with its own history. A WhatsApp chat, an email thread, an IRC
query, and a web chat are each a separate conversation.

All of Giles' state lives in one data directory, `~/.giles` by default. It holds the config files,
the agent directories, the database, and blobs.

Start with [Installing and Running](running.md), then [Configuration](configuration.md).

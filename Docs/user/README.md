# User guide

This part of the book is for the person running Doozers. It covers installing and configuring
Doozers, and talking to its agents.

Doozers answers its users and drops messages from anyone else. Each user adds their own accounts on
the channels, their identities, on the [**Identities** page](web.md#identities) of the web chat.

Each agent has its own:

- files (`SOUL.md`, skills)
- memories
- model
- tools
- scheduled jobs

Every conversation is its own session with its own history. A WhatsApp chat, an email thread, an IRC
query, and a web chat are each a separate conversation.

All of Doozers' state lives in one data directory, `~/.doozers` by default. It holds the config
files, the agent directories, the database, and blobs.

Start with [Installing and running](running.md), then [Configuration](configuration.md).

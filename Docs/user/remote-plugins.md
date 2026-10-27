# Running a plugin on another machine

This page shows how to run a channel plugin that ships with Giles (`giles-channel-discord`,
`giles-channel-email`, `giles-channel-irc`, or `giles-channel-whatsapp`) on a different machine from
Giles, and how to point Giles at it. By default Giles launches its plugins itself and doesn't need
any of this.

A plugin you run yourself is a [remote plugin](configuration.md#remote-plugins). Giles neither
starts nor sandboxes it, so it runs with your own privileges.

## Install the plugin

Install the plugin's binary on the machine where it will run. It doesn't need Giles.

## Create a token

Giles proves who it is to the plugin with a bearer token. Make one and keep it in a file only you
can read:

```sh
openssl rand -hex 32 > /etc/giles/discord.token
chmod 600 /etc/giles/discord.token
```

The plugin refuses a token file that group or others can read.

## Write the transport file

The transport file says where the plugin listens and where it keeps its state. It has the same keys
for every plugin:

```toml
listen = "0.0.0.0:7443"
data = "/var/lib/giles-channel-discord"
tls_cert = "/etc/giles/fullchain.pem"
tls_key = "/etc/giles/key.pem"
token_file = "/etc/giles/discord.token"
```

- `listen`: the address to listen on. Required.
- `data`: the plugin's data directory. Required. Giles creates it if it's missing.
- `tls_cert` and `tls_key`: a PEM certificate chain and its private key. Set both or neither.
- `token_file`: the file with the token.

**Use TLS for any plugin that Giles reaches over a network.** The plugin's own settings, including
secrets such as a Discord bot token, stay in `giles.toml` and travel from Giles to the plugin on
every connection. Giles only connects without TLS to an address on its own machine.

Every key is also a flag: `--listen`, `--data`, `--tls-cert`, `--tls-key`, and `--token-file`. A
flag wins over the file. To pass the token in the environment instead of a file, set
`GILES_CHANNEL_<NAME>_TOKEN`, such as `GILES_CHANNEL_DISCORD_TOKEN`. The plugin has no flag for the
token, because a flag shows in the process list.

## Start the plugin

```sh
giles-channel-discord serve --config /etc/giles/discord-serve.toml
```

The plugin prints `listening on <address>` and runs until you stop it with `SIGINT` or `SIGTERM`. It
refuses to start without a token. To accept any caller, for a plugin on a private network that
something else protects, pass `--no-auth`.
**Anyone who can reach the port can then read and send the channel's messages.**

Run `giles-channel-discord --help` for the full list of options.

## Point Giles at it

Add `address` and `auth` to the instance in `giles.toml`. The plugin's settings stay in `config`,
the same as for a plugin Giles launches:

```toml
[channel.discord]
component = "discord"
address = "https://discord.example.com:7443"
auth = { bearer = { secret = "discord-remote" } }

[channel.discord.config]
token = { secret = "discord" }
```

Put the same token in `secrets.toml` as `discord-remote`. See
[Configuration](configuration.md#remote-plugins) for the keys of a remote instance.

The certificate must be valid for the host in `address` and issued by a public certificate
authority.

## Check that it works

Run `giles status`. The instance shows `running` with no process id:

```text
Plugins:
  discord (channel, discord)
    state:     running, since 2026-10-23 09:12:05 PDT
    location:  https://discord.example.com:7443/
    sandbox:   none (remote)
```

If it shows `down, retrying`, the last error says whether the plugin refused the token or the
certificate.

## Change the plugin's settings

The plugin builds its channel from the `config` Giles sends on its first connection, and refuses a
different one until it restarts. After you change the instance's `config` in `giles.toml`, restart
the plugin. Giles then reconnects by itself.

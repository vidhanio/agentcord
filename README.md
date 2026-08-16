# herdcord

a discord bot that mirrors [herdr](https://herdr.dev) agent sessions into
discord forums: each workspace gets a forum channel, each agent session a
post, and the transcript mirrors into the thread.

## features

- forum channel per herdr workspace, created and renamed automatically
- one post per agent session, with harness and status tags
- full transcript mirror: messages, tool calls, user echoes
- typing indicators and blocked notices from herdr's event stream
- resume a dead session by typing into its closed post
- `/agent` slash command: launch an agent from a native modal
- optional `/herdr` control command: one-shot external command, prompt piped
  to stdin
- everything configurable: toml config file, every delay a knob

## config

the bot reads `$XDG_CONFIG_HOME/herdcord/config.toml`
(`~/.config/herdcord/config.toml` by default). override the path with
`--config <path>` or the `HERDCORD_CONFIG` env var.

minimum config:

```toml
[discord]
bot_token = "..."
guild_id = 1234567890
allowed_user_id = 1234567890
```

see [`config.example.toml`](config.example.toml) for the full sample with
every knob and its default. a missing config file makes the bot print a
sample with the error.

## run

```
nix run .#default
```

## license

AGPL-3.0-or-later

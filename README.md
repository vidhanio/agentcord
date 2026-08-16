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

the bot looks for a config at `$XDG_CONFIG_HOME/herdcord/config.toml`
(`~/.config/herdcord/config.toml` by default), overridable via
`--config <path>` or the `HERDCORD_CONFIG` env var.

default config:

```toml
[discord]
bot_token = "..."
guild_id = 1234567890
allowed_user_id = 1234567890
```

see [`config.example.toml`](config.example.toml) for all knobs. String
values support `${NAME}` environment expansion after TOML parsing; values
containing quotes or other TOML syntax are safe. If `NAME` is unset, the
`${NAME}` placeholder remains literal.

## run

```
nix run .#default
```

## Home Manager

The flake exports a Home Manager module without requiring a Home Manager flake
input. Add `inputs.herdcord.homeManagerModules.default` to your Home Manager
configuration and enable the program:

```nix
{ inputs, ... }:
{
  imports = [ inputs.herdcord.homeManagerModules.default ];

  programs.herdcord = {
    enable = true;
    settings = {
      discord = {
        bot_token = "...";
        guild_id = 1234567890;
        allowed_user_id = 1234567890;
      };
    };
  };
}
```

`settings` is rendered as TOML at
`$XDG_CONFIG_HOME/herdcord/config.toml`. The `package` option defaults to this
flake's `packages.<system>.default` and can be overridden when needed.

## license

AGPL-3.0-or-later

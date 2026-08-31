# agentcord

Agentcord is a Discord client for coding agents that implement the
[Agent Client Protocol](https://agentclientprotocol.com). One configured Discord
forum contains every session, one post per ACP session.

Agentcord has no built-in knowledge of agent brands. Commands, arguments,
environment overlays, display names, forum tags, and Unicode or custom Discord
emoji all come from configuration.

## What it does

The current rewrite implements the first conversation slice:

- Messages from the configured user in a persisted session thread are queued
  to a per-thread ACP actor and sent as `session/prompt` requests.
- Agent text updates are reduced into durable Toasty projections and reconciled
  to ordered Discord messages with edits, sends, and deletes.
- Agent thoughts, tool calls, tool results, and plans are projected into the
  same ordered message stream with stable source IDs.
- Prompts originating outside the Discord gateway can be mirrored through a
  forum webhook under the user's current display name and avatar, with a
  bot-authored fallback.
- Imported ACP sessions are restored through `session/load` when their actor
  starts; newly created sessions keep the live connection used by
  `session/new` and accept messages typed in the forum thread immediately.
- The allowed user can create a session through the `/agent` slash command,
  import one exposed by an agent's `session/list` with `/import`, or reload the
  current thread with `/reload`. New sessions show the model default advertised
  by ACP; `/model` changes it for the current session.

ACP permission requests are presented as Discord buttons, with an optional
`approve_all` policy for unattended operation. Elicitation, session controls,
and richer metadata updates are still to be added.

## Configuration

Agentcord loads `$XDG_CONFIG_HOME/agentcord/config.toml` by default. Override it
with `--config` or `AGENTCORD_CONFIG`. `${NAME}` placeholders in string values
are expanded after TOML parsing.

See [config.example.toml](config.example.toml) for the complete schema.

## Home Manager

```nix
{ inputs, ... }:
{
  imports = [ inputs.agentcord.homeManagerModules.default ];
  programs.agentcord = {
    enable = true;
    environmentFile = "/run/secrets/agentcord";
    settings = {
      discord = {
        bot_token = "\${DISCORD_TOKEN}";
        guild_id = 123;
        allowed_user_id = 456;
        forum_channel_id = 789;
      };
      agents.example = {
        display_name = "Example";
        command = "example-acp";
        emoji = "🤖";
      };
    };
  };
}
```

`environmentFile` is loaded by the systemd user service and should contain
`KEY=VALUE` lines. Referencing `\${DISCORD_TOKEN}` from `settings` keeps the
secret out of the Nix store.

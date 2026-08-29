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
- ACP session state is restored lazily through `session/load` when a session
  receives a prompt.
- The allowed user can create a session with `/agent` or import one exposed by
  an agent's `session/list` with `/import`.

Permissions and elicitation are deliberately follow-up slices. Session
controls and richer metadata updates are also still to be added. See
[docs/projection.md](docs/projection.md) for the event and persistence
contract.

## Configuration

Agentcord loads `$XDG_CONFIG_HOME/agentcord/config.toml` by default. Override it
with `--config` or `AGENTCORD_CONFIG`. `${NAME}` placeholders in string values
are expanded after TOML parsing.

See [config.example.toml](config.example.toml) for the complete schema.
Discord snowflake fields accept either TOML integers or quoted decimal strings,
so they can be populated from environment variables.

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
      projects.base_path = "~/Projects";
      agents.example = {
        display_name = "Example";
        command = "example-acp";
        tag = { name = "example"; emoji = "🤖"; };
      };
    };
  };
}
```

`environmentFile` is loaded by the systemd user service and should contain
`KEY=VALUE` lines. Referencing `\${DISCORD_TOKEN}` from `settings` keeps the
secret out of the Nix store.

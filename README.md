# agentcord

Agentcord is a Discord client for coding agents that implement the
[Agent Client Protocol](https://agentclientprotocol.com). One configured Discord
forum contains every session, one post per ACP session.

Agentcord has no built-in knowledge of agent brands. Commands, arguments,
environment overlays, display names, forum tags, and Unicode or custom Discord
emoji all come from configuration.

## What it does

- `/agent` opens a native modal for choosing a configured agent and entering a
  working directory, then creates an ACP session and sends its initial prompt.
- Messages in a session post are delivered as ACP prompts.
- Streamed thoughts are kept visible, while final output is appended and split
  without truncation when it exceeds Discord's message limit.
- Structured ACP tool calls, plans, modes, configuration, usage, titles, and
  permission requests are projected into Discord.
- Restorable sessions reconnect through `session/load` after a restart or when
  the allowed user sends a message to an inactive post.
- Each ACP session owns an isolated, process-group-supervised subprocess.

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
    settings = {
      discord = {
        bot_token = "...";
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

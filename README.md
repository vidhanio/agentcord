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
- `/import` binds an existing ACP session of a configured agent to a new forum
  post; imported posts stay archived until their first message restores them.
- Inside a session post, `/mode`, `/model`, and `/command` change the session
  mode, the model and thinking-level config options, and run the agent's
  advertised slash commands — all with autocomplete fed by the session state.
- Messages in a session post are delivered as ACP prompts.
- Streamed thoughts and tool-call content are kept visible in full; only shell
  output is tail-capped.
- Sessions used outside Discord are pulled in on restore: `session/load`
  replays the conversation and agentcord renders whatever the thread has not
  seen yet, keyed by the agent's replay message ids.
- Structured ACP tool calls, plans, modes, configuration, usage, titles,
  permission requests, and elicitation forms and URL consents are projected
  into Discord.
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

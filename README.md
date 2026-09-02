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
  with autocomplete suggestions from persisted project paths,
  import one exposed by an agent's `session/list` with `/import`, recreate the
  current thread with `/recreate`, or refresh it from a fresh ACP history load
  with `/refresh`. `/session` shows the current thread's first message
  ephemerally. New sessions show the model default advertised by ACP;
  `/model` changes it for the current session.
- Startup reconciliation removes active forum threads that are not managed by
  the configured agents and persisted sessions, and updates active managed
  threads to match their configured agent, title, and starter message.
  Archived threads remain untouched and are updated and loaded when they are
  unarchived. Threads created by anyone other than the bot are removed as they
  appear.

ACP permission requests are presented as Discord buttons, with an optional
`approve_all` policy for unattended operation. Elicitation, session controls,
and richer metadata updates are still to be added.

## Configuration

Agentcord loads `$XDG_CONFIG_HOME/agentcord/config.toml` by default. Override it
with `--config` or `AGENTCORD_CONFIG`. `${NAME}` placeholders in string values
are expanded after TOML parsing.

Set `projects.base_path` to make relative `/agent` project selections resolve
below a common directory and remove that prefix from forum-title project
labels. For example, with `base_path = "~/Projects"`, `agentcord` resolves to
`~/Projects/agentcord` and a project at that path is labeled `agentcord`.

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
        emoji = "🤖";
      };
    };
  };
}
```

`environmentFile` is loaded by the systemd user service and should contain
`KEY=VALUE` lines. Referencing `\${DISCORD_TOKEN}` from `settings` keeps the
secret out of the Nix store.

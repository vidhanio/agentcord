# Control plane as a configurable external command

The original `/herdr` command (added `5ec3776`, removed `2a63b4c`) ran a
one-shot control agent in a throwaway herdr session: the bot spawned a whole
headless herdr server, started an LLM agent in it with the herdr skill, waited
up to 600s for it to settle, and relayed its acknowledgment — then tore the
session down. It was slow (a server spawn + full agent turn per invocation),
fragile (session lifecycle, one-at-a-time mutex, crash cleanup), and put an LLM
between the user and a socket API the bot already speaks natively.

The revisit decides the opposite shape: `/herdr` spawns a **user-configured
external command** (config file, opt-in, no default) with the user's prompt piped
to its stdin, waits up to a configurable timeout, and relays the concatenated
stdout+stderr as an ephemeral reply. No herdr session is spawned — the bot
injects `HERDR_ENV=1` and the resolved socket env instead, so the command's
herdr skill / `herdr` CLI acts on the **main session**, which the forum mirror
already follows through herdr's event stream. The natural default payload is
`pi -p` (print mode, merges piped stdin), giving a fast one-shot agent without
any herdr-server machinery in the bot. The prompt on stdin is prefixed with a
control-plane preamble (mirroring the old `control_prompt`): the session
exists only to perform the request, bootstrap the herdr skill via
`herdr --skill`, fire off the herdr commands and ensure they succeed without
monitoring output, and reply with a single short confirmation.

The recommended configuration is a lean one-shot `pi`:

```toml
[herdr]
control_command = "pi -p --no-session --tools bash --no-skills --no-context-files --no-extensions --no-themes --no-prompt-templates"
```

Shell access only — herdr context arrives via the preamble's `herdr --skill`
read, not through the model's own skill discovery. `-p` (print mode) merges
the piped stdin into the initial prompt and prints the reply to stdout, which
is exactly the one-shot contract; `--no-session` keeps the transient
invocation from persisting a pi session.

Rejected alternatives: typed slash subcommands over `src/herdr/` (locks the
surface to herdr's API — the point is a general escape hatch), and reviving
the throwaway-session control agent (the original's slowness and fragility).
Consequence: the configured command runs as the bot's user with full power —
arbitrary code execution gated only by `allowed_user_id` and the command being
explicitly configured, which is the accepted model for a personal bot.

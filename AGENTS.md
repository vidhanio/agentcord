# Repository Guidelines

## Product

`agentcord` is a Discord client for arbitrary Agent Client Protocol agents. It
uses one configured forum, one post per ACP session, and one supervised ACP
subprocess per active session. There is no terminal scraping, transcript
parsing, agent-name branching, or external workspace manager integration.

`Bot` is the application state and owns the configuration, database, Discord
context, active ACP registry, and session-local locks. Pass a shared `Bot`, not
independent copies of its constituent state.

## Source layout

- `src/lib.rs`: `Bot`, Serenity event handling, startup and wiring.
- `src/acp/`: ACP subprocess/session supervision, prompt queues, restoration,
  protocol lifecycle, and model configuration.
- `src/config.rs`: TOML schema, environment expansion, validation and paths.
- `src/db.rs`: new Agentcord SQLite state and Discord projection bindings.
- `src/discord/`: Discord-specific forum, webhook, permission, rendering, and
  command integrations.
- `src/discord/commands/`: Poise wiring for `/agent`, `/import`, `/model`, and
  `/reload`.

## Development

There is no Rust toolchain on the ordinary PATH. Use the Nix dev shell:

```sh
nix develop -c cargo fmt
nix develop -c cargo clippy --all-targets -- -D warnings
nix develop -c cargo test
nix build .#default
```

New files must be staged before Nix builds because the flake source includes
only tracked files.

Use nightly rustfmt and fix lints rather than adding `allow` attributes. Keep
ACP code generic: no behavior may branch on an agent configuration key. Spawn
configured executables directly with argument vectors, never through a shell.
Do not advertise ACP client filesystem or terminal capabilities unless their
security and Discord semantics are implemented.

Do not add low-value tests for getters, formatting helpers, or behavior made
impossible by the implementation. Tests should cover meaningful protocol,
persistence, concurrency, or recovery risks.

## Commits

Use Conventional Commits with lowercase subjects. Put backticks around code
identifiers, paths, commands, APIs and types in commit messages. Commit a
finished coherent change after formatting, tests and linting pass.

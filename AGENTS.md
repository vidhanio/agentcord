# Repository Guidelines

## Project Overview

`herdcord` is a Discord bot that turns forum channels into a control surface
for [herdr](https://herdr.dev), a terminal workspace manager for AI coding
agents.

- **Each workspace gets its own forum channel.** Workspaces are discovered
  from herdr; their forum channels are created on demand and the
  workspace↔forum mapping persisted in the state database
  (`WorkspaceRow.forum_channel_id`). Rows are keyed by the workspace
  **label** — the stable identity — and also store herdr's positional
  `workspace_id` so a rename can re-key the row to the new label (the
  forum survives renames). The forum channel name is the sanitized
  workspace label and is renamed when the workspace is renamed.
- **Worktrees share their repo's forum.** A worktree workspace mirrors its
  repo's main workspace: `worktree.list` (scoped by workspace id) reports
  the repo's main workspace (`source_workspace_id`) and each worktree's
  checked-out branch, so `ensure_workspace_forum` resolves a worktree to
  the main workspace's row and forum (`forum_workspace`; the main
  workspace is the one whose row the forum is keyed to). Worktree
  workspaces never get their own row or forum while the main is open.
  The starter message gains a `worktree `branch`` field right after the
  pane when the agent runs in a worktree. If the main workspace is closed,
  the worktree falls back to its own label and forum.
- **Each forum post is one agent session.** Every agent launch is a session
  identified by its transcript path (`agent_session.value`, e.g.
  `~/.omp/agent/sessions/…`). `SessionRow` in the state database keys posts
  by session path. Post titles follow the chain: the **transcript's own
  title record** (`title`/`title_change` for omp; `custom-title` >
  `ai-title` > `summary` for claude-code; none for codex) — stable, unlike
  the animated terminal title — else herdr's **stripped terminal title**
  (`terminal_title_stripped`; herdr has already removed ANSI escapes and
  the leading activity glyph, so no local stripping exists). The agent
  name is never used for titles. Thread renames are skipped when the title
  is unchanged: renaming posts a channel-name-change system message into
  the thread, so identical renames would spam it. The post's **starter
  message** is a one-line plain-text intro —
  `` `pane` · worktree `…` · cwd `…` · session `…` `` (kind/status are
  already on the tags; the worktree segment only appears when the agent
  runs in a git worktree) — rewritten to `inactive · cwd …` when the
  session dies.
- **Transcripts can rotate under the session.** When a session is replaced
  in the same pane, omp starts a new transcript file and herdr may keep
  reporting the old path. The session row's `transcript_path` (initially the
  session path) is the file actually synced; the poll re-binds it to a
  newer unclaimed file in the same directory when the bound one goes quiet
  (`SESSION_STALE_GRACE`), so the post survives rotations and the cursor
  restarts on the new file. Live-agent matching (relay, reconcile, post
  recovery) uses `SessionRow::hosts`, which accepts the row key or the
  adopted transcript.
- **Two sources of truth, nothing cached.** herdr is the truth for live
  state: workspace labels, agent kinds, statuses, and titles are queried
  fresh on every event and mirrored onto Discord posts. Discord is the
  mirror — tags and post titles live there. The database holds
  only what neither side knows: the workspace↔forum and session↔post
  bindings plus the transcript sync cursors (the one Discord-dependent
  state, because rebuilding the mirror is expensive). In-memory state is
  three small `Mutex<HashMap>`/`Mutex<HashSet>`s on `Forum`: the
  pane→session map (marks a post dead the instant its pane closes, and is
  the poll's live-session set), the resuming set (a message in a dead
  thread must not launch two agents), and the tool-embed bookkeeping
  (posted message id + shown state per tool call). The first two are
  pruned when their session dies; the tool map is pruned when a session
  dies.
- **Dead threads stay open and keep only their kind tag.** A dead
  session's post is not locked: the status tag is dropped, the agent-kind
  tag stays, and the starter message's pane part flips to `inactive`.
  Users can keep typing in the thread — a message re-launches the agent
  **resuming the same conversation** (native harness resume:
  `omp --resume=<path>`, `claude --resume <id>`, `codex resume <id>`; the
  session row's key is herdr's reported session reference, which is
  exactly what each harness resumes) and relays the message to it. The
  post, its row, and the sync cursor are untouched by the resume — the
  transcript continues where it stopped. If the workspace is gone, the
  resume re-creates it.
- **Posts launch agents; every other manual post is deleted.** The host
  user's (`ALLOWED_USER_ID`; everyone when unset) new post in a managed
  forum launches an agent in that workspace: the kind comes from the
  post's applied tags (`omp`/`claude-code`/`codex`; no kind tag → the
  default; several kind tags or a tag the bot does not manage → DM the
  author and delete without launching), the prompt is the thread title
  plus the starter message body, and the host's post is deleted once the
  launch settles (with a DM when the launch fails). Any other manual post
  is deleted silently. The bot's own posts are recognized by the starter
  message's author (fresh posts aren't session-bound yet when the
  `thread_create` event arrives) and by the session binding.
- **Forum tags describe the session**: the agent kind (`omp`, `claude-code`,
  `codex`, … 🤖) and the lifecycle status (`idle`/`working`/`blocked`/`done`/
  `unknown`). The bot owns a forum's tags outright: every tag write replaces
  the list with the managed set (the 5 statuses + one tag per agent kind),
  so tags this bot does not manage are dropped. Stateless — the forum's tag
  list is fetched fresh on each write. A post's kind tag doubles as the
  launch selector for new posts and as the resume kind for dead threads;
  on death the applied tags are pruned to just the kind tag.
- **Deleted forums and posts are re-created.** A workspace always gets its
  forum channel (`ensure_workspace_forum` re-creates a deleted one and
  re-binds the mapping) and a live agent always gets its post
  (`ensure_session_post` re-creates a deleted post in the workspace's
  forum, re-binding the session row — key and adopted transcript
  preserved). The 2s poll detects the breakage and escalates to the full
  live-agent sync, so recovery happens without waiting for an event.
- **Conversations come from session files.** The bot reads each agent's
  transcript file directly and normalizes it with per-harness parsers
  (`src/session/`: `omp.rs`, `claude.rs`, `codex.rs` JSONL formats sharing
  one completion pre-scan skeleton in `common.rs`), instead of scraping
  terminal output. User/assistant turns are posted as plain messages;
  **tool calls** are parsed out of each harness's tool records and posted
  as embeds — one per call, posted once and **edited in place** when the
  call completes (the colour carries running/done/failed; the `tool_messages`
  map tracks posted embeds). Posted-message bookkeeping lives on the
  session row (`synced_messages`, `last_discord_message_id`,
  `transcript_path`) — the only Discord-dependent state, because rebuilding
  the mirror is expensive. A backlog beyond `CATCHUP_BACKLOG` (50) messages
  is truncated to the last `MAX_SYNC_MESSAGES` (5), announced in small
  italic text; normal turns are mirrored whole. The cursor commits after
  every post, so a mid-sync failure (e.g. a Discord rate limit) resumes
  from the last posted message instead of re-posting it. User turns that
  never appeared in Discord (typed in herdr) are echoed through a
  per-forum **webhook** named `"{nickname} (via herdr)"` and avatared
  after the allowed user, so they look like the user's own messages.
- **The bot controls herdr over its Unix-socket API** (newline-delimited
  JSON): every call goes through `src/herdr/`, which dials the socket
  (resolved in `src/config.rs`: `HERDR_SOCKET_PATH`, else
  `sessions/<name>/herdr.sock` for `HERDR_SESSION`, else
  `$XDG_CONFIG_HOME/herdr/herdr.sock`) with a fresh connection per request
  and parses the JSON envelope protocol. A long-lived `events.subscribe`
  connection drives event-based state detection; **input** (prompts) and
  **state** (status changes) still flow through the socket — only the
  conversation log is read from files.
- State lives in a SQLite database via [toasty] at
  `$XDG_STATE_HOME/herdcord`, opened with the schema pushed from the
  registered models on startup (skipped when the tables already exist).

[toasty]: https://docs.rs/toasty

## Architecture & Data Flow

```
Discord ──► EventHandler (src/lib.rs)
              │  message in a live session's forum post ──► Relay.submit (src/relay.rs)
              │                              │ per-agent FIFO worker (mpsc, 1 at a time;
              │                              │ job carries session_path + post channel)
              │                              ▼
              │                    herdr agent prompt (wait until idle/done/blocked)
              │                              │ still working (timeout/stall error) ──► silent wait loop
              │                              ▼
              │                    read_session(session file) ──► messages since last sync
              │                              │ (delta tracked on the SessionRow)
              │                              ▼
              │                    chunked messages + tool embeds + tag update
              │
              │  message in a dead session's post ──► Forum::resume_session
              │                              (native harness resume in the same
              │                              workspace → relay the message)
              │
              │  new post in a managed forum ──► Forum::handle_thread_create
              │     host post: launch agent (kind from tags, prompt from
              │     title+body) then delete the post ──► relay the prompt
              │     other posts: deleted silently
              │
              ├─ poll task (2s tick, src/forum/poll.rs)
              │    syncs every live session's transcript (cursor no-ops)
              │    + probes for transcript rotations (staleness + adoption)
              │
              └─ event loop (long-lived events.subscribe stream, src/forum/events.rs)
                   pane.agent_status_changed (per agent pane) ──► typing + instant tags/title
                   pane.agent_detected / pane.closed / pane.exited / workspace.updated / workspace.renamed
                   │ re-subscribe on stream drop or new agent
                   └─ periodic reconcile (SYNC_INTERVAL 600s) as drift backstop
                        + prunes dead panes/sessions from the in-memory maps
```

- `src/herdr/` — typed async client over herdr's Unix socket (NDJSON, one
  request per connection: the server answers once, then FINs the stream,
  so every call dials afresh; only `events.subscribe` is long-lived).
  `mod.rs` holds the client, `Agent`/`Workspace` models, the nutype ids
  (`WorkspaceId`/`PaneId`/`TabId`/`SessionPath`), `AgentStatus`, and
  `Error` (`is_timeout`/`is_stalled`); `event.rs` holds `EventKind`/
  `Subscription`/`Event`/`EventStream` (broadcast, drop-oldest) and the
  wire `EventLine`; `wire.rs` holds the response envelope + per-method
  result payloads, each pinned by a fixture test
  (`tests/fixtures/api/*.json` via `include_str!`).
- `src/session/` — normalized transcripts: `mod.rs` defines `AgentKind`,
  `SessionRole`, `ToolState`, `ToolCall`, `SessionMessage`, `read_session`,
  and `read_session_title` (transcript-sourced titles per harness);
  `common.rs` is the shared parsing skeleton (text extraction, caps,
  completion pre-scan, tool-message builder); `omp.rs`/`claude.rs`/
  `codex.rs` are the per-harness parsers. Malformed/empty lines are
  skipped; truncated final lines are tolerated. Line timestamps are not
  parsed (nothing consumes them).
- `src/forum/` — forum↔herdr reconciliation. `mod.rs` holds the `Forum`
  struct and the workspace-forum/session-post lifecycle (ensure/rename
  forums, ensure posts, tag application, manual-post handling — host
  posts become `LaunchSpec`s, everything else is deleted, dead-thread
  tag pruning, session resume, spawn helpers); `sync.rs` the transcript
  mirror (cursor sync, tool embeds, user echoes, starter-message
  refresh); `events.rs` the `events.subscribe` loop, `reconcile`, and
  pane lifecycle handling (`workspace.closed` inactivates the workspace's
  sessions instantly — herdr emits no per-pane events for it); `poll.rs`
  the 2s transcript poll + rotation adoption; `titles.rs` the post-title
  selection, the `post_prompt` assembly, and the one-line starter message
  (`` `pane` · cwd `…` · session `…` ``, the pane part reading `inactive`
  once the agent is gone). Agents are spawned with an explicitly declared
  workspace + cwd: launch-from-post resolves the cwd from a live agent in
  the workspace, else a previous session row, else the home directory.
- `src/relay.rs` — per-agent conversation workers (keyed by pane id —
  agents are unnamed; the `RelayJob` carries the session path): one `mpsc`
  channel per agent (shared `Arc<Mutex<HashMap>>` of senders with
  same-channel-guarded removal, 600s idle timeout). `process_job` = prompt
  (waits until idle/done/blocked; a `timeout`/`stalled` error → still-
  working wait loop) → sync the session file → post the new messages since
  the last sync.
- `src/lib.rs` (Bot + EventHandler) — the launch-from-post orchestration
  (`launch_from_post`: spawn → bind session → relay the prompt → delete
  the host's post, DM on failure) and the dead-thread resume path
  (`resume_session_and_relay`).

## Key Directories

| Path | Purpose |
|---|---|
| `src/` | Bot core: `lib.rs` (Bot + EventHandler), `config.rs`, `error.rs`, `db.rs`, `relay.rs`, `utils.rs` |
| `src/forum/` | Forum-side state: `mod.rs` (struct + lifecycle), `sync.rs` (transcript mirror), `events.rs` (event loop + reconcile), `poll.rs` (2s transcript poll + rotations), `titles.rs` (titles + starter message) |
| `src/herdr/` | herdr Unix-socket client: `mod.rs` (client + models + errors), `event.rs` (subscription machinery), `wire.rs` (envelope + result payloads) |
| `src/session/` | Transcript normalization: `mod.rs` (models + read_session), `common.rs` (shared parsing skeleton), `omp.rs`/`claude.rs`/`codex.rs` (per-harness parsers) |
| `tests/` | `herdr_live.rs` (live integration, gated) and `fixtures/api/` (captured herdr API JSON, embedded via `include_str!`) |
| `.github/workflows/` | CI — a single `nix flake check` job (clippy/doc/fmt/deny/nextest) |

## Development Commands

There is **no rustup/rustc on PATH** — everything goes through nix. The
toolchain is nightly (required for `rustfmt.toml` unstable options).

```sh
# fmt (nightly rustfmt), lint, unit tests
nix shell --inputs-from . 'rust-overlay#rust-nightly' 'nixpkgs#gcc' -c cargo fmt
nix shell --inputs-from . 'rust-overlay#rust-nightly' 'nixpkgs#gcc' -c cargo clippy --all-targets -- -D warnings
nix shell --inputs-from . 'rust-overlay#rust-nightly' 'nixpkgs#gcc' -c cargo test

# live integration test (spawns real herdr agents; cleans up after itself)
HERDR_LIVE_TESTS=1 nix shell --inputs-from . 'rust-overlay#rust-nightly' 'nixpkgs#gcc' -c cargo test --test herdr_live

# build/run the binary
nix build .#default && ./result/bin/herdcord
nix run .#default

# license/advisory check
nix shell --inputs-from . 'nixpkgs#cargo-deny' 'nixpkgs#cargo' -c cargo deny check
```

- `nix develop` works but **builds every flake check first** (slow) — prefer
  the `nix shell` one-liners for iteration.
- **CRITICAL:** the flake source is `src = ./.` which only includes
  **git-tracked** files. Any new file must be `git add`-ed before
  `nix build .#default`/`nix flake check`, or the sandbox build silently won't
  see it.
- Run the bot with env config (see `src/config.rs`): `DISCORD_BOT_TOKEN` and
  `GUILD_ID` required; `ALLOWED_USER_ID` optional (when set, only that
  Discord user may talk to agents and launch them via forum posts);
  `RUST_LOG` default `warn,herdcord=trace`. Everything else (timeouts,
  agent kind, sync interval, state dir, socket path) is a sane default
  const in `src/config.rs`.

## Code Conventions & Common Patterns

- **Formatting**: `rustfmt.toml` uses nightly-only options (`unstable_features`,
  `group_imports = "StdExternalCrate"`, `imports_granularity = "Crate"`,
  `wrap_comments`, `reorder_impl_items`). Imports: std → external → `crate::`,
  one `use` per crate. Always run nightly `cargo fmt`.
- **Lints** (`Cargo.toml [lints]`): `unsafe_code = "forbid"`,
  `missing_copy_implementations`/`missing_debug_implementations` warn,
  clippy `pedantic` + `nursery` warn (allows: `cast_possible_wrap`,
  `cast_sign_loss`, `missing_errors_doc`, `module_name_repetitions`). CI denies
  warnings — **do not add `#[allow]` attributes**; fix the lint or split the
  function (watch `too_many_lines`/`too_many_arguments`).
- **No stringly-typed APIs where a closed enum fits**: lifecycle statuses are
  `AgentStatus` (`src/herdr/mod.rs`) with `as_str()` only at the wire
  boundary; agent harnesses are the closed `AgentKind` enum
  (`src/session/mod.rs`) with `as_str()` only for session paths/kind tags;
  herdr error actions use a private `HerdrAction` enum.
- **Errors**: `BotError` (`src/error.rs`) with thiserror; `serenity::Error` is
  boxed (`Box<serenity::Error>`) to keep the variant small (clippy
  `result_large_err`). `?` works via `#[from]` adapter variants
  (including `toasty::Error`).
- **Async**: tokio multi-thread; long herdr calls run inside
  `tokio::time::timeout`; per-agent relay workers are `tokio::spawn`ed
  tasks owning `mpsc` receivers. State is shared via `Arc` (`Arc<Config>`,
  `Arc<Forum>`, `Arc<Relay>`); the only mutable in-memory state is the
  three small collections on `Forum` (`sessions_by_pane`, `resuming`,
  `tool_messages`), and `sync_lock` (an `Arc<tokio::Mutex<()>>`) serializes
  transcript syncs so the poll and the event loop can never double-post
  from the same cursor — syncs also re-read the row under the lock,
  because a caller's copy may predate another sync's commits. Beware:
  `serenity::cache::CacheRef` is `!Send` — clone out of the cache before
  any `.await`.
- **Sessions**: a session is identified by herdr's reported session
  reference (`agent_session.value`): a transcript path for omp, a session
  id for claude-code and codex — unique per launch, and exactly what the
  harness resumes with. Session transcripts are read with
  `read_session(AgentKind::parse(&kind), Path::new(&session.transcript_path))`
  — never scraped from terminal output.
- **herdr socket discipline**: dial a fresh connection per request — the
  server answers the first request and FINs the stream, so a second request
  on the same connection is never answered; `events.subscribe` is the one
  long-lived exception. The request id is a constant (unique per
  connection, and a connection is one request). Use explicit targets
  (pane id or workspace id). The bot runs outside herdr (`HERDR_ENV` unset)
  and connects to the user-owned socket in the herdr config dir.
- **Titles**: session post titles are the transcript's own title when the
  harness records one, else herdr's stripped terminal title. The raw
  `terminal_title` is never used — herdr strips ANSI and the leading
  activity glyph itself.

## Important Files

- `src/main.rs` — entry: color_eyre, dotenvy, `Config::from_env` (envy),
  tracing init, `herdcord::run(config)`.
- `src/lib.rs` — `Bot` struct (config, herdr client, forum, relay, state
  `Db`), `run()` (ratelimiter-disabled `Http` backed by a 30s-timeout
  reqwest client — serenity's ratelimiter has wedged in production and has
  no per-request timeout), `EventHandler` (message relay + dead-thread
  resume, new-post handling + launch-from-post), the poll task + event
  loop spawn.
- `src/herdr/mod.rs` — the herdr Unix-socket client (NDJSON, one request
  per connection); nutype newtypes `WorkspaceId`/`PaneId`/`TabId`/
  `SessionPath`; the JSON envelope protocol; the public methods (new,
  list_workspaces, create_workspace_with_pane, close_workspace, create_tab,
  close_tab, list_agents, get_agent, start_agent, prompt_agent, wait_agent,
  session_snapshot → `Vec<Agent>`, subscribe) plus `AgentStatus` and
  `Error` with `is_timeout`/`is_stalled`. Fixture tests pin the wire
  format.
- `src/db.rs` — toasty SQLite state: `WorkspaceRow`/`SessionRow` models and
  the `Db` wrapper (workspace/session lookups and upserts; schema pushed
  when the tables are missing, legacy column renames applied when they
  exist, workspace/session rows re-keyed from positional ids to labels on
  the first reconcile).
- `src/forum/mod.rs` — per-workspace forum channels, session posts, tags,
  manual-post handling (host posts → `LaunchSpec`, everything else
  deleted), dead-thread tag pruning, session resume, spawn helpers;
  submodules for sync, events, poll, and titles (see Key Directories).
- `src/relay.rs` — conversation workers and the session-file sync delta.
- `src/config.rs` — minimal env config: `DISCORD_BOT_TOKEN`, `GUILD_ID`,
  `ALLOWED_USER_ID`. Sane defaults as consts: `DEFAULT_AGENT_KIND`
  (`AgentKind::Omp`), `PROMPT_TIMEOUT` (300s), `OPERATION_TIMEOUT` (30s),
  `SYNC_INTERVAL` (600s), `MESSAGE_POLL_INTERVAL` (2s),
  `MAX_SYNC_MESSAGES` (5), `CATCHUP_BACKLOG` (50). `socket_path()` honors
  `HERDR_SOCKET_PATH`/`HERDR_SESSION`; the state db lives under
  `$XDG_STATE_HOME/herdcord`.
- `flake.nix` — crane + rust-overlay nightly; `packages.default`; checks
  clippy/doc/fmt/deny/nextest; treefmt (nixfmt, statix, deadnix, rustfmt,
  taplo).
- `deny.toml` — license allowlist + 4 ignored rustls-webpki 0.102 advisories
  (no patched 0.102.x exists; fix arrives with a serenity/rustls bump — see
  the comment in the file).

## Runtime/Tooling Preferences

- **Rust**: nightly only (rustfmt.toml unstable options; doc builds under
  `-D warnings`). Edition 2024.
- **Nix**: flake-based (`flake-parts` + `crane` + `rust-overlay` + `treefmt-nix`).
  No rustup. `gcc` is needed in the shell (ring build script).
- **Discord**: serenity 0.12 (rustls backend). Required intents: GUILDS,
  GUILD_MESSAGES, MESSAGE_CONTENT.
- **herdr**: control over the Unix socket in the herdr config dir
  (`$XDG_CONFIG_HOME/herdr/herdr.sock`; `HERDR_SOCKET_PATH`/`HERDR_SESSION`
  to override).
- **Dependencies**: prefer popular libraries over vendored logic (serenity's
  built-in `Typing`, `ExecuteWebhook`, the `time` crate, etc.). Current
  deps: serenity, toasty, nutype, thiserror, envy, dotenvy, dirs, tokio,
  tracing, reqwest, serde/serde_json, color-eyre (main only). No anyhow in
  the lib.

## Testing & QA

- **Unit tests**: pure tests in `src/` — `src/herdr/` parses the captured
  fixture envelopes (`tests/fixtures/api/*.json` via `include_str!`; the
  fixtures are the contract for herdr's wire format — regenerate them
  from a live server, not by hand), `src/session/` tests the transcript
  parsers (`parse_omp`/`parse_claude_code`/`parse_codex`,
  malformed/truncated lines, titles) and `read_session`, `src/db.rs` tests
  workspace/session upserts and lookups on an in-memory database,
  `src/forum/` tests `sanitize_agent_name`, the title selection, the
  post-prompt assembly, and the per-kind resume args, `src/config.rs`
  tests state-dir resolution.
- **Live tests** (`tests/herdr_live.rs`): gated behind `HERDR_LIVE_TESTS=1`
  (no-ops otherwise, so plain `cargo test`/nextest needs no herdr). Spawns
  real agents in `herdcord-live-{pid}`/`herdcord-events-{pid}`/
  `herdcord-session-{pid}` workspaces — a roundtrip (create → start →
  prompt → close), an event-stream test asserting
  `pane.agent_status_changed` events reach the bot, and a session-file test
  asserting the agent's transcript records the conversation and parses via
  `read_session`; self-clean via a synchronous `std::process::Command` Drop
  guard (async cleanup dies with the tokio runtime) plus a startup sweep of
  leaked workspaces. Never run in CI.
- **QA gates**: clippy `--all-targets -- -D warnings` (pedantic+nursery),
  nightly `cargo fmt --check`, `cargo deny check`, `nix flake check`
  (clippy, doc, fmt, deny, nextest). CI runs `nix flake check` only.
- **Coverage expectations**: no coverage tooling; correctness is enforced by
  the fixture tests, the session parser + db tests, the live tests, and
  clippy's pedantic set. New herdr wire shapes should get a fixture +
  parsing test; new session formats should get a parser test.

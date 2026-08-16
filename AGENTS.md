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
  `ai-title` > `summary` for claude-code; `session_info` `name` for pi;
  the store's `session.title` for opencode; none for codex) — stable,
  unlike the animated terminal title — else herdr's **stripped terminal
  title** (`terminal_title_stripped`; herdr strips ANSI escapes and the
  leading activity glyph). The agent name is never used for titles. Thread
  renames are skipped when the title is unchanged: renaming posts a
  channel-name-change system message into the thread, so identical renames
  would spam it. The post's **starter message** is a one-line plain-text
  intro — `` `pane` · worktree `…` · cwd `…` · session `…` `` (harness/
  status are already on the tags; the worktree segment only appears when
  the agent runs in a git worktree) — rewritten to `inactive · cwd …` and
  the post closed when the session dies, and refreshed to the new agent's
  pane on a resume.
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
  state: workspace labels, harnesses, statuses, and titles are queried
  fresh on every event and mirrored onto Discord posts. Discord is the
  mirror — tags and post titles live there. The database holds
  only what neither side knows: the workspace↔forum and session↔post
  bindings plus the transcript sync cursors (the one Discord-dependent
  state, because rebuilding the mirror is expensive). In-memory state is
  four small `Mutex<HashMap>`/`Mutex<HashSet>`s on `Forum`: the
  pane→session map (marks a post dead the instant its pane closes, and is
  the poll's live-session set), the resuming set (a message in a dead
  thread must not launch two agents), the tool-embed bookkeeping
  (posted message id + shown state per tool call), and the poll's
  transcript stamps (mtime+size per live session, so an unchanged file
  costs one stat instead of a full mirror pass). The first two are
  pruned when their session dies; the last two are pruned when a session
  dies.
- **Dead posts are closed and keep only their harness tag.** A dead
  session's post is closed (archived, never locked): the status tag is
  dropped, the harness tag stays, and the starter message's pane part
  flips to `inactive`. Users can keep typing in the thread — Discord
  auto-unarchives it on the next message, which re-launches the agent
  **resuming the same conversation** (native harness resume:
  `omp --resume=<path>`, `claude --resume <id>`, `codex resume <id>`,
  `pi --session <path-or-id>`, `opencode --session <id>`; the
  session row's key is herdr's reported session reference, which is
  exactly what each harness resumes) and relays the message to it. The
  post, its row, and the sync cursor are untouched by the resume — the
  transcript continues where it stopped. If the workspace is gone, the
  resume re-creates it.
- **Post archive state mirrors herdr, one-way.** A live agent's sync
  reopens (unarchives) its post — a live agent's post is always open —
  and the death flow closes it. Discord-side archive changes do not
  touch herdr: closing or reopening a post never deactivates or resumes
  its agent. Resuming a dead session happens only through a message in
  the thread.
- **Agents launch through slash commands; manual forum posts are deleted.**
  `/agent` (global guild command) opens a Discord native modal — an
  harness dropdown (preselected to the configured default harness), a
  workspace dropdown (live herdr workspaces; the workspace of the forum
  the command ran in is preselected), and a multiline prompt input — and
  launches the agent with the same spawn/bind/relay flow the forum
  launch used: spawn in the workspace, bind the session to a forum post,
  relay the prompt, reply ephemerally with the thread link. Any manual
  post in a managed forum is deleted silently, and the bot's own posts
  get their transcripts mirrored by the 2s poll.
- **`/herdr` is a configurable escape hatch.** When `HERDR_CONTROL_COMMAND`
  is set, `/herdr` spawns that one-shot external command (e.g. a lean
  `pi -p`) with the user's prompt piped to its stdin — prefixed with a
  control-plane preamble telling it to bootstrap the herdr skill via
  `herdr --skill`, act on the main session, and reply with a short
  confirmation — and relays the concatenated stdout+stderr back as an
  ephemeral reply, truncated to Discord's 2000-char cap. The subprocess
  gets `HERDR_ENV=1` and the bot's resolved socket injected, so it acts
  on the main herdr session (the forum mirror follows via herdr's event
  stream — no second write path). When unset, `/herdr` is not registered
  at all. Runs in parallel — one process per invocation, killed as a
  process group on the configured timeout.
- **Forum tags describe the session**: the harness (`omp`, `claude-code`,
  `codex`, … 🤖) and the lifecycle status (`idle`/`working`/`blocked`/`done`/
  `unknown`). The bot owns a forum's tags outright: every tag write replaces
  the list with the managed set (the 5 statuses + one tag per harness),
  so tags this bot does not manage are dropped. Stateless — the forum's tag
  list is fetched fresh on each write. A dead thread's harness tag is the
  resume harness; on death the applied tags are pruned to just the harness tag.
- **Deleted forums, posts, and messages are repaired on Discord events.** A
  workspace always gets its forum (re-created on Discord's `channel.delete`), a live
  agent always gets its post (re-created on `thread.delete` via the recovery pass:
  ensure + tags + mirror), and a tool-embed message the user deleted is re-posted on
  the next completion (an edit-404 drops the stale bookkeeping instead of stalling
  the mirror). A dead session's deleted post, and a deleted forum whose workspace is
  gone from herdr, are pruned on the same events. Every Discord-side repair is
  event-driven; the poll's recovery escalation and the reconcile's existence probes
  remain only as backstops for events missed during a disconnect.
- **One writer per concern.** The transcript mirror (posting agent turns,
  tool embeds, user echoes) runs only from the 2s poll and the relay's
  settle (the immediate reply path); events and the reconcile do post
  metadata only — tags, title, unarchive, typing — never reading the
  transcript. `pane.agent_detected` only triggers the re-subscribe; the
  post-reconnect reconcile applies the new agent's metadata and the poll
  mirrors its transcript. The starter message refreshes only on post
  creation, session death, and resume. The poll's deleted-post recovery
  is the one rare full pass (ensure + metadata + mirror). The `sync_lock`
  serializes the mirror passes (poll vs settle) and the ensure passes.
- **Conversations come from session files.** The bot reads each agent's
  transcript file directly and normalizes it with per-harness parsers
  (`src/session/`: `omp.rs`, `claude.rs`, `codex.rs`, `pi.rs` JSONL formats
  sharing one completion pre-scan skeleton in `common.rs`), instead of
  scraping terminal output. `opencode.rs` is the exception: opencode
  persists sessions in a SQLite store
  (`$XDG_DATA_HOME/opencode/opencode-<channel>.db`), so its transcript is
  read from the store by session id (`read_session_messages`). Pi inlines
  an invoked skill's whole `SKILL.md` into the user turn; the pi parser
  condenses those `<skill …>…</skill>` blocks to a `/skill:name` marker so
  the mirror shows the invocation, not hundreds of lines of context (an
  overlong echo would exceed Discord's message limit). User/assistant turns are posted as plain messages;
  **tool calls** are parsed out of each harness's tool records — one per
  call, posted once and **edited in place** when the call completes (the
  colour carries running/done/failed; the `tool_messages` map tracks the
  posted message). Single-argument calls post as plain text —
  `⚙️ **name** \`value\`` while running, gear off once resolved (the
  gear becomes ❌ and the error is appended as a code block underneath
  when the call fails), the single field's value with no field name
  (newlines or values longer than 100 chars switch to a code block) —
  multi-argument calls keep the field-per-argument embed, and a failed
  call's error is an `error` embed field on it. omp records some calls
  (`hub`, `task`, …) without
  arguments; the parser falls back to the record's `intent` as the single
  argument — and omp truncates the args it records in
  `tool_execution_start` (≈230 chars), so a truncated record falls back
  to the full arguments from the assistant's `toolCall` message record,
  matched by tool name in order. Posted-message bookkeeping lives on the
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
  `$XDG_STATE_HOME/herdcord`, with the schema pushed from the registered
  models on startup (a no-op when the tables already exist).

[toasty]: https://docs.rs/toasty

## Architecture & Data Flow

```
Discord ──► Bot event handler (src/lib.rs, serenity EventHandler::dispatch)
              │  message in a live session's forum post ──► Relay.submit (src/relay.rs)
              │                              │ per-agent FIFO worker (mpsc, 1 at a time;
              │                              │ job carries session_path + post channel)
              │                              ▼
              │                    herdr agent prompt, delivered immediately (no wait)
              │                              │ turn settlement tracked in a detached
              │                              │ task: wait until idle/done/blocked
              │                              │ ──► mirror session file + blocked notice
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
              │     bot's own posts: left alone (the poll mirrors them)
              │     manual posts: deleted silently (agents launch via /agent)
              │  deleted post ──► Forum::handle_thread_delete
              │     live session: re-created (recovery pass); dead: row pruned
              │  deleted channel (forum) ──► Forum::handle_channel_delete
              │     forum + live posts re-created; row pruned if workspace gone
              │
Discord ──► poise framework (src/commands/, serenity Framework)
              │  /agent ──► native modal (harness dropdown w/ default harness,
              │             workspace dropdown, prompt input)
              │     submit ──► launch_from_modal: spawn → bind session post →
              │                relay the prompt → ephemeral thread link
              │  /herdr (when HERDR_CONTROL_COMMAND is set)
              │     ──► control_prompt + stdin pipe ──► one-shot external
              │         command (process group, timeout) ──► truncated
              │         ephemeral reply (HERDR_ENV=1 + socket injected)
              │
              ├─ poll task (2s tick, src/forum/poll.rs)
              │    stats every live session's transcript: mirrors the
              │    changed ones (unchanged = one stat, skipped entirely)
              │    + probes for transcript rotations (staleness + adoption)
              │
              └─ event loop (long-lived events.subscribe stream, src/forum/events.rs)
                   pane.agent_status_changed (per agent pane) ──► typing + instant tags/title
                   pane.agent_detected ──► re-subscribe (reconcile applies metadata)
                   pane.closed / pane.exited / workspace.closed ──► post inactivated
                   workspace.created ──► forum created right away
                   workspace.updated / workspace.renamed
                   │ re-subscribe on stream drop or new agent
                   └─ periodic reconcile (SYNC_INTERVAL 600s) as metadata drift
                        backstop + prunes dead panes/sessions from the in-memory maps
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
- `src/session/` — normalized transcripts: `mod.rs` defines `Harness` and
  the `read_session`/`read_session_messages` dispatch; `model.rs` the
  conversation model (`SessionRole`, `ToolState`, `ToolCall`,
  `SessionMessage`); `title.rs` `read_session_title`
  (transcript-sourced titles per harness);
  `common.rs` the shared parsing skeleton (text extraction, caps,
  completion pre-scan, tool-message builder); `omp.rs`/`claude.rs`/
  `codex.rs`/`pi.rs` the per-harness file parsers (each with its parser
  tests) and `opencode.rs` reads the opencode SQLite store. Malformed/empty lines are
  skipped; truncated final lines are tolerated. Line timestamps are not
  parsed (nothing consumes them).
- `src/forum/` — forum↔herdr reconciliation, split by feature: `mod.rs`
  holds the `Forum` struct and its shared state; `workspace.rs` the
  workspace-forum lifecycle (ensure/rename forums, worktree resolution,
  stale-row pruning); `post.rs` the session-post binding lifecycle
  (ensure/create posts, tag-application support, manual-post deletion);
  `tags.rs` the status/harness tag application; `spawn.rs`/`resume.rs`
  the spawn and dead-session resume helpers; `sync.rs`
  the transcript mirror (cursor sync, tool embeds, starter-message
  refresh) with user echoes in `echo.rs` and tool-call/message rendering
  in `render.rs`; `events.rs` the `events.subscribe` loop, `reconcile`, and
  pane lifecycle handling (`workspace.closed` inactivates the workspace's
  sessions instantly — herdr emits no per-pane events for it;
  `workspace.created` gets its forum created immediately via the same
  sync path as updated/renamed); `poll.rs`
  the 2s transcript poll + rotation adoption; `titles.rs` the post-title
  selection and the one-line starter message (`` `pane` · cwd `…` ·
  session `…` ``, the pane part reading `inactive` once the agent is
  gone). Agents are spawned with an explicitly declared workspace + cwd:
  `/agent` resolves the cwd from a live agent in the workspace, else a
  previous session row, else the home directory.
- `src/commands/` — the poise framework (registered as serenity's
  `Framework`, guild-only commands): `/agent` (`agent.rs`) builds a native modal (kind
  dropdown defaulted to `DEFAULT_HARNESS`, workspace dropdown with the
  invocation forum's workspace preselected, prompt input) by hand — poise's
  derive only knows text inputs — sends it as the command's initial
  response, awaits the submit through serenity's modal collector, defers
  the submit, and launches via the forum's spawn/bind/relay helpers,
  editing the deferred response with the thread link. `/herdr` (`herdr.rs`) runs the configured `HERDR_CONTROL_COMMAND`
  (`build_commands` registers it only when configured) — the prompt
  (preamble-prefixed via `control::control_prompt`) is piped to the
  command's stdin, `HERDR_ENV=1` + the bot's resolved socket are
  injected, and the concatenated output is truncated and edited into the
  deferred ephemeral response. The `allowed` check gates every command
  on `ALLOWED_USER_ID`.
- `src/control.rs` — the `/herdr` process runner: `control_prompt`
  frames the one-shot session, `run_control_command` spawns the
  whitespace-split command in its own process group (prompt on stdin,
  stdout+stderr concatenated, group-killed via `kill -TERM -<pid>` on
  timeout, `kill_on_drop` backstop), and `truncate_reply` cuts the
  reply to Discord's cap without splitting a UTF-8 character.
- `src/relay.rs` — per-agent conversation workers (keyed by pane id —
  agents are unnamed; the `RelayJob` carries the session path): one `mpsc`
  channel per agent (shared `Arc<Mutex<HashMap>>` of senders with
  same-channel-guarded removal, 600s idle timeout). `process_job` delivers
  the prompt **immediately** (`agent.prompt` without `wait` — herdr writes
  it to the agent's input and answers at once), so a long turn never holds
  the queue: later messages reach the agent as they arrive instead of
  sitting invisible behind the previous job's settle. Turn settlement runs
  in a **detached task per message** (`settle_job`): `agent.wait` loop
  until idle/done/blocked, then mirror the session file (post the new
  messages since the last mirror) and post the
  blocked notice (deduped per pane within 30s, since several outstanding
  prompts can settle into one blocked state). The typing indicator is the
  event loop's, driven by the working status event.
- `src/lib.rs` (Bot + event handler) — the serenity `EventHandler` in its
  `dispatch` form (Ready spawns the poll + event loop; ThreadCreate,
  ThreadDelete, and ChannelDelete are delegated to the forum's thread/channel
  handlers; Message
  relays to sessions and resumes dead ones), plus the `run()` wiring: `Http` with the default
  ratelimiter, `ClientBuilder` with the bot handler and the poise framework.

## Key Directories

| Path | Purpose |
|---|---|
|`src/`|Bot core: `lib.rs` (Bot + event-handler dispatch), `config.rs`, `error.rs`, `relay.rs`, `control.rs` (the `/herdr` process runner), `test_util.rs` (shared test fixtures)|
|`src/forum/`|Forum-side state, split by feature: `mod.rs` (struct + shared state), `tags.rs` (status/harness tag management), `workspace.rs` (workspace↔forum lifecycle, worktree resolution, stale-row pruning), `post.rs` (session↔post binding lifecycle, thread handling), `spawn.rs` (agent spawn + naming + cwd), `resume.rs` (dead-session resume), `lookup.rs` (Discord channel resolution), `sync.rs` (transcript mirror), `echo.rs` (user webhook echoes), `render.rs` (tool-call rendering + message splitting), `events.rs` (event loop + reconcile + pane lifecycle), `poll.rs` (2s transcript poll + rotations), `titles.rs` (titles + starter message)|
| `src/herdr/` | herdr Unix-socket client, split by concern: `mod.rs` (re-exports), `model.rs` (agent/workspace records + status + ids), `error.rs` (error type), `client.rs` (the socket client + API methods), `event.rs` (subscription machinery), `wire.rs` (envelope + result payloads) |
| `src/session/` | Transcript normalization: `mod.rs` (Harness + read_session dispatch), `model.rs` (conversation model), `title.rs` (transcript-sourced titles), `common.rs` (shared parsing skeleton), `omp.rs`/`claude.rs`/`codex.rs`/`pi.rs` (per-harness file parsers, each with its parser tests), `opencode.rs` (opencode SQLite store reader) |
| `src/db/` | SQLite state: `mod.rs` (Db wrapper + queries), `model.rs` (row types), `migrate.rs` (schema push + legacy migrations) |
| `src/commands/` | Slash commands: `mod.rs` (poise framework wiring), `agent.rs` (the `/agent` modal + launch), `herdr.rs` (the `/herdr` control command) |
| `tests/` | `herdr_live.rs` (live integration, gated) and `fixtures/api/` (captured herdr API JSON, embedded via `include_str!`) |
| `.github/workflows/` | CI — `ci-cd.yaml` (test, test-docs, check/clippy, check-docs, check-format via the flake's treefmt check; dtolnay toolchain + Swatinem rust-cache, nightly for clippy/docs, nix for formatting) and `security-audit.yaml` (daily + on manifest changes, cargo-deny) |

## Development Commands

There is **no rustup/rustc on PATH** — everything goes through nix. The
toolchain is nightly (required for `rustfmt.toml` unstable options).

**`nix develop` is the source of truth** — direnv (`.envrc` uses
`use flake`) loads the same devshell. It provides the nightly toolchain,
the treefmt wrapper, nil, and prek, plus the check tools (cargo-deny,
cargo-nextest, …) propagated from the checks' native build inputs, and
builds the flake checks (clippy/doc/fmt/treefmt/deny/nextest) so the
environment is verified. The first load builds the checks; later loads
are cached.

```sh
# inside the devshell (nix develop, or direnv on `cd`):
cargo fmt                          # nightly rustfmt
cargo clippy --all-targets -- -D warnings
cargo test

# live integration test (spawns real herdr agents; cleans up after itself)
HERDR_LIVE_TESTS=1 cargo test --test herdr_live

# build/run the binary
nix build .#default && ./result/bin/herdcord
nix run .#default

# license/advisory check
cargo deny check
```

Outside the devshell the same commands run via `nix shell` one-liners
(e.g. `nix shell --inputs-from . 'rust-overlay#rust-nightly' 'nixpkgs#gcc' -c cargo test`).
Every commit is guarded by a prek hook that runs `treefmt --ci` on the
whole tree (`.pre-commit-config.yaml`); run it manually with `prek run`,
wire it into git with `prek install`.
- **CRITICAL:** the flake source is `src = ./.` which only includes
  **git-tracked** files. Any new file must be `git add`-ed before
  `nix build .#default`/`nix flake check`, or the sandbox build silently won't
  see it.
- Run the bot with env config (see `src/config.rs`): `DISCORD_BOT_TOKEN` and
  `GUILD_ID` required; `ALLOWED_USER_ID` optional (when set, only that
  Discord user may talk to agents and launch them via forum posts);
  `HERDR_CONTROL_COMMAND` (the `/herdr` one-shot control command —
  whitespace-split, opt-in: unset registers no `/herdr`; the recommended
  lean payload is `pi -p --no-session --tools bash --no-skills
  --no-context-files --no-extensions --no-themes
  --no-prompt-templates`), `HERDR_CONTROL_CWD` (default: home dir; a
  leading `~`/`~/` expands to home) and
  `HERDR_CONTROL_TIMEOUT` (seconds, default 300);
  `RUST_LOG` default `warn,herdcord=trace`. Everything else (timeouts,
  harness, sync interval, state dir, socket path) is a sane default
  const in `src/config.rs`.

## Commits

- Conventional Commits (`type(scope): subject`, see
  https://www.conventionalcommits.org/en/v1.0.0/), with a scope when it
  makes sense (a module or area of the bot, e.g. `fix(relay): …`).
- Subjects are always lowercase.
- Backticks around anything referencing code or code constructs in the
  message body — identifiers, paths, commands, API names, types
  (e.g. ``the `Context` data type``, ```ClientBuilder::data``).

## Code Conventions & Common Patterns

- **Commits**: commit as soon as a feature or fix has been developed — never
  leave finished work uncommitted, so state is never lost.
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
  `AgentStatus` (`src/herdr/model.rs`) with `as_str()` only at the wire
  boundary; agent harnesses are the closed `Harness` enum
  (`src/session/mod.rs`) with `as_str()` only for session paths/harness tags;
  herdr error actions use a private `HerdrAction` enum.
- **Errors**: `BotError` (`src/error.rs`) with thiserror; `serenity::Error` is
  boxed (`Box<serenity::Error>`) to keep the variant small (clippy
  `result_large_err`). `?` works via `#[from]` adapter variants
  (including `toasty::Error`).
- **Async**: tokio multi-thread; long herdr calls run inside
  `tokio::time::timeout`; per-agent relay workers are `tokio::spawn`ed
  tasks owning `mpsc` receivers. State is shared via `Arc` (`Arc<Config>`,
  `Arc<Forum>`, `Arc<Relay>`); the only mutable in-memory state is the
  four small collections on `Forum` (`sessions_by_pane`, `resuming`,
  `tool_messages`, `transcript_stamps`), and `sync_lock` (an
  `Arc<tokio::Mutex<()>>`) serializes
  transcript mirrors so the poll and the relay settle can never
  double-post from the same cursor — mirrors also re-read the row under
  the lock,
  because a caller's copy may predate another mirror's commits. Beware:
  `serenity::cache::CacheRef` is `!Send` — clone out of the cache before
  any `.await`.
- **Sessions**: a session is identified by herdr's reported session
  reference (`agent_session.value`): a transcript path for omp, a session
  id for claude-code and codex — unique per launch, and exactly what the
  harness resumes with. Session transcripts are read with
  `read_session(harness, Path::new(&session.transcript_path))` — the
  harness comes typed on the wire record (`Agent::harness`), never
  scraped from terminal output.
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
  `Db`), `run()` (serenity `Http` with the default ratelimiter,
  `ClientBuilder` with the bot's `EventHandler` and the poise framework),
  the `dispatch`-based `EventHandler` (message relay + dead-thread resume,
  thread handling, lifecycle spawn), the poll task + event loop spawn.
- `src/herdr/` — the herdr Unix-socket client (NDJSON, one request
  per connection): `client.rs` holds the client and its public methods
  (new, list_workspaces, create_workspace_with_pane, close_workspace,
  create_tab, close_tab, list_agents, get_agent, start_agent, send_prompt,
  prompt_agent, wait_agent, session_snapshot → `Vec<Agent>`, subscribe);
  `model.rs` the nutype newtypes `WorkspaceId`/`PaneId`/`TabId`/
  `SessionPath` and the `Agent`/`Workspace` records plus `AgentStatus`;
  `error.rs` the `Error` type with `is_timeout`/`is_stalled`. Fixture
  tests pin the wire format.
- `src/db/` — toasty SQLite state: `WorkspaceRow`/`SessionRow` in
  `model.rs` and the `Db` wrapper in `mod.rs` (workspace/session lookups
  and upserts; schema pushed from the registered models on startup; rows
  are label-keyed and re-keyed when a workspace or session is renamed);
  schema push and the legacy migrations in `migrate.rs`.
- `src/forum/` — per-workspace forum channels, session posts, tags,
  manual-post deletion, dead-thread tag pruning, session resume, spawn
  helpers, the transcript mirror, and the event loop, split by feature
  (see Key Directories).
- `src/commands/` — the poise framework in `mod.rs` plus the `/agent`
  modal command in `agent.rs` and the `/herdr` control command in
  `herdr.rs`.
- `src/relay.rs` — conversation workers and the session-file sync delta.
- `src/config.rs` — minimal env config: `DISCORD_BOT_TOKEN`, `GUILD_ID`,
  `ALLOWED_USER_ID`, `HERDR_CONTROL_COMMAND`/`HERDR_CONTROL_CWD`/
  `HERDR_CONTROL_TIMEOUT` (the `/herdr` control command knobs). Sane
  defaults as consts: `DEFAULT_HARNESS`
  (`Harness::Pi`, the modal's preselected harness), `PROMPT_TIMEOUT`
  (300s),
  `OPERATION_TIMEOUT` (30s),
  `SYNC_INTERVAL` (600s), `MESSAGE_POLL_INTERVAL` (2s),
  `MAX_SYNC_MESSAGES` (5), `CATCHUP_BACKLOG` (50), `CONTROL_TIMEOUT`
  (300s) and `CONTROL_REPLY_LIMIT` (2000). `socket_path()` honors
  `HERDR_SOCKET_PATH`/`HERDR_SESSION`; `session_socket_path(name)` resolves
  a named session's socket regardless of env overrides; the state db lives
  under `$XDG_STATE_HOME/herdcord`.
- `src/control.rs` — the `/herdr` process runner: `control_prompt`,
  `run_control_command` (spawn + stdin pipe + process-group kill on
  timeout), `truncate_reply`.
- `flake.nix` — crane + rust-overlay nightly; `packages.default`; checks
  clippy/doc/fmt/deny/nextest; treefmt (nixfmt, statix, deadnix, rustfmt,
  taplo). The serenity and poise dependencies are git branches, so `nix
  flake check` needs network access to fetch them.
- `deny.toml` — license allowlist (incl. MIT-0/MPL-2.0 for toasty's
  transitive deps).

## Runtime/Tooling Preferences

- **Rust**: nightly only (rustfmt.toml unstable options; doc builds under
  `-D warnings`). Edition 2024.
- **Nix**: flake-based (`flake-parts` + `crane` + `rust-overlay` + `treefmt-nix`).
  No rustup. `gcc` is needed in the shell (ring build script).
- **Discord**: serenity on the `next` branch (Component V2; git dependency)
  + poise on `serenity-next` (git dependency) for slash commands. Required
  intents: GUILDS, GUILD_MESSAGES, MESSAGE_CONTENT.
- **herdr**: control over the Unix socket in the herdr config dir
  (`$XDG_CONFIG_HOME/herdr/herdr.sock`; `HERDR_SOCKET_PATH`/`HERDR_SESSION`
  to override).
- **Dependencies**: prefer popular libraries over vendored logic (serenity's
  built-in `Typing`, `ExecuteWebhook`, poise, the `time` crate, etc.).
  Current deps: serenity, poise, toasty, nutype, thiserror, envy, dotenvy,
  dirs, tokio, tracing, serde/serde_json, color-eyre (main only). No anyhow
  in the lib.

## Testing & QA

- **Unit tests**: pure tests in `src/` — `src/herdr/` parses the captured
  fixture envelopes (`tests/fixtures/api/*.json` via `include_str!`; the
  fixtures are the contract for herdr's wire format — regenerate them
  from a live server, not by hand), `src/session/` tests the transcript
  parsers (`parse_omp`/`parse_claude_code`/`parse_codex`,
  malformed/truncated lines, titles) and `read_session`, `src/db/` tests
  workspace/session upserts and lookups on an in-memory database,
  `src/forum/` tests the agent-name timestamp, the title selection, the
  modal construction, and the per-harness resume args, `src/config.rs`
  tests state-dir resolution and the control knobs (`control_cwd`/
  `control_timeout` defaults and overrides), `src/control.rs` runs real
  `cat`/`sh`/`sleep` processes (stdin pipe, stderr concatenation, env
  injection, nonzero exit, process-group kill on timeout), and
  `src/commands/` tests `build_commands` registration gating.
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
  (clippy, doc, fmt, treefmt, deny, nextest). CI runs the same gates
  directly — `cargo test --all-targets` on stable, clippy/doc on nightly,
  formatting via the flake's treefmt check
  (`nix build .#checks.x86_64-linux.treefmt`), and `cargo-deny` (see
  `.github/workflows/`); `nix flake check` remains the local hermetic
  equivalent. Every commit is additionally gated by a prek hook running
  `treefmt --ci` on the whole tree (`.pre-commit-config.yaml`).
- **Coverage expectations**: no coverage tooling; correctness is enforced by
  the fixture tests, the session parser + db tests, the live tests, and
  clippy's pedantic set. New herdr wire shapes should get a fixture +
  parsing test; new session formats should get a parser test.

## Agent skills

### Issue tracker

Issues and specs live as GitHub issues, managed via the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

Default vocabulary: `needs-triage` / `needs-info` / `ready-for-agent` / `ready-for-human` / `wontfix`. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context layout: one `CONTEXT.md` at the repo root plus `docs/adr/`. See `docs/agents/domain.md`.

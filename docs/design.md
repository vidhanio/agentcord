# Agentcord design

Status: draft

## Summary

Agentcord is a Discord client for agents that implement the Agent Client
Protocol (ACP). It exposes every configured agent and project through one
Discord forum. Each forum post represents one ACP session, and the post's
thread is the user interface for that session.

Agentcord replaces herdcord in place. The project retains its useful Git,
Rust, Nix, CI, formatting, Serenity, and Poise infrastructure, but removes the
herdr integration and all herdcord-specific runtime behavior. Agentcord does
not scrape terminals or read agent-specific transcript formats. All agent
interaction and conversation state flows through ACP.

## Goals

- Support arbitrary ACP agents without agent-specific application code.
- Put sessions for every configured agent in one configured Discord forum.
- Create sessions through a single `/agent` command.
- Preserve live and restorable sessions across Agentcord restarts when the
  agent advertises the required ACP capability.
- Render ACP messages, thoughts, tool calls, metadata, permissions, and
  lifecycle changes as native Discord interactions.
- Keep agent commands, environment, display names, and forum-tag presentation
  entirely in configuration.
- Keep projects concise and recognizable by displaying paths relative to a
  configured base path.

## Non-goals

- Integrating with herdr or its socket API.
- Discovering sessions from terminal state or transcript files.
- Hard-coding knowledge of Pi, Claude Code, Codex, or any other agent.
- Migrating the existing herdcord database or configuration.
- Providing `/herdr` or another general-purpose control command.
- Silently replacing a lost ACP conversation with a new session.

## Discord model

### Forum

Agentcord uses one existing Discord forum identified by a required channel ID.
It does not discover the forum by name or recreate a deleted forum. Agentcord
owns the posts it creates and the configured agent tags within that forum.

All sessions across all projects and agents appear in this forum.

### Agent tags

Each configured agent has one forum tag. Its name and emoji are configuration,
including support for Unicode emoji and Discord custom emoji. No set of agent
names or emoji is compiled into Agentcord.

A session post retains the tag for the agent that owns the ACP session. The
initial design does not require hard-coded lifecycle-status tags.

Discord's forum-tag limits must be validated when configuration is loaded. The
exact policy for configurations with more agents than Discord can represent is
an open decision.

### Session posts

One forum post maps to one ACP session. Its title has the form:

```text
<project> · <session title>
```

`<project>` is the project's path relative to the configured project base.
For example, with `base_path = "~/Projects"`, the project
`~/Projects/agentcord` is displayed as `agentcord`.

The ACP session title is preferred when the protocol or agent supplies one.
The fallback title and rename policy remain to be specified.

The starter message summarizes stable session metadata, including at least:

- configured agent display name;
- project label;
- absolute working directory;
- ACP session ID;
- negotiated ACP protocol version and relevant capabilities;
- whether the session can be restored.

The starter message is metadata, not a second conversation transcript.

### `/agent`

`/agent` is the only command interface. It opens a Discord modal containing:

1. an agent selector populated from configured ACP agents;
2. a project selector populated from projects under the configured base path;
3. a multiline initial prompt.

Submitting the modal starts the configured ACP process, negotiates protocol
capabilities, creates a session with the selected project's absolute path as
its working directory, creates the forum post, and submits the prompt.

Project selector labels use the same base-relative representation as post
titles. Discord component option limits and project discovery depth remain
open design decisions.

Messages sent by the allowed Discord user in a session thread become ACP user
prompts. Agentcord never injects messages from unapproved users into an ACP
session.

## Project model

Configuration defines a base path, such as `~/Projects`. Projects are
available beneath that path, while their user-facing identity is the normalized
relative path from the base.

```text
base:     /home/alice/Projects
project:  /home/alice/Projects/example/agentcord
label:    example/agentcord
```

Agentcord passes the absolute, canonical project path to ACP as the session
working directory. A discovered path must remain beneath the canonical base;
symlinks must not permit traversal outside it.

The exact discovery rule—immediate children, recursive directories, or Git
repositories only—remains open.

## Agent configuration

Each agent definition has a stable configuration key and supplies all details
needed to start and present that agent. The eventual schema is expected to
cover:

- stable key;
- display name;
- executable and argument vector;
- environment additions;
- optional working-environment controls;
- forum tag name;
- Unicode or custom Discord emoji;
- process startup and shutdown timeouts;
- optional ACP capability requirements.

The command is represented as an executable plus an argument array, not as a
whitespace-split shell string. Agentcord initially supports ACP subprocesses
over stdio. The internal connection boundary should permit additional ACP
transports later without changing Discord session handling.

No renderer, resume argument, transcript parser, status detector, or other
behavior may branch on the configured agent key.

## ACP lifecycle

### Connection and initialization

Each active Discord session owns one supervised ACP subprocess and connection.
Agentcord:

1. spawns the configured executable;
2. establishes ACP over stdio using the Rust SDK;
3. performs initialization and capability negotiation;
4. creates or restores the session;
5. consumes ordered session updates;
6. sends Discord prompts through ACP; and
7. shuts down or cancels work through ACP before terminating the process.

Agentcord exposes only explicitly supported client capabilities. Filesystem,
terminal, and permission behavior must not be advertised until their Discord
and security semantics are implemented.

### Creation

A successful `/agent` submission creates a new ACP session. The selected
project's canonical absolute path is its working directory. Agentcord persists
the returned session ID and Discord binding before treating the post as fully
active.

If initialization or session creation fails, the deferred command response
reports the failure and Agentcord does not leave a live-looking orphan post.

### Restoration

ACP restoration is capability-dependent. Agentcord stores enough binding state
to reconnect a post to the same ACP session when the configured agent supports
restoration.

After an Agentcord restart or subprocess failure:

- a restorable session may start a new configured ACP process and restore the
  original session ID;
- a non-restorable session becomes unavailable;
- Agentcord never creates a replacement session behind the existing post.

The post must clearly distinguish active, restorable inactive, and permanently
unavailable states. The exact archive and resume-on-message behavior remains
open.

### Process isolation

A failed or blocked agent process must not stall other sessions. Each session
has independent process ownership, update handling, cancellation, and timeout
state. Process groups are terminated on timeout and on Agentcord shutdown so
child processes do not leak.

## Conversation rendering

Discord is a live projection of ordered ACP session updates, not the source of
agent transcript truth. Agentcord stores only the Discord-dependent binding
and rendering state needed to update messages idempotently.

### User messages

A Discord thread message submitted to ACP already exists in Discord and is not
mirrored a second time. User messages originating through another ACP client
are rendered only if ACP exposes them through the session update stream.

### Thinking and final output

Thinking is streamed into one italicized Discord message and edited in place.
As new thought text arrives, Agentcord updates that message rather than posting
new fragments.

If the rendered thought reaches Discord's message limit, Agentcord removes text
from the beginning and preserves the newest content. Truncation must occur on
UTF-8 and Discord-formatting boundaries and should include a small indication
that earlier thought was omitted.

When final output begins, Agentcord edits the same Discord message, removes all
thinking text, and replaces it with the final agent output. Subsequent output
updates continue editing that message in place. Thinking is therefore
transient and does not remain in the completed conversation.

The behavior for final output longer than one Discord message remains open; it
must preserve the final answer rather than applying the thought-truncation
policy.

### Tool calls

One ACP tool-call ID maps to one Discord message. Streaming and status updates
edit that message in place instead of producing duplicates.

Rendering is selected from ACP's structured tool-call kind and content, never
from an agent name:

- file edits are rendered as fenced unified diffs using the `diff` language;
- shell commands are rendered as fenced code blocks using an appropriate shell
  language;
- every other tool call is rendered as an embed.

Running, completed, failed, and cancelled states are represented consistently.
Errors are added to the existing tool message. If ACP replaces structured
content in a later update, Discord is edited to match the latest authoritative
state.

Agentcord must define a generic fallback for unknown or extension tool kinds so
new ACP agents remain usable without code changes.

### Ordering and idempotency

ACP updates are processed in protocol order. Message and tool-call identifiers
are persisted with their Discord message IDs where later upserts must survive a
restart. Full-content replacements supersede prior chunks according to ACP
semantics.

A per-session serialization boundary prevents concurrent Discord events and
ACP updates from racing to create or edit the same message.

## Permissions and client capabilities

ACP permission requests must eventually be surfaced as Discord interactions,
not approved implicitly. Only the configured allowed user may answer them.
Requests time out safely and denial is the default when Discord delivery or
Agentcord state is uncertain.

The exact button layout, timeout, remembered-decision policy, and treatment of
filesystem and terminal callbacks remain open. Until those decisions are
implemented, Agentcord must not advertise unsupported client capabilities.

## State

Agentcord uses a new database beneath `$XDG_STATE_HOME/agentcord`. Existing
herdcord state is neither migrated nor deleted.

At minimum, persisted state associates:

- ACP session ID;
- configured agent key;
- canonical project path and display label;
- Discord thread and starter-message IDs;
- latest session title and availability state;
- negotiated restoration support; and
- ACP message/tool identifiers with Discord message IDs when required for
  idempotent edits.

Secrets, subprocess handles, and live ACP connection objects are never stored
in the database.

The configuration moves to `$XDG_CONFIG_HOME/agentcord`, with an
overridable explicit path and safe environment-variable expansion.

## Concurrency and ownership

- One task supervises each active ACP subprocess and connection.
- One ordered worker serializes prompts and updates for each session.
- Discord event handlers enqueue work rather than holding global locks across
  ACP calls.
- Database commits establish durable bindings before externally visible state
  is considered complete.
- Recovery is session-local; one unhealthy agent cannot block the forum.
- Discord rate limits and transient failures retry from persisted idempotency
  state.

## Security

- Only the configured Discord user may invoke `/agent`, submit prompts, or
  answer permission requests.
- Project paths are canonicalized and constrained to the configured base path.
- Agent executables and arguments come from trusted local configuration, never
  from Discord input.
- Commands are spawned directly without an implicit shell.
- Environment inheritance and overrides must be explicit in the final config
  contract.
- ACP filesystem and terminal capabilities follow least privilege and remain
  disabled until intentionally configured.

## Rewrite strategy

This is an in-place product replacement rather than a compatibility layer.
The rewrite keeps useful repository infrastructure but removes:

- the herdr socket client and event model;
- workspace-to-forum mirroring;
- harness enums and agent-specific resume arguments;
- transcript parsers and transcript polling;
- `/herdr` and its control subprocess;
- herdcord database models and migrations; and
- obsolete herdcord/herdr domain documentation.

The crate, binary, configuration paths, state paths, Home Manager module,
examples, tests, and user-facing text are renamed to Agentcord.

Implementation proceeds in coherent, independently tested stages:

1. rename the product and replace configuration/domain models;
2. add generic ACP process initialization and session creation;
3. create the one-forum `/agent` flow and durable session bindings;
4. stream thoughts and final messages with in-place Discord edits;
5. render and update generic ACP tool calls;
6. implement restoration and process recovery;
7. implement permission requests and intentionally selected client
   capabilities; and
8. harden rate-limit recovery, shutdown, documentation, and packaging.

The rewrite is complete only when session creation and restoration, streaming,
tool rendering, permissions, metadata/title updates, tags, and recovery work
without agent-specific branches.

## Open decisions

The following decisions were not settled by the planning captured in this
document:

1. Which directories beneath `base_path` count as projects, and how deeply to
   discover them.
2. How selectors handle more projects or agents than Discord permits in one
   component.
3. The session-title fallback and when a post may be renamed.
4. How completed final output exceeding Discord's message limit is split.
5. The exact inactive, archive, and resume-on-message lifecycle.
6. The Discord permission-request interaction and timeout policy.
7. Which ACP filesystem and terminal client capabilities Agentcord should
   implement and advertise.
8. How to validate or degrade gracefully when configured agents exceed
   Discord's forum-tag limit.
9. The precise configuration schema and environment inheritance policy.
10. Whether one `base_path` is sufficient or multiple project roots are
    required.

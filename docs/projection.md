# Discord projection

This document is the contract for the first end-to-end Agentcord message
path. It intentionally describes a small projection surface; protocol features
are added as new update cases instead of making the renderer know about agent
implementations.

## Responsibilities

The path has four boundaries:

```text
Serenity message
  -> Bot message handler
  -> Session supervisor command
  -> ordered ACP update
  -> projection reducer
  -> Discord message operations
```

`Bot` is the composition root. It owns the immutable configuration, Toasty
database, Serenity context, webhook cache, and session supervisor. A session
actor owns its ACP process, connection, and command ordering. The projection
reducer owns no process or Discord handles.

## Input contract

The ACP actor sends `ProjectionEvent` values in the order received from one
ACP session. Each event contains:

- the Discord thread;
- the current prompt-turn identifier (a Discord message or interaction ID);
- whether the event is a replay from `session/load`; and
- the ACP `SessionUpdate`.

The actor is the sole writer for a session's event stream. The reducer does
not reorder or concurrently merge events. ACP message IDs are used as stable
source IDs when provided. A live, unkeyed text stream uses the turn identifier
as its source ID; unkeyed replay text is ignored because it cannot be
distinguished safely from already-rendered history. A replay chunk already
present in a source is ignored as a small idempotence guard.

## First supported updates

The first implementation handles only the update needed for a useful text
conversation:

- `agent_message_chunk`: append text to a message source;
- `user_message_chunk`: ignore it because the Discord-originated message is
  already mirrored.

Thought, tool-call, plan, metadata, and usage updates are intentionally
ignored until their Discord semantics are specified. Ignoring an unsupported
update is observable in tracing but does not terminate the session.

The client advertises no filesystem, terminal, permission, or elicitation
capabilities in this slice. Those request/response flows are separate features
and must be implemented before the corresponding capabilities are enabled.

## Source and persistence model

One logical source is identified by `(thread_id, source_kind, source_id)`.
Its reducer state is JSON owned by the renderer, and its ordered Discord
message IDs are persisted separately. The desired logical state is persisted
with the previous message IDs before Discord is touched; the resulting IDs
are persisted in a second Toasty transaction. If Discord fails part-way
through, the known IDs are retained so a retry can continue from them.

The database is a projection cache, not an event log. If a process exits
between a Discord operation and its database write, the next replay may repeat
an operation; source IDs and message IDs make live updates idempotent once the
projection is loaded. Cross-system exactly-once delivery is not claimed.

## Discord operations

The reducer renders a source into bounded message chunks. The Discord adapter
then:

1. edits existing messages by position;
2. creates missing messages in the session thread; and
3. deletes surplus messages from the prior projection.

The adapter never calls ACP and never mutates reducer state. A failed Discord
operation leaves the desired logical state durable and records the IDs known
so far for retry. Discord operations are never performed while an application
lock is held.

User prompts use one forum webhook, named and cached per process. The allowed
user's current guild display name and avatar are applied to each execution.
Webhook discovery/creation is serialized; a failed execution clears the cache
and falls back to normal bot-authored messages. A `NeedsMirror` prompt is
forwarded to ACP after the mirror attempt, and a mirror failure is logged but
does not discard the user's request. Gateway messages use `AlreadyVisible`
origin and are not mirrored; prompts created by another surface use
`NeedsMirror` and are mirrored by the session actor before `session/prompt`.

## Message-handler contract

The Serenity message handler accepts only messages from the configured user,
ignores the bot's own messages and blank content, and requires a persisted
session bound to the message's thread. It creates an `AlreadyVisible` prompt
and delegates to the session supervisor; the supervisor serializes prompts per
session and sends `session/prompt` to ACP. No gateway callback performs ACP
protocol work directly.

## Clean-code constraints

- Keep ACP types at the ACP boundary; use small renderer-owned state types.
- Keep mutable turn and process state inside a session actor.
- Keep Discord API calls in the Discord adapter and webhook module.
- Do not add global mutable state beyond the shared `Bot` dependencies and
  bounded supervisor registry.
- Preserve ACP update order with one per-session queue.
- Use bounded queues and explicit session faults for overload; a full update
  queue stops the session actor instead of silently dropping text or tool-call
  updates.
- Add tests for reducer ordering/idempotence, message chunking, webhook
  fallback policy, and forwarding authorization. Do not test trivial getters.

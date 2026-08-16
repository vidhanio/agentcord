# The transcript mirror is poll-owned; events own metadata

herdcord's forum mirror was written from eight call sites — status events,
agent detection, the 2s poll, the relay settle, thread creation, the periodic
reconcile, transcript-rotation adoption, and deleted-post recovery — all
serialized by one lock, all re-doing the same cursor-guarded pass, while the
per-status-event path also refreshed the starter message (a herdr `worktree`
call plus a Discord get+edit) and the relay owned a second, independent typing
indicator. The decision splits the mirror by concern, one writer each: the
**transcript mirror** runs only from the 2s poll (stat-gated on file
mtime/size, so an unchanged file costs one stat) and from the relay settle
(the immediate reply path); **post metadata** — tags, title, unarchive —
runs only from status events and the reconcile drift backstop (startup,
reconnect, 600s); the starter message's intro refreshes only on post
creation, session death, and resume; `pane.agent_detected` only triggers the
re-subscribe (the post-reconnect reconcile applies the new agent's metadata);
thread creation only deletes manual posts; deleted posts and forums are
repaired on Discord's `thread.delete` and `channel.delete` events (posts and
forums re-created, dead rows pruned) through the one rare full pass (ensure +
metadata + mirror), with the poll's recovery escalation and the reconcile as
backstops.

**Considered Options** — (1) keep the status event's instant mirror: a
brand-new transcript turn appears in the thread immediately instead of within
2s, at the cost of every status change re-running the whole mirror pass;
rejected because the 2s poll already mirrors everything and the cursor makes
the delta cheap while the parse is not. (2) Make `pane.updated` (session-wide,
full pane record) replace the per-pane `pane.agent_status_changed`
subscriptions: rejected because herdr emits `pane.updated` only on agent-name
change and metadata expiry, not on status changes — the per-pane status
subscription and its re-subscribe dance are forced by the wire API.

**Consequences** — a transcript turn is mirrored within 2s rather than on the
status event; a brand-new agent's first metadata arrives on the post-reconnect
reconcile (~1s later) instead of in the detect handler; a re-created post gets
its tags from the recovery pass itself, so a quiet idle agent's post is never
left tagless; `sync_lock` still serializes the poll, settle, ensure, and
recovery passes. The mirror/mirror pass and metadata pass vocabulary is what
the forum code is organized around; keep new triggers on this side of the
split.

# herdcord

A Discord bot that turns herdr forum channels into a control surface: herdr is
the truth, Discord is the mirror, and agents launch from `/agent` and talk
through their forum posts.

## Language

**Control command**:
The user-configured external command that `/herdr` runs as a one-shot
subprocess: the user's prompt is piped to its stdin and its output is relayed
back as the reply. Configurable via env var, opt-in, herdr-agnostic — herdr
control is its common payload, not its contract.
_Avoid_: control-plane agent (the old design's LLM-in-a-throwaway-session),
control runner

**Main session**:
The user's real herdr session — the one the bot mirrors into forums and the
one the control command acts on when the bot injects `HERDR_ENV=1` and the
socket env. Distinct from the old design's throwaway session, which was a
private, unmirrored herdr session spawned per command.
_Avoid_: throwaway session, control session

**Mirror**:
The Discord-side projection of live herdr state (forum channels, session
posts, tags). One-way: herdr is the truth, Discord reflects it; actions the
control command takes on the main session reach the mirror through herdr's
own event stream, never through a second write path.
_Avoid_: sync (two-way implies the reverse direction)

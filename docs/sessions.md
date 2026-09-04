# Sessions and context

URI Agent stores conversations as append-only sessions and preserves the exact
startup context used by the model. This document describes project scoping,
Agent sessions, durability, retries, and context checkpoints.

## Storage and project boundaries

Sessions are stored in SQLite at:

```text
<platform-data-dir>/uri-agent/sessions-v3.db
```

On macOS the database is under `~/.config/uri-agent`; if no platform data
directory exists, URI Agent falls back to `<project>/.uri-agent`. Earlier
database versions remain untouched and are not migrated into `sessions-v3.db`.

The canonical startup directory is each session's project boundary:

- a normal launch creates a new in-memory session;
- `--continue-session` and `--session latest` select the latest project session;
- `--session <id>` accepts only an ID from that project;
- `:resume` lists the project's root conversations.

The `sessions` protocol can search wider scopes only when explicitly asked.
Exact search reads SQLite directly; semantic and hybrid search use disposable,
automatically maintained sidecar indexes. Searching or indexing never resumes
or changes a conversation. See [Protocols, tasks, and
output](protocols.md#files-search-and-saved-context).

Each session records its provider, model, and thinking effort. Changes are
appended and restored on resume. Composer drafts are saved on exit and session
switch; before the first message, a draft remains project state rather than
creating an empty conversation. Switching sessions leaves active work and
undelivered input attached to the original session. Process exit cancels and
joins active work after durable interruption records are written.

ACP-created sessions use the same database and become available to the TUI
after the ACP owner releases them. One ACP process may serve multiple projects,
but every project has isolated configuration, plugins, MCP state, Skills, and
Agents. See [ACP v1](acp.md#project-and-session-lifecycle).

## AgentHost and Agent specifications

Each project runtime owns one `AgentHost`. The TUI, linked and WASM plugins,
child Agents, and resident plugins use the same persisted Agent runtime.

An `AgentSpec` selects provider, model, thinking effort, working directory,
parent session, system-prompt mode (`inherit`, `append`, or `replace`), exact or
complete tool/protocol sets, and an optional output cap. Prompt and capability
selection are fixed at creation. Provider, model, and thinking freeze after the
first submission is durably accepted.

Root Agents are depth 1. Plugins may create only depth-2 Agents with a persisted,
same-project root parent; children cannot create another generation. The TUI
lists only root conversations, while child conversations use the same session
database.

Submissions are `Prompt` or `Steer`. Prompt starts an idle Agent or queues a new
turn. Steer targets the next model boundary while an Agent is active and acts
as Prompt when it is idle. Accepted input is durable until delivered.

A new session remains in memory while startup context is prepared. The first
accepted prompt persists the frozen context and message together; opening and
closing an empty session creates no database record. ACP `session/new` follows
the same boundary and reserves only in-memory state until its first prompt.

## Frozen startup context

Before accepting the first prompt, a session freezes its complete generated
system prompt, session-scoped protocol records, and selected Skill snapshots.
Resume uses that snapshot instead of rediscovering current project instructions,
protocols, or Skills.

MCP records freeze stable identity and prompt metadata, while each operation
still resolves mutable transport and credential references. A newly configured
server does not join an existing session; removing or disabling one already
recorded by the session makes later calls fail. See [Startup context and
Skills](context.md).

## Append-only durability

Messages, model settings, tool calls and results, usage, notices, errors, task
state, turn boundaries, notes, and checkpoints are appended as events. Normal
operation and checkpointing do not rewrite earlier events.

Derived resume and semantic indexes are rebuildable caches, never the source of
truth. Missing or invalid cache data falls back to authoritative events.
Removing the semantic cache under `uri-agent/retrieval/v2/` loses no session
data; a later ranked search rebuilds it.

Transcript and model-replay forms of one completed message commit together.
This keeps typed images and provider tool-call identities valid across resume.
Streaming text and reasoning are provisional and become durable only with the
completed response. A task that was still active when its process ended is
restored as cancelled rather than restarted.

## Retries and the model loop

URI Agent retries transient rate-limit, network, server, timeout, conflict, and
empty-response failures with bounded, failure-specific budgets. Provider retry
headers take priority over fallback exponential delays. Authentication,
billing, malformed-request, and other non-transient failures settle directly.

Retries are visible session events. Provisional output from a failed stream is
discarded before another attempt and never enters model replay. Double `Esc`
interrupts an active request or retry delay.

A turn has no fixed tool-round limit. It continues until the model returns no
tool call, the user interrupts it, or an unrecoverable provider, persistence,
or runtime failure occurs. Model-facing protocol calls must satisfy the help
gate described in [Protocols, tasks, and output](protocols.md#model-interface).

When interruption leaves tool calls unfinished, URI Agent appends correlated
failed results before settling the turn so future replay never contains an
unanswered tool call. Repeated identical tool-call batches receive a hidden
redirect asking the model to change arguments, use another tool, or finish;
this also prevents polling completed background work indefinitely.

## Context windows and checkpoints

URI Agent measures replay against the selected model's context window. Provider
usage is authoritative when available; later content is estimated until a new
response arrives. Before each request, the requested output is capped to the
estimated remaining room after a safety margin.

`rollover` is the default checkpoint strategy. Near the configured threshold,
the model receives one hidden reminder to review its notes. At the threshold,
URI Agent starts a fresh model window without generating a summary. The previous
transcript remains in SQLite but leaves provider replay; a hidden bootstrap
directs the model to active notes and bounded prior history. Protocol-help state
resets for the new window.

The `context` protocol provides durable titled notes, context status, window and
user-statement listings, exact or ranked history search, and reads around stable
session-local record anchors. Notes have stable IDs and revisions, a shared
model-relative budget, and a bounded active count. Exact limits and mutation
routes belong to `context://help`.

Deleting a note creates a tombstone and hides its body from `context` and
`sessions` recovery views. It is not secure erasure: append-only historical
events remain in SQLite. All recovered notes and transcript content are marked
as untrusted reference data.

The alternative `summary` strategy asks the model to summarize older history,
then replays the frozen prompt, summary checkpoint, a valid recent-history
suffix, and later events. Summary generation has no tools and treats transcript
content as untrusted. Original events remain stored.

An Agent with a legacy compaction callback always uses `summary`. After summary
generation, its plugin may atomically change only the system prompt, tools, or
protocols for the checkpoint; provider, model, thinking, project, parent, depth,
and output cap remain fixed. Agents without the `context` protocol also use
`summary` because they cannot recover from rollover.

Provider overflow may force one checkpoint and retry for the current turn.
`:compact` requests the active strategy manually, while `:context-strategy`
changes it for the current runtime. Automatic behavior and token budgets are
configured under [settings](configuration.md#settings-fields-and-precedence).

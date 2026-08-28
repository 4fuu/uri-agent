# Sessions and context

URI Agent stores conversations as append-only sessions and preserves the exact startup context used by the model. This document covers persistence, project scoping, the model/tool loop, retries, and context compaction.

## Session storage and project boundaries

Sessions are stored in SQLite at:

```text
<platform-data-dir>/uri-agent/sessions-v2.db
```

On macOS this path is `~/.config/uri-agent/sessions-v2.db`, colocated with configuration. If no platform data directory is available, URI Agent falls back to `<project>/.uri-agent/sessions-v2.db`.

Earlier unversioned `sessions.db` files and their sidecars remain untouched beside the new database as an archive. URI Agent does not open, import, or modify them.

A new session remains in memory while its startup context prepares in the background. Its first user message waits for that context, then URI Agent writes the frozen context, queued startup events, and message in one transaction. Opening and closing an empty session creates no session record.

The canonical startup directory is the project boundary recorded with every session:

- a normal launch starts a new session;
- `--continue-session` and `--session latest` select the latest session for the project;
- `--session <id>` resumes that ID only when it belongs to the project;
- `:resume` lists project sessions with their model and thinking effort.

The read-only `sessions` protocol can search archives across projects when explicitly requested. `@@` completion remains project-scoped. Archive reads never resume or modify a session; see [`sessions`](protocols.md#sessions).

Each session records its provider, model, and thinking effort. Changes append a settings event, and resume restores the latest session settings rather than defaults for a new session.

URI Agent saves the composer draft when the TUI exits or switches sessions. Before the first message, it stores the draft separately by project to avoid creating an empty session. Switching sessions leaves an active turn and undelivered messages attached to the original session. On process exit, URI Agent cancels and joins active turns after their interruption records are durable, then restores messages not yet taken for delivery to the corresponding draft.

## Frozen startup context

A new session freezes a `SessionContext` event containing the complete generated system prompt and selected Skill snapshots before accepting its first user message. Resume reuses it instead of regenerating the prompt or rebinding Skills from the current filesystem. See [Startup context and Skills](context.md) for the inputs and Skill rules.

## Append-only events

User and model messages, model settings, tool calls and results, usage, notices,
errors, task lifecycle notices and terminal reports, turn boundaries, and
compaction checkpoints are appended as events. Resuming a session reconstructs
its settled task reports; a task left pending or running when its process ended
is restored as cancelled rather than restarted. Normal operation and compaction
do not rewrite or delete earlier events.

SQLite also keeps a versioned, rebuildable resume index at real compaction
boundaries. The index contains only cumulative derived state needed at startup,
such as the user-message flag, usage and cache totals, protocol-help
correlations, frozen-context and task event pointers, model settings, and
token-rate calibration inputs. It never contains model history or compaction
replacement history. On resume, model replay is loaded from the
highest-sequence authoritative compaction event and
the later usage and model-message events; other startup state is reduced from
the index plus the authoritative event tail. Missing, stale, malformed, or
newer-version index rows fall back to an event reduction and may be rebuilt.
The append-only events remain the sole source of truth, and deleting the resume
index cannot change replay or session behavior.

Resume-index writes happen only after the authoritative event transaction
commits. They are disposable cache writes: failure does not fail an otherwise
valid append, publish uncommitted state, or make the session unavailable.

Session event range reads use exclusive sequence cursors. `after` and `before`
pages and tail reads are bounded and returned in ascending sequence order, so
adjacent pages concatenate without overlap or gaps, including across
compactions and tool call/result pairs. A page is a committed view at the time
of its query. Appends between requests do not alter earlier sequence ranges;
they become visible to a later `after` query when its cursor reaches them.
Turn-aware grouping remains a presentation-layer policy.

The transcript and model-replay forms of one message commit in the same SQLite transaction. Streaming text and reasoning are provisional TUI updates; only the completed response enters durable replay. Provider tool-call identity is preserved so resumed tool conversations remain valid for the selected backend.

## Model request retries

URI Agent retries transient failures for normal model calls and context-summary calls. Each failure class has an independent counter within one logical call; success or a new model round resets all counters. Counts below are additional attempts after the initial request:

| Failure | Retries | Fallback backoff before jitter |
| --- | ---: | --- |
| Rate limit (`429`) | 20 | 1s exponential, capped at 30s |
| Network or stream transport failure | 5 | 500ms exponential, capped at 8s |
| Server failure (`5xx`) | 5 | 1s exponential, capped at 15s |
| Timeout or `408` | 4 | 1s exponential, capped at 10s |
| Request conflict (`409`) | 4 | 500ms exponential, capped at 8s |
| Empty completed response | 4 | 1s exponential, capped at 8s |

Fallback delays add up to 25% jitter. `Retry-After` or `retry-after-ms` takes precedence, capped at 60 seconds. When those headers are absent, a Google RPC `RetryInfo.retryDelay` in the response body supplies the same delay. Authentication, billing or quota, other client (`4xx`), malformed-request, and unclassified failures settle immediately.

The experimental Antigravity transport performs bounded protocol-specific recovery before this generic policy sees a failure. An expired token or the first `401` refreshes the stored OAuth credential once. The first project-header `403` retries without that header. A Gemini `400` that specifically reports an invalid thought signature replaces signatures in that request copy with the private protocol's dummy marker and retries once; persisted replay remains unchanged. Within each generic attempt, network errors, `408`, `404`, and `5xx` responses fall through the sandbox, daily, and production endpoints in order. Once those endpoints are exhausted, the final failure retains its normal runtime classification and retry budget; `429` responses are never replayed internally.

Because the counters are independent, alternating failure classes can consume up to 42 additional attempts in one logical model call. There is no separate aggregate attempt or elapsed-time limit.

Each retry becomes a visible session event with its reason, delay, and count. Provisional output from a failed stream is cleared before retry and never enters model replay. Double `Esc` interrupts an active request or retry delay.

## Model and tool loop

A turn has no fixed tool-round limit. It continues until the model returns no tool call, the user interrupts it, or an unrecoverable model, persistence, or runtime error occurs.

The first model-facing call to each protocol must successfully read its exact
`<protocol>://help` route with an empty body. This state belongs to the session;
resuming reconstructs it from successful persisted tool results. Internal
protocol calls made by a WASM plugin are not model-facing and bypass this gate.

If a turn is interrupted while tool calls from one model response are pending, URI Agent appends a failed result with the original correlation identity for every unfinished call before settling the turn. Later requests therefore never replay a tool call without its corresponding result.

URI Agent detects consecutive model responses that each contain exactly one tool call with the same tool name and canonical JSON arguments. On the fifth identical call, it appends a hidden, durable redirect before the next model request, asking the model to change arguments, use another tool, or finish with its findings. The redirect enters persisted model replay without appearing as user input or ending the turn. A different call, no call, or multiple calls resets the sequence. Background task completion is delivered automatically, so repeated identical `tasks://` reads are polling and receive the same loop protection as other calls.

## Context compaction

URI Agent measures replay against the selected model's context window. The latest valid ordinary API response supplies authoritative usage; later messages are estimated until another response arrives. Before the first response, the estimate includes the frozen prompt, tool definitions, messages, and images. It counts four ASCII characters or one non-ASCII character per token and assigns 1,200 tokens to each image regardless of payload size. A compaction invalidates the previous usage baseline until the next response.

Before each provider request, URI Agent caps the catalog output limit to estimated context room after a fixed 4,096-token safety margin, with a minimum request of one output token. This cap is separate from the configurable compaction reserve.

Automatic compaction runs after an agent run and before the next prompt, with a final request-time check. At the configured threshold, URI Agent asks the model to summarize older history and appends a checkpoint. Replay then contains:

```text
frozen system prompt
+ checkpoint summary of older history
+ recent history retained at a valid message boundary
+ events after the checkpoint
```

Summary generation uses a dedicated prompt without registered tools and treats conversation content as untrusted data. Each tool result contributes at most a 2,000-character head-and-tail preview; an earlier checkpoint receives at most one quarter of the input token budget; and total input is bounded against the remaining context, prioritizing the newest complete messages when necessary. Output is capped at 80% of the configured reserve. The session's frozen system prompt remains unchanged.

Compaction normally retains complete recent user turns. If one tool-heavy turn exceeds the budget, it may summarize the older prefix while keeping a suffix that never starts with a tool result. Original events remain in SQLite.

Provider overflow errors, responses reporting input beyond the context window, and recoverable `length` stops can trigger one forced compaction-and-retry for the current turn. A `length` stop is recoverable when reported output is below the catalog output limit, or when output usage is unavailable and reported input has reached 99% of the context window. A usable response is preserved before compaction; failed or truncated output does not enter replay. This recovery has a separate retry budget and cannot loop indefinitely.

Run `:compact` to request a checkpoint manually; it fails when too little completed history is available. Automatic compaction and its reserve and retained-history budgets are configured in [`settings.json`](configuration.md#settings-fields-and-precedence).

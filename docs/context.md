# Context

## Frozen startup context

A new session builds its system prompt from:

- the text-mode operating prompt;
- the selected tool names and descriptions;
- the project's `AGENTS.md`, when present.

The complete prompt is frozen in the session's `session_context` event before
the first model request. Resuming a session reuses that frozen prompt even if
`AGENTS.md` or the tool registry changed later. A new session picks up current
files.

`AGENTS.md` content is wrapped in `<project_rule_md>` and identified as project
instructions. Files are read once at startup; UAT does not rescan during the
session.

## Tools and prompt size

The model sees only selected tools. The simple spec selects:

```text
read, write, edit, bash, search, notes, steer, codemode, mcp, apply_patch
```

Tool definitions are part of every request and therefore part of the measured
context. Adding tools is not free. Prefer extending `codemode` or an MCP server
over adding another always-visible tool.

## Notes

`notes` is the durable working-state channel. Note events live in the session
log independently of model history, so they remain readable after a summary
checkpoint, a restart, or a connection from another session that reads the same
session record.

Use notes for:

- the current goal and constraints;
- decisions and rejected alternatives;
- exact file paths and commands that matter;
- verified results and remaining work.

Do not use notes as a transcript copy. Keep entries short and replace stale
entries instead of accumulating duplicates.

The note budget is a percentage of the context available after the base prompt
and current usage. A hard budget rejects writes that would exceed it; a warning
budget appears in the note index first. Delete or consolidate stale notes when
the warning appears.

## Search and history

`search` reads the live workspace. It does not depend on session history and
does not index previous conversations.

UAT does not expose a history-search tool. If a later session needs prior work,
the earlier session's notes are the compact handoff; the caller can also pass
relevant text into the prompt or write a file for the next session to read.

## Context checkpoints

When automatic compaction is enabled, UAT estimates the next request size. Near
the model window it asks the model to summarize older history, then replaces
that history with the summary and retains recent turns. The summary prompt treats
the conversation as untrusted data and asks for goals, decisions, paths,
verification, open questions, and next steps.

Project instructions, the selected tools, notes, and the session record remain
outside the replaced history. Compaction does not delete session events; it
changes the model-visible history.

## Custom instructions

Project instructions come from `<project>/AGENTS.md`. A session can also be
created with an appended or replaced system prompt through the library API.
There are no Skills, protocol help pages, or URI context routes in the current
mode.

# Terminal interface and sessions

URI Agent presents one conversation surface with floating controls for composition, commands, settings, selection, and an embedded terminal. It has no Browse, Insert, or Detail modes and no slash-command syntax.

## Startup and conversation surface

Startup may show a short animated splash before the conversation. An empty conversation keeps the centered animated brand and shows only the working directory and active provider/model with its thinking effort below it, followed by a locally centered compose/command/help key hint. If no usable model is configured, the provider/model line prompts for `:login`. Usage, context pressure, Git branch, and extension status are omitted from this welcome state.

After the first record appears, the transcript uses the available content area and a minimal footer stays on the bottom row. It keeps the active model and effort at the left edge and the animated context meter plus `percentage/context-window-size` at the right edge. Activity such as thinking or tool execution appears immediately above the footer. Branding, project, branch, token, extension, separator, and shortcut-hint details are omitted from the compact row. Click the footer, press `F4`, or run `:status` to open the bottom-anchored project, session, usage, and extension panel, where the model row also includes effort.

User prompts use a low-contrast full-width band, while assistant responses remain unboxed and read like a document. Completed reasoning and tool calls collapse to compact semantic summaries; assistant responses remain expanded up to the transcript preview limit. Select a block with the arrow keys or mouse and press `Enter` to expand, fold, or open its full document. Reasoning remains in the conversation instead of moving to a separate mode.

## Composer and commands

Press `i` to open the rounded, bottom-anchored composer. An empty composer shows a placeholder; while a turn is running, its muted state explains that another turn cannot be submitted:

| Key | Action |
| --- | --- |
| `Enter` | Send the request |
| `Shift+Enter` | Insert a newline |
| `Ctrl+Enter` or `Ctrl+J` | Insert a newline |
| `Esc` | Close the composer and preserve the draft |

The terminal cursor is placed at the text caret so IME candidate windows can follow the active insertion point. Opening the composer pauses interface animation.

Press `:` from the conversation to open the command panel. Type to filter registered command names and aliases, use `Tab` or `Shift+Tab` to complete and cycle matching commands, use the arrow keys or mouse to choose a result, press `Enter` to run it, and press `Esc` to close it. The unfiltered panel shows canonical names only. A matching alias replaces the canonical name in search results, so typing `t` can show `:thinking` for the `:effort` command; completing an alias inserts its canonical command name. Commands that need a value open a selector or a separate input float. Search text filters the panel; it is not a secondary command syntax.

Core commands are registered through `CommandRegistry`:

| Command | Purpose |
| --- | --- |
| `:insert` | Open the composer |
| `:copy` | Copy the current selection or visible panel through OSC52 |
| `:tasks` | Inspect and cancel managed protocol work |
| `:protocols` | List registered read and exec routes |
| `:status` | Show project, model, usage, and extension status |
| `:model` | Search runnable models |
| `:effort` | Select thinking effort supported by the active model |
| `:settings` | Inspect and edit active settings |
| `:login`, `:logout` | Manage provider credentials |
| `:resume`, `:new` | Switch project sessions or create one |
| `:compact` | Request a context checkpoint after usage exceeds 20% |
| `:set-terminal`, `:terminal` | Configure and open the embedded terminal |
| `:help` | Show the active commands and keymap |
| `:quit` or `:q` | Exit URI Agent |

Extensions register commands through the same registry, so they appear in the panel, help, and key-bindable action set without TUI-specific routing.

## Default navigation

| Surface | Useful defaults |
| --- | --- |
| Conversation | `Up`/`Down` select, `Enter` open/fold, `PageUp`/`PageDown` page, `Home`/`End` jump |
| Row filters | `r` reasoning, `t` tools, `h` user messages, `Esc` clear filter |
| Global | `F1` help, `F2` settings, `F3` models, `F4` status, `Ctrl+P` protocols, `Ctrl+T` tasks |
| Copy | `Ctrl+Shift+C` copy |

Arrow keys and mouse input are first-class. Selection wraps from the last item to the first and from the first item to the last in every selectable list. `j` and `k` exist as optional aliases on the main and several list surfaces, but defaults and help do not require Vim knowledge.

URI Agent ignores `Ctrl+C` on its own surfaces. Exit through `:quit` or `:q` instead.

`F1` and `:help` are more authoritative than this summary because they reflect loaded keymap overrides and registered extension commands.

## Layered keymap

Key bindings are loaded in this order:

```text
built-in defaults
< <config>/keymap.rhai
< <project>/.uri-agent/keymap.rhai
```

Later files override earlier mappings. Rhai files call `map` and `unmap`:

```rhai
map("main", "x", "copy");
unmap("main", "j");
map("composer", "ctrl+j", "newline");
```

Bindings belong to surfaces such as `global`, `main`, `composer`, `command`, `list`, `selector`, `settings`, `models`, `document`, `selection`, and `terminal`. A surface binding is checked before a global binding.

Configurable actions must go through the keymap. New commands that should be available from the command panel or key bindings must go through `CommandRegistry`; do not add a modeless hard-coded shortcut as a separate path.

## Embedded terminal

`:set-terminal` stores the command used by `:terminal`, such as `bash` or `pwsh -NoLogo`. The `URI_AGENT_TERMINAL` environment variable can override the stored command for an invocation.

`:terminal` opens that command in a PTY float rooted at the project directory. Terminal input, including `Ctrl+C`, is forwarded to the terminal program; resize, mouse events, and process exit are handled by the embedded terminal layer. Press `Esc` twice within 500 milliseconds to close the float; a single `Esc` is sent to the running terminal program.

Ordinary clicks and drags are sent to the terminal application. Hold `Shift` while dragging to select rendered text. `Ctrl+Shift+C` copies the selection through OSC52.

Read-only URI Agent surfaces use direct drag selection without Shift. Terminal restoration, mouse selection, and OSC52 copy must remain functional on normal exits and error paths.

## Image attachments

For a model whose catalog `input` includes `image`, add a standalone `@path` argument to the composer text:

```text
Describe @screenshots/error.png and suggest a fix.
```

URI Agent recognizes PNG, JPEG, GIF, and WebP extensions, validates the file signature, and adds the binary image as multimodal user content. The original text, including the `@path`, remains part of the user message.

Relative paths resolve from the project. Absolute paths are accepted only when their canonical location remains inside the canonical project directory. Symlink escapes are rejected. If the active model is text-only, a request containing a recognized image attachment fails explicitly.

## Sessions and context

Sessions preserve the conversation and the exact startup context used by the model.

### Session storage and project boundaries

Sessions are append-only and stored in SQLite:

```text
<platform-data-dir>/uri-agent/sessions.db
```

If a platform data directory cannot be determined, URI Agent falls back to `<project>/.uri-agent/sessions.db`.

URI Agent keeps a new session in memory until its first user message is accepted, then writes the frozen context, queued startup events, and user message in one transaction. Opening and closing the application without sending a message does not create a session record.

The canonical startup directory is the project boundary and is recorded with every session. Session selection cannot cross it:

- a normal launch starts a new session;
- `--continue-session` resumes the most recently updated session for this project;
- `--session <id>` resumes that ID only if it belongs to this project;
- `--session latest` selects the same project-scoped latest session;
- `:resume` lists sessions for the current project, including each session's model and configured effort.

`Esc` keeps draft text in the composer. URI Agent writes the current draft to SQLite when the TUI exits or switches sessions. Before the first message, it stores the draft separately by project so preserving a draft does not create an empty session record.

### Frozen startup context

A new session stores a `SessionContext` event containing the complete generated system prompt and selected Skill snapshots. Resume reuses that event. It does not regenerate the prompt or rebind Skills from the current filesystem layout.

This makes model replay stable across application restarts and configuration changes. The detailed Skill rules are in [Frozen session behavior](protocols.md#frozen-session-behavior).

### Append-only events

User messages, model messages, tool calls and results, usage, notices, errors, task notices, turn boundaries, and compaction checkpoints are appended as events. Existing events are not rewritten or deleted during normal operation or context compaction.

Provider tool-call identity is preserved in model history so resumed tool conversations remain valid for the selected backend.

### Context compaction

URI Agent estimates replay size against the selected model's context window. Before an overflowing request, it may ask the model for a durable summary of older history and append a compaction checkpoint. Replay then uses:

```text
frozen system prompt
+ checkpoint summary of older history
+ complete recent user turns
+ events after the checkpoint
```

Summary generation uses a dedicated compaction system prompt and does not
expose registered tools. Normal execution resumes with the session's frozen
system prompt unchanged.

Compaction boundaries are complete user turns. A tool call is never separated from its result. Original events remain available in SQLite even when model replay uses the checkpoint.

Run `:compact` to request an earlier checkpoint once estimated context usage is
strictly above 20%. The command also fails clearly when there is not enough
completed history to summarize.

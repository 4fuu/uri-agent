# Terminal interface and sessions

URI Agent presents one conversation surface with floating controls for composition, commands, settings, selection, and an embedded terminal. It has no Browse, Insert, or Detail modes and no slash-command syntax.

## Startup and conversation surface

Startup may show a short animated splash before the first conversation. Session switches keep the terminal interface active; `:new` opens a fresh welcome view directly instead of replaying the splash. An empty conversation keeps the centered animated brand and shows only the working directory and active provider/model with its thinking effort below it, followed by a locally centered compose/command/help key hint. If no usable model is configured, the provider/model line prompts for `:login`. Usage, context pressure, Git branch, and extension status are omitted from this welcome state.

After the first record appears, the transcript uses the available content area and a minimal, low-contrast footer stays on the bottom row. It keeps the active model and effort at the left edge and the context meter plus `percentage/context-window-size` at the right edge. The meter animates during live activity and stays static when idle. It uses provider-reported usage when available. `≈` appears only when an idle conversation remains on an estimate or an API baseline plus estimated trailing messages, such as after an interrupted response; live estimates omit the marker. `?` means a compaction invalidated the old baseline and no new provider usage has arrived. During live activity such as thinking or tool execution, the footer expands upward by one row; activity begins at the model's usual trailing position on that upper row. Fixed statuses such as pending messages or an active filter remain visible immediately above the footer. Transient notifications overlay the welcome or conversation instead of resizing it. They stack above fixed statuses, with each newer notification above the earlier ones, then expire independently; all bottom notices wrap to the available width. Transient notifications remain visible longer as their character count grows, within a bounded reading interval. Branding, project, branch, token, extension, separator, and shortcut-hint details are omitted from the compact row. Click the footer, press `F4`, or run `:status` to open the bottom-anchored project, session, usage, and extension panel, where the model row also includes effort.

The base conversation surface uses the terminal's default background. User prompts use a padded teal band distinct from the footer, while assistant responses remain on the terminal background. Both use the full transcript width without decorative prefixes or selection rails, so copied text contains only message content. Assistant responses render as Markdown documents, including highlighted headings at every level, emphasis, links, lists, quotes, code blocks, and responsive tables. User prompts and final assistant responses never fold. A follow-up prompt has one blank row between the previous turn's final response and its teal top padding. The teal padding supplies the blank row between a prompt and the first reasoning, tool, or assistant block. While a turn runs, intermediate assistant text, reasoning, and tools remain visible in event order. When the turn finishes, everything before its final response folds into one `Process` row, separated from the padded user prompt by one additional blank row; a turn with no intermediate blocks has no empty process row. The final response, or the terminal error for a failed turn, remains visible after one blank row. Restored sessions reconstruct the same default-collapsed process row from their persisted turn boundaries.

Select a process row with the arrow keys or mouse and press `Enter` to reveal or hide its intermediate timeline. Each completed reasoning or tool row inside an expanded process remains independently foldable to a compact semantic summary; tool rows describe the routed URI, command, patch targets, and result instead of exposing raw argument JSON. Live reasoning expands by default, follows its newest lines when its preview overflows, folds when the model advances to text or a tool call, and stays folded when the user folds it while deltas are still streaming. Process blocks align their semantic icons and indented details to the same transcript edges, and retain their selected-row background. A single click performs the same fold action immediately; press `o` or right-click to open the full document at any time, including while output is streaming. Conversation search and the reasoning/tool jump keys automatically reveal a folded process when they target one of its children. A left click outside dismisses the composer, delivery chooser, command panel, status, help, protocols, tasks, model picker, extension panels, full documents, and generic selectors. The composer preserves its draft, while the delivery chooser returns to the composer. Settings, text prompts, OAuth, and the embedded terminal require an explicit close so an outside click cannot discard edits or stop active work. Reasoning remains in the conversation instead of moving to a separate mode.

## Composer and commands

Press `Space` to open the rounded, bottom-anchored composer. An empty composer shows a placeholder. The composer remains editable while a turn is running and shows messages waiting for delivery:

| Key | Action |
| --- | --- |
| `Enter` | Send when idle; while running, choose Queue or Guidance |
| `Shift+Enter` | Insert a newline |
| `Ctrl+Enter` or `Ctrl+J` | Insert a newline |
| `Alt+Up` | Restore the latest undelivered Queue or Guidance message to the draft |
| `Alt+Enter` | Upgrade the latest queued message to Guidance |
| `Up`/`Down` | Move between lines; at the first or last line, move to the start or end of the draft |
| `Home`/`End`, `Ctrl+A`/`Ctrl+E` | Move to the start or end of the current line |
| `Ctrl+Home`/`Ctrl+End` | Move to the start or end of the draft |
| `Ctrl+Left`/`Ctrl+Right`, `Alt+Left`/`Alt+Right` | Move by word |
| `Ctrl+Backspace`/`Ctrl+Delete` | Delete the previous or next word |
| `Tab` | Insert the selected `@` or `@@` completion |
| `Ctrl+V`, or `Cmd+V` with the macOS key style | Paste text, or insert a clipboard image when the terminal forwards the key to URI Agent |
| `Alt+V` | Explicitly insert the current clipboard image |
| `Ctrl+Z`/`Ctrl+Shift+Z`, or `Cmd+Z`/`Cmd+Shift+Z` with the macOS key style | Undo or redo an edit |
| `Esc` | Close the composer and preserve the draft |

Click to place the caret, or drag across the draft to select editable text. `Ctrl+C`, `Ctrl+Shift+C`, `Cmd+C`, or right-click copies the selected draft through OSC52.
The terminal cursor is placed at the text caret so IME candidate windows can follow the active insertion point. Opening the composer pauses interface animation.
Long logical lines soft-wrap at the visible composer edge; only the newline shortcuts above insert a newline into the submitted text.

Type `@` at the start of a token to list project files and insert an `@file://<path>` reference. Type `@@` to list saved sessions from the current project and insert a stable `@@<session-id>` reference. `Up` and `Down` select an open candidate, `Tab` or `Enter` inserts it, `Esc` closes the candidates without closing the composer, and mouse selection is supported. File and session matching run through linked completion providers; the composer only handles generic replacement ranges and candidates.

Pressing `@` on the conversation surface opens the composer with the same file completion, so reference entry has one interaction path.

While a turn is running, `Enter` opens a keyboard- and mouse-selectable delivery float. **Guidance** is appended as user input after the current assistant response and its tool calls finish, immediately before the next model request. It does not interrupt an in-flight model request or tool operation. **Queue** waits until the active agent run reaches its terminal response, then starts a new user turn. Guidance takes priority over queued follow-ups at a shared boundary.

The composer preview contains only messages that have not been taken for delivery. `Alt+Up` removes the newest such message and prepends it to the current draft; `Alt+Enter` changes the newest queued follow-up to Guidance. Once the runtime takes a message at its delivery boundary, it leaves the preview and can no longer be restored or upgraded. If URI Agent exits first, all still-undelivered messages are restored ahead of the saved draft.

While a turn is running, press `Esc` twice within 500 milliseconds to interrupt its current model request or tool operation. The first press keeps the active surface's normal `Esc` behavior, such as closing a float, preserving the composer draft, or clearing a row filter, and a fixed status above the footer prompts for the second press. The interrupted turn records an error and a complete turn boundary so another request can be sent normally. The embedded terminal keeps its separate double-`Esc` behavior: it closes the terminal float instead of interrupting the model turn.

Press `:` from the conversation to open the command panel. Type to fuzzy-filter registered command names, aliases, and descriptions; use `Tab` or `Shift+Tab` to complete and cycle matching commands, use the arrow keys or mouse to choose a result, press `Enter` to run it, and press `Esc` to close it. The unfiltered panel shows canonical names only. A matching alias replaces the canonical name in search results, so typing `t` can show `:thinking` for the `:effort` command; description matches keep the canonical name, and completing any match inserts its canonical command name. Commands that need a value open a selector or a separate input float. Search text filters the panel; it is not a secondary command syntax.

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
| `:search` or `:find` | Search text already shown in the current conversation and jump to a matching block |
| `:compact` | Request a context checkpoint when older completed history is available |
| `:set-env` | Add or replace an Agent environment variable through a masked value prompt |
| `:set-terminal`, `:terminal` | Configure and open the embedded terminal |
| `:help` | Show the active commands and keymap |
| `:quit` or `:q` | Exit URI Agent |

`:login` includes model providers plus the Parallel and Exa web providers. Selecting Parallel or Exa opens a masked API-key prompt with that provider's key-management URL. Saved credentials take effect immediately for search and page extraction in the built-in `https` protocol and appear in `:logout` like other stored credentials.

Conversation search includes user, assistant, reasoning, tool, notice, compaction, and error text currently loaded in the transcript. Type to filter the results, use the arrow keys to choose one and press `Enter`, or click a result to return to that block. It is unavailable before the conversation has any text and while the `:resume` session selector is open.

Extensions register commands through the same registry, so they appear in the panel, help, and key-bindable action set without TUI-specific routing.

### Agent environment manager

`:set-env` opens separate name and masked-value prompts, then saves the variable globally. The **Agent environment** row in `:settings` shows only the number of configured variables. Press `Enter` or double-click the row to open the manager; values are never shown in its list. In the manager, `Enter` or a double-click replaces the selected value, `Ctrl+N` adds a variable, `Delete` removes the selected variable, and `Esc` returns to Settings.

Saved values apply to future Agent `bash` and `pwsh` commands without a restart. They are not added to `:terminal`; see [Agent environment](configuration.md#agent-environment) for storage, scope, and plugin access.

## Default navigation

| Surface | Useful defaults |
| --- | --- |
| Conversation | `@` open the composer and list file references, `Alt+V` insert a clipboard image, `Up`/`Down` select, `Ctrl+Up`/`Ctrl+Down` scroll, `Enter` expand/fold, `o` open full document, `PageUp`/`PageDown` page, `Home`/`End` jump |
| Row filters | `r` reasoning, `t` tools, `h` user messages, `Esc` clear filter |
| Global | Double `Esc` interrupts a running turn; `F1` help, `F2` settings, `F3` models, `F4` status, `Ctrl+,` settings, `Ctrl+P` protocols, `Ctrl+T` tasks; the macOS key style also accepts `Cmd+,` |
| Copy | `Ctrl+C` or right-click copies an active selection; without a selection, right-click opens a reasoning or tool block's full document; `Ctrl+Shift+C` copies the selection or visible surface; `Cmd+C` is accepted when the terminal forwards it |

Arrow keys and mouse input are first-class. The mouse wheel and `Ctrl+Up`/`Ctrl+Down` scroll the conversation viewport without changing the selected block. Manual scrolling can move the final transcript row up to the middle of the viewport; this virtual tail space is not persisted conversation content. New output follows the real content bottom until the user scrolls away. When the viewport is showing content above the live tail, the activity row shows a padded `↓ bottom` mouse button above and slightly inset from the context meter. Without an activity row, a padded `↓` button floats at the same inset without taking layout space. The button stays hidden in virtual space past the live tail. Click either button or press `End` to resume following the tail. Keyboard navigation keeps an off-screen destination visible and centers it when the transcript has enough room. Selection wraps from the last item to the first and from the first item to the last in every selectable list. `j` and `k` exist as optional aliases on the main and several list surfaces, but defaults and help do not require Vim knowledge.

On URI Agent surfaces, `Ctrl+C` copies an active selection and is otherwise ignored. Exit through `:quit` or `:q` instead.

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

Key names are parsed into a normalized key stroke when the keymap loads. Modifier names are case-insensitive; `control` aliases `ctrl`, `option` aliases `alt`, and `cmd` or `command` aliases `super`. Invalid key names fail startup with the owning keymap file in the error instead of creating an unreachable binding.

All visible action hints are resolved back through the effective keymap after global and project overrides. The default text style renders labels such as `Ctrl+R`, `Shift+Enter`, and `Alt+Up`. The macOS style renders the same strokes as `⌃R`, `⇧↩`, and `⌥↑`, and renders `super` as Command (`⌘`). This keeps panel titles, composer guidance, pending-message controls, transcript actions, and `F1` help synchronized with user overrides.

Configure the style with `keyDisplay` in `settings.json` or `URI_AGENT_KEY_DISPLAY`; see [Settings fields and precedence](configuration.md#settings-fields-and-precedence). `auto` selects macOS symbols when URI Agent runs locally on macOS. For a macOS terminal connected to URI Agent on Linux or Windows, select `macos` explicitly because the remote process cannot reliably detect the client operating system. The macOS style adds Command aliases for Settings, paste, undo, and redo while retaining all portable bindings. Command shortcuts work only when the terminal forwards them.

Bindings belong to surfaces such as `global`, `main`, `composer`, `command`, `list`, `selector`, `settings`, `environment`, `models`, `document`, `selection`, and `terminal`. A surface binding is checked before a global binding.

Configurable actions must go through the keymap. New commands that should be available from the command panel or key bindings must go through `CommandRegistry`; do not add a modeless hard-coded shortcut as a separate path.

## Embedded terminal

`:set-terminal` stores the command used by `:terminal`, such as `bash` or `pwsh -NoLogo`. The `URI_AGENT_TERMINAL` environment variable can override the stored command for an invocation.

`:terminal` opens that command in a PTY float rooted at the project directory. It inherits the URI Agent process environment but deliberately does not receive values from the Agent environment manager. Terminal input, including `Ctrl+C` when no URI Agent selection is active, is forwarded to the terminal program; resize, mouse events, and process exit are handled by the embedded terminal layer. Press `Esc` twice within 500 milliseconds to close the float; a single `Esc` is sent to the running terminal program.

Ordinary clicks and drags are sent to the terminal application. Hold `Shift` while dragging to select rendered text. `Ctrl+C`, `Ctrl+Shift+C`, or right-click copies that selection through OSC52. On macOS, `Cmd+C` also works when the terminal forwards the Command modifier; terminals that reserve the shortcut require `Ctrl+Shift+C`.

User prompts, assistant responses, and read-only floats support direct drag selection without Shift. Hold `Shift` while dragging reasoning or tool blocks so their ordinary clicks remain available for expand and open actions. Terminal restoration, mouse selection, and OSC52 copy must remain functional on normal exits and error paths.
On URI Agent surfaces, a copy shortcut copies the active selection and `Esc` clears it. Any other shortcut clears the selection and continues through normal key routing, so the command panel and global panels remain available while text is selected.

## Image attachments

Clipboard attachments live in the composer. Normal terminal paste inserts text and opens the composer when used from the conversation surface. When a terminal forwards `Ctrl+V` as a key event, URI Agent reads the system clipboard itself, prefers an image, and falls back to text. Because some terminals consume `Ctrl+V`, `Alt+V` is the reliable explicit image shortcut from either the conversation or composer surface; from the conversation it opens the composer first. Clipboard reads run in the background. Submitting is disabled until a read finishes, so an image cannot arrive after its intended message has already been sent.

Each clipboard image appears at the caret as an atomic `🖼 #N` chip. Arrow and word movement cross the whole chip instead of stopping inside it. `Backspace` immediately after a chip, `Delete` immediately before it, or deleting a selection that touches it removes the chip and its attachment together. `Esc` preserves the composer text and active image chips. On submission, active chips expand to `[Image #N]` markers in message order; only their corresponding images are included. Unsent clipboard images are process-local: their binary data is not written to SQLite and is discarded when the process exits or the user switches sessions. On restart, image chips and markers without binary attachments are removed from saved drafts.

For a model whose catalog `input` includes `image`, add a standalone `@file://<path>` argument to the composer text:

```text
Describe @file://screenshots/error.png and suggest a fix.
```

URI Agent encodes clipboard images as PNG. For paths, it recognizes PNG, JPEG, GIF, and WebP extensions, validates the file signature, and adds the binary image as multimodal user content. Clipboard and path images can be included in the same message. The original text, including each `@file://<path>`, remains part of the user message.

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

The built-in `sessions` extension separately provides read-only archive discovery. `@@` completion remains project-scoped, while model-facing discovery can use `scope: "all"` when the user asks for context from another project. Exact reads return a bounded linear event history; URI Agent sessions do not have Pi's branch or entry tree semantics. Archive reads never resume or modify a session.

Each session records its provider, model, and thinking effort. Model or effort changes append a settings event, and resume restores the latest session settings instead of applying the defaults for a new session.

`Esc` keeps draft text in the composer. URI Agent writes the current draft to SQLite when the TUI exits or switches sessions. Before the first message, it stores the draft separately by project so preserving a draft does not create an empty session record.

Switching sessions leaves an active turn and its undelivered messages attached to the original session. Exiting URI Agent cancels each active turn's in-flight wait and joins it after its interruption error and turn boundary are durable; any messages that were not taken for delivery return to that session's draft, so the user does not need to wait for the model to finish naturally.

### Frozen startup context

A new session stores a `SessionContext` event containing the complete generated system prompt and selected Skill snapshots. Resume reuses that event. It does not regenerate the prompt or rebind Skills from the current filesystem layout.

This makes model replay stable across application restarts and configuration changes. The detailed Skill rules are in [Frozen session behavior](protocols.md#frozen-session-behavior).

### Append-only events

User messages, model messages, model settings, tool calls and results, usage, notices, errors, task notices, turn boundaries, and compaction checkpoints are appended as events. Existing events are not rewritten or deleted during normal operation or context compaction.

The transcript event and model-replay event for one message commit in the same SQLite transaction. Streaming text and reasoning deltas are provisional TUI updates; the completed response replaces them after its durable boundary commits.

Provider tool-call identity is preserved in model history so resumed tool conversations remain valid for the selected backend.

### Model request retries

URI Agent retries transient failures for both normal model calls and context-summary calls. Each failure class has its own counter within one logical model call, so changing failure type does not consume another type's budget. A successful response or new model round resets all counters. The limits below count additional retries after the initial attempt:

| Failure | Retries | Fallback backoff before jitter |
| --- | ---: | --- |
| Rate limit (`429`) | 6 | 1s exponential, capped at 30s |
| Network or stream transport failure | 5 | 500ms exponential, capped at 8s |
| Server failure (`5xx`) | 5 | 1s exponential, capped at 15s |
| Timeout or `408` | 4 | 1s exponential, capped at 10s |
| Request conflict (`409`) | 4 | 500ms exponential, capped at 8s |
| Empty completed response | 4 | 1s exponential, capped at 8s |

Fallback delays include up to 25% jitter. When a retryable response supplies `Retry-After` or `retry-after-ms`, that delay takes precedence and is capped at 60 seconds. Authentication, billing or quota, other client (`4xx`), malformed-request, and unclassified failures settle immediately.

Because the counters are independent, alternating failure classes can consume up to 28 additional attempts in one logical model call. There is no separate aggregate attempt or elapsed-time ceiling.

Each retry is recorded as a visible session event containing its reason, delay, and retry count. Provisional text and reasoning from the failed stream are cleared before the next attempt and never enter model replay. Double `Esc` interrupts either an active request or its retry delay.

### Model and tool loop

A turn has no fixed tool-round limit. It continues until the model returns no tool call, the user interrupts it, or an unrecoverable model, persistence, or runtime error occurs.

URI Agent detects consecutive model responses that each contain exactly one tool call with the same tool name and canonical JSON arguments. On the fifth identical call, it appends a hidden, durable redirect before the next model request, asking the model to change arguments, use another tool, or finish with its findings. The redirect is part of persisted model replay but is not displayed as user input and does not terminate the turn. A different call, no call, or multiple calls resets the sequence. Repeated reads of a managed-task status URI are exempt because polling the same route is expected.

### Context compaction

URI Agent measures replay size against the selected model's context window. The most recent valid ordinary API response is authoritative; messages after that response are estimated until the next response. A full estimate is used before the first response and includes the frozen system prompt, registered tool definitions, message content, and bounded image costs. The coarse estimate counts four ASCII characters or one non-ASCII character per token and assigns 1,200 tokens to each image regardless of its encoded payload size. A compaction invalidates usage from the earlier replay, so context remains unknown until the next ordinary API response.

Before each provider request, the catalog output limit is capped to the estimated context room after a fixed 4,096-token safety margin, with a minimum request of one output token. This request cap is separate from the configurable compaction reserve.

Automatic compaction is checked after an agent run and before accepting the next prompt, with a final request-time check as a fallback. When the configured threshold is crossed, URI Agent asks the model for a durable summary of older history and appends a compaction checkpoint. Replay then uses:

```text
frozen system prompt
+ checkpoint summary of older history
+ recent history retained at a valid message boundary
+ events after the checkpoint
```

Summary generation uses a dedicated compaction system prompt and does not expose registered tools. Conversation messages are serialized as untrusted data. Each tool result contributes at most a 2,000-character head-and-tail preview, an earlier checkpoint receives at most one quarter of the summary input token budget, and the total input is bounded against the remaining context. If that input is still too large, the newest complete messages take priority over the older prefix. Summary output is capped at 80% of the configured reserve. Normal execution resumes with the session's frozen system prompt unchanged.

Compaction normally retains complete recent user turns. If one tool-heavy turn exceeds the retention budget, URI Agent may summarize its older prefix and retain its recent suffix. A retained suffix never starts with a tool result, so a tool call is not separated from its result. Original events remain available in SQLite even when model replay uses the checkpoint.

Explicit provider errors, successful responses whose reported input exceeds the context window, and recoverable `length` stops participate in overflow handling. A `length` stop is considered recoverable when reported output is below the catalog output limit, or when output usage is unavailable and reported input has reached 99% of the context window. A usable successful response is preserved and followed by compaction. A failed or truncated response can make one forced compaction-and-retry attempt for that user turn and does not enter replay before that retry. This recovery has its own budget: it neither consumes nor resets transient-failure counters. A second overflow settles without retrying indefinitely.

Run `:compact` to request an earlier checkpoint at any context usage. The command fails clearly when there is not enough completed history to summarize. Automatic compaction can be disabled and its reserve and retained-history budgets changed through `settings.json`; see [Settings fields and precedence](configuration.md#settings-fields-and-precedence).

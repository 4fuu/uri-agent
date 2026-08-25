# Terminal interface

URI Agent presents one conversation surface with floating controls for composition, commands, settings, selection, and an embedded terminal. It has no Browse, Insert, or Detail modes and no slash-command syntax.

## Conversation surface

Startup may show a short splash. An empty conversation then shows the project, active model and thinking effort, and compose, command, and help hints; if no model is configured, it prompts for `:login`. `:new` returns directly to this welcome view.

After the first record, a compact footer shows the active model, effort, and context usage. The context meter animates during live activity, stays static when idle, and uses provider-reported usage when available. `≈` appears only when an idle conversation remains on an estimate or an API baseline plus estimated trailing messages; live estimates omit it. `?` means compaction invalidated the previous baseline and no new provider usage has arrived. Activity and fixed statuses appear above the footer, while transient notifications overlay the current view and expire independently. Click the footer, press `F4`, or run `:status` for project, session, usage, and extension details.

User prompts use a teal band; assistant responses render as Markdown on the terminal background. Both occupy the transcript width without decorative prefixes, so copied text contains only message content. During a turn, intermediate text, reasoning, and tools remain visible in event order. At completion they fold into one `Process` row, while the final response or terminal error stays visible. Restored sessions reconstruct the same collapsed form.

Select a process, reasoning, or tool row with the keyboard or mouse and press `Enter` or click to fold or expand it. Tool summaries describe the routed URI, command, patch targets, and result instead of raw argument JSON. Press `o` or right-click to open the full document, including during streaming. In that document, press `c` to copy the complete contents. Search and reasoning/tool jump actions reveal a folded parent automatically.

If the model calls a protocol before reading that protocol's help, the blocked
tool row uses a purple header. Its error detail keeps the normal error color.

A click outside dismisses read-only and navigational floats, including the composer, command panel, selectors, status, help, protocols, tasks, and documents. The composer preserves its draft. Settings, text prompts, OAuth, and the embedded terminal require an explicit close so an outside click cannot discard edits or stop work.

## Composer and commands

Press `Space` to open the rounded, bottom-anchored composer. An empty composer shows a placeholder. The composer remains editable while a turn is running and shows messages waiting for delivery:

| Key | Action |
| --- | --- |
| `Enter` | Send when idle; while running, choose Queue or Guidance |
| `Shift+Enter` | Insert a newline; Windows consoles never report Shift with Enter, so Windows shows and expects `Ctrl+Enter` instead |
| `Ctrl+Enter` or `Ctrl+J` | Insert a newline |
| `Alt+Up` | Restore the latest undelivered Queue or Guidance message to the draft |
| `Alt+Enter` | Upgrade the latest queued message to Guidance |
| `Up`/`Down` | Move between lines; at the first or last line, move to the start or end of the draft |
| `Home`/`End`, `Ctrl+A`/`Ctrl+E` | Move to the start or end of the current line |
| `Ctrl+Home`/`Ctrl+End` | Move to the start or end of the draft |
| `Ctrl+Left`/`Ctrl+Right`, `Alt+Left`/`Alt+Right` | Move by word |
| `Ctrl+Backspace`/`Ctrl+Delete` | Delete the previous or next word |
| `Tab` | Insert the selected `@` or `@@` completion |
| `Ctrl+V`, or `Cmd+V` with the [macOS key style](terminal.md#layered-keymap) | Paste text, or insert a clipboard image when the terminal forwards the key to URI Agent |
| `Alt+V` | Explicitly insert the current clipboard image |
| `Ctrl+Z`/`Ctrl+Shift+Z`, or `Cmd+Z`/`Cmd+Shift+Z` with the macOS key style | Undo or redo an edit |
| `Esc` | Close the composer and preserve the draft |

Click to place the caret, or drag across the draft to select editable text. `Ctrl+C`, `Ctrl+Shift+C`, `Cmd+C`, or right-click copies the selected draft through OSC52.
The terminal cursor is placed at the text caret so IME candidate windows can follow the active insertion point. Opening the composer pauses interface animation unless its completion popup is open.
Long logical lines soft-wrap at the visible composer edge. `Enter` sends; the newline shortcuts above insert a newline. A multi-line paste, including one that a terminal reports as ordinary key presses, is inserted as draft text and does not send.

Type `@` at the start of a token to list project files and insert an `@file://<path>` reference. Type `@@` to list saved sessions from the current project and insert a stable `@@<session-id>` reference. `Up` and `Down` select an open candidate, `Tab` or `Enter` inserts it, `Esc` closes the candidates without closing the composer, and mouse selection is supported. File and session matching run through linked completion providers; the composer only handles generic replacement ranges and candidates.

Pressing `@` on the conversation surface opens the composer with the same file completion, so reference entry has one interaction path.

While a turn is running, `Enter` opens a keyboard- and mouse-selectable delivery float. **Guidance** is appended as user input after the current assistant response and its tool calls finish, immediately before the next model request. It does not interrupt an in-flight model request or tool operation. **Queue** waits until the active agent run reaches its terminal response, then starts a new user turn. Guidance takes priority over queued follow-ups at a shared boundary.

The composer preview contains only messages that have not been taken for delivery. `Alt+Up` removes the newest such message and prepends it to the current draft; `Alt+Enter` changes the newest queued follow-up to Guidance. Once the runtime takes a message at its delivery boundary, it leaves the preview and can no longer be restored or upgraded. If URI Agent exits first, all still-undelivered messages are restored ahead of the saved draft.

While a turn is running, press `Esc` twice within 500 milliseconds to interrupt its current model request or tool operation. The first press keeps the active surface's normal `Esc` behavior, such as closing a float, preserving the composer draft, or clearing a row filter, and a fixed status above the footer prompts for the second press. The interrupted turn records an error and a complete turn boundary so another request can be sent normally. The embedded terminal keeps its separate double-`Esc` behavior: it closes the terminal float instead of interrupting the model turn.

Press `:` from the conversation to open the command panel. Type to fuzzy-filter registered command names, aliases, and descriptions; use `Tab` or `Shift+Tab` to complete and cycle matching commands, use the arrow keys or mouse to choose a result, press `Enter` to run it, and press `Esc` to close it. The unfiltered panel shows canonical names only. A matching alias replaces the canonical name in search results, so typing `t` can show `:thinking` for the `:effort` command; description matches keep the canonical name, and completing any match inserts its canonical command name. Commands that need a value open a selector or a separate input float. Search text filters the panel; it is not a secondary command syntax.

Selectable single-line rows keep their columns and mouse targets stable. Overflowing unselected text ends with `…`; after a short pause, the selected text scrolls to reveal the hidden content and then returns. Multi-line panel bodies and details wrap instead.

Core commands are registered through `CommandRegistry`:

| Command | Purpose |
| --- | --- |
| `:insert` | Open the composer |
| `:copy` | Copy the current selection or visible panel through OSC52 |
| `:tasks` | Inspect and cancel managed protocol work |
| `:protocols` | List registered read and exec routes |
| `:status` | Show project, model, usage, and extension status |
| `:model` | Search runnable models |
| `:refresh-catalog` | Force-refresh and apply cloud model configurations |
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
| Document | `c` copy the complete contents, arrows or page keys scroll, `Esc` close |
| Row filters | `r` reasoning, `t` tools, `h` user messages, `Esc` clear filter |
| Global | Double `Esc` interrupts a running turn; `F1` help, `F2` settings, `F3` models, `F4` status, `Ctrl+,` settings, `Ctrl+P` protocols, `Ctrl+T` tasks; the macOS key style also accepts `Cmd+,` |
| Copy | `Ctrl+C` or right-click copies an active selection; without a selection, right-click opens a reasoning or tool block's full document; `c` in that document copies its complete contents; `Ctrl+Shift+C` copies the selection or visible surface; `Cmd+C` is accepted when the terminal forwards it |

Arrow keys and mouse input are first-class. The mouse wheel and `Ctrl+Up`/`Ctrl+Down` scroll the conversation viewport without changing the selected block. Manual scrolling can move the final transcript row up to the middle of the viewport; this virtual tail space is not persisted conversation content. New output follows the real content bottom until the user scrolls away. When the viewport is showing content above the live tail, the activity row shows a padded `↓ bottom` mouse button above and slightly inset from the context meter. Without an activity row, a padded `↓` button floats at the same inset without taking layout space. The button stays hidden in virtual space past the live tail. Click either button or press `End` to resume following the tail. Keyboard navigation keeps an off-screen destination visible and centers it when the transcript has enough room. Selection wraps from the last item to the first and from the first item to the last in every selectable list. `j` and `k` exist as optional aliases on the main and several list surfaces, but defaults and help do not require Vim knowledge.

On URI Agent surfaces, `Ctrl+C` copies an active selection and is otherwise ignored. Exit through `:quit` or `:q` instead.

`F1` and `:help` are more authoritative than this summary because they reflect loaded keymap overrides and registered extension commands.

# Terminal interface

URI Agent presents one conversation surface with floating controls for composition, commands, settings, selection, and an embedded terminal. It has no Browse, Insert, or Detail modes and no slash-command syntax.

Rendering is demand-driven. Continuous visual states, including the splash and empty-conversation wordmark, are presented at a 60 Hz target from a continuous wall-clock phase, while fully idle settled conversations do not redraw periodically. Cell graphics such as activity waves and wordmark shimmer distribute their transitions across presentation frames without changing their cycle duration. Finite-state spinner glyphs and marquee character steps necessarily repeat across presentation frames and retain their original timing instead of speeding up. Streaming and background updates are coalesced to the display cadence; direct input and resize events redraw immediately without postponing that cadence. Embedded terminal output explicitly wakes the interface even when no other activity is present.

## Conversation surface

Startup may show a short splash. An empty conversation then shows the project, active model and thinking effort, and compose, command, and help hints; if no model is configured, it prompts for `:login`. `:new` returns directly to this welcome view.

After the first record, a compact footer shows the active model, effort, and context usage. The context meter animates during live activity, stays static when idle, and uses provider-reported usage when available. `≈` appears only when an idle conversation remains on an estimate or an API baseline plus estimated trailing messages; live estimates omit it. `?` means compaction invalidated the previous baseline and no new provider usage has arrived. While model output streams, the activity row appends a smoothed `tok/s` estimate after its animation. The estimate uses a three-second window of visible words, CJK characters, and punctuation calibrated against provider output usage, and freezes while tools run. Once the complete turn is ready, the model and effort footer instead appends the visible-aligned provider output tokens divided by the summed first-output-to-completion time of its model responses. Hidden reasoning tokens are excluded unless reasoning text was visible; missing usage or timing suppresses the rate rather than showing a partial average. Activity and fixed statuses appear above the footer, while transient notifications overlay the current view and expire independently. Click the footer, press `F4`, or run `:status` for project, session, usage, the per-session diagnostic log path, and extension details.

User prompts use a teal band; assistant responses render as Markdown on the terminal background. Both occupy the transcript width without decorative prefixes, so copied text contains only message content. Visual soft wraps do not add line breaks to copied transcript text; intentional rendered line breaks remain. During a turn, intermediate text, reasoning, and tools remain visible in event order. At completion they fold into one `Process` row, while the final response or terminal error stays visible. Restored sessions reconstruct the same collapsed form.

When a restored session has a compaction checkpoint, the conversation initially loads the latest checkpoint and its complete containing turn plus the committed transcript tail. Scrolling upward loads older complete turns in bounded pages. The loaded history always remains one contiguous suffix; a single large turn can exceed the nominal page size rather than being split. Sessions without a checkpoint retain eager transcript restoration. `Home`, conversation search, and reasoning, tool, or user jumps load all older pages before applying their session-global behavior. `End` and tail following stay immediate. Resizing relayouts only the currently loaded blocks and does not fetch history.

Select a process, reasoning, or tool row with the keyboard or mouse and press
`Enter` or click to fold or expand it. Tool summaries describe the routed URI,
command, patch targets, and result instead of raw argument JSON. Press `o` or
right-click to open a Markdown-rendered full document, including during
streaming. Tool documents show status, target, meaningful input, and result or
error; commands, patches, edits, and dynamic output use fenced blocks without
changing their indentation. Internal call IDs and raw `CALL`/`RESULT` markers
are omitted. In that document, press `c` to copy the complete contents. Search
and reasoning/tool jump actions reveal a folded parent automatically.

If the model calls a protocol before reading that protocol's help, the blocked
tool row uses a purple header. Its error detail keeps the normal error color.

A click outside dismisses read-only and navigational floats, including the composer, command panel, selectors, status, help, protocols, tasks, and documents. The composer preserves its draft. Model Hub, Settings, text prompts, OAuth, and the embedded terminal require an explicit close so an outside click cannot discard a staged choice or stop work. A float never renders narrower than 60 columns while the terminal is wider; below that, it spans the full width without left and right borders or inner horizontal padding. Centered floats above that minimum keep half of the side margins their percentage split would otherwise leave empty.

## Composer and commands

Press `Space` to open the rounded, bottom-anchored composer. An empty composer shows a placeholder. The composer remains editable while a turn is running and shows messages waiting for delivery:

| Key | Action |
| --- | --- |
| `Enter` | Send when idle; while running, choose Queue or Steer |
| `Shift+Enter` | Insert a newline; Windows consoles never report Shift with Enter, so Windows shows and expects `Ctrl+Enter` instead |
| `Ctrl+Enter` or `Ctrl+J` | Insert a newline |
| `Alt+Up` | Restore the latest undelivered Queue or Steer message to the draft |
| `Alt+Enter` | Upgrade the latest queued message to Steer |
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

Click to place the caret, double-click to select a Unicode word, or drag across the draft to select editable text. `Ctrl+C`, `Ctrl+Shift+C`, `Cmd+C`, or right-click copies the selected draft through OSC52.
The terminal cursor is placed at the text caret so IME candidate windows can follow the active insertion point. Opening the composer pauses interface animation unless its completion popup is open.
Long logical lines soft-wrap at the visible composer edge. `Enter` sends; the newline shortcuts above insert a newline. A multi-line paste, including one that a terminal reports as ordinary key presses, is inserted as draft text and does not send.

Type `@` at the start of a token to list project files and insert an `@file://<path>` reference. Type `@@` to list saved sessions from the current project and insert a stable `@@<session-id>` reference. `Up` and `Down` select an open candidate, `Tab` or `Enter` inserts it, `Esc` closes the candidates without closing the composer, and mouse selection is supported. File and session matching run through linked completion providers; the composer only handles generic replacement ranges and candidates.

Pressing `@` on the conversation surface opens the composer with the same file completion, so reference entry has one interaction path.

While a turn is running, `Enter` opens a keyboard- and mouse-selectable delivery float. **Steer** follows Pi-style model-boundary delivery: it is appended as user input after the current assistant response and its tool calls finish, immediately before the next model request. It does not interrupt an in-flight model request or tool operation. If the Agent becomes idle before Steer is accepted, the runtime treats it as Prompt and starts a new turn. **Queue** waits until the active Agent run reaches its terminal response, then starts a new prompt. Steer takes priority over queued prompts at a shared boundary. Accepted input that has not reached its delivery boundary remains durable.

The composer preview contains only messages that have not been taken for delivery. `Alt+Up` removes the newest such message and prepends it to the current draft; `Alt+Enter` changes the newest queued follow-up to Steer while the Agent is still active. Once the runtime takes a message at its delivery boundary, it leaves the preview and can no longer be restored or upgraded. If URI Agent exits first, accepted undelivered input remains pending in the session database and resumes after that Agent is reopened; a lone pending Steer is promoted to Prompt on resume.

While a turn is running, press `Esc` twice within 500 milliseconds on the conversation surface to interrupt its current model request or tool operation. An `Esc` consumed by an open float only closes or steps back from that float and does not prime interruption. With no float open, the first press keeps the conversation surface's normal `Esc` behavior, such as clearing a row filter, and a fixed status above the footer prompts for the second press. The interrupted turn records an error and a complete turn boundary so another request can be sent normally. The embedded terminal keeps its separate double-`Esc` behavior: it closes the terminal float instead of interrupting the model turn.

Press `:` from the conversation to open the command panel. Type to fuzzy-filter registered command names, aliases, and descriptions; use `Tab` or `Shift+Tab` to complete and cycle matching commands, use the arrow keys or mouse to choose a result, press `Enter` to run it, and press `Esc` to close it. The unfiltered panel shows canonical names only. A matching alias replaces the canonical name in search results, so typing `t` can show `:thinking` for the `:effort` command; description matches keep the canonical name, and completing any match inserts its canonical command name. Commands that need a value open a selector or a separate input float. Search text filters the panel; it is not a secondary command syntax.

Selectable single-line rows keep their columns and mouse targets stable. Overflowing unselected text ends with `…`; after a short pause, the selected text scrolls to reveal the hidden content and then returns. Multi-line panel bodies and details wrap instead.

Core commands are registered through `CommandRegistry`:

| Command | Purpose |
| --- | --- |
| `:insert` | Open the composer |
| `:copy` | Copy the current selection or visible panel through OSC52 |
| `:tasks` | Inspect and cancel managed protocol work |
| `:protocols` | List registered read and exec routes |
| `:mcp` | Add, edit, test, enable, reconnect, or remove MCP servers |
| `:status` | Show project, model, usage, and extension status |
| `:model` | Search runnable models |
| `:model-roles` or `:roles` | Assign models to built-in and custom plugin roles |
| `:terminal-title-role` or `:title-role` | Choose the role used to generate terminal titles |
| `:refresh-catalog` | Force-refresh and apply pi and provider model catalogs |
| `:effort` | Select thinking effort supported by the active model |
| `:settings` | Inspect and edit active settings |
| `:login`, `:logout` | Manage provider credentials |
| `:resume`, `:new` | Switch project depth-1 sessions or create a root Agent; plugin-owned depth-2 conversations are not listed |
| `:search` or `:find` | Search text already shown in the current conversation and jump to a matching block |
| `:compact` | Request a context checkpoint when older completed history is available |
| `:set-env` | Add or replace an Agent environment variable through a masked value prompt |
| `:set-terminal`, `:terminal` | Configure and open the embedded terminal |
| `:help` | Show the active commands and keymap |
| `:quit` or `:q` | Exit URI Agent |

`:login` includes model providers plus the Parallel, Exa, and TinyFish web providers. Selecting one of these web providers opens a masked API-key prompt with that provider's key-management URL. Saved credentials take effect immediately for search and page extraction in the built-in `https` protocol and appear in `:logout` like other stored credentials.

`:model`, `F3`, and the Settings Model action open Model Hub. Its **Models** tab
lists models only for providers with a configured model credential source, and
its **Roles** tab keeps role assignments in the same workspace. `Tab` and
`Shift+Tab` switch tabs; clicking a tab has the same effect. Model search,
selection, and catalog refresh remain available in the Models tab. A model
chosen from Settings returns there as a pending choice, and `s` saves it with
the other Settings values. A conversation model chosen through `:model` or
`F3` is saved immediately. Logging out the current provider
clears the current session model and that provider's saved default only when no
other credential source remains; URI Agent does not automatically switch to a
different provider.

`:model-roles` opens Model Hub on Roles. It lists the built-in `small` role
first, followed by custom roles, with assignment, thinking effort, and source
columns. A project role that replaces a global assignment is marked explicitly.
`small` starts unassigned and does not follow the conversation model. Press
`Enter` or double-click to choose a runnable model and then its thinking effort.
Each `Esc` returns one step—to model selection, then the role list—rather than
closing the whole workflow. `Ctrl+N` names and assigns a custom role inline.
`Delete` opens a confirmation that names the affected scope and warns when
removing a project override will reveal the global assignment. Saving returns
to the role list.
Plugin commands can open a generic selector that stores any available role in
that plugin's settings, separately from role-to-model assignments.

Settings is divided into **Model** and **Agent** tabs. Model contains the
conversation model, current-provider credential status, and thinking effort;
Agent contains the inline output limit and private Agent environment manager.
The selected row has a short detail explanation. Value sources and unsaved
changes are shown beside each row. Use `Tab`, `Shift+Tab`, `Left`, or `Right`
to switch tabs, arrows or a click to select a row, `Enter` or a double-click to
edit it, and `Esc` to close without saving pending Settings choices.

On the first user message in a session, the built-in terminal-title plugin asks
the configured `small` role for a short title while the main turn continues.
It runs without tools or protocols and updates the outer terminal title when
the result arrives. Use `:terminal-title-role` to choose another role. Missing
roles, credentials, and generation failures do not interrupt or notify the
conversation.

Conversation search includes user, assistant, reasoning, tool, notice, compaction, and error text across the complete persisted transcript. It loads any older lazy pages before opening. Type to filter the results, use the arrow keys to choose one and press `Enter`, or click a result to return to that block. It is unavailable before the conversation has any text and while the `:resume` session selector is open.

Extensions register commands through the same registry, so they appear in the panel, help, and key-bindable action set without TUI-specific routing.

### MCP server manager

`:mcp` opens the linked MCP plugin's settings workflow. Its root list shows
scope, transport, enabled state, and the latest known connection state without
connecting every server. Use arrows, the mouse wheel, or a click to select;
`Enter` or a double-click edits; `Ctrl+N` adds; `T` tests; `R` reconnects;
`Space` toggles enabled state; and `Delete` opens removal confirmation.

The add/edit form exposes one row per stdio argument, environment mapping, or
HTTP header so spaces and special characters are preserved. Use `Ctrl+N` or an
Add row to insert one, and `Delete` to remove a selected dynamic row. `Enter`
advances or toggles the selected value, `Tab` and `Shift+Tab` navigate, and
`Ctrl+S` proceeds to automatic connection testing and review. Text fields
support cursor movement, backspace, and paste. Existing names are read-only;
scope, transport, and enabled state remain editable. Test, automatic test, and
Reconnect run in the background, keep input and rendering responsive, and can
be cancelled by leaving their workflow. Footer actions are clickable as well
as keyboard-accessible, including Add on an empty list, review actions, and
remove confirmation.

The review screen can be saved with `Enter` even after a failed connection
test; press `T` to test again or `E`/`Esc` to return to editing. `Esc` otherwise
backs out one workflow level before closing the panel. Removing a Project
entry warns when a same-named User definition will reappear. Detailed file,
credential, and layering behavior is in [MCP server
configuration](configuration.md#mcp-servers).

### Agent environment manager

`:set-env` opens separate name and masked-value prompts, then saves the variable globally. The **Agent environment** row in the Agent tab of Settings shows only the number of configured variables. Press `Enter` or double-click the row to open the manager; values are never shown in its list. In the manager, `Enter` or a double-click replaces the selected value, `Ctrl+N` adds a variable, `Delete` removes the selected variable, and `Esc` returns to Settings.

Saved values apply to future Agent `bash` and `pwsh` commands without a restart. They are not added to `:terminal`; see [Agent environment](configuration.md#agent-environment) for storage, scope, and plugin access.

## Default navigation

| Surface | Useful defaults |
| --- | --- |
| Conversation | `@` open the composer and list file references, `Alt+V` insert a clipboard image, `Up`/`Down` select, `Ctrl+Up`/`Ctrl+Down` scroll, `Enter` expand/fold, `o` open full document, `PageUp`/`PageDown` page, `Home`/`End` jump |
| Document | `c` copy the complete contents, arrows or page keys scroll, `Esc` close |
| Row filters | `r` reasoning, `t` tools, `h` user messages, `Esc` clear filter |
| Global | Double `Esc` interrupts a running turn; `F1` help, `F2` settings, `F3` models, `F4` status, `Ctrl+,` settings, `Ctrl+P` protocols, `Ctrl+T` tasks; the macOS key style also accepts `Cmd+,` |
| Copy | `Ctrl+C` or right-click copies an active selection; `Ctrl+X` copies the selection or, when none is active, the latest assistant response; without a selection, right-click opens a reasoning or tool block's full document; `c` in that document copies its complete contents; `Ctrl+Shift+C` copies the selection or visible surface; the macOS key style also accepts `Cmd+X` and `Cmd+C` when the terminal forwards them |

Arrow keys and mouse input are first-class. The mouse wheel and trackpad use the same smooth scrolling on every platform: each event keeps its six-row distance, and rapid same-direction events accumulate without an artificial pending-distance limit. Short distances advance one row per frame, while longer backlogs take geometrically larger steps so fast scrolling catches up in a bounded number of frames. Scrolling does not change the selected block; `Ctrl+Up`/`Ctrl+Down` scroll the conversation directly in the same six-row steps. `PageUp` and `PageDown` move by the current viewport height on the conversation, in documents, and in selectable panels. An inset scrollbar appears at the right edge when the loaded transcript and its virtual tail exceed the viewport; click its track or drag its thumb to scroll. Its layout and scrollbar metrics are exact for the currently loaded contiguous range and expand as older pages load; URI Agent does not read old Markdown merely to estimate whole-session rows. The scrollbar is excluded from text selection and includes the complete loaded virtual-tail range, so its bottom is the furthest reading position. Manual scrolling can move the final transcript row up to the middle of the viewport; this virtual tail space is not persisted conversation content. New output follows the real content bottom until the user scrolls away. When the viewport is showing content above the live tail, the activity row shows a padded `↓ bottom` mouse button above and slightly inset from the context meter. Without an activity row, a padded `↓` button floats at the same inset without taking layout space. The button stays hidden in virtual space past the live tail. Click either button or press `End` to resume following the tail. Keyboard navigation keeps an off-screen destination visible and centers it when the transcript has enough room. Arrow-key selection wraps from the last item to the first and from the first item to the last in every selectable list; page navigation stops at the first or last item. `j` and `k` exist as optional aliases on the main and several list surfaces, but defaults and help do not require Vim knowledge.

On URI Agent surfaces, `Ctrl+C` copies an active selection and is otherwise ignored. Exit through `:quit` or `:q` instead.

`F1` and `:help` are more authoritative than this summary because they reflect loaded keymap overrides and registered extension commands.

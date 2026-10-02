# Terminal interface

URI Agent uses one conversation surface with floating controls for composition,
commands, settings, documents, and the embedded terminal. It has no modal
Browse/Insert split and no slash-command syntax. `F1` and `:help` are the
authoritative command and key reference because they include keymap overrides
and extension commands.

## Conversation surface

An empty conversation shows the project, active model and thinking effort, and
entry points for composing, commands, and help. If no model is configured, it
prompts for `:login`. When networking is enabled, the welcome view may also
report a newer URI Agent release without blocking startup. The wordmark and
these lines follow the terminal width, so a narrow window never clips them: the
wordmark draws fewer cells per pixel or falls back to its name, and the
entry-point hints drop trailing entries before the line disappears.

After the first record, the footer shows model, effort, and context usage. It
also exposes active background tasks: the running count plus the newest
running task's label and elapsed time, degrading to the count alone when the
footer narrows. Clicking the badge opens `:tasks` with that task selected;
elapsed times tick once per second while tasks run. `:status` shows project,
session, usage, model timing, checkpoint strategy, diagnostic path, and
extension status.

User messages, final assistant responses, and errors stay visible. While a
turn runs, its intermediate reasoning and tool activity render as a bounded
process card in the position the finished process row will occupy: a summary
line ("Ran 2 commands · read 3 files · edited 1 file · 1 failed") followed
by the latest activity rows, with an "… N earlier steps" marker above them
and fewer rows in the compact layout. The summary counts every protocol step
individually, classified as a command, file read, edit, or search, plus the
failed calls; a narrow row shortens the counts but keeps the failures.
Assistant text from a model response that also calls a tool is intermediate:
once the call starts, the text joins the card and stays on its last rows
(up to three wrapped rows, two in the compact layout) until newer text arrives
or two further tool rounds complete. Only the footer animates; the card marks
itself `◆` while running and the row still in progress `›`. When the turn
finishes, the process row keeps the same summary. Select a process, reasoning, or tool row and press `Enter` or click
to fold it: a collapsed process shows the card (while running) or the
summary row, an expanded process lists the full activity inline. Press `o`
or right-click to open the complete rendered document, and press `c` there
to copy it.

Restored sessions initially load the latest checkpoint and following transcript.
Scrolling upward loads older complete turns in bounded pages. `Home`, transcript
search, and message or tool jumps load the older pages they need; following the
live tail remains immediate.

## Compact layout

Terminals at most 64 columns wide, such as a phone connected over SSH, use a
compact touch layout. `F5` or `:layout` toggles it for the running process;
`:layout auto`, `:layout wide`, or `:layout compact` sets the mode directly,
and the `layout` setting chooses the startup mode (see
[settings](configuration.md#settings-fields-and-precedence)). A toggle that
lands on what the configured mode would choose returns to following the width.

In the compact layout:

- a two-row action bar below the footer writes a message, opens commands,
  jumps to the latest output, and shows status, or stops the running turn;
- the footer shows the model, effort, task count (with the newest running
  task's label when it fits), and context percentage only;
- the transcript has no scrollbar or side padding and fills the full width,
  leaving selection and swipe scrolling to the terminal client;
- each mouse-wheel event scrolls 2 rows instead of 6, because a phone swipe
  arrives as a burst of wheel events; keyboard scrolling is unchanged;
- panels fill the screen with a top border only, drop key hints from their
  titles, and carry a `✕` button that acts like the panel's `Esc`; message
  input stays at the bottom;
- list rows use two lines, name above details, and a single tap chooses a row
  instead of a double click.

## Composer and delivery

Press `Space` to open the composer. `Enter` sends when idle; use
`Shift+Enter`, `Ctrl+Enter`, or `Ctrl+J` for a newline. Windows uses the
Ctrl-based form because its console does not report Shift+Enter reliably.
Multi-line paste always remains draft text and does not send.

The composer supports normal character, word, line, selection, clipboard,
undo, and redo editing. `Esc` closes it while preserving the draft. Exact
bindings appear in `F1` and follow the active keymap.

Type `@` at the start of a token to complete project files and `@@` to complete
saved sessions from the current project. File references use `@file://<path>`;
session references use stable IDs. Keyboard and mouse selection share the same
completion path.

While a turn is active, sending opens a choice:

- **Steer** is delivered after the current assistant response and its tool calls,
  immediately before the next model request. It does not interrupt in-flight
  work and acts as Prompt if the Agent becomes idle first.
- **Queue** waits for the active Agent run to finish, then starts a new prompt.

Steer has priority at a shared delivery boundary. Accepted messages remain
durable until delivered. `Alt+Up` restores the newest still-undelivered message;
`Alt+Enter` upgrades the newest queued message to Steer while work remains
active.

Press `Esc` twice within 500 milliseconds on the conversation surface to
interrupt the current model request, retry delay, or tool operation. An `Esc`
consumed by an open float only closes that float. The embedded terminal uses the
same gesture to close itself rather than interrupting the Agent.

## Commands and settings

Press `:` to open the command panel. Type to fuzzy-filter registered names,
aliases, and descriptions; choose a result with the keyboard or mouse. Commands
that need values open a selector or form. Search text filters the panel—it is
not a second command syntax. Commands are listed by expected use: session
lifecycle (`:new`, `:resume`, `:quit`), model choices, in-conversation tools,
configuration, then rare setup; extension commands follow alphabetically. While
searching, match quality ranks first and this order breaks ties. `:compact` is
offered only under the summary context strategy.

Common entry points include:

- `:login`, `:logout`, `:model`, `:effort`, and `:model-roles` for model access;
- `:settings`, `:set-env`, `:set-terminal`, and `:layout` for configuration;
- `:resume`, `:new`, `:search`, `:compact`, and `:context-strategy` for sessions;
- `:protocols`, `:tasks`, `:mcp`, `:status`, and `:terminal` for tools and status;
- `:help` and `:quit` for reference and exit.

Model Hub combines conversation-model selection and plugin model-role
assignments. Model rows show the provider display name when the catalog defines
one (`providerName`, for example Step Plan for `stepfun`) and the provider ID
otherwise; search matches both forms. Settings separates Model and Agent
values, shows their source, and marks unsaved changes. Conversation search
covers the complete persisted transcript, loading older pages before presenting
matches.

The terminal-title plugin names the terminal after the first prompt when its
declared `title` model role is assigned. Missing role assignments or generation
failures do not interrupt the conversation.

Extensions register through the same command, panel, status, completion, and
submission interfaces, so they do not create a second navigation system.

### MCP server manager

`:mcp` lists user and project servers with scope, transport, enabled state, and
known connection status. It supports adding, editing, testing, reconnecting,
enabling, disabling, and removing servers. Forms preserve each argument,
environment mapping, and HTTP header as a separate value. A failed connection
test can be reviewed and saved for later correction.

Server files, credential references, layering, and session behavior are in
[Models and configuration](configuration.md#mcp-servers).

### Agent environment manager

`:set-env` adds or replaces one masked value. The Agent Environment row in
Settings opens the full name-only manager for adding, replacing, and deleting
entries. Saved values apply to future Agent shell commands, not `:terminal`;
see [Agent environment](configuration.md#agent-environment).

### Task manager

`:tasks` lists managed background work: running tasks first, then finished
tasks newest first. Each row shows a status glyph (a live spinner while
running, `✓` completed, `×` failed, `⊘` cancelled), the owning protocol, the
label, and a live elapsed time that becomes the total duration once a task
settles. Compact rows put protocol, status, and elapsed time on a second
line.

Below the list, the selected task shows its id, protocol, timestamps, and a
bounded live tail of its newest output. Terminal escape sequences and other
control characters are stripped from the tail; task output is untrusted
process data. Actions: `x` cancels, `i` interrupts an interactive shell task,
`o` opens the complete output in the shared document viewer, and `c` copies
it. Interrupt uses the same signal the `tasks` protocol's `/interrupt`
operation sends. When a background task settles while the conversation is
open, a transient notice such as `✓ task <label> completed in 1m 12s>`
appears at the bottom without opening the panel.

## Navigation and copy

Arrow keys and mouse input are first-class across conversation rows, lists,
panels, documents, and forms. Page keys move by a viewport; `Home` and `End`
jump through the conversation, and `End` resumes following new output. Row
filters can focus reasoning, tools, or user messages.

Drag to select text and double-click to select a Unicode word. Copy shortcuts
use OSC52. On URI Agent surfaces, `Ctrl+C` copies only when a selection exists;
exit with `:quit` rather than treating it as a process signal. Selection,
terminal-specific behavior, keymap overrides, and image paste are documented in
[Keymaps, terminal, and attachments](terminal.md).

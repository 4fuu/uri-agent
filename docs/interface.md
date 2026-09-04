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
report a newer URI Agent release without blocking startup.

After the first record, the footer shows model, effort, and context usage. It
also exposes active background-task count; `:tasks` opens the task manager.
`:status` shows project, session, usage, model timing, checkpoint strategy,
diagnostic path, and extension status.

User messages, final assistant responses, and errors stay visible. Intermediate
reasoning and tool activity remain in event order while a turn runs, then fold
into one Process row. Select a process, reasoning, or tool row and press `Enter`
or click to fold it. Press `o` or right-click to open the complete rendered
document, and press `c` there to copy it.

Restored sessions initially load the latest checkpoint and following transcript.
Scrolling upward loads older complete turns in bounded pages. `Home`, transcript
search, and message or tool jumps load the older pages they need; following the
live tail remains immediate.

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
not a second command syntax.

Common entry points include:

- `:login`, `:logout`, `:model`, `:effort`, and `:model-roles` for model access;
- `:settings`, `:set-env`, and `:set-terminal` for configuration;
- `:resume`, `:new`, `:search`, `:compact`, and `:context-strategy` for sessions;
- `:protocols`, `:tasks`, `:mcp`, `:status`, and `:terminal` for tools and status;
- `:help` and `:quit` for reference and exit.

Model Hub combines conversation-model selection and plugin model-role
assignments. Settings separates Model and Agent values, shows their source, and
marks unsaved changes. Conversation search covers the complete persisted
transcript, loading older pages before presenting matches.

The terminal-title plugin can use an assigned model role to name the terminal
after the first prompt. Missing role assignments or generation failures do not
interrupt the conversation.

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

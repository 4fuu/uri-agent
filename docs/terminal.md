# Keymaps, terminal, and attachments

This document covers terminal-dependent input: keymap overrides, the embedded PTY, selection and copy behavior, and image attachments. For conversation navigation and commands, see [Terminal interface](interface.md).

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

Key names are normalized when the keymap loads. Modifier names are case-insensitive; `control` aliases `ctrl`, `option` aliases `alt`, and `cmd` or `command` aliases `super`. An invalid name stops startup and identifies the owning keymap file instead of creating an unreachable binding.

Visible action hints are resolved from the effective keymap after global and project overrides. Text style renders labels such as `Ctrl+R`, `Shift+Enter`, and `Alt+Up`; macOS style renders `⌃R`, `⇧↩`, and `⌥↑`, with `super` shown as Command (`⌘`). Panel titles, composer guidance, pending-message controls, transcript actions, and `F1` help therefore stay synchronized with overrides. On Windows, a hint prefers the Ctrl-based binding over the Shift-based one for the same key, because the Windows console never reports Shift with Enter as a key press.

Set `keyDisplay` or `URI_AGENT_KEY_DISPLAY` to `auto`, `macos`, or `text`; see [Settings fields and precedence](configuration.md#settings-fields-and-precedence). `auto` selects macOS symbols only when URI Agent itself runs on macOS. Choose `macos` explicitly when a macOS terminal connects to a non-macOS host. That style adds Command aliases for Settings, paste, undo, and redo while keeping portable bindings; the terminal must still forward those shortcuts.

Bindings belong to surfaces such as `global`, `main`, `composer`, `command`, `list`, `selector`, `settings`, `environment`, `models`, `document`, `selection`, and `terminal`. A surface binding is checked before a global binding.

Configurable actions must go through the keymap. Commands available from the panel or key bindings use `CommandRegistry`; they do not add a separate hard-coded command path.

## Embedded terminal

`:set-terminal` stores the command used by `:terminal`, such as `bash` or `pwsh -NoLogo`. `URI_AGENT_TERMINAL` can override it for one invocation.

`:terminal` opens the command in a PTY rooted at the project directory. It inherits the URI Agent process environment but not values from the Agent environment manager. Input, including `Ctrl+C` when no URI Agent selection is active, is forwarded to the terminal program. Press `Esc` twice within 500 milliseconds to close the float; a single `Esc` is sent to the program.

Clicks and drags normally go to the terminal application. Hold `Shift` while dragging to select rendered text, then use `Ctrl+C`, `Ctrl+Shift+C`, or right-click to copy through OSC52. On macOS, `Cmd+C` also works when the terminal forwards the modifier.

User prompts, assistant responses, blank conversation space (including the virtual tail), and read-only floats support direct drag selection. Hold `Shift` for reasoning and tool blocks so ordinary clicks remain available for folding and opening. On URI Agent surfaces, a copy shortcut copies the active selection, `Esc` clears it, and any other shortcut clears it before continuing through normal key routing. In an open reasoning, tool, or process document, `c` copies the complete document rather than only the visible viewport. Terminal restoration, selection, and OSC52 copy remain active on normal and error exits.

## Image attachments

Normal paste inserts text and opens the composer when used from the conversation. When the terminal forwards `Ctrl+V`, URI Agent reads the clipboard, preferring an image and falling back to text. Because some terminals consume `Ctrl+V`, `Alt+V` is the reliable image shortcut. Submission waits for the background clipboard read to finish.

Each image appears in the composer as an atomic `🖼 #N` chip. Cursor movement crosses the whole chip; adjacent `Backspace` or `Delete`, or a selection touching it, removes both chip and attachment. On submission, active chips become `[Image #N]` markers in message order. Unsent image bytes are process-local and are discarded on exit or session switch; stale chips and markers are removed from restored drafts.

For a model whose catalog `input` includes `image`, a standalone `@file://<path>` attaches a project image:

```text
Describe @file://screenshots/error.png and suggest a fix.
```

URI Agent encodes clipboard images as PNG. File attachments support PNG, JPEG, GIF, and WebP after extension and signature validation. Clipboard and file images can share a message, and the original `@file://` text remains in the user message.

Relative paths resolve from the project. Absolute paths are accepted only when their canonical location remains inside the project; symlink escapes are rejected. A recognized image attachment fails explicitly when the active model is text-only.

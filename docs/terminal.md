# Keymaps, terminal, and attachments

This document covers terminal-dependent input. Conversation commands and
navigation are described in [Terminal interface](interface.md); `F1` shows the
effective bindings.

## Layered keymap

Bindings load from lowest to highest priority:

```text
built-in defaults
< <config>/keymap.rhai
< <project>/.uri-agent/keymap.rhai
```

Rhai files call `map` and `unmap`:

```rhai
map("main", "x", "copy");
unmap("main", "j");
map("composer", "ctrl+j", "newline");
```

Modifier names are case-insensitive; `control` aliases `ctrl`, `option` aliases
`alt`, and `cmd` or `command` aliases `super`. Invalid names stop startup and
identify the owning file. A surface-specific binding takes priority over a
global binding.

Visible hints and `F1` are generated from the effective keymap. Set
`keyDisplay` or `URI_AGENT_KEY_DISPLAY` to `auto`, `macos`, or `text`. The
macOS style uses symbols and adds common Command aliases, but the terminal must
still forward those keys. See [settings](configuration.md#settings-fields-and-precedence).

## Embedded terminal

`:set-terminal` saves the command opened by `:terminal`;
`URI_AGENT_TERMINAL` overrides it for one process. The PTY starts in the
project directory and inherits URI Agent's process environment, but not values
from the Agent Environment manager.

Input goes to the terminal program. Press `Esc` twice within 500 milliseconds
to close the terminal float; a single `Esc` is forwarded. Hold `Shift` while
dragging or double-clicking to select terminal text, then copy through OSC52.

URI Agent surfaces support direct drag and Unicode word selection. Hold
`Shift` on reasoning and tool rows so ordinary clicks remain available for
folding and opening. Conversation selections remain anchored while scrolling;
rewrapping, rewriting an affected block, or switching sessions clears them.
`Ctrl+C` copies an active selection, and complete reasoning or tool documents
can be copied with `c`.

## Image attachments

Normal paste inserts text and never sends a multi-line paste automatically.
When the terminal forwards `Ctrl+V`, URI Agent prefers a clipboard image and
falls back to text. `Alt+V` is the reliable image shortcut when the terminal
consumes normal paste.

Images appear as atomic composer chips and remain process-local until sent.
Unsent image bytes are discarded on exit or session switch. Clipboard images
are encoded as PNG; file attachments support PNG, JPEG, GIF, and WebP after
signature validation.

A standalone project reference attaches an image while retaining its text in
the user message:

```text
Describe @file://screenshots/error.png and suggest a fix.
```

Relative paths resolve from the project. Absolute paths are accepted only when
their canonical location remains inside it, and symlink escapes are rejected.
A recognized image attachment fails explicitly when the active model is
text-only.

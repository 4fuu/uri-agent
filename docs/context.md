# Startup context and Skills

URI Agent builds startup context for each new session from its core prompt, prompt-only plugins, and discovered Skills. It freezes the resulting system prompt and Skill selection so later filesystem or environment changes do not reinterpret an existing session.

## Project instructions

When the canonical project directory contains `AGENTS.md`, a prompt-only built-in plugin appends its content to the new session's system prompt:

```text
<project_rule_md>
The following content is from the project's AGENTS.md. Follow these instructions.

<AGENTS.md content>
</project_rule_md>
```

A missing file contributes nothing; other read failures stop session creation. Changes to `AGENTS.md` apply only to new sessions.

## Installed binary hints

Another prompt-only plugin scans `PATH` once for a fixed set of modern command-line tools:

```text
rg, fd, fdfind, sd, bat, batcat, eza, exa, lsd, delta,
jq, yq, fzf, xh, hyperfine, dust, duf, procs, btm, zoxide,
doggo, gping, hexyl, choose, sad, ast-grep, broot, tokei, watchexec, glow
```

When it finds any, the generated prompt names the available programs and asks the model to prefer them over classical Unix equivalents. Detection is case-insensitive, preserves the display order above, removes duplicate names, and never invokes a detected program. Changes to installed binaries or `PATH` apply only to new sessions.

Neither startup plugin registers a protocol, command, panel, status provider, key binding, or setting.

## Skills

### Discovery

URI Agent scans these roots once at startup, from highest to lowest priority:

```text
<project>/.agents/skills
<project>/.claude/skills
<project>/.codex/skills
~/.agents/skills
~/.claude/skills
~/.codex/skills
```

Each root may contain `SKILL.md` directly or in one immediate child directory. Discovery does not recurse deeper.

A Skill begins with YAML frontmatter containing nonempty `name` and `description` values:

```yaml
---
name: Code Review
description: Review a change for correctness and regressions.
---
```

### Protocol name and resources

URI Agent lowercases the name, replaces runs of non-ASCII-alphanumeric characters with `-`, and appends `-skill` when absent. The example registers routes such as:

```text
code-review-skill://help
code-review-skill://scripts/check.py
```

The first Skill for a normalized protocol name wins. Later duplicates and names that collide with an existing protocol are skipped with a notice.

`<name>-skill://help` reads `SKILL.md`; other targets read files relative to the Skill directory. Absolute targets and paths that escape that directory, including through symlinks, are rejected.

### Frozen session behavior

A new session stores:

- the complete generated system prompt;
- each selected Skill's name and description;
- each selected Skill's canonical `SKILL.md` path.

Resume reuses this snapshot instead of rediscovering current context. Resources continue to load from the frozen location, so removing it produces an explicit error; a same-named Skill elsewhere cannot replace it. A historical session without frozen context is invalid rather than being reinterpreted with current startup state.

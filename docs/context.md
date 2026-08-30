# Startup context and Skills

URI Agent builds startup context for each new session from its core prompt, prompt-only plugins, configured protocol descriptors, and discovered Skills. Preparation begins in the background after the TUI opens; the first user message waits for the complete result. URI Agent then freezes the system prompt, session-scoped protocol records, and Skill selection so later filesystem or environment changes do not reinterpret an existing session.

## Project instructions

When the canonical project directory contains `AGENTS.md`, a prompt-only built-in plugin appends its content to the new session's system prompt:

```text
<project_rule_md>
The following content is from the project's AGENTS.md. Follow these instructions.

<AGENTS.md content>
</project_rule_md>
```

A missing file contributes nothing; other read failures prevent the first message from starting. Changes to `AGENTS.md` apply only to new sessions.

The project-instruction plugin does not register a protocol, command, panel,
status provider, key binding, or setting.

## Skills

### Discovery

For a new session, URI Agent scans these roots once while preparing its startup context, from highest to lowest priority:

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

Resume reuses this snapshot and does not scan current project instructions, `PATH`, or Skill roots. Resources continue to load from the frozen location, so removing it produces an explicit error; a same-named Skill elsewhere cannot replace it. A stored session without frozen context is invalid rather than being reinterpreted with current startup state.

Linked plugins may also contribute session-scoped protocol records. The built-in
MCP plugin records only each enabled server's stable configuration identity and
the protocol name and description placed in the generated prompt; it does not
snapshot commands, URLs, headers, environment values, or server metadata.
Resume and compaction retain those exact records and the already generated
prompt. Calls resolve mutable MCP configuration when used, so a new server does
not join an existing session and a removed recorded server fails directly.

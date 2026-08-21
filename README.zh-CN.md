# uri-agent

[English](README.md) | [简体中文](README.zh-CN.md)

[![License: MIT](https://img.shields.io/badge/license-MIT-6ed2c2.svg)](LICENSE)

一个协议驱动、专注于终端体验的 coding agent。无论扩展多少能力，模型始终只看到 `read` 和 `exec` 两个工具；协议、Skills、任务与大型输出都在需要时才进入上下文。

## 为什么做 uri-agent

Agent 每增加一种集成，工具列表通常就会继续增长。这些 schema 会在模型判断是否需要对应能力之前占用上下文，每个新工具也会带来另一套调用方式。

uri-agent 将模型面对的契约保持在最小范围：

- **两个工具** — 所有能力都通过 `read(uri, body?)` 或 `exec(uri, body?)` 访问。
- **分层加载** — 初始提示词只列出协议名称和用途，详细说明保存在 `<protocol>://help`。
- **不透明地址** — 路由器只按第一个 `://` 分割，其余 target 保持原样。
- **默认异步** — 执行立即返回任务地址；确实需要即时结果时可以请求有界等待。
- **一个 Skill，一个协议** — 每个 Skill 都拥有独立且对模型可见的名称，而不是共用一个通用 Skill 入口。
- **上下文可恢复** — 会话保留模型消息和工具调用身份；超长输出仍可通过 `file://` 地址完整读取。

这让工具面保持稳定，同时允许能力增长，而不必让每项集成都永久占用提示词。

## 功能

### 双工具协议路由

模型只会收到以下两个工具定义：

```text
read(uri: string, body?: any)
exec(uri: string, body?: any)
```

`body` 可以是任意 JSON 值，包括 Markdown 字符串、数组、对象、数字、布尔值或 null。注册中心会把原始 URI、不透明 target 和 body 交给对应协议，不进行 URL 解码或规范化。

```text
read("file://src/main.rs?offset=1&limit=200")
read("code-review-skill://help")
exec("bash://?wait=30", "cargo test")
```

协议通过 [`Protocol`](src/protocol.rs) trait 实现 `read`、`exec` 或二者。增加协议不会增加模型可见的工具。

### 系统管理的异步任务

除非协议另有说明，`exec` 默认异步执行。Shell 和 edit 通常会立即返回任务地址：

```text
exec("bash://run", "cargo test")
→ Read status: bash://tasks/<id>
```

使用 `?wait=N` 可以等待最多 300 秒：

```text
exec("bash://?wait=30", "cargo test")
```

任务在等待窗口内结束时会直接返回结果；等待超时后任务仍在后台运行，可以通过 `read("bash://tasks/<id>")` 查看。

### 内置协议

| 协议 | 入口 | 用途 |
| --- | --- | --- |
| `file` | `read` | 按限定行数读取文件或目录列表 |
| `edit` | `read`, `exec` | 原子写入文件，或替换唯一匹配的文本 |
| `bash` | `read`, `exec` | 环境存在 Bash 时运行受管理的 Bash 任务 |
| `pwsh` | `read`, `exec` | 环境存在 `pwsh` 时运行受管理的 PowerShell 7 任务 |
| `<name>-skill` | `read` | 加载一个 Skill 提示词及其附属资源 |

模型会在使用陌生协议前读取 `<protocol>://help`。Bash 和 PowerShell 会在启动时探测，只有找到对应可执行文件才会注册。

### 将 Skills 映射为提示词协议

每个被发现的 `SKILL.md` 都必须包含 `name` 与 `description` YAML frontmatter。Skill 名称会成为独立协议：

```yaml
---
name: Code Review
description: Review a change for correctness and regressions.
---
```

```text
code-review-skill://help
code-review-skill://scripts/check.py
```

help 响应包含完整的 `SKILL.md`，并附上 Skill 文件所在的真实 `file://` 目录，方便模型检查或运行附带脚本。资源路径不能通过 `..` 或符号链接逃出 Skill 目录。

Skills 按以下顺序扫描：

```text
<cwd>/.agents/skills
<cwd>/.claude/skills
<cwd>/.codex/skills
<cwd>/.amp/skills
~/.agents/skills
~/.claude/skills
~/.codex/skills
~/.config/amp/skills
~/.cache/amp/global-skills
```

两个 Skill 产生相同协议名时，优先级更高的位置生效。

### 保留完整输出而不永久占用上下文

工具输出返回模型前会受到长度限制。内容超限时，uri-agent 会展示头尾预览、保存完整输出，并返回 `file://` 地址供模型按需筛选和继续读取。

### 与提供商无关的会话

Rig 适配层支持 OpenAI Responses、Anthropic 和 Gemini。uri-agent 自己持有工具循环，并把与提供商无关的模型消息写入 append-only JSONL 会话，包括正确恢复所需的工具调用 ID 和 reasoning 签名。

最后一条 JSONL 写入中断时，会话下次打开会自动修复；更早位置的损坏记录仍会报错，不会被静默忽略。

### 专注的终端界面

Ratatui 界面支持流式文本与 reasoning、多行输入、bracketed paste、鼠标和键盘滚动、协议与任务浮窗、任务取消、会话回放，以及模型工作时的低噪声抖动动画。

## 环境要求

- Rust stable 与 Cargo
- 支持标准 ANSI 和 alternate screen 的终端
- OpenAI、Anthropic 或 Gemini 的 API key
- 只有使用对应 Shell 协议时才需要 Bash 或 PowerShell 7

## 安装

克隆仓库并构建 release 二进制：

```bash
git clone https://github.com/4fuu/uri-agent.git
cd uri-agent
cargo build --release
```

直接从仓库运行：

```bash
export OPENAI_API_KEY=...
./target/release/uri-agent --cwd /path/to/project
```

### 从源码安装

```bash
cargo install --path .
uri-agent --cwd /path/to/project
```

## 配置

Provider 和模型可以通过参数或环境变量选择：

```bash
uri-agent \
  --provider anthropic \
  --model claude-sonnet-4-6 \
  --cwd /path/to/project
```

| 设置 | 环境变量 | 默认值 | 作用 |
| --- | --- | --- | --- |
| `--provider` | `URI_AGENT_PROVIDER` | `openai` | 选择 `openai`、`anthropic` 或 `gemini` |
| `--model` | `URI_AGENT_MODEL` | 由 Provider 决定 | 覆盖模型标识符 |
| `--cwd` | — | `.` | 设置内置协议使用的工作目录 |
| `--session` | — | 新会话 | 恢复指定会话；`latest` 表示最近会话 |
| `--output-limit` | — | `32768` | 完整输出落盘前允许模型看到的字节数 |

Provider 凭据使用标准环境变量：

| Provider | API key | 默认模型 |
| --- | --- | --- |
| OpenAI | `OPENAI_API_KEY` | `gpt-5.2` |
| Anthropic | `ANTHROPIC_API_KEY` | `claude-sonnet-4-6` |
| Gemini | `GEMINI_API_KEY` | `gemini-3-flash-preview` |

恢复最近一次会话：

```bash
uri-agent --session latest --cwd /path/to/project
```

会话位于平台数据目录的 `uri-agent/sessions/<session-id>/events.jsonl`。完整工具输出位于平台缓存目录的 `uri-agent/outputs/<session-id>/`。

## TUI 快捷键

| 按键 | 行为 |
| --- | --- |
| `Enter` | 发送消息 |
| `Shift+Enter` | 插入换行 |
| `PageUp` / `PageDown` | 滚动对话或当前浮窗 |
| `F1` | 打开帮助 |
| `Ctrl+P` | 打开协议列表 |
| `Ctrl+T` | 打开任务列表 |
| `x` | 在任务列表中取消所选任务 |
| `Esc` | 关闭当前浮窗 |
| `Ctrl+C` | 退出 |

## 安全

内置文件和 Shell 协议不提供沙箱。Agent 拥有 uri-agent 进程的文件与命令权限，也可以访问绝对路径。请只在可信项目和允许 Agent 使用其中凭据的环境中运行。

## 开发

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo check
```

测试覆盖协议分发、任意 body 透传、异步等待、原子编辑、输出保留、Skill 路径限制、会话恢复和不依赖 Provider 的端到端工具循环。仓库不变量和代码结构记录在 [`AGENTS.md`](AGENTS.md)。

## 许可证

[MIT](LICENSE) © 2026 4fuu

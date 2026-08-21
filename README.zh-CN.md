# uri-agent

[English](README.md) | [简体中文](README.zh-CN.md)

[![License: MIT](https://img.shields.io/badge/license-MIT-6ed2c2.svg)](LICENSE)

一个面向终端的协议驱动 coding agent。模型始终只看到 `read` 和 `exec` 两个工具；协议、Skills、任务结果和大型输出都在需要时才加载。

## 设计

Agent 每增加一种集成，工具列表通常就会继续增长。这些 schema 会在模型判断是否需要对应能力之前占用上下文，每个新工具也会带来另一套调用方式。

uri-agent 将模型面对的契约保持稳定：

- **两个工具** — 所有能力都通过 `read(uri, body?)` 或 `exec(uri, body?)` 访问。
- **分层加载** — 系统提示词只列出协议名称和用途，操作说明保存在 `<protocol>://help`。
- **不透明地址** — 路由器只按第一个 `://` 分割，其余 target 不做 URL 解码或规范化。
- **任意 body** — `body` 可以是任意 JSON 值，并原样交给协议。
- **异步执行** — 协议可以立即返回系统管理的任务。等待由协议自行实现，不是 `exec` 或 URI 的通用行为。
- **一个 Skill，一个协议** — 每个 Skill 都拥有独立且对模型可见的协议名。
- **可恢复会话** — 模型消息和工具调用身份保存在 SQLite；超长输出仍可通过 `file://` 完整读取。

## 工具协议

模型只会收到以下两个工具定义：

```text
read(uri: string, body?: any)
exec(uri: string, body?: any)
```

示例：

```text
read("file://src/main.rs?offset=1&limit=200")
read("code-review-skill://help")
exec("bash://?wait=30", "cargo test")
```

协议通过 [`Protocol`](src/protocol.rs) trait 实现 `read`、`exec` 或二者。注册新协议不会增加模型可见的工具。

### 系统管理的任务

Shell 和 edit 执行通常会立即返回任务地址：

```text
exec("bash://run", "cargo test")
→ Read status: bash://tasks/<id>
```

内置 shell 协议支持用 `?wait=N` 等待最多 300 秒：

```text
exec("bash://?wait=30", "cargo test")
```

等待超时后任务仍在后台运行，可以通过 `<protocol>://tasks/<id>` 读取状态和完整结果。该选项由 `bash` 与 `pwsh` 插件自行解析；路由器只原样传递不透明 target，不会为其他协议解释 `wait`。协议实现可以调用与 URI 无关的系统任务等待接口，自行提供所需的等待语法。

### 内置协议

| 协议 | 操作 | 用途 |
| --- | --- | --- |
| `file` | `read` | 按限定行数读取文件或目录列表 |
| `edit` | `read`, `exec` | 原子写入文件，或替换唯一匹配的文本 |
| `bash` | `read`, `exec` | 环境存在 Bash 时运行受管理的 Bash 任务 |
| `pwsh` | `read`, `exec` | 环境存在 `pwsh` 时运行受管理的 PowerShell 7 任务 |
| `<name>-skill` | `read` | 加载一个 Skill 提示词及其附属资源 |

Bash 和 PowerShell 会在启动时探测，只有找到对应可执行文件才会注册。

### 将 Skills 映射为提示词协议

每个被发现的 `SKILL.md` 都必须包含 `name` 与 `description` YAML frontmatter。名称会规范化为独立协议：

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

两个 Skill 规范化为相同协议名时，优先级更高的位置生效。

### 完整保留大型输出

工具输出返回模型前会受到长度限制。内容超限时，uri-agent 会返回头尾预览、在平台缓存目录保存完整字节，并提供 `file://` 地址供模型按需筛选读取。

## 模型与 Provider

uri-agent 直接使用当前 [pi](https://github.com/badlogic/pi-mono) 云端模型目录：

```text
https://pi.dev/api/models/providers
https://pi.dev/api/models/providers/<provider-id>
```

程序生成的 `models-store.json` 使用 pi 的缓存 schema，包括 `checkedAt`、`lastModified` 和 `etag`。缓存刷新周期为四小时；刷新失败时仍可使用已有缓存。`--offline`、`URI_AGENT_OFFLINE=1` 或 `PI_OFFLINE=1` 会禁用模型目录网络请求。

当前 Rust/Rig 后端支持以下 pi API family：

- `openai-responses`
- `openai-completions`
- `anthropic-messages`
- `google-generative-ai`

完整远端目录都会缓存，Settings 选择器只展示可运行 API family 中的模型。如果某个 Provider 使用受支持的 API family，但需要 OAuth 或云环境凭据，它仍可能出现在界面中；uri-agent 当前只实现 API key 认证。Bedrock、Vertex、Azure Responses、Codex OAuth 和 Mistral Conversations 仍需要专用 Rust 适配器。

## 配置

没有 API key 也可以启动 uri-agent。使用 `F2`、`Ctrl+,`、`/settings`、`/model` 或 `/login` 打开设置浮窗，编辑 Provider、模型、当前 Provider 凭据和内联输出上限。保存后模型后端会立即生效，不会丢弃当前会话。

### 文本文件

uri-agent 保持普通配置可编辑，并将程序生成数据与用户覆盖分离。Linux 默认配置目录为 `~/.config/uri-agent`；可以用 `URI_AGENT_CONFIG_DIR` 覆盖。

| 文件 | 维护者 | 用途 |
| --- | --- | --- |
| `settings.json` | uri-agent 和用户 | 与 pi 兼容的全局设置，包括 `defaultProvider` 与 `defaultModel` |
| `auth.json` | uri-agent 和用户 | 与 pi 兼容的 Provider 凭据；Unix 权限为 `0600` |
| `models-store.json` | uri-agent | 从 `pi.dev` 拉取的程序缓存 |
| `models.json` | 用户 | 与 pi 兼容的自定义 Provider、模型、headers 和模型覆盖 |
| `<cwd>/.uri-agent/settings.json` | 用户 | 可选的项目设置，覆盖全局设置 |

项目设置文件已存在时，TUI 会将 Provider、模型和输出上限写入项目文件；否则写入全局设置。凭据始终写入全局 `auth.json`。

`settings.json` 示例：

```json
{
  "defaultProvider": "openai",
  "defaultModel": "gpt-5.2",
  "outputLimit": 32768
}
```

`auth.json` 示例：

```json
{
  "openai": {
    "type": "api_key",
    "key": "$OPENAI_API_KEY"
  }
}
```

`models.json` 覆盖示例：

```json
{
  "providers": {
    "local-openai": {
      "baseUrl": "http://127.0.0.1:11434/v1",
      "api": "openai-completions",
      "apiKey": "local",
      "models": [
        {
          "id": "qwen3-coder",
          "name": "Qwen3 Coder",
          "contextWindow": 131072,
          "maxTokens": 16384
        }
      ]
    }
  }
}
```

凭据和 header 值支持 pi 风格的 `$VAR`、`${VAR}`、`$$`、`$!`，以及开头的 `!shell command`。命令会在十秒后超时，成功结果在当前进程内缓存。这些文件属于可信配置：开头的 `!` 会以 uri-agent 的权限执行命令。

### 优先级

普通设置从低到高按以下顺序合并：

```text
内置默认值
< 全局 settings.json
< 项目 .uri-agent/settings.json
< URI_AGENT_* 环境变量
< 命令行参数
```

API key 优先级为：

```text
models.json apiKey
< auth.json
< Provider 环境变量
< URI_AGENT_API_KEY
< --api-key
```

环境变量和命令行参数只覆盖当前进程，不会写回文件。Settings 浮窗会显示实际生效值的来源，避免把已保存值误认为当前覆盖已经消失。

### 命令行选项

```bash
uri-agent \
  --provider anthropic \
  --model claude-sonnet-4-6 \
  --cwd /path/to/project
```

| 参数 | 环境变量 | 作用 |
| --- | --- | --- |
| `--provider` | `URI_AGENT_PROVIDER` | 选择 pi Provider ID |
| `--model` | `URI_AGENT_MODEL` | 选择该 Provider 的模型 ID |
| `--api-key` | `URI_AGENT_API_KEY` | 设置只对当前进程生效的凭据 |
| `--output-limit` | `URI_AGENT_OUTPUT_LIMIT` | 设置内联输出字节数，最小 1024 |
| `--offline` | `URI_AGENT_OFFLINE`, `PI_OFFLINE` | 只使用本地模型缓存 |
| `--cwd` | — | 设置内置协议可访问的工作目录 |
| `--continue-session` | — | 恢复最近更新的会话 |
| `--session <id>` | — | 按 ID 恢复会话，也接受 `latest` |

已知 Provider 使用标准环境变量，例如 `OPENAI_API_KEY`、`ANTHROPIC_API_KEY`、`GEMINI_API_KEY`、`OPENROUTER_API_KEY` 和 `GROQ_API_KEY`。自定义 Provider ID 会回退到 `<NORMALIZED_PROVIDER>_API_KEY`。

## 会话

所有会话保存在平台数据目录中的一个 SQLite 数据库：

```text
<data-dir>/uri-agent/sessions.db
```

SQLite WAL 模式和事务内 sequence 分配保证事件顺序与模型历史一致。超长完整输出仍独立保存在平台缓存目录：

```text
<cache-dir>/uri-agent/outputs/<session-id>/
```

SQLite 是项目最初公开的持久化格式，因此不包含 JSONL 兼容层。

## TUI

Ratatui 界面支持流式文本与 reasoning、多行和 bracketed-paste 输入、鼠标与键盘滚动、协议/任务/设置浮窗、任务取消、会话回放，以及模型工作时的低噪声抖动动画。

| 按键 | 行为 |
| --- | --- |
| `Enter` | 发送消息 |
| `Shift+Enter` | 插入换行 |
| `F2` | 打开 Settings |
| `Ctrl+,` | 打开 Settings |
| `PageUp` / `PageDown` | 滚动对话或当前浮窗 |
| `F1` | 打开帮助 |
| `Ctrl+P` | 打开协议列表 |
| `Ctrl+T` | 打开受管理任务 |
| `Esc` | 停止编辑或关闭当前浮窗 |
| `Ctrl+C` | 退出 |

Settings 内使用 `↑/↓` 选择字段、`←/→` 浏览、`Enter` 搜索 Provider/模型或编辑凭据/输出上限、`x` 清除所选凭据、`s` 保存、`r` 刷新 pi 模型目录。

## 安装

```bash
git clone https://github.com/4fuu/uri-agent.git
cd uri-agent
cargo build --release
./target/release/uri-agent --cwd /path/to/project
```

也可以从当前 checkout 安装：

```bash
cargo install --path .
uri-agent --cwd /path/to/project
```

## 安全

内置 file 和 Shell 协议不提供沙箱。Agent 拥有 uri-agent 进程的文件与命令权限，也可以访问绝对路径。请只在可信项目和允许 Agent 使用其中凭据的环境中运行，并将 `auth.json`、`models.json`、项目设置与已发现 Skills 视为可信输入。

## 开发

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo check
```

仓库不变量和代码结构记录在 [`AGENTS.md`](AGENTS.md)。

## 许可证

[MIT](LICENSE) © 2026 4fuu

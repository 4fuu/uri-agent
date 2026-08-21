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
- **稳定会话** — 每个会话都会固化完整系统提示词和 Skill 元数据；模型消息、工具调用身份与压缩 checkpoint 保存在 SQLite，超长输出仍可通过 `file://` 完整读取。

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
~/.agents/skills
~/.claude/skills
~/.codex/skills
```

发现过程在进程启动时执行一次。每个扫描根目录可以直接包含 `SKILL.md`，也可以在其一级子目录中包含 `SKILL.md`；二进制不会固化开发者机器上的 Skill 列表。两个 Skill 规范化为相同协议名时，优先级更高的位置生效。

创建会话时只保存每个选中 Skill 的 `name`、`description` 和规范化后的 `SKILL.md` 路径，同时保存完整系统提示词。恢复会话时直接使用该快照，不会根据当前文件系统重新生成提示词。`://help` 和资源读取仍会访问保存的路径，因此 Skill 正文的后续修改可见；保存的文件被移除时会明确报错。其他位置后来出现的同名 Skill 不会让旧会话重新绑定。

### 扩展注册

Rust 扩展接口通过 [`PluginHost`](src/plugin.rs) 统一模型与界面贡献：

- 注册一个或多个 [`Protocol`](src/protocol.rs) 实现；
- 注册稳定的命令 ID、标题、描述与冒号别名，自动进入命令面板并可由 Rhai keymap 绑定；
- 注册异步 TUI panel provider，返回的文档支持滚动、鼠标选择与 OSC52 复制。

协议贡献仍隐藏在 `read` 和 `exec` 后面，注册命令或面板不会增加模型可见工具。内置协议使用相同的协议契约。URI Agent 当前不加载原生动态库；第三方 Rust 插件需要在应用组装阶段链接并注册。

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

没有 API key 也可以启动 uri-agent。使用 `F2`、`Ctrl+,`、Space 命令面板或 `:settings` 打开设置浮窗；Insert 模式仍支持 `/settings`、`/model` 和 `/login`。浮窗可以编辑 Provider、模型、当前 Provider 凭据、内联输出上限、编辑器与选取器命令，以及两者使用内嵌浮窗还是接管整个终端。保存后立即生效，不会丢弃当前会话。

### 文本文件

uri-agent 保持普通配置可编辑，并将程序生成数据与用户覆盖分离。Linux 默认配置目录为 `~/.config/uri-agent`；可以用 `URI_AGENT_CONFIG_DIR` 覆盖。

| 文件 | 维护者 | 用途 |
| --- | --- | --- |
| `settings.json` | uri-agent 和用户 | 与 pi 兼容的全局设置，包括 `defaultProvider` 与 `defaultModel` |
| `auth.json` | uri-agent 和用户 | 与 pi 兼容的 Provider 凭据；Unix 权限为 `0600` |
| `models-store.json` | uri-agent | 从 `pi.dev` 拉取的程序缓存 |
| `models.json` | 用户 | 与 pi 兼容的自定义 Provider、模型、headers 和模型覆盖 |
| `keymap.rhai` | 用户 | 覆盖内置现代模态默认值的全局 Rhai 快捷键 |
| `<cwd>/.uri-agent/settings.json` | 用户 | 可选的项目设置，覆盖全局设置 |
| `<cwd>/.uri-agent/keymap.rhai` | 用户 | 覆盖全局映射的可选项目快捷键 |

项目设置文件已存在时，TUI 会将 Provider、模型、输出上限、编辑器和选取器设置写入项目文件；否则写入全局设置。凭据始终写入全局 `auth.json`。

`settings.json` 示例：

```json
{
  "defaultProvider": "openai",
  "defaultModel": "gpt-5.2",
  "outputLimit": 32768,
  "editor": "hx",
  "editorMode": "float",
  "picker": "fzf",
  "pickerMode": "float"
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
| `--editor` | `URI_AGENT_EDITOR`, `VISUAL`, `EDITOR` | 设置外部编辑器命令 |
| `--editor-mode` | `URI_AGENT_EDITOR_MODE` | 选择 `float` 或 `fullscreen` 编辑器集成 |
| `--picker` | `URI_AGENT_PICKER` | 设置会话内容模糊选取器命令 |
| `--picker-mode` | `URI_AGENT_PICKER_MODE` | 选择 `float` 或 `fullscreen` 选取器集成 |
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

上下文压缩后，append-only 事件流仍保留原始模型消息。当预计重放内容接近当前模型在 pi 目录中的 `contextWindow` 时，uri-agent 会让模型生成可持久化摘要，将压缩 checkpoint 写入 SQLite，后续使用该摘要加完整的近期用户 turn。工具调用与对应结果不会被拆到 checkpoint 两侧。可以从命令行或命令面板运行 `:compact` 提前压缩；手动压缩至少需要一个已完成的旧 turn。

规范化后的启动目录就是项目边界。普通启动会为该项目创建新会话；`--continue-session` 只恢复存储目录与当前项目相同的最近会话；`--session <id>` 会拒绝属于其他项目的会话。TUI 暂不提供跨项目会话总览或目录选择器。

## TUI

Ratatui 界面将事件浏览、消息输入和详细查看分开。对话区为每条用户消息、回复、reasoning 片段或工具调用保留一条可选择的预览；工具调用及结果合并在同一行，因此流式思考和大型工具输出不会把有效对话持续推出屏幕。`Enter` 在可滚动浮窗中打开完整事件，`e` 使用配置的编辑器查看。相同列表支持滚轮和鼠标选择，双击事件即可查看详情。

| 模式 | 默认按键 | 行为 |
| --- | --- | --- |
| Browse | `↑/↓`、`Enter`、`i`、`e`、`/`、`y`、`Space`、`:`、鼠标 | 选择预览、查看详情、输入、查找事件、复制或执行命令 |
| Insert | `Enter`、`Shift+Enter`、`Ctrl+E`、`Esc` | 发送、换行、在外部编辑器编辑草稿或返回 Browse |
| Detail | `↑/↓`、`PageUp/PageDown`、`e`、`Esc`、拖选、滚轮 | 查看、选取、复制完整内容，或用编辑器打开 |
| 内嵌终端 | 外部程序正常按键、双 `Esc`、Shift 拖选 | 操作编辑器/选取器、关闭 PTY 或选取终端文本 |
| Global | `F1`、`F2`、`Ctrl+P`、`Ctrl+T`、`Ctrl+Shift+C`、`Ctrl+C` | 帮助、Settings、协议、任务、复制和退出 |

Browse 模式只借鉴 Helix 交互中适合 Agent 的部分，而不要求用户掌握 Vim：方向键和鼠标是一等操作，`j/k` 保留为别名，`Space` 打开可点击命令面板，`:` 打开命令行，`/` 打开全局会话内容选取器。命令包括 `:settings`、`:model`、`:login`、`:find`、`:copy`、`:tasks`、`:protocols`、`:compact`、`:compose`、`:detail`、`:editor`、`:help` 和 `:quit`；插件注册的命令也会出现在同一个命令面板和帮助浮窗。顶栏始终标明当前模式；帮助浮窗展示实际生效的 keymap，而不是固定按键表。模型工作时仍会显示低噪声抖动动画。

只读浮窗可以直接用鼠标拖选。交互式面板和内嵌终端使用 Shift 拖选，使普通点击仍能交给程序处理。按 `y` 或 `Ctrl+Shift+C` 通过 OSC52 复制选区；没有选区时，同一操作会复制当前可见面板。

### Rhai 快捷键

快捷键按以下顺序加载：

```text
内置默认值
< <config-dir>/keymap.rhai
< <project>/.uri-agent/keymap.rhai
```

每个 Rhai 脚本调用 `map(mode, key, action)` 或 `unmap(mode, key)`。按键名称使用 `enter`、`space`、`shift+g`、`ctrl+e` 等形式：

```rhai
map("browse", "x", "detail");
unmap("browse", "e");
map("insert", "ctrl+j", "newline");
```

可用模式包括 `global`、`browse`、`insert`、`detail`、`list`、`tasks`、`settings`、`palette`、`command`、`text`、`selection` 和 `terminal`。可用 action 会在 `F1` 帮助中显示，包括 `next`、`previous`、`finder`、`copy`、`insert`、`detail`、`editor`、`palette`、`command`、`send`、`newline`、`settings`、`protocols`、`tasks`、`escape`、`close` 和 `quit`。注册命令的 ID 同时也是稳定的 action ID，因此可直接绑定 `map("browse", "c", "compact")` 或插件命令 ID。脚本最多执行 100,000 个 Rhai 操作，不会获得宿主文件系统或进程 API。

### 外部编辑器与选取器

[Helix](https://github.com/helix-editor/helix) 是默认外部编辑器，其可执行文件名为 `hx`。它不是强依赖：未安装时其余 TUI 仍可使用，URI Agent 会提示如何更改编辑器，而不会退出。请参考 [Helix 官方安装说明](https://docs.helix-editor.com/install.html)，例如：

```bash
# macOS
brew install helix

# Arch Linux
sudo pacman -S helix
```

[fzf](https://github.com/junegunn/fzf) 是默认的会话内容选取器。安装后即可使用 `/`、`:find` 或命令面板中的查找操作：

```bash
# Debian/Ubuntu
sudo apt install fzf

# macOS
brew install fzf
```

两者默认都运行在真实 PTY 中，并渲染为 Ratatui 浮窗。需要让程序临时接管整个终端时，可在 Settings 或 `settings.json` 中将 `editorMode`、`pickerMode` 改为 `fullscreen`；程序返回后 URI Agent 会恢复 raw mode、鼠标捕获和 bracketed paste。命令本身可以在 Settings 中设置，也可用 `EDITOR`/`VISUAL`/`URI_AGENT_EDITOR` 与 `URI_AGENT_PICKER` 覆盖。GUI 编辑器应附带等待参数（例如 `code --wait`）并使用 fullscreen 模式。

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

内置 file 和 Shell 协议不提供沙箱。Agent 拥有 uri-agent 进程的文件与命令权限，也可以访问绝对路径。请只在可信项目和允许 Agent 使用其中凭据的环境中运行，并将 `auth.json`、`models.json`、项目设置、Rhai keymap、编辑器命令与已发现 Skills 视为可信输入。

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

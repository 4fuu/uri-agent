# URI Agent

[English](README.md) | [简体中文](README.zh-CN.md)

[![License: MIT](https://img.shields.io/badge/license-MIT-6ed2c2.svg)](LICENSE)

URI Agent 是一个终端 coding agent，以精简且稳定的模型接口为核心。模型始终只看到 `read` 和 `exec` 两个工具，并通过 `file://...`、`bash://...` 等协议地址访问文件、Shell、编辑、Skills 及后续扩展。

这种设计只在能力真正有用时才把对应说明加载到模型上下文。每个协议都在 `<protocol>://help` 提供自身文档；长时间工作由系统管理的任务表示；超长输出不会被丢弃，而是保留为可读取的 `file://` 地址。

> [!WARNING]
> URI Agent 不提供沙箱。文件与 Shell 协议使用 `uri-agent` 进程本身的权限运行。请只在可信的项目和环境中使用。

## 快速开始

### 环境要求

- stable Rust 工具链与 Git；
- 受支持 Provider 的 API key；
- 支持标准键盘输入的终端。鼠标支持可选。

### 从源码安装

```bash
git clone https://github.com/4fuu/uri-agent.git
cd uri-agent
cargo install --path .
```

### 启动会话

URI Agent 不会预设默认模型。在目标项目中启动后，运行 `:login` 和 `:model`：

```bash
uri-agent --cwd /path/to/project
```

尚未配置时，界面显示 `尚未配置，请运行 :login`。`:login` 用于保存 API key 或完成 Anthropic OAuth；`:model` 从可运行目录中选择模型。

发送第一条请求：

1. 按 `i` 打开输入浮窗。
2. 输入请求；`Shift+Enter` 换行。
3. 按 `Enter` 发送。`Esc` 会把草稿保存在 SQLite 中。

按 `:` 打开命令面板，或按 `F1` 查看当前生效的快捷键与命令参考。

## 为什么使用协议

增加新能力时，模型可见的工具面不会增长：

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

URI Agent 只按第一个 `://` 分割地址。剩余 target 是不透明数据：注册表不会对它进行 URL 解码、规范化或重新解释。`body` 可以是任意 JSON 值，并原样传给选中的协议。

协议通过 [`Protocol`](src/protocol.rs) trait 实现 `read`、`exec` 或二者。注册新协议不会增加新的模型可见工具。

### 内置协议

| 协议 | 操作 | 用途 |
| --- | --- | --- |
| `file` | `read` | 读取文件和有长度限制的目录列表 |
| `edit` | `read`, `exec` | 原子写入文件，或替换唯一精确匹配 |
| `bash` | `read`, `exec` | 安装 Bash 时，将 Bash 命令作为系统管理的任务运行 |
| `pwsh` | `read`, `exec` | 安装 `pwsh` 时，将 PowerShell 7 命令作为系统管理的任务运行 |
| `<name>-skill` | `read` | 加载一个已发现的 Skill 及其附属资源 |

程序会在启动时检测 `bash` 与 `pwsh`，只有找到相应可执行文件时才注册协议。

### 系统管理的任务与完整输出

执行默认是异步的。Shell 或 edit 请求通常会立即返回任务地址：

```text
exec("bash://run", "cargo test")
→ Read status: bash://tasks/<id>
```

Shell 协议支持 `?wait=N`，最多等待 300 秒：

```text
exec("bash://?wait=30", "cargo test")
```

等待超时后任务仍会继续运行。通过 `<protocol>://tasks/<id>` 读取当前状态和最终结果。等待是 Shell 协议自身的功能，不是通用 URI 行为。

工具结果超过内联上限时，URI Agent 会返回头尾预览，并将完整字节保存到会话输出目录。预览中包含可读取完整结果的 `file://` 地址。

## Skills

URI Agent 启动时会按以下优先级发现一次 Skills：

```text
<project>/.agents/skills
<project>/.claude/skills
<project>/.codex/skills
~/.agents/skills
~/.claude/skills
~/.codex/skills
```

每个根目录可以直接包含 `SKILL.md`，也可以在一级子目录中包含 `SKILL.md`。Skill 必须提供 `name` 和 `description` YAML frontmatter；规范化后的名称会成为独立协议：

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

同一规范化协议名只采用第一个 Skill。与已注册内置协议冲突的 Skill 会被跳过，并产生一条通知。资源读取不能越出 Skill 目录。

新建会话时，程序会固化完整的系统提示词，以及每个选中 Skill 的名称、描述和规范化 `SKILL.md` 路径。恢复会话时复用该快照。Skill 帮助和资源仍从固化路径读取，因此文件缺失会明确失败，其他位置的同名 Skill 也不能静默替换它。

## 模型与认证

URI Agent 使用 [pi](https://github.com/badlogic/pi-mono) 模型目录，目前通过 Rust/Rig 后端运行以下 API family：

- `openai-responses`
- `openai-completions`
- `anthropic-messages`
- `google-generative-ai`

模型选择器只展示可运行的 API family。`:login` 对齐 Pi Agent：API key，以及 Anthropic、OpenRouter、OpenAI Codex、GitHub Copilot、Kimi Code、xAI 和 Radius 的 OAuth。存储格式与 Pi 的 `auth.json` 相同（`type: "api_key"` 或 `type: "oauth"`）。

目录缓存四小时。使用 `--offline`、`URI_AGENT_OFFLINE=1` 或 `PI_OFFLINE=1` 可禁用目录请求，只使用本地数据。在模型选择器或 Settings 中按 `Ctrl+R` 可刷新目录。

## 配置

按 `:settings` 可查看当前 Provider、模型、凭据状态和输出上限。凭据用 `:login` / `:logout`，换模型用 `:model`。更改会立即应用到当前会话。

Linux 默认配置目录为 `~/.config/uri-agent`；可通过 `URI_AGENT_CONFIG_DIR` 指定其他位置。

| 文件 | 用途 |
| --- | --- |
| `settings.json` | 全局 Provider、模型、输出和终端设置 |
| `auth.json` | 全局 Provider 凭据；Unix 上创建为 `0600` 权限 |
| `models.json` | 用户定义的 Provider、模型、header 和模型覆盖 |
| `models-store.json` | 程序生成的 pi 目录缓存 |
| `keymap.rhai` | 全局快捷键覆盖 |
| `<project>/.uri-agent/settings.json` | 可选的项目设置 |
| `<project>/.uri-agent/keymap.rhai` | 可选的项目快捷键覆盖 |

项目设置覆盖全局设置，环境变量覆盖文件，命令行参数覆盖环境变量。凭据从低到高采用以下优先级：

```text
models.json apiKey
< auth.json
< Provider 环境变量
< URI_AGENT_API_KEY
< --api-key
```

已知 Provider 使用常见的环境变量，包括 `OPENAI_API_KEY`、`ANTHROPIC_API_KEY`、`GEMINI_API_KEY`、`OPENROUTER_API_KEY` 和 `GROQ_API_KEY`。

可信配置中的凭据与 header 值支持 pi 风格的环境变量展开，以及开头的 `!shell command`。开头的 `!` 会使用 URI Agent 的权限执行命令，因此不要使用不可信项目提供的配置。

### 常用命令行选项

```text
--provider <ID>          选择 Provider
--model <ID>             选择该 Provider 的模型
--api-key <KEY>          设置仅当前进程使用的凭据
--cwd <PATH>             设置项目和协议工作目录
--continue-session       恢复该项目最近的会话
--session <ID|latest>    恢复指定会话
--output-limit <BYTES>   设置内联输出大小（最小 1024）
--offline                禁用 pi 目录网络请求
```

运行 `uri-agent --help` 可查看当前 CLI 参考。

### 自定义 OpenAI 兼容 Provider

如需使用本地或自定义 endpoint，可在 `models.json` 中增加 Provider：

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

## 会话与上下文

会话以 append-only 方式保存在 SQLite：

```text
<platform-data-dir>/uri-agent/sessions.db
```

规范化后的 `--cwd` 目录是项目边界。`--continue-session` 只恢复该项目最近的会话；`--session <id>` 会拒绝为其他项目创建的会话。

模型重放内容接近所选模型的上下文窗口时，URI Agent 会创建摘要 checkpoint，并使用摘要和完整的近期用户 turn 继续重放。原始事件仍保留在 SQLite 中，工具调用绝不会与对应结果分离。运行 `:compact` 可以请求提前创建 checkpoint。

## 终端界面

启动时先播放短动画，随后进入单一会话界面。空会话保留动画品牌页；一旦出现记录，界面变为默认折叠的历史，底部是 pi 风格的页脚：第一行 `cwd (分支)`，第二行是累计 token 用量、成本、上下文占用和当前模型，最后一行是提示。

| 界面 | 常用默认按键 |
| --- | --- |
| 会话 | `i` 输入，`:` 命令，`?` 帮助，`Enter` 展开/折叠，`r`/`t`/`h` 在思维链/工具/用户消息间跳转 |
| 输入浮窗 | `Enter` 发送，`Shift+Enter` 换行，`Esc` 保留草稿 |
| 命令面板 | 输入过滤，`Tab` 补全，`Enter` 执行，`Esc` 关闭 |
| 全局 | `F1` 帮助，`F2` 设置，`F3` 模型，`Ctrl+C` 退出 |

常用冒号命令：`:login`、`:logout`、`:model`、`:resume`、`:new`、`:set-terminal`、`:terminal`、`:compact`、`:help`、`:q`。

`:set-terminal` 保存浮窗终端命令（如 `pwsh`、`bash`）。`:terminal` 以 PTY 浮窗打开；连按两次 `Esc` 关闭。Shift 拖选文字，普通点击仍交给终端程序。

方向键和鼠标是一等操作，`j`、`k` 只是可选别名。只读视图支持鼠标选择与 OSC52 复制。

快捷键按内置默认值、全局 `keymap.rhai`、项目 `keymap.rhai` 的顺序分层加载。每个文件都可以映射或移除 action：

```rhai
map("main", "x", "copy");
unmap("main", "j");
map("composer", "ctrl+j", "newline");
```

## 扩展 URI Agent

Rust 扩展通过 [`PluginHost`](src/plugin.rs) 注册协议、命令和通用 TUI panel provider。协议始终位于 `read` 和 `exec` 之后；命令会进入命令面板、冒号命令行和快捷键 action 注册表。URI Agent 当前不加载原生动态库，因此第三方 Rust 扩展必须在应用组装阶段链接。

## 开发

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo check
```

仓库不变量、模块职责和修改要求记录在 [`AGENTS.md`](AGENTS.md)。

## 许可证

[MIT](LICENSE) © 2026 4fuu

# URI Agent

一个协议驱动的 Rust coding agent。无论安装多少能力，模型始终只看到两个工具：

```text
read(uri: string, body?: any)
exec(uri: string, body?: any)
```

系统提示词只提供协议名称、用途和帮助地址。模型在需要某项能力时读取 `<protocol>://help`，让工具说明、Skill 和大型输出都按需进入上下文。

## 特性

- **固定工具面**：协议数量不会扩大模型的 tool schema。
- **不限制 payload**：`body` 可以是字符串、Markdown、数组、对象或其他 JSON 值，并原样传给协议。
- **异步优先**：`exec` 默认返回任务地址；`?wait=N` 可以请求最多 300 秒的有界等待，超时不会取消任务。
- **完整输出可追溯**：超限内容会保存到文件，并返回可继续读取的 `file://` 地址。
- **一个 Skill，一个协议**：例如 `Code Review` 注册为 `code-review-skill://`，避免共享 Skill 入口降低调用意愿。
- **多模型后端**：通过 Rig 接入 OpenAI Responses、Anthropic 和 Gemini，同时由项目持有工具循环和会话格式。
- **终端优先**：Ratatui 界面提供多行输入、流式回复、鼠标与滚动、协议/任务浮窗和低噪声像素动画。
- **可恢复会话**：append-only JSONL 保存模型消息、工具关联信息和界面事件。

## 快速开始

需要 Rust stable，以及所选模型提供商的 API key。

```bash
git clone https://github.com/4fuu/uri-agent.git
cd uri-agent

export OPENAI_API_KEY=...
cargo run --release -- --cwd /path/to/project
```

可选后端和默认模型：

| Provider | 环境变量 | 默认模型 |
| --- | --- | --- |
| OpenAI | `OPENAI_API_KEY` | `gpt-5.2` |
| Anthropic | `ANTHROPIC_API_KEY` | `claude-sonnet-4-6` |
| Gemini | `GEMINI_API_KEY` | `gemini-3-flash-preview` |

通过参数覆盖 provider 或模型：

```bash
cargo run --release -- \
  --provider anthropic \
  --model claude-sonnet-4-6 \
  --cwd /path/to/project
```

恢复最近一次会话：

```bash
cargo run --release -- --session latest --cwd /path/to/project
```

> [!WARNING]
> 内置文件和 Shell 协议不提供沙箱。Agent 拥有当前进程的文件与命令执行权限，请只在可信目录和可接受的凭据环境中运行。

## 工作方式

地址只按第一个 `://` 分成协议名称和 opaque target，不进行 RFC URL 解析、百分号解码或统一格式化：

```text
odd protocol://a://b?x=a b
└── protocol ──┘└ opaque target ┘
```

协议由 [`Protocol`](src/protocol.rs) trait 注册，可以实现 `read`、`exec` 或二者。协议收到原始 URI、target 和 body；共享的 `TaskManager` 负责状态、等待、取消和通知。

```text
Model
  │
  ├─ read(uri, body) ─┐
  └─ exec(uri, body) ─┤
                      ▼
              Protocol Registry
                 │    │    │
               file  edit  bash/pwsh ... skills
                        └──────┬──────┘
                               ▼
                         Task Manager
```

### 异步任务

Shell 与 edit 默认立即返回任务 URI：

```text
exec("bash://run", "cargo test")
→ Read status: bash://tasks/<id>
```

需要即时结果时，可以选择一个等待窗口：

```text
exec("bash://?wait=30", "cargo test")
```

任务在 30 秒内结束时直接返回结果；否则返回任务 URI并继续后台执行。之后使用 `read("bash://tasks/<id>")` 获取状态和输出。

## 内置协议

模型会按需读取各协议的 `://help`。以下是面向开发者的索引：

| 协议 | 入口 | 用途 |
| --- | --- | --- |
| `file` | `read` | 读取文件或目录；支持 `?offset=1&limit=200` |
| `edit` | `read`, `exec` | 原子写入文件，或执行唯一匹配的精确替换 |
| `bash` | `read`, `exec` | 在检测到 Bash 时注册并管理异步命令 |
| `pwsh` | `read`, `exec` | 在检测到 PowerShell 7 时注册并管理异步命令 |
| `<name>-skill` | `read` | 返回 Skill 提示词及其附属文件 |

## Skills

每个 `SKILL.md` 必须包含 `name` 和 `description` YAML frontmatter。其名称会被规范化为独立协议，例如：

```text
name: Code Review
protocol: code-review-skill://
help: code-review-skill://help
resource: code-review-skill://scripts/check.py
```

读取 `://help` 时，系统会在 `SKILL.md` 后附加 Skill 的真实 `file://` 目录，方便模型运行脚本或筛选资源。附属文件只能在 Skill 目录内读取，`..` 和符号链接不能越过目录边界。

扫描顺序按项目优先、用户与缓存其次：

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

同名协议冲突时，保留优先级更高的 Skill。

## TUI 快捷键

| 按键 | 行为 |
| --- | --- |
| `Enter` | 发送消息 |
| `Shift+Enter` | 换行 |
| `PageUp` / `PageDown` | 滚动对话或浮窗 |
| `F1` | 打开帮助浮窗 |
| `Ctrl+P` | 打开协议浮窗 |
| `Ctrl+T` | 打开任务浮窗 |
| `x` | 在任务浮窗中取消所选任务 |
| `Esc` | 关闭浮窗 |
| `Ctrl+C` | 退出 |

## 数据与恢复

- 会话：平台数据目录的 `uri-agent/sessions/<session-id>/events.jsonl`
- 完整工具输出：平台缓存目录的 `uri-agent/outputs/<session-id>/`
- 平台目录不可用时，会话回退到当前目录的 `.uri-agent/`

会话日志同时保存 provider-neutral 的完整模型消息，因此工具调用 ID、reasoning 签名和工具结果关联能在恢复后继续使用。不完整的最后一条 JSONL 写入会在下次打开时安全修剪。

## 开发

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo check
```

代码布局和必须保持的协议不变量见 [`AGENTS.md`](AGENTS.md)。

## License

[MIT](LICENSE) © 2026 4fuu

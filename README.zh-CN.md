# URI Agent

[English](README.md) | [简体中文](README.zh-CN.md)

[![License: MIT](https://img.shields.io/badge/license-MIT-6ed2c2.svg)](LICENSE)

URI Agent 是一个终端 coding agent，核心是固定且精简的模型接口。模型始终只看到 `read` 和 `exec` 两个工具，并通过 `file://...`、`bash://...` 等协议地址访问文件、Shell、编辑能力、Skills 与扩展。

协议按需从 `<protocol>://help` 加载操作说明。长时间操作会成为系统管理的任务；超长输出仍可通过 `file://` 地址读取；会话历史则保存在 SQLite 中。

> [!WARNING]
> URI Agent 不提供沙箱。文件与 Shell 协议使用 `uri-agent` 进程本身的权限运行。请只使用可信的项目和配置。

## 为什么使用 URI Agent

- **稳定的工具面：**增加能力不会增加新的模型工具 schema。
- **渐进式上下文：**只有模型读取协议或 Skill 帮助时，相应说明才进入上下文。
- **可观察的执行过程：**异步工作的状态和最终输出都通过协议的读取路由提供。
- **持久化会话：**草稿、事件、固化的会话上下文和压缩 checkpoint 都能跨重启保留。
- **完整键盘操作的 TUI：**会话、输入、命令、模型选择、设置和终端集中在同一界面。

模型可见接口始终是：

```text
read(uri: string, body?: any)
exec(uri: string, body?: any)
```

## 快速开始

### 环境要求

- stable Rust 工具链与 Git；
- 受支持模型 Provider 的凭据；
- 支持标准键盘输入的终端。鼠标支持可选。

### 从源码安装

```bash
git clone https://github.com/4fuu/uri-agent.git
cd uri-agent
cargo install --path .
```

### 启动第一个会话

启动 URI Agent，并指定允许它访问的项目目录：

```bash
uri-agent --cwd /path/to/project
```

如果项目根目录存在 `AGENTS.md`，URI Agent 会在每个新会话的系统提示词中包含其指令。恢复已有会话时，仍使用该会话创建时固化的副本。

URI Agent 不会自动选择默认模型。在 TUI 中：

1. 运行 `:login` 保存 API key 或完成受支持的 OAuth 登录。
2. 运行 `:model` 并选择一个可运行模型。
3. 按 `i`，输入请求，再按 `Enter` 发送。

配置完成后，欢迎界面会显示选中的 Provider、模型和思考强度，不再显示配置提示；随后提交的请求与响应会出现在会话中。

`Shift+Enter` 插入换行，`Esc` 关闭输入浮窗并保留草稿，`:` 打开可搜索的命令面板，`Tab` 补全匹配的命令，`F1` 显示当前生效的快捷键与命令参考。命令别名会参与搜索，但不会使默认列表变得拥挤。搜索内容只用于筛选命令；需要设置值的命令会打开选择面板或独立输入浮窗。

## 协议示例

```text
read("file://src/main.rs?offset=1&limit=200")
read("code-review-skill://help")
exec("bash://?wait=30", "cargo test")  # Unix-like 系统
exec("pwsh://?wait=30", "cargo test")  # Windows
```

非 Windows 平台只启用 Bash。在 Windows 上，PowerShell 7 或更高版本会启用 `pwsh` 并关闭 `bash`；如果 PowerShell 7 不可用，URI Agent 会显示警告，已安装的 Bash 仍会保持启用。

URI Agent 只按第一个 `://` 分割地址；剩余 target 由选中的协议负责解释。注册表会原样传递可选 JSON `body`。完整设计和内置协议清单见英文文档 [Protocols, tasks, Skills, and extensions](docs/protocols.md)。

## 文档

[`docs/` 索引](docs/README.md)按任务组织详细的英文文档：

- [Protocols, tasks, Skills, and extensions](docs/protocols.md) — 模型接口约束、内置协议、异步执行、完整输出保留、Skill 发现与插件注册。
- [Models and configuration](docs/configuration.md) — 模型目录、认证、配置优先级、CLI 参数、thinking 等级与自定义 Provider。
- [Terminal interface and sessions](docs/interface.md) — 输入与命令、快捷键、内嵌终端、图片附件、持久化、会话恢复与上下文压缩。
- [Architecture and development](docs/development.md) — 模块职责、不可变约束、修改规则与验证要求。

程序运行时，协议支持的 URI 和 body 格式以 `<protocol>://help` 为准。

## 开发

项目使用 stable Rust。提交代码修改前运行：

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo check
```

修改仓库前请阅读 [`AGENTS.md`](AGENTS.md)；详细的模块职责和测试规则见[开发文档](docs/development.md)。

## 许可证

[MIT](LICENSE) © 2026 4fuu

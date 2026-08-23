# URI Agent

[English](README.md) | [简体中文](README.zh-CN.md)

[![License: MIT](https://img.shields.io/badge/license-MIT-6ed2c2.svg)](LICENSE)

URI Agent 是一个终端 coding agent，核心是固定且精简的模型接口。模型始终只看到 `read` 和 `exec` 两个工具，并通过 `file://...`、`bash://...` 等协议地址访问文件、Shell、编辑能力、Skills 与扩展。

协议按需从 `<protocol>://help` 加载操作说明。长时间操作会成为系统管理的任务；超长输出仍可通过 `file://` 地址读取；会话历史则保存在 SQLite 中。

> [!WARNING]
> URI Agent 不提供沙箱。文件与 Shell 协议使用 `uri-agent` 进程本身的权限运行。请只使用可信的项目和配置。

URI Agent 仍处于早期发布阶段，不同日期版本之间可能发生变化。模型请求及其所需上下文会发送给你选择的 Provider；除非启用离线模式，URI Agent 还会从 pi.dev 获取模型目录元数据。

## 为什么使用 URI Agent

- **稳定的工具面：**增加能力不会增加新的模型工具 schema。
- **渐进式上下文：**只有模型读取协议或 Skill 帮助时，相应说明才进入上下文。
- **内置网络能力：**通过已登录的 Parallel 或 Exa 服务搜索和提取 HTTPS 网页；两者均未配置时在本地转换网页。
- **可观察的执行过程：**异步工作的状态和最终输出都通过协议的读取路由提供。
- **便携的可信扩展：**Extism WASM 模块可以添加支持热重载的协议，而不改变模型工具面。
- **持久化会话：**草稿、事件、固化的会话上下文和压缩 checkpoint 都能跨重启保留。
- **运行中调整方向：**当前 turn 运行时，可以排队稍后跟进，也可以引导下一次模型请求；尚未生效的消息仍可编辑。
- **可复用的 Agent 环境变量：**token 只需保存一次，后续 Agent Shell 命令即可使用，同时不会写入项目和会话文件。
- **完整键盘操作的 TUI：**会话、输入、命令、模型选择、设置和终端集中在同一界面，并支持 `@` 文件引用和 `@@` 会话引用。

模型可见接口始终是：

```text
read(uri: string, body?: any)
exec(uri: string, body?: any)
```

## 快速开始

### 环境要求

- 受支持模型 Provider 的凭据；
- 支持标准键盘输入的终端。鼠标支持可选。

### 安装

在 macOS 上使用 Homebrew：

```bash
brew tap 4fuu/uri-agent https://github.com/4fuu/uri-agent
brew install 4fuu/uri-agent/uri-agent
```

在 64 位 Windows 上使用 Scoop：

```powershell
scoop bucket add uri-agent https://github.com/4fuu/uri-agent
scoop install uri-agent
```

在 x86-64 或 ARM64 Linux 上，安装脚本会校验 Release checksum，并写入 `~/.local/bin`：

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://raw.githubusercontent.com/4fuu/uri-agent/main/scripts/install.sh | sh
```

如需从 crates.io 构建，请先安装 stable Rust 工具链，再运行：

```bash
cargo install --locked uri-agent
```

如需构建仓库中的当前代码：

```bash
git clone https://github.com/4fuu/uri-agent.git
cd uri-agent
cargo install --locked --path .
```

### 启动第一个会话

启动 URI Agent，并指定它应使用的项目目录：

```bash
uri-agent --cwd /path/to/project
```

`--cwd` 设置项目和默认工作目录，不是文件系统访问边界。如果项目包含 `AGENTS.md`，请在启动前检查它；新会话会包含其中的指令，并固化启动上下文。

URI Agent 不会自动选择默认模型。在 TUI 中：

1. 运行 `:login` 保存 API key 或完成受支持的 OAuth 登录。
2. 运行 `:model` 并选择一个可运行模型。
3. 按空格键，输入一个小型只读请求，例如 `读取顶层文件并说明这个项目的用途，不要修改文件。`，再按 `Enter` 发送。

当界面出现协议活动，并且 assistant 根据项目内容返回答案时，第一个会话就已正常工作。按 `F1` 或运行 `:help` 可查看当前生效的命令和快捷键。

运行 `:set-env` 可保存 `NPM_TOKEN` 等变量；后续 Agent Shell 命令会自动收到这些变量。设置界面只列出变量名，不显示变量值。全局作用域、私密文件存储、与 `:terminal` 的隔离以及插件访问规则见英文文档 [Agent environment](docs/configuration.md#agent-environment)。

受支持的 API 系列、认证、离线模式和自定义端点见英文文档 [Models and configuration](docs/configuration.md)。

## 协议示例

```text
read("file://src/main.rs?offset=1&limit=200")
read("sessions://search", {"query":"refresh token"})
read("https://search?limit=10", "stable Rust release notes")
read("https://www.rust-lang.org/")
read("uri-agent-docs://README.md")
read("code-review-skill://help")
exec("bash://?wait=30", "cargo test")  # Unix-like 系统
exec("pwsh://?wait=30", "cargo test")  # Windows
```

URI Agent 在第一个 `://` 处选择协议；剩余 target 和可选 JSON body 由该协议负责解释。通过 `:login` 保存 Parallel 或 Exa API key 后，内置 `https` 协议会使用该服务商进行搜索和网页提取。两个网络服务商均未登录时，搜索会请求登录，网页读取则回退到本地 HTTPS 获取和 HTML-to-Markdown 转换。`uri-agent-docs` 让与当前版本匹配的文档可从任意启动目录读取。可用的 Shell 协议取决于平台。精确的运行时契约以 `<protocol>://help` 为准；共享设计见英文文档 [Protocols, tasks, output, and Skills](docs/protocols.md)。

## 扩展

可信 WASM 模块可以添加运行时加载的协议。这里的 WASM 是便携 ABI，不是安全边界；启用的插件拥有文件系统、HTTP、WASI 和内置协议访问能力，其用户权限与 URI Agent 相同。直接读取已保存的 Agent 环境变量或 Provider API key 时，插件源码必须显式申请相应能力；它只是审计标记，不是批准流程。安装、reload、ABI、SDK 用法和可靠性限制见英文文档 [WASM plugins](docs/plugins.md)。

## 文档

[`docs/` 索引](docs/README.md)按任务组织详细的英文文档：

- [Protocols, tasks, output, and Skills](docs/protocols.md) — 模型路由、内置协议、执行语义、受管任务、完整输出保留与 Skill 发现。
- [WASM plugins](docs/plugins.md) — 安装、reload、信任边界、ABI、SDK 与运行限制。
- [Models and configuration](docs/configuration.md) — 模型目录、认证、配置优先级、CLI 参数、thinking 等级与自定义 Provider。
- [Terminal interface and sessions](docs/interface.md) — 输入与命令、快捷键、内嵌终端、图片附件、持久化、会话恢复与上下文压缩。
- [Architecture and development](docs/development.md) — 模块职责、不可变约束、修改规则与验证要求。

程序运行时，协议支持的 URI 和 body 格式以 `<protocol>://help` 为准。

## 开发

项目使用 stable Rust。修改仓库前请阅读 [`AGENTS.md`](AGENTS.md)；模块职责、修改规则和必需的验证见[开发文档](docs/development.md)。

## 许可证

[MIT](LICENSE) © 2026 4fuu

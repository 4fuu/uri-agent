<p align="center">
  <img src="docs/assets/logo.svg" width="420" alt="URI Agent 标志">
</p>

# URI Agent

[English](README.md) | [简体中文](README.zh-CN.md)

[![License: MIT](https://img.shields.io/badge/license-MIT-6ed2c2.svg)](LICENSE)

URI Agent 是一个以 URI 协议为核心的终端 coding agent；对于参数复杂或转义密集的操作，则提供有类型的直接工具。内置 Rust 插件会注册四个工具，可信 WASM 插件还可以在运行时添加更多有类型工具：

```text
read(uri: string, body: string)
exec(uri: string, body: string)
replace(path: string, old_text: string, new_text: string)
apply_patch(patch: string)
```

`read` 和 `exec` 的 body 始终必填且固定为字符串。协议不需要 body 时传
`""`，文本输入直接传字符串，结构化协议输入则传完整序列化后的 JSON 文本。

URI Agent 把其他 Agent 加载 Skills 的方式延伸到大多数能力：启动时只暴露简短的名称与描述，选中后才加载完整指令和资源。因此，新会话只预载路由规则、每个协议的一行描述和当前工具 schema；详细协议契约保留在 `<protocol>://help`，Skill 正文与文档保留在各自协议之后，超长结果则保留在 `file://` 地址之后。这既延续了极致的按需加载和上下文渐进理念，又让直接编辑工具避开 JSON 字符串内的二次转义。

输入只是简单字符串的能力只需增加一条协议索引，不必预载整份说明书；有类型或转义密集的能力则可以通过同一插件系统注册直接工具。Shell 命令较短时直接返回，运行较久时自动转为后台受管任务，并在完成后主动通知模型，无需轮询；会话历史以只追加方式保存在 SQLite 中。

使用支持图片输入的模型时，内置 `file` 协议会把读取到的 PNG、JPEG、GIF 和 WebP 文件直接返回给模型查看。

> [!WARNING]
> URI Agent 不提供沙箱。文件与 Shell 协议使用 `uri-agent` 进程本身的权限运行。请只使用可信的项目和配置。

URI Agent 仍处于早期发布阶段，不同日期版本之间可能发生变化。模型请求及其所需上下文会发送给你选择的 Provider；除非启用离线模式，URI Agent 还会从 pi.dev 获取模型目录元数据，并在配置凭据后访问受支持 Provider 的模型列表 API。

## 渐进式启动上下文

```text
精简路由规则 + 协议索引 + 当前工具 schema
    → read("<protocol>://help", "")
    → 只加载该协议的契约
    → 执行当前任务所需的读取与操作
```

Skills 也遵循同一路径：启动时只加入每个已发现 Skill 的名称和描述；模型选择该 Skill 后，才会加载它的 `SKILL.md` 与配套资源。实际启动上下文还会加入项目的 `AGENTS.md`（如果存在）。直接工具会贡献有类型的 schema，但不会额外预载一整份手册。

## 为什么使用 URI Agent

- **URI 原生的上下文渐进：**一个地址空间覆盖所有资源和操作，并像加载 Skills 一样，只在需要时载入契约、指令、资源和完整输出。
- **调用可靠且易于扩展：**稳定的字符串 `read` / `exec` 契约处理简单协议，有类型的直接工具避免嵌套转义，而且两条路径都由插件注册。
- **最新模型与登录方式：**把 pi.dev 的云端目录和 Provider 登录方式与按凭据隔离的实时发现结合起来，在共享目录更新前即可选择 Provider 新上线的可运行模型。
- **工作持久且可观察：**受管任务公开状态和最终输出，完成时自动通知模型，并随会话恢复已结束的报告；只追加会话、草稿、固化上下文与压缩 checkpoint 都能跨重启保留。
- **统一且可控的终端工作流：**内置网络访问、实时 Queue 与 Steer、完整键盘操作以及 `@` 文件和 `@@` 会话引用都集中在同一界面。

## pi.dev 模型覆盖

本项目兼容 pi.dev 中分发的模型配置。截至 2026-08-26，URI Agent 已实现的 API 系列及 Provider 实时发现覆盖：

| 目录指标 | 已支持 |
| --- | ---: |
| API 系列 | 9 个中的 5 个 |
| 模型条目 | 1,307 个中的 1,107 个（84.7%） |
| Provider ID | 39 个中的 35 个 |
| 支持实时发现的 Provider ID | 35 个可运行 Provider 中的 28 个 |

Provider 实时结果按凭据隔离缓存，只补充而不取代 pi.dev 元数据。目录内容和账户权限会变化；目录中的模型仍需匹配的凭据、地区和订阅。精确的发现覆盖、API 系列与认证要求见英文文档 [Models and configuration](docs/configuration.md#model-catalog)。

## 快速开始

### 环境要求

- 受支持模型 Provider 的凭据；
- 支持标准键盘输入的终端。鼠标支持可选。

### 安装

在 Apple Silicon macOS 上使用 Homebrew：

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

如需由外部进程管理器监督常驻插件，可使用 `uri-agent --background`：它不启动 TUI，但会刻意保持前台阻塞。生命周期与功能边界见英文文档 [WASM plugins](docs/plugins.md#resident-plugins)。

## 扩展

可信 WASM 模块可以添加运行时加载的协议和有类型的直接工具。这里的 WASM 是便携 ABI，不是安全边界；启用的插件拥有文件系统、HTTP、WASI 和内置协议访问能力，其用户权限与 URI Agent 相同。直接读取已保存的 Agent 环境变量或 Provider API key 时，插件源码必须显式申请相应能力；它只是审计标记，不是批准流程。安装、reload、ABI、SDK 用法和可靠性限制见英文文档 [WASM plugins](docs/plugins.md)。

## 文档

[`docs/` 索引](docs/README.md)按任务指向各篇英文文档，分别覆盖协议、启动上下文与 Skills、WASM 插件、模型与配置、终端界面、会话与上下文压缩、开发以及发布。

程序运行时，协议支持的 URI 和 body 格式以 `<protocol>://help` 为准。

## 开发

项目使用 stable Rust。修改仓库前请阅读 [`AGENTS.md`](AGENTS.md)；模块职责、修改规则和必需的验证见[开发文档](docs/development.md)。

## 许可证

[MIT](LICENSE) © 2026 4fuu

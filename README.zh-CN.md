<p align="center">
  <img src="docs/assets/logo.svg" width="420" alt="URI Agent 标志">
</p>

# URI Agent

[English](README.md) | [简体中文](README.zh-CN.md)

[![License: MIT](https://img.shields.io/badge/license-MIT-6ed2c2.svg)](LICENSE)

URI Agent 是一个以协议为核心的终端 coding agent。通过 URI 协议，它可以让任意工具像其他 Agent 中的 Skills 一样按需加载：模型起初只看到精简的名称和描述，选中工具后才读取完整契约、指令和资源。

内置插件向模型注册一组精简接口：

```text
read(uri: string, body: string)
exec(uri: string, body: string)
replace(path: string, old_text: string, new_text: string)
apply_patch(patch: string)
```

`read` 和 `exec` 通过 URI 协议处理简单字符串输入，body 始终是字符串，无内容时也要传 `""`。参数不适合编码成字符串时则使用有类型工具。可信 WASM 插件可以在运行时注册更多协议和有类型工具。

> [!WARNING]
> URI Agent 不提供沙箱。文件与 Shell 协议以及已启用的 WASM 插件都拥有 `uri-agent` 进程的用户权限。请只使用可信的项目、配置和插件。

URI Agent 仍处于早期发布阶段，不同日期版本之间可能发生变化。模型请求及其上下文会发送给你选择的 Provider；除非启用离线模式，URI Agent 还会从 pi.dev 和受支持的 Provider 获取模型目录元数据。

## 为什么使用 URI Agent

- **渐进式上下文：**协议契约、Skill 资源、内置文档和超长输出只在需要时进入模型上下文。
- **可扩展工具：**内置 Rust 插件和可信 WASM 插件都可以添加协议或有类型工具，无需改变运行时分发路径。
- **广泛模型支持：**pi.dev 目录、Provider 专属登录和按凭据隔离的实时发现，把广泛的 Provider 生态集中在同一个模型选择器中。
- **持久化工作：**长命令会转为受管任务；只追加 SQLite 会话会跨重启保留草稿、固化的启动上下文和压缩 checkpoint。
- **单一终端工作流：**Queue 与 Steer、内置网络访问、键盘和鼠标控制、图片输入，以及 `@` 文件和 `@@` 会话引用都集中在同一个对话界面。

## 模型与 Provider 覆盖

URI Agent 的目标是广泛兼容 pi.dev 模型生态，而不是只适配少数固定模型。截至 2026-08-30，当前目录覆盖如下：

| 目录指标 | 已支持 |
| --- | ---: |
| API 系列 | 9 个中的 5 个 |
| 模型条目 | 1,274 个中的 1,073 个（84.2%） |
| Provider ID | 39 个中的 35 个 |
| 支持实时发现的 Provider ID | 35 个可运行 Provider 中的 28 个 |

已支持 OpenAI Responses、OpenAI Codex Responses、OpenAI Chat Completions、Anthropic Messages 和 Google Generative AI 五类 API。Provider 实时结果按凭据隔离缓存，并补充共享目录，因此账户中新开放的模型可以在 pi.dev 收录前出现。

对于无法只靠通用目录兼容的 Provider，URI Agent 还提供专用集成：ChatGPT Codex 订阅 OAuth 与 WebSocket 传输、Cloudflare AI Gateway 的凭据安全端点边界、WorkBuddy 中国站浏览器登录与账户模型发现，以及明确标记为实验性的 Antigravity 私有协议。此外还支持 Anthropic、GitHub Copilot、Kimi Coding、xAI、Radius 和 OpenRouter 的 Provider 专属登录流程。

目录内容和账户权限会变化；目录中的模型仍需匹配的凭据、地区和订阅。当前 Provider、实时发现、认证和兼容性细节见英文文档 [Models and configuration](docs/configuration.md#model-catalog)。

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

### 启动第一个会话

启动 URI Agent，并指定它应使用的项目目录：

```bash
uri-agent --cwd /path/to/project
```

`--cwd` 设置项目和默认工作目录，不是文件系统访问边界。如果项目包含 `AGENTS.md`，请在启动前检查它；新会话会包含其中的指令，并固化启动上下文。

URI Agent 不会自动选择默认模型。在 TUI 中：

1. 运行 `:login` 保存 API key 或完成受支持的 OAuth 登录。
2. 运行 `:model` 并选择一个可运行模型。
3. 按空格键，输入请求，再按 `Enter` 发送。

当界面出现协议活动，并且 assistant 根据项目内容返回答案时，第一个会话就已正常工作。按 `F1` 或运行 `:help` 可查看当前生效的命令和快捷键。

Provider 支持、认证、离线模式、环境变量和自定义端点见英文文档 [Models and configuration](docs/configuration.md)。

## 文档

| 目标 | 文档 |
| --- | --- |
| 选择 Provider、认证或修改设置 | [Models and configuration](docs/configuration.md) |
| 使用对话、命令、keymap、终端或附件 | [Terminal interface](docs/interface.md) 和 [terminal features](docs/terminal.md) |
| 了解工具、协议、任务和完整输出 | [Protocols, tasks, and output](docs/protocols.md) |
| 使用项目指令或 Skills | [Startup context and Skills](docs/context.md) |
| 恢复会话，或了解持久化与上下文压缩 | [Sessions and context](docs/sessions.md) |
| 构建或审计扩展 | [WASM plugins](docs/plugins.md) |

[`docs/` 索引](docs/README.md)还包含开发与发布文档。程序运行时，协议支持的 URI 和 body 格式以 `<protocol>://help` 为准；当前生效的界面说明以 `F1` 和 `:help` 为准。

## 开发

项目使用 stable Rust。修改仓库前请阅读 [`AGENTS.md`](AGENTS.md)；模块职责、修改规则和必需的验证见[开发文档](docs/development.md)。

## 许可证

[MIT](LICENSE) © 2026 4fuu

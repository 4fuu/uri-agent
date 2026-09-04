<p align="center">
  <img src="docs/assets/logo.svg" width="420" alt="URI Agent 标志">
</p>

# URI Agent

[English](README.md) | [简体中文](README.zh-CN.md)

[![License: MIT](https://img.shields.io/badge/license-MIT-6ed2c2.svg)](LICENSE)

URI Agent 是一个可扩展的终端编程 Agent，让模型上下文集中在当前任务上。工具通过 URI 协议按需加载完整契约、指令和资源，使用方式与其他 Agent 中的 Skills 类似。模型起初只看到每个工具的精简名称和描述。

四个内置工具为模型提供精简的读取、执行和编辑接口：

```text
read(uri: string, body: string)
exec(uri: string, body: string)
replace(path: string, old_text: string, new_text: string)
apply_patch(patch: string)
```

`read` 和 `exec` 始终使用字符串请求体，并通过 URI 协议路由；没有内容时传 `""`。结构复杂或需要大量转义的参数由有类型工具处理。可信 WASM 插件可以在运行时添加协议和有类型工具。

> [!WARNING]
> URI Agent 不提供沙箱。文件与 Shell 协议以及已启用的 WASM 插件都拥有 `uri-agent` 进程的用户权限。请只使用可信的项目、配置和插件。

URI Agent 仍处于早期发布阶段，不同日期版本之间可能发生变化。模型请求及其上下文会发送给你选择的模型服务商；除非启用离线模式，URI Agent 还会从 pi.dev 和受支持的服务商获取模型目录元数据。

## 为什么使用 URI Agent

- **渐进式上下文：**仅在需要时加载协议契约、Skill 资源、内置文档和超长输出。
- **可扩展工具：**通过内置 Rust 插件和可信 WASM 插件添加协议或有类型工具，无需修改运行时分发逻辑。
- **内置 MCP 桥接：**使用 `:mcp` 连接 stdio 和 Streamable HTTP 服务器。每个服务器都会成为按需加载的 `<name>-mcp://` 协议；简单参数优先使用查询字符串，复杂参数可使用完整 JSON。
- **ACP 编辑器集成：**通过稳定的 ACP v1 stdio 接口，在兼容的编辑器中使用 URI Agent。每个会话都能选择模型，对话之后还可以在普通 TUI 中重新打开。
- **广泛模型支持：**直接在模型选择器中使用 pi.dev 目录、模型服务商专属登录和账户模型实时发现。
- **持久化工作：**让长命令作为受管任务继续运行，并在重启后恢复工作。仅追加的 SQLite 会话会保留草稿、固化的启动上下文、有标题的工作笔记，以及上下文滚动或摘要检查点。
- **会话协作：**多个 URI Agent 进程之间可以相互通信。
- **本地语义检索：**利用随安装包提供的 zvec 和 Model2Vec 资产，按需搜索项目文件和已保存会话。
- **单一终端工作流：**在同一个对话界面中使用 Queue 与 Steer、网络访问、键盘和鼠标控制、图片输入，以及 `@` 文件和 `@@` 会话引用。

## 模型和服务商覆盖

URI Agent 的目标是广泛兼容 pi.dev 模型目录，而不是只适配少数固定模型。截至 2026-09-04，目录覆盖如下：

| 目录指标 | 已支持 |
| --- | ---: |
| API 系列 | 9 个中的 5 个 |
| 模型条目 | 1,337 个中的 1,132 个（84.7%） |
| 服务商 ID | 39 个中的 35 个 |
| 支持实时发现的服务商 ID | 35 个可用服务商中的 28 个 |

已支持 OpenAI Responses、OpenAI Codex Responses、OpenAI Chat Completions、Anthropic Messages 和 Google Generative AI 五类 API。服务商的实时结果按凭据隔离缓存，并补充共享目录，因此账户中新开放的模型可以在 pi.dev 收录前出现。

通用目录兼容无法覆盖的能力由专用集成提供：

- ChatGPT Codex 订阅 OAuth 和 WebSocket 传输；
- Cloudflare AI Gateway 的凭据安全端点处理；
- WorkBuddy 中国站浏览器登录和账户模型发现；
- 明确标记为实验性的 Antigravity 私有协议；
- Abliteration.ai 按凭据隔离的实时发现和静态后备模型。

URI Agent 还支持 Anthropic、GitHub Copilot、Kimi Coding、xAI、Radius 和 OpenRouter 的服务商专属登录流程。

目录内容和账户权限会变化；目录中的模型仍需匹配的凭据、地区和订阅。当前服务商、实时发现、认证和兼容性细节见英文文档 [Models and configuration](docs/configuration.md#model-catalog)。

## 快速开始

### 环境要求

- 受支持模型服务商的凭据；
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

在 x86-64 或 ARM64 Linux 上，安装脚本会校验发布包的校验和，并将完整的版本化安装包写入 `~/.local/bin`：

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://raw.githubusercontent.com/4fuu/uri-agent/main/scripts/install.sh | sh
```

以上每种安装方式都包含匹配的 zvec 动态库、Jieba 词典和嵌入模型。URI Agent 运行时不会下载这些语义检索资产。源码构建需要按[开发文档](docs/development.md#native-retrieval-assets)准备同一套资产。

### 启动第一个会话

启动 URI Agent，并指定它应使用的项目目录：

```bash
uri-agent --cwd /path/to/project
```

`--cwd` 选择项目和默认工作目录，但不会限制文件系统访问。如果项目包含 `AGENTS.md`，请在启动前检查；新会话会将其中的指令纳入固化的启动上下文。

URI Agent 不会自动选择默认模型。在 TUI 中：

1. 运行 `:login` 保存 API 密钥或完成受支持的 OAuth 登录。
2. 运行 `:model` 并选择一个可运行模型。
3. 按空格键，输入请求，再按 `Enter` 发送。

当界面出现协议活动，并且助手根据项目内容返回答案时，第一个会话就已正常工作。按 `F1` 或运行 `:help` 可查看当前生效的命令和快捷键。

模型服务商支持、认证、离线模式、环境变量和自定义端点见英文文档 [Models and configuration](docs/configuration.md)。

### 连接 ACP 客户端

先在 TUI 中完成模型服务商认证，再将 ACP 客户端配置为启动：

```text
uri-agent --acpv1
```

客户端需要为每个会话提供绝对项目目录，同一个 ACP 进程可以为多个项目承载相互独立的会话。兼容的 ACP 客户端可在发送第一条请求前选择已认证的模型和思考级别，且不会修改 URI Agent 的默认设置。第一条请求会使用已分配的会话 ID 完成持久化；客户端释放会话后，可在 TUI 中重新打开。支持的内容、MCP 服务器、生命周期操作和所有权约束见英文文档 [ACP v1](docs/acp.md)。

## 文档

| 目标 | 文档 |
| --- | --- |
| 连接编辑器或其他 ACP 客户端 | [ACP v1](docs/acp.md) |
| 选择模型服务商、认证或修改设置 | [Models and configuration](docs/configuration.md) |
| 使用对话、命令、快捷键、终端或附件 | [Terminal interface](docs/interface.md) 和 [terminal features](docs/terminal.md) |
| 了解工具、协议、任务和完整输出 | [Protocols, tasks, and output](docs/protocols.md) |
| 使用项目指令或 Skills | [Startup context and Skills](docs/context.md) |
| 恢复会话，或了解协作、笔记、上下文滚动与持久化 | [Sessions and context](docs/sessions.md) |
| 构建或审计扩展 | [WASM plugins](docs/plugins.md) |

[`docs/` 索引](docs/README.md)还包含开发与发布文档。程序运行时，协议支持的 URI 和请求体格式以 `<protocol>://help` 为准；当前生效的界面说明以 `F1` 和 `:help` 为准。

## 开发

项目使用稳定版 Rust。修改仓库前请阅读 [`AGENTS.md`](AGENTS.md)；模块职责、修改规则和必需的验证见[开发文档](docs/development.md)。

## 许可证

[MIT](LICENSE) © 2026 4fuu

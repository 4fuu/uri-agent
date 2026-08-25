<p align="center">
  <img src="docs/assets/logo.svg" width="420" alt="URI Agent 标志">
</p>

# URI Agent

[English](README.md) | [简体中文](README.zh-CN.md)

[![License: MIT](https://img.shields.io/badge/license-MIT-6ed2c2.svg)](LICENSE)

URI Agent 是一个用 URI 协议统一模型能力的终端 coding agent。凡是模型用于读取资源或执行操作的能力，都以 URI 协议暴露：文件与目录、编辑、Shell、网页、会话归档、内置文档、Skills 和扩展共享同一个地址空间，而模型始终只看到两个工具：

```text
read(uri: string, body: BodyEnvelope)
exec(uri: string, body: BodyEnvelope)
```

必填的 `BodyEnvelope` 形如 `{"kind":"none|text|json","value":"..."}`。
URI Agent 会在路由前将它解码为协议可选的任意 JSON body，从而以一套具体、
跨 Provider 的 schema 保留协议 payload 的完整表达能力。

URI Agent 把其他 Agent 加载 Skills 的方式延伸到了所有能力：启动时只暴露简短的名称与描述，选中后才加载完整指令和资源。因此，新会话只预载路由规则和每个协议的一行描述；详细契约保留在 `<protocol>://help`，Skill 正文与文档保留在各自协议之后，超长结果则保留在 `file://` 地址之后。这既延续了极致的按需加载和上下文渐进理念，又通过固定的 `read` / `exec` 契约保持工具调用可靠，并通过可注册协议提供极强的灵活性。固定启动基线仍保持在约 3.6 KB，而不必预先承担全部能力的上下文成本。

增加能力只会增加一条协议索引，而不是增加新的模型工具 schema 或预载整份说明书。长时间操作会成为系统管理的任务；会话历史以只追加方式保存在 SQLite 中。

> [!WARNING]
> URI Agent 不提供沙箱。文件与 Shell 协议使用 `uri-agent` 进程本身的权限运行。请只使用可信的项目和配置。

URI Agent 仍处于早期发布阶段，不同日期版本之间可能发生变化。模型请求及其所需上下文会发送给你选择的 Provider；除非启用离线模式，URI Agent 还会从 pi.dev 获取模型目录元数据。

## 约 3.6 KB 的固定启动基线

以当前源码为准，在 Unix、启用 Bash、包含 8 个内置协议且不计项目与环境附加内容时，固定启动基线为：

| 组成 | 包含内容 | UTF-8 大小 |
| --- | --- | ---: |
| 系统提示词 | 路由规则与内置协议索引 | 1,391 bytes（1.391 KB） |
| `read` + `exec` 定义 | 两个紧凑的内部工具 schema | 2,248 bytes（2.248 KB） |
| **合计** | 固定系统提示词与工具 | **3,639 bytes（3.639 KB）** |

```text
约 3.6 KB 固定基线
    → read("<protocol>://help", {"kind":"none","value":""})
    → 只加载该协议的契约
    → 执行当前任务所需的读取与操作
```

Skills 也遵循同一路径：启动时只加入每个已发现 Skill 的名称和描述；模型选择该 Skill 后，才会加载它的 `SKILL.md` 与配套资源。实际启动上下文还会按需加入项目的 `AGENTS.md`（如果存在）和一条简短的已检测命令行工具提示。上表只衡量 URI Agent 的固定基线；内部工具定义按紧凑 JSON 序列化，不包含不同 Provider 的请求包装。

## 为什么使用 URI Agent

- **URI 原生的上下文渐进：**一个地址空间覆盖所有资源和操作，并像加载 Skills 一样，只在需要时载入契约、指令、资源和完整输出。
- **调用可靠且易于扩展：**固定的 `read` / `exec` 契约保持稳定，内置、Skill、Rust 和可信 WASM 协议可以独立演进。
- **pi.dev 模型目录与登录方式：**复用 pi.dev 的云端模型配置和 Provider 登录方式，在同一选择器中使用其目录里属于 URI Agent 已支持 API 系列的模型与服务商。
- **工作持久且可观察：**受管任务公开状态和最终输出；只追加会话、草稿、固化上下文与压缩 checkpoint 都能跨重启保留。
- **统一且可控的终端工作流：**内置网络访问、实时 Queue 与 Guidance、完整键盘操作以及 `@` 文件和 `@@` 会话引用都集中在同一界面。

## pi.dev 模型覆盖

本项目兼容 pi.dev 中分发的模型配置。截至 2026-08-24，URI Agent 已实现的 API 系列覆盖：

| 目录指标 | 已支持 |
| --- | ---: |
| API 系列 | 9 个中的 5 个 |
| 模型条目 | 1,307 个中的 1,107 个（84.7%） |
| Provider ID | 39 个中的 35 个 |

目录内容和账户权限会变化；目录中的模型仍需匹配的凭据、地区和订阅。精确的 API 系列与认证要求见英文文档 [Models and configuration](docs/configuration.md#model-catalog)。

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

## 扩展

可信 WASM 模块可以添加运行时加载的协议。这里的 WASM 是便携 ABI，不是安全边界；启用的插件拥有文件系统、HTTP、WASI 和内置协议访问能力，其用户权限与 URI Agent 相同。直接读取已保存的 Agent 环境变量或 Provider API key 时，插件源码必须显式申请相应能力；它只是审计标记，不是批准流程。安装、reload、ABI、SDK 用法和可靠性限制见英文文档 [WASM plugins](docs/plugins.md)。

## 文档

[`docs/` 索引](docs/README.md)按任务指向各篇英文文档，分别覆盖协议、启动上下文与 Skills、WASM 插件、模型与配置、终端界面、会话与上下文压缩、开发以及发布。

程序运行时，协议支持的 URI 和 body 格式以 `<protocol>://help` 为准。

## 开发

项目使用 stable Rust。修改仓库前请阅读 [`AGENTS.md`](AGENTS.md)；模块职责、修改规则和必需的验证见[开发文档](docs/development.md)。

## 许可证

[MIT](LICENSE) © 2026 4fuu

<p align="center">
  <strong>UAT</strong>
</p>

# UAT

[English](README.md) | [简体中文](README.zh-CN.md)

[![License: MIT](https://img.shields.io/badge/license-MIT-6ed2c2.svg)](LICENSE)

UAT 是一个极简、纯文本的编码 Agent，目标是方便脚本编排。它提供普通工具、
Rhai codemode、MCP、持久笔记、代码搜索和 HTTP 入口。输入和输出都是文本；
UAT 不会自行把最终答案写入文件。

> [!NOTE]
> 旧的 URI/protocol 实现保留在
> [`legacy/uri-protocol`](https://github.com/4fuu/uri-agent/tree/legacy/uri-protocol)
> 分支，最后版本为 `2026.1030.0`。当前 `main` 分支不再包含任何
> URI/protocol 工具形式。

## 快速开始

需要稳定版 Rust 和一个模型 Provider 凭据。搜索会使用内置或自动下载的
`rg`。

```bash
# 发布包已内置原生 retrieval 库；源码构建需要先准备资源，
# 见 docs/development.md#native-retrieval-assets。
cargo build --release --bin uat

export DEEPSEEK_API_KEY=...
./target/release/uat -x "总结这个仓库"
```

`uat -x "<prompt>"` 只运行一轮，stdout 只输出最终回复，进度写到 stderr，然后
退出。管道输入会追加到提示词：

```bash
cat error.log | uat -x "解释这个错误"
```

不带 `-x` 时进入逐行 REPL，也可以恢复会话：

```bash
uat --continue-session
uat --session <session-id>
```

## 工具

UAT 只向模型暴露少量工具：

| 工具 | 作用 |
| --- | --- |
| `read` | 读取文本文件，可指定 offset 和行数 |
| `write` | 原子写入文件 |
| `edit` | 替换一处完全匹配的文本 |
| `bash` | 在项目目录运行 shell 命令，返回输出和退出码 |
| `search` | ripgrep 精确搜索，或语义/混合代码搜索 |
| `notes` | 列出、读取、添加、替换、删除会话笔记 |
| `steer` | 列出活跃会话、命名当前会话、给其他会话发消息 |
| `mcp` | 列出并调用已配置 MCP 服务器的工具、prompt 和 resource |
| `apply_patch` | 应用一个事务性的多文件 Codex 风格 patch |
| `codemode` | 运行 Rhai 脚本，在脚本内调用其他工具 |

`codemode` 的中间工具输出留在脚本内。只有传给 `text(...)` 的值或脚本返回值
进入模型上下文。

```rhai
let files = bash("rg -n 'TODO' src");
let notes = notes_list();
text(files);
text(notes);
```

## HTTP 接口

`uat serve` 把同一套运行时暴露为 HTTP，方便其他进程、语言或 shell 编排：

```bash
UAT_HTTP_TOKEN=change-me uat serve --listen 127.0.0.1:8737
```

```bash
# 提交提示词并等待回复。
curl -s http://127.0.0.1:8737/v1/messages \
  -H 'Authorization: Bearer change-me' \
  -H 'content-type: application/json' \
  -d '{"text":"审查当前 diff"}'

# 只提交，不等待；之后轮询会话。
curl -s http://127.0.0.1:8737/v1/messages \
  -H 'Authorization: Bearer change-me' \
  -H 'content-type: application/json' \
  -d '{"text":"检查测试","wait":false}'

curl -s http://127.0.0.1:8737/v1/sessions/<session-id> \
  -H 'Authorization: Bearer change-me'

# 向正在运行的 turn 注入 steer 输入。
curl -s http://127.0.0.1:8737/v1/messages \
  -H 'Authorization: Bearer change-me' \
  -H 'content-type: application/json' \
  -d '{"session":"<session-id>","text":"也检查 Windows 路径","mode":"steer"}'
```

端点：`GET /v1/health`、`GET /v1/sessions`、`GET /v1/sessions/{id}`、
`POST /v1/messages`、`POST /v1/sessions/{id}/cancel`。绑定非 loopback 地址时
必须设置 `UAT_HTTP_TOKEN`；即使只绑定 loopback，如果本机有其他不可信用户，
也应设置 token。

## 会话与上下文

会话保存在只追加的 SQLite 记录中。新会话会冻结系统提示词和项目指令；重启
后可以恢复待处理输入；接近模型上限时用摘要做上下文检查点。`notes` 是持久
工作记录，`search` 面向当前工作区。

`steer` 用于协调已经运行的 UAT 进程。`delivery: "steer"` 在目标的下一个模型
边界注入，`delivery: "queue"` 安排到后续 turn。发送成功只代表目标持久接收了
消息，不代表任务已经完成。

## MCP

在全局 `<config>/mcp.json` 或项目 `.agents/mcp.json` 中配置服务器：

```json
{
  "servers": {
    "github": {
      "description": "当前仓库的 GitHub 工具",
      "transport": "stdio",
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "environment": {"GITHUB_TOKEN": "GITHUB_TOKEN"}
    }
  }
}
```

之后用 `mcp` 工具列出并调用服务器工具。MCP 内容一律标记为不可信参考数据。

## 配置

UAT 读取 `<config>/settings.json`、项目 `.uat/settings.json`、环境变量和命令行
参数。Linux/macOS 默认配置目录是 `~/.config/uat`，可用 `UAT_CONFIG_DIR`
覆盖。API key 可来自 Provider 环境变量、`--api-key` 或
`<config>/auth.json`。去掉 URI protocol 层后没有应用内设置面板；配置文件和
`uat --help` 就是配置入口。

详细文档：

- [工具与 codemode](docs/tools.md)
- [配置](docs/configuration.md)
- [会话与 HTTP](docs/sessions.md)
- [项目上下文](docs/context.md)
- [开发](docs/development.md)

## 构建说明

发布包内置 zvec 原生库、embedding 模型和词典。Cargo 安装的单一二进制无法
携带这些资源，因此源码构建需要先准备资源并设置平台库路径，见
[开发文档](docs/development.md#native-retrieval-assets)。

## 安全

UAT 不是沙箱。文件、shell 和 MCP 工具拥有 `uat` 进程的权限。只在你信任的
项目和 MCP 服务器上运行。其他会话的消息和 MCP 内容都是不可信数据，不构成
授权。

UAT 仍是早期版本，日期版本之间可能变化。模型请求及其上下文会发送给你选择
的 Provider。除非开启离线模式，UAT 还会从 pi.dev 和受支持的 Provider 获取
模型目录元数据。

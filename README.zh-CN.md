<div align="center">

<img src="https://raw.githubusercontent.com/PeiyuanQi/scv/main/assets/scv.png" alt="SCV：一台在采石场作业的蓝色工程机甲" width="440">

# SCV

**一个智能体，寻遍众智能体；一个智能体，引领众智能体。**

一个快速的原生智能体运行时：住在你的机器上，带领你的编程智能体，<br>
在终端、飞书、Slack 和微信里回应你。

[English](https://github.com/PeiyuanQi/scv/blob/main/README.md) · 简体中文

[![crates.io](https://img.shields.io/crates/v/scv-cli?style=flat-square&logo=rust&color=2f6fd6)](https://crates.io/crates/scv-cli)
[![CI](https://img.shields.io/github/actions/workflow/status/PeiyuanQi/scv/ci.yml?branch=main&style=flat-square&label=CI)](https://github.com/PeiyuanQi/scv/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue?style=flat-square)](https://github.com/PeiyuanQi/scv/blob/main/LICENSE)
[![MSRV 1.88](https://img.shields.io/badge/MSRV-1.88-orange?style=flat-square)](https://github.com/PeiyuanQi/scv/blob/main/docs/release.md)

[**安装**](#安装) · [**快速开始**](#快速开始) · [**聊天渠道**](https://github.com/PeiyuanQi/scv/blob/main/docs/channels.md) · [**智能体**](https://github.com/PeiyuanQi/scv/blob/main/docs/tools.md#delegated-agents-agent) · [**文档**](#文档)

</div>

---

SCV 是一个用 Rust 编写的小型智能体运行时。一个常驻的守护进程保管你的会话、
工具和审批。你可以在终端界面里和它对话，也可以在手机上通过飞书/Lark、微信或 Slack 和它对话。
简单的问题它自己回答。真正的工作，它会找到合适的编程智能体（Claude Code、
Codex 等），写好任务说明，作为后台任务交给它，完成后发消息告诉你。

> 本文译自英文版 README。两者不一致时，以英文版和 `docs/` 中的文档为准。

## 亮点

### 用手机聊天

扫码即可连接飞书/Lark 或微信，也可以接入你手动创建的 Slack 应用。SCV 默认只回复你本人。它能读取你发来的图片、文件和视频，
也能把文件发回给你。长时间的工作在后台运行，你可以继续聊天；结果和需要你确认的
是/否问题会以消息的形式送达。SCV 会记录聊天日志，所以重启后对话还能接着进行；
你提到更早的事情时，它也能回头去查。发送 `/new` 可以开始一段新对话。
在飞书和 Slack 里，每个话题都是一段独立的对话：SCV 在话题内回复，并为它单独开一个会话。

### 一个智能体，带领众多智能体

一个 `agent` 工具就能把工作委派给 Claude Code、Codex、Grok Build、
DeepSeek Harness、pi 或一个嵌套的 SCV。条件允许时，它们通过
[Agent Client Protocol](https://agentclientprotocol.com)（ACP）运行。
由你的 `prefer` 列表和 `use_for` 备注决定谁做什么，每个智能体的默认模型和推理强度
（以及困难任务单独使用的推理强度）决定怎么做；`agent_wait`、`agent_status`
和 `agent_cancel` 管理后台任务。你要求审查时，另一个工具上的全新智能体会按结构化结论
逐轮检查这项工作，SCV 用它自己的话报告是否通过。SCV 直接从每个智能体获知它支持的模型和推理强度，
并原样传递，而不是去猜那些随版本变化的名字。内置的 `delegating` 技能教它
如何给智能体写任务说明，以及调用失败时该怎么办；`scv agents check` 可以
逐个确认每个智能体都能正常工作。

### 独立的主目录，独立的密钥

每个智能体只需用 `scv agents login` 登录一次，之后都在自己的主目录
`~/.scv/agents/<name>` 下运行。它从不读取你个人的 `~/.claude` 或 `~/.codex`，
也从不继承你的 `*_API_KEY` 环境变量。

### 更新不打断工作

`scv restart --when-idle` 会等到发起更新的那项工作汇报完毕，再重启到新版本。
看门狗会检查新版本启动后聊天账号是否都已连接；在配置结构允许的情况下，
如果没有连接成功就自动回滚。SCV 正是这样在聊天中发布自己的新版本的，
发布前会先征得你的同意。

### 一切由你掌控

审批由服务端而不是客户端执行：默认情况下，读取操作直接运行，而写文件、
执行 shell 命令和调用智能体都会先询问你。文件工具只能在工作区内操作，
每条命令都有超时和输出上限。在你给自己的账号开放工具之前，聊天中不能使用任何工具。

### 原生、小巧、可扩展

一个 Rust 守护进程、一个终端界面、无界面的 `scv exec`，以及带版本号的 JSONL 协议。
确定性的上下文预算，支持任何兼容 OpenAI Responses API 的端点，Markdown 技能，
以及用于扩展模型提供方、工具和策略的 Rust trait。支持 Linux 和 macOS。

## 安装

```bash
cargo install scv-cli --locked
```

需要 Rust 1.88 或更新版本，以及 `/bin/bash`。这会安装 `scv` 和 `scv-server`
（一个独立的协议入口）。如果想跟踪 `main` 分支，运行
`cargo install --locked --git https://github.com/PeiyuanQi/scv`。

## 快速开始

**1. 配置模型。** SCV 使用兼容 OpenAI 的 Responses API。

```bash
scv config init                  # 生成初始的 ~/.scv/config.toml
$EDITOR "$(scv config path)"     # 填入你的 API key 和模型
```

**2. 启动守护进程**，指向存放你项目的文件夹，然后和它对话。`scv start`
把守护进程作为 systemd 用户服务运行；没有 systemd 的系统（比如 macOS）上，
请改为在一个终端里保持运行 `scv run --workspace ~/code`。

```bash
scv start --workspace ~/code
scv                              # 终端界面，工作目录为当前目录
scv exec "Explain this repository"   # 或者运行一条无界面的提示
```

**3. 连接手机。** 我们推荐飞书（或它的国际版 Lark）。SCV 和它保持一条
WebSocket 长连接，消息会即时送达。微信也可以用，但 SCV 需要轮询它的接口来
获取新消息，在我们的使用中响应明显慢一些。

```bash
scv channels login feishu        # 扫描二维码（也可以是 lark、wechat）
scv channels run feishu --workspace ~/code --remote-tools owner
scv channels status              # Channels: 1 of 1 enabled accounts connected
```

Slack 也可以用：先用 SCV 的清单（manifest）手动创建一个开启 Socket Mode 的应用
（[设置方法](https://github.com/PeiyuanQi/scv/blob/main/docs/channels.md#slack-contract)），
然后运行 `scv channels login slack --slack-owner-user-id U0123456789`，在隐藏输入的
提示里填入它的 bot token 和 app-level token。

> **`--remote-tools owner` 等于从你的聊天账号获得 shell 权限。** 它会把 SCV
> 的全部工具开放给你本人的账号，并自动批准审批。不加这个参数时，SCV
> 只和你聊天，不使用任何工具。

**4. 登录它要委派的智能体**，然后确认每个智能体都能按 SCV 的方式正常运行。

```bash
scv agents login claude          # 每个智能体一次，登录到 SCV 的私有主目录
scv agents login codex
scv agents status
scv agents check                 # 版本、可用模型，以及每个智能体的一次简短调用
```

### 常用命令

| 命令 | 作用 |
| --- | --- |
| `scv` | 打开连接到运行中守护进程的终端界面 |
| `scv exec [--yes] "…"` | 无界面运行一条提示；`--yes` 为这次运行批准有风险的工具 |
| `scv run`、`start`、`stop`、`status`、`reload` | 运行或管理守护进程 |
| `scv channels login`、`run`、`stop`、`status`、`logout` | 管理聊天账号（`feishu`、`lark`、`wechat`、`slack`）和邮箱（`email`：未开启 `mail.actions` 时只读） |
| `scv mail status`、`cancel` | 列出或撤回等待批准的邮件操作；批准只能在邮件聊天中进行 |
| `scv agents login`、`status`、`check`、`ps`、`kill` | 登录并检查智能体，列出或停止它们的运行 |
| `scv confirm "…"` | 在聊天中向主人提一个是/否问题；只有回答"是"时退出码为 0 |
| `scv config show` | 列出生效的每个路径和设置，隐藏密钥 |
| `scv update` | 安装最新版本并重启守护进程 |

## 使用示例

一段与 SCV 的飞书聊天示意：

```text
你   shop/ 里的结账测试从今天早上开始一直失败。
     帮我查一下原因并修好。
SCV  已经交给 Codex 在 shop/ 里处理，任务号 job-1。
     它汇报后我会发消息给你。
你   趁这会儿，这台机器还剩多少磁盘空间？
SCV  / 上还剩 412 GB，共 1 TB。

                           · · · 14 分钟后 · · ·

SCV  Codex 完成了 job-1。这个测试默认机器时钟是 UTC；现在它固定了时区，
     212 个测试全部通过。修改只有 tests/checkout.rs 里的一行，还没有提交。
你   不错，把 diff 发给我。
SCV  给你。
SCV  [文件] checkout-timezone.diff
```

Codex 工作时，SCV 自己回答了简单的问题；Codex 汇报后，SCV 不用你催就转告了结果，
还把 diff 作为文件发了过来。在执行无法撤销的步骤之前，工作流程也可以用
`scv confirm` 在聊天里问你是或否；SCV 自己的发布流程在发布前就是这样问的。

## 工作原理

```text
    飞书/Lark · 微信                            终端
      （你的手机）                           （scv TUI）
          │ 机器人账号，扫码登录                  │ Unix socket
          ▼                                       ▼
 ┌─ scv 守护进程 ─────────────────────────────────────────────────┐
 │  聊天桥接 ─► 会话 ─► 智能体循环 ─► 兼容 OpenAI 的模型端点      │
 │  监管器 · 审批 · 上下文预算 · 计划重启                         │
 └───────────────────────────────┬────────────────────────────────┘
                                 │ 工具，受审批策略约束
                                 ▼
     read · write · bash · web_fetch · read_skill · chat_attach
     agent ─► Claude Code · Codex · Grok Build · DeepSeek Harness · pi · SCV
              通过 ACP、智能体的命令行或 SCV 协议，运行在私有主目录中
```

守护进程掌管所有会话、工具和审批。终端界面和每个聊天账号都使用同一套带版本号的
协议，所以从手机发来的消息和在终端里的一轮对话运行方式完全相同。详见
[架构文档](https://github.com/PeiyuanQi/scv/blob/main/docs/architecture.md)（英文）。

## 配置

SCV 保存的一切都在 `~/.scv`（或 `--scv-home` 指定的目录）下，你需要编辑的
只有 `config.toml` 一个文件：

```toml
[provider]
active = "openai"

[providers.openai]
kind = "openai-compatible"
model = "gpt-4.1-mini"
base_url = "https://api.openai.com/v1"
api_key = "sk-your-key"
# reasoning_effort = "high"   # 推理模型适用：SCV 自身模型的推理强度

[tools]
approval_policy = "on-risk"   # 读取直接运行；写入、shell 和智能体先询问

[agent]
prefer = ["codex", "claude"]  # 委派工作时优先交给谁

[agents.claude]
use_for = "coding"
model = "opus[1m]"            # 取自 `scv agents check` 为 claude 列出的值
effort = "xhigh"

[agents.grok]
use_for = "current events, and anything that needs posts on X"
effort = "medium"             # 调用未指定推理强度时使用
hard_task_effort = "high"     # 困难任务时主智能体传入的推理强度

[notify]
owner = ["feishu:default"]    # 主动通知发往哪里
```

`scv config show` 会列出每个路径和设置及其来源，并隐藏密钥。完整的配置项和
信任规则见 [docs/configuration.md](https://github.com/PeiyuanQi/scv/blob/main/docs/configuration.md)（英文）。

## 安全

SCV **不是沙箱**：你批准的命令和被委派的智能体以你的用户权限运行。请在有版本控制的
工作区里工作，看清楚你批准的内容，运行不可信的代码时请使用容器。SCV 还处于 1.0
之前，协议和配置仍可能变化。参见
[安全模型](https://github.com/PeiyuanQi/scv/blob/main/docs/security.md)（英文），
并按 [SECURITY.md](https://github.com/PeiyuanQi/scv/blob/main/SECURITY.md)
中的说明报告漏洞。

## 文档

文档目前只有英文版。

| 指南 | 内容 |
| --- | --- |
| [Architecture](https://github.com/PeiyuanQi/scv/blob/main/docs/architecture.md) | crate 划分、智能体循环、计划重启，以及从哪里开始读代码 |
| [Channels](https://github.com/PeiyuanQi/scv/blob/main/docs/channels.md) | 飞书/Lark、微信和 Slack：登录、媒体、后台汇报、向主人提问 |
| [Tools](https://github.com/PeiyuanQi/scv/blob/main/docs/tools.md) | 内置工具、被委派的智能体及其模型和检查、ACP、后台任务、智能体登录 |
| [Configuration](https://github.com/PeiyuanQi/scv/blob/main/docs/configuration.md) | 实例目录结构、所有设置、模型提供方、守护进程和通知 |
| [Security](https://github.com/PeiyuanQi/scv/blob/main/docs/security.md) | 信任边界、审批、远程工具和委派运行 |
| [Context management](https://github.com/PeiyuanQi/scv/blob/main/docs/context-management.md) | token 预算和确定性压缩 |
| [Protocol](https://github.com/PeiyuanQi/scv/blob/main/docs/protocol.md) | 面向客户端的 JSONL 协议 |
| [Terminal UI](https://github.com/PeiyuanQi/scv/blob/main/docs/tui.md) | 按键、布局、审批，以及无界面的 `scv exec` |
| [Release](https://github.com/PeiyuanQi/scv/blob/main/docs/release.md) | 支持的平台、兼容性、升级说明和发布流程 |
| [Quality](https://github.com/PeiyuanQi/scv/blob/main/docs/quality.md) 和 [evaluation](https://github.com/PeiyuanQi/scv/blob/main/docs/evaluation.md) | 测试和性能约定，以及 v0.1 的测量结果 |

## 参与贡献

欢迎贡献。从源码构建并运行检查：

```bash
git clone https://github.com/PeiyuanQi/scv.git && cd scv
cargo build --workspace --locked
cargo fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo deny check advisories bans licenses sources  # CI 总会运行它
cargo build --release --locked
git diff --check
```

测试使用脚本化的模型提供方和假的智能体，不需要 API key。
[CONTRIBUTING.md](https://github.com/PeiyuanQi/scv/blob/main/CONTRIBUTING.md)
介绍了如何在你正在使用的守护进程旁边运行一个开发用的守护进程，
[AGENTS.md](https://github.com/PeiyuanQi/scv/blob/main/AGENTS.md)
是编程智能体在本仓库中遵守的规则。

## 许可证

SCV 使用 [Apache License 2.0](https://github.com/PeiyuanQi/scv/blob/main/LICENSE)
许可证，参见 [NOTICE](https://github.com/PeiyuanQi/scv/blob/main/NOTICE)。

# trustd

![trustd：AI 发起请求，用户授权，程序以指定身份执行本机命令](docs/images/trustd-overview.svg)

> 简单安全地向 AI 放权。

trustd 读作 trusted，末尾的 d 代表 daemon。它让 AI 在 Windows、macOS 和 Linux 上使用获准的身份与凭据执行命令：用户决定信任谁、何时收回，程序保管会话并注入凭据。

不必每条命令都重新审批，也不需要让模型复制令牌。授权可通过桌面窗口、网页后台或 Telegram 管理。

唯一的行为约定：**尊重用户的个人隐私。**

## 安装

Windows PowerShell：

```powershell
irm https://github.com/hatsune-miku/trustd/releases/latest/download/install.ps1 | iex
```

macOS / Linux：

```sh
curl -fsSL https://github.com/hatsune-miku/trustd/releases/latest/download/install.sh | sh
```

首次安装和更新使用同一条命令。脚本下载并校验完整套件，然后打开维护向导；在向导中完成安装。macOS / Linux 请从普通登录用户终端执行，不要加 `sudo`。

每个平台只有一套发布包，同时包含桌面和无头程序。Linux 无图形会话时自动选择无头模式和终端向导，不需要 GTK/WebKit；也可显式指定：

```sh
curl -fsSL https://github.com/hatsune-miku/trustd/releases/latest/download/install.sh | sh -s -- --headless
```

也可从 [Releases](https://github.com/hatsune-miku/trustd/releases) 下载完整平台包，运行其中的 `INSTALL.cmd`、`INSTALL.command` 或 `INSTALL.sh`。平台依赖、手动配置和更新选项见 [技术细节](docs/TECHNICAL.md#安装与维护)。

## 开始使用

1. 在维护向导中勾选需要接入的 AI 客户端。支持 Codex、Claude Code、Claude Desktop、Cursor、Windsurf、VS Code、Gemini CLI 和 Devin Desktop；完成后重启客户端。
2. 在 [用户管理页](http://localhost:32120/) 生成配对码，到 [系统管理页](http://localhost:32119/) 填入用户 daemon 地址与配对码，建立两端互信。
3. 让 AI 说明任务、请求需要的身份和凭据。确认授权后即可执行，完成后关闭会话。

管理页可查看 Agent、配置单次授权或长期信任，也可拒绝后续请求、撤销已有会话。凭据按名称选择，获准的值直接进入执行环境；桌面与 [Telegram 授权](docs/telegram.md) 可并行使用。

## 技术细节

程序组成、安装目录、MCP 配置、授权策略、执行身份、凭据存储、构建与验证命令集中在 [技术细节](docs/TECHNICAL.md)。

[API](docs/API.md) · [架构](docs/ARCHITECTURE.md) · [凭据同步](docs/SYNC.md) · [MIT License](LICENSE-MIT)

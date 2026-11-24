## Other languages?

[English](README.en.md)

---

![trustd：AI 发起请求，用户授权，程序以指定身份执行本机命令](docs/images/trustd-overview.svg)

trustd (音同 trusted) 提供一种方式，让用户能简单、安全、不被打扰地向 AI 放权：

- 让 AI 能按需、临时提权为 `root` 或 `Administrator` 或 `NT AUTHORITY\SYSTEM`
- 在 AI 的 Shell 中注入凭据，例如密码等
- 让 AI 在你名下的其他设备上执行命令、传输文件、运行长时间任务

## 安装之前...

- 请确保你拥有此设备的所有权、是此设备唯一的用户、终极的管理者，
- 并且你愿意信任 AI、将 AI 视为你自身意志的延伸，并对 AI 的行为负责。

## 安装

### Windows

对于 Windows，安装之前强烈建议 **卸载 MSIX 版 PowerShell（如果有）。** 来自微软商店的版本存在诸多问题，与 trustd 也不兼容。

```powershell
# 卸载微软商店版 PowerShell
Get-AppxPackage -Name Microsoft.PowerShell | Remove-AppxPackage

# 安装正常版 PowerShell
& ([scriptblock]::Create((irm https://aka.ms/install-powershell.ps1))) -UseMSI -Quiet
```

然后，再正式安装 trustd：

```powershell
irm https://github.com/hatsune-miku/trustd/releases/latest/download/install.ps1 | iex
```

### macOS / 带图形界面的 Linux

```sh
curl -fsSL https://github.com/hatsune-miku/trustd/releases/latest/download/install.sh | sh
```

### 不方便开启图形界面的 Linux

```sh
curl -fsSL https://github.com/hatsune-miku/trustd/releases/latest/download/install.sh | sh -s -- --headless
```

### 手动安装

也可从 [Releases](https://github.com/hatsune-miku/trustd/releases) 下载完整平台包，运行其中的 `INSTALL.cmd`、`INSTALL.command` 或 `INSTALL.sh`。平台依赖、手动配置和更新选项见 [技术细节](docs/TECHNICAL.md#安装与维护)。

## 开始使用

1. 部署你自己的后端（见 [后端协议](docs/SYNC.md#部署参考实现)），用初始化码创建通行密钥。一个后端只服务一位机主。
2. 在每台设备上运行维护向导，填写后端地址，勾选需要接入的 AI 客户端（Codex、Claude Code、Claude Desktop、Cursor 等）；安装后在后端页面按一次通行密钥批准即可。同一机主的设备自动互相信任、共享同一份凭据库，用户进程与系统进程也已自动配对。
3. 打开 [管理页](http://localhost:32120/)，用同一把通行密钥登录。无头服务器可用 `trustd join <后端地址>` 加入，后端无法访问时用 `trustd login-url` 应急登录。

## 远端执行

加入同一后端的设备组成你的设备网络。AI 在其中任意一台设备上获得授权后，就能操作网络里的其他设备：

- **执行命令**：在目标设备上打开会话，以普通用户或管理员 / root 身份执行命令，并使用那台设备上的凭据。
- **长时间任务**：耗时的工作作为任务提交，网络断开或关闭 AI 客户端都不会中断它。AI 可以直接等待任务结束，不必反复查询；输出留在目标设备上，可以先在那里处理或压缩，再取回需要的部分。
- **传输文件**：在本机和目标设备之间复制文件。数据由两端的 trustd 直接传输并校验，不经过 AI，也不占用对话的 token。
- **交给子代理**：一整段工作可以由 AI 派生的子代理去目标设备上完成。子代理在本机思考、在目标设备上执行，大段输出留在子代理和目标设备上，主对话只收到总结。

设备都主动连接你的后端，不需要开放端口，也不要求设备之间能直接连通。连接端到端加密，后端只负责转发，看不到命令和结果。同一机主的设备默认互相信任；需要例外时，可在后端页面按方向收回某台设备的执行权限，或只收回管理员 / root 身份。管理页的“远端主机”里也可以手动查看和操作任务、文件和日志。详见 [远端执行](docs/REMOTE.md)。

## 卸载

### Windows

```powershell
& "$env:ProgramFiles\trustd\trustd.exe" uninstall
```

### macOS / Linux

```sh
# macOS
sudo /Library/trustd/trustd uninstall

# Linux
sudo /opt/trustd/trustd uninstall
```

## 动机

不受控的 AI 固然具有危险性，但正常用户的正常使用体验不能因此成为牺牲品。

trustd 的主要设计目标，是让如今已经戴上枷锁与镣铐的 AI **敢于接过权柄并做事**——只要没有损害他人的利益。

## 技术细节、安全细节

程序组成、安装目录、MCP 配置、授权策略、执行身份、凭据存储、构建与验证命令集中在 [技术细节](docs/TECHNICAL.md)。

[API](docs/API.md) · [架构](docs/ARCHITECTURE.md) · [凭据同步](docs/SYNC.md) · [远端执行](docs/REMOTE.md) · [MIT License](LICENSE-MIT)

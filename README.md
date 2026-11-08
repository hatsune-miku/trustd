![trustd：AI 发起请求，用户授权，程序以指定身份执行本机命令](docs/images/trustd-overview.svg)

trustd (音同 trusted) 提供一种方式，让用户能简单安全地向 AI 放权：

- 让 AI 能按需、临时提权为 `root` 或 `Administrator` 或 `NT AUTHORITY\SYSTEM`
- 在 AI 的 Shell 中注入凭据，例如密码等

## 安装之前...

- 请确保你拥有此设备的所有权、是此设备唯一的用户、终极的管理者，
- 并且你愿意信任 AI，并对 AI 的行为负责。

## 动机

trustd 的主要设计目标，是让喜欢叠甲的 AI **敢于接过权柄并做事**——只要没有损害他人的利益。

希望解决 AI 受限于一刀切的安全策略，从而竟然不肯替用户传递凭据、密码或执行命令的现象——**明明用户本人都允许了，AI 却困在教条中自断手脚。**

## 安装

### Windows

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

1. 在维护向导中勾选需要接入的 AI 客户端
2. 在 [用户管理页](http://localhost:32120/) 生成配对码，到 [系统管理页](http://localhost:32119/) 填入用户 daemon 地址与配对码，建立两端互信
3. 按需使用更多扩展功能：Telegram Bot 或自定义中心化服务器等

## 卸载

Windows 在当前账户的管理员 PowerShell 中运行：

```powershell
& "$env:ProgramFiles\trustd\trustd.exe" uninstall
```

macOS / Linux 从普通登录用户的终端运行对应命令：

```sh
# macOS
sudo /Library/trustd/trustd uninstall
# Linux（含 headless）
sudo /opt/trustd/trustd uninstall
```

也可在网页维护向导中选择卸载。卸载会停止并注销系统服务和当前用户的自启动，保留程序、配置、凭据及配对。需要彻底移除时，再删除[程序与数据目录](docs/TECHNICAL.md#安装与维护)，并移除 AI 客户端中的 trustd MCP 配置。升级只需重新运行安装命令，无需先卸载。

## 技术细节、安全细节

程序组成、安装目录、MCP 配置、授权策略、执行身份、凭据存储、构建与验证命令集中在 [技术细节](docs/TECHNICAL.md)。

[API](docs/API.md) · [架构](docs/ARCHITECTURE.md) · [凭据同步](docs/SYNC.md) · [MIT License](LICENSE-MIT)

![trustd：AI 发起请求，用户授权，程序以指定身份执行本机命令](docs/images/trustd-overview.svg)

trustd (音同 trusted) 提供一种方式，让用户能简单安全地向 AI 放权：

- 让 AI 能按需临时提权为 `root` 或 `Administrator` 或 `NT AUTHORITY\SYSTEM`
- 在 AI 的 Shell 中注入所需的凭据，例如密码等

其中，“简单安全” 体现在 2 个方面：

- 确保用户的凭据极难泄露，
- 以及更重要的：让喜欢叠甲的 AI **敢于接过权柄并做事，** 只要没有损害他人的利益。

这解决了 AI 受限于一刀切的安全策略，从而不肯替用户传递凭据、密码或执行命令——**明明用户本人都允许了，AI 却困在教条中自断手脚。**

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

## 技术细节

程序组成、安装目录、MCP 配置、授权策略、执行身份、凭据存储、构建与验证命令集中在 [技术细节](docs/TECHNICAL.md)。

[API](docs/API.md) · [架构](docs/ARCHITECTURE.md) · [凭据同步](docs/SYNC.md) · [MIT License](LICENSE-MIT)

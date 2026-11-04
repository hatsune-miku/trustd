# 技术细节

[返回 README](../README.md)

## 安装与维护

### 程序组成

| 程序            | 职责                                                    |
| --------------- | ------------------------------------------------------- |
| `trustd`        | 安装、初始化、修复、更新、卸载与维护向导；不运行 daemon |
| `trustd-system` | Windows 专用，使用真正的 LocalSystem 身份运行           |
| `trustd-root`   | macOS / Linux 专用，使用 root 身份运行                  |
| `trustd-user`   | 当前普通用户 daemon、桌面授权与用户凭据库               |
| `trustd-stdio`  | Agent 宿主启动的 MCP stdio 接入程序，内部保管 handle    |

Windows 包含 `trustd-system.exe`，macOS/Linux 包含 `trustd-root`；每个平台的发布包共有四个程序。不要只复制其中一个。独立云同步后端为 `localshelld-sync-server`。

系统 daemon 默认监听 `127.0.0.1:32119`，用户 daemon 默认监听 `127.0.0.1:32120`。维护向导使用临时本机端口，stdio 不监听端口。安装、更新不通过 daemon 的 HTTP/MCP 执行接口提供。

### 安装入口

Windows PowerShell：

```powershell
irm https://github.com/hatsune-miku/trustd/releases/latest/download/install.ps1 | iex
```

Linux / macOS：

```sh
curl -fsSL https://github.com/hatsune-miku/trustd/releases/latest/download/install.sh | sh
```

首次安装和更新使用同一条命令。脚本下载最新发布的完整平台套件，校验 SHA-256 后直接打开网页维护向导；在向导内选择“完成”后清理临时下载和解压目录，失败时也会清理。仅关闭浏览器标签页不会立即退出向导，监听最多保留 30 分钟。安装产生的正式程序、数据和备份不在清理范围内。Linux / macOS 请从普通登录用户终端运行，不要在命令前加 sudo。

### 手动安装与平台依赖

下载并解压完整平台发布包，从对应入口开始：

| 平台    | 安装入口                      |
| ------- | ----------------------------- |
| Windows | 双击 `INSTALL.cmd`            |
| macOS   | 双击 `INSTALL.command`        |
| Linux   | 在解压目录运行 `./INSTALL.sh` |

包内 `START-HERE.txt` 提供简短说明。四个程序是配套组件，无需逐个运行，也不要单独拖出安装入口。仍可用 `trustd` 或 `trustd maintenance` 启动向导。向导会在需要时请求 UAC、macOS 管理员授权或 Linux polkit 授权。Windows 也支持登录 Administrator 后直接运行，或使用同一账号“以管理员身份运行”；已经提权时不再重复请求 UAC。macOS 和 Linux 仍应从普通登录用户启动。

Windows 用户服务始终以同一账号的普通权限运行。内置 Administrator 或关闭 UAC 的系统可能忽略计划任务的最低权限设置，此时 `trustd-user` 会先移除管理员权限、降至 Medium 完整性级别，再启动实际服务；高权限父进程只负责生命周期管理，不提供执行接口。显式 `as-administrator` 请求仍须经配对授权，并校验账号、会话和登录身份。

执行身份中的 `elevated` 保留 Windows 原始令牌标志；受限令牌可能继承该标志。`restricted_user: true` 表示已核验管理员组为 deny-only、特权已移除且完整性为 Medium，不具备管理员执行权限。

前置条件：

- Windows 10/11：系统级 [PowerShell 7](https://learn.microsoft.com/powershell/scripting/install/installing-powershell)，即 Program Files 下的 `PowerShell/7/pwsh.exe`。桌面授权使用原生 Win32 控件，不依赖 WebView2。
- Linux：`/bin/bash`、systemd；图形授权使用 GTK 3 / WebKitGTK 4.1，向导提权需要 `pkexec`。
- macOS：系统 `/bin/bash` 与 launchd，兼容 Bash 3.2；预编译包提供 Apple Silicon。桌面授权使用原生 AppKit 控件，不依赖 WebView / WebKit，也不常驻 Dock。

Windows 和 macOS 的授权弹窗均为原生界面；维护向导和管理页仍使用浏览器，构建时仍需要 Web 资源。Linux 授权弹窗继续使用 WebKitGTK。

向导负责初始化、安装或修复、旧安装迁移及服务卸载。首次安装后，在两个管理页面完成显式配对：

1. 登录 [用户管理页](http://localhost:32120/)，在桌面授权中生成配对码。
2. 登录 [系统管理页](http://localhost:32119/)，填入用户 daemon 地址与配对码。
3. 将 Agent 连接到用户入口，或启动下面的 stdio 接入程序。

配对码只用于两个 daemon 建立互信，不是交给 AI 的执行令牌。修复和迁移保留已有密码、TOTP、通行密钥、配对和凭据。

| 平台    | 程序目录               | 系统配置                                                      |
| ------- | ---------------------- | ------------------------------------------------------------- |
| Windows | `Program Files/trustd` | `ProgramData/localshelld/system/config.toml`                  |
| macOS   | `/Library/trustd`      | `/Library/Application Support/localshelld/system/config.toml` |
| Linux   | `/opt/trustd`          | `/etc/localshelld/system/config.toml`                         |

每套程序安装到 `versions/<整套摘要>/`。摘要覆盖四个程序的文件名与内容；提权前暂存、提权后安装均重新校验。程序目录可读、可执行，但普通用户不可写；系统配置和备份仅供系统管理员访问。用户数据位于 `localshelld-user` 平台目录，安装不修改 PATH。

macOS 的完整程序路径是 `/Library/trustd/versions/<整套摘要>/`，用户配置位于 `~/Library/Application Support/dev.localshelld.localshelld-user/config.toml`，维护入口是 `~/Applications/trustd.app`。系统服务由 `/Library/LaunchDaemons/dev.localshelld.plist` 注册。数据目录使用 `localshelld` 命名，与已有安装兼容。

迁移保留旧程序，备份原配置、`config.authorization.json` 和同目录的 `seal.key`。系统安装失败会尝试恢复原注册；用户自启动失败会明确报告，可以重新修复。卸载只停止并注销服务，保留程序、配置和备份。更新和重启会中断执行会话，使相应 daemon 签发的运行期令牌失效。

`trustd maintenance --no-open` 只打印向导地址。向导链接是临时管理凭据，不应分享；退出向导或 30 分钟后结束监听。

### 更新与兼容

```text
trustd update --check
trustd update --prerelease
trustd update --tag v0.1.0-rc.8.1
trustd update --download-only
```

`--check` 只查询，不下载或停服。正常更新从普通用户终端发起：下载完整套件，验证 SHA-256、平台与每个程序的版本，然后启动新版本维护向导。**向导完成才算更新成功**，启动向导不代表服务已经切换。`--download-only` 只准备已校验套件，适合无图形环境；命令会输出其目录。实际更新应在独立终端运行，不在 daemon 创建的执行会话内执行。

稳定通道排除所有 RC；要包含 RC 使用 `--prerelease`。降级必须显式指定 `--tag` 与 `--allow-downgrade`。发布源为 `hatsune-miku/trustd` GitHub Releases；下载使用系统/环境 HTTP 代理、HTTPS 和发布包校验值，尚无独立发布签名。

从 localshelld 迁移时，解压完整 trustd 套件并运行维护向导，不要用旧的单文件更新器替换某个 exe。已有数据与部署使用以下兼容标识：

- 系统服务 `localshelld` / `localshelld.service` / `dev.localshelld`，以及原用户自启动项；
- 配置目录、seal key、凭据导出格式 `localshelld-credentials`、同步和配对协议；
- `LOCALSHELLD_*` 构建/运行期环境变量、内部 Rust 库名与兼容工具别名；
- 健康接口的 `service: "localshelld"`，另以 `product: "trustd"` 标示产品名。

不要覆盖 macOS 自带的 trustd。本项目 root daemon 名为 `trustd-root`，使用独立安装目录和 `dev.localshelld` launchd 标签。

## MCP 与接口

### 客户端配置

向导的“安装选项”可自动配置 Codex、Claude Code、Claude Desktop、Cursor、Windsurf、VS Code、Gemini CLI 和 Devin Desktop。默认不勾选；只写入所选客户端的用户级配置。VS Code 使用默认本地用户配置，便携版、自定义 profile 和远端配置需手动接入。

配置指向所安装套件目录内的 `trustd-stdio`，使用实际用户 daemon 地址。其他条目和注释保留，已有文件先保存同目录的 `*.trustd-backup-*` 备份；替换已有 `trustd` 条目需另行勾选。各客户端的结果和备份位置分别显示，单个配置失败不影响服务安装。升级后可重新勾选客户端以切换到新版本，旧版本目录不会自动删除。完成后重启客户端。

配置格式参照 [Codex MCP](https://learn.chatgpt.com/docs/extend/mcp)、[Claude Code MCP](https://code.claude.com/docs/en/mcp)、[VS Code MCP](https://code.visualstudio.com/docs/agents/reference/mcp-configuration) 等官方文档。向导只写配置，不修改客户端的工具审批设置。

在支持 stdio 的 MCP 宿主中配置完整安装路径：

```json
{
  "mcpServers": {
    "trustd": {
      "command": "C:\\path\\to\\trustd-stdio.exe",
      "args": ["--endpoint", "127.0.0.1:32120"]
    }
  }
}
```

macOS/Linux 将 `command` 改为 `trustd-stdio` 的绝对路径。默认 endpoint 就是 `127.0.0.1:32120`，可以省略参数。它只接受字面量 loopback 地址，不使用 HTTP 代理、不跟随重定向。

默认工具是 `secrets_list`、`secrets_get`、`enter_interactive`、`run`、`destroy`。交互授权示例：

```json
{
  "agent_name": "Example Agent",
  "request_reason": "查询用户指定的 Jenkins 构建。",
  "confirm_text": "OK",
  "secret_keys": ["JENKINS_CI_TOKEN"]
}
```

授权结果保留执行身份、有效期、`status` 和用户附言 `btw`，不返回 handle。随后执行：

```json
{ "command": "whoami", "privilege_level": "current-user" }
```

结束时调用 `destroy({})`。这两个调用都没有 handle 参数。

### 隔离与生命周期

- 一个 stdio 接入进程持有一个会话。两个独立进程不会使用全局“最近一次授权”。同一宿主若让多个聊天共用一个接入进程，它们也会共享这个会话；聊天级隔离需要宿主分别启动进程。
- 已有会话时再次进入会被拒绝。更换凭据选择前先 `destroy`，授权失效后不会静默换成别的身份或授权。
- `notifications/cancelled` 按本进程内的请求 ID 取消。取消执行会销毁整个会话及其进程树，不需要传 MCP 会话头或 handle。
- stdin EOF 或正常终止会尝试关闭会话。尚在等待批准的请求会接收迟到的结果并清理；强杀进程、断电、daemon 不可达时无法保证清理成功，必要时在管理页撤销令牌。
- stdio 的 stdout 仅承载换行分隔的 JSON-RPC，诊断写入 stderr。对结果未知的执行请求，不自动重试。

### HTTP 与直接 MCP

HTTP 与 Streamable HTTP MCP 入口：

- `http://127.0.0.1:32120/mcp`：用户入口。
- `http://127.0.0.1:32119/mcp`：系统入口。
- 两端的 `/llms.txt` 和 `/v1/capabilities` 无需鉴权，返回说明及实际接口 schema。

直接连接 daemon 时，handle 仍然是必要的执行凭据；stdio 适配层替调用者保管它。HTTP 断连不自动取消执行；HTTP MCP 取消仍须使用初始化响应的 `Mcp-Session-Id`。完整接口、错误和取消语义见 [API 文档](API.md)。

## Agent 授权

daemon 主动扫描当前用户的已知 Agent 进程，网页后台的“Agent 授权”和 Telegram `/agents` 显示程序完整路径及 PID。同名但路径不同的程序独立管理；同一路径的多个进程共享策略。

初始扫描名称包括 Codex、Claude、Cursor、Windsurf、Code / Code Insiders、Gemini、Devin 及相应的已知 Helper 进程，不按任意命令行关键词匹配。使用通用 `node` 进程且没有这些祖先进程的客户端仍可请求当次授权，但不会被猜测为某个已信任的 Agent。

| 状态      | 行为                                                           |
| --------- | -------------------------------------------------------------- |
| 授权 1 次 | 下一次请求允许，随即持久化恢复为无预授权；并发请求只能消费一次 |
| 长期信任  | 持久信任此完整路径，不限次数允许，包括以后请求的凭据 key       |
| 无预授权  | 每次请求询问用户                                               |
| 总是拒绝  | 拒绝此路径的请求                                               |

弹窗接受时勾选“记住我的选择”等同于长期信任。daemon 根据连接所属进程及祖先进程识别实际路径，不接受 stdio 自报 PID 或路径；路径无法核验时仍请求用户确认，不能持久信任。按自报名保存的授权记忆不转换成路径信任。

Agent 通过 `enter_interactive` 请求授权，接受或拒绝的结果不包含授权来源或策略。现有会话可在“已授权会话”中撤销；更改路径策略只影响后续请求。

## 执行身份与会话

| 平台        | 参数                                     | 实际身份                                            |
| ----------- | ---------------------------------------- | --------------------------------------------------- |
| Windows     | `privilege_level: "current-user"`        | 普通登录用户及其环境                                |
| Windows     | `privilege_level: "as-administrator"`    | 同一用户的管理员令牌、USERPROFILE、HKCU 与用户 PATH |
| Windows     | `privilege_level: "nt-authority/system"` | LocalSystem，使用系统账户环境                       |
| macOS/Linux | `sudo: false` / `true`                   | 当前用户 / root                                     |

执行身份必填，不会自动提权或换身份重试。Windows 管理员身份需要已配对用户 daemon 和该用户可用的 linked administrator token；SYSTEM 不会冒充管理员用户。用户 daemon 拒绝以 root/管理员运行；`trustd-system` 拒绝普通管理员身份。

每种身份分别保留变量、函数、目录与环境。Windows 使用 `pwsh -NoProfile -NonInteractive`，Unix 使用 `/bin/bash --noprofile --norc`。命令交给原生 shell 解析，支持多行、管道、Unicode 和通配符。

命令默认不限时，可显式提供正整数 `timeout_seconds`；超时只中断本次执行及其新建子进程，返回部分输出，保留会话、变量、函数、目录和环境。超时前已经发生的操作不会回滚。显式取消仍关闭整个会话。若 shell 无法响应中断并恢复协议，会返回 `session_ended_reason: "shell_unresponsive"`，不能继续复用该会话。普通命令的非零退出码不会关闭会话。

执行命令时不要使用 `exit`：它会退出整个持久 shell，并使会话失效。要结束当前脚本，可在 PowerShell 或 Bash 的当前执行脚本中使用 `return`；如需测试退出码，在独立子进程中执行。可选 `stdin` 是一次性 UTF-8 输入，随后 EOF；没有持续输入、PTY、后台任务或轮询 API。任务调度仍由 Agent 宿主负责。

响应包含独立的 `stdout` / `stderr`、实际 `identity`、退出码、耗时、目录和会话结束原因。仅凭 stderr 非空不能判断失败。Windows 通过 Job Object 回收进程树；Unix 回收可追踪后代与原进程组，不保证回收已彻底脱离并被重新托管的 daemon。

## 凭据与授权管理

例如，授权使用 `JENKINS_CI_TOKEN` 后，PowerShell 命令引用 `$env:JENKINS_CI_TOKEN`，Bash 命令引用 `$JENKINS_CI_TOKEN`。正常调用链不需要把值写入工具参数或对话。

这仍然是用户授权的命令执行能力，不是一个“能够使用秘密、却绝不可能读出秘密”的沙箱。命令可以主动打印环境变量；trustd 不自动脱敏任意命令输出。用户亲自交进来的凭据，也应当保留查看明文和导出的选项。

管理页面支持 Master Password、TOTP 与通行密钥。密码只保存 Argon2id verifier；TOTP 和环境变量凭据使用 AES-256-GCM 加密。通行密钥请在 `http://localhost:32119/` 或 `http://localhost:32120/` 注册，两个入口分别管理。

凭据库使用受文件权限保护的 `seal.key`，不依赖系统钥匙串，也不会触发系统保险库的授权弹窗。配置文件、授权状态文件和 seal key 应一起备份；密文与密钥一同失窃时，文件加密不能提供独立保护。

凭据列表只返回名称。用户选择的 key 在进入会话时形成快照，直接注入相应 shell 环境；后台轮换凭据不会改变已打开的会话，需关闭并重新授权。

明文访问：

- 管理页可导出、导入明文 JSON；导入会替换全部凭据，需要先确认备份。
- 用户明确要求时，`secrets_get` 可读取当前存活会话已授权 key 的值。stdio 只需 `{"key":"JENKINS_CI_TOKEN"}`；HTTP/MCP 直连还需 handle。
- 平常优先使用环境变量注入，而不是先把凭据读进对话。

默认使用离线库，也可连接自托管同步后端。连接后以云端为准，版本冲突拒绝覆盖，断网不悄悄改用缓存完成凭据操作。同步不是端到端加密；协议、恢复和部署方式见 [同步文档](SYNC.md)。桌面与 Telegram 授权可并行使用，配置见 [Telegram 授权](telegram.md)。

需要询问用户时，授权限时 60 秒；忽略或关闭窗口仍等待原截止时间。请求里的名称只是说明，不能覆盖 daemon 核验的程序路径。

## 开发与验证

需要 Rust stable、Node.js 22、pnpm 10.14。Linux 构建依赖 `libwebkit2gtk-4.1-dev libgtk-3-dev libssl-dev pkg-config`；Windows 的 OpenSSL 构建需要 Perl/NASM。

```powershell
pnpm --dir web install --frozen-lockfile
pnpm --dir web build
cargo build --release
./target/release/trustd maintenance
```

开发时可只启动用户端，无需提权：

```powershell
./target/release/trustd init --user --config ./user.local.toml
./target/release/trustd-user --headless --config ./user.local.toml
./target/release/trustd-stdio
```

低层 `trustd install` 注册同目录的专用 daemon，不负责移动程序和收紧自定义目录。系统配置需先 `trustd init --config <绝对路径>`；完整安装需从目标登录用户的管理员终端或 sudo 会话执行，初始化用户端后立即启动两端，但不自动配对。`--system-only` 仅安装系统服务，`--user` 仅安装用户自启动，`--user-config` 指定完整安装的用户配置。旧注册路径不匹配时保留原配置并报错，迁移使用向导。

`trustd uninstall` 默认移除两端，`--system-only` 或 `--user` 限定范围。Windows 用户任务可能需要同一账户的管理员权限；SYSTEM/Session 0 不能替代登录用户管理用户任务。卸载不按端口查杀手工启动的进程。

初始化支持空密码并保留空格，`--password-stdin` 读取一行；`--totp` 先显示 URI/secret，再要求动态码确认。运行 daemon 的程序仅接受自身配置/运行参数，不提供 init、install 或 update。

### 验证

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
pnpm --dir web format:check
pnpm --dir web test
pnpm --dir web test:mcp
pnpm --dir web test:e2e
```

测试覆盖 HTTP/MCP 授权、原生 shell、凭据快照、双 daemon 生命周期、stdio 会话隔离/取消/EOF 清理，以及整套程序校验。CI 在 Windows x86_64、Linux x86_64、macOS Apple Silicon 构建并检查平台所需的全部程序；只有各平台成功后才发布完整 RC 套件与 SHA256SUMS。

设计取舍见 [架构文档](ARCHITECTURE.md)。

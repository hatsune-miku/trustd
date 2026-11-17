# 远端执行与 Codex

远端功能由用户 daemon 提供。设备都主动连接后端，不要求两台设备之间能建立连接。现有本机执行接口继续使用。

## 加入与信任

远端执行使用同一个后端（见 [SYNC.md](SYNC.md)）。设备在管理页“凭据 → 后端”加入后端，主人在后端页面按一次通行密钥批准即可，不需要另外部署中转、登记设备或复制公钥。

- 同一主人的设备默认互相信任：可以互相执行命令、申请管理员 / root 身份并使用全部凭据。任何一台设备被攻破，都可能借此在其他设备上执行命令。
- 设备之间的公钥来自主人通行密钥签名的名册，每台设备自行验证；后端无法伪造设备或冒充主人。
- 在后端页面撤销设备后，其他设备会断开它的连接，并终止其会话、任务和 Codex 进程。
- 连接使用 Noise IK，双方固定对方的设备公钥；后端中转只看到设备、连接时间和流量大小，无法解密执行请求及结果。客户端使用环境变量及受支持的系统代理设置，除 loopback 测试外拒绝明文连接。

部署后端时，Nginx 需要为 `/v2/relay/` 转发 WebSocket 升级，`deploy/sync/nginx-https.conf` 已包含相应配置。

## 使用远端

MCP 授权后调用 `remote_hosts` 获取已配对设备，再调用 `remote`，传入 `target`、`operation` 和 `arguments`。`host_info` 返回目标平台、可用身份和各操作参数；所有路径、cwd、身份、凭据名称都属于目标主机。`session_open` 加载目标身份的 shell profile，空 cwd 使用该身份的用户目录。需要 sudo 时，执行端的用户与特权 daemon 必须先完成本机配对。

- `run`：持久 shell 执行，显式选择身份；不设置默认执行超时。
- `job_start`：提交独立任务，指定稳定的 `idempotency_key`；相同参数重试返回原任务，不重复执行。用 `job_get`、`job_list` 查询，`job_output` 分页读取 stdout/stderr，`job_cancel` 终止进程树。
- `file_stat`、`file_read`、`file_write`：按绝对路径读取和分块传输文件。上传使用固定 UUID `transfer_id`、字节 offset；重复块必须相同，最终写入返回在远端计算的 SHA-256，可提供期望值校验。覆盖已有文件需要显式 `replace`。同一页面上传失败后再次点击上传可从上次确认位置继续。
- `logs_page`：读取该设备在目标主机上的 command / audit 日志。网页也提供任务、文件、日志和对话页面。

任务属于执行端 daemon，关闭发起端、网络断开、关闭父会话或结束 Codex 对话不会停止已经提交的任务。中转恢复后查询原 job ID；中转不缓存待执行命令。执行端 daemon 重启后保留任务状态和输出，将未完成任务标记为 interrupted，不自动重放命令。`run`、`agent_send` 的响应丢失时，先查询状态，不应盲目重试。

文件处理、依赖安装和计算可全部留在目标主机。优先让远端拉取代码和数据，在远端生成产物，仅取回必要文件和摘要；发起端无需参与计算，也无需转发完整数据集。

## Remote Subagent

首版只实现 Codex。目标用户须已安装并登录 Codex CLI；trustd 复用该用户的 CLI 配置及登录，不复制本机登录凭据。通过 [Codex app-server](https://learn.chatgpt.com/docs/app-server) 的 stdio 协议启动独立进程，每个 trustd 对话对应一个 Codex thread。

`agent_start` 创建对话；`agent_list` 列出已有对话；`agent_send` 开始一轮，`agent_get` 查询状态及待回答问题，`agent_events` 按 offset 读取持久事件。`agent_interrupt` 中断当前轮次，`agent_close` 停止进程；`agent_resume` 使用新的授权会话恢复 trustd 创建的 thread。网页处理 Codex 的命令、文件与权限审批及提问；MCP 调用方不能代替网页回答审批。创建响应丢失时，先通过 `agent_list` 检查结果。

Codex 本身以目标普通用户身份启动，默认采用目标 CLI 的 model、approval policy、sandbox 配置。trustd 为它提供一个绑定当前授权会话的 MCP adapter，能够提交、查询、取消持久任务及传输文件。需要独立于对话生命周期的工作应通过 `job_start` 提交。Codex 内置工具的活动保存在对话事件中；通过 trustd 执行的命令进入 command / audit 日志。

启动或恢复对话时，注入的 `mcp_servers.trustd_remote` 设置 `default_tools_approval_mode = "approve"`，避免该 adapter 的请求等待不可见的 Codex MCP 工具审批。

每个对话的 trustd 工具使用独立 shell，初始目录与对话的 cwd 一致。对话恢复后可继续查询、取消自己先前提交的任务，不能访问其他对话的任务。

远端状态保存在用户配置旁的 `config.remote.json` 和 `config.remote/` 中，包括设备身份、授权配置、任务元数据、输出及 Codex 事件。配置与能力令牌仅允许对应用户访问；不要把这些运行数据放入代码仓库。

## 验证

`cargo test --workspace --all-targets --no-default-features` 覆盖中转身份校验、密文分片、对端断开、任务独立性、幂等提交、上传重试和授权范围。`pnpm --dir web test:mcp` 使用官方 MCP SDK 校验本机和 Codex adapter 的工具定义。

已配对的隔离测试实例可运行 `python3 scripts/test-remote.py --hosts /private/test-hosts.json --codex`。配置文件是主机数组，每项包含 `name`、本机管理接口 `endpoint`、目标 `device_id`、管理登录 `admin_session`、测试目录 `cwd`、`platform`（windows / linux / macos）；SSH 转发管理端口时可通过 `host_header` 指定原始 loopback 地址及端口。该脚本会执行命令、写入测试文件，并在指定 `--codex` 时使用各主机已有的 Codex 登录；测试数据应放在专用目录，配置文件不要提交到仓库。

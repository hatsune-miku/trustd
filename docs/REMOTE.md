# 远端执行与 Codex

远端功能由用户 daemon 提供。发起端、执行端都主动连接 HTTPS 中转，不要求两台设备之间能建立连接。现有本机执行接口继续使用。

## 配置中转

在一台可被两端访问的服务器上运行独立程序 `trustd-relay`。中转只监听 loopback，通过已有 HTTPS 反向代理提供 WebSocket 入口。默认端口是 `32129`。

在各设备管理页面的「远端」中复制设备 ID 或公钥，在中转服务器上登记：

```sh
trustd-relay add-device 'DEVICE_ID_OR_PUBLIC_KEY'
trustd-relay serve
```

位置参数接受 64 个十六进制字符的设备 ID，或管理页面显示的 Base64 公钥。公钥会自动换算成设备 ID，即公钥的 SHA-256 摘要；两种输入登记的是同一台设备。

`--config` 可省略，默认路径为：

| 平台 | 中转配置文件 |
| --- | --- |
| Linux | `/var/lib/trustd-relay/config.json` |
| macOS | `/Library/Application Support/trustd-relay/config.json` |
| Windows | `%ProgramData%\trustd-relay\config.json` |

`--token-file` 可省略，默认将令牌写入配置文件同目录下的 `<设备ID>.token`，命令完成后会显示文件路径。配置文件保存各设备的令牌摘要。使用 `--config /custom/relay.json` 时，默认令牌也写入 `/custom/`；仍可用 `--token-file` 单独指定输出路径。

以独立服务账号运行中转时，使用该账号登记设备，例如 Linux 上的 `sudo -u trustd-relay trustd-relay add-device 'DEVICE_ID_OR_PUBLIC_KEY'`。同一设备再次登记会生成新令牌并替换旧令牌，配置变更不需要重启中转。旧的 `--device DEVICE_ID` 写法仍兼容。

将令牌文件中的完整内容填入对应设备管理页的接入令牌栏，同时填写 HTTPS 中转地址，再添加另一台设备的公钥。双方都要添加对方公钥；在接受执行请求的一端明确授予执行权限、sudo 权限和可用凭据名称。中转接入登记不代替设备之间的公钥配对。凭据与 sudo 独立，凭据保留在执行端。

nginx 的示例配置，放入已经配置证书的 HTTPS server：

```nginx
location /trustd-remote/ {
    proxy_pass http://127.0.0.1:32129/;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade";
    proxy_read_timeout 90s;
    proxy_send_timeout 90s;
    proxy_buffering off;
}
```

设备填写 `https://your-host/trustd-remote`。客户端使用环境变量及受支持的系统代理设置。除 loopback 测试外拒绝明文中转。连接使用已有的 Noise IK 协议实现，双方固定对方的设备公钥；先完成双向握手，再发送请求。中转看到设备、连接时间和流量大小，无法解密执行请求及结果。接入令牌不等于执行授权。

不使用网页时，可在用户 daemon 停止期间配置，随后重新启动：

```sh
trustd remote identity --config /path/to/user/config.toml
trustd remote connect --config /path/to/user/config.toml \
  --relay https://your-host/trustd-remote --token-file /private/path/device.token
trustd remote pair --config /path/to/user/config.toml \
  --public-key PEER_PUBLIC_KEY --label workstation --allow-execution
```

按需添加 `--sudo`、重复的 `--secret-key NAME`。撤销设备或缩减权限会终止其现有会话、任务和 Codex 进程。`trustd-relay remove-device 'DEVICE_ID_OR_PUBLIC_KEY'` 则撤销中转接入，连接在下一次心跳检查后关闭。

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

每个对话的 trustd 工具使用独立 shell，初始目录与对话的 cwd 一致。对话恢复后可继续查询、取消自己先前提交的任务，不能访问其他对话的任务。

远端状态保存在用户配置旁的 `config.remote.json` 和 `config.remote/` 中，包括设备身份、授权配置、任务元数据、输出及 Codex 事件。配置与能力令牌仅允许对应用户访问；不要把这些运行数据放入代码仓库。

## 验证

`cargo test --workspace --all-targets --no-default-features` 覆盖中转身份校验、密文分片、对端断开、任务独立性、幂等提交、上传重试和授权范围。`pnpm --dir web test:mcp` 使用官方 MCP SDK 校验本机和 Codex adapter 的工具定义。

已配对的隔离测试实例可运行 `python3 scripts/test-remote.py --hosts /private/test-hosts.json --codex`。配置文件是主机数组，每项包含 `name`、本机管理接口 `endpoint`、目标 `device_id`、管理登录 `admin_session`、测试目录 `cwd`、`platform`（windows / linux / macos）；SSH 转发管理端口时可通过 `host_header` 指定原始 loopback 地址及端口。该脚本会执行命令、写入测试文件，并在指定 `--codex` 时使用各主机已有的 Codex 登录；测试数据应放在专用目录，配置文件不要提交到仓库。

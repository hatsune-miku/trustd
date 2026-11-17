# 后端协议 v1

每台设备都加入主人自托管的后端，不再有单机模式。后端负责主人的通行密钥、设备名册、凭据与 Telegram bot 配置同步，以及设备之间的加密中转。不同步执行令牌、桌面授权记忆或设备私钥。

参考实现位于 `crates/sync-server`，共享数据类型与校验位于 `crates/sync-protocol`。实现采用单实例、单所有者模型；不同客户端可以按本文独立实现。

本机凭据注入另有保留环境变量限制：PATH、HOME、系统目录、动态加载器、运行时启动钩子及代理等控制变量不可通过凭据创建、导入或注入。旧同步快照可以读取和移除这些键，但执行器仍会拒绝注入；服务端兼容读取不代表客户端允许执行。

## 使用

1. 在本机“凭据”页导出需要保留的数据。
2. 部署后端，打开其 HTTPS 地址。读取服务器数据目录的 `bootstrap-code`，创建第一个通行密钥。它是所有设备固定信任的主人密钥。
3. 在设备管理页“凭据 → 后端”填入后端地址和设备名称，选择“加入后端”。
4. 打开批准链接，核对设备上显示的验证码，按一次通行密钥批准。无需先登录后端。
5. 设备在本机验证这次批准，下载云端整库并替换本机库，并与同一主人的其他设备互相信任。新服务的云端库为空；需要迁移原数据时，再导入第 1 步的文件。

一个后端只服务一位主人。通行密钥属于后端的域名，不属于 localhost。私钥不上传至服务端。

## 一致性

服务端是唯一的数据权威。每份凭据库有递增的 `revision`，从 0 开始。

- 读：客户端向云端读取完整快照，校验后覆盖本机加密缓存，再返回结果。
- 写：先读取最新快照，在其上应用修改，携带 `expected_revision` 整库提交。服务端原子检查版本、持久化、增加版本，再返回确认快照。客户端保存确认快照，并再次拉取云端。
- 冲突：返回 409，不覆盖另一端的写入。客户端拉取最新快照并要求用户重试，不自动重放旧整库。
- 实时通知：在线客户端长轮询版本变化；提交成功立即唤醒等待者，客户端随后拉取整库。重连和每次心跳也重新读取云端。
- 断网：仍配置云端时，凭据列表、导出、修改及新会话的凭据解析失败，不使用旧缓存授予新凭据，也不积压离线写入。
- 离开后端：用户明确离开后，保留最后确认的本机快照，停止同步，也不再与其他设备互通。

保证的是服务端读写的顺序一致和客户端成功操作以云端为准。网络存在延迟，断线设备的磁盘缓存不可能与云端在同一瞬间变化；它不会被当作最新数据使用。已经授权的 shell 保留创建时的环境变量快照，不会因同步而改写正在运行的程序。

网络错误可能发生在云端提交成功之后。客户端会尝试重新读取，仍无法确认时报告错误；不能据此推断云端没有变化，也不能盲目重试。

## 通用约定

- Base URL 为 HTTPS origin，不含路径、用户名、密码、query 或 fragment。本地开发允许 `http://localhost:<port>`。
- 接口前缀为 `/sync/v1`，请求与响应采用 UTF-8 JSON。凭据及 token 不放入 URL。
- 访问认证使用 `Authorization: Bearer <access_token>`。不使用 cookie，不开放跨域读取。带 `Origin` 的请求必须与配置的 origin 完全匹配。
- 时间为 Unix epoch 秒。客户端应以服务端返回的有效期为准。
- HTTP 400/422 表示请求不合法，401 表示认证无效，403 表示权限不足，409 表示版本冲突，429 表示需要退避。业务错误格式为 `{"error":"说明"}`；框架的 JSON 格式错误可能返回普通文本。
- 请求体上限 8 MiB；客户端也必须限制响应体大小。参考实现不记录请求体或 token，响应设置 `Cache-Control: no-store`。

`GET /health` 返回 `{"service":"localshelld-sync","protocol":1}`。

`GET /sync/v1/info` 无需认证：

```json
{ "protocol": 1, "initialized": false, "origin": "https://sync.example.com" }
```

## 初始化和通行密钥

首次启动生成 256 bit 随机初始化码，保存在私有数据目录的 `bootstrap-code` 文件；状态库保存其哈希。初始化码不通过 HTTP 返回，不写入日志。第一个通行密钥注册成功时，原子消耗初始化码并移除文件；并发初始化只能有一个成功。

### 注册

`POST /passkeys/register/start`：

```json
{ "name": "我的通行密钥", "bootstrap_code": "<INITIALIZATION_CODE>" }
```

初始化后，不再接受初始化码授权。添加通行密钥需要有效的浏览器登录 JWT，`bootstrap_code` 可以省略。设备同步 JWT 不能添加或移除通行密钥。

返回 `{"challenge_id":"...","options":{"publicKey":{...}}}`。按照 WebAuthn JSON 格式把 challenge、user.id、credential id 从 base64url 解码后传给 `navigator.credentials.create()`。

`POST /passkeys/register/finish`：

```json
{
  "challenge_id": "...",
  "credential": {
    "id": "...",
    "rawId": "...",
    "type": "public-key",
    "response": { "attestationObject": "...", "clientDataJSON": "...", "transports": ["internal"] },
    "extensions": {}
  }
}
```

二进制字段使用不带 padding 的 base64url。追加注册的 finish 必须使用 start 所属的同一浏览器会话。挑战仅在服务端保存，5 分钟过期，一次使用，校验 RP ID、origin、challenge 和用户验证。最多 32 个通行密钥。

成功返回浏览器登录 JWT。

### 登录

1. `POST /passkeys/authenticate/start`，body `{}`，取得相同结构的 challenge/options。
2. 使用 `navigator.credentials.get()`。
3. `POST /passkeys/authenticate/finish`：

```json
{
  "challenge_id": "...",
  "credential": {
    "id": "...",
    "rawId": "...",
    "type": "public-key",
    "response": { "authenticatorData": "...", "clientDataJSON": "...", "signature": "...", "userHandle": null },
    "extensions": {}
  }
}
```

成功返回浏览器登录 JWT。认证结果更新已保存的通行密钥状态。

### JWT 和续期

响应格式：

```json
{
  "access_token": "<JWT>",
  "refresh_token": "<OPAQUE_REFRESH_TOKEN>",
  "expires_at": 1900000300,
  "refresh_expires_at": 1902592000
}
```

参考实现使用 HS256，固定验证签名、算法、`iss`、`aud`、`sub`、`exp` 和仍有效的服务端 `sid`。`iss` 为配置的 origin，`aud` 为 `localshelld-sync`。JWT 不包含凭据值。

- 浏览器登录有效期为 5 分钟，`refresh_token` 为空。浏览器仅将 JWT 保存在页面内存；刷新页面或到期后重新验证通行密钥。信任管理操作要求这种近期验证过的浏览器会话。
- 设备访问 JWT 有效期为 5 分钟；refresh token 最多有效 30 天，从设备授权时计，不无限滑动延期。
- `POST /session/refresh`，body `{"refresh_token":"..."}`，无需访问 JWT；返回一对新的 token，旧 refresh token 立即失效。
- 服务端保存 refresh token 哈希和已消费历史。重放已消费 token 会撤销整个设备会话，其访问 JWT 也随即失效。伪造一个不存在的 token 不会撤销已知会话。
- 同一客户端必须串行续期，并立即持久化新 token。续期响应丢失或本地持久化失败时，重新授权设备，不能循环使用旧 refresh token。参考客户端将 token 加密存储。

此实现不是 OAuth/OIDC 服务；设备授权流程借鉴其交互方式，但端点和字段以本文为准。

## 主人签名的声明

设备不把后端的结论当作授权依据。设备加入、撤销以及通行密钥的增删，都是主人通行密钥签名的“声明”：一段 JSON 文本，WebAuthn challenge 为 `SHA-256("trustd-owner-statement-v1\n" + 声明文本)`。每台设备用加入时固定的主人公钥自行验证每条声明，从而在本机推导设备名册。后端只保存和转发声明，无法伪造或改写它们，但可以扣留声明，例如不转发某条撤销。

声明类型（`kind`）：

- `device_approve`：`device_id`、`public_key`（设备的 Noise 静态公钥，Base64）、`name`、`issued_at`。`device_id` 必须等于公钥的 SHA-256 十六进制值。
- `device_revoke`：`device_id`、`issued_at`。撤销是永久的，无论声明顺序如何都优先于批准；同一设备身份不能再次加入。
- `passkey_add`：`key`（凭据 ID 与 COSE 公钥）、`issued_at`。必须由已信任的通行密钥签名，新密钥才会被设备信任。
- `passkey_remove`：`credential_id`、`issued_at`。最后一个受信任的密钥不会被移除。
- `rule_set`：`rules`（`[{from, to, deny}]`，`deny` 为 `execution` 或 `elevation`）、`issued_at`。完整替换设备之间的单向规则，`issued_at` 最新的一条生效。没有规则时，同一主人的设备互相完全信任。
- `management_login`：`device_id`、`nonce`、`issued_at`。一次性的管理页登录证明，只返回给发起的设备，不进入名册。

声明拒绝未知字段，签名要求 user presence 与 user verification，并校验 origin、RP ID 和 challenge。共享实现位于 `crates/sync-protocol/src/owner.rs`。

## 设备加入

1. 设备生成自己的加入声明（`device_approve`，含本机公钥），`POST /device/start`，body `{"statement":"..."}`，无需认证。设备时钟与服务端相差超过 10 分钟、声明无效或身份已被撤销时拒绝。返回 `device_code`、`user_code`、`verification_uri`（`https://sync.example.com/#join=<USER_CODE>`）、`expires_in` 和 `interval`。
2. `device_code` 仅保留在设备进程内。页面打开 verification URI，`POST /device/inspect`，body `{"user_code":"..."}`，返回待签名的原文 `statement`、`challenge`、`rp_id`、`allow_credentials` 以及设备名称和验证码。此接口无需登录；`user_code` 只能用于查看这一项请求。
3. 主人核对验证码后按一次通行密钥。页面 `POST /device/approve`，body `{"user_code":"...","assertion":<WebAuthn 断言>}`。服务端用当前受信任的主人密钥验证签名后记录这条声明。
4. 设备每 3 秒 `POST /device/poll`，body `{"device_code":"..."}`。未批准返回 202 和 `{"pending":true}`；批准后一次性返回 `tokens`、`owner`（RP ID、origin 与主人基础公钥）、`approval` 和全部 `statements`。
5. 设备核对 `owner` 的 origin 与 RP ID 是否就是自己选择的后端，确认 `approval` 正是自己的声明并通过签名验证、本机已出现在推导出的名册中，然后固定 `owner`，读取 `/vault` 并保存。此前不覆盖本机库。

首次加入时，设备从后端获得主人公钥，这一步属于首次信任；此后的名册更新都必须通过已固定公钥的验证。

## 名册与撤销

`GET /roster`（任意有效会话）返回 `owner` 和 `statements`，另附便于页面显示的 `devices`、`rules` 与 `trusted_keys`；设备只采信自己从声明推导的结果。在线设备的长轮询在新声明到达时被唤醒，随后重新拉取名册。声明只增不减：如果后端返回的列表缺少设备已经见过的声明（例如想隐藏一次撤销或更严格的规则），设备拒绝这次更新。

撤销设备或增删通行密钥时，已登录的页面调用 `POST /statements/prepare`（浏览器 JWT），body 为 `{"kind":"device_revoke","device_id":"..."}`、`{"kind":"rule_set","rules":[...]}`、`{"kind":"passkey_add","id":"..."}` 或 `{"kind":"passkey_remove","id":"..."}`，取得待签名原文与 challenge；签名后 `POST /statements`，body `{"statement":"...","assertion":...}`。签名本身即授权，此接口无需会话。撤销会立即删除该设备的会话，设备随之失去同步与中转。

`GET /sessions` 要求浏览器 JWT，返回 `passkeys:[{id,name,trusted}]`；`trusted` 表示设备是否信任该密钥。在已登录页面新增通行密钥后，页面会立即请求一个已受信任的密钥签署 `passkey_add`。

`POST /session/revoke`，body `{"id":""}` 撤销调用者自己的会话，用于浏览器退出登录或设备离开后端。

## 签名请求

设备可以请主人签署关于自己的一次性声明，目前用于管理页登录（`management_login`）。设备以自己的会话 `POST /signatures`，body `{"statement":"..."}`，只能请求关于本机 device ID 的声明，返回私有的 `id`、给页面用的 `code` 和 `verification_uri`（`https://sync.example.com/#sign=<CODE>`），5 分钟内有效。页面 `POST /signatures/inspect`（无需登录）显示设备名称与待签名原文，主人签名后 `POST /signatures/approve`。设备 `POST /signatures/poll`，body `{"id":"..."}` 取回断言，只有发起请求的设备能取回，且只能取回一次。设备自行验证签名与声明原文；后端只做转发。

## 设备中转

后端同时提供设备之间的加密中转：`GET /v2/relay/<device_id>`（WebSocket），`Authorization: Bearer <access_token>`。只有名册中的设备、使用绑定到自己的有效会话才能连接；心跳时重新检查名册与会话，被撤销的设备会被断开。设备之间使用 Noise IK 端到端加密，公钥来自已验证的名册，中转只能看到密文、设备 ID 和流量大小。

## 凭据整库

`GET /vault` 要求有效浏览器或设备 JWT，返回：

```json
{ "revision": 7, "credentials": { "JENKINS_CI_TOKEN": "...", "OPENAI_API_KEY": "..." } }
```

`PUT /vault`：

```json
{ "expected_revision": 7, "credentials": { "JENKINS_CI_TOKEN": "new value" } }
```

这是整库替换，不是 merge；示例会删除未包含的 `OPENAI_API_KEY`。成功返回新完整快照，例如 `revision:8`。空对象清空库。版本不匹配返回 409，任何写入都不发生。

所有实现必须遵守相同校验：最多 64 个 key；key 符合 `[A-Z_][A-Z0-9_]{0,127}`；value 为 1–16,384 UTF-8 字节，不含 NUL。禁止 `LOCALSHELLD_*`、`BASH_ENV`、`ENV`、`SHELLOPTS`、`BASHOPTS`、`CDPATH`。整份请求先校验，再原子提交，不部分导入。

## 实时通知

`GET /events?after=7` 要求访问 JWT。版本不同立即返回 `{"revision":8}`；相同则等待变化，最多 25 秒后返回当前版本。响应前再次检查会话有效性。通知不包含凭据值。

客户端收到响应后拉取 `/vault` 并再次订阅。连接中断时退避重连；客户端必须保证即使漏掉通知，下一次读取仍从云端取值。v1 使用 HTTP 长轮询，不要求 WebSocket。

## 离线凭据文件

文件是普通 JSON，不加密，不附签名。使用者自行保密。

```json
{
  "format": "localshelld-credentials",
  "version": 1,
  "credentials": { "EXAMPLE_TOKEN": "example value" }
}
```

不包含服务器地址、JWT、执行 token、WebAuthn 私钥或 revision。导入前完整校验格式、版本和所有 key/value；导入会替换全部凭据。连接云端时，导入也通过 CAS 写入云端，不只修改本机缓存。不声称兼容 TOTP authenticator 的导出格式。

本机导入导出只接受管理页面的登录 session，MCP 执行 token 不能调用。`secrets_list` 无参数，返回全部 key 名称，不含值；执行接口不提供任何凭据明文读取。

## 部署参考实现

Linux 构建需要 Rust、Node.js、pnpm、C 编译器、OpenSSL 开发包和 pkg-config，不需要桌面 GTK/WebKit 依赖：

```bash
pnpm --dir web install --frozen-lockfile
pnpm --dir web build
cargo build --locked --release -p localshelld-sync-server
sudo bash deploy/sync/install.sh "$PWD/target/release/localshelld-sync-server" sync.example.com
```

安装脚本要求已有 Nginx、Certbot、systemd，并且域名已解析至当前服务器、80/443 可达。它只配置独立的 `localshelld-sync` 站点和服务，使用 Certbot webroot 获取证书并登记 Nginx reload hook。应确认系统已有 Certbot 续期 timer，必要时启用对应的发行包或 Snap timer。

服务以独立的 `localshelld-sync` 系统账户运行，只监听 `127.0.0.1:32121`。Nginx 提供 TLS、请求限流和连接数限制，并为 `/v2/relay/` 转发 WebSocket 升级；设备中转与同步使用同一个域名和服务，不需要另外部署中转程序。程序与数据分别位于 `/opt/localshelld-sync`、`/var/lib/localshelld-sync`。初始化码：

```bash
sudo cat /var/lib/localshelld-sync/bootstrap-code
```

检查：

```bash
systemctl status localshelld-sync.service
curl --fail https://sync.example.com/health
sudo certbot renew --cert-name sync.example.com --dry-run
```

数据目录权限 0700，密钥与状态文件权限 0600。状态使用 AES-256-GCM 整体加密、原子替换并 fsync；同一目录只能由一个进程持有。`master.key` 缺失、状态损坏或被占用时拒绝启动，不创建空库覆盖。JWT 签名密钥和凭据都包含在加密状态中。Windows 构建用于开发测试；生产部署模板面向 Linux。

这不是端到端加密：后端进程及掌握服务器数据和 master key 的管理员可以解密凭据。备份应包含 `state.aes` 与对应的 `master.key`，并单独做好权限与保密管理。丢失全部通行密钥后，旧初始化码不能重新启用；从可用备份恢复，或使用独立的新数据目录重新初始化，再导入凭据文件。

升级替换二进制并重启此服务，保留数据目录；客户端会重连。安装脚本为旧二进制保留 `.previous`，不清空凭据库。

## 参考

- [WebAuthn server implementation](https://docs.rs/webauthn-rs/0.5.5/webauthn_rs/)
- [JWT 验证约束](https://www.rfc-editor.org/rfc/rfc8725.html)
- [Certbot webroot 与续期](https://eff-certbot.readthedocs.io/en/stable/using.html)

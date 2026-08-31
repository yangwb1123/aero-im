# Snaplink 应用接入与弹性运维指南

本文面向接入 ERP、工单、监控等外部系统的开发者，以及负责 Aero IM、Snaplink 和 Aero Vault 的运维人员。它描述当前实现的接口与边界，不把机器身份转换成人类账号：

- 人类管理员通过 Snaplink SSO 登录 Aero IM，再以正常的 Aero 会话管理安装。
- 外部应用通过 Snaplink `client_credentials` 获取短期 access token，只能调用机器发布和上传接口。
- 每个安装把机器 token 的精确 `(issuer, client_id)` 绑定到一个工作区 Bot 和目标白名单，并以独立的 `user_identity_issuer` 解析人类 Snaplink `sub`。
- 消息、聊天记录、事件 outbox、幂等回执和审计记录存入 PostgreSQL；附件元数据也在 PostgreSQL，附件字节可统一存入 Aero Vault。

## 0. 登录页面与 Snaplink SDK

Aero IM 直接使用 Snaplink 生成的 TypeScript SDK：源文件同步到
[`web/vendor/snaplink_sso_client.ts`](../web/vendor/snaplink_sso_client.ts)，由
esbuild 生成浏览器可执行的
[`web/vendor/snaplink_sso_client.js`](../web/vendor/snaplink_sso_client.js)。
`snaplink_auth.js` 只负责页面状态和 Aero 会话衔接，不重新实现 SDK 的 HTTP
操作。SDK 的 `login`/`postMFAComplete` 会直接调用 Snaplink 的
`POST /auth/login`、`POST /auth/mfa`；Aero 只把返回的 ID token 换成本项目会话，
不会保存或接收 Snaplink client secret。

登录页面由 `GET /api/auth/config` 暴露的非敏感配置控制：

```bash
# Aero 自有登录页面；表单由 Snaplink SDK 验证账号
AERO__OIDC__LOGIN_PAGE=local

# 跳转 Snaplink 托管登录页面（生产推荐）
AERO__OIDC__LOGIN_PAGE=snaplink

# 同时显示两种入口（默认，兼容旧部署）
AERO__OIDC__LOGIN_PAGE=both
```

在 `both` 模式下，若 Snaplink 已配置，Aero 自有表单同样使用官方 SDK，两个入口都可用；
只有未配置 Snaplink 时才保留旧的 Aero 用户名/密码注册流程。`local` 模式要求 Snaplink 的 CORS 精确允许 Aero 的来源（例如
`https://im.ywbsd.site`），因为浏览器 SDK 会直接调用 Snaplink 的 JSON API；
不得使用 `*` 搭配凭据。`snaplink` 模式走服务端 PKCE/HttpOnly-cookie 流程，
不需要浏览器跨域调用。修改模式后滚动重启 Aero IM，并在浏览器重新打开页面。

示例约定：

```bash
AERO_BASE=https://im.ywbsd.site
SNAPLINK_BASE=https://sso.ywbsd.site
WORKSPACE_ID='<WORKSPACE_ULID>'
BOT_ID='<BOT_PARTICIPANT_ULID>'
ROOM_ID='<ROOM_ULID>'
INSTALLATION_ID='<INSTALLATION_UUID>'
```

`WorkspaceId`、`ParticipantId`、`RoomId`、`MessageId` 和 `BlobId` 在 HTTP 中是 ULID；`installation_id` 和 `Idempotency-Key` 是 UUID，不要混用。

## 1. 部署前配置

所有 Aero IM 实例必须使用完全相同的集成鉴权配置：

```bash
AERO__INTEGRATIONS__ISSUER=https://sso.ywbsd.site
AERO__INTEGRATIONS__AUDIENCE=aero-im-integration
AERO__INTEGRATIONS__JWKS_URI=https://sso.ywbsd.site/.well-known/jwks.json
# 每实例同时持有的完整 multipart/扫描/Vault 写入任务；范围 1..=32，默认 4
AERO__INTEGRATIONS__UPLOAD_MAX_CONCURRENCY=4
# 人类登录身份的可信命名空间；也作为新安装 user_identity_issuer 的默认值
AERO__OIDC__ISSUER=https://sso.ywbsd.site
```

- `ISSUER` 未设置时回退到 `AERO__OIDC__ISSUER`。
- `JWKS_URI` 未设置时回退到 `AERO__OIDC__JWKS_URI`。
- `AUDIENCE` 没有回退值，必须显式设置。
- `AERO__INTEGRATIONS__ISSUER` 仅校验机器 token；`AERO__OIDC__ISSUER` 仅作为人类身份命名空间。两者可以相同，也可以独立迁移。
- 两类 issuer 都是精确、区分大小写的命名空间；不会自动增删末尾 `/`。
- 修改这些值后要滚动重启全部实例。进程内 JWKS provider 会缓存密钥，不能依赖运行中修改环境变量。

在 Snaplink 中注册机密客户端时，至少允许：

- grant：`client_credentials`
- scope：`aero.notify.publish`
- resource/audience：与 `AERO__INTEGRATIONS__AUDIENCE` 完全一致
- 签名算法：RS256 或 EdDSA；Aero IM 不接受对称算法或 ES256 机器令牌

### Aero Vault

生产多实例部署应使用所有实例都能访问的共享对象存储。选择 Aero Vault 时，各 Aero IM 实例配置同一租户和前缀：

```bash
AERO_BLOB_BACKEND=vault
AERO_VAULT_URL=https://vault.example.com
AERO_VAULT_TENANT=aero-im
AERO_VAULT_PREFIX=im-attachments
AERO_VAULT_OAUTH_TOKEN_ENDPOINT=https://sso.ywbsd.site/token
AERO_VAULT_OAUTH_CLIENT_ID=aero-im-vault
AERO_VAULT_OAUTH_CLIENT_SECRET='<SECRET>'
AERO_VAULT_OAUTH_SCOPE='read write'
AERO_VAULT_OAUTH_RESOURCE=aero-vault
```

这是 Aero IM 服务访问 Aero Vault 的独立机器身份，与 ERP 发布通知所用客户端不同。浏览器、管理员和 ERP 的 token 都不会透传给 Aero Vault。显式选择 `vault` 后，配置不完整会使服务启动失败，不会静默回退到本机目录。

## 2. 管理员安装、轮换与停用

管理接口要求工作区的有效 Owner/Admin 身份。这里的 `Authorization` 是用户完成 Snaplink SSO 后获得的 Aero 会话凭据，不是外部应用的 client_credentials token。

### 创建安装

先准备一个属于该工作区、未停用的 Bot，并让它加入所有允许发布的房间。创建安装：

```http
POST /api/workspaces/{workspace_id}/integrations
Authorization: Bearer {aero_admin_token}
Content-Type: application/json

{
  "bot_id": "<BOT_PARTICIPANT_ULID>",
  "client_id": "erp-production",
  "name": "ERP production notifications",
  "user_identity_issuer": "https://sso.ywbsd.site",
  "allow_user_dm": false,
  "room_ids": ["<ROOM_ULID>"]
}
```

成功返回 `201 Created` 和安装对象。响应中的 `id` 是后续机器接口所用的安装 UUID。机器 `issuer` 只能由服务器的 IntegrationAuthConfig 写入，请求体不能指定；可选的 `user_identity_issuer` 只接受这个已通过 Aero 用户鉴权和工作区 Owner/Admin 校验的管理接口提供。省略它时使用服务端可信的 `AERO__OIDC__ISSUER`，两者都不存在则拒绝创建。机器发布接口和机器 token claim 永远不能写入或覆盖人类 issuer。Aero IM 不保存 Snaplink client secret，也不返回任何机器凭据。

从旧版本升级时，迁移会把既有安装的 `user_identity_issuer` 回填为原 `issuer`，因此原本共用一个 Snaplink issuer 的安装行为不变；升级后可再由管理员独立轮换人类命名空间。

约束如下：

- 同一工作区内 `(issuer, client_id)` 唯一。
- `room_ids` 最多 100 个；每个房间必须属于该工作区，且 Bot 已是房间成员。
- 每个工作区最多 200 个安装。
- `allow_user_dm=false` 会禁止 `snaplink_user` 目标，但不影响房间目标。

`allow_user_dm` 在 HTTP 和数据库层都默认 `false`。只有确实需要按 Snaplink
稳定 `sub` 主动私聊、并已评审用户阻止关系、信息屏障与通知合规策略时才改为
`true`；这会扩大外部应用可触达的账号范围，不应作为普通房间推送的默认配置。

### 查询安装

```http
GET /api/workspaces/{workspace_id}/integrations
Authorization: Bearer {aero_admin_token}
```

响应为 `{"installations":[...]}`，包含 Bot、client ID、启用状态、DM 策略和完整房间白名单，不包含 secret。

### 轮换 client、Bot 或白名单

```http
PATCH /api/workspaces/{workspace_id}/integrations/{installation_id}
Authorization: Bearer {aero_admin_token}
Content-Type: application/json

{
  "rotate_to_current_issuer": false,
  "user_identity_issuer": "https://sso.ywbsd.site",
  "client_id": "erp-production-v2",
  "bot_id": "<NEW_BOT_PARTICIPANT_ULID>",
  "room_ids": ["<ROOM_ULID>", "<ANOTHER_ROOM_ULID>"]
}
```

PATCH 中省略的字段保持原值；提供 `room_ids` 时是替换整个白名单，不是增量追加。client、Bot、白名单和策略在一个数据库事务内切换。请求体不接受机器 `issuer` 字段；需要迁移机器 issuer 时只能设置 `rotate_to_current_issuer:true`，服务端会从当前可信 `IntegrationAuthConfig` 注入精确 issuer。`user_identity_issuer` 是独立字段，只能由当前工作区 Owner/Admin 显式轮换，不随机器 token 或机器 issuer 自动变化。

- 仅轮换 Snaplink secret 且 `client_id` 不变时，不需要修改 Aero 安装；在 Snaplink 完成 secret 轮换即可。
- 轮换 `client_id` 时，先在 Snaplink 准备新客户端，再 PATCH 同一个安装 ID。切换后旧 client 的 token 立即不能用于该安装。
- 保留同一个安装 ID 可保留历史幂等回执。不要为同一生产流量临时创建第二个安装来模拟双写，否则两个安装的幂等命名空间不同，可能生成两条消息。
- 轮换 Bot 前，新 Bot 必须已经是工作区及新白名单内所有房间的有效成员。旧消息仍归旧 Bot；同一 installation 的 durable blob ledger 会保留轮换前已完成上传但尚未发送的附件，新 Bot 可在同工作区、当前允许目标中继续引用，无需重新上传。该授权不会跨 installation 或跨工作区扩散。

### 迁移机器 issuer

机器 issuer 迁移使用短暂停写，避免旧、新 Snaplink 命名空间同时向同一业务流发送：

1. 暂停该 ERP 的业务 outbox 发送，但保留待发送记录、原请求体和 Idempotency-Key；等待在途 HTTP 请求结束。
2. 在 Snaplink 新 issuer 准备 `client_credentials` 客户端、scope、audience 和 JWKS。优先显式配置独立的 `AERO__INTEGRATIONS__ISSUER`，不要把人类 OIDC issuer 的迁移与机器迁移隐式绑在一起。
3. 将全部 Aero IM 实例切到同一组可信的新 integration issuer/JWKS 配置并滚动验证健康；此时旧 issuer token 会 fail-closed，ERP 仍保持暂停。
4. 使用 Snaplink 登录后的 Aero Owner/Admin 会话 PATCH **原 installation ID**，不能新建平行安装：

   ```json
   {
     "rotate_to_current_issuer": true,
     "client_id": "erp-production-on-new-issuer"
   }
   ```

   issuer 与可选的 client ID、Bot、白名单在一个 PostgreSQL 事务中切换。历史通知/blob 回执和 durable blob ledger 继续挂在同一 installation ID；审计只记录 `issuer_rotated:true`，不保存旧/新 issuer 字符串。
   此操作不会改动 `user_identity_issuer`，所以只迁移机器发行方不会把人类 DM 解析切到机器命名空间。
5. 用新 issuer/client token 和一个全新测试 Idempotency-Key 验证房间通知、重放和附件；确认旧 issuer/client 对该安装立即返回拒绝。
6. 恢复 ERP outbox，所有未完成业务事件继续使用暂停前持久化的原 Idempotency-Key 与请求体。若验证失败，在恢复流量前重新配置受信旧 issuer，再用同一布尔开关受控旋回；不要直接改数据库。

### 迁移人类身份 issuer

人类 OIDC issuer 迁移独立于机器 issuer。先完成 OIDC 登录与 `sso_identities` 的受控身份迁移，再以 Aero Owner/Admin 会话 PATCH 原安装：

```json
{
  "user_identity_issuer": "https://new-human-sso.example"
}
```

该操作只改变后续 `snaplink_user` 的 `(user_identity_issuer, subject)` 解析；原机器 `(issuer, client_id)` 继续授权。审计仅记录 `user_identity_issuer_rotated:true`，不会记录旧、新 issuer 值。切换前必须确认目标 issuer 下的身份已落库，否则用户 DM 会按安全默认返回未找到。

### 停用与恢复

以下 DELETE 是可恢复的软停用，不会删除安装和幂等回执：

```http
DELETE /api/workspaces/{workspace_id}/integrations/{installation_id}
Authorization: Bearer {aero_admin_token}
```

成功返回 `204 No Content`。也可用 PATCH 设置 `{"active":false}`。恢复使用：

```http
PATCH /api/workspaces/{workspace_id}/integrations/{installation_id}
Authorization: Bearer {aero_admin_token}
Content-Type: application/json

{"active":true}
```

停用后发布和上传都 fail-closed。恢复前应确认绑定 Bot 仍有工作区和房间访问权。

## 3. 获取和校验机器令牌

推荐通过 HTTP Basic 向 Snaplink `/token` 请求令牌，但长期 client secret 不得通过
curl 的 `--user` 选项进入进程参数，也不得出现在 shell 历史或普通环境变量里。
由 secret manager（或 systemd `LoadCredential=`）以运行账号所有、`0600` 权限原子渲染 curl 配置，例如
`/run/credentials/erp-im/snaplink-token.curl`：

```text
url = "https://sso.ywbsd.site/token"
basic
user = "erp-production:<secret-manager-rendered-client-secret>"
fail
silent
show-error
header = "Content-Type: application/x-www-form-urlencoded"
data-urlencode = "grant_type=client_credentials"
data-urlencode = "scope=aero.notify.publish"
data-urlencode = "resource=aero-im-integration"
```

运行时只把配置文件路径放入命令行，并把短期 token 响应和后续 Authorization 配置
也限制为 `0600`：

```bash
SNAPLINK_TOKEN_CURL_CONFIG=/run/credentials/erp-im/snaplink-token.curl
TOKEN_RUNTIME_DIR="${XDG_RUNTIME_DIR:?}/erp-im"
TOKEN_RESPONSE="$TOKEN_RUNTIME_DIR/snaplink-token.json"
AERO_AUTH_CURL_CONFIG="$TOKEN_RUNTIME_DIR/aero-authorization.curl"

test "$(stat -c '%a' "$SNAPLINK_TOKEN_CURL_CONFIG")" = 600 || exit 1
install -d -m 0700 "$TOKEN_RUNTIME_DIR"
umask 077
curl --config "$SNAPLINK_TOKEN_CURL_CONFIG" --output "$TOKEN_RESPONSE"
chmod 0600 "$TOKEN_RESPONSE"

jq -er '.access_token
  | select(type == "string" and test("^[A-Za-z0-9._~-]+$"))
  | "header = \"Authorization: Bearer \(.)\""' \
  "$TOKEN_RESPONSE" >"$AERO_AUTH_CURL_CONFIG"
chmod 0600 "$AERO_AUTH_CURL_CONFIG"
trap 'rm -f "$TOKEN_RESPONSE" "$AERO_AUTH_CURL_CONFIG"' EXIT
```

也可使用 secret manager 渲染的 mode-`0600` netrc，并仅传
`curl --netrc-file /run/credentials/...`。不要使用默认的共享 `~/.netrc`。任何包含
凭据的脚本段都禁止 `set -x`、`bash -x` 和 CI 命令回显；日志、崩溃报告与 APM
header capture 也必须排除凭据。上例的 token 文件仅应存在于账号私有的运行时目录，
过期或进程退出后立即删除。

使用响应的 `access_token` 调用 Aero IM。机器 token 必须满足全部条件：

| 项目 | 要求 |
|---|---|
| JOSE `typ` | `at+jwt` 或 `application/at+jwt` |
| `alg` | RS256 或 EdDSA，且签名密钥来自配置的 JWKS |
| `iss` | 与集成 issuer 精确相等 |
| `aud` | 包含配置的集成 audience |
| `sub`、`client_id` | 均存在、非空、无首尾空白/控制字符，并且二者完全相等 |
| `exp` | 必须存在且未过期 |
| `nbf` | 必须存在且令牌已经生效 |
| `iat` | 必须存在，且不能比 Aero IM 时钟超前超过 60 秒 |
| `jti` | 必须存在、非空且不含控制字符 |
| scope | `scopes` 数组或空格分隔的 `scope` 中必须包含 `aero.notify.publish` |

`exp`、`nbf` 和 `iat` 允许 60 秒时钟偏差。所有节点都应运行 NTP/chrony。`jti` 用于令牌标识，不替代通知请求的 `Idempotency-Key`。

## 4. 发布房间通知

每次业务事件生成一个稳定 UUID，并在所有重试中复用：

```http
POST /api/integrations/v1/installations/{installation_id}/notifications
Authorization: Bearer {snaplink_access_token}
Idempotency-Key: 93b8df55-8d2e-43c2-a79f-73f67d63a978
Content-Type: application/json

{
  "target": {
    "type": "room",
    "room_id": "<ROOM_ULID>"
  },
  "blocks": [
    {
      "type": "text",
      "content": "ERP 单据 SO-20260803-001 已审批"
    },
    {
      "type": "card",
      "schema": "erp.sales-order.v1",
      "payload": {
        "order_no": "SO-20260803-001",
        "status": "approved"
      }
    }
  ]
}
```

房间必须在安装的 `room_ids` 白名单中，Bot 在提交时仍须具备有效发言权限。消息还会经过普通消息路径的 Block 大小校验、关键词审核、垃圾/慢速模式、PII 策略和附件授权检查，机器身份不会绕过工作区治理。

首次提交成功返回：

- `201 Created`
- `Location: /api/messages/{message_id}`
- `Idempotency-Replayed: false`
- JSON：`{"message":{...},"deduplicated":false}`

## 5. 向 Snaplink 用户发私聊

目标 `subject` 必须是安装的精确 `user_identity_issuer` 下的 Snaplink 稳定 `sub`，不是邮箱、显示名或 Aero participant ID。它不使用机器 token 的 `issuer` claim。该身份必须已经通过可信 OIDC 登录映射到当前工作区的有效成员。

```http
POST /api/integrations/v1/installations/{installation_id}/notifications
Authorization: Bearer {snaplink_access_token}
Idempotency-Key: e4a78d7e-2ad2-49d3-8c95-e8887b8f34b7
Content-Type: application/json

{
  "target": {
    "type": "snaplink_user",
    "subject": "snaplink-stable-user-subject"
  },
  "blocks": [
    {
      "type": "text",
      "content": "你负责的采购申请 PR-1042 已退回，请补充报价单。"
    }
  ]
}
```

Aero IM 会在该工作区内查找或创建 Bot 与用户的真实双人 DM，并按普通私聊保存历史、产生实时事件和通知副作用。以下情况会拒绝请求：

- 安装的 `allow_user_dm` 为 false；
- `(user_identity_issuer, subject)` 不存在、已擦除，或用户在工作区被停用/删除；
- Bot 与用户之间存在信息屏障、阻止关系或其他 DM 策略限制；
- 目标 subject 实际指向安装 Bot 自身。

## 6. 附件：先上传，再在 Block 中引用

外部应用不要直接向 Aero Vault 写对象，也不要自行构造 `blob_id`。先以同一个安装和目标调用上传接口；服务会完成 MIME/内容检查、病毒扫描策略、工作区存储区域选择、Aero Vault 写入和 PostgreSQL 元数据落库。

房间附件：

```bash
curl --fail --silent --show-error \
  --config "$AERO_AUTH_CURL_CONFIG" \
  -X POST \
  -H "Idempotency-Key: 41f7f49f-97db-4a70-913c-1cc51d2fc9ed" \
  -H "X-Aero-Room-Id: $ROOM_ID" \
  -F 'file=@./approval.pdf;type=application/pdf' \
  "$AERO_BASE/api/integrations/v1/installations/$INSTALLATION_ID/blobs"
```

用户 DM 附件改用 `X-Aero-Snaplink-Subject: <SNAPLINK_STABLE_SUBJECT>`；房间目标使用 `X-Aero-Room-Id`。两个静态目标头必须且只能提供一个，且都只允许单值、trim 后完全一致、无控制字符的有界 ASCII 值。服务不接受 `?subject=...`，避免稳定外部身份进入 FRP/反向代理的 request-target 与普通 access log。`X-Aero-Snaplink-Subject` 仍属于敏感标识，代理、WAF、APM 和 trace 的 header capture 必须使用 allowlist 并排除或完整脱敏该头。单文件最大 32 MiB，multipart 字段名必须是 `file`。

上传响应示例：

```json
{
  "id": "<BLOB_ULID>",
  "name": "approval.pdf",
  "mime": "application/pdf",
  "size": 24831,
  "kind": "document",
  "workspace_id": "<WORKSPACE_ULID>",
  "storage_region": "default",
  "residency_scoped": true,
  "message_attachment_eligible": true
}
```

随后把响应中的 `id`、`name`、`size`、`kind` 原样放入同一目标的通知 Block：

```json
{
  "target": {
    "type": "room",
    "room_id": "<ROOM_ULID>"
  },
  "blocks": [
    {
      "type": "text",
      "content": "审批附件"
    },
    {
      "type": "file",
      "blob_id": "<BLOB_ULID>",
      "kind": "document",
      "name": "approval.pdf",
      "size": 24831
    }
  ]
}
```

提交事务会再次锁定并检查附件：必须已完成上传、属于同一工作区并可用于最终房间。安装的 durable blob ledger 允许在 Bot 轮换后继续引用该安装在轮换前上传的附件，但不会授权其他安装或其他工作区。只上传而未成功引用的对象会由保留/GC 机制回收；已被消息引用或仍由安装 ledger 持有的对象不会被普通 GC 误删。

每个安装最多持有 10,000 个不同 Blob，ledger 统计的总大小最多 10 GiB；同一安装重复命中同一个内容 Blob 不重复计费。达到任一上限会拒绝新的 Blob commit。回执到期本身不会错误删除仍被消息或其他安装持有的共享对象；不再被任何 live 消息/emoji/安装 ledger 引用的对象才进入普通 GC。

## 7. 幂等与重试

幂等键的作用域是 `(installation_id, Idempotency-Key)`：

- 首次成功会在一个 PostgreSQL 事务中写入消息、事件 outbox、后续副作用、幂等回执和审计记录。
- 相同安装、相同 UUID、相同目标和相同 Blocks 的重试返回原消息，状态为 `200 OK`，并带 `Idempotency-Replayed: true`。
- 相同键配不同目标或内容返回 `409 Conflict`，绝不会覆盖原消息。
- 回执在 client ID、Bot 和启用状态轮换后仍保留，因此轮换不会让同一个键生成第二条消息。安装处于启用状态且当前 client/Bot 仍有工作区访问权时，当前凭据用完全相同的目标与 Blocks 重试会在重新解析目标前返回旧消息；它不会因 Bot 轮换而新建另一个 DM，也不会把旧键改投到新目标。需要阻断包括历史重放在内的全部机器访问时，应先停用安装。
- 若原消息后来已删除或过期，同键重试返回冲突，而不会创建替代消息。
- 相同请求仍由另一节点处理时，服务只做有限次数的指数退避探测；若尚未完成则返回 `409 Conflict`、`code=integration_request_pending` 和 `Retry-After: 1`，调用方必须使用同一 Idempotency-Key 与完全相同的请求重试，不能生成新键。

请求状态与规范回执的服务端重放窗口为 7 天。窗口内同键严格返回规范结果或冲突；窗口到期后，同一个 UUID 可以被视为新请求，因此接入方不能把 Aero 的短期回执当作永久业务去重表。ERP 应永久记录业务事件到稳定 UUID 的映射，并在自身业务 outbox 中阻止已完成事件再次发送。后台清扫由 `AERO__SERVER__INTEGRATION_RECEIPT_SWEEP_SECS` 控制（示例值 `60`，设为 `0` 禁用）；清扫有界执行，绝不会删除仍持有有效 processing lease 的请求，过期精确重试也会在 claim 时清理旧状态。

接入方应在本地业务 outbox 中持久保存 `installation_id`、业务事件 ID、Idempotency-Key、目标和完整请求体。通知与附件上传都必须带 UUID `Idempotency-Key`；上传重试还必须保持目标、文件名、MIME 和文件内容不变。网络超时、连接中断、上述 pending 409 或 5xx 时，用同一键和同一请求体重试；429 按 `Retry-After` 退避。收到 2xx 后再标记本地任务完成。

HTTP 2xx 表示消息事务已经持久化。同步 NATS 快速发布失败不会把已提交请求改成失败；后台 event-outbox relay 会继续投递。NATS/WS 是 at-least-once，实时消费者仍应按消息 ID和事件序号去重、排序。

## 8. SCIM 账号生命周期与 tombstone

启用 Snaplink 身份绑定时配置：

```bash
AERO__SCIM__IDENTITY_ISSUER=https://sso.ywbsd.site
```

它必须与 `AERO__OIDC__ISSUER` 精确一致。SCIM `externalId` 必须填写 Snaplink 的稳定 `sub`；绑定后不可通过普通 PUT/PATCH 改写或清空。邮箱、姓名和 `userName` 可以变化，但 `externalId` 不应使用会变化的邮箱。

### 停用

```http
PATCH /scim/v2/Users/{participant_ulid}
Authorization: Bearer {workspace_scim_token}
Content-Type: application/scim+json

{
  "schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"],
  "Operations": [
    {"op": "Replace", "path": "active", "value": false}
  ]
}
```

`active=false` 是可恢复暂停：保留 SCIM 资源、工作区成员边和私有房间拓扑，同时写入工作区停用 fence。所有规范鉴权路径会立即拒绝其工作区/房间访问，集成 DM 目标解析也会失败。

### 恢复

将同一路径 PATCH 为 `active=true`。恢复会修复缺失的基础工作区成员关系并移除停用 fence，原有房间拓扑继续生效。

### SCIM 删除

```http
DELETE /scim/v2/Users/{participant_ulid}
Authorization: Bearer {workspace_scim_token}
```

SCIM DELETE 返回 204，并删除该工作区的 SCIM 资源、工作区成员关系和房间成员关系。它只做租户级 deprovision：全局 participant、SSO 身份和历史消息仍保留，也不会创建身份 tombstone。将来用同一 issuer/externalId 再次 provision 时，可重新使用原 participant，但被删除的房间成员关系不会自动恢复。

停用、SCIM 删除和全局擦除都受治理约束。目标若仍是工作区 Owner 或频道 Owner，必须先完成所有权转移，否则返回 409，整个变更回滚。

### 全局账号擦除与 tombstone

全局 GDPR 账号删除是另一条生命周期：当前自助入口为 `DELETE /api/me`，本地凭据账号须提交密码确认。存储事务会匿名化 participant、清理 participant 维度 PII、撤销会话，并为其每个 `(issuer, subject)` 写入 `sso_identity_tombstones`。

tombstone 是不可被普通登录或 SCIM provision 绕过的 deny 记录：

- OIDC JIT 不会为同一 `(issuer, subject)` 创建一个“新账号”；
- SCIM 重试/重建返回 409，提示需要行政恢复；
- SCIM `active=true`、SCIM DELETE/重建都不会清除 tombstone；
- 当前没有面向接入方的公共 tombstone 恢复或账号合并 API。

Snaplink-only 账号不能用 SCIM DELETE 代替全局擦除，也不要直接删除
tombstone 或直接改 `sso_identities`。

### 有计划的 issuer / subject 迁移

若未发生全局擦除，只是 Snaplink 租户、issuer 或稳定 `sub` 需要切换，工作区
有效成员可在同时证明新 Snaplink 身份后，把它绑定到自己的内部 participant。
内部 participant ID 不会变化，因此消息、私聊/群聊、文件、成员关系和审计历史
都无需搬表或改归属。此接口是 self-service，不接受 `participant_id`：

```http
POST /api/workspaces/{workspace_id}/identity-migrations
Authorization: Bearer {aero_user_access_token}
Content-Type: application/json

{
  "from": {
    "issuer": "https://old-sso.example",
    "subject": "stable-old-sub"
  },
  "to": {
    "issuer": "https://sso.ywbsd.site",
    "subject": "stable-new-sub"
  },
  "target_id_token": "<fresh ID token for stable-new-sub>",
  "retire_source": false
}
```

`target_id_token` 必须由当前 `AERO__OIDC__ISSUER` 签名、面向当前
`AERO__OIDC__AUDIENCE`，其 `sub` 与 `to.subject` 精确相等，`to.issuer` 也必须与
配置 issuer 精确相等。令牌必须携带 `iat`，提交时年龄不超过 5 分钟；前后最多
容许 60 秒时钟偏差。令牌只用于本次目标身份证明，不写数据库、不写日志、不进入
审计或响应。建议使用独立的 Snaplink re-auth/账号切换窗口获取该 ID token，避免
把目标账号凭据暴露给管理员或运维脚本。

建议分两阶段执行：

1. 用户以现有 Aero 会话和目标 Snaplink 身份的新鲜 ID token，提交
   `retire_source:false` 增加新登录别名；随后验证新身份进入的是原 participant，
   旧身份仍可回退。
2. 切流稳定后，用户重新获取新鲜目标 ID token，以相同 `from`/`to` 和
   `retire_source:true` 退役源身份。提交时先确保目标别名仍属于同一 participant，
   再删除源绑定并写入 `identity_migrated` tombstone；源身份以后不能被 JIT/SCIM
   重新占用。

调用者必须是目标工作区的当前有效 human 成员，并且 Aero 会话 participant 必须等于
被迁移 participant；停用、移出工作区、删除账号或未满足强制 2FA 的用户均不能调用。
工作区 Owner/Admin 也不能凭一个未占用的 issuer/sub 字符串给其他全局 participant
改绑身份；管理员权限只用于可恢复停用/恢复和 SCIM 租户级 provision/deprovision。
如账号处于停用状态，应先按正常治理流程恢复，再由账号本人重新认证目标 Snaplink
身份。目标身份若已属于别人、已经 tombstone，或源身份不属于当前 participant，整个
事务返回冲突/拒绝且不产生部分迁移。成功响应只返回 participant ID、创建/退役布尔值
和 SCIM 更新数量，不回显 token/issuer/subject；审计事件同样不记录这些外部标识。

若该工作区的 SCIM 行仍以源 `subject` 为 `externalId`，退役事务会精确改为目标
`subject`。跨 issuer 时还应同步更新 Aero IM 的 `AERO__SCIM__IDENTITY_ISSUER` 与
Snaplink provisioner 配置，并遵循“先接受新 issuer、验证、再退役旧 issuer”的滚动
窗口。已因 GDPR 擦除形成的 `account_erased` tombstone 不能通过此接口恢复；账号合并
和擦除恢复仍须走单独的隐私、安全审批流程。

## 9. 多实例弹性伸缩

部署多个 Aero IM 实例时，各层职责如下：

| 组件 | 多实例中的职责 |
|---|---|
| PostgreSQL | 安装、账号映射、消息/历史、附件元数据、幂等回执、审计和 event outbox 的共享事实源 |
| NATS JetStream | `im.room.*` 等跨实例事件传输；未 ACK 事件可重投，客户端按序号去重 |
| Redis | presence、直播观看者、通话 roster 和部分集群限流/路由状态；TTL 心跳清理离线节点状态 |
| Aero Vault | 所有实例共享的附件字节存储；避免本机磁盘随节点缩容而丢失 |
| Hub / WebSocket | 仅本进程连接扇出；节点退出时客户端必须重连 |

通知 REST 请求可以落到任意实例。幂等锁与唯一约束位于 PostgreSQL，所以并发重试打到不同节点仍只会提交一条规范消息。不要把本机内存、单个 WebSocket 节点或本地文件目录当成集群事实源。

每个实例还必须设置唯一且跨重启稳定的 `AERO_INSTANCE_ID`（例如
`im-node-01`）。Aero IM 据此创建独立的 JetStream 实时扇出 cursor；两个
并发实例复用同一 ID 会竞争同一个 cursor，使其中一台的本地 WebSocket
客户端漏掉实时帧。新实例只从启动后的事件开始消费，聊天历史始终从
PostgreSQL 补齐。

缩容顺序：

1. 确认剩余实例容量、PostgreSQL/NATS/Redis/Aero Vault 健康，且负载均衡器使用 `/health/ready`。
2. 向一个实例发送 SIGTERM。实例会立刻让 `/health/ready` 返回 503 `draining`，默认等待 `AERO_SHUTDOWN_DRAIN_SECS=5` 后关闭连接。
3. 给进程至少 `AERO_SHUTDOWN_DRAIN_SECS + AERO_TASK_DRAIN_SECS` 的终止宽限期；后台任务默认再等待 10 秒协作退出。
4. 等待连接从负载均衡摘除、WebSocket 客户端重连，再处理下一个实例。
5. Redis 中该节点的 presence/roster 状态会在显式离线或 TTL 后收敛；不要手工清空整个共享 keyspace。

每个实例对应一个 `aero-server-<instance>-<sha256>` JetStream durable。
临时重启应保留它，以便恢复 cursor；实例永久退役后若一直保留，未消费事件会持续
占用 `IM_MESSAGES` 的 NATS 磁盘。只有在确认该实例不会恢复、客户端已重连且聊天
历史可由 PostgreSQL 补齐后，才删除该精确 consumer：

```bash
nats consumer ls IM_MESSAGES
CONSUMER='aero-server-<已确认退役实例>-<精确sha256>'
nats consumer info IM_MESSAGES "$CONSUMER"
nats consumer rm IM_MESSAGES "$CONSUMER"
```

先用 `info` 核对名称、filter subject 和 pending 数；保留交互确认并记录变更。
不要删除或 purge `IM_MESSAGES` stream，也不要用前缀批量删除 consumer。若不能证明
实例已永久退役，就保留 cursor 并将磁盘占用列为待处理运维项。

未完成的 NATS 投递会重投，未完成的数据库作业会由其他 worker 领取，已经提交但尚未快速发布的消息仍在 event outbox 中。若外部调用在缩容时丢失 HTTP 响应，调用方按原 Idempotency-Key 重试即可。

至少保留一个运行中的 `aero-server` 实例；缩容到零会暂停 WebSocket、outbox relay、通知副作用和其他内置 worker，虽然 PostgreSQL 中的待处理状态不会因此消失。

## 10. 本次身份迁移发布顺序（停写升级）

迁移通过 `sqlx::migrate!` 编译进二进制，因此新增迁移后必须先构建新产物，再由该产物执行迁移。尤其是 `0231_managed_external_identity_lifecycle.sql` 不能与旧身份写路径混跑：旧版本可能先提交 participant/成员关系，再因 tombstone trigger 拒绝身份绑定，留下无法登录的幽灵账号；旧擦除路径也不会写 tombstone。

本次发布必须采用短暂停写窗口，而不是 migrate-first 滚动升级：

1. 备份 PostgreSQL，确认备份可列举恢复，并检查 NATS、Redis、Aero Vault 与 Snaplink JWKS 健康。
2. 构建待发布版本，例如 `cargo build --release`；后续迁移命令必须来自这个新产物。
3. 从负载均衡摘除并 SIGTERM **全部旧版** `aero-server`，等待 drain 完成；确认没有旧 OIDC、SCIM、身份迁移或账号擦除 writer 仍能访问数据库。
4. 在停写状态用新产物执行 `target/release/aero-cli migrate`。不要用旧二进制执行新迁移，也不要手改 `_sqlx_migrations`。
5. 只启动新版本实例；逐台确认 `/health/live`、`/health/ready`、数据库迁移版本和后台 worker 正常后再恢复入口流量。
6. 全部实例均为新版本后，才开放 `/api/integrations/v1/*` 并创建生产安装。
7. 用新 UUID 做房间发布，再以相同键重放并确认 `Idempotency-Replayed: true`；随后验证 DM、附件上传、Aero Vault 下载及审计记录。

迁移前失败可以继续运行旧版本。`0231` 已应用或已经产生 tombstone 后，**不得直接回滚到旧应用**；应保持停写并修复前进，或恢复步骤 1 的整库备份后再启动旧版本。不要在有流量时逆向删除表、trigger、列或 tombstone。未来迁移只有在单独证明双向版本兼容后才可恢复普通滚动升级。

Snaplink 签名密钥轮换采用“先加、再签、后删”：先把新公钥发布到 JWKS，再用新 `kid` 签发。Aero IM 遇到未知 `kid` 会强制刷新一次 JWKS；仍应保留旧公钥直到旧 token 的最大 TTL、60 秒时钟偏差和滚动升级窗口全部过去，再从 JWKS 移除。

## 11. 接入验收清单

- 管理员来自 Snaplink SSO，且对目标工作区具有有效 Owner/Admin 权限。
- Snaplink client 的 grant、scope、resource、算法与 Aero 集成配置一致。
- 安装 client ID 与 token 的 `sub == client_id` 精确一致。
- Bot 属于目标工作区，并加入全部允许房间。
- ERP 将业务事件 ID 映射为稳定 UUID Idempotency-Key，并持久化请求体。
- DM 使用 Snaplink 稳定 `sub`，不使用邮箱或显示名。
- 附件先上传，随后在同一目标的消息中引用上传响应。
- 所有实例共享 PostgreSQL、NATS、Redis 和 Aero Vault 配置。
- 负载均衡使用 `/health/ready`，终止宽限期覆盖两个 drain 窗口。
- SCIM `externalId` 与 OIDC `sub` 一致；停用、删除和全局擦除按不同语义执行。
- issuer/subject 切换由账号本人提交 5 分钟内的 target ID-token 证明，先加新别名并验登录，再退役旧别名；请求不含可代选他人的 participant ID，不移动内部 participant 数据，也不直接改身份表或 tombstone。

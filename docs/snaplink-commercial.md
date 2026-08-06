# Snaplink 商业额度、计量与审计接入

本接入把 Aero IM 工作区映射到 Snaplink 商业租户，同时保持请求路径不依赖中心网络：

- Snaplink Billing 提供版本化 Entitlement，并接收月度 usage fact。
- Snaplink Audit Governance 接收 Aero IM 的安全审计事件。
- PostgreSQL 保存最后一个有效 Entitlement 投影、本月计数和统一 durable outbox。
- 普通消息、机器通知、usage fact 和本地安全审计都在数据库事务边界执行，其他 SQL 写入路径不能绕过额度。

该能力默认关闭。关闭时现有 Aero IM 行为不变；开启后必须覆盖数据库中的每个工作区。

## 1. 中心侧准备

每个 Aero IM 工作区使用两套相互独立的 Snaplink confidential client。客户端只使用
`client_credentials`，`sub` 必须等于验签后的 `client_id`：

- Billing client 只允许 `billing:entitlement:read` 和 `metering:write`；
- Audit client 只允许 `audit:event:write`。

Aero IM 会分别申请单 scope token，不转发浏览器、用户或外部 ERP 的 token。Billing
resource 默认是 `billing-api`，Audit resource 默认是 `audit-governance`，可用环境变量覆盖。
Billing/Audit 的 client ID 和 secret 值在所有绑定中都必须全局不同；仅使用不同环境变量名但
注入相同 secret 也会使启动失败。

在 Snaplink Billing 为该工作区的 Billing client 注册服务端 source binding：

- `tenant_id` 是绑定文件中的商业租户；
- `allowed_dimensions` 只能包含 `messages_per_month` 和
  `notifications_per_month`；
- `source_system` 使用下述稳定派生值；
- 请求 body、query 或转发头都不能覆盖 tenant/source。

在 Audit Governance 的同一租户注册同一个 `source_system`，并把 Audit client 的精确
`client_id` 加入 `allowed_client_ids`。Aero IM 不发送 `tenant_id` 字段；Audit Governance
只能通过验签后的 client 和已注册 source 解析租户。

还需为每个租户注册 active schema：`schema_id=aero.im.security`、`version=1`、
`event_type=aero.im.security`、`classification=confidential`。`required_fields` 和
`allowed_fields` 留空，使不同 action 的结构化本地 detail 可以进入 payload；具体操作名保存在
事件的 `action` 字段，而不是动态改变 event type。

source 的派生算法为：

```text
<AERO_SNAPLINK_SOURCE_PREFIX>.<SHA-256(tenant_id UTF-8) 的完整 base64url（无 padding）>
```

这与 Snaplink `auditgovernance.TenantSourceID` 完全一致。旧版截断 hex
source 不能继续用于新投递；升级时先用 provisioner 创建新 source/schema，更新
bindings 文件并提高其 revision，确认新 source 的 ledgered receipt 后再停用旧 source。

默认 prefix 是 `aero-im`。它不是调用方输入，运行时只从受信绑定的 tenant 派生。

## 2. Aero IM 配置

复制 [`snaplink-commercial-bindings.example.json`](snaplink-commercial-bindings.example.json)
到 secret/config volume。文件必须小于 2 MiB、`version` 必须为 `2`，未知字段会使启动失败。
secret 本身不写入文件，只引用环境变量名。

```bash
AERO_SNAPLINK_COMMERCIAL_ENABLED=true
AERO_SNAPLINK_TOKEN_ENDPOINT=https://sso.example.com/token
AERO_SNAPLINK_BILLING_BASE_URL=https://billing.example.com
AERO_SNAPLINK_AUDIT_BASE_URL=https://audit.example.com
AERO_SNAPLINK_BINDINGS_FILE=/run/aero/snaplink-commercial-bindings.json
AERO_SNAPLINK_ACME_BILLING_CLIENT_SECRET='<secret-manager-injected>'
AERO_SNAPLINK_ACME_AUDIT_CLIENT_SECRET='<different-secret-manager-entry>'
AERO_SNAPLINK_LOG_PSEUDONYM_KEY='<independent-random-key-at-least-32-bytes>'
```

所有副本必须挂载完全相同的 desired state 和 secret 版本。绑定规则如下：

- `workspace_id`、`tenant_id` 与两套 destination client 一一对应；所有既有工作区都必须有 enabled binding。
- 每个 client secret 和日志伪名 HMAC key 都必须至少 32 字节，且不能包含首尾空白或控制字符。
- 新绑定从 `revision: 1` 开始；状态或 client ID 变化必须连续加一。旧 revision 和同 revision 不同内容会阻止启动。
- 工作区、tenant 和派生 source 不允许改绑。Billing client 只受 pending usage 约束，Audit client 只受 pending audit 约束；清空对应 outbox 后用连续 revision 原子轮换。secret 原地轮换不需要增加 revision。
- 新建工作区应先在 Billing/Audit 注册 client/source，并把预定 workspace ID 加入文件，再创建工作区。这样创建事务中的安全审计也不会出现无绑定窗口。
- 环境变量未设置不等于关闭：如果数据库已启用商业约束，无配置副本会拒绝启动。受控回滚必须在所有副本显式设置 `AERO_SNAPLINK_COMMERCIAL_ENABLED=false`。

HTTP 仅允许 HTTPS。开发时必须显式设置
`AERO_SNAPLINK_ALLOW_INSECURE_LOOPBACK=true`，且 HTTP host 只能是 localhost/loopback；重定向始终禁用，避免 bearer 泄漏到另一个 origin。

## 3. 产品维度与额度语义

| Aero IM 写入 | Entitlement feature | 月度 limit | 计量 |
|---|---|---|---|
| 用户、Bot 的普通聊天消息 | `im` | `messages_per_month` | 每个已提交 message 为 1 |
| 受信 Snaplink installation 发布的机器通知 | `notifications` | `notifications_per_month` | 每个已提交 notification message 为 1 |

机器通知不会同时扣 `messages_per_month`。因此允许通知但设置 `im=false`、
`messages_per_month.hard=0` 的套餐仍能使用通知 API。

Limit 使用显式 `{soft, hard, unlimited}`：

- `unlimited=false, hard=0` 是确切的零额度，首个写入即拒绝；
- `unlimited=true` 时 `soft` 和 `hard` 必须都为零；
- 有限额度在 PostgreSQL 行锁/原子 upsert 下跨副本执行，不会超卖；
- 删除或编辑消息不会回退月度累计事实。

fact ID 稳定为
`aero-im:<dimension>:<message UUID>`，并同时作为 `Idempotency-Key`。普通消息的 client
idempotency 重放不会产生第二个 message，事务竞争失败也会回滚本地 counter/outbox。

## 4. 故障、重试与升级边界

Entitlement 投影按 `revision` 单调更新：旧版本忽略，同 revision 改内容拒绝。服务启动和
周期 projector 通过 `billing:entitlement:read` 刷新；刷新失败保留最后一个仍有效的本地投影。

- 已有、active、已生效且未过期的投影：中心短暂不可达时继续服务。
- 首次没有投影、inactive、尚未生效或已过期：消息写入 fail closed，readiness 为 503。
- feature 关闭：403；有限 hard quota 用尽：429；本地投影不可用：502。
- `/health/live` 永远不探测 Billing/Audit；`/health/ready` 只检查本地投影，不检查中心网络和 outbox backlog。

usage 和 audit 共用 PostgreSQL leased outbox：`FOR UPDATE SKIP LOCKED`、随机 claim token、
租约到期接管、稳定幂等键、指数退避加抖动。没有最大尝试次数或 dead 状态；每个非成功响应都会重新 park，数据库故障时由租约到期恢复。HTTP 并发、batch、poll 和 lease 均有界；一次冷投递可能依次获取 token 再发送事实，因此 lease 必须长于两次请求 timeout 加安全余量。

迁移启用时，服务会在接受流量前把本月已存在的消息补成 usage fact 并恢复本地 counter；
历史本地 audit 由有界后台 reconciliation 逐批补齐。启用后的新消息和新审计不依赖该扫描，
它们与业务事务严格原子。outbox 会保留已投递行，既作为永久幂等证据，也作为 reconciliation cursor。

从旧的 version 1 shared-client 配置升级时，迁移会先保留旧 client 作为两列的兼容值。先用旧
版本把两类 pending outbox 排空，再部署迁移和 version 2 文件；已有 binding 的 revision 必须
连续加一并安装不同的 Audit client。新安装仍从 revision 1 开始。运行日志只记录使用独立
`AERO_SNAPLINK_LOG_PSEUDONYM_KEY` 生成的固定 HMAC 引用，不记录 tenant、workspace、source、
client 或 delivery ID 原文。

## 5. 审计内容与隐私

所有通过 `AuditRepo` 写入的工作区安全事件都会进入治理 outbox。事件 ID 和
idempotency key 使用本地 audit UUID；`source_system` 来自绑定，payload 不含 tenant。
actor、target、action、发生时间和本地结构化 detail 会进入固定的
`aero.im.security` schema v1，classification 为 `confidential`、retention class 为 `security`。
投影会递归移除 Audit Governance 禁止的敏感字段名；超过 64 KiB 的 detail 只发送摘要、
原始字节数和省略原因，完整内容仍只保留在本地审计表。

不要在本地 audit detail 中写 token、secret 或明文凭据。Audit Governance 的 source
注册、schema/retention policy、存储加密和法律保全仍由其自身控制面负责。

## 6. 灾备与滚动操作

PostgreSQL 恢复必须把业务表、binding、Entitlement、月度 counter 和 outbox 恢复到同一个
事务一致点。恢复后先保持入口摘流，启动一个副本完成本月 usage reconciliation 和
Entitlement 刷新，再开放 readiness；稳定 fact/event ID 允许对 Billing/Audit 安全重放。

滚动升级顺序：

1. 先在 Billing 和 Audit Governance 发布/验证 tenant-scoped source binding。
2. 部署迁移和相同 desired-state 文件；观察 `snaplink_commercial` readiness 和
   `aero_snaplink_delivery_outbox_backlog`。
3. 验证普通消息、机器通知分别只增加自己的维度，并验证 hard=0 拒绝。
4. 轮换前等待对应 destination 的 pending outbox 为零，再发布连续 revision；两套 secret 分开轮换。
5. `AERO_TASK_DRAIN_SECS` 必须大于两次 Snaplink request timeout、商业 shutdown drain 和两秒安全余量；systemd/Kubernetes 的终止预算还要覆盖前置摘流窗口。

可调参数及默认值见 [`.env.example`](../.env.example)。

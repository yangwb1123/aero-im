# Aero IM — 扩展路线图(ROADMAP · 第二版)

> **资深架构师 / 产品经理视角。** 基于 2026-06-08 全局代码扫描,当前基线为 Wave 24 + Post-Wave 补全(87 功能点、65 条迁移、846 单测、全部第一版 ROADMAP 方向已交付)。  
> 本文只做规划与论证,不含任何代码。  
> 工程约定见 [`AGENTS.md`](AGENTS.md),交付状态见 [`README.md`](README.md)。

---

## 0. 当前基线确认

全局扫描确认以下部分已正确交付,后续工作建立在其之上:

- **内容安全**: `AiJobKind::Moderate` 调用 Anthropic 分类 API,返回结构化 verdict;`KeywordModerator` 为预审前置过滤。
- **通话编排**: `aero-im-call::CallOrchestrator` 完整实现 invite/answer/end/join/leave;ws.rs 通过 ImService 驱动全生命周期;SFU peer 注册/拆除通过 `SfuRouter`。
- **SRT 可靠性**: `pump()` 调用已接入 UDP send 回路;ACK/NAK/ACKACK 正确发回发送方。
- **WHIP 级联**: `WhepUpstreamSource` 实现 `UpstreamSource`,通过 WHEP SDP 交换 + str0m 接收 RTP → Annex-B 接入 `CascadeRelay`。
- **AI 流式 + 记忆**: `/api/ai/ask/stream` 返回 SSE;`ask_with_context()` 通过 Redis `AiContextStore` 保留滚动会话历史。
- **Quick Wins 全部完成**: 有界 WS channel、Blob IDOR 防护、WS since 游标、Hub O(1) 注销、Tower TimeoutLayer、CORS 白名单、jti nonce。

---

## 方向一：生产集群就绪——从单节点原型到水平可扩展服务

### 代码证据

**Blob 存储**(`crates/aero-storage/src/blob_store.rs` 第 34 行):  
`LocalFsBlobStore` 将文件写入节点本地磁盘 `blob_dir`。在三节点部署中,节点 A 上传的文件在节点 B 上不可访问。`BlobStore` trait 已定义(第 23 行),S3/MinIO 适配口已预留但从未实现。

**Hub 在线人数**(`crates/aero-server/src/hub.rs` 第 216、254 行):  
`room_members_online()` 和 `stream_viewer_count()` 返回的是**当前进程内**的连接数。即使 `StreamViewerStore`(Redis HyperLogLog)已在 `live_presence.rs` 中实现,Hub 的两个方法仍从本地 `DashMap` 读取,多节点环境下将返回不准确数据。

**NATS 发布静默失败**(`crates/aero-im-core/src/service.rs` 第 1489 行):

```
if let Err(err) = publish_event(...) {
    warn!(?err, %subject, "publish RoomEvent failed");  // 仅 warn,不向调用方返回错误
}
```

消息已写入 Postgres,但 NATS 断开时广播静默丢失。调用方收到 `200 OK`,但其他节点上的 WebSocket 订阅者永远不会收到该消息。

**数据库连接池**(`crates/aero-storage/src/db.rs` 第 8 行):  
`PgPoolOptions::new().max_connections(max_conns)` 未设置 `acquire_timeout`,连接耗尽时请求将无限阻塞。

### 为什么优先级最高

1. **不可绕过的物理约束**: Blob 存储是本地文件系统这一事实意味着无论如何增加节点,都无法做到无状态横向扩展。这不是性能问题,而是正确性问题——节点 B 的 CDN 请求会直接返回 404。
2. **在线人数不一致是产品级 Bug**: 直播间显示 "1200 观看" 但实际 3 节点各显示约 400——用户体验崩溃,创作者数据分析失真,广告主无法信任数据。
3. **NATS 静默失败是未记录的数据丢失**: 当前行为是 "消息入库但不广播",用户感知为 "消息发出但对方看不到"。这在 B2B 企业场景中是合规风险,不只是 UX 问题。

### "完成"的样子

- `BlobStore` 有一个生产级 `S3BlobStore`(从环境变量 `AERO_BLOB_BACKEND=s3` 切换);`LocalFsBlobStore` 保留用于本地开发。
- `/api/rooms/:id/online` 和计数端点从 Redis 聚合数据读取,Hub 本地数据作为 Redis 不可用时的降级兜底。
- `publish_room_event()` 失败时向调用方传播错误;HTTP 层在 NATS 不可用时返回 `503`(而非静默丢失);客户端重试协议有文档。
- `PgPoolOptions` 明确配置 `acquire_timeout(Duration::from_secs(5))` 和 `idle_timeout`,连接耗尽时快速失败而非无限等待。

---

## 方向二：移动端推送通知——补全不在前台就失联的致命缺口

### 代码证据

全局搜索 `FCM`、`APNs`、`push_notification`、`device_token`:**零匹配**。

当前通知链路依赖 WebSocket 实时连接。当用户将 App 切到后台(iOS 15 秒后断开 WS,Android 电源管理主动 Kill),以下所有交互均无法触达:

- `@提及`(`crates/aero-storage/src/notification.rs` 中 `inbox` 表有持久化行,但无推送出口)
- 未接来电(`crates/aero-server/src/ws.rs` CallEnd 分支写入 `activity_feed`,但无推送)
- DM 私信、任务指派、审批请求、开播通知

`aero-storage/src/notification.rs` 已有持久化通知模型;`ActivityFeedRepo` 写入逻辑完整。推送分发层完全缺失。

### 为什么优先级高

1. **移动 App 失去核心价值**: 一个不能在后台接收消息通知的 IM App,用户留存率接近零。Slack、Teams、Lark 的移动用户日活之所以高,根本上依赖推送。没有推送 = 没有移动端产品。
2. **基础设施已就位,工作量集中在分发层**: 通知模型(`inbox` 表)、未读计数、活动 Feed 全部完成。缺的只是:设备 token 注册表(一张 migration)+ FCM/APNs HTTP 调用(一个 `PushService` crate)+ 现有 notification 写入点之后插入分发调用。
3. **企业客户的硬门槛**: 企业 IT 采购评审表上,推送通知是 "Required" 功能。没有它,无法进入任何 MDM 管理的企业移动设备部署。

### "完成"的样子

- 新增 `device_tokens` 表:`(participant_id, device_id, platform: fcm|apns, token, updated_at)`;`POST /api/me/device-token` 注册/刷新。
- `aero-push` crate 实现 `PushGateway` trait,两个具体实现:`FcmGateway`(Android/Web)和 `ApnsGateway`(iOS/macOS);令牌从环境变量注入,单测 mock 接口。
- 现有 `inbox` 写入点(提及通知、未接来电、任务指派等)在写入后触发 `PushGateway::send(participant, payload)`,best-effort(推送失败不影响 IM 主链路)。
- 推送 payload 遵循平台规范:FCM `data` 字段携带 `room_id`、`message_id`;APNs `aps.alert` 包含发送方昵称和前 N 个字符。
- 静默推送(badge 更新)不唤醒屏幕;带内容的推送尊重 DND/snooze 状态(与现有 `NotificationPrefRepo` 联动)。

---

## 方向三：安全加固——会话凭据与文件上传的已知漏洞

### 代码证据

**刷新令牌不轮换**(`crates/aero-auth/src/service.rs`):  
刷新端点消费旧 token 并颁发新 access token,但旧刷新 token 未被吊销。攻击者窃取一次刷新 token 即可永久维持访问,直至用户主动登出。现有 `auth_sessions` 表有 `revoked_at` 字段,但刷新调用路径不写入该字段。

**文件上传无 MIME 白名单**(`crates/aero-server/src/routes.rs` 第 921–941 行):  
`MAX_BLOB_BYTES = 32 MiB` 已限制大小,`guess_file_kind(&mime)` 已解析 MIME,但未拒绝任何类型。用户可上传 `application/x-msdownload`(Windows 可执行文件)、`text/x-shellscript`(Shell 脚本)等危险内容,经由 `/api/blobs/:id/download` 提供下载。同时 `sha256: None`(第 951 行),文件完整性无法校验,也无法去重。

**无每端点速率限制差异化**(`crates/aero-server/src/rate_limit.rs`):  
全局统一令牌桶,无法对高风险端点(`/api/auth/login`、`/api/auth/reset-password`、`/api/auth/forgot-password`)施加更严格的限制。当前架构允许以全局限速数量级发起凭据枚举攻击。

### 为什么优先级高

1. **刷新令牌不轮换是 OWASP A07(身份认证失败)**: 一旦 token 通过网络劫持、日志泄露或 XSS 被窃取,攻击者持有永久凭据。`auth_sessions.revoked_at` 字段已在数据库中,修复代价极低,但不修复则风险持续累积。
2. **恶意文件上传是已知的平台级漏洞**: 企业 IM 平台曾多次发生通过文件上传分发 RAT(远程访问木马)的安全事件。MIME 白名单不能阻止所有威胁,但能拦截绝大多数脚本型攻击。
3. **密码重置枚举是凭据接管的入口**: `/api/auth/forgot-password` 已做反枚举处理(不泄露是否存在该邮箱),但无速率限制意味着高频探测不受阻止。

### "完成"的样子

- 刷新端点在颁发新 access token 后将旧 `auth_sessions` 行写入 `revoked_at = NOW()`;再次使用已吊销的刷新 token 返回 `401 Revoked`;`AuthService::refresh()` 内原子完成"吊销旧会话 + 创建新会话"。
- `blob_upload` 路由维护 MIME 白名单(`image/*`、`video/*`、`audio/*`、`application/pdf`、`text/plain`、`application/zip` 等),拒绝 `application/x-*`、`text/x-shellscript`、所有可执行类型;同时计算并存储 `sha256` 用于去重和完整性校验。
- 速率限制支持每路由配置:`/api/auth/login` 5 次/分钟/IP;`/api/auth/forgot-password` 3 次/小时/IP;其余路由保持现有全局限速。实现不变(仍用 `RateLimiter`),仅在路由注册时传入不同的 `RateLimitConfig`。

---

## 方向四：GDPR 合规与数据治理——法律义务,不是锦上添花

### 代码证据

**用户删除不做消息归因处理**(`crates/aero-storage/src/participant.rs` 第 146–155 行):

```sql
DELETE FROM participants WHERE id = $1
```

直接硬删除参与者行。消息的 `sender_id` 外键要么级联删除(消息永久丢失,违反审计要求),要么成孤儿(消息保留但发送方消失,违反 GDPR 第 17 条的正确实现)。无论哪种结果都不合规。

**审计日志覆盖不完整**(`crates/aero-server/src/workspaces.rs`):  
审计记录已覆盖成员加入/移除、角色变更、工作区删除。但以下操作**没有**审计记录:
- 管理员删除消息(`DELETE /api/rooms/:id/messages/:msg_id` 路径无 audit append)
- Stream key 轮换
- 工作区成员强制登出(Wave 21 session revoke)
- 频道角色变更

**无用户自助数据导出**(`/api/me/export` 端点不存在):  
`workspaces.rs` 有工作区级导出(管理员权限),但 GDPR 第 20 条要求任何普通用户可获取其个人数据的可携带副本。

**孤儿 Blob 永不清理**(`crates/aero-storage/src/message.rs` 第 112–122 行):  
软删除清空 `blocks` 字段(blob ID 随之丢失),但从不调用 `BlobStore::delete()`。Blob 文件无限累积,无垃圾回收机制。

### 为什么优先级高

1. **GDPR 第 17 条"被遗忘权"是法定义务**: 在欧盟运营的平台,用户要求删除账户时必须删除或匿名化其个人数据。当前实现两种结果都不合规。监管处罚起点是年营业额的 2%。
2. **不完整的审计日志在企业销售中是直接拒绝项**: ISO 27001、SOC 2 Type II 均要求对特权操作有完整审计轨迹。"管理员删除了哪些消息" 是最常见的安全审查问题之一,现在无法回答。
3. **Blob 泄露是隐性存储成本**: 每条被删除的带附件消息都在磁盘上永久留存孤儿文件。规模化后存储账单持续增长,且这些文件可能包含已被用户"删除"的隐私图片。

### "完成"的样子

- `delete_participant()` 改为软删除:将 `participants.deleted_at` 置为当前时间,同时将该用户发送的所有消息执行 `blocks = '[{"type":"text","text":"[已注销用户]"}]'` 式匿名化;auth_sessions 全部吊销。
- 审计日志新增事件类型:`message.deleted`(含操作者 ID + 被删消息摘要)、`stream_key.rotated`、`channel_role.changed`、`session.revoked`;所有管理员操作写入前统一过 `audit_append()` 函数。
- `GET /api/me/export` 端点:异步生成包含该用户所有消息、文件上传列表、账户基本信息的 JSON 归档,返回 24 小时有效的下载链接;大型导出通过 AI 工作队列同款异步架构处理,避免超时。
- 消息软删除路径在清空 `blocks` 时同步提取 blob IDs 并插入 `blob_gc_queue` 表;后台 GC Job(每小时)从队列消费,调用 `BlobStore::delete()`,确认后删除队列行。

---

## 方向五：可观测性纵深——从"能跑"到"可运营"

### 代码证据

**已完成的可观测性基础**:  
`crates/aero-server/src/metrics.rs` 已有:HTTP RED 指标(请求数/延迟/状态码)、速率限制拒绝计数器(第 49 行)、WS 连接 gauge(`WS_CONNECTIONS`)、DB 连接池指标。  
`crates/aero-ai/src/worker.rs` 已有:AI Job 耗时直方图、成功/失败/死信计数器、队列深度 gauge、成本追踪。  
`/health/live` 和 `/health/ready` 已按 k8s 规范分离。

**缺失的关键信号**:

_消息吞吐量_:`crates/aero-im-core/src/service.rs` 中 `send_message()`、`edit_message()`、`delete_message()` 路径均无 Prometheus counter/histogram。无法回答 "峰值每秒处理多少条消息" 这个最基本的运营问题。

_NATS 订阅者积压_:`crates/aero-bus/src/jetstream.rs` 消费者侧无 pending-acks gauge。当 AI Worker 因 Anthropic API 降速而积压时,无法可见,直到 NATS 内存溢出或消息过期。

_SRT/WHIP 媒体指标_:`crates/aero-live-srt/src/lib.rs` 和 WHIP 会话均无推流比特率、丢包率、活跃连接数 gauge。直播运营团队无法判断 "当前 OBS 推流质量如何"。

_AI 死信队列可见性_:`crates/aero-ai/src/worker.rs` 第 60–66 行:死信 job 标记 `status='dead'` 后停止重试,但无告警、无 API、无指标导出。运营需要手动查询数据库才能发现 AI 审核/摘要失败积压。

_分布式追踪采样率_:`crates/aero-common/src/telemetry.rs` 已集成 OpenTelemetry,但采样率硬编码。高流量下全量追踪会产生海量 span 写入,淹没 Jaeger/Tempo。

### 为什么优先级高

1. **没有消息吞吐量指标,容量规划是瞎猜**: 无法回答 "数据库/NATS/Redis 在多少 MAU 时会成为瓶颈",就无法做数据驱动的扩容决策。这在 B2B 销售阶段会直接被技术评审问倒。
2. **NATS 积压不可见是定时炸弹**: AI 审核 Worker 积压时,消息仍会正常投递给用户,但审核延迟可能从秒级扩大到小时级。没有 lag 指标,这个问题只有在监管审查时才会被发现。
3. **直播质量指标是 B2B 直播 SLA 的前提**: 企业客户购买直播能力时会要求 SLA(如 "99.9% 帧接收率")。没有 SRT 丢包率指标,SLA 无从量化,合同无从签署。

### "完成"的样子

- `ImService::send_message()` 路径新增 `MESSAGES_SENT_TOTAL`(counter,按 `room_type` 标签分层)和 `MESSAGE_PROCESSING_DURATION_SECONDS`(histogram);edit/delete 路径类似。
- `aero-bus` JetStream 消费者在每次 `pull_messages()` 后从 NATS server API 查询 `NumPending`,更新 `NATS_CONSUMER_PENDING_MESSAGES`(gauge,按 consumer_name 标签);30 秒查询一次。
- SRT 会话在 `handle_datagram` 循环中更新:`SRT_ACTIVE_SESSIONS`(gauge)、`SRT_PACKETS_RECEIVED_TOTAL`、`SRT_PACKETS_LOST_TOTAL`;WHIP 会话类似统计 RTP 接收/解包失败计数。
- `ai_jobs WHERE status='dead'` 通过定期 SQL 查询导出为 `AI_DEAD_LETTER_QUEUE_SIZE`(gauge,按 `kind` 标签);`/api/admin/ai/dlq` 端点允许管理员查看和重新入队死信 job。
- OpenTelemetry 采样率通过环境变量 `AERO_TRACE_SAMPLE_RATE`(浮点,0.0–1.0,默认 0.01)配置;高优先级操作(登录、消息发送、通话创建)强制采样。

---

## 建议排序

| 优先级 | 方向 | 理由 | 预估规模 |
|---|---|---|---|
| **P0** | 方向三(安全加固) | 刷新 token 不轮换是已知漏洞,MIME 无白名单是恶意文件上传入口;修复代价低,不修复是持续风险 | 小(1–2 周) |
| **P0** | 方向四(GDPR 合规) | 用户删除不匿名化消息在 EU 属违规;企业客户必问审计覆盖度 | 中(2–3 周) |
| **P1** | 方向二(移动推送) | 无推送 = 无移动产品;FCM/APNs 接入是独立工程,基础设施(通知模型)已就绪 | 中(3–4 周) |
| **P1** | 方向一(集群就绪) | Blob 本地存储和 NATS 静默失败在多节点部署时会直接爆发;应在第一次扩容前解决 | 大(4–6 周) |
| **P2** | 方向五(可观测性) | 消息吞吐量和媒体质量指标是运营和 SLA 谈判的前提;基础已有,补全信号即可 | 小(1–2 周) |

---

> 落地原则承袭 [`AGENTS.md`](AGENTS.md):新功能沿用「`RoomEvent` → Hub 扇出」主轴;依赖只加到自身 crate;凡需真实外部服务(FCM/APNs、S3)才能验证的,不在沙箱内标「done」,以冒烟测试 + mock 单测为交付边界。  
> 安全加固(方向三)的任何变更在合并前须经过独立安全审查,不走常规 Wave 流程。

# 架构分析报告：Aero IM 系统

## 1. 架构评估

### 当前架构的优势

**事件驱动骨架成熟度高。** 系统复用 NATS JetStream 作为跨进程事实源 + 进程内 Hub 扇出，这一层的抽象（`EventBus` trait、per-subject seq、durable/ephemeral consumer 的区分）是代码质量最高的部分。AGENTS.md §1 的事件 DAG 描述与实际代码一致：`ImService::publish_room_event` → NATS → `run_bus_listener`（两阶段解码）→ `Hub::fan_out_raw`。这个 pipeline 支持多实例水平扩展，且 `seq` 戳为客户端提供了天然的去重/排序能力。

**Crate 边界清晰，依赖自下而上无环。** 从 `aero-common`（叶子类型）→ `aero-bus`/`aero-storage`/`aero-auth` → `aero-im-core`/`aero-ai`/`aero-live-*` → `aero-server` 的 DAG 是健康的。每个 crate 有明确的职责边界，没有出现跨层耦合。这是 16 个 crate 的系统能保持可维护性的关键。

**Bot/Worker/Timer 体系结构一致。** §2 描述的常驻智能体体系——总线消费 bot（durable consumer） + 作业 worker（SKIP LOCKED 轮询） + 定时器（interval sweep）——是成熟的异步事件模式。`background.rs` 单一装配点使系统启动路径可预测。

**迁移策略老练。** 157 个迁移文件（`migrations/`）使用 `sqlx::migrate!` 编译期嵌入，配合 `CREATE TABLE IF NOT EXISTS` 幂等语句。迁移序号连续、命名有语义前缀（`p2_collab_ai`/`p4_live_interactive`/`p10_transcribe`）。fresh-deploy 的 `make migrate-smoke` 用 throwaway 库全链验证——这是生产级做法。

**可观测性基础设施已就位。** Prometheus gauge 采样器（DB 池、NATS backlog、AI DLQ）、`x-request-id` 注入、`/health` 三探针（`/live`/`/ready`）、OTLP 导出——这些在高吞吐 IM 系统中是常被低估但实际运维时最关键的组件。

### 关键设计决策评估

| 决策 | 合理性 | 备注 |
|------|--------|------|
| **NATS JetStream 做跨实例事件总线** | ✅ 合理 | 纯 Rust 生态中，async-nats 是唯一成熟的 Rust-native 消息系统。JetStream 的 at-least-once + durable cursor 是 IM 实时性/可靠性的好平衡。替代品（RabbitMQ、Kafka）会增加 Ops 复杂度 |
| **Postgres + pgvector 做主要持久化** | ✅ 合理 | 单一数据库减少运维面；pgvector 在 1024 维嵌入下 QPS 可满足中规模；`pg_trgm` 的 `similarity()` 为 FTS 提供了低成本的模糊匹配。但 157 个迁移已开始暴露 schema 膨胀 |
| **Redis sorted-set 做集群状态** | ✅ 合理 | presence/roster/计数器用 Redis 的 `zadd`+`zremrangebyscore` 是成熟的心跳驱逐模式。`INCR + EXPIRE` 到 UTC 午夜的成本模型可复用（如计费 Phase A） |
| **str0m 纯 Rust WebRTC** | ⚠️ 合理但有风险 | 纯 Rust DTLS-SRTP 在生态里是前沿选择。str0m 0.19 的 API 稳定性不如 libwebrtc（C++），但规避了 CGo/FFI 的集成税。风险在于 str0m 的 RTCP/congestion-control 成熟度——生产故障排查时会缺少社区经验 |
| **零依赖 ES2020 SPA** | ⚠️ 双刃剑 | 无构建步骤 = 无 webpack/vite 复杂度和供应链风险。但 ES2020 模块化在大型 SPA 中会导致手动管理 import graph、无 tree-shaking/代码分割、无类型检查。当前 `web/` 目录大小和 153 个 server 模块的数量对比暗示了前端/后端规模的不匹配 |
| **SCIM 仅入站 + 手写 OpenAPI** | ✅ 务实 | 出站同步会增加 3x 复杂度（变更跟踪、重试、冲突解决），入站供给满足 80% 需求。手写 OpenAPI 而非 code-gen 在快速迭代期是正确的取舍 |

### 架构债务与技术债

**P1 债务（应在本季度内处理）：**

1. **Web 前端与后端的功能差距（"Dark Features"）。** 已验证：`templates`/`bookmarks`/`favorites`/`drafts`/`message_reminders` 五个方向后端完备但 web/ 零 UI 引用。这不是简单的"忘了写 UI"——每次 WS 帧到达浏览器但没有 handler 监听时，帧被静默丢弃。这会误导后续开发者认为某个功能正常工作（后端路由 200、总线事件已投递），而用户完全看不到。**治理建议：在 `routes/routes.rs` 的 `.merge()` 链上加注释 `// UI: done`/`// UI: missing`，或在 CI 中加 web handler 覆盖率门禁。**

2. **`aero-server/src/` 模块膨胀（153 个顶层模块）。** 这是 crate-as-feature-unit 治理的反面案例：虽然 crate 分解正确（16 个），但 `aero-server` 内部的模块粒度爆炸了。153 个模块在单一 `mod.rs` 收口，没有二级命名空间，导致：
   - 编译时间劣化（修改任意模块触发 `aero-server` 重新编译）
   - 命名冲突风险（`channels.rs` vs `channel_roles.rs` vs `channel_retention.rs` vs `channel_bookmarks.rs` vs `channel_points.rs` vs `channel_sections.rs`——6 个 `channel_*` 并列）
   - 新开发者认知过载（153 个文件，每个都是 `pub fn routes() -> Router<AppState>`，模式高度重复）
   
   **缓解：按领域划分子目录（`server/src/im/`、`server/src/live/`、`server/src/admin/`、`server/src/ai/`），每目录内部 `mod.rs` 收口。** AGENTS.md §4.4 已意识到"以 crate 为 feature 单位"的原则，但 `aero-server` 内部仍需要二次分解。

3. **`webhooks.rs` 事件类型匹配的单臂实现。** 当前 `run_webhook_dispatcher` 只消费 `RoomEvent::Message`（`matches!` 单臂），而系统实际有 15+ 种 `RoomEvent` variant。这意味着 webhook 子系统在事件扩展性上有一个硬编码瓶颈：每次加新事件类型，需要改这一处。这违反了 §4.5 "at-least-once 状态机"的规范——webhook 重试/死信逻辑已实现，但消费端覆盖不全。

**P2 债务（可规划到下季度）：**

4. **`AERO__` 配置命名空间不一致。** AGENTS.md §4.3 指出双下划线 `AERO__SECTION__KEY` vs 单下划线 `AERO_RATE_LIMIT_PER_SEC` 两种格式共存。这不是致命问题，但运维人员需要记忆两份约定。建议统一迁移（向后兼容）。

5. **Webhook `secret` 列明文存储。** 虽然 HMAC 签名工具链（`crypto.rs`）存在，但 DB 层没有加密。这是合规审计（SOC2/HIPAA）时会标记的问题。`pgp_sym_encrypt` 或 `AEAD` 加密的成本很低，建议优先处理。

6. **数据库迁移膨胀（157 个且持续增长）。** 每个功能对应 1-2 个迁移文件是合理的，但缺乏迁移合并策略。当达到 ~200 个迁移时，`sqlx::migrate!` 在 CI 中的执行时间会开始影响开发体验。

---

## 2. 架构扩展方向

基于验证报告识别的 5 个方向 + 我补充的 2 个发现，以下按业务价值排序：

### 方向一：通知路由与渠道管理引擎

**为什么需要：** 当前通知系统有碎片化的基础设施（`keyword_alerts`、`channel_mutes`、`dnd_settings`、`snooze_until`、`notification_importance`、`notification_bundles`、`thread_notification_prefs`、`workspace_notification_defaults`——8 个独立迁移文件）但没有统一的编排层。这意味着：
- 每次加新通知事件类型需要改 N 个分散的表和路由
- 无法实现 "工作区管理员指定通知渠道优先级：push > email > in_app > digest" 
- 缺乏时区感知（`dnd_settings` 的 `start_minute`/`end_minute` 是 UTC 分钟，非用户时区）
- 无法做通知预算（用户每天最多收 N 条 push）

**核心挑战：**
- 现有 8 个表的迁移数据结构需要**向前兼容地**统一到一个 `notification_policies` 聚合根下（迁移模式：新建表写双写，旧表保留读兼容，逐步弃用）
- 优先级继承链（工作区 → 频道 → 线程 → 事件类型）的查询性能——每次通知触发时可能需要 4 次 DB 查询 + 合并
- push token 的管理（`push_tokens` 表 + `FakeGateway`）和 email 发送基础设施缺失

**预期的架构变更：**
1. 引入 `NotificationEngine` 聚合服务（在 `aero-im-core` 或新 `aero-notify` crate），封装所有通知决策逻辑
2. `run_bus_listener` 的 `Notify`/`NotifyBatch` handler 从直接扇出改为经过 `NotificationEngine` 路由
3. 后端 channel adapter trait：`trait NotificationChannel { fn send(&self, recipient, payload); }`，实现 `PushChannel`、`EmailChannel`、`InAppChannel`、`DigestChannel`
4. 异步 digest 构建器：每用户每日 digest 批处理（PostgreSQL `FOR UPDATE SKIP LOCKED` + cron timer，复用 AiWorker 模式）

**现有影响：** 低。`Hub::fan_out_raw` 是扇出点，在其前插入 `NotificationEngine` 不影响 WebSocket 实时路径。digest 是新增业务逻辑，不影响现有代码。

### 方向二：计费与用量计量骨架

**为什么需要：** 当前系统有 `ws_rate.rs` 的 `rate_tier`（standard/premium/unlimited）骨架和 Redis 计数器模式，但没有计费管道：

- **业务价值：** 没有计费就无法商业化。`rate_tier` 的存在说明团队已经预想过多层定价，但卡在支付集成上
- **技术价值：** 用量计量基础设施（配额检查、预算跟踪、阈值告警）是跨功能重用的——AI 预算（`AiWorker` 的 `CostBudget`）已展示了这个模式，可以统一

**核心挑战：**
- **Stripe/Paddle 集成不是技术问题，是合规问题。** PCI DSS、税务发票、退款仲裁——这些是纯工程无法解决的操作复杂度。不要低估支付集成的持续维护成本（webhook 端点、签名验证、重放保护、发票生成）
- **用量计量准确性**：at-least-once 事件投递意味着直接按事件计数会重复。需要幂等计数器或 dedup window（`AiWorker` 已用 `ON CONFLICT DO NOTHING` + 仅成功记成本，这是正确的模式）
- **配额实施点散布在各处**：AI 预算在 `AiWorker`，API 限流在 `ws_rate.rs`，存储配额（预计）在 `BlobStore`。需要统一配额检查点或配额中间件

**预期的架构变更：**
1. Phase A（配额骨架，4-6 周）：`UsageMeter` trait + Redis `INCR + EXPIRE` 实现 + SQL 用量快照表，复用 `WsRateStore` 的 Redis 计数器模式
2. Phase B（支付集成，8-12 周）：`BillingProvider` trait（`subscribe`/`cancel`/`invoice`/`webhook`）→ Stripe 实现 → `subscriptions` 表
3. 配额实施点模式：`trait QuotaCheck { fn check_quota(&self, user, resource) -> Result<(), QuotaExceeded>; }`，在各检查点注入

**现有影响：** 中。Redis 计数器读路径已存在（`ws_rate.rs`），但需要将配额决策点集成到现有请求路径中。`AiWorker` 的预算逻辑需要迁移到新的统一 `UsageMeter`。

### 方向三：事件目录与集成框架（Webhook/Event Bridge）

**为什么需要：** 当前 webhook 只投递 `RoomEvent::Message`，且没有出站事件目录供集成开发者发现可用事件。**这是平台化的瓶颈**——没有事件目录，外部开发者不知道能订阅什么；没有 connector 框架，SaaS 集成（Slack、Discord、Zapier）需要手写每个集成。

**核心挑战：**
- **事件目录的 schema 治理**：15+ 种 `RoomEvent` variant 没有一个 JSON Schema 或 Protobuf 定义。`serde(tag="kind")` 是 Rust 枚举序列化，不是外部可消费的契约。需要输出 OpenAPI 风格的 `EventSpec`（名称、payload schema、触发条件、速率）
- **出站 webhook 的可靠性**：现有 `run_webhook_dispatcher` 只有 `Message` 单臂。扩展为全事件类型 + 重试/死信/幂等（`Idempotency-Key`）
- **OAuth 授权流缺失**：AGENTS.md 明确 "`oauth/OAuth/oauth2` 零匹配"。第三方集成需要 OAuth 2.0 授权 + token 刷新 + scope 管理——这是工程量最大的部分

**预期的架构变更：**
1. `EventRegistry` 模块：在 `aero-common` 中用宏或构建脚本输出事件 schema（`event_registry!` macro → JSON 文件）
2. `run_webhook_dispatcher` 重构：从 `matches!` 单臂 → 全事件 `match` + `event_filters` JSONB 过滤（复用已有 `webhook.event_filters` 列）
3. `connector` crate（新）：`trait Connector { fn handle_event(&self, event, context); fn register_webhooks(&self) -> Vec<WebhookDef>; }`，为 Slack/Discord/Zapier 提供 seam
4. 速率控制层：每集成 `rate_limiter`，防第三方 API 背压回流到主路径

**现有影响：** 中-低。`EventRegistry` 是新增模块，不影响既有事件流。webhook dispatch 的改造是向前兼容的（加 variant 不改现有 `Message` 投递）。

### 方向四：合规审计与管理员控制台

**为什么需要：** 当前无 `is_super_admin`/`superadmin` 概念，无 `admin_audit_log` 表。这意味着：
- 超级管理员操作（数据导出、消息删除恢复、用户账户操作）没有审计追踪
- 没有"最小特权原则"的全局管理角色分层（auditor vs operator vs admin）
- 法务保全（`legal_holds` 表已存在）和管理操作之间有间隙——谁在何时解除了法务保全无法追溯

**核心挑战：**
- **超级管理员权限的滥用防范**：AGENTS.md §4 指出当前所有路由需要租户 ID，无全局管理路由。引入超级管理员角色后，必须防止逃离租户隔离的"后门"（IDOR）
- **审计日志的完整性**：append-only 表 + 防止日志篡改（若 DB 被入侵，攻击者可以 DELETE FROM admin_audit_log）。建议：日志写入独立的审计数据库或 `pg_audit` 扩展，或使用 `SERIALIZABLE` 隔离级别 + 跨行哈希链
- **合规搜索的范围边界**：超级管理员能看到所有工作区的消息。这需要在 pgvector 搜索中加入全局索引（当前搜索是按房间/工作区 scope 的）

**预期的架构变更：**
1. `admin_users` 表（与 `users` 分离，非 `is_super_admin` 布尔列） + `admin_roles` 表 + `admin_audit_log` append-only 表
2. `AdminMiddleware`（Axum middleware）：在全局路由树之前注入，为所有 `/admin/*` 路由提供鉴权 + 审计
3. 合规搜索路径：`AdminSearchService` 包装 `ImService::search` 但禁用工作区限制，添加 `before:`/`after:`/`user:` 操作符（复用已有 search_advanced 的 `from:`/`in:` 文法）
4. 审计日志完整性：建议用 **HMAC 链**（每行包含上一行的 HMAC，密钥由 Vault/HashiCorp 管理），而非强行引入区块链

**现有影响：** 高（模式变更）。需要在现有中间件栈中插入超级管理员检查点。`assert_room_access` 守卫是硬编码的——需要修改为"如果是超级管理员，允许跨越"但不影响租户隔离审计。CI 的 `authz_lint` 需要更新以识别 `is_admin()` 守卫。

### 方向五：高可用与区域性部署

**补充方向**（验证报告未覆盖，但实际代码暴露了需求）：

**为什么需要：** NATS JetStream 的 durable consumer (`aero-server`) 在跨区域部署时无法很好地工作——NATS 集群的跨 AZ/Region 延迟会阻塞消息确认。当前 `run_bus_listener` 是同步 ack 模式：收到消息 → 处理 → ack。当实例在 us-east-1 但 NATS 主集群在 us-west-2 时，RTT 延迟会堆叠。Redis sorted-set 做 presence 也会有类似问题（跨区域写入延迟 > 每次心跳间隔）。

**核心挑战：**
- 跨区域事件分发：NATS JetStream 不支持跨集群镜像（这是 NATS Enterprise 的功能）。替代方案：每个区域独立 NATS + 区域间桥接（pump 模型）
- 会话亲缘性：WebSocket 连接绑定到单实例。用户跨区域漫游需要重新连接 + presence 状态重同步
- 存储层：Postgres 逻辑复制 + 应用层读写分离（`sqlx` 已经支持 `PgPoolOptions` 多池配置，但当前所有查询走单一池）

**预期的架构变更：**
1. 区域感知的 bus listener：每个区域的 `run_bus_listener` 只订阅该区域 subject（`im.room.{region}.{id}`），区域间事件通过 NATS 桥接泵转发
2. `Hub` 支持区域广播：`fan_out_raw` 可以标记事件是否需要跨区域投递
3. 读写分离：`StorageRepo` 构造函数接受 `(writer: PgPool, reader: PgPool)`，查询走 reader pool，写入走 writer pool

**现有影响：** 低。当前所有区域是单集群部署，迁移到多区域是**net-new**，不会破坏现有单区域功能。NATS subject 命名空间已用 `im.room.*` 和 `live.stream.*`——加区域前缀不影响现有消费者。

---

## 3. 接口设计原则

### 核心原则

当前代码的接口风格整体良好——`routes()` → `Router<AppState>` + `XRepo::new(s.pg.clone())` 的模式简单直接。以下原则应在扩展中保持：

1. **仓储（Repo）保持纯 CRUD。** 当前各 `XRepo` 的职责干净（`storage/src/x.rs` 只做 DB 操作）。不要往 Repo 里塞业务逻辑。`ImService` 层（`aero-im-core`）做编排，`Hub` 做扇出。这个三层分离（Repo → Service → Transport）是好的。

2. **新增功能：新 crate，非新模块。** 153 个 `aero-server/src/` 模块已证明单 crate 膨胀是有代价的。计费、通知引擎、connector 框架都应作为新 crate（`aero-billing`、`aero-notify`、`aero-connector`）开始，而非在 `aero-server` 下加 `billing.rs` + `billing_admin.rs` + `billing_repo.rs`。

3. **Trait 定义靠近使用方，而非生产方。** 当前 `EventBus` trait 在 `aero-bus` 中定义（靠近 NATS 实现）——这是对的。新抽象如 `NotificationChannel`、`BillingProvider`、`Connector` 也应如此：定义在消费方 crate（`aero-notify`/`aero-billing`/`aero-connector`），而非 `aero-common`。

4. **避免过度抽象的"trait 农场"。** 当前代码务实：`XRepo::new(s.pg.clone())` 直接创建实例，没有 RepoFactory trait 或 DI 容器。在扩展到计费/通知/connector 时，应保持这种"显式构造"风格。只在需要**替换实现**（如本地 BlobStore vs S3BlobStore、FakeGateway vs 真实 FCM）时才定义 trait。不要为单一实现引入 trait。

### 是否需要新的抽象层

**需要：用量计量层（Meter）。** 当前 AI 预算、API 限流、消息计数散布在三个独立模块中，各自实现各自的 Redis `INCR`。引入一个统一的 `UsageMeter` trait（`count()`、`check()`、`report()`）可以将计费、限流、用量审计统一到一个视窗下。

```rust
// 建议的接口（纯示意，非实现代码）
trait UsageMeter {
    /// Increment counter. Returns current count.
    async fn count(&self, key: &MeterKey, amount: u64) -> Result<u64>;
    /// Check if key has remaining budget.
    async fn check(&self, key: &MeterKey) -> Result<QuotaStatus>;
    /// Snapshot current counters for billing.
    async fn snapshot(&self, since: Instant) -> Result<Vec<UsageRecord>>;
}
```

**不需要：通用 Job/Task 抽象。** 当前 AiWorker（SKIP LOCKED 轮询）和 Bot 体系（NATS consumer）用了不同的调度模型。强行统一为"作业队列"抽象会增加复杂度（需要解决：投递延迟 vs 长轮询、消息大小限制 vs 大 payload、重试语义差异）。让 AiWorker 留在 SKIP LOCKED 模式，Bot 留在 NATS durable consumer 模式——两个调度器是正常的架构多样性。

### 向后兼容性

当前 schema 没有 API 版本号（所有路由是 `/api/rooms/:id`，无 `/v2/`）。在引入计费/通知/connector 时：

1. **新功能用新路由路径。** 计费路由 `/api/billing/`、通知偏好 `/api/notifications/preferences`——这些都是新路由，不修改现有路径。不需要版本前缀。
2. **现有字段用 `Option` + 默认值扩展。** 新 `subscriptions` 表的 `plan_id` 默认 null 表示免费层（如 `ws_rate` 的 `rate_tier` 默认 `standard`）。不修改现有 `users` 表加 NOT NULL 列。
3. **WebSocket 帧加 `version` 字段。** 当前 `ClientFrame`/`ServerFrame` 没有版本号。建议在帧头加 `ver: 1` 字段，默认 1。客户端可协商版本。新事件类型（如 `call_*`）加在 `kind` 枚举的尾部——反序列化时未知 variant 静默跳过（当前 serde 已经是 `deny_unknown_fields` 否？需要确认并放宽）。

---

## 4. 技术选型

### 是否需要引入新技术栈

| 方向 | 建议 | 理由 |
|------|------|------|
| 计费 | **复用 Stripe 现有集成模式**，无需新框架 | Stripe SDK（`async-stripe` crate）成熟度高，Rust 生态原生支持。Paddle 也有 Rust SDK。不要在支付集成上自建抽象 |
| 通知推送 | **保持 FCM/APNs 直连，不加推送中间件** | 当前 `aero-push` 的 `FakeGateway` + `trait PushProvider` 模式足够。Firebase Cloud Messaging HTTP v1 API 可直接 curl 直发，无需引入 Firestore/Cloud Messaging 厚重 SDK |
| 前端 | **渐进式引入轻量框架** | 当前零依赖 ES2020 在 153 模块后端面前已显吃力。建议不引入 React（太重），考虑 **Preact**（~3KB）或 **Lit**（Web Components 原生）用于模板/书签/草稿等 UI 组件。非强制重构——可以混合：现有页面保持 ES2020，新组件用 Preact/Lit 增量构建 |
| 审计日志 | **pg_audit 或应用层 HMAC 链** | pg_audit（PostgreSQL 扩展）提供 DDL/DML 审计，但不防篡改。应用层 HMAC 链（每行包含上一行哈希）在防篡改和性能之间平衡更好。不推荐区块链（无意义）或专用审计数据库（运维负担） |
| OAuth 集成 | **axum-oauth2 或 oauth2 crate** | 当方向三（connector）需要 OAuth 授权时，`oauth2` crate（Rust 原生）是标准选择。Axum 生态的 `axum-oauth2` 提供 session/token 管理。注意 OAuth 的 scope 设计需要一次做对——后期加 scope 需要用户重新授权 |
| 事件 schema | **JSON Schema，非 Protobuf** | Protobuf 的好处（强类型、向前兼容）在 15+ variant 规模下不足以抵消其代码生成税。JSON Schema + `schemars` crate（Rust 原生，从类型派生成 schema）可以在不增依赖的情况下输出事件目录 |

### 第三方依赖评估标准

现有 `deny.toml` 和 CI 门槛已经包含 license 扫描。建议补充：

1. **"为什么是这个 crate" 文档化。** 每个依赖在 `Cargo.toml` 中加注释说明替代品和选择理由（类似 Bazel 的 `why = "..."`）。例如，当前 `fred` 9（Redis）替代品 `redis-rs`（tokio 生态更好但缺少 cluster 支持）。`async-nats` vs `nats.rs` vs `nats-aflowt`。

2. **生态活性评估标准：**
   - 最近 6 个月有发布
   - 至少 2 位维护者
   - `cargo audit` 无未修复的安全公告
   - 与 MSRV 1.80 兼容

3. **依赖来源分级：**
   - Tier 1（核心）：`axum`、`sqlx`、`fred`、`async-nats`、`serde`——这些是骨架依赖，替换成本极高。maintainer 变更 = 风险评估
   - Tier 2（业务）：`str0m`、`rml_rtmp`、`schemars`——领域特定，可替换
   - Tier 3（工具）：`tokio-util`、`tracing`、`thiserror`——低成本替换

### 自建 vs 采购

| 场景 | 建议 | 理由 |
|------|------|------|
| 支付处理器集成 | **采购（Stripe/Paddle/Lemon Squeezy）** | 合规审计、税务、退款仲裁的工程成本远超 Stripe 的手续费。切勿自建支付 |
| 邮件发送 | **采购（SendGrid/Resend/Postmark）** | 邮件送达率、SPF/DKIM/DMARC 配置、退信处理是专业领域。自建 SMTP 在发送量 > 1000/天时不可行 |
| 实时 WebRTC SFU | **自建（已有 str0m）** | 当前 SFU 方案已经投入工程资源。替换为 LiveKit（Go）/Janus（C）会引入 FFI 依赖和运维复杂度。自建的长期成本在 RTCP/congestion-control 调优 |
| AI 嵌入/推理 | **采购 API + 自建 HashEmbedder 兜底** | 当前 Hybrid（Voyage API + HashEmbedder fallback）模式正确。少量嵌入调用用 API，规模后自建 ONNX/BERT 推理 |
| 推流 CDN | **采购（Cloudflare Stream / Mux / Fastly）** | HLS 分发需要全球边缘网络。自建 CDN 的边缘缓存是不可行的 |

---

## 5. 实施路线图

### 优先级排序

| 优先级 | 方向 | 理由 |
|--------|------|------|
| **P0** | 计费骨架（Phase A） | 拦路虎——没有计费，其他所有方向都无法产生收入。当前 `rate_tier` 已暗示商业变现的需求。Phase A（配额计量 + Redis 计数器）可以 4 周内完成，不依赖支付集成 |
| **P0** | Dark Features UI 补全（模板/书签/草稿/收藏/提醒） | ROI 最高的工作——后端代码已写、已测试、已迁移，只差浏览器 handler。每个功能的 UI 集成成本估计：2-5 天。5 个功能共 3-4 周，即可解锁 25% 的已有代码价值 |
| **P1** | 通知路由引擎 | 用户体验的最大痛点。8 个碎片化的通知表 → 统一的 `NotificationEngine`。依赖计费的 `UsageMeter`（通知预算需要配额检查），所以排 P1 |
| **P1** | 事件目录 + Webhook 全事件 | 平台化的第一步。当前 webhook 只投递 `Message`——外部开发者无法集成其他事件。不依赖其他方向，可以并行推进 |
| **P2** | 合规审计与管理控制台 | 合规要求通常是客户驱动的（SOC2、企业合同条款）。在达到一定规模前可以推迟。依赖超级管理员角色引入（影响 `assert_room_access` 守卫） |
| **P2** | Connector 框架（Slack/Discord/Zapier） | 需要事件目录完成（方向三）、OAuth 授权就位、webhook 全事件。依赖链最长 |
| **P3** | 区域部署 / 高可用 | 当前单区域架构至少支持到 10 万 DAU。到达该规模前不需要投资 |

### 阶段划分

```
Q3 2026（现在 - 9月）  Phase 1: 基础设施 + 高 ROI 补全
├── 计费 Phase A: UsageMeter + Redis 计数器 + ws_rate 复用
├── Dark Features: 5 个后端的 web UI 补全
├── 技术债清理: webhook 全事件、secret 加密、模块目录拆分
└── 里程碑: 所有后端的端到端可访问
    验收标准: 模板/书签/草稿在浏览器中可创建、编辑、删除

Q4 2026（10月 - 12月）  Phase 2: 通知 + 事件平台
├── NotificationEngine（aero-notify crate）
├── 通知渠道: push（已有）、email（新增 SendGrid 集成）、digest
├── EventRegistry（JSON Schema 输出）
├── Webhook 全事件 dispatch
└── 里程碑: 用户可配置 per-event-type 通知偏好
    验收标准: 创建消息 → webhook 收到全 payload、用户可配置静音/推送/邮件

Q1 2027（1月 - 3月）  Phase 3: 商业化 + 集成
├── 计费 Phase B: Stripe 集成 + subscriptions 表 + 配额挂载
├── OAuth 2.0 授权框架
├── Connector 骨架（Slack 双向 bridge MVP）
├── 合规审计: 超级管理员 + admin_audit_log
└── 里程碑: 付费订阅可下单
    验收标准: Stripe checkout → webhook → subscription 激活 → premium 配额生效

Q2 2027（4月 - 6月）  Phase 4: 规模 + 治理
├── 区域部署骨架（多 AZ NATS bridge）
├── 读写分离 PgPool
├── 审计日志 HMAC 链 + 合规搜索
├── 迁移合并策略（超 200 个迁移后）
└── 里程碑: 多地实例可互通
    验收标准: 两个区域的实例加入同一房间，消息实时同步
```

### 风险点和缓解策略

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| **计费集成工程低估**（PCI DSS、税务、退款） | 高 | 高 | Phase A/B 分离：配额计量与支付解耦。Phase A 可在无支付情况下发布上线（仅计量不收费），降低 Phase B 的时间压力 |
| **153 模块拆分破坏 git blame** | 中 | 中 | 模块拆分时必须**保持文件内容不变**（仅移动 + `mod.rs` 收口），用 `git mv` 迁移。拆分后立即提交，不在同一 PR 中加新功能 |
| **前端框架引入导致 API 前后端分离不兼容** | 中 | 高 | 规则：前端框架不影响接口协议。`ServerFrame` 和 `ClientFrame` 的 JSON 形状不变。前端框架只影响 render 层——WS handler 无需改动 |
| **超级管理员 IDOR 漏洞** | 低 | 极高 | CI 的 `authz_lint` 在引入超级管理员角色时必须升级。审核路径：`is_admin()` + `admin_audit_log` insert 必须是**同一事务**。每次管理员操作写审计行后才执行操作 |
| **NATS 跨区域延迟** | 低 | 中 | Phase 4 前不需要投资。到达前，保持单 NATS 集群 + 单区域部署。跨区域计划开始时，先用 NATS leaf node 而非 full replication |
| **str0m 生态失速** | 低 | 中 | str0m 0.19 的生产准备度需要季度评估。预警信号：超过 12 个月无发布、核心维护者离职。替代方案：预留 LiveKit 集成路径（CGo 兼容） |

### 关键决策点

1. **立即 - 6 周内：Webhook 事件扩展。** 当前 `run_webhook_dispatcher` 的单臂 `matches!` 是未来所有事件集成（计费通知、合规告警、connector）的瓶颈。这是改动最小（改 1 个 match arm 为全覆盖）但解锁最多的重构。

2. **Phase 1 结束前：decide crates.io publishing policy。** 当前 16 个 crate 全标记 `publish = false`。如果考虑开源或组织间复用，需要决定 `aero-common`/`aero-bus`/`aero-storage` 是否发布到 crates.io。这影响模块拆分时的 `[dependencies]` 声明方式（path 依赖 vs version 依赖）。

3. **Phase 2 结束前：前端技术选型。** 当前零依赖 ES2020 是否继续？如果需要模板/书签等 UI 有交互复杂度（拖拽、自动完成、弹出日历），建议在 Phase 2 前引入 Preact（3KB，渐进式，不影响现有 JS 文件）。

4. **Q1 2027 前：数据库迁移策略决定。** 157 个迁移在 Phase 1-4 期间可能增长到 250+。`sqlx::migrate!("../../migrations")` 的编译时间会增长。决定：A) 定期压缩迁移（`migrate merge` 工具），B) 改用 `sqlx::migrate!` 的 `Migrator::new` + 运行时加载，C) 保持现状——200 迁移 ~5 秒编译，可接受。

---

**总结：** Aero IM 的架构基础坚实——事件驱动骨架、crate 隔离、持久化策略都是正确的选择。当前最大的架构债不是技术选型问题，而是**功能集成不一致**（后端完备但前端缺失）和**模块组织膨胀**（153 个平铺模块）。修复这两个问题后，计费骨架、通知引擎、事件目录三个方向可以并行推进。谨慎选择 Phase 顺序（先计量后支付、先通知后集成、先事件目录后 connector），避免过度工程化 OAuth 和 Ops 基础设施在规模验证前。

# 架构分析报告：Aero IM 交叉验证评审

## 1. 架构评估

### 1.1 核心架构优势

**事件驱动骨架 + 进程内扇出** 是当前架构最正确的决策。三个证据支持这一判断：

1. **NATS JetStream 作为跨实例事实源**：`im.room.*` durable consumer + `live.stream.*` ephemeral consumer 的两级隔离，保证了 IM 消息 at-least-once 交付 与 直播弹幕可丢扇出 的差异化 QoS。这是单体架构做不到的韧性。

2. **Hub 的 bounded mpsc 背压模型**：`WsConfig::send_queue_capacity`（默认 256）绑定每连接扇出队列 + `disconnect_on_full` 断连策略 + `lossy` AtomicBool 的丢帧→resync 协议——这是一套**严谨的 OOM 守卫**。多数 IM 系统在这里要么无界（内存泄漏），要么阻塞扇出（级联延迟）。Aero 的选择是正确且激进的。

3. **256 路 Redis sorted-set 分片**：`presence`、`StreamViewerStore`、`CallRosterStore` 全以 participant ID 低字节分片，避免大房间的热键争用。这是已知大流量场景的前瞻设计——多数系统在遇到瓶颈后才改。

### 1.2 架构债务与技术债

**债务一：NATS JetStream 2MB 消息体上限无保护**

`publish_room_event`（`aero-im-core/src/service/events.rs:68`）直接将整个 `RoomEvent`（含 `Vec<Block>` 序列化）发布到 `im.room.*`。当消息包含大文件附件（尤其是 Base64 内嵌图片）时，async-nats 默认 1MB 或 JetStream 的 2MB 限制会静默失败。当前代码路径：

- 无前置体量检查 → 失败后 `BusError::Nats` 被上层 warn log（`events.rs:91` 的 `warn!(error = %e, "failed to publish")`）
- 无 fallback 策略（分块发布 / 引用发布 / 压缩）

**影响**：这不仅是可靠性问题——它意味着「发大图」在特定场景下静默丢事件。如果客户端不期待服务器响应（`send_message` 的 `expect_ack` 模式），发送者看到成功但其他成员收不到。

**债务二：REST API 无限流**

`routes.rs` 的中间件栈中，`rate_limit` 层（`msg:rate_limit`）只覆盖 WS 消息路径。`/api/` 下的全部 REST handler **默认无全局限流**。当前代码中：

- `rate_limit.rs` 实现了 per-client token bucket（`LEAKY_BUCKET` + 可配的 `AERO_RATE_LIMIT_PER_SEC`）
- 但该中间件只通过 `r0.layer(rate_limit)` 绑在 WS 路由上

这意味着攻击者可以通过 `POST /api/rooms/:id/messages` 直接批量发消息绕过 WS 限流。这也不是「缺失」——是有中间件但未绑到正确位置。

**债务三：PG 无 WAL 归档**

`docker-compose.yml` 中的 PG 配置是 pure development：无 `archive_mode=on`、无 `archive_command`、无流复制。这意味着：

- `docker compose down -v` 确实丢所有数据（volumes 被删）
- 即使有持久卷，也没有 PITR 能力
- 当前的「容灾」完全依赖 pg_dump（如果有定时任务的话），RPO 在小时级

**债务四：webhook 幂等协议未定义**

`webhooks` 模块已实现回调投递 + DLQ + requeue，但没有标准化的 `Idempotency-Key` / `event_id` 协议。消费者收到重试时无法区分「新事件 vs 重试」。这是**集成面 API 契约缺失**，不是代码缺失——影响与第三方集成的可靠性。

---

## 2. 扩展方向

### 方向一：事件体量守卫与压缩层

**为什么需要**：AGENTS.md §4.5 明确提到了已知瓶颈——NATS JetStream 的消息体上限。但整个系统（从 `publish_room_event` 到 `fan_out_raw`）对超大 payload 完全没有防御。用户上传 4MB 图片 → `Block::File` 引用 blob_id（小）→ 但富文本消息内嵌多个大 `Block::Card` / `Block::ToolCall.result` 仍可能超限。

**核心挑战**：
1. **在哪里截断**：生产者侧（`publish_room_event`）vs 消费者侧（`run_bus_listener`）。生产者侧更安全——否则坏消息已经进 JetStream，清理成本高。
2. **截断后的降级策略**：Drop（丢事件）vs Truncate（截断后发布大 Block）vs Offload（将大 Block 替换为引用 ID，消费者按需拉取）。三者各有适用场景。
3. **兼容性**：现有 `RoomEvent` 变体（`Message`/`Edited`/`Deleted` 等）的 consumer 需要能处理「被压缩」的事件。

**预期的架构变更**：

```
publish_room_event
  │
  ├─ payload ≤ 512KB ──────────────────→ publish raw
  │
  └─ payload > 512KB ──→ 检测超大 Block
         │
         ├─ Block::File ──→ 跳过（已 ID 引用，体量小）
         ├─ Block::Card with large payload
         ├─ Block::ToolCall with large result
         └─ Block::Text > 100KB
              │
              └─ 在 JSON 序列化前将超大 Block 替换为
                 placeholder { type: "oversized", ref: "<blob_id>" }
                 原 Block 另存为 blob_store + 异步清理
```

**对现有系统的影响**：
- `ImService::publish_room_event` 新增 `payload_size` 检查分支
- 新增 `Block::Oversized { ref: BlobId }` variant（客户端需要新增渲染 fallback：「此内容过大，点击下载」）
- web 端需要响应式处理新 Block type
- 零迁移影响（表结构不变）

---

### 方向二：信任与滥用防御引擎（T&S Platform）

**为什么需要**：交叉验证报告指出，当前 T&S 功能碎片化——`moderation_bot`（关键词 + AI 审核）→ `audit_events` 表 → 人工操作面板，**但缺乏统一的风险事件总线**。一个用户注册→发骚扰消息→被举报→被 ban 的闭环，中间需要串联 4 个模块，没有中心化的风险评分。

**核心挑战**：
1. **身份锚点选择**：trust score 绑定到 `participant_id`（易绕——换号重来）vs `(ip_hash, device_fingerprint)` 簇。后者的工程复杂度高一个数量级——需要保存指纹历史 + 处理 NAT 共享 IP（企业客户所有员工同一公网 IP）。
2. **评分反馈延迟**：内容审核是异步的（`moderation_bot` 有界 mpsc + AiWorker），trust score 更新需要串在审核结果回调中。如果审核队列积压，恶意用户在扣分生效前可以继续作恶。
3. **阈值策略的可配置性**：企业客户要求不同的敏感度。SaaS/合规松 vs 金融/合规严。当前是硬编码 env `AERO_BLOCKED_WORDS`。

**预期的架构变更**：

```
注册 ─→ registrant_email_domain check (blocked_domains + enterprise_auto_approve)
  │
  ├─ blocked ─→ reject + audit_event
  │
  └─ allowed ─→ participant_trust_scores 表插入初始分 (100)
                  │
        ┌─────────┼─────────┐
        │         │         │
    content_flag 举报被确认   spam_detected
        │         │         │
        └─────────┼─────────┘
                  │
           score -= weight(event)
                  │
            score < threshold?
                  │
          ┌───────┴───────┐
          │               │
      rate_limit_escalate  silent_ban + 审核工单
```

**对现有系统的影响**：
- 新表：`participant_trust_scores`、`email_domain_reputation`、`risk_events`（统一风险事件流）
- `moderation_bot` 产出需回写 trust score
- `audit_events` 表（migration `0150_audit_events.sql`）需验证是否能接入风险事件流闭环
- 注册流程新增 domain check hook（轻量，HTTP GET to 预配置的 deny list 或 Redis 缓存）
- WS 速率限制层需要读 trust score 来动态调整 bucket size
- **迁移量**：~5 个新模块，但可 P0→P2 分三阶段推

---

### 方向三：事务性邮件管线

**为什么需要**：交叉验证确认这是**真正未被覆盖的方向**。当前系统的通知仅依赖：
- WebSocket 实时（在线用户）
- 移动推送（`push_bot`，需 FCM/APNs 配置 + 设备注册）
**完全没有邮件 fallback**。这意味着：
- 离线用户收不到密码重置、邀请、摘要、账单通知
- 企业合规需要邮件审计日志（「谁在何时批准了什么」）
- 密码重置/2FA 恢复码只能通过 WS（用户已登出时死锁）

**核心挑战**：
1. **邮件模板安全（SSTI）**：邀请邮件的 `workspace_name` 是用户可控字符串。如果在 worker 阶段渲染模板，SSTI 攻击面开放。正确的设计是：写入 `email_jobs` 队列时**预渲染** `body_html`/`body_text`，worker 只负责发送。
2. **退信处理的安全验证**：SES/SendGrid/Mailgun 的 webhook 必须验证 HMAC 签名，否则攻击者可伪造退信禁用企业邮箱通知。
3. **i18n 前置依赖**：邮件模板的多语言是硬依赖——如果先做邮件管线再做 i18n，模板文本需要全部返工。
4. **速率限制**：事务性邮件不需要像营销邮件那样的节流，但仍需 per-workspace 出站速率（防止单个工作区触发批量邀请导致被 SES 标记为垃圾发送方）。

**预期的架构变更**：

```
        ImService 或定时器
              │
         enqueue_email(kind, recipient, params)
              │
              ▼
        email_jobs 表 (kind, to, body_html, body_text, state, attempts, workspace_id)
              │
              │  worker: 轮询 FOR UPDATE SKIP LOCKED
              ▼
        EmailSender trait ──→ SmtpSender / SesSender / SendGridSender
              │
              ├─ 成功 → ack + delete job
              ├─ 硬退信 → mark_bounced + 更新 participant.email_bounced
              ├─ 软退信 → backoff + 指数退避
              └─ 超时 → 留队列重试 (max 5)
```

**对现有系统的影响**：
- 新 crate `aero-email`（或归入 `aero-push` 扩展）
- 新表 `email_jobs`、`email_templates`、`participant.email_bounced` 列
- `ImService` 的 invite/missed-call/password-reset 路径需 `enqueue_email` 调用
- `push_bot` 的离线通知可并行走邮件（WebSocket off → push → email fallback 链）
- web 端需要「通知偏好」页面新增邮件通知开关
- **风险**：邮件基础设施（SMTP 凭据 / SES 域验证 / 发信 IP 预热）是运维前置条件，代码做了但没过 env gate 会静默失败

---

### 方向四：开发者平台统一化（增量提炼）

**为什么需要**：交叉验证指出这篇分析与 `2026-06-29-codebase-analysis.md` 方向二高度冗余。我只提炼**确实缺失**的增量：

1. **Webhook 幂等事件 ID 协议**：当前 webhook 投递不带 `X-Event-Id` 头。第三方收到重试无法去重。需要 `webhook_deliveries` 表新增 `event_id`（UUID v7）+ 投递时设 `X-Event-Id` + `Idempotency-Key` 头。
2. **速率限制披露头**：REST API 无 `X-RateLimit-*` 头。即使全局限流还没开，API 响应中带 `X-RateLimit-Limit` / `X-RateLimit-Remaining` / `X-RateLimit-Reset` 也是 SDK 开发的必备契约接口（即使当前值是「无限制」）。

**这两个不需要新 crate 或框架变更**——是 `rate_limit.rs` 中间件 + `webhooks.rs` 的产出格式变更。

**核心挑战**：REST 限流默认关闭意味着插入全局限流层后可能破坏现有 API 消费者（超出预期的 429）。需要先加披露头（告知现状），再逐步加限流（grace period 模式：`X-RateLimit-Limit` 头明确告知阈值，如果超限只 warn log 不真正拒绝）。

**对现有系统的影响**：最小。中间件变更 + 新增两个 HTTP 头的注入。

---

### 方向五：弹性与容量已知瓶颈工程化

**为什么需要**：交叉验证报告指出「已知瓶颈但未工程化」。三个已知瓶颈都有代码证据：

1. **Hub::fan_out_raw bounded mpsc 的 256 capacity**（`hub.rs`）：单连接慢导致该连接丢帧→resync，但如果是广播风暴（1K 用户同时在同一个大房间发消息），扇出循环本身是**串行**的（`for pid in recipients` 循环 → 每个 pid 顺序 try_send）。大房间（10K 成员）的 `RoomEvent::Message` 扇出延迟会线性增长。
2. **NATS JetStream 消息体上限（见方向一）**
3. **Redis zadd 风暴**：10K 用户同时断线重连时，`presence`（256 分片） + `viewer`（256 分片） + `roster`（不分片并发写入 512+ 个 sorted-set，每个的 ZADD + ZREMRANGEBYSCORE 序列化在 Redis 主线程）

此外还有第四个未引起注意的：
4. **`assert_room_access` 的缓存穿透**：每次 WS 消息和 REST 请求都调用此函数，内部串行执行「room→workspace 解析 + workspace 成员检查 + room 成员检查 + 停用门 + 强制 2FA 门」。`participant_cache` 虽然热路径缓存，但 miss 时的回源查询链很长。

**核心挑战**：
1. 大房间扇出并行化（当前是纯串行 `for pid` 循环访问 `DashMap`）
2. Redis 风暴的客户端侧节流（指数退避 + jitter 的重连间隔）
3. `assert_room_access` 的缓存命中率监控（当前无 metrics）

**预期的架构变更**：
- `fan_out_arc_inner` 的并行化：`recipients` 切 chunk（≤100）后 `tokio::spawn` 并行扇出，但持 `Hub` 的 `&self` 引用，DashMap 是 `get_mut` 所以需要串行化 per-pid 写入。正确的方案是**通道式扇出**：每个连接有自己的 mpsc receiver task，`Hub::fan_out` 只负责往每连接的 tx 塞消息，不负责处理背压。
- Redis 客户端侧重试加 jitter（当前 `fred` 配置无显式重试策略）
- `assert_room_access` 新增 cache hit/miss metrics
- 无迁移影响（纯代码变更）

---

## 3. 接口设计原则

### 3.1 EventBus trait 的演进方向

当前 `EventBus` trait：

```rust
#[async_trait]
pub trait EventBus: Send + Sync {
    async fn publish(&self, subject: &str, payload: bytes::Bytes) -> BusResult<()>;
    async fn publish_json<T: serde::Serialize + Send + Sync>(&self, ...)
    where Self: Sized;
    async fn subscribe(&self, subject: &str, durable: Option<&str>)
        -> BusResult<BoxStream<'static, Box<dyn Subscription + Send>>>;
}
```

两个问题：
1. **`publish_json` 不是 dyn-safe 的**——`ImService` 通过 `dyn BusSink`（object-safe shim）来调用。这层 shim 增加认知负担。
2. **`subscribe` 返回 `BoxStream`**——每个 consumer 需要 `Box::pin`，对消费侧 ergonomics 有损。

**建议的演进**：不在当前版本改（兼容性代价高），但在下一个大版本考虑将 `publish` 的 payload 改为 `Payload` enum：

```rust
pub enum Payload {
    Raw(bytes::Bytes),
    Json(serde_json::Value),       // 需要时可惰性序列化
    Compressed(bytes::Bytes),      // 自动 gzip 解压
    Reference(BlobId),             // >1MB 的事件通过 blob_store 引用投递
}
```

这样 `publish_room_event` 可以自动选择路径（小→Raw、中→Compressed、大→Reference），对消费者透明。

### 3.2 中间件抽象层

当前中间件配置在 `routes.rs::build()` 中内联，建议在 `routes/` 下新增 `middleware.rs` 集中管理：

```rust
// 不写具体代码，只给出设计原则

pub fn rate_limit_layer(state: &AppState) -> ...  // 全局限流（目前只有 WS）
pub fn compression_layer(state: &AppState) -> ... // 目前已有 CompressionLayer
pub fn request_id_layer() -> ...                  // 目前已有
pub fn cors_layer(config: &Config) -> ...         // 目前已有
pub fn trust_score_layer(state: &AppState) -> ... // 未来：读 trust score 动态调限流
```

目标是让 `routes.rs::build()` 变为纯路由聚合，中间件栈由 `middleware.rs` 组装。

### 3.3 保持向后兼容性

WebSocket 帧协议通过 `RoomEvent` / `StreamEvent` 的 serde tag 演进：
- 新增 variant 对新客户端可见，旧客户端 serde 静默忽略（`deny_unknown_fields` 未启用）
- **不要启用 `deny_unknown_fields`**：当前设计的核心假设是旧 consumer 可以忽略未知字段。这个假设一旦打破，所有 consumer 必须同时部署。
- 字段弃用使用 `#[serde(skip_serializing_if = "Option::is_none")]` + `#[serde(default)]`，不要直接删除。

---

## 4. 技术选型评估

### 4.1 需要引入的新技术栈

| 方向 | 推荐方案 | 替代方案 | 选择依据 |
|------|---------|---------|---------|
| 邮件发送 | `lettre` (Rust SMTP) + SES/SendGrid 作为 transport | 自建 SMTP 中继 | `lettre` 成熟度最高，支持 SMTP STARTTLS + OAuth2。SES 作为 transport 可避免处理退信/SPF/DKIM |
| 邮件模板 | `minijinja` (mitsuhiko 的 Rust Jinja2) | `tera` / `handlebars` | minijinja 零 unsafe、sandboxed 渲染（默认逃逸防 SSTI）、比 tera 更轻 |
| 事件体量守卫 | 不引入新 crate——在现有 `serde_json::to_vec` 前插 `payload_size` guard | 引入 `zstd` 压缩 | 避免增加编译时依赖。压缩层可通过通用的 `CompressionLayer` 在 HTTP 层实现 |
| 信任分数引擎 | 自建（标准 SQL + Redis） | 引入规则引擎（`rsrule`/`zen`） | T&S 的业务规则是企业特定的，自建比集成通用引擎更灵活。不存在开箱即用的 Rust 规则引擎 |

### 4.2 自建 vs 采购决策框架

| 条件 | 自建 | 采购/集成 |
|------|------|----------|
| 核心差异化能力（T&S 评分模型） | ✅ | ❌ |
| 基础设施通信（邮件发送） | ❌ | ✅（SES API/SMTP） |
| 身份验证（OIDC/SSO） | ❌ | ✅（已有 `aero-auth` 的 OIDC 实现） |
| 模板渲染（邮件） | ❌ | ✅（minijinja 集成） |

**原则**：业务逻辑（如何评分、如何限流）自建；管道能力（怎么发邮件、怎么渲染模板）集成现有 crate。

### 4.3 第三方依赖评估标准

当前 `Cargo.toml` 的 workspace lints 已设置高门槛（`unsafe_code = "forbid"`、clippy pedantic）。新增依赖应满足：

1. **活跃维护**：最近 6 个月有提交，issue 响应 < 2 周
2. **零 unsafe 或 unsafe 已审计**：与当前政策一致
3. **tokio 兼容**：async 运行时一致性
4. **Apache-2.0 / MIT 许可**（避免 GPL 传染）

---

## 5. 实施路线图

### P0（必须，当前 sprint 的已知风险）

| 任务 | 工作量估计 | 依赖 | 风险 |
|------|-----------|------|------|
| 方向一：`publish_room_event` 事件体量守卫 | 2 天 | 新增 `Block::Oversized` + web fallback 渲染 | 客户端未渲染 Oversized 时体验降级 |
| 方向五：REST API 速率披露头（`X-RateLimit-*`） | 1 天 | 无 | 低——只加头不强制执行 |
| 方向五：Hub fan_out 大房间并行化（>100 recipients chunk 并行） | 3 天 | 需确认 DashMap 并发安全模式 | 测试覆盖不足时可能引入死锁 |

### P1（高价值，下一迭代）

| 任务 | 工作量估计 | 依赖 | 风险 |
|------|-----------|------|------|
| 方向三：邮件管线 Phase A（`email_jobs` 表 + `SmtpSender` + worker） | 5 天 | `lettre` + `minijinja` 引入 | SMTP 凭据配置缺失时降级日志（不阻塞系统） |
| 方向三：密码重置 / 邀请 / 离线通知邮件模板 | 3 天 | Phase A | 模板 i18n 后续可能返工 |
| 方向二：`participant_trust_scores` 表 + 注册 email domain 检查 | 4 天 | 方向一的 moderation 审核结果回写 | domain check 的黑白名单需要初始种子数据 |
| 方向五：Redis 客户端重试 + jitter 策略 | 2 天 | 无（fred 配置变更） | 低配置风险 |

### P2（重要但可延迟）

| 任务 | 工作量估计 | 依赖 | 风险 |
|------|-----------|------|------|
| 方向二：统一风险事件总线（risk_events 表 + 事件驱动评分） | 8 天 | P1 的 trust_score 表 | 评分权重需数据校准，初始默认值可能过严/过松 |
| 方向二：IP/设备指纹簇绑定 trust score | 10 天 | 风险事件总线 | NAT 环境误伤率高，需要逃逸机制 |
| 方向三：邮件退信 webhook + HMAC 验证 | 3 天 | Phase A | SES webhook 配置是运维操作 |
| 方向三：邮件 i18n 模板 | 5 天 | 方向三 Phase A + i18n 模块 | i18n 是整个系统的大工程 |
| 方向四：Webhook 幂等 event_id 协议 | 3 天 | 无 | 需要与已接 webhook 的第三方协商升级时间窗 |
| 方向五：`assert_room_access` 缓存监控 | 2 天 | 无 | 低 |

### 阶段划分

```
Phase 1 (P0, 1 week)
  ├── 事件体量守卫（修复已知 Class 1 bug）
  ├── REST API 速率披露头（添加契约接口）
  └── Hub 大房间扇出并行化（缓解已知性能瓶颈）

Phase 2 (P1, 2 weeks)
  ├── 邮件管线 Phase A（填补最大产品空白）
  ├── 事务性邮件模板（密码重置 / 邀请 / 通知）
  ├── 信任分数表 + domain check（T&S 奠基）
  └── Redis 重试策略（弹性改进）

Phase 3 (P2, 3 weeks)
  ├── 统一风险事件总线（T&S 核心闭环）
  ├── 邮件退信 webhook
  ├── IP/设备指纹簇（T&S 进阶）
  ├── Webhook 幂等协议（开发者平台）
  └── assert_room_access 缓存监控（可观测性）
```

### 风险缓解策略

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| 事件体量守卫导致合法大消息被降级 | 中 | 中 | Phase A 只加监控（warn log + metrics），Phase B 才 active 拦截 |
| 邮件引入 SSTI 漏洞 | 低 | 高 | 队列写入时预渲染 body_html（不传用户输入给模板引擎的 `|safe` 过滤器） |
| Hub 并行化引入并发 bug | 中 | 高 | 现有 60+ 行测试先加固；先切 10% recipients 并行验证正确性再全量 |
| Trust score 权重不合理导致误杀 | 中 | 高 | 初始只记录不处罚（shadow mode），权重经过两周数据校准后生效 |

---

## 总结

Aero IM 的架构基础是扎实的——事件驱动骨架、进程内 bounded mpsc 背压、256 路 Redis 分片，都是经过深思熟虑的决策。当前最紧迫的不是加新功能，而是**工程化三个已知瓶颈**（NATS 消息体上限、REST 无全局限流、Hub 串行扇出）和**填补一个真实产品空白**（事务性邮件）。T&S 平台的统一风险闭环是中期最有价值的架构投入，但需要先通过 trust score shadow mode 积累校准数据。

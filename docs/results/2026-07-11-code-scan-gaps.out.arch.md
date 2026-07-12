# 架构师分析：Aero IM — 5 个生产级缺口

> **基于文档**: `docs/requirements/2026-07-11-codebase-scan-five-production-gaps-email-load-test-deeplink-verify-notify.md`
> **视角**: 资深架构师 | **方法**: 结合项目现有骨架（AGENTS.md + 代码结构）进行交叉验证

---

## 一、架构评估

### 1.1 当前架构的优势

这份分析报告虽然聚焦缺口，但不应掩盖 Aero IM 现有的架构优势——这些是其未来扩展的基石：

| 优势 | 对应缺口 | 原因 |
|------|---------|------|
| **严格的分层 crate 架构** | 方向二 | 16 crate 的依赖方向已收敛（自下而上无环），这为性能测试提供了天然的**隔离边界**——瓶颈可归因到单个 crate，不担心级联干扰 |
| **事件驱动骨架健壮** | 方向二、五 | NATS durable/ephemeral consumer 分离 + Hub 扇出的 bounded mpsc 模式，是经过考验的架构模式。要扩展邮件通知渠道，只需复用相同的**事件→消费者→动作**管线模式 |
| **已有 mailer.rs 抽象** | 方向一、四、五 | `mailer.rs` 虽是 MVP 级别，但 `AsyncSmtpTransport` + lettuce 的选型正确，模板引擎的引入不会导致架构重塑 |
| **notif_prefs 的扩展性** | 方向五 | 通知偏好模型已被设计为键值可扩展结构（非固定列），新增 `email_digest` 字段不会破坏现有查询 |
| **bot 模式成熟** | 方向五 | push_bot 作为通知渠道 bot 的模式可被邮件通知 bot 复用——监听总线事件、按偏好门控、异步发送 |

### 1.2 架构债务与技术债

交叉验证 AGENTS.md 和代码扫描报告，识别出以下债务：

#### 债务 A：通信渠道层缺少抽象

**严重度**: P2（中期）

当前通知的架构是**隐式渠道耦合**：

```
RoomEvent → bus → push_bot (FCM/APNs)
         ↘ bus → WebSocket (实时)
```

缺少一个 `NotificationDispatcher` 抽象层，它应该：
- 接收 `NotifyEvent`（非原始 RoomEvent）
- 查询用户偏好（`notif_prefs`）
- 按**优先级顺序**尝试多个渠道：WS → Push → Email → SMS
- 记录送达状态

这意味着新增邮件通知涉及**在 push_bot 内部加 if-else**，而不是自然扩展到一个新的 channel。如果以后再加 SMS 或 Webhook 通知，push_bot 会膨胀为一个不可维护的巨石。

**标志**: `push_bot.rs` 的职责已经偏离其命名——它应该叫 `notification_dispatcher`。当前它只做推送，但未来要承载邮件回退逻辑。

#### 债务 B：mailer.rs 的同步模型是请求路径中的定时炸弹

**严重度**: P1（紧急）

```
// mailer.rs line 12-13
// No queue. Emails are sent synchronously in the request handler.
```

`lettre` 的 `AsyncSmtpTransport` 是 `async` 的，但**不排队意味着**：
- 邮件服务器延迟 5s → HTTP 响应延迟 5s
- 批量邀请 100 人 → `tokio::spawn` 100 个并发 `send()` → 连接池耗尽
- 不可重试：失败即 `warn!` 跳过，无退信回溯

**这不是架构缺陷，是架构债务**——代码结构本身支持 `tokio::mpsc` 队列（项目其他部分广泛使用，如 AI usage ledger drain），只是未实施。

#### 债务 C：Web SPA 无架构级路由设计

**严重度**: P3（低紧急，高产品价值）

`web/app.js` 的 `showAuth()`/`showChat()` 模式在原型阶段合理，但要支撑深度链接需要**客户端状态机**。

当前 web 目录结构中无 router、无 store、无 view abstraction——这是一个**架构真空**（architecture vacuum vs. architecture debt）。不是需要重构，是**需要建造**。

### 1.3 关键设计决策评估

| 决策 | 现状验证 | 评估 |
|------|---------|------|
| NATS 作为跨实例事实源 | ✅ 方向二中的测试真空不质疑此决策 | 正确。NATS 的 at-least-once + durable cursor 为邮件通知的 exactly-once 提供了基础 |
| Hub 的 bounded mpsc 模式 | ✅ 方向二提到需要 benchmark 量化 | 正确但有盲区：bounded mpsc 保护了扇出端，但**消费端**（每个 WS 连接的 `send()`）可能阻塞 → 需要端到端背压测试 |
| `notif_prefs` 的每事件开关 | ✅ 方向五需要扩展此模式 | 正确。不要拆成多个表，保持当前的结构化 JSON/位字段 |
| mailer 用 `lettre` | ✅ 方向一需要扩展 | 正确。`lettre` 支持 DKIM、TLS、SMTP auth，且是 Rust 生态中最成熟的 SMTP 库 |
| 注册无验证 | ❌ 方向四指出 | 设计决策合理（最小可行产品），但现已到 P1 阶段。属于**必须偿还的遗留债务** |

---

## 二、扩展方向

### 扩展方向 A：通信渠道抽象层（Notification Dispatcher + Channel Registry）

这是一个架构级扩展，不仅解决方向五（邮件通知），还为未来所有通知渠道提供统一的扩展点。

#### 为什么需要

当前三个通知渠道（WS、Push、即将有的 Email）各自独立：
- WS 扇出：`Hub::fan_out_raw`（总线的直接消费者）
- Push 推送：`push_bot`（监听总线）
- 邮件通知：需要新建 bot 或混入 push_bot

没有统一抽象的风险：
- 通知渠道优先级/降级/回退需要在每个 bot 中重复实现
- 每个类型的通知（@mention、DM、直播开播）需要独立配置权限、频率、渠道
- 推送失败回落其他渠道的逻辑不可复用

#### 核心挑战

1. **状态机复杂度**：渠道尝试 → 部分失败 → 回退 → 重试 → 死信。这需要的有状态 DAG（类似 webhook 的 backoff 模型）。
2. **偏好聚合**：每个通知需查询用户偏好 + 工作区默认 + 频道覆盖 → 最终决定用哪些渠道。查询链长度可能导致瓶颈。
3. **频率控制**：邮件和推送各有速率限制，避免用户被通知淹没。
4. **事务性与幂等**：通知不应该是事务性的（通知丢失可接受），但通知**重复**是需要避免的（用户反感）。需要通知去重。

#### 预期的架构变更

```
当前：
  RoomEvent → push_bot (if-else for each channel)

目标：
  RoomEvent → NotificationDispatcher
    → 1. 查询 notif_prefs → [channel_prefs]
    → 2. 按优先级尝试各 Channel
         └─ WsChannel    (当前: Hub)
         └─ PushChannel  (当前: push_bot)
         └─ EmailChannel (新建: email_digest_templates + mail_queue)
    → 3. 记录 delivery_outcomes (可选: Prometheus histograms)
```

**trait 定义示意**（非代码，仅概念）：

```
trait NotificationChannel {
    fn name(&self) -> &'static str;
    async fn deliver(&self, event: NotifyEvent, recipient: Participant) -> Result<DeliveryOutcome>;
    fn priority(&self) -> u8;  // 0=WS, 1=Push, 2=Email, 3=SMS
}
```

#### 对现有系统的影响

- **push_bot.rs 需要重构**：从「通知调度器」缩小为「推送通道实现」，把事件路由交给 `NotificationDispatcher`
- **新加文件**：`notification_dispatcher.rs`、`email_channel.rs`
- **对 Hub 无影响**：WS 通道保持现有扇出路径
- **对 notif_prefs 的影响**：扩展字段，但不破坏现有结构

#### 时间线估计

| 阶段 | 工作量 | 并行度 |
|------|--------|-------|
| 定义 Channel trait + NotifyEvent 规范化 | 2-3 天 | 独立 |
| 重构 push_bot → PushChannel | 2 天 | 可与上一步并行 |
| 实现 EmailChannel | 3-4 天 | 依赖方向一的 mailer 扩展 |
| 实现 NotificationDispatcher 编排逻辑 | 3-5 天 | 依赖前两步 |
| 单测 + 集成测试 | 2-3 天 | 并行 |

#### 架构选项对比

| 选项 | 方案 | 优点 | 缺点 |
|------|------|------|------|
| **A: Dispatcher + Channel trait** | 引入显式的 trait 和注册表 | 扩展性最佳，每个渠道独立测试 | 前期投入较大（约 10-12 天） |
| **B: 扩展现有 push_bot** | 在 push_bot 内添加 `match channel` 分支 | 快速（2-3 天），最小化变更 | 长期难以维护，渠道间逻辑耦合 |
| **C: 事件总线多 bot** | 每个渠道一个 bot（已有 push_bot，新建 email_bot） | 复用当前 bot 模式，零重构 | 渠道回退逻辑需要跨 bot 通信（复杂），通知去重困难 |

**推荐选项 A**。当前阶段引入 Channel trait 的成本可控，而将来要加 SMS、Webhook、Slack 集成等渠道时，收益会指数级增长。

---

### 扩展方向 B：异步作业框架（Job Runner / Workflow Engine）

方向二（负载测试）暴露的是表象，深层问题是：**系统缺少统一的异步作业框架**。

#### 为什么需要

当前代码库中有多个独立的「后台任务」模式：

| 模式 | 例子 | 问题 |
|------|------|------|
| `tokio::spawn` loop | 定时器（sweep、heartbeat） | 无重试、无监控、无超时 |
| `FOR UPDATE SKIP LOCKED` 轮询 | AiWorker (ai_jobs 表) | 没有通用的 job 定义/调度 |
| 总线 bot | push_bot、unfurl_bot | at-least-once 投递但无退信/死信管理 |
| 内存 `tokio::mpsc` drain | MailQueue（方向一建议） | 每个队列独立实现 |

邮件队列、通知调度、摘要编译、数据导出、Webhook 重试——这些都需要**同一个基础设施**：
- 延迟调度（`schedule_at` / `schedule_after`）
- 重试策略（指数退避 + 最大次数 + 死信）
- 速率限制
- Prometheus 指标（排队中 / 执行中 / 已完成 / 失败）
- 去重（幂等键）

#### 核心挑战

1. **PG vs Redis vs NATS 作为作业后端**：各有取舍（见下方选型表）。
2. **事务一致性**：当作业的副作用是 DB 写入时，`enqueue job` 和 `commit transaction` 需要原子性（outbox 模式）。
3. **worker 水平扩展**：AiWorker 的 `SKIP LOCKED` 模式可复用，但 job 类型多时需要避免不同类型互相阻塞。
4. **延迟精度**：摘要邮件需要每天 08:00 发送，不是 ±5 分钟——如果直接用 cron loop，节点重启后延迟会漂移。

#### 作业后端选型

| 维度 | **PG (pg_task / 自建)** | **Redis (Bull/Resque 模式)** | **NATS JetStream** |
|------|------------------------|----------------------------|-------------------|
| 事务一致性 | ✅ 与业务 DB 同事务（outbox） | ❌ 2PC 或最终一致 | ❌ 最终一致 |
| 延迟调度 | ✅ `pg_try_advisory_lock` + sleep | ✅ ZSET + 轮询 | ❌ 不原生支持 |
| 持久化 | ✅ 强一致性 | ❌ RDB/AOF 可能丢数据 | ✅ 类 Kafka 持久 |
| 速率限制 | ❌ 需自行实现 | ✅ 多个 Redis 工具 | ❌ 需自行实现 |
| 运维复杂度 | 低（复用 PG） | 中（已有 Redis） | 低（已有 NATS） |
| 延迟精度 | 秒级 | 秒级 | 毫秒级（但延迟调度需额外实现） |
| 项目已有 | ✅ PG | ✅ Redis | ✅ NATS |

**推荐**：基于 PG 的自建轻量作业表（复用 `ai_jobs` 的模式，但泛化为 `scheduled_jobs`）。理由：
- AiWorker 已经证明了 `FOR UPDATE SKIP LOCKED` 在生产环境可靠
- outbox 模式天然支持事务一致性（`INSERT job` + `COMMIT message/email` 原子）
- 0 新基础设施依赖

#### 对现有系统的影响

- **新 crate/模块**：`aero-jobs` 或 `aero-server/src/job_runner.rs`
- **迁移**：`CREATE TABLE IF NOT EXISTS scheduled_jobs (...)` — 类型、状态、payload（JSONB）、幂等键、调度时间、重试信息
- **现有定时器逐步迁移**：不是一次性替换，而是新作业用 job runner，旧的逐步迁移

---

### 扩展方向 C：API 契约治理 + 版本化 API 策略

方向二的 API 契约测试真空是一个症状，深层问题是：**API 无契约治理**。

#### 为什么需要

126+ `.merge()` 路由 + 30+ 内联路由 ≈ **150+ 端点**，但：
- 无 OpenAPI 文档与实现的一致性校验（手写 OpenAPI doc 放在仓库里，但与实际路由分离——这是**文档债务**）
- WS 帧类型（~30 种）无 schema 定义（靠 serde 反序列化时的 panic 发现异常）
- 无向后兼容性门禁（修改 `RoomEvent` 字段时，不通知移动端/Web 端等消费方）
- 无 API changelog 自动生成

#### 核心挑战

1. **Rust 的强类型 vs OpenAPI 的 schema 生成**：当前用 `axum` + `serde`，虽然有 `utoipa`/`okapi` 等 OpenAPI 库，但需要为每个路由显式注解——当前 150+ 端点的工作量可能很大。
2. **WS 帧的 schema 定义**：WS 的帧类型是 tagged-enum（`ClientFrame`/`ServerFrame`），OpenAPI 对 WS 的支持有限。可能需要独立 schema registry。
3. **版本化策略**：当前 API 无版本前缀（`/api/rooms/...` 而非 `/api/v1/rooms/...`）。引入版本化是合理的，但路径上的版本 vs 头上的版本 vs 协商版本各有取舍。

#### 建议方案

分三步，渐进式：

| 阶段 | 目标 | 方法 | 工作量 |
|------|------|------|--------|
| **P0** | 核心路由的快照测试 | 用 `insta` 对 10 个关键路由记录 JSON 响应 schema。CI 中检测 schema diff | 2-3 天 |
| **P1** | WS 帧 schema lint | 为 `ClientFrame`/`ServerFrame` 的 serde enum 编写测试，验证反序列化/序列化对称性 | 2 天 |
| **P2** | OpenAPI 生成 | 引入 `utoipa`，为关键路由（消息/搜索/房间/用户）添加注解，生成 OpenAPI 3.1 规范 | 1-2 周 |

**关键决策：API 版本化**。

| 选项 | 优点 | 缺点 |
|------|------|------|
| **路径版本化** `/api/v2/rooms` | 明确、易缓存、可并行部署 | URL 冗长，每个新版本需要复制路由集 |
| **Header 版本化** `Accept: application/vnd.aero.v2+json` | URL 简洁 | 缓存困难，需要中间件解析 |
| **协商版本化**（参数 `?v=2`） | 简单 | 不可缓存，URL 含义不明确 |

**推荐**：当前不需要版本化（还没有外部 API 消费者）。在引入第一个**不向后兼容**的变更时，按情况选路径版本化。在此之前，只做 schema 快照 + 兼容性检测。

---

### 扩展方向 D：客户端状态机与离线架构

方向三（深度链接）和方向四（身份验证）其实都指向同一个缺口：**客户端架构不完整**。

#### 为什么需要

当前的 SPA（`web/app.js` 约 5.9K JS）是一个**柱塞式视图切换器**，而不是一个**状态驱动的前端应用**。这导致：

1. URL 路由缺失（方向三的直接问题）
2. 离线状态无管理（WS 断开后不显示「重新连接中」）
3. 无状态持久化（刷新丢失全部上下文）
4. 无乐观更新（消息发出的动画与实际发送之间无中间态）

#### 核心挑战

1. **架构决策：库 vs 自建**——引入 React/Vue 需要重写全部 SPA（不现实）；在纯 JS 中自建状态机需要设计约束。
2. **WS 状态恢复**——WS 重连后需要重新订阅房间、恢复消息游标、同步已读回执。
3. **乐观 UI + 冲突解决**——消息发送的乐观插入本地，服务端回执与本地 ID 映射。

#### 建议架构

```
┌─────────────────────────────┐
│       App Router            │ ← hash-based routing
├─────────────────────────────┤
│    Application State        │ ← 根状态对象（房间、消息、用户、连接）
├──────────┬──────────────────┤
│  WS      │  Auth / Session  │
│  Client  │  Manager         │
├──────────┴──────────────────┤
│     Notification            │
│     Dispatcher (OS + App)    │
├─────────────────────────────┤
│     View Layer              │ ← 纯 DOM 渲染（不引入框架）
└─────────────────────────────┘
```

**不要引入 React/Vue/Svelte**。5.9K JS 的规模引入框架是过度工程。而是在现有 `web/app.js` 基础上分层：
- `router.js`（~200 行）：hash-based 路由 + popstate 处理
- `state.js`（~300 行）：根状态对象 + 房间/消息/用户子状态
- `ws_client.js`（~400 行）：连接管理 + 重连 + 帧序列化

增量重构，每晚不再引入。

---

## 三、接口设计建议

### 3.1 关键模块的接口设计原则

基于当前架构和 5 个缺口分析，我提出以下接口设计原则：

#### 原则 1：渠道接口可组合（Composite Channel）

```
// 概念：渠道是插件，不是继承
trait NotificationChannel {
    fn id(&self) -> ChannelId;
    fn max_rate_per_hour(&self) -> u32;
    async fn deliver(&self, envelope: Envelope) -> Result<DeliveryReceipt>;
}
```

**为什么**：当前 push_bot 监听了总线事件→查询偏好→发送推送。未来的邮件通知不应该也监听总线+查询偏好——而是由一个 `NotificationDispatcher` 统一做偏好查询和事件路由，**各渠道只负责发送**。

#### 原则 2：作业接口可观测（Observable Job）

```
// 概念：每个作业自带生命周期
trait Job {
    fn id(&self) -> JobId;
    fn kind(&self) -> JobKind;
    fn max_retries(&self) -> u8;
    async fn execute(&self, ctx: JobContext) -> Result<()>;
}
```

**为什么**：当前 AiWorker 的 `ai_jobs` 表已经有 `status`、`attempts`、`last_error`、`deferred_until`——这是正确的抽象雏形。需要将它提升为框架级约定，让邮件发送、摘要编译、数据导出等任务复用相同的重试/死信/监控基础设施。

#### 原则 3：前端状态可序列化（Serializable State）

```
// 概念：整个应用状态可转 JSON → sessionStorage → 恢复
interface AppState {
    currentRoomId: string | null;
    currentUserId: string | null;
    rooms: Map<RoomId, RoomState>;
    lastScrollPositions: Map<RoomId, number>;
    draftMessages: Map<RoomId, string>;
    authToken: string | null;
}
```

**为什么**：刷新丢失上下文是方向三的核心问题。可序列化的状态对象是解决的起点。

#### 原则 4：幂等键驱动（Idempotency-Key Driven）

**为什么**：方向四（注册验证）和方向五（邮件通知）都涉及**用户触达**——重复发送验证邮件比漏发更糟糕。所有用户触达 API 应接受可选的 `Idempotency-Key` 头。

```
// POST /api/auth/send-verification
// Idempotency-Key: <client-generated>

// 第一次调用 → 发送验证邮件
// 第二次调用（重复 key）→ 返回相同响应，不重发邮件
// 60s 后 key 过期，可重发
```

这不是新概念——Stripe 的幂等键就是标准。在此类用户触达场景中，幂等键比 email 内冷却更安全。

### 3.2 是否需要引入新的抽象层

| 抽象层 | 需要？ | 理由 |
|--------|--------|------|
| **Notification Channel trait** | **是** | 避免 push_bot 膨胀，为邮件/SMS/Webhook 提供统一扩展点 |
| **Job 框架** | **是** | 统一 ai_jobs、邮件队列、摘要编译、webhook 重试等后台任务 |
| **Mailer 分层** | **是** | 当前 `mailer.rs` 是 1 个文件 2 个函数 → 拆分成 Config/Template/Renderer/Transport/Queue 层次 |
| **API 契约生成器** | **否**（可以晚） | utoipa 注解工作量在 P2 阶段，当前先做 schema 快照 |
| **前端状态管理** | **是**（轻量级） | 不是 Redux/Zustand，是 200 行 `state.js` 对象 + `sessionStorage` 序列化 |
| **前端路由** | **是** | 不是 react-router，是 150 行 hashchange 处理 |

### 3.3 向后兼容性设计

#### 邮件基础设施（方向一 → 方向四 → 方向五）

**兼容策略**: 配置驱动的渐进增强

```
// config.toml
[email]
require_verification = false       # 默认关闭，兼容现有部署
dkim_enabled = false               # 默认关闭
template_dir = "templates/email"   # 可选，不存在则回落纯文本
```

- 现有用户不受影响（`email_verified_at` 为 NULL 表示「未验证，但跳过」）
- 新部署可开启 `require_verification=true`，现有部署可随时开启
- 模板引擎可选：无模板时回落纯文本 `format!()`

#### 通知渠道扩展（方向五）

**兼容策略**: 渐进偏好模型扩展

```
// NotifPrefs 新增字段（默认 false/never）
email_on_mention: bool = false
email_digest: "never"  // "never" | "daily" | "weekly"
```

- 现有 notif_prefs 行：查询时 COALESCE/默认值，无需迁移脚本
- 新增字段不破坏已有序列化格式（如果 notif_prefs 是 JSONB，直接加 key）

#### 前端深度链接（方向三）

**兼容策略**: 渐进式 URL 引入

```
// 阶段 1：只读 hash（不破坏现有视图切换）
window.addEventListener('hashchange', () => {
    parseHash(location.hash);  // 新增，不修改现有 showAuth/showChat
});

// 阶段 2：写入 hash（不改变现有交互）
showChat() → showChat() + history.replaceState(...) + updateHash()

// 阶段 3：全面路由（旧 URL 重定向）
if (location.hash) { parseAndNavigate(); }
```

每个阶段可独立合并，不影响现有用户。

---

## 四、技术选型

### 4.1 现有技术栈评估

| 组件 | 现有选型 | 对 5 个缺口的适配度 | 评估 |
|------|---------|-------------------|------|
| **邮件客户端** | `lettre` 0.11 | ✅ DKIM/TLS/SMTP auth 全支持 | 无需更换，只需要更充分的配置 |
| **模板引擎** | 无（`format!()`） | ❌ 需要引入 | 见下方选型 |
| **性能测试** | 无 | ❌ 需要引入 | 见下方选型 |
| **Web 前端构建** | 无 bundler（零依赖 CDN） | ⚠️ 轻量级路由无需 bundler | 不要引入 webpack/vite |
| **作业后端** | PG + `FOR UPDATE SKIP LOCKED` | ✅ AiWorker 已证明可行 | 泛化复用，不引入新依赖 |

### 4.2 需要引入的新技术栈

#### 4.2.1 邮件模板引擎

| 选项 | 优点 | 缺点 |
|------|------|------|
| **`tinytemplate`** | 零依赖、编译期检查、Rust 原生 | 功能有限（无部分渲染、无 helpers） |
| **`handlebars` (rust)** | 功能完整（partials/helpers/layout）、成熟生态 | 运行时解析，大小约 100KB+ |
| **`minijinja`** | Jinja2 语法（Python 用户友好）、sandboxed | 相对新（0.x 系列），文档较少 |
| **服务器端渲染 HTML 字符串** | 无依赖 | 维护性差，XSS 风险，不推荐 |

**推荐**: `tinytemplate` 用于邮件模板（简单、编译期安全），如果将来需要复杂模板（条件分支、循环、布局），再升级到 `handlebars`。

邮件模板的复杂度较 AI 的 prompt 模板更低——通常只需要变量替换 + 简单的条件显示。`tinytemplate` 足够。

#### 4.2.2 负载测试工具

| 选项 | 优点 | 缺点 |
|------|------|------|
| **k6** | JS 脚本、CI 友好（`k6 run --out json`）、WS 支持 | 需安装运行环境 |
| **locust** | Python、分布式支持 | WebSocket 支持差（需插件） |
| **`cargo bench` + criterion** | 开发环境零新依赖、精确微基准 | 只能测 Rust 内部函数，不能测 HTTP/WS 端到端 |
| **自定义测试客户端** | 精确控制 Aero IM 协议 | 维护成本高 |

**推荐**: 组合策略——
- **微基准**（Rust 内部）：`cargo bench` + `criterion`（零外部依赖）
- **端到端负载**：k6（脚本简单，CI 可集成）
- **WS 模糊测试**：自建状态机模糊器（因为 k6 的 WS 模糊能力有限）

#### 4.2.3 Web 前端（深度链接需要的内容）

**不需要引入框架**。5.9K JS 引入 React/Vue 是过度工程。需要的是：

| 需要 | 方案 | 行数估计 |
|------|------|---------|
| 路由 | `hashchange` 事件 + URL parser | ~150 行 |
| 状态管理 | 根 `state` 对象 + `sessionStorage` 序列化 | ~200 行 |
| WS 重连管理 | Exponential backoff 模式 | ~150 行 |
| 消息锚定滚动 | `scrollIntoView()` + highlight | ~80 行 |

这些都可以用原生 JS 实现，不需要 bundler/transpiler/dependency。

### 4.3 自建 vs 采购/集成决策

| 场景 | 建议 | 理由 |
|------|------|------|
| **邮件发送** | 自建（mailer.rs 扩展） | `lettre` 已集成，扩展成本低 |
| **邮件模板** | 自建（tinytemplate + templates/email/） | 邮件模板数量少（~5 个模板），自建可控 |
| **邮件队列** | 自建（tokio::mpsc + PG backfill） | 复用 AiWorker 模式，不引入新依赖 |
| **负载测试** | 开源 k6 | 行业标准，免费，CI 可集成 |
| **Web 路由** | 自建纯 JS | 5.9K 应用不需要路由库 |
| **第三方邮件服务** | SendGrid/Mailgun/SES（通过 SMTP） | `lettre` 不绑定特定供应商，通过 SMTP 统一接入 |
| **身份验证流程** | 自建 | 邮件验证是简单的 CRUD + 令牌 + 邮件发送，无现成库比自建更轻 |

### 4.4 第三方依赖评估标准

对于每个新引入的依赖，使用以下评估框架：

```
1. 必要性：这个问题的复杂度和重要性，是否必须依赖三方库？
2. 维护性：库的维护状态（star/commit/issue 响应/与 rustc 兼容性）
3. 安全：SAST 扫描结果 + CVE 历史 + unsafe 使用范围
4. 大小：编译时间影响 + 二进制体积影响
5. 替代性：是否容易自建（复杂度 vs 价值比）
```

以 `tinytemplate` 为例：
- 必要性：✅ 手工 `format!()` 不安全（XSS）+ 不可维护
- 维护性：✅ 成熟稳定（2017 年至今，1.x）
- 安全：✅ 0 unsafe，编译期检查（无运行时注入）
- 大小：✅ 极小（~200 行核心代码）
- 替代性：❌ 自建模板引擎是浪费

**结论：引入 `tinytemplate` 是合理决策。**

---

## 五、实施路线图

### 5.1 优先级矩阵

按「业务影响 × 技术依赖 × 工程成本」三维评估：

| 方向 | 业务影响 | 技术依赖 | 工程成本 | 综合优先级 |
|------|---------|---------|---------|-----------|
| 方向一（邮件基础设施） | **高** — 邮件验证是安全基线 | 低 — 已有 mailer.rs | 5-8 天（模板 + 队列 + DKIM） | **P0** |
| 方向二（性能测试） | **中** — 间接（长期质量保障） | 无 — 独立 | 4-6 天（k6 + bench + fuzz） | **P0** |
| 方向三（深度链接） | **高** — 直接影响协作 UX | 无 — 纯前端 | 3-5 天 | **P1** |
| 方向四（身份验证） | **高** — 安全基线合规 | 高 — 依赖方向一（邮件） | 3-5 天（验证流程 + 限流） | **P0**（与方向一紧耦合） |
| 方向五（邮件通知） | **中高** — 离线触达保障 | 高 — 依赖方向一（模板 + 队列） | 5-8 天 | **P1**（依赖方向一完成后） |

### 5.2 阶段划分

#### 阶段 1：「安全基线 + 质量基线」（P0）—— 预计 2 周

```
目标：还债，建立安全基线和质量基线
依赖：无外部依赖，可独立并行
```

| 周 | 内容 | 并行 |
|----|------|------|
| **Week 1** | **团队 A**：方向一邮件基础设施（模板 + DKIM + 队列） | **团队 B**：方向二负载测试（k6 场景 + cargo bench + WS fuzz） |
| **Week 2** | **团队 A**：方向四邮件验证（验证表 + 发送 + 注册门控 + 限流） | **团队 B**：方向二 CI 集成（criterion + bench compare + k6 weekly cron） |

**里程碑**： `M1 — 安全基线就绪`
- 新注册用户可验证邮箱
- 未验证用户可被门控（`AERO_REQUIRE_EMAIL_VERIFICATION=true`）
- 注册限流阻止暴力注册
- CI 中有性能回归门禁（至少 3 个基准 + 1 个 k6 场景）

**风险点**：
- ⚠️ 注册限流在 Redis 上实现（多实例共享）可能需要 `fred` 的额外配置——已有 Redis 连接，风险可控
- ⚠️ k6 CI 集成可能需要 Docker（k6 可能不在 runner 上）——提前确认 CI 环境

#### 阶段 2：「通知多渠道 + 深度链接」（P1）—— 预计 2-3 周

```
目标：补全通知渠道，改善用户体验
依赖：依赖阶段 1 的邮件基础设施
```

| 周 | 内容 | 并行 |
|----|------|------|
| **Week 3** | **团队 A**：方向五邮件偏好（notif_prefs 扩展 + API） | **团队 B**：方向三深度链接核心（hash 路由 + 复制链接 + 浏览器历史） |
| **Week 4** | **团队 A**：方向五邮件通知调度器（摘要编译 + 发送） + 离线回退策略 | **团队 B**：方向三会话恢复 + 草稿保存 |
| **Week 5** | 集成测试 + bug 修复 | — |

**里程碑**： `M2 — 多渠道通知就绪`
- 用户可在偏好中设置邮件通知频率
- 离线时 @mention 通过邮件回退
- 每日/周摘要邮件正常工作
- 可分享消息链接（`#/room/{id}?msg={mid}`）
- 页面刷新恢复房间上下文和滚动位置

**风险点**：
- ⚠️ 邮件通知调度器需要处理时区（用户的「每日摘要」应该基于用户时区的 08:00）——第一阶段只做 UTC 每日，后期再加时区支持
- ⚠️ 摘要编译的 PG 查询可能较重（跨房间聚合未读消息）——需要加 `page_size` 限制 + 超时

#### 阶段 3：「架构治理 + 契约」（P2）—— 预计 3-4 周

```
目标：引入抽象层，建立 API 契约治理
依赖：依赖阶段 1-2 的实际使用反馈
```

| 周 | 内容 |
|----|------|
| **Week 6-7** | Job 框架提取（从 ai_jobs 提取泛化，迁移邮件队列、摘要编译、webhook 重试等任务） |
| **Week 7-8** | Notification Channel trait 引入 + push_bot 重构为 PushChannel |
| **Week 8-9** | API 契约快照测试 + WS schema lint + 阶段性 OpenAPI 注解 |

**里程碑**： `M3 — 架构治理就绪`
- 后台任务统一使用 Job 框架
- 通知渠道通过 Channel trait 扩展
- 关键路由的 schema 快照在 CI 中自动 diff
- 无新增的后台任务绕过 Job 框架

**风险点**：
- ⚠️ push_bot 重构有回归风险（当前 push_bot 在生产环境运行中）——需灰度切换：先在新版本部署时并行运行两者，确认新 dispatcher 行为一致后再下线旧 bot
- ⚠️ Job 框架的「迁移成本」——现有 ai_jobs 表的查询模式可能需要调整——但 ai_jobs 本来就是自用表，迁移向后兼容

### 5.3 风险矩阵与缓解策略

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| **邮件发送被标记为垃圾** | 高 | 高（验证邮件进垃圾箱 = 用户无法注册） | 1) DKIM/SPF/DMARC 配置文档 + 部署时校验工具 2) 提供邮件测试工具（`aero-cli test-email`）|
| **邮件模板 XSS** | 中 | 高（用户名/房间名可注入 HTML） | tinytemplate 默认 HTML escape，但需确认每个变量用了 `{var|escape}` |
| **k6 WS 场景不成熟** | 中 | 中（花时间写错了 WS 协议） | 先用 REST API 负载测试（简单），WS 场景放在第二阶段 |
| **深度链接影响现有用户** | 低 | 中（hashchange 事件可能干扰现有 SPA 逻辑） | 第一阶段**只读** hashchange，不写入，不改变现有导航路径 |
| **邮件队列与业务事务不一致** | 中 | 高（邮件重投/丢邮件） | outbox 模式：在业务事务中 INSERT email_jobs，后台 drain 处理 |

### 5.4 关键决策点时间线

```
Week 1          Week 3          Week 6          Week 9
│               │               │               │
P0 安全基线     P1 多渠道通知   P2 架构治理     持续演进
│               │               │               │
├─ 邮件模板     ├─ 邮件偏好     ├─ Job 框架      ├─ 更多渠道
├─ DKIM         ├─ 摘要调度     ├─ Channel trait  (SMS/Webhook)
├─ 邮件队列     ├─ 离线回退     ├─ API 契约      ├─ 自动化测试扩展
├─ 注册验证     ├─ 深度链接     └─ push_bot 重构 └─ FCM/APNs 改进
├─ 注册限流     └─ 会话恢复
└─ 性能测试
   └─ k6              ▲                ▲
   └─ bench           注意：方向三      注意：push_bot 重构
   └─ CI 集成         （深度链接）      与通知 dispatcher 引入
                       独立可并行。        有回归风险，需灰度。
```

---

## 六、总结性架构意见

### 6.1 这 5 个缺口的本质

缺口 | 本质 | 一句话根因
-----|------|-----------
邮件基础设施 | 通信渠道的单点故障 | 只有一条离线触达渠道（推送），而邮件渠道停在原型阶段
性能测试 | 工程成熟度缺失 | 从「可工作」到「可规模化」的差距——没有性能基准就无法做容量规划
深度链接 | 产品架构阶段错位 | SPA 架构停在「原型展现层」，未演进到「应用路由层」
身份验证 | 安全架构的基线缺失 | 最小可行产品遗留了过度简化的注册流程
邮件通知 | 多渠道通知的架构空白 | 通知系统缺少「渠道抽象层」，每个新渠道需要从零实现

### 6.2 架构建议的核心主题

**一个主题贯穿所有 5 个方向：抽象层的引入时机。**

架构的核心问题是「什么时候引入抽象层」：
- 引入过早：过度工程（YAGNI）
- 引入过晚：重构成本高，if-else 蔓延

这份分析报告指出的抽象层引入建议：

| 抽象层 | 引入时机 | 当前状态 |
|--------|---------|---------|
| Notification Channel trait | **此时** | push_bot 即将从 1 个渠道扩展到 2 个（+email），正好引入 |
| Job 框架 | **此时** | 已有 ai_jobs（单体实现），泛化成本低 |
| 邮件模板引擎 | **立刻** | 两个静态模板已不够用，第三个（验证邮件）就是引入点 |
| 前端路由 | **立刻** | 0 路由 → 有路由，现在是引入时机 |
| API 版本化 | **非现在** | 尚无外部 consumer，过早引入无用 |

### 6.3 最终建议

1. **不要一次性改造所有 5 个方向**。按阶段 1 → 2 → 3 排序，每个阶段交付后评估再决定下一步。
2. **方向一（邮件基础设施）和方向四（身份验证）是 P0，应优先且紧耦合执行**——因为没有邮件基础设施，身份验证无法落地。
3. **方向二（性能测试）与方向一/四完全独立，可并行推进**。建议两个团队并行，最大化利用率。
4. **方向三（深度链接）和方向五（邮件通知）放在第二阶段**，一是依赖方向一，二是需要时间观察第一阶段变更的稳定性。
5. **架构治理（阶段 3）中的 Channel trait + Job 框架是提升长期可维护性的关键**，但不要过早抽象（当只有 2 个渠道/2 个作业类型时就引入框架是过度工程）。建议在邮件通知开发过程中提炼抽象——先实现，再泛化。

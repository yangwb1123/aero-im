现在我有了文档的完整内容。下面是架构师角度的综合分析。

---

# 架构分析：Aero IM 五条战略扩展方向

## 1. 架构评估

### 1.1 现有架构优势

Aero IM 的架构设计在以下方面表现出色，为扩展提供了坚实的基础：

- **事件驱动骨架的正确性**：`Hub → NATS → 扇出` 的事件 DAG 是经过充分验证的架构模式。`durable consumer` 的选型确保了消息投递的 at-least-once 语义，`ephemeral consumer` 用于弹幕等可丢失场景——这个二元区分是成熟的设计决策。

- **数据层充分预埋**：`price_cents`、`subscription_tiers`、`creator_subscriptions` 等变现数据模型的完整 CRUD、幂等键、事务保证——说明作者在设计阶段已经预见了商业化的方向。这正是架构前瞻性的体现：**数据结构就位后，接入支付网关是纯工程工作**。

- **SFU 端的 Simulcast 基础设施**：`simulcast.rs` 的空间/时间层级选择 + RTCP PLI/FIR 反馈已在 `aero-live-webrtc` 中实现。虽然上层 `SfuForwarder` 未接线消费 Receiver Report，但底层解析能力已经有了——这降低了方向一的 SFU 端改造成本。

- **AGENTS.md 的 seam 标注机制**：明确标注了 `call_bridge_supervisor.ensure_egress` 为"已建+单测但生产未接线"、`SfuMediaSession.run` 为"生产无实例化"。这种透明标注减少了后续开发者的认知负担。

### 1.2 架构债务/技术债

| 类型 | 严重度 | 描述 |
|------|--------|------|
| **NATS 错误传播导致的事务级联** | **高** | `messages.rs:109` 的 `publish_room_event` 中 NATS 发布失败会传递到上层，导致 PG 事务也回滚。这是《方向五》中需要优先解决的连锁故障风险。当前架构中消息存储和实时投递是耦合的，缺失异步容错层 |
| **WebRTC 质量监控的真空** | **中** | `calls.js` 中 `connectionstatechange` 仅处理 failed/disconnected，无 `getStats` 管线。这不是 bug，而是**架构上的缺失层**——客户端 WebRTC 质量管理应被视作一个与信令层并列的能力层，当前未被识别为独立架构模块 |
| **静态资源分发无缓存策略** | **低（但随规模恶化）** | `ServeDir` 无 `Cache-Control`、无 hash-based 版本化、无 ETag 优化。单请求影响小，但随着用户增长会成为不必要的源站负载 |
| **客户端状态架构属于"单标签页单例"** | **中** | `context.js` 的 `state` 对象和 `ws.js` 的 `WsClient` 实例每个标签页独立创建，不存在跨标签页协调机制。这不是 bug，而是客户端状态架构缺少一个**状态协调层** |
| **退化策略分散在各模块** | **中** | Redis 故障时 `presence.rs`、`rate_limit.rs`、`room_member_cache.rs`、`online.rs` 各自做降级判断，无统一策略入口。这是"应急补丁"而非"系统设计" |

### 1.3 关键设计决策评估

| 决策 | 合理性 | 需要重新审视的情况 |
|------|--------|-------------------|
| Mesh 通话架构 | 合理（≤4 人群组场景） | 当群组通话规模扩展到 >4 人时，Mesh 的上行带宽瓶颈会迫使转向 SFU 架构。需要考虑 SFU 模式何时成为默认 |
| HLS 切片由源服务器直接分发 | 早期阶段合理 | 当直播同时观看人数 >100 时，CDN 成为必须。当前设计需要支持"从源站直发"到"CDN 边缘分发"的切换 |
| 客户端状态全内存不持久化 | 符合 SPA 定位 | 当需要离线/PWA 支持和跨设备同步时，需要引入 `IndexedDB` 层和同步协议 |
| 变现数据模型先于支付管线 | **高度合理** | 这是"数据先行"的正确设计决策。支付网关是有成熟 SDK 的工程集成，而数据模型设计需要更多的领域思考 |

---

## 2. 扩展方向（高价值架构扩展）

基于文档中五条方向的分析和交叉验证报告的额外发现，我提出以下架构扩展方向：

### 2.1 方向 A（P0）：发布-存储解耦层——消除 NATS 故障导致的连锁事务回滚

**为什么需要（技术价值）**：
当前 `publish_room_event` 中 NATS 发布失败会向上传播，导致已成功存储的 PG 事务被回滚。这是_可避免的破坏_——消息已安全存储在 PG 中，实时投递不应成为消息存储的依赖。

**核心挑战**：
- 需要引入一个**持久化的 outbox 表**（PG 本地）或**可靠重试队列**，在 PG 事务提交后异步 drain 到 NATS
- 需要处理"应用重启时 outbox 中有待发布事件"的场景
- outbox 读取和 NATS 发布之间需幂等性（已通过 per-subject seq 解决）

**预期的架构变更**：

```
当前：HTTP Handler → Service → Store(PG) → publish(NATS) → Response
                                      ↑ 失败回滚整个事务 ↓

目标：HTTP Handler → Service → Store(PG + INSERT into outbox) → Response(200)
                                        ↓ (异步 drain 任务)
                                publish(NATS) → DELETE from outbox
```

- `ImService::publish_room_event` 拆分为**同步部分**（PG 存储 + outbox 写入）和**异步部分**（drain outbox → NATS）
- 新增 `message_outbox` 表（`id`, `event_type`, `payload_json`, `subject`, `seq`, `created_at`, `attempts`）
- 新增 drain 任务（`run_outbox_drainer`），类似 `blob_gc_drain` 的模式
- `GET /api/rooms/:id/messages` 作为回退：即使 NATS 延迟，用户仍然通过 REST 获取消息

**对现有系统的影响**：
- 不改变 `RoomEvent` 结构、WS 扇出逻辑、WS 帧格式
- 不改变客户端代码
- `publish_room_event` 的同步调用签名不变，但内部逻辑改为"写入 outbox 后直接返回"
- `run_bus_listener` 的 durable consumer 保持完全不变——因为 outbox drain 最终也会发到同一个 subject

**权衡选项**：

| 选项 | 优点 | 缺点 |
|------|------|------|
| **PG outbox（推荐）** | 与 PG 事务在同一事务中，强一致性；无新依赖 | outbox 表需要定期清理；drain 任务需幂等 |
| **Redis Stream outbox** | 低延迟、自动过期 | 增加 Redis 依赖耦合（Redis 也可能故障）；Redis 故障时 outbox 丢失 |
| **NATS JetStream KV Store** | 原生持久化队列 | 增加架构依赖深度（需要 JetStream 稳定）；NATS 再次故障时又回到原问题 |

**选择推荐 PG outbox**，因为它是与 PG 绑定的事务性 outbox，保持了一致性、审计性，且不引入新依赖。

### 2.2 方向 B（P0）：客户端 WebRTC 质量管理层——从"能通就行"到"自适应质量"

**为什么需要（业务价值）**：
通话断线是留存杀手。当前"全有或全无"的 WebRTC 策略在移动网络下会产生大量异常断线。文档已论证其必要性，我从架构角度补充其中的关键设计决策。

**核心挑战**：
- `getStats` 的跨浏览器差异：Chrome/Firefox/Safari 的 stats 结构不同，需要抽象层
- 降级决策的时机选择：过度降级（用户还在看高清却降到 360p）和延迟降级（卡了半天才降）之间需要权衡
- `setParameters` 的异常安全性：部分浏览器不支持动态编码参数调节，需要 fallback

**预期的架构变更**：

**客户端侧**（需要在 `web/` 新增模块）：

```
web/
├── calls.js          # 现有：信令逻辑（减少修改）
├── media-stats.js    # 新增：getStats 采集 + 指标聚合
├── media-adapt.js    # 新增：质量降级决策引擎 + setParameters 调节
└── media-quality.js  # 新增：质量上报（可选 WS 帧 → 服务端统计）
```

关键接口设计：

```javascript
// media-stats.js 的核心抽象
class MediaStatsCollector {
  constructor(pc)  // 注入 RTCPeerConnection
  start(intervalMs = 1000)
  stop()
  onStats: EventEmitter<(stats: ConnectionStats) => void>
  getCurrentStats(): ConnectionStats
}

// media-adapt.js 的核心抽象
class MediaQualityAdaptor {
  constructor(senders: RTCRtpSender[])  // 注入所有视频/屏幕共享 sender
  onBandwidthChange(bandwidthKbps: number)  // 由 MediaStatsCollector 触发
  getCurrentLevel(): QualityLevel  // '1080p30' | '720p30' | '480p24' | '360p20' | 'audio-only'
}
```

**SFU 侧**（`aero-live-webrtc` 内改造）：

```
aero-live-webrtc/
├── simulcast.rs       # 保留（层级选择能力已存在）
├── forward/
│   └── mod.rs         # 修改：接入 bwe.rs 的带宽信息，做转发层级选择
├── bwe.rs            # 现有（ThroughputEwma/BandwidthEstimator）— 启用消费
└── rtcp_fb.rs        # 现有（Receiver Report 解析）— 确保上行已接线到 bwe.rs
```

**对现有系统的影响**：
- `calls.js` 需要引入 `MediaStatsCollector`/`MediaQualityAdaptor`，但不改变现有信令逻辑（invite/answer/ICE/end 保持不变）
- SFU 侧不改变 `SfuPeer` / `SfuMediaSession` 的核心接口，只在 `forward/mod.rs` 中加入带宽感知转发决策
- 无后端 API 变更（质量上报可往后放）
- **需要灰度**：可以在 `navigator.connection.effectiveType` 为 `4g` 及以下时自动启用自适应

**降级台阶设计（关键设计决策）**：

| 阶梯 | 带宽需求 | 触发条件 | 备注 |
|------|---------|---------|------|
| 1080p@30 | ~4 Mbps | 带宽充裕 | 默认 |
| 720p@30 | ~2 Mbps | 带宽 < 3 Mbps 或丢包 > 2% | 稳定体验下限 |
| 480p@24 | ~1 Mbps | 带宽 < 1.5 Mbps 或丢包 > 5% | 通话仍清晰 |
| 360p@20 | ~500 Kbps | 带宽 < 800 Kbps 或丢包 > 10% | 仅保连接 |
| audio-only | ~100 Kbps | 带宽 < 300 Kbps 或严重丢包 > 15% | 保音频不断 |

每个等级在降级和升级之间应有 **hysteresis（滞回曲线）**——避免用户在 480p↔720p 之间反复切换。建议：升级需要持续 15s 的充裕带宽 + 低丢包，降级只需要 5s 的恶劣条件。

### 2.3 方向 C（P1）：客户端状态协调层——从"单标签页单例"到"多设备收敛"

**为什么需要（业务价值 + 技术价值）**：
"红点幽灵"（手机已读→桌面仍标红）是用户每日体验的核心痛点。方向四已被充分论证。我额外指出：**这是当前所有方向中 ROO 最高的一个**——因为同设备多标签页协调约 50 行 JS（`BroadcastChannel`）即可显著改善体验，而跨设备同步的架构设计可以渐进完成。

**核心挑战**：
- 跨设备已读同步需要在服务端做游标合并：设备 A 读了 msg_50，设备 B 读了 msg_100 → 两个设备都应显示已读到 msg_100（取 max）
- 设备清单管理需要与 session 生命周期耦合
- 推送去重中"设备活跃"的判断窗口：如果用户 PC 待机但 WS 未断，应视为不活跃

**预期的架构变更**：

**客户端侧（短期，高回报）**：

```javascript
// 新增 web/broadcast-channel.js
const bc = new BroadcastChannel('aero-im');
// 心跳：每 30s 广播当前已读游标的 max 值
// 事件：room_switch / mark_read / send_message / typing
```

仅需约 50-80 行 JS，无后端变更。让同设备多个标签页共享已读状态、未读计数、房间列表。

**客户端侧（中期）**：

```javascript
// 修改 web/context.js
// 从纯内存改为 IndexedDB-backed store（可选，用于离线/PWA 准备）
```

**服务端侧（中期）**：

```
新增 API：
  GET /api/me/devices          ← 设备列表
  POST /api/me/devices         ← 注册设备（name, kind, push_token）
  DELETE /api/me/devices/:id   ← 删除设备（同时断关联 WS 连接？）
  PATCH /api/me/devices/:id    ← 命名/更新设备

修改：
  WS 帧 msg:read ← 增加可选 device_id 字段（向后兼容）
  push_bot ← 发送推送前检查同用户其他设备 WS 活跃状态
```

**设备感知 presence 的数据结构**：

```
Redis key: presence:{participant_id}:{device_id}
Value: { "kind": "mobile"|"desktop"|"tablet", "state": "active"|"idle"|"dnd", "last_seen": 1754321000 }
TTL: 60s (heartbeat-driven)
```

聚合查询 `GET /api/rooms/:id/online` 时：
1. 扫描 `presence:{participant_id}:*` 模式
2. 聚合展示：`"alice (手机在线)"`、`"bob (桌面闲置 + 手机在线)"`

**对现有系统的影响**：
- `mark_read` 的 WS 帧增加可选字段 `device_id`，现有客户端无此字段时行为不变（向后兼容）
- `push_bot` 的推送去重逻辑是做**减法**（现有推送行为不变，仅在用户有其他活跃设备时跳过）
- Redis presence key 模式的改造是**增量**的：现有 `presence:{participant_id}` 继续写入，新增 `presence:{participant_id}:{device_id}` 补充设备信息。旧模式在新老转换期间共存

### 2.4 方向 D（P1）：统一退化策略框架——从"各模块自行降级"到"系统级退化矩阵"

**为什么需要（技术价值）**：
这是_防御性架构投入_——在故障发生时保证可预测行为。不同于前三者（面向用户功能改善），这是面向运维可靠性。优先级为 P1 的原因是：**NATS 发布失败导致 PG 事务回滚**已在 `messages.rs:109` 验证为可触发的连锁故障，这直接影响核心功能（消息发送）。

**核心挑战**：
- 降级决策必须是**快速的**（微秒级），不能成为新瓶颈
- 各依赖的健康状态需要聚合和合并：如果 Redis 的健康状态在 Healthy/Degraded 之间抖动，不能每秒触发全局降级/恢复
- 降级状态需要传播到客户端（UI 提示）和运维端（告警）
- 需要避免"降级风暴"——所有 handler 同时检测到依赖故障并触发降级，导致系统 CPU 在降级处理中耗尽

**预期的架构变更**：

```rust
// 新增模块：aero-common 或 aero-server 中的 degradation.rs

pub enum DependencyHealth {
    Healthy,
    Degraded { reason: String, since: Instant },
    Down { since: Instant, last_known_good: Option<Instant> },
}

pub struct DegradationManager {
    health: Arc<RwLock<HashMap<Dependency, DependencyHealth>>>,
    // 后台检查任务
    checker_handles: Vec<JoinHandle<()>>,
}

impl DegradationManager {
    pub fn is_healthy(&self, deps: &[Dependency]) -> bool;
    pub fn required_healthy(&self, deps: &[Dependency]) -> Result<(), DegradationError>;
    // 可选：注册 handler 的健康要求
    pub fn register_handler(&self, method: &str, path: &str, required: &[Dependency]);
}
```

**退化矩阵（需文档化为 `DEGRADATION_MATRIX.md`）**：

| 功能域 | PG Down | Redis Down | NATS Down | S3 Down |
|--------|---------|------------|-----------|---------|
| 发送消息 | **不可用**（未接线只读副本） | 正常（写入已绕过缓存） | **可用延迟**（消息存储 OK，通过 outbox 异步重试，REST 拉取正常） | 正常（附件上传受影响，纯文本消息正常） |
| 查看历史消息 | 不可用 | 正常（fallback to PG） | 正常 | 正常（附件可能缺图） |
| 直播观看 | 正常（不依赖 PG 播放） | 正常（viewer count 不准确） | 正常（无互动功能） | 正常（HLS 已在 FS） |
| 通话 | 正常（不依赖 PG 信令） | 正常（roster 缓存失效） | 正常 | 正常 |
| 认证/登录 | 不可用 | 可降级到缓存 sessions | 正常 | 正常 |
| WebRTC 通话质量统计 | 正常 | 正常 | 正常 | 正常 |

**Publish-Store 解耦（方向 A 的独立依赖）**：
这是方向五最关键的子项。单独列出是因为它涉及核心消息路径的重构，而不仅仅是监控层面的变化。

**对现有系统的影响**：
- `DegradationManager` 是纯新增模块，不影响现有 handler
- 各 handler 选择性地调用 `degradation.required_healthy(&[Dependency::Postgres])` — 不强制
- NATS 解耦需要修改 `ImService::publish_room_event`（如方向 A 所述）
- `/ready` 端点改为结构化 JSON，需要编排系统（K8s probe）的适配
- 运维需要为每个依赖配置健康检查参数（超时、重试、判断窗口）

### 2.5 方向 E（P2）：支付与结算网关抽象层——从"有数据无资金流"到"可插拔支付管线"

**为什么需要（业务价值）**：
这是将现有完成的数据基础设施（`price_cents`、订阅、礼物）转化为实际收入的最后一公里。方向二的论证充分，我补充架构层面的设计。

**核心挑战**：
- 支付网关的抽象：不仅仅从 Stripe 开始，需要设计一个 `PaymentGateway` trait，支持多后端（Stripe / Paddle / LemonSqueezy / 自建）
- 支付 webhook 的安全：入站 webhook 需要签名验证（`stripe-signature` header），必须严格校验
- 订阅状态机的动态 + 幂等：`pending_payment → active → past_due → cancelled` 的每个状态转移都需要是幂等的（webhook 可能重传）
- 平台抽成与创作者结算：需要引入金额级别的记账（分账、计提、结算）

**预期的架构变更**：

**新增 crate**：`aero-billing`（独立的结算 crate，隔离支付网关依赖）

```
aero-billing/
├── Cargo.toml          # 依赖：stripe-rust（可选）、serde、chrono、aero-storage（仅数据模型）
├── src/
│   ├── lib.rs
│   ├── gateway.rs      # PaymentGateway trait
│   ├── gateway/
│   │   └── stripe.rs   # Stripe 实现（使用 stripe-rust crate）
│   ├── webhook.rs      # 入站支付回调 handler（签名验证 + 事件路由）
│   ├── subscription.rs # 订阅状态机
│   └── payout.rs       # 创作者结算
```

**PaymentGateway trait 设计**：

```rust
#[async_trait]
pub trait PaymentGateway: Send + Sync + 'static {
    /// 创建支付意图（一次性礼物）
    async fn create_payment_intent(
        &self, amount_cents: u32, currency: &str, metadata: HashMap<String, String>,
    ) -> Result<PaymentIntent, PaymentError>;

    /// 创建订阅
    async fn create_subscription(
        &self, customer_id: &str, price_id: &str, metadata: HashMap<String, String>,
    ) -> Result<SubscriptionInfo, PaymentError>;

    /// 取消订阅
    async fn cancel_subscription(
        &self, subscription_id: &str,
    ) -> Result<(), PaymentError>;

    /// 验证 webhook 签名
    async fn verify_webhook(
        &self, payload: &[u8], signature_header: &str,
    ) -> Result<WebhookEvent, PaymentError>;

    /// 查询支付状态
    async fn get_payment_status(
        &self, payment_id: &str,
    ) -> Result<PaymentStatus, PaymentError>;
}
```

**数据模型变更**：

```sql
-- 订阅扩展
ALTER TABLE creator_subscriptions ADD COLUMN stripe_subscription_id TEXT;
ALTER TABLE creator_subscriptions ADD COLUMN stripe_customer_id TEXT;
ALTER TABLE creator_subscriptions ADD COLUMN current_period_start TIMESTAMPTZ;
ALTER TABLE creator_subscriptions ADD COLUMN current_period_end TIMESTAMPTZ;
ALTER TABLE creator_subscriptions ADD COLUMN status TEXT NOT NULL DEFAULT 'pending_payment';
  -- pending_payment | active | past_due | cancelled | expired

-- 新增结算表
CREATE TABLE payouts (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    creator_id UUID NOT NULL REFERENCES participants(id),
    amount_cents INTEGER NOT NULL,
    platform_fee_cents INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'pending',  -- pending | processing | paid | failed
    period_start TIMESTAMPTZ NOT NULL,
    period_end TIMESTAMPTZ NOT NULL,
    paid_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- 新增支付流水表（审计）
CREATE TABLE payment_transactions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    gateway TEXT NOT NULL,
    gateway_transaction_id TEXT NOT NULL UNIQUE,
    type TEXT NOT NULL,  -- subscription_creation | gift | refund
    amount_cents INTEGER NOT NULL,
    currency TEXT NOT NULL DEFAULT 'usd',
    status TEXT NOT NULL,
    metadata JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

**订阅状态机**：

```
                    ┌─────────────┐
     subscribe() → │ pending_    │ ← checkout.session.completed
                    │ payment     │ → active (无 webhook 超时清理)
                    └──────┬──────┘
                           │ webhook: checkout.session.completed
                           ▼
                    ┌─────────────┐
                    │   active    │
                    └──────┬──────┘
                           │ 续期扣款失败
                    ┌──────▼──────┐
                    │  past_due   │ ← 7天宽限期后自动 cancel
                    └──────┬──────┘
                           │ 用户取消 / 宽限期结束
                    ┌──────▼──────┐
                    │ cancelled   │
                    └─────────────┘
```

**对现有系统的影响**：
- `POST /api/creators/:id/subscribe` 的语义需要变化：从"立即激活"变为"返回支付 URL"
- 现有 `creator_subscriptions` 表中的 `active: true` 记录需要数据迁移（转换为 `status: active` + 设置 `current_period_start` 为创建时间）
- 礼物端点 `POST /api/streams/:id/gift` 保持免费模式，新增付费礼物 endpoint（避免破坏现有客户端）
- 工作区配置中新增 `platform_fee_bps` 字段（默认 0，平台抽成）
- **重要**：在 Stripe 集成未完成前，保留现有的"免费订阅"语义作为 `free_subscribe()`，付费订阅走新路径。两者在 UI 上体现为"关注"和"订阅"的差异

---

## 3. 接口设计建议

### 3.1 新增抽象层的接口原则

**原则一：同步层与异步层分离**

在 `ImService` 中，消息存储和实时投递应拆分为同步/异步两个阶段：

```rust
// 当前（耦合）
impl ImService {
    pub async fn publish_room_event(...) -> Result<()> {
        // 1. 存储消息到 PG
        // 2. 发布到 NATS
        // 3. 如果第2步失败，回滚第1步  ← 问题在这里
    }
}

// 推荐（解耦）
impl ImService {
    /// 同步：存储消息 + 写入 outbox（整个操作在同一 PG 事务中）
    pub async fn store_message(...) -> Result<MessageId> {
        // 1. 存储消息到 PG
        // 2. INSERT into message_outbox
        // 提交事务
    }
}

// 新增异步 drain 任务（在 boot/background.rs 中 spawn）
async fn run_outbox_drainer(bus: EventBus, pool: PgPool) {
    // 定期：SELECT * FROM message_outbox ORDER BY created_at LIMIT 100 FOR UPDATE SKIP LOCKED
    // 逐一：bus.publish → DELETE FROM message_outbox WHERE id = ?
    // 重试上限（MAX_ATTEMPTS=5）后标记为 failed 并告警
}
```

**原则二：支付网关应视为可插拔抽象**

PaymentGateway trait（见方向 E 的接口设计）确保了 Stripe 是第一个实现而非唯一实现。好处：
- 可在测试环境中使用 `MockGateway`（不依赖真实 webhook）
- 可替换为 Paddle/LemonSqueezy（如果特定市场需要）
- 可在开发环境使用 `NullGateway`（支付始终成功——用于本地开发）

**原则三：客户端 WebRTC 质量管理应为独立模块**

`MediaStatsCollector` 和 `MediaQualityAdaptor` 应是独立于 `calls.js` 信令逻辑的模块，通过事件总线通信：

```javascript
// 信令模块（calls.js）——不直接调用质量模块
pc.addEventListener('connectionstatechange', ...);

// 质量模块（media-adapt.js）——不直接调用信令
collector.onStats((stats) => {
  adaptor.ingest(stats);
  // 降级决策通过 setParameters 直接作用在 sender 上
});
```

两者的桥梁是 `RTCPeerConnection` 对象本身——信令模块创建它，质量模块消费它的 stats 和控制 sender。

### 3.2 向后兼容策略

| 变更内容 | 兼容策略 | 过渡期 |
|---------|---------|--------|
| WS 帧 `msg:read` 增加 `device_id` | 可选字段，缺失时行为不变 | 永久 |
| `creator_subscriptions.status` 从 `bool` 改为 `enum` | `active: true` → `status: 'active'` 自动迁移 | 一次性迁移 |
| Redis presence key 从 `presence:{pid}` 改为 `presence:{pid}:{did}` | 写入两处（旧+新），读取优先新 | 持续直至旧 key 自动过期 |
| REST `POST /api/streams/:id/gift` 现有免费模式 | 保留，新增 `POST /api/streams/:id/paid-gift` | 永久 |
| `publish_room_event` 返回值不变 | 内部改为 outbox 模式，对外保持相同 API | 无感知变化 |
| `/health`/`/ready` 响应格式 | 新增 `dependencies` 字段，旧字段保留 | 永久 |

### 3.3 是否需要新的 BFF 层

**不建议引入 BFF（Backend For Frontend）层**。当前 Axum gateway 已足够处理 HTTP/WS/Media 协议。引入 BFF 的认知负载（额外部署、额外延迟、额外认证模型）在当前阶段不匹配收益。

但如果未来需要以下场景，可重新考虑：
- 移动端原生 SDK 接入（需要不同响应结构）
- 第三方开发者平台的 API gateway 与 WS gateway 分离
- 每个客户端生态需要不同的聚合 API

---

## 4. 技术选型

### 4.1 新依赖引入评估

| 依赖 | 用途 | 评估 | 建议 |
|------|------|------|------|
| `stripe-rust` | Stripe 支付网关 Rust SDK | 成熟，官方维护，1.8K GitHub stars，社区活跃 | **引入**（可选依赖，gateway trait 后只需在 `aero-billing` 中依赖） |
| hls.js `dist` 构建优化 | 可选的 HLS 播放优化（如 `audioTrack` 切换） | 已加载 CDN | **不引入**——当前 CDN 加载模式对静态资源缓存友好 |
| `BroadcastChannel` polyfill | Safari/Firefox 兼容性 | 原生支持已足够（Safari 12.5+, Firefox 54+） | **不需要 polyfill**——降级到无协调模式即可 |
| Chaos engineering 工具（如 `toxiproxy`） | 降级路径测试 | 写入集成测试用 | **测试工具，不入产品依赖**——引入测试辅助 crate |
| CDN 服务商 SDK（CloudFront / Cloudflare） | CDN 预热/Purge API | 视最终选择的 CDN 而定 | **延迟到方向 C 的实施阶段再决定** |

### 4.2 自建 vs 采购决策

| 能力 | 自建 | 采购 | 建议 |
|------|------|------|------|
| **支付网关** | 自行处理 PCI DSS 合规 → 不可行 | Stripe/Paddle（合规外包） | **采购 Stripe**（开发者体验最优，支持 135+ 货币，PayPal 可后续追加） |
| **CDN** | 自行部署边缘节点 → 高成本 | CloudFront / Cloudflare / Fastly | **采购 Cloudflare**（亚太边缘节点丰富，与 HLS 延迟要求匹配） |
| **WebRTC 质量采集与统计** | 客户端 getStats + 服务端聚合 | 第三方 RTC 监控平台 | **自建**——这是核心差异化能力，且有成熟规范（W3C GetStats API） |
| **跨设备同步协议** | 基于已有数据结构 | 无现有匹配方案 | **自建**——架构上已有 receipts 表支持 participant 维度 |
| **Chaos testing** | 集成测试框架 | Gremlin 等商业工具 | **自建**（集成测试套件），工具辅助 |

### 4.3 架构风险引入

每个新方向引入的架构风险：

| 方向 | 引入的风险 | 缓解措施 |
|------|-----------|---------|
| A（发布-存储解耦） | outbox 表变成新瓶颈；drain 任务重启时的事件顺序 | outbox 有索引覆盖；drain 前先读 max seq 确保顺序 |
| B（WebRTC 质量自适应） | 降级决策引入的客户端复杂度；Firefox 的 setParameters 不兼容 | 异常捕获 + fallback；在 canary 用户中灰度 |
| C（跨设备同步） | 设备状态管理的复杂度；设备幽灵（断电登出不通知） | 设备心跳超时自动清除；session 吊销联动 |
| D（退化矩阵） | DegradationManager 变成单点故障；判断抖动导致频繁切换 | 状态去抖（debounce）；DegradationManager 无锁只读路径 |
| E（支付结算） | 支付网关 API 变化（Stripe breaking change）；webhook 重放攻击 | Webhook 签名验证严格化（timestamp + tolerance）；crate 版本锁定 |

**最大的单一风险**：方向 A（发布-存储解耦）和方向 D（NATS 故障隔离）是同一个核心能力（outbox pattern）的两面。这个变更涉及消息发送的核心路径，必须配备全面的集成测试（模拟 NATS 故障、outbox 重试、重启后的 outbox 恢复）。

---

## 5. 实施路线图

### 5.1 优先级排序

```
P0（当前 sprint / 下一 sprint）：不可推迟的架构风险
├── 方向 A：发布-存储解耦（NATS 故障隔离）
│   ├── outbox 表建立 + drain 任务
│   ├── publish_room_event 改造
│   └── 集成测试（NATS 模拟故障）
│
├── 方向 B-Step 1：客户端 getStats 管线（方向 B 的基础）
│   ├── MediaStatsCollector 模块
│   ├── 每秒采集 + 日志输出（不做自动降级，先看数据）
│   └── getStats → WS 帧上报服务端采集（可选）

P1（后续 2-4 sprints）：核心功能差距
├── 方向 B-Step 2：自适应编码参数调节
│   ├── MediaQualityAdaptor 模块
│   ├── scaleResolutionDownBy / maxBitrate / maxFramerate 调节
│   └── 灰度发布（4G 用户优先启用）
│
├── 方向 C-Step 1：同浏览器多 Tab 协调（BroadcastChannel）
│   ├── broadcast-channel.js（50 行，高回报）
│   └── 已读游标 max 聚合
│
├── 方向 D-Step 1：DegradationManager + 结构化 /ready
│   └── 不包含 NATS 解耦（已在方向 A 中覆盖）
│
├── 方向 E-Step 1：Stripe Checkout 集成（最小可行支付）
│   ├── PaymentGateway trait + Stripe 实现
│   ├── POST /api/billing/checkout → Stripe URL
│   └── Webhook 端点 + subscription 激活

P2（后续路线图）
├── 方向 C-Step 2：跨设备已读同步
│   ├── receipts 表的 device_id 感知
│   └── push_bot 跨设备去重
│
├── 方向 C-Step 3：设备清单 + 设备感知 presence
│
├── 方向 D-Step 2：退化矩阵文档化 + 各 handler 接入
│
├── 方向 E-Step 2：付费礼物 + 创作者结算
│
├── 方向 3（原文）：CDN 与 HLS 分发优化
```

### 5.2 阶段划分与里程碑

```
Phase 1（2-3 sprints）——核心风险消除
├── 里程碑 M1：消息发布-存储解耦完成
│   ├── outbox 表 + drain 任务上线
│   ├── 集成测试覆盖 NATS 故障场景
│   └── 生产观察 7 天（无异常）
│
├── 里程碑 M2：getStats 采集管线上线
│   ├── MediaStatsCollector 模块上线
│   ├── 采集数据后日志输出（观察模式）
│   └── 决策口径：是否启动自动降级

Phase 2（3-4 sprints）——核心功能补齐
├── 里程碑 M3：自适应编码降级上线
│   ├── MediaQualityAdaptor 在 4G 用户灰度上线
│   └── 通话断线率指标改善
│
├── 里程碑 M4：同标签页协调 + Stripe Checkout
│   ├── BroadcastChannel 上线（解决"红点幽灵"）
│   └── Stripe 集成上线（订阅可支付）
│
├── 里程碑 M5：DegradationManager + 结构化健康接口
│   ├── /ready 返回结构化 JSON
│   ├── DEGRADATION_MATRIX.md 文档化
│   └── 运维 dashboard 接入降级状态

Phase 3（2-3 sprints）——体验深化
├── 里程碑 M6：跨设备已读同步
│   ├── device_id 感知已读游标
│   └── 推送去重
│
├── 里程碑 M7：设备清单 + 设备感知 presence
│   ├── 设备管理 UI
│   └── presence 中显示设备类型
│
├── 里程碑 M8：CDN + HLS 防盗链
│   ├── HLS CDN 对接
│   └── signed token
```

### 5.3 风险点与缓解策略

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| **方向 A 的 outbox drain 变为瓶颈** | 中 | 高 | outbox drain 采用批量 + 并发控制；监控 outbox 积压长度；设置告警 |
| **getStats 采集在低端设备增加 CPU 负载** | 中 | 中 | Adaptive polling（2s 正常 / 500ms 波动期）；增加采集性能监控 |
| **setParameters 跨浏览器兼容性问题** | 低 | 中 | try-catch + fallback；在 canary 测试用户中验证 |
| **Stripe webhook 交付延迟导致订阅激活滞后** | 低 | 低 | Webhook 重试 + 客户端主动轮询状态（备用路径） |
| **跨设备同步增加消息延迟** | 低 | 低 | 游标聚合在本设备可独立推进（不强依赖跨设备同步完成） |
| **DegradationManager 本身的单点可用性** | 低 | 高 | DegradationManager 设计为纯读取的不健康状态本身就是 "Degraded"；各 handler 的降级检查有本地缓存（5s TTL） |
| **方向 A-方向 D 的重叠导致实施依赖** | 中 | 中 | 方向 A 的 outbox 独立实现，方向 D 的 DegradationManager 也独立。两者可并行开发 |

### 5.4 各方向间的交叠图

```
方向 A（发布-存储解耦）
  ↓
  为方向 D（退化矩阵）提供 NATS 故障隔离的架构基础
  ↓
方向 D 的 DegradationManager 独立于方向 A——可并行

方向 B（WebRTC 质量）
  └── 完全独立——客户端工作，无后端依赖

方向 C（跨设备同步）
  └── 依赖方向 A 的 outbox？（不需要——已读游标独立于消息投递）
  └── 与方向 E 的推送去重关联（推送去重需要设备清单）

方向 E（支付结算）
  └── 与方向 A/D 无直接依赖
  └── 依赖 devops：Stripe webhook 需要在生产环境配置域名和 HTTPS
```

**建议的实施并行度**：
- **Sprint 1-2**：方向 A（outbox） + 方向 B-Step 1（getStats）+ 方向 C-Step 1（BroadcastChannel） → **三人并行，互不干扰**
- **Sprint 3-5**：方向 B-Step 2（自适应编码） + 方向 E-Step 1（Stripe Checkout） + 方向 D-Step 1（DegradationManager） → **三人并行**
- **Sprint 6+**：方向 C-Step 2/3 + 方向 E-Step 2 + HLS/CDN（方向三）

---

## 总结

这份分析指出了 Aero IM 在架构成熟度演进路径上的五个关键断层点。从架构角度看，最重要的单一改进是**发布-存储解耦（方向 A）**——它消除了一个可触发的连锁故障模式（NATS 故障→PG 事务回滚），并为退化矩阵策略奠定了基础。

从产品价值角度看，**WebRTC 质量自适应（方向 B）**和**支付管线（方向 E）**提供了最直接的业务收益——前者减少通话断线率（留存），后者激活创作者变现（收入）。

推荐的实施节奏是：**先修核心架构风险（A），再补客户端体验缺口（B、C），最后完善商业和运营基础设施（E、D）**。所有方向均可按"第一阶段先做可观测/可监控的埋点，第二阶段再启用自动化决策"的节奏渐进推进，降低单次上线风险。

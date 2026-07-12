Now I have a thorough understanding of the project's architecture, existing analyses, and codebase. Let me produce the architectural analysis.

---

# 架构分析报告：Aero IM 平台

> **基于**：全量源码扫描 + 既有 5 方向缺口分析交叉验证结果 + 设计 Spec + ROADMAP
> **视角**：资深架构师
> **日期**：2026-07-12

---

## 1. 架构评估

### 1.1 当前架构的优势

本次交叉验证再次确认了 Aero IM 拥有**远超早期阶段产品**的架构质量。以下几个设计决策在分析中反复被证明是正确的：

**事件驱动的水平扩展骨架**。NATS JetStream 作为跨实例事实源 + 进程内 `Hub` 扇出的两层设计，是系统最坚固的抽象。`publish_room_event` → NATS → `run_bus_listener` → `Hub::fan_out_raw` 的管线在方向五验证中被确认：NATS publish 失败**不会回滚 PG 事务**（`warn!` + 指标递增，不 propagate），发布-存储已经解耦。这是一项常被误诊为缺口的已就位能力——比许多生产系统更早意识到了这层分离。

**Crate-per-feature 的模块边界**。16 个 crate 自下而上的依赖方向清晰（`aero-common` 叶子 → 基础设施 crates → 业务 crates → `aero-server` 组合）。方向二的验证中，支付相关代码（`creator_subscription.rs`, `subscription_tier.rs`）被严格限制在 `aero-storage` 和 `aero-im-core` 内，没有泄漏到网关层——这意味着引入 Stripe 时，接口面很清晰。

**AI 作为一等公民而非事后插件**。`AiWorker` 的预算系统（`CostBudget` + `KeyedCostBudget` + per-kind 权重）、双路检索（Voyage 非对称嵌入 + FTS 经 RRF 融合）、异步审核管线——这些都不是 demo 级实现。方向一交叉验证确认了 `AiWorker` 的 defer 循环问题是一个**正确性 bug** 而非架构缺失，恰恰说明 AI 管线已经成熟到值得深究正确性了。

**被动基础设施 + 可观测**。Prometheus 指标、OTLP trace、健康检查、请求关联 ID——这些在 800+ 测试覆盖的代码库中不是装饰，是实际运行的 seam。

### 1.2 关键局限性

交叉验证暴露了以下几类架构债：

**第一类债：客户端贫血（Client Anemia）**。`web/` 是一个零依赖 ES2020 SPA，在功能的广度上令人印象深刻（hls.js / RTCPeerConnection / SpeechRecognition / polls.js / notifications.js），但架构上处于**原始状态**：

- 无状态管理（`context.js` 用全局变量 `export const state = { ... }`）
- 无离线支持（断连 = 丢失未发送消息，重连 = 全量 backfill）
- 无跨标签页协调（`BroadcastChannel` 零出现）
- 无组件模型（render 函数直接操作 DOM）
- 无 Service Worker（PWA 清单零出现）

这是 MVP 阶段的合理取舍，但**已经成为产品化最大的架构瓶颈**。方向四（跨设备协同）和方向一（WebRTC 质量自适应）的几乎所有缺口都根源于此——不是服务端不支持，是客户端没有能力消费。

**第二类债：媒体面与生产就绪之间的 seam**。分析中反复出现的「已建+单测，未接线」模式（`call_bridge_supervisor` 的 `ensure_egress`、`SfuMediaSession::run`、真实 ICE/DTLS 握手）暴露了一个架构现实：**媒体面管线在结构上是正确的，但缺乏端到端的生产验证**。这不是「未完成」，而是「缺真对端」——CI 中无法跑浏览器/ffmpeg/OBS。战略上需要决定：是继续深化媒体面（联调），还是先收口其他方向。

**第三类债：降级策略的碎片化**。方向五的修正结论值得重复：NATS-PG 解耦已存在，但系统对基础设施故障的应对是**按模块各自判断**的——`presence` fail-open、`rate_limit` fail-open、`cache` 回落 PG、PG 本身无降级（直接 500）。缺乏一个 `DegradationManager` 或等价的结构化声明。这在单节点部署中不致命，但在多租户 SaaS 中会成为运维黑洞。

**第四类债：零停机发布能力的缺失**。方向四（ROADMAP 版本）和方向五（分析版本）分别从不同角度触及了这个问题：157 个 up-only 迁移、无 schema 版本兼容层、无蓝绿部署适配。这在不中断服务的前提下无法做大表变更（消息表分区被明确标记为维护窗口操作）。

### 1.3 设计决策评估

| 决策 | 评估 | 理由 |
|------|------|------|
| NATS 作为事件总线 | ✅ 正确 | 发布-存储已解耦（方向五验证），水平扩展脊柱就位 |
| PG + Redis + NATS 三存储 | ✅ 正确 | 各司其职：持久化 + 缓存/presence + 实时扇出 |
| 无 E2E 加密状态机 | ✅ 正确 | 产品阶段未到，scaffold 已留够 |
| 无联邦协议 | ✅ 正确 | 单租户起步，联邦是 P3+ |
| 客户端零框架 SPA | ⚠️ 阶段性正确 | MVP 阶段合理，但已成产品化瓶颈 |
| 媒体面选 str0m | ✅ 正确 | 纯 Rust DTLS/SRTP，无 C 依赖，与项目技术栈一致 |
| 无支付集成 | ⚠️ 已到转折点 | `price_cents` 字段存在但零金额操作（方向二验证），变现能力是空壳 |
| up-only 迁移 | ❌ 技术债 | 157 个迁移无 rollback，企业 SLA 无法承诺 |
| webhook 仅 Message variant | ❌ 产品缺陷 | 基础设施为全事件类型而建，调度只用了 1/15（方向一 2026-07-10 分析） |

---

## 2. 扩展方向

基于交叉验证结果和架构依赖关系，我提出以下 5 个扩展方向，与既有分析的 5 方向部分重叠但视角不同——我更关注**架构层的基础设施投资**而非产品功能缺口。

### 方向 A：统一客户端架构层（P0）

**为什么需要**：方向一（WebRTC 质量自适应）、方向四（跨设备协同）的根因都在客户端。没有状态管理、离线支持、跨标签页协调，服务端再强的能力也无法转化为 UX。这是**产品化必须跨越的门槛**，不是可选项。

**核心挑战**：
- 现有 SPA 是零依赖的，引入框架/库意味着重写大量 render 逻辑。渐进式迁移比重写更现实
- WebRTC `getStats` + `RTCRtpSender.setParameters` 需要在媒体 Pipeline 中插入质量监控点，与现有 `calls.js` 的简易架构冲突
- `BroadcastChannel` 跨标签页状态同步 + Service Worker 离线缓存是独立的关注点，不能耦合进业务逻辑

**预期架构变更**：
- 引入轻量级状态容器（非框架，而是 ~200 行的 `createStore` 模式），将 `context.js` 的全局变量替换为订阅-通知模型
- 新增 `web/sw.js` Service Worker，托管 HLS 切片缓存 + 离线消息队列 + 推送通知
- 新增 `web/sync.js` 跨标签页协调层（`BroadcastChannel` + `localStorage` 兜底）
- 在 `calls.js` 中插入 `getStats` 采样循环 + `setParameters` 自适应降级

**对现有系统的影响**：
- Service Worker 需要服务端配合（`Cache-Control` 头策略更新，方向三联动）
- 跨标签页状态同步需要权衡与方向四（跨设备同步）的关系——先做同设备跨标签，再做跨设备
- **风险**：渐进式引入比一次性重写更安全，但需要明确界定第一阶段范围（避免 scope creep）

### 方向 B：支付结算基础设施（P0）

**为什么需要**：方向二验证确认了 `price_cents` 存在但零金额操作。创作者订阅、礼物系统、预测系统都在用虚拟点数，没有真实变现能力。这是产品商业模型的核心缺口。

**核心挑战**：
- Stripe webhook 的幂等处理：`payment_intent.succeeded` 可能重复投递，必须用 idempotency key 保护
- 免费订阅与付费订阅的共存：当前 subscribe 是零金额的「关注」语义，引入付费后需要保留免费路径
- 订阅状态变更的实时通知：PC 端购买 → 手机端立即看到（方向四联动）
- 税务/退款/争议处理：Stripe 只处理支付网关部分，业务层需要处理全额退款、部分退款、订阅暂停

**预期架构变更**：
- 新增 `aero-billing` crate（或并入 `aero-im-core`）：`PaymentRepo`（存储 payment_intent/charge/refund）、`SubscriptionOrchestrator`（管理 Stripe订阅 ↔ 本地订阅的映射）、`WebhookHandler`（Stripe 事件处理）
- 新增 `billing_webhook` bus consumer（critical path——要求 PG 可用，可容忍 Redis 故障，独立于其他降级状态）
- RoomEvent 新增 `SubscriptionStatus` variant，用于跨设备同步订阅状态
- 迁移：`payment_intents`、`charges`、`subscription_items` 表

**对现有系统的影响**：
- `creator_subscription.rs` 的 `subscribe` 方法需要重写：从纯 INSERT 变为 Stripe 交互
- 现有 `price_cents` 字段语义不变，但引入 `currency`（ISO 4217）和 `interval`（month/year）
- **关键风险**：Stripe webhook 的端到端测试依赖真实密钥，无法在 CI 中 hermetic 跑——需要 `FakeStripe` 或 stripe-mock 容器

### 方向 C：CDN 与内容分发层（P1）

**为什么需要**：方向三验证确认 HLS 通过 `ServeDir` 直接暴露，无 `Cache-Control` 策略、无鉴权、无 SRI。如果直播已开始生产使用，这是带宽成本和播放体验的双重问题。即使直播仍在 beta，架构上预留 CDN seam 比事后重构便宜得多。

**核心挑战**：
- HLS 鉴权的设计选择：signed URL（token 绑定 participant + stream，短期过期）vs referer 头 vs 全局 secret？推荐 signed URL，因为同时满足防盗链和跨设备共享（方向四联动）
- 播放列表（`index.m3u8`）和切片（`.ts`）的缓存策略不同：m3u8 需要 `no-cache` 或极短 `max-age`（秒级），ts 可以较长缓存（小时级）
- 附件 blob 的 CDN 回源：现有 `BlobStore` 走 Rust 代理回源，CDN 部署后需要支持 CDN 回源到 S3/MinIO 而非 Rust
- HLS 直播的低延迟 vs CDN 缓存：常规 CDN 增加 10-30s 延迟，需要配置 chunked transfer 或 LL-HLS

**预期架构变更**：
- 新增 `serve.rs` 的 `Cache-Control` 策略层（按路径模式匹配：`/hls/*.m3u8` → `no-cache`，`/hls/*.ts` → `max-age=86400`，`/api/blobs/*` → `max-age=31536000, immutable`）
- 新增 signed URL 中间件（查询参数签名 + 过期时间，用 HMAC-SHA256）
- `BlobStore` trait 新增 `presigned_get_url(ttl)` 方法（S3 实现直接返回 S3 presigned URL，LocalFs 回落回现有代理模式）
- `web/index.html` 为 hls.js CDN 加载添加 `integrity` 属性

**对现有系统的影响**：
- 低风险——不改变任何业务逻辑，只改变 HTTP 响应头和 URL 生成策略
- `BlobStore::presigned_get_url` 是 trait 新增方法，需为既有实现（LocalFs）提供默认回落
- HLS 鉴权需要客户端携带 token——现有 `<video>` 元素直接加载 `/hls/{id}/index.m3u8` 的模式需要改为通过 JS 获取 signed URL

### 方向 D：系统降解矩阵与混沌工程就绪（P1→P2）

**为什么需要**：方向五修正确认了 NATS-PG 已经解耦，但系统对故障的应对碎片化。随着多实例部署和真实流量，运维团队需要一个结构化的降解声明，而不是翻代码找每个模块的 fallback 行为。

**核心挑战**：
- 降解矩阵需要列出每个关键操作对 PG / Redis / NATS / BlobStore / AI Provider 的依赖及故障模式
- PG 故障目前无降级（直接 500）——是否引入只读模式是一个架构决策
- 降解边界测试需要引入 chaos testing 工具（如 Toxiproxy），这是新增的基础设施成本
- 告警聚合：多个降解同时发生时，运维需要一个统一的系统健康视图而非 6 个独立告警

**预期架构变更**：
- 新增 `DegradationManager`（或集成进既有 `AppState`）：一个 `EnumMap<Dependency, HealthStatus>` 运行时状态 + `on_degraded` 回调注册
- 每个故障点统一调用 `deg_manager.record_failure(Dependency::Redis)`，而非各自 `warn!`
- 新增 `/health/degradation` 端点，暴露当前降解状态
- 引入 Toxiproxy 或等价工具用于 CI 中的混沌测试（env-gated，不影响 hermetic 测试）

**对现有系统的影响**：
- 低风险——新增基础设施，不改变既有 fallback 逻辑，只包装它们
- 初期可以只做监视器（passive），不做主动降级（active）——先测量现有 fallback 覆盖率，再决策哪些需要统一治理
- **ROI 评估**：在单节点部署中价值有限，在多租户多实例场景中价值很高。如果当前仍是单节点部署，可放 P2

### 方向 E：零停机发布与 Schema 版本管理（P2）

**为什么需要**：157 个 up-only 迁移。一次 schema 事故 = 全量回滚到备份 + 数据丢失窗口。企业 SLA 99.99% 无法承诺。

**核心挑战**：
- down migration 不是简单的 `DROP TABLE`——需要处理数据回流（如分区合并、列类型回退）
- 零停机迁移需要兼容新旧两个 schema 版本：部署 v2（代码兼容 v1 schema）→ 运行迁移 → 部署 v3（代码依赖 v2 schema）。现有 sqlx 编译期校验使得这种模式需要运行时 SQL 而非编译时宏
- 消息表分区（shadow migration `0148` 未 cutover）需要选择：在线迁移（pg_repack/pg_partman）vs 维护窗口

**预期架构变更**：
- 为关键迁移编写 `down.sql`（至少最近的 10-20 个迁移，历史迁移风险较低可不追溯）
- 新增 `aero-cli migrate:down N` 命令
- 建立 schema 兼容层：为经常查询的大表（messages, rooms, participants）建立视图或函数封装，使应用层不直接依赖表结构
- 蓝绿部署的 DNS/负载均衡切换流程文档化

**对现有系统的影响**：
- down migration 的编写成本不低——每个迁移需要理解数据依赖关系
- 运行时 SQL（`sqlx::query_scalar` 而非 `sqlx::query!`）绕过编译期校验，需要在测试中补覆盖
- **优先建议**：先完成消息表分区 cutover（shadow 迁移转生产），再投入 down migration。分区是更紧迫的扩展性瓶颈

---

## 3. 接口设计建议

### 3.1 关键抽象层

当前架构在以下位置已经建立了正确的抽象边界，建议保持并强化：

**`EventBus` trait（`aero-bus/traits.rs`）**。方向五修正中确认了 `publish` 方法的 warn-only 语义是正确的。建议下一步为 `publish` 添加可选 `headers` 参数（W3C `traceparent`），但保持向后兼容：

```
// 当前
pub async fn publish(&self, subject: &str, payload: &[u8]) -> Result<()>;

// 建议
pub async fn publish(&self, subject: &str, payload: &[u8], headers: Option<HeaderMap>) -> Result<()>;
```

现有调用方传 `None`，零迁移成本。

**`BlobStore` trait**。当前抽象（LocalFs / S3BlobStore）是正确的。建议新增 `presigned_get_url(ttl: Duration) -> Option<String>` 方法，默认返回 `None`（不使用 CDN），S3 实现返回 presigned URL。这保持了 trait 的 seam 性质——不强制所有实现支持 CDN。

**`LiveIngest` trait**。RTMP / WHIP / SRT 三种摄入共用同一 trait 是良好的抽象。golive_bot 的三种摄入通知路径（WHIP 内联 publish vs RTMP/SRT 经 `go_live` bus-free 钩子）是合理的——不强行统一。建议为 `LiveIngest` 添加 `stream_stats()` 方法暴露实时码率/帧率，供方向一的 SFU 质量决策使用。

### 3.2 需要新增的抽象

**支付抽象层**。建议引入 `PaymentGateway` trait：

```rust
#[async_trait]
pub trait PaymentGateway: Send + Sync {
    /// 创建支付意图（一次性支付）
    async fn create_payment(&self, amount: Money, metadata: Value) -> Result<PaymentIntent>;
    /// 创建订阅
    async fn create_subscription(&self, plan_id: &str, customer_id: &str) -> Result<Subscription>;
    /// 取消订阅
    async fn cancel_subscription(&self, sub_id: &str) -> Result<()>;
    /// 处理 webhook 事件（统一入口，内部按 event type 分发）
    async fn handle_webhook(&self, payload: Bytes, signature: &str) -> Result<WebhookEvent>;
    /// 退款
    async fn refund(&self, payment_intent_id: &str, amount: Option<Money>) -> Result<Refund>;
}
```

第一实现是 Stripe（`stripe::StripeGateway`），预留 `FakeGateway` 用于测试。后续可以扩展 Paddle / LemonSqueezy。

**降解声明层**。建议引入声明式宏或结构体来定义每个操作的依赖：

```rust
// 理想形式
#[degradation(
    pg = "required",
    redis = "tolerated(30s)",
    nats = "tolerated(stale)",
    ai = "optional"
)]
async fn send_message(...) -> Result<...> { ... }
```

但 Rust 的过程宏可能过度设计。一个更务实的方案是引入 `DependencySet` 结构体 + 运行时检查：

```rust
pub struct DependencySet {
    pub requires_pg: bool,
    pub requires_redis: bool,
    pub requires_nats: bool,
    pub requires_ai: bool,
    pub degraded_fallback: DegradedFallback, // enum: Reject / Warn / UseStale
}
```

每个关键操作在启动时注册其 `DependencySet`，`DegradationManager` 在依赖故障时查询哪些操作受影响。

### 3.3 向后兼容策略

**NATS 事件 schema 演进**。当前 `RoomEvent` / `StreamEvent` 使用 `serde(tag = "kind")`。方向二的补充发现确认了 `kind` 标签撞名陷阱已用 `#[serde(rename)]` 规避。新增 variant 时，需要：

1. 向 `RoomEvent`/`StreamEvent` enum 添加新 variant
2. 旧客户端收到不识别的 variant → serde 会跳过（`deny_unknown_fields` 未启用）
3. web 端 `ws.on('msg:..')` 处理新帧——确保新帧不导致客户端崩溃

**API 版本策略**。目前所有 REST API 在 `/api/` 下无版本前缀。建议保持无版本（内部工具），但在引入破坏性变更时走 URL 路径版本（`/api/v2/`）而非 header 版本。当前阶段远未需要 v2，但应在 `routes.rs` 中预留版本挂载点。

---

## 4. 技术选型

### 4.1 新增依赖评估

以下引入基于交叉验证确认的缺口，按推荐优先级排列：

| 候选技术 | 用于 | 推荐 | 理由 |
|---------|------|------|------|
| **Stripe** (Rust crate) | 支付处理 | **引入** | 方向二确认变现缺口。Stripe 是 IM/creator 平台的事实标准，Rust crate `stripe` 成熟度尚可，且 Stripe webhook 的幂等设计成熟 |
| **BroadcastChannel API** (浏览器原生) | 跨标签页协调 | **引入** | 方向四缺口，零依赖，浏览器原生 API |
| **Service Worker** (浏览器原生) | 离线缓存 + PWA | **引入** | 方向一/四联动，浏览器原生 API |
| **Toxiproxy** (测试工具) | 混沌测试 | **条件引入** | 方向五联动。如果走多实例部署路线则推荐，单节点部署可推迟 |
| **FFmpeg** (进程外工具) | 直播转码/缩略图 | **不引入** | 当前直播管线是纯 Rust（rml_rtmp + str0m + 真 MPEG-TS），引入 FFmpeg 破坏零 C 依赖策略。可预留 seam 但等待明确需求 |
| **Redis Cluster / KeyDB** | Redis 水平扩展 | **暂不引入** | 方向四（ROADMAP 版本）识别了 Redis 热键分片需求，但在单实例场景下 Redis 单点足够。标记为扩展点 |
| **pg_repack / pg_partman** | PG 在线分区 | **条件引入** | 方向四（ROADMAP 版本）消息表分区 cutover 时需要。如果选择维护窗口分区则不需要 |

### 4.2 自建 vs 采购决策框架

基于本项目的技术栈（纯 Rust，零 C 依赖）和团队规模，建议以下决策标准：

**自建条件**（同时满足）：
1. 核心差异化竞争力（如 AI 管线的检索融合、SFU 的选择性转发）
2. 现有架构已有 seam（如 BlobStore trait）
3. Rust 生态有基础库可组装（如 str0m 用于 WebRTC）

**采购/集成条件**（任一满足）：
1. 非差异化基础设施（支付网关、推送网关、对象存储）
2. 需要合规审计背书（Stripe 的 PCI DSS 层级）
3. RI 循环（Reinvention 陷阱）：自建需要持续投入追赶上游变更（如 Stripe API 版本更新）

**当前决策复盘**：
- WHIP/WHEP/SFU 自建 ✅ 正确——str0m 是 Rust 生态的最优选择，WebRTC 是产品的核心差异点
- SRT 自建 ✅ 正确——手写 HSv5 握手 + AES-CTR 是强差异化，Rust 生态无替代品
- 推送网关（FCM/APNs）自建 ✅ 正确——`aero-push` 的 `FakeGateway` seam 设计合理，外部服务故障不影响业务逻辑
- 支付尚未决策 ⚠️ —到了必须引入的转折点，推荐 Stripe（采购）而非自建

### 4.3 Rust 生态风险

**str0m 版本策略**。当前 `aero-live-whip` 和 `aero-live-webrtc` 各自在 `Cargo.toml` 声明 str0m 0.19。`AGENTS.md` 强调 root 无 str0m。这是一个合理的隔离策略——如果 str0m 发布破坏性更新（如 API 重命名），两个 crate 可以独立升级。但需要留意两个 crate 的 str0m 版本是否同步，否则可能出现类型不兼容。

**sqlx 编译期校验的双刃剑**。编译时 SQL 校验 → 安全性高 → 但运行时无法执行动态 SQL → 零停机迁移受限。这是一个知悉的权衡，不是待修复的 bug。引入 `sqlx::query`（非 `query!`）用于迁移兼容层是合理的技术债。

---

## 5. 实施路线图

### 5.1 优先级排序

基于交叉验证结果和架构依赖关系，调整后的优先级：

| 优先级 | 方向 | 理由 |
|--------|------|------|
| **P0** | A · 统一客户端架构层 | 方向一/四的根因，产品化必须跨越的门槛 |
| **P0** | B · 支付结算基础设施 | 变现能力是产品商业模型的空壳，商业核心缺口 |
| **P1** | C · CDN 与内容分发层 | 如果直播已生产则 P0，否则 P1。建议给架构预留 seam 即使不立刻启用 |
| **P1** | D · 降解矩阵与混沌工程 | 多实例部署前需完成，单节点部署时可降 P2 |
| **P2** | E · 零停机发布 | 企业 SLA 需求，但当前阶段可暂缓 |

### 5.2 阶段划分

**Phase 1（6-8 周）：客户端架构层 + 支付 MVP**

| 周 | 方向 A · 客户端 | 方向 B · 支付 |
|---|----------------|--------------|
| 1-2 | 引入 `createStore` 状态容器，重构 `context.js` 全局变量 | Stripe 账号 + `aero-billing` crate scaffold + `PaymentGateway` trait |
| 3-4 | `calls.js` 插入 `getStats` 采样 + `setParameters` 降级 | `POST /api/creators/:id/subscribe` 改为 Stripe 交互（保留免费路径） |
| 5-6 | Service Worker 初始化（HLS 缓存 + 离线消息队列） | Stripe webhook handler（payment_intent.succeeded + subscription.*） |
| 7-8 | `BroadcastChannel` 跨标签页已读同步 | 迁移 `payment_intents` + `charges` 表，幂等保护 |

**Phase 2（4-6 周）：CDN + 降解矩阵**

| 周 | 方向 C · CDN | 方向 D · 降解矩阵 |
|---|-------------|------------------|
| 1-2 | `Cache-Control` 策略层 + `BlobStore::presigned_get_url` | `DegradationManager` 结构体 + 依赖注册 API |
| 3-4 | HLS signed URL 中间件 + web 端 token 加载 | 逐模块 audit fallback 行为，统一注册到 DegradationManager |
| 5-6 | SRI for hls.js + 附件 CDN 回源配置 | Toxiproxy CI 集成 + 降解边界测试 |

**Phase 3（4 周，可选）：零停机发布**

| 周 | 方向 E · 零停机 |
|---|----------------|
| 1-2 | 关键迁移 `down.sql`（最近 20 个）+ `aero-cli migrate:down` |
| 3-4 | 消息表分区 cutover（shadow migration 转生产）+ schema 兼容视图 |

### 5.3 风险与缓解

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| 客户端重写范围膨胀 | 高 | 中 | 严格限定 Phase 1 范围为状态容器 + Service Worker，不做 UI 框架迁移 |
| Stripe 集成端到端测试困难 | 中 | 中 | `FakeGateway` + stripe-mock 容器 + 沙箱密钥的 smoke test |
| SFU 媒体面联调打断支付优先级 | 中 | 低 | 媒体面 seam 不阻塞支付——两者正交，并行推进 |
| 降解矩阵 audit 发现覆盖率过低 | 中 | 低 | 先 passive（监视器），再 active（统一 fallback），分步走 |
| down migration 编写成本高于预期 | 中 | 低 | 只做关键迁移的 down，历史老迁移标注为「不可回滚，需从备份恢复」 |

### 5.4 跨方向依赖处理

方向 A（客户端）和方向 C（CDN）之间有重叠——Service Worker 的 HLS 缓存策略需要 CDN 的 `Cache-Control` 头配合。建议**先做 CDN 的缓存头策略**（Phase 2）再引入 Service Worker，或者两者并行但确保缓存头先就位。

方向 B（支付）和方向 A（客户端）在订阅状态同步上有交集——PC 端购买后手机端立即看到。支付 webhook 应广播 `SubscriptionStatus` 事件到 NATS，客户端通过 `BroadcastChannel` 跨标签页同步。这要求 Phase 1 中两个方向协调这一 seam。

方向 D（降解矩阵）与方向 E（零停机发布）有间接依赖——零停机发布期间需要降解矩阵声明来定义「降级运行」的边界。建议 Phase 2 先做降解矩阵，Phase 3 再做零停机发布。

---

## 总结

这是一个**架构基础扎实、功能面宽广**的项目。交叉验证确认了核心设计决策（NATS-PG 解耦、crate-per-feature、AI 预算系统）的正确性，也暴露了从「功能丰富的 demo」走向「可规模化运营的产品」过程中最关键的瓶颈：**客户端架构层**。

五方向中，如果只能选一个投入，我推荐**方向 A（统一客户端架构层）**。不是因为支付不重要，而是因为支付、CDN、跨设备协同、WebRTC 质量自适应——这些能力的服务端 seam 都已经就位或容易添加，瓶颈在客户端没有能力消费它们。

如果选第二个，是**方向 B（支付结算）**——变现能力是空壳状态，而 `price_cents` 字段的存在意味着产品团队已经考虑了定价模型却在工程侧未闭环。补齐这个闭环的商业价值是最直接的。

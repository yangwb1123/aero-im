以下为基于 `2026-07-12-five-operational-edge-cases.md` 中揭示的 5 个运营边界问题，结合 Aero IM 整体架构的全方位架构分析。

---

# 架构分析：Aero IM 运营 Edge Cases 与技术债务评估

## 1. 架构评估

### 1.1 当前架构的优势

**事件驱动骨架正确**。系统采用 NATS JetStream 作为跨实例事实源 + 进程内 `Hub` 本地扇出的双层模型，是经过验证的实时架构模式（类 Discord / Slack 的早期架构）。该决策带来：

- **水平扩展通透**：durable consumer 加实例即水平消费，ephemeral consumer 保障直播弹幕的每个实例全量可见
- **进程内扇出解耦**：`Hub::fan_out_raw` 有界 mpsc 阻断了总线背压向业务层的传导
- **at-least-once 语义统一**：全系统 bus listener 一致性确认，避免状态机碎片化

**Crate 边界合理**。自下而上的严格依赖方向（`aero-common` → `aero-bus/storage/auth` → `aero-im-core` → `aero-server`）符合领域驱动设计的六边形架构精神，且避免了 Rust 编译期的循环依赖陷阱。

**Agent/Bot 架构先见**。将 Bot 作为一等公民（`ParticipantKind::{Human, Agent, Bot}`）并预埋总线消费者框架（agent_bot、ooo_bot、unfurl_bot 等），使得 AI 能力插拔不侵入核心 IM 逻辑。这是设计中最有远见的决策之一。

### 1.2 当前架构的局限性

五个 edge case 揭示了三个**系统级结构性缺口**：

| 缺口 | 根源 | 表象 |
|------|------|------|
| **生命周期管理缺失** | WS 连接不与进程生命周期挂钩 | 方向一：WS 排干缺失 |
| **单向数据流不完整** | 客户端→服务端的错误路径不可见 | 方向四：乐观发送无回退 |
| **可观测性偏向外围** | 外部依赖健康监控完备，内部组件盲区 | 方向五：PG 内部指标缺失 |

这三个缺口并非孤立 bug，而是**架构层面对「运营态」和「客户端状态」的重视不足**的体现。当前架构专注于「功能正确性」（消息能不能发、事件能不能扇出），但对「运营正确性」（关闭时是否优雅、退化时是否可发现）和「客户端正确性」（发送是否确认）缺少正交的基础设施抽象。

### 1.3 关键技术债

**方向二（Webhook 串行）** 是**最具破坏力的技术债**。`run_webhook_dispatcher` 中事件级串行 + 目标级串行的模型，在一个多端点 webhook 场景下，一个 429 响应就可能导致全局事件投递延迟分钟级。更关键的是，这暴露了**事件消费管线缺少通用的并发边界抽象**——对比 `moderation_bot` 已经使用了有界 mpsc + Semaphore worker，webhook 投递管线缺少了同一层防护。这不是 webhook 模块的独立 bug，而是**整个 durable consumer 基础框架缺少插件式的并发治理契约**。

**方向三（Blob 无缓存）** 是 **HTTP 资源层缺少 RESTful 成熟度**的体现。`blob_download` 仅返回 3 个响应头，不实现任何 HTTP 缓存契约（ETag / Cache-Control / Range / 内容协商），意味着 blob 层实际上是以 HTTP 为隧道传输二进制数据，而非真正**作为 RESTful 资源服务器运作**。对于一个将视频/图片/附件作为核心交互元素的 IM + 直播平台，这是一个基础设施级别的遗漏。

**方向四（乐观发送无回退）** 暴露了**客户端架构中「写路径」缺少确认契约**。`ws.sendMessage()` 返回 `boolean` 却无人检查——这是类型系统无法捕获的语义断裂。`optimisticAdd` 的 `findPendingMatch` 按文本相似度而非 `temp_id` 匹配的细节更值得警惕：即使消息发送成功，AI 审核修改内容后，`temp_id` 匹配也会失败，导致 pending 消息僵尸化。

---

## 2. 扩展方向

### 方向 A：连接生命周期管理框架（P1 · 架构基础设施）

**为什么需要**：方向一（WS 排干）和方向四（发送回退）的共同根因是**系统没有一个统一的连接生命周期契约**。当前每 WS 连接有独立的 `CancellationToken`，但该 token 不与进程生命周期、客户端状态、连接质量挂钩。

**核心挑战**：
1. **Token 派生树**：从全局 `CancellationToken` 派生 per-connection token，当前代码无此设施
2. **Drain 协议定义**：drain 阶段各层（bus listener 停新消息 → hub 排完 in-flight → WS 发 Close 帧 → 等客户端确认）的时序约束需精确定义
3. **排干期间的新消息路由**：排干期间新消息应暂存还是丢弃？——NATS 持久化保障了不会丢，但需要本地内存暂存直到 WS 关闭

**预期架构变更**：
- 引入 `ConnectionLifetimeManager`：持有「全局 cancel → 连接层 cancel → 连接 drain 完成」的层级关系
- WS 连接从 `Hub` 获取派生 token，而非各自 `CancellationToken::new()`
- 指标：`ws_draining_connections`、`ws_drain_duration_seconds`、`ws_shutdown_total`
- 客户端 `WsClient` 识别 code 1001 后走静默重连路径

**对现有系统的影响**：
- `run_socket` 签名变化：从 `close: CancellationToken` 变为 `lifetime: ConnectionLifetimeHandle`
- `Hub` 需暴露 `child_token()` 方法
- 影响范围：`ws/ws_impl/mod.rs`、`serve.rs`、`shutdown.rs`
- **风险低**，因当前 token 独立，改为派生不会破坏功能，只增加排干能力

### 方向 B：事件消费管线并发治理契约（P1 · 架构基础设施）

**为什么需要**：方向二（Webhook 串行）和 `moderation_bot` 的 mpsc+worker 模式展示了同一问题的两面——**总线消费端没有统一的并发治理层**。当前每个 durable consumer 自己实现串行/并行逻辑，没有正交的「并发边界 + 速率限制 + 断路器」可插拔抽象。

**核心挑战**：
1. **框架 vs 定制**：需设计一个足够通用的 `ConsumerPipeline<T>` trait，能够同时服务 webhook（串行消费+并行投递）、moderation（有界队列+预算约束）、push（每设备一投递）等不同并发模型
2. **断路器与队列背压的交互**：断路器打开时应触发消费暂停还是消息 requeue？
3. **预算治理泛化**：`AiWorker` 的 per-ws + global cost budget 是一个成功模式，但当前是 AI 专用。需抽象为 `ConsumerBudget` trait 供 webhook/push 等复用

**预期架构变更**：
- 在 `aero-bus` crate 中新增 `pipeline/` 模块：`ConsumerPipeline`, `DispatchStrategy::{Serial,Parallel(Semaphore)}`, `RetryPolicy`
- `BusConsumer` trait 扩展为 `async fn run(self, pipeline: ConsumerPipeline<Self::Event>)` 模式
- Webhook 重构：事件级保持串行（因果顺序），目标级 `tokio::spawn` + `Semaphore`（可配置并发度）
- per-endpoint token bucket 速率限制

**对现有系统的影响**：
- `moderation_bot`、`agent_bot`、`push_bot` 等将逐步迁移使用新 pipeline
- `webhooks.rs` 重构幅度中等（~200 行核心逻辑，需拆 dispatch+retry+breaker）
- **需谨慎**：因果顺序保证——群通话事件必须按序投递，不能因为并发引入乱序

### 方向 C：HTTP 资源层的 RESTful 成熟度提升（P1 · 基础设施/网络）

**为什么需要**：方向三揭示 blob 下载停留在 HTTP 隧道阶段。对于一个 IM 平台，附件/图片/视频是每天数万次请求的热路径。无缓存意味着：
- 带宽成本：CDN 出口费用按量计，未缓存=100% 回源
- 移动端体验：大图片每次都完整下载
- 无法前置 CDN：缺少 `Cache-Control` 导致 CDN 拒绝缓存

**核心挑战**：
1. **鉴权与缓存的冲突**：private blob（仅部分人员可访问）不能 CDN 缓存，但 public blob（头像/频道封面）可以。需要 `blobs.cache_policy` 字段区分
2. **Range 请求与 BlobStore 的流式接口**：当前 `blob_store.get(id) -> Vec<u8>` 需要扩展为 `AsyncRead` 流式读取
3. **ETag 的强/弱校验选择**：用 `sha256` 做 strong ETag（精确匹配）还是弱 ETag（等价内容）？——`meta.sha256` 已存在，直接用 strong 是合理选择

**预期架构变更**：
- `BlobStore` trait 新增 `get_stream(id, range: Option<Range>) -> AsyncRead`，原有 `get` 作为便利方法保留
- `StorageRepo::get_meta` 缓存（避免每次 304 查 DB）
- `blob_download` 路由重构：先校验 ETag → 304 短路，然后流式输出
- `blobs.cache_policy` 字段：`enum CachePolicy { Private, Public { max_age: Duration }, NoStore }`

**对现有系统的影响**：
- `routes.rs` blob_download 函数从 ~80 行扩至 ~200 行
- `BlobStore` trait 变化影响 `LocalFs` 和 `S3BlobStore` 两个实现
- `blobs` 表需新增 `cache_policy` 列（迁移）
- **正向影响**：ETag 可直接复用现有 `sha256` 字段，L1 实施仅 ~15 行 Rust（`.out.md` 已验证）

### 方向 D：消息提交确认契约（P1 · 客户端架构/协议）

**为什么需要**：方向四揭示**客户端写路径缺少协议层面的确认**。当前 `sendMessage` 调用是「发后即忘」：`ws.sendMessage()` 返回 `boolean` 只表示「WS 连接当时是开的」，不表示「消息已被服务器持久化」。`false` 被忽略使问题不可见。

**核心挑战**：
1. **ack 帧的时序**：服务器是收到消息后立即回复 ack（在 bus publish 前），还是持久化后回复？——后者语义更强但增加延迟
2. **幂等去重**：ack 超时后客户端重试，服务器需要去重——需引入客户端 `temp_id` 作为幂等键
3. **离线队列持久化**：`localStorage` 持久化重试队列 vs Service Worker Background Sync——前者实现简单但 tab 关闭后丢失，后者更可靠但需注册 Service Worker（当前 SPA 无 sw）

**预期架构变更**：
- WS 协议扩展：客户端帧统一加 `seq: u64` 或 `temp_id: String`，服务器回复 `{ type: "ack", temp_id, message_id, status: "ok"|"duplicate" }`
- `WsClient.sendWithAck()` 返回 `Promise<AckResult>`，调用方 `await`（或超时 fallback）
- 客户端状态机：`pending → acked → confirmed`（收到消息帧替换 pending）
- 重试队列：内存 `Map<temp_id, PendingMessage>` + `localStorage` 持久化 + WS `open` 事件 drain

**对现有系统的影响**：
- WS 协议扩展——是向后兼容的（新增 `temp_id` 可选字段），旧客户端不传则服务器不返回 ack
- `ws.rs` WsClient 重构幅度中等：send 返回 `Promise<bool>` → `Promise<AckResult>`
- `app.js` sendMessage 调用方需调整，但业务逻辑不变
- **高价值低风险**：`temp_id` 已在 `optimisticAdd` 中使用，只需确保传递到 WS 帧

### 方向 E：系统级可观测性内窥层（P2 · 运维/可观测性）

**为什么需要**：方向五揭示可观测性目前的覆盖是「外围健康」而非「内部健康」。DB pool 连接数正常 ≠ 消息表没有 50% 死元组膨胀。当前 `observability_gauge_samplers` 只采样外部依赖的连接状态，没有进入 PG 系统表做内窥。

**核心挑战**：
1. **查询开销**：`pg_stat_user_tables` 的查询本身是轻量的（系统视图，非锁），但大规模集群下频繁查也有累加开销。需控制采样间隔（默认 60s，可配置）
2. **死元组比率的阈值设定**：不同表的工作负载不同，死元组比率不能一刀切。消息表（高写入）允许 30%，但在配置表中 10% 就不正常
3. **报警疲劳**：死元组告警在没有授权干预手段（如 autovacuum 调参）的情况下容易变成噪声

**预期架构变更**：
- 在 `metrics_tasks.rs` 中新增 gauge sampler：`sample_pg_stats()`（60s 间隔，`Err` 留旧值）
- 指标：
  - `pg_dead_tuple_ratio{table="messages"}` — `n_dead_tup / (n_live_tup + n_dead_tup)`
  - `pg_seq_scan_total{table}` — `seq_scan` counter
  - `pg_index_scans_total{index}` — `idx_scan` per index
  - `pg_idle_in_transaction_max_seconds` — `max(now() - xact_start) WHERE state = 'idle in transaction'`
  - `pg_longest_running_query_seconds`
- Prometheus 告警规则补充：死元组比率 > 30% → warning，> 50% → critical

**对现有系统的影响**：
- 无业务逻辑变更，纯运维增强
- 采样器配置纳入 `Config` struct（`[metrics]` section），间隔可配
- **风险极低**：PG 系统视图查询只读、轻量、不涉及业务锁

---

## 3. 接口设计建议

### 3.1 关键模块接口原则

基于上述 5 个 edge case 和 5 个扩展方向，提出以下接口设计原则：

**原则一：每个连接生命周期必须有显式的状态机**

当前 WS 连接的生命周期只是隐式的：`accept → run_socket → TCP close`。需显式化为：

```
Created → Handshaken (joined room) → Draining (server cancel) → Drained (close frame sent) → Closed
                                        → Error (TCP RST) → Closed
```

`ConnectionLifetimeHandle` 应暴露 `.is_draining()`、`.on_drain(callback)`、`.deadline(timeout)` 方法。

**原则二：消费端并发治理必须正交于业务逻辑**

当前 `run_webhook_dispatcher` 的业务逻辑和投递策略耦合。应分离为：

```rust
// 业务方只定义：
struct WebhookHandler;
#[async_trait]
impl EventHandler<RoomEvent> for WebhookHandler {
    type Output = DispatchResult;
    async fn handle(&self, event: RoomEvent) -> Vec<DispatchTarget>;
}

// 管线框架负责：
ConsumerPipeline::new(handler)
    .with_strategy(DispatchStrategy::Parallel { max_concurrency: 10 })
    .with_retry(RetryPolicy::exponential_backoff(3, Duration::from_secs(1)))
    .with_rate_limit(RateLimit::per_endpoint(10, Duration::from_secs(1)))
    .run(stream);
```

**原则三：所有「写路径」返回必须携带结果类型，而非 boolean**

`ws.sendMessage()` 返回 `boolean` 是类型系统无法捕获的语义断裂。应返回：

```typescript
type SendResult = 
    | { status: "queued", temp_id: string }     // WS offline, queued for retry
    | { status: "sent", temp_id: string }       // WS online, awaiting ack
    | { status: "confirmed", temp_id: string, message_id: string }  // ack received
    | { status: "failed", reason: string };      // max retries exceeded
```

**原则四：资源层必须实现 HTTP 缓存契约**

`blob_download` 响应必须至少具备：
- `ETag`（复用 `meta.sha256`）
- `Cache-Control`（按 `blobs.cache_policy` 取值）
- `Vary: Accept`（为 WebP 内容协商预留）
- `Accept-Ranges: bytes`（大文件支持）

### 3.2 是否需要引入新的抽象层

**需要——在 `aero-bus` 中引入 `pipeline/` 抽象层**。当前 `BusConsumer` trait 只有 `run()`，没有编排能力。增加 `ConsumerPipeline` 抽象层可使：

- Webhook、moderation、push、agent 等消费端统一遵守同一套并发治理契约
- 新 bot 开发时只需实现 `EventHandler`，不需要再考虑并发/重试/速率限制
- 运维可观测：统一采集 `pipeline_queue_depth`、`pipeline_latency`、`pipeline_retries` 等指标

**不需要——引入全局事件总线抽象**。当前 NATS + Hub 双层模型已经验证正确，不需要像 Kafka Connect 那样的外围抽象。NATS 的 subject 通配符 + consumer 配置已经足够灵活。

### 3.3 如何保持向后兼容性

| 变更 | 向后兼容策略 |
|------|------------|
| WS 协议加 `temp_id` / `ack` 帧 | 可选字段，旧客户端不传则不回复 ack；旧帧不报错 |
| `BlobStore` trait 新增 `get_stream` | 保留 `get` 作为后备，`get_stream` 默认实现调 `get` |
| `blobs.cache_policy` 迁移 | `NOT NULL DEFAULT 'public'`，存量 blob 继承默认策略 |
| `ConnectionLifetimeHandle` | 接入期间 `close: CancellationToken` 继续可用，逐步弃用 |
| Webhook 管线重构 | 外部 API（配置 webhook 端点）不变，仅内部投递逻辑变化 |
| PG 系统表查询 | 纯新增 gauge，不影响任何业务路径 |

---

## 4. 技术选型

### 4.1 是否需要新的技术栈或框架

**不需要引入新框架**。以下能力通过既有技术栈即可实现：

| 需要的能力 | 实现方式（现有技术栈） |
|-----------|----------------------|
| 流式读取（Blob Range） | `tokio::fs::File` + `axum::body::StreamBody` + `http::HeaderMap` |
| per-endpoint 速率限制 | `Governor` crate（已有限流模式引用）或手写 token bucket |
| PG 系统表查询 | `sqlx::query_scalar` 查询 `pg_stat_user_tables` |
| WS ack 协议 | 现有 `ServerFrame` 枚举加 `Ack` variant |
| 连接生命周期 | 现有 `CancellationToken` 派生，不引入新依赖 |

**但需评估一个依赖**：`axum-extra` 的 `Range` 头解析。当前 `axum` 核心不直接支持 Range 请求，而 `axum-extra` 提供 `headers::Range`。如果该项目已是 `axum` 生态，加此依赖比自写 Range 解析更合理。

### 4.2 第三方依赖的评估标准

对于 `aero-im` 这种 infra-intensive 的 Rust 项目，依赖选择的评估标准（按权重排序）：

1. **编译安全**（`#![forbid(unsafe_code)]` 已强制执行）——所有符合 unsigned 代码架构原则
2. **tokio 生态兼容**——必须使用 tokio 兼容的 async API，无需 `block_in_place`
3. **活跃维护**——最后一次提交在 6 个月内，且有明确维护者
4. **API 稳定性**——未发布 0.x 版本的依赖应避免，除非是 str0m 级别不可或缺
5. **审计面**——避免引入 CVE 历史库或依赖树过大的库

### 4.3 自建 vs 采购的决策依据

**应自建**：
- **连接生命周期管理框架**：因为需要深度集成到现有 `CancellationToken` + `Hub` + `run_socket` 中，外部库无法理解
- **ConsumerPipeline**：因为需要正交现有 `BusConsumer` trait 和 `StorageRepo` 模式

**应沿用现有轻量方案**：
- **Webhook per-endpoint 速率限制**：用 `Governor` 或手写 token bucket（~50 行），不适合引入 Redis-backed 限流（增加延迟且 webhook 重试本身有幂等语义）
- **Blob 流式读取**：现有 `tokio::fs` + `axum` 响应体，不需要 `actix-web` 的 `Payload` 抽象

**不适用采购/外部服务**：
- DB 健康监控直接查询 PG 系统表，不需要 Datadog / New Relic 等外部 APM 代理

---

## 5. 实施路线图

### 5.1 优先级排序

基于商业价值 + 故障严重度 + 实施成本的三维评估：

| 排序 | 方向 | 商业价值 | 故障严重度 | 实施成本 | 综合优先级 |
|------|------|---------|-----------|---------|-----------|
| 1 | **方向一：WS 优雅排干** | 高（滚动部署体验） | 中（部署时断连） | 低（3 个模块改动） | **P1** |
| 2 | **方向五：DB 健康监控** | 高（防线前置） | 高（静默性能塌陷） | 极低（纯 gauge 新增） | **P1** |
| 3 | **方向三：Blob 缓存 L1** | 高（带宽节省 50-90%） | 中（带宽成本） | 低（~15 行 + 迁移） | **P1**（从 P2 升级） |
| 4 | **方向四：客户端回退 L0** | 高（信任度） | 高（消息静默丢失） | 低（重试队列 ~100 行 JS） | **P1**（从 P2 升级） |
| 5 | **方向二：Webhook 并发边界** | 中（防级联延迟） | 中高（全局阻塞风险） | 中（重构投递管线） | **P1** |
| 6 | **方向四：客户端回退 L1**（localStorage+sw） | 中（离线恢复） | 中（重连后丢失 window） | 中（Service Worker 注册） | **P2** |
| 7 | **方向三：Blob 缓存 L2**（Range + WebP） | 中（大文件 + 图片优化） | 低（体验优化） | 中（流式读写重构） | **P2** |
| 8 | **方向 B：ConsumerPipeline 抽象** | 中（可维护性） | 低（不影响功能） | 高（新抽象引入+迁移） | **P2** |
| 9 | **方向 A：ConnectionLifetime 框架** | 低（排干已解决） | 低（方向一已解决） | 中（框架化） | **P3** |

### 5.2 阶段划分

#### 阶段 1（~1-2 周）：低投入高回报止损

```
方向五（DB 监控） + 方向三（Blob 缓存 L1） + 方向四（客户端回退 L0）
```

| 里程碑 | 可交付物 | 验收标准 |
|--------|---------|---------|
| M1.1 | `sample_pg_stats()` 在 metrics_tasks 中运行 | 死元组比率/seq_scan/idle-tx gauge 出现在 `/metrics` |
| M1.2 | PG 死元组告警规则 | 死元组 > 30% 触发 warning |
| M1.3 | `blob_download` 返回 ETag + Cache-Control | 浏览器第二次请求返回 304 |
| M1.4 | 客户端重试队列 | 断网后发送的消息在 WS 重连后自动补发 |
| M1.5 | 客户端 pending 消息超时清理 | 60s 未确认的 pending 消息标记「发送失败」 |

**风险**：无。全纯新增或局部修改，不涉及架构重构。

#### 阶段 2（~2-3 周）：连接生命周期 + Webhook 重构

```
方向一（WS 优雅排干） + 方向二（Webhook 并发边界）
```

| 里程碑 | 可交付物 | 验收标准 |
|--------|---------|---------|
| M2.1 | WS per-connection token 从全局派生 | `pkill aero-server` 后 WS 收到 Close(1001) 而非 TCP RST |
| M2.2 | `ws_shutdown_draining` gauge | 排干期间暴露连接数 |
| M2.3 | WS drain 超时强制关闭（默认 5s） | 超时后连接关闭，不 hang |
| M2.4 | Webhook `dispatch_event` 并行投递 | 10 个目标中 1 个慢响应，其余 9 个不受影响 |
| M2.5 | Webhook per-endpoint token bucket | 429 响应后自动减速 |
| M2.6 | Webhook metrics | `webhook_concurrency`、`webhook_queue_depth`、per-endpoint latency |

**风险**：M2.1 需在 `shutdown.rs` 中向 `Hub` 暴露 `child_token` 方法，涉及 `Hub` 接口变更。需确保 `Hub` 在 banner 打印前完成初始化，否则 WS 接受早于 token 派生。

#### 阶段 3（~3-4 周）：Client ack 协议 + Blob 高级缓存

```
方向四（客户端 ack 协议 L1） + 方向三（Blob 缓存 L2）
```

| 里程碑 | 可交付物 | 验收标准 |
|--------|---------|---------|
| M3.1 | WS 协议新增 `ack` 帧（服务端） | 客户端带 `temp_id` 发消息，收到 `ack` 回复 |
| M3.2 | `ws.sendWithAck()` 返回 `Promise<AckResult>` | 调用方可 `await sendWithAck(...)` 确认消息已持久化 |
| M3.3 | Blob Range 请求支持 | `Range: bytes=0-1023` 返回 206 Partial Content |
| M3.4 | `BlobStore::get_stream` 实现（LocalFs + S3） | 流式读取，不一次性载入内存 |
| M3.5 | `blobs.cache_policy` 字段 + 迁移 | Public blob 走 CDN 缓存，Private 走 no-cache |

**风险**：WS 协议扩展需要确保旧客户端兼容。`temp_id` 字段在新客户端传入但旧服务器忽略是可以的，但反之需注意：旧客户端不传 temp_id，新服务器不返回 ack——这本身是兼容的，但运维需知道两个版本并存时 ack 不会发生。

### 5.3 风险点和缓解策略

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| WS 排干期间新事件溢出 | 低 | 中 | Hub 的 bounded mpsc 满时 ack 前 backpressure 到 NATS consumer；排干期间可配置「排干窗口内新事件丢弃或 requeue」 |
| Webhook 并行投递导致乱序 | 中 | 中高 | 同一 webhook 目标内保持顺序（per-target 串行通道）；不同目标之间可乱序（无因果依赖） |
| `temp_id` 命名冲突（不同客户端生成相同 UUID） | 极低 | 低 | `temp_id` 前缀 instance_id + UUID，全局唯一 |
| Blob Range 与 ETag 的交互 | 低 | 低 | ETag 基于完整内容 sha256；Range 请求的 304 响应不返回 body 但保留 Content-Range |
| 死元组告警在高峰期误报 | 中 | 低 | 告警规则使用 `avg_over_time`（15m 平均）而非瞬时值，避免 autovacuum 还没来得及清理的短暂高峰 |
| 多个 edge case 同时实施引起合并冲突 | 中 | 中 | 阶段 1 和阶段 2 的变更集不相交（阶段 1: metrics + routes + client; 阶段 2: ws + webhook），可并行开发 |

### 5.4 架构师推荐

**立即执行（Phase 0 — 本周）**：

1. **方向五**的 `sample_pg_stats()` 实施。这是纯新增 gauge，0 风险，0 依赖，投入 2 小时产出可获得：死元组比率可视化 + 长事务告警。这是唯一一个「做了一定比没做好」的方向——即使其他方向被优先级挤压，这个也应立即投入。

2. **方向三 L1** 的 ETag 响应头。直接复用 `meta.sha256`，`blob_download` 加 ~15 行 Rust（if-none-match 检查 + 响应头设置）。零依赖，零迁移，投入 1 小时产出 304 缓存。

3. **方向四 L0** 的发送返回值检查。`app.js` 中 `sendMessage` 调用点加 if-false 入重试队列。这是前端代码，不做协议扩展，纯止损。投入 2 小时产出防止消息静默丢失。

这三个任务相互独立，可并行分配。

**需架构评审后执行（Phase 1 — 下个迭代）**：

- WS 排干的设计决策：从全局 `CancellationToken` 派生 per-connection token 的时机和方式。风险在于 `Hub` 初始化时序——WS 连接可能在 `Hub` 尚未启动派生 token 之前建立。解决：`Hub` 在 `new()` 时生成根 token，WS handler 只能在 `Hub` 初始化后注册。

- Webhook 并发的因果顺序保证策略：同一房间订阅的多个 webhook 是否看到事件按序到达？如果并行投递，一个 endpoint 收到 event 3 但尚未收到 event 2 是否可能？——解决方案：per-subscriber 通道保序，不同 subscriber 之间可乱序。

**推迟（Phase 2+）**：

- 抽象 ConsumerPipeline 框架（方向 B）。当前优先修复具体问题（webhook 串行），框架化可在第 2-3 次重构时引入。过度设计过早会拖慢修复速度。

- 全量 connection lifetime 框架（方向 A）。方向一的 WS 排干用 `child_token` 即可解决，不急于将 `ConnectionLifetimeManager` 做成通用框架。

---

## 总结

这五个 edge case 揭示了一个清晰的信号：**Aero IM 的架构正确性在前面（功能正确、事件驱动、AKF 扩展），运营正确性在后面（运维可观测性、运行中可靠性、客户端确认）**。这是一个项目中后期的自然现象——早期关注「能不能跑起来」，后期关注「跑起来后稳不稳」。

五个方向中没有「架构转折」——不需要重写事件总线、不需要换数据库、不需要重构 crate 边界。所有修复都可以在现有架构框架内以纯增量方式完成。这是好事：说明当初的架构决策（NATS + Hub 双层、CancellationToken 排干、有界 mpsc 扇出）提供了足够的扩展空间。

最值得警惕的发现不是任何一个 edge case 本身，而是 **`findPendingMatch` 按文本相似度而非 `temp_id` 匹配**——这表明客户端代码中有一个隐含假设（AI 审核不会修改消息内容从而 break temp_id 匹配）已经不再成立。这比 WS 排干或 webhook 串行更难排查，因为它是语义级的 bug，不产生错误日志，只产生不可理解的用户行为：「我发的消息不见了」。此类「静默语义断裂」是 IM 系统最难 debug 的一类问题。架构师的建议：**在此次修复中不仅要加临时 ID 匹配，还要在 QA 团队的回归测试中加入「发送后 AI 修改」「发送后服务器 restart」「发送时 WS 断开」等场景的自动化测试套件**。

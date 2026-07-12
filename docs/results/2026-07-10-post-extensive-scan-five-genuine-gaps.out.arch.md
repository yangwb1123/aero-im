# 架构分析报告：Aero IM 系统

## 1. 架构评估

### 1.1 核心架构优势

**事件驱动骨架的成熟度令人印象深刻**。系统以 NATS JetStream 作为跨进程事实源、Hub 作为进程内扇出层的两层设计，已经经受住了真实生产级评审的检验。几个设计亮点值得强调：

- **扇出层与背压处理**：`Hub::fan_out_raw` 的 `lossy` 旗标 + `RESYNC_FRAME` 组合是高质量的慢消费者策略。代码明确区分了两条路径——丢弃模式（`drop-only`，打开 loss episode → 容量恢复时发单帧 resync 信号）和断开模式（`disconnect_on_full = true`，直接驱逐）。这种设计既避免了 OOM，又预留了客户端自愈通道（`?since=` 回填），比静默丢帧或无限阻塞都更成熟。

- **consumer 分布策略清晰**：`im.room.*` 用 durable consumer（at-least-once），`live.stream.*` 用 ephemeral consumer（at-most-once 可接受丢弹幕）。两种语义对应不同可靠性需求。8 个 durable consumer（`aero-server` + 6 bots + `aero-webhooks`）各自独立游标，consumer 隔离性正确。

- **幂等性覆盖较广**：webhook 有 `(webhook_id, event_id)` 唯一索引防重投重复 insert，但 bot delivery log 没有；OOO bot 有 `ON CONFLICT DO NOTHING` 幂等；AiWorker 有 `SKIP LOCKED` + 预算系统；record_attempt 有防重。这种分层幂等策略在实践上是合理的——不是所有路径都需要幂等，但关键副作用路径（外推、扣费）都覆盖了。

- **架构分层干净**：17 个 crate 的 workspace 结构清晰，自下而上无成环依赖。`aero-common` 作为纯叶子 crate（共享类型、ID、事件枚举），`aero-bus` 作为总线抽象，`aero-storage` 作为数据访问层，上层按功能域拆分，复合层在 `aero-server` 装配。约束单路由的迁移 → 仓储 → 路由 → 鉴权 → 实时流程（§4.1）提供了可复制的模式。

### 1.2 关键的架构风险和局限

#### 风险 1：stuck pending 行——僵尸状态的数据累积

这是**生产裂缝**级别的问题。`claim_due` 仅查询 `status = 'failed'`（`webhook_delivery.rs:319`），但 `record_attempt` 插入行时状态为 `pending`。当 webhook 端点返回网络错误（非应用层 4xx/5xx）且 `mark_failed_with_backoff` 未被调用时——dispatch 流程中的异常路径或崩溃可能导致行永久停留在 `pending`。`sweep_terminal_before` 只清理 `delivered`/`dead`，不覆盖 `pending` 或 `failed`。这意味着：

1. `pending` 行永远不会被重试，也永远不会被清理
2. `failed` 行（backoff 时窗内未到期的）同样不被 sweep 触及
3. 随时间累积，虽单行轻量，但无上限

**类似问题也出现在 bot 侧**：`bot_subscription_deliveries` 表是纯 write-only log，既无 retry loop 也无 sweep。

#### 风险 2：agent_bot 同步 AI 调用阻塞 NATS consumer 线程

`agent_bot.rs:47-52`（`while let Some(sub) = stream.next()`）在循环中直接 `await ai.answer_question(…)`，阻塞了整个 `next()` 流。虽然 Rust 的异步是协作式调度，但这样做意味着：

- 该 agent_bot 进程在调用 AI 的数百毫秒到数秒内**完全无法处理任何其他消息**
- 如果 AI 服务降级（超时、可用性下降），agent_bot 的 consumer 会完全停顿
- 对于单进程部署，这意味着在该窗口内其他机器人（OOO、unfurl、transcribe）**不会受影响**（它们有独立的 consumer），但 agent_bot 自身的吞吐量降为 1 QPS

相比之下，`moderation_bot` 采用了 bounded mpsc queue + N workers 的隔离模式，agent_bot 显然可以复用此模式。

#### 风险 3：`message_history` 与 retention 脱钩

`message_history` 表（迁移 0036）记录每次编辑前的内容版本。`retention.rs` 的 `sweep_messages` 只清理 `messages` 主表，不 touch `message_history`。编辑历史因此永久积累。对于高频编辑的场景（如 AI 自动重写、协作编辑），这构成了无上限存储增长。法务保全的 message （`legal_holds`）也未被上述清理豁免——但 `message_history` 的 retention 脱钩是确凿的数据生命周期缺口。

#### 风险 4：跨节点 retry 真空

Webhook retry loop（`run_webhook_retry_loop`）、AiWorker claim loop 等周期性任务都是 `tokio::spawn` 单节点任务。在多实例部署中：

- 每个实例都会运行 `claim_due`，导致**所有实例竞争同一批到期行**——`FOR UPDATE SKIP LOCKED` 虽防双送，但冗余查询存在，且每实例睡眠间隔不同导致不必要的数据库负载
- 没有 leader election 机制来让一个实例承担协调者角色
- 在滚动升级期间，同一个 retry loop 可能在新旧实例上都短暂运行

好消息是 webhook 有幂等性保护（`record_attempt` 的 `ON CONFLICT`），因此副作用层面安全。但 AiWorker 的 `defer` 逻辑和预算系统是进程内状态，多实例间不协调——一个实例 defer 的行可能被另一实例 claim，预算控制因此失效。

#### 风险 5：总线消费者无启动健康探测

所有 bus listener（WS、6 bots、webhook dispatcher）的启动模式都是「尝试 subscribe，失败则重试」——没有检查 PG 连接、Redis 连接、AI 服务是否就绪。这意味着：

1. 如果 PG 宕机，`agent_bot` 可能在 AI 响应后调用 `im.send_message` 时失败
2. 如果 Redis 宕机，presence 心跳和 room member cache 失效
3. 失败表现不一：有些路径有 `warn!` + skip，有些则只记录 error

当前模式是「延迟暴露问题」而不是「启动时快速失败」——可能依赖关系仅在真正调用时才暴露。

---

## 2. 扩展方向

### 方向一：有状态交付语义层（P0 — 数据面完整性）

**为什么需要**：
当前系统有三种交付模式混合使用——at-least-once（durable NATS consumer）、at-most-once（ephemeral consumer）、best-effort（bot dispatch one-shot + no DLQ）。缺少一个统一的**交付状态跟踪层**来回答「这个事件被成功处理了吗？」。对于 webhook，已有 delivery_log + retry/DLQ 机制（虽然 pending 有漏洞）；对于 bot 类型的订阅交付，只有 write-only log 无重试。修复这些漏洞的成本低但收益明确。

**核心挑战**：
- 幂等键的设计需要跨表复用还是每条线独立？
- 状态机模型分层：当前是单层 `pending → delivered | failed → dead`。是否需要引入 `deferred` 态（专门标记预算不够/限流跳过，而非失败）？
- 需要与 NATS 的 ack/nack 语义配合——目前所有 consumer 都是统一 ack（坏 payload 也 ack-drop），引入交付状态机后是否需要区分「消费成功但交付暂挂」？

**架构变更**：
1. 为 `claim_due` 增加 `pending` 状态的查询（如前所述，修复 stuck pending）
2. 将 `bot_subscription_deliveries` 接入 retry loop（目前只有 record，没有 claim）
3. 在 message_history 和 retention sweep 之间建立联动——`sweep_messages` 后按 `room_id` 清理 `message_history` 中的孤儿行
4. 可选：引入统一的 `DeliveryStatus` trait，让 webhook 和 bot dispatch 共用同一套重试/DLQ 管理代码

**影响**：
- 无侵入——已有表和现有代码不动，只在 sweep 路径和 claim 查询加条件
- 对 bot dispatch 是纯提升——从一次性的 log 变为可重试的交付

### 方向二：消费者隔离 — 将 agent_bot 改为 bounded queue + worker 模式（P0 — 系统韧性）

**为什么需要**：
agent_bot 的同步阻塞模式是验证报告中确认的真实问题。只需将其 consumer 改为 moderation_bot 模式——bounded mpsc（默认 512）→ N workers（默认 2），即可实现：
- consumer loop 永远不被 AI 延迟阻塞
- 背压：队列满时 drop（或 nack 让 NATS redeliver），而不是积压内存
- 可控并发：worker 数量决定最大并行 AI 请求数，与下游 AI 服务容量对齐

**核心挑战**：
- agent_bot 当前 `handle` 函数内有 `state.rooms.is_member()` 和 `state.participants.get()` 等 DB 查询。如果引入 worker 队列，worker 侧的 `state` 引用需要是 `Arc<AppState>` ——这已经满足。
- 需要在 worker 返回后 ack（当前模式是 consumer loop ack）。改为队列模式后，ack 应发生在 worker 成功处理完成后（至少一次），或者队列背压丢帧时 ack-drop（与 moderation_bot 一致）。

**架构变更**：
本质上是将一个内部函数调用从 sync 改为 async dispatcher + worker 池。对 consumer 侧几乎没有侵入——agent_bot 的 `run` 函数创建 mpsc channel，consumer 侧 `send` 消息，worker 侧 `recv` 和 `handle`。

**影响**：
- 正向：agent_bot 的 NATS consumer 不再成为 AI 延迟的瓶颈
- 侧效应：需要处理 worker 中 AI 调用超时——当前 `ai.answer_question` 无超时参数，可能需要增加 `tokio::time::timeout`

### 方向二（B）：总线消费者健康探测 / 启动顺序（P1 — 运维体验）

**为什么需要**：
当前所有 bus listener 启动时无依赖检查。在容器化 / k8s 部署中，启动顺序不确定可能导致：「agent_bot 在 PG 就绪前启动 → 收到消息尝试 DB 查询 → 失败」。虽然当前实现是 fail-open（warn + continue），但更好的做法是 readiness gate。

**架构变更**：
在 `run_bus_listener`、`agent_bot::run`、`ooo_bot::run` 等函数的 subscribe 之前，增加轻量级健康探测：
```rust
// 伪代码模式
async fn wait_for_ready(state: &AppState) {
    // 并行检查 PG、Redis、AI（如配置）
    tokio::time::timeout(startup_grace, ...)
}
```
失败则终止启动（`anyhow::bail!`），由 `TaskTracker` 捕获并记录致命错误。

**权衡**：
- **激进**：任一依赖不达则进程退出（fail-fast）。好处是 ops 视角明确，k8s restart 重试自然。坏处是 transient 启动窗（如 NATS 刚启动但 consumer 未就绪）可能导致反复重启
- **保守**：健康探测只记录 warning，不阻止消费者启动。与当前行为一致，但至少提供可观测性信号

建议保守模式起步——加 health probe 但不 fail-fast。

### 方向三：跨实例 budget 协调器（P1 — 多节点规模）

**为什么需要**：
当前 AiWorker 的预算控制（`KeyedCostBudget` per-ws + 全局 `CostBudget` over 60s）是进程内状态。多实例部署下：

- 每个实例独立记录成本，全局 120 预算变成 `120 × N_instances` 实际容量
- `defer` 逻辑在实例 A 上 defer 的行可能在实例 B 上被 claim（因为是 `SKIP LOCKED`）
- 预算限制形同虚设——超售风险真实存在（尤其是付费的 Embed/Summarize 任务）

**核心挑战**：
- 预算协调需要中心化记账。Redis sorted-set 可以胜任（incrby + expire），但引入额外的每次成本记录的 Redis RTT
- 完全的全局协调意味着每次 AI 调用前查 Redis——这会增加延迟；替代方案是「本地 burst budget + 全球 hard cap」两层模型
- 需要决策：是否允许超卖？（保守设计：hard cap 用 Redis + 本地 burst window）

**架构变更**：
```
当前：AiWorker 进程内 Semaphore + per-ws Budget per 60s
目标：本地 Semaphore（控制并发） + Redis ZINCRBY（控制全局成本）
      + 每 60s 重置窗口（ZREMRANGEBYSCORE 清除过期窗口）
```
仅在 `spend`/`charge_cost` 中增加一次异步 Redis 调用。使用 `incrby` 的原子性和 `EXPIRE` 保证窗口重置。Redis 不可用时的退化行为：fallback 到当前本地预算（超卖风险可接受，罕见故障窗口）。

**影响**：
- 主要影响 `AiWorker` crate（`crates/aero-ai/src/worker/` 或类似路径）
- 引入新的 Redis 连接（或复用 `state.redis_client`，后者更合理）
- 在 Redis 宕机时可退化到本地仅并发限制（无全局预算）

### 方向四：媒体管线可观测性（P2 — 运维可观测）

**为什么需要**：
验证报告指出 SFU forwarder、call bridge、live stream 均无 Prometheus metrics。这是系统当前最大的黑箱——IM 消息的可观测性完备（`MESSAGES_SENT_TOTAL`、`BUS_POISON_DROPPED_TOTAL`、WS conenctions gauge 等），但媒体面（通话质量、推流延迟、丢包率）完全不可观测。对于一个承载互动直播和实时通话的系统，这可能是运维盲区中最危险的一个。

**核心挑战**：
- str0m 库的内部统计接口尚未明确——需要确认 str0m 的 `RtcConfig`/`RtcSession` 是否提供丢包、RTT、jitter 统计，还是需要从 RTCP Receiver Report 中自行解析
- 延迟直方图（`histogram!`）的 buckets 定义需要经验值：IM 消息期望 <200ms，直播期望 <2s，通话期望 <150ms
- 跨节点桥的可观测需要两端配合——不仅记录本端 RTP 统计，还要记录桥接出口/入口的帧计数映射

**预期的度量集**：

| 指标 | 类型 | 来源 |
|------|------|------|
| `sfu_forwarded_packets_total` | counter | SfuForwarder |
| `sfu_dropped_packets_total` | counter | SfuForwarder (congestion) |
| `call_duration_seconds` | histogram | CallOrchestrator (end→start) |
| `stream_publish_latency_seconds` | histogram | LiveIngest→HLS first segment |
| `call_bridge_egress_packets_total` | counter | CallEgress |
| `rtcp_fir_total` / `rtcp_pli_total` | counter | RTCP feedback handler |
| `ws_fanout_dropped_total` | counter | Hub::fan_out_raw |

**架构变更**：
纯新增——在现有代码中植入 metric 调用点。不改变控制流。需要确保 metrics registry 在 `aero-common` 中是可共享的（已验证是 `lazy_static` 或 `once_cell` 全局）。

### 方向五：跨节点 retry/调度去中心化（P2 — 高可用）

**为什么需要**：
当前 `run_webhook_retry_loop` 和定时扫表任务都是**每个节点独立运行相同逻辑**。对于少量任务（webhook 失败、retention sweep、embedding backfill），冗余是安全的——`SKIP LOCKED` / 幂等性防副作用泄漏。但随着节点数增长：

| 任务 | N=2 冗余 | N=10 冗余 |
|------|----------|-----------|
| webhook retry | 每个实例 claim_due 竞争，2倍冗余查询 | 10倍无用查询 |
| retention sweep | 两个实例先后跑相同 DELETE | 10次相同 DELETE（幂等但浪费） |
| viewer sampler | double-count 风险 | 10倍冗余 |

**核心挑战**：
- 需要 leader election 但不需要 ZooKeeper 级别的强一致性——NATS JetStream 的 EPHEMERAL consumer 绑定到单个实例的槽位（leader 投递），可以天然实现**谁消费谁干活**
- 替代方案：POSTGRES advisory lock（`pg_try_advisory_lock`），轻量且准确
- 经济决策：对于目前的部署规模（2-3 节点），冗余是否可接受？

**推荐方案**：
持轻量级 advisory lock 方案。定时器任务启动时 `SELECT pg_try_advisory_lock(<task_id>)`，成功者执行，失败者 skip。锁在连接断开时自动释放。不需要引入新基础设施。

---

## 3. 接口设计建议

### 3.1 现存接口设计的评估

系统当前的接口设计有几个值得肯定的原则：

- **EventBus trait 抽象**（`aero-bus`）：NATS 实现通过 trait 隐藏，测试可用 fake bus。这是正确的抽象层次——不强到允许替换消息队列（设计目标：NATS 是事实源），但强到方便测试。

- **WebhookSender trait**：为测试可 mock 的 seam，当前有 `ReqwestSender` 和 `FakeSender`。同样正确。

- **仓储模式**（`XRepo`）：每个功能域一个仓储，内部封装 sqlx 查询，返回强类型结果。DB 测试统一 `#[ignore]` + `DATABASE_URL` 门控。

但有以下改进空间：

### 3.2 建议引入的抽象

**A. 统一的交付状态机（DeliveryStateMachine）**

当前 webhook、bot dispatch、push notification 各有独立的交付跟踪逻辑，但状态转换模式高度相似——都是 `pending → delivered | failed → dead`。建议提取一个共用的 `DeliveryTracker` trait：

```
trait DeliveryTracker {
    async fn record_attempt(&self, target_id, event_id) -> Result<Option<DeliveryAttemptId>>;
    async fn mark_delivered(&self, id, status_code) -> Result<()>;
    async fn mark_failed(&self, id, err) -> Result<()>;
    async fn claim_due(&self, now, limit) -> Result<Vec<DeliveryAttempt>>;
    async fn sweep_terminal(&self, cutoff) -> Result<u64>;
}
```

**权衡**：
- **赞成**：bot dispatch 直接获得 webhook 级别的重试/DLQ 能力，push notification 也可复用
- **反对**：当前 webhook_delivery_log 的表结构与 bot_subscription_deliveries 不同（webhook FK vs bot FK），强行统一可能引入泛型过度
- **折中**：只统一「状态转移的逻辑模式」，不统一表结构——即共享 `claim_due` 的 `FOR UPDATE SKIP LOCKED` SQL 模式，但各表各自实现

**B. 进程间 budget 协调接口**

当前 AiWorker 的 `CostBudget` 是 `std::sync::Mutex<HashMap>` + 时钟驱动过期。如果引入跨实例协调，建议通过 trait 将「budget 存储」接口化：

```
trait CostLedger: Send + Sync {
    async fn spend(&self, ws_id, weight, window_secs) -> Result<bool>;
    async fn remaining(&self, ws_id) -> Result<u64>;
}
```

**实现方案**：
- `LocalCostLedger`：当前进程内 Mutex+HashMap——默认、退化路径
- `RedisCostLedger`：使用 ZINCRBY + ZREMRANGEBYSCORE——生产推荐
- `NoopCostLedger`：预算永远返回 true——开发/测试

这是**最少侵入改动**——仅将 `charge_cost` 中的 budget check 改为 trait 调用，现有测试可用 `NoopCostLedger` 或 `LocalCostLedger` 运行。

**C. Consumer Health gate**

每个 bus listener 都需要健康探测模式。建议在 `aero-bus` 或 `aero-common` 中提供：

```
/// Check that all named dependencies are reachable, returning the first failing
/// one or `None` (all ready). Implementations should be lightweight (ping/head).
#[async_trait]
trait StartupCheck {
    async fn check(&self) -> Result<(), String>;
}
```

boot 时装配 `Vec<Box<dyn StartupCheck>>`，每个 listener 消费前或 subscribe 前调用。`wait_for_ready` 使用指数退避等待（而非忙等），超时则记录致命错误。

**决定**：是每个 listener 独立检查还是统一 readiness gate？建议统一——在 `serve.rs` 的 `bind` 之前做一次全面 check，所以 `GET /health/ready` 可以直接复用同一份检查列表。这样 bus listener 启动时不需要再检查（假设启动时已通过 readiness gate）。

### 3.3 向后兼容性

所有建议的接口设计都遵循**纯加法原则**：

- 新的 trait/default impl 不影响已有调用者
- 表结构已有数据不动，新索引/约束用 `CREATE INDEX CONCURRENTLY IF NOT EXISTS`
- 已有 metric 名不变（新 metric 另开命名空间 `aero_*`）
- AiWorker budget 面：`LocalCostLedger` 作为 `AiService::new` 的默认后端、`RedisCostLedger` 通过 builder 模式选装（`AiConfig::with_redis_ledger(redis)`）

---

## 4. 技术选型

### 4.1 现有技术栈评估

| 技术 | 角色 | 评估 |
|------|------|------|
| NATS JetStream | 事件总线 / 跨进程交付 | ✅ 正确的选择。ephemeral+durable consumer 两种语义、at-least-once 投递、轻量运维 |
| Postgres + pgvector | 持久 / 向量搜索 / FTS | ✅ 全栈不走 ES/Meilisearch 节省运维复杂度。pgvector 是合理起点 |
| Redis (sorted set) | 集群状态 / presence / 限流 | ✅ sorted set 的 `zadd` + `zremrangebyscore` 时序数据用得很恰当 |
| str0m | DTLS-SRTP / ICE / SFU | ✅ Rust 原生纯 DTLS、无 C 依赖。但可观测性接口待验证 |
| Axum 0.7 | HTTP gateway | ✅ 0.7 路线稳定，`.merge` + layer 组合子架构优雅 |

### 4.2 不需要引入的

基于代码审查，以下**不需要**新增：

- **外部消息队列替代 NATS**：NATS 的设计决策正确且无迁移压力。修复 claim_due、stuck pending 比更换基础设施优先级高 100 倍
- **跨进程状态管理引入 ZK/etcd**：Redis sorted set 已满足现有需求（presence、roster）。budget 协调用 Redis 即可，不需要强一致性共识
- **APM 系统额外引入**：现有 Prometheus + OTLP 路径已就绪，只需增加 metric 点

### 4.3 可评估引入的

| 技术 | 场景 | 决策标准 |
|------|------|----------|
| `opentelemetry` 的 `histogram!` 宏 | 媒体管线延迟直方图 | ✅ 建议引入。当前代码库零 `histogram!` 调用，但 `metrics::inc_counter` 已有 OTLP 路径，一致性好 |
| `bb8` 或 `deadpool` 连接池 | 如果新增 Redis 连接做 budget 协调 | ❌ 不需要——fred 的 `RedisClient` 自带连接池 | 
| `rtrb` (lock-free ring buffer) | moderation_bot / agent_bot 的 mpsc 队列 | ❌ `tokio::sync::mpsc` bounded channel 已经够用——额外引入 lock-free 结构不会给这些任务带来可感知的收益 |
| `tower` 的 `Service` trait | HTTP handler 间的中间件化 | ❌ 当前 Axum handler 直接调用仓储模式已经干净。`tower::Service` 层引入只为「统一接口」不值得 |

### 4.4 自建 vs 依赖第三方

**决策原则**：
- **网络/IO 边界层自建**（call-bridge 帧编解码、SfuForwarder 选择性转发）：自建正确——领域特定，第三方无成熟库
- **标准协议层用第三方**（str0m for WebRTC、rml_rtmp for RTMP、sqlx for PG）：正确——协议标准化的部分不应自建
- **时序/可观测层自建 metric 点 + Prometheus 收集**：正确——`aero-common` 中 `metrics` 模块的自建封装 + prometheus crate 导出

**需要重新评估的点**：
`str0m` 的社区活跃度和 RTCP 统计接口。如果 str0m 0.19 不提供 RTT/loss/jitter 的内部统计导出，系统要么需要从裸 RTCP Receiver Report 解析（自建），要么考虑在 `aero-live-webrtc` 中增加一层 RTP 统计头部捕获。建议在季度规划中专测一次 str0m 的用户态统计能力。

---

## 5. 实施路线图

### 优先级总览

| 优先级 | 方向 | 估计工作量 | 风险等级 | 业务价值 |
|--------|------|----------|----------|----------|
| P0 | 方向一：stuck pending + message_history 修复 | 1-2 天 | 低 | 高（防止系统级无界增长） |
| P0 | 方向二：agent_bot 队列隔离 | 1-2 天 | 低 | 中（单实例阻塞风险） |
| P1 | 方向三：跨实例 budget 协调 | 3-5 天 | 中 | 中（多实例超售风险） |
| P1 | 方向二（B）：健康探测 | 2-3 天 | 低 | 中（运维体验） |
| P2 | 方向四：媒体可观测性 | 5-8 天 | 中 | 高（媒体黑箱风险） |
| P2 | 方向五：轻量级 leader election | 2-3 天 | 低 | 低（当前 N≤3 可接受） |

### 阶段划分

**Phase 1（P0 — 数据面完整性 + 系统韧性）**：

| 里程碑 | 交付物 | 验证方式 |
|--------|--------|----------|
| M1.1 | `claim_due` 增加 `pending` 状态查询 | `SELECT ... WHERE status IN ('failed','pending')` + 单测覆盖 |
| M1.2 | `sweep_terminal_before` 增加 `failed` 超期清理 | 熔断保护：保留 `failed` 行直到其 `next_attempt_at` 窗口完全过去 |
| M1.3 | `message_history` 接入 retention sweep | `sweep_messages` 后清理孤儿 `message_history` 行；跳被 legal_holds 引用的 |
| M1.4 | agent_bot 改为 mpsc + N workers | consumer 循环不再直接 await AI，通过 bounded channel 投递 |
| M1.5 | `bot_subscription_deliveries` 增加 retry loop | 复用 `claim_due` 模式，或至少增加 `status` 索引以便未来迭代 |

**Phase 2（P1 — 规模 + 运维）**：

| 里程碑 | 交付物 | 验证方式 |
|--------|--------|----------|
| M2.1 | `CostLedger` trait + `RedisCostLedger` 实现 | 存量 `LocalCostLedger` 为默认，env flag `AERO_AI_REDIS_LEDGER` 切 Redis |
| M2.2 | 进程内 budget 在 `spend` 前查 Redis | 原子性 `EVAL` 或 `INCRBY` + `EXPIRE` |
| M2.3 | readiness gate 集成到 boot | 统一 `Vec<Box<dyn StartupCheck>>`，`GET /health/ready` 复用 |
| M2.4 | 所有 bus listener 启动时可选检查（warning-only） | 日志记录「依赖未就绪」不阻塞启动 |

**Phase 3（P2 — 媒体可观测 + 高可用）**：

| 里程碑 | 交付物 | 验证方式 |
|--------|--------|----------|
| M3.1 | SFU 模块注入 counter + histogram | 关键路径：`SfuForwarder::on_rtp` 计数、`SfuMediaSession::run` 循环中记录 |
| M3.2 | call orchestration 录制 latency + duration | `CallOrchestrator::end_call` 计算并记录 `call_duration_seconds` |
| M3.3 | 跨节点桥的帧计数 | `CallEgress::bridge_frame` 出口计数 vs `SfuMediaSession::on_rtp` 入口计数 |
| M3.4 | 汇总 `observability_gauge_samplers` 中加媒体面 | 已有时钟驱动的 gauge sampler，增加媒体相关 gauge |
| M3.5 | PG advisory lock 轻量级 leader election | retention sweep、webhook retry 等批量任务 = `pg_try_advisory_lock` |

### 风险点与缓解策略

| 风险 | 可能性 | 影响 | 缓解 |
|------|--------|------|------|
| stuck pending 手动修复引入回归 | 低 | 中 | 为 `claim_due` 增 `WHERE status = ANY(...)` 而非替换条件，旧查询走全表扫描仍可用 |
| agent_bot 队列模式中 ack 时序错误 | 中 | 高（消息丢失） | worker 处理后 ack 而非 consumer 侧；bounded queue full 时 nack 而非 drop（让 NATS 重投） |
| RedisCostLedger 在 Redis 宕机时触发级联超时 | 中 | 中 | 超时退化到 `LocalCostLedger`，仅本进程内限制并发但无全局 cap |
| str0m 不提供用户态统计（RTT/jitter/loss） | 中 | 高（Phase 3 阻塞） | 备选方案：从 RTCP Sender Report/Receiver Report 手动解析（`aero-live-webrtc/rtcp_fb.rs` 已有基础） |
| message_history 清理误删法务保全引用 | 低 | 高 | DELETE 时 `NOT EXISTS (SELECT 1 FROM legal_holds WHERE ...)` 保护已保全 message |
| 跨节点桥可观测需要两端协调 | 低 | 中 | 先做单端计数（本节点扇出量），跨节点错位计数为 Phase 4 |

---

## 总结

Aero IM 是一个**架构成熟度远高于一般 Rust 项目的系统**。核心的事件驱动骨架、分布式消费者策略、幂等性守卫设计都已经通过了高信号量的代码审核验证。

最具价值的改进方向集中在**数据面完整性修复**（stuck pending、message_history 脱钩、bot dispatch 无重试）——这些都是低成本、高回报的精确修复，而非架构大改。agent_bot 的队列隔离是唯一需要适度代码改动的 P0 项。

在 P1/P2 阶段向**跨实例协调**（预算、leader election）和**媒体可观测性**移动，将使系统从「单节点可观测」真正进化到「分布式可运维」。这两个方向的核心挑战不在于代码复杂度，而在于设计的决策面——如何在轻量（不引入新基础设施）和完备（覆盖所有级联故障模式）之间找到平衡。

推荐的实施顺序是：修复漏洞（Phase 1）→ 规模韧性（Phase 2）→ 媒体黑箱化（Phase 3）。每一步都独立可交付、可验证、不产生技术债务回退。

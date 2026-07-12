# Aero IM 架构深度分析报告

> 基于代码验证文档 + AGENTS.md 项目上下文，从架构师视角评估现状、识别风险、规划演进。

---

## 1. 架构评估

### 1.1 当前架构优势

**事件驱动骨架设计精良。** 系统采用「NATS JetStream 跨实例投递 → 每进程 Hub 本地 bounded mpsc 扇出」的双层事件路由模型。这一决策带来了三个关键收益：

- **水平扩展透明**：多实例无中心 broker 瓶颈，每个实例独立扇出到本地 WebSocket 连接
- **持久消费与易失消费分离**：`im.room.*` 用 durable consumer 保证 at-least-once 投递；`live.stream.*` 用 ephemeral consumer 接受弹幕丢失，互不影响
- **进程内扇出隔离**：Hub 的 bounded mpsc 提供了天然背压边界，不会因单房间大量 WebSocket 写阻塞阻塞 NATS 消费循环

**crate 分层清晰。** 自下而上的依赖链（common → bus/storage/auth/signaling → im-core/im-call/ai/push → live-* → server）无环，每层职责边界明确。直播各协议（RTMP/WHIP/SRT/HLS）拆分为独立 crate，遵循接口隔离原则。

**Redis 作为集群级状态的事实源。** 凡需要跨实例一致的状态（presence、观看数、通话 roster）都走 Redis sorted-set，不做进程内存假设。这是分布式系统中最正确的决策之一。

**AI 能力的退化设计。** 无 API key 时回落 `HashEmbedder` + 启发式 completion，保持了所有 AI 路由的响应路径完整，不会因外部依赖不可用而产生连锁故障。

### 1.2 关键设计决策合理性评估

| 决策 | 评价 | 依据 |
|------|------|------|
| NATS JetStream 作为总线 | **合理但引入运维复杂度** | 相比 Redis pub/sub，JetStream 提供持久化游标、at-least-once、按 subject 单调 seq——这些对 IM 场景至关重要。但 NATS 集群运维是额外负担 |
| str0m 纯 Rust WebRTC | **战略正确但有成熟度风险** | 避开 libwebrtc 的构建地狱和进程模型，纯 Rust 实现 SRTP/DTLS/SCTP 是长期正确选择。但 `SfuMediaSession` 的生产未接线状态意味着该决策尚未经真实流量验证 |
| sqlx 编译期 SQL 校验 | **正确** | 对比 diesel 的 ORM 厚重感，sqlx + 迁移编译期嵌入提供了类型安全 + 灵活 SQL 的最佳平衡 |
| 每 crate 内建仓储模式 | **合理但局部冗余** | `XRepo::new(s.pg.clone())` 在路由函数中内联创建，避免了 DI 框架的复杂性。代价是每个路由 handler 都要新建 repo，slight overhead 可接受 |
| 进程内桶(token bucket) + Redis 集群检查 | **混合模型合理** | 兼顾低延迟（本地桶拒绝爆发的 99% 请求）和跨实例协调（Redis 兜住慢速分布攻击） |

### 1.3 架构债务与技术债

**P1——进程生命周期管理的优雅关闭缺陷。** 这是当前最严重的技术债：
- `run_bus_listener`/`run_live_bus_listener` 无 `CancellationToken`、无 `tokio::select!`
- HTTP server 关闭后，bus listener 继续消费事件并尝试通过已关闭的 Hub 扇出
- 结果：关闭窗口期的事件静默丢弃，数据丢失 + 资源泄漏
- 根因：`background.rs` 中将总线循环与 HTTP server 生命周期解耦，缺少统一的 graceful shutdown 契约接口

**P1——总线消费者安全守卫缺失。** push_bot、webhook_dispatcher、bot_dispatch 各自拥有独立的 NATS durable consumer，但它们**不经过** `assert_room_access` 守卫链。当用户被停用、被移出房间、被要求 2FA 时，这些异步通路仍然继续投递。这不是性能问题——这是安全漏洞。

**P2——软删除数据孤儿。** `soft_delete_in_tx` 仅操作 messages 表，留下 reactions、receipts、pins、message_history、block_interactions、message_reports 中的孤儿行。这是一个随时间累积的数据质量问题。随着消息量增长，这些孤儿行会导致：
- 不准确的通知计数（reactions 仍存在）
- 膨胀的全文搜索索引
- GDPR 删除不完整

**P2——单点登录限流器。** 登录限流器（`AuthService` 中的 in-process `HashMap`）是多实例架构中最明显的放大攻击面。攻击者可以在 N 个节点上各发送 N 次登录尝试，有效限流阈值被放大 N 倍。

**P3——env 配置键的双标准。** 限流参数用 `AERO_RATE_LIMIT_PER_SEC`（单下划线），其余用 `AERO__SECTION__KEY`（双下划线）。这是一个小但持续的认知负担——每次加新配置都需要查证应该用哪种风格。

**P3——`Hub` 的内部状态使用 `Arc<RwLock<HashMap>>` 而非 DashMap。** 对于高频读写的连接表，`RwLock` 在读写冲突时会产生锁竞争。虽然当前规模下不是瓶颈，但这是一个已知的可扩展性债务。

---

## 2. 扩展方向

### 方向 A：统一的 Graceful Shutdown 框架（P0）

**为什么需要：** 当前 `run_bus_listener` 等常驻循环没有纳入进程生命周期管理，导致关闭时数据静默丢失。随着系统承载的业务增多（更多 bot、更多 worker、更多定时器），这将成为可靠性瓶颈。K8s Pod 滚动更新时每次重启都产生数据丢失窗口。

**核心挑战：**
1. 多种循环类型需要统一的生命周期契约——NATS durable consumer（需要 drain + ack-in-flight）、ephemeral consumer（直接取消）、定时器（跑完当前 tick）、HTTP server（优雅 drain）
2. 关闭顺序有依赖——WebSocket Hub 必须先关闭（停止接受新扇出），再 drain bus listener（不再投递新事件），最后关闭 NATS 连接
3. 现有代码中 `run_bus_listener` 的内层 `loop { let mut stream = bus.subscribe(...) }` 是一个**重订阅**循环——cancel 需要在两层循环中都能检测

**预期架构变更：**

```
// 当前：无生命周期接口
pub async fn run_bus_listener(state: AppState) { loop { ... } }

// 目标：统一的 Lifecycle  trait
pub trait ServiceLoop: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn run(self: Box<Self>, shutdown: CancellationToken) -> impl Future<Output = ()> + Send;
}

// 装配点
let shutdown = CancellationToken::new();
let mut supervisor = ServiceSupervisor::new(shutdown.child());
supervisor.add(Box::new(BusListenerLoop::new(state.clone())));
supervisor.add(Box::new(WebhookDispatcherLoop::new(state.clone())));
supervisor.add(Box::new(HttpServer { ... }));
// shutdown cascade: stop accepting → drain in-flight → close connections
```

**对现有系统的影响：**
- 每个 `run_*` 函数需要包裹为实现了 `ServiceLoop` 的结构体（机械工作量，逻辑不变）
- 需要引入关闭阶段（phase）概念——先 drain WebSocket Hub，再 drain bus listeners，最后关闭 DB/NATS 连接
- 改造期间不影响现有功能，可增量迁移

### 方向 B：跨总线消费者安全层（P0）

**为什么需要：** 当前 7 个总线消费者（bot、ooo、unfurl、transcribe、golive、push、moderation）各自独立消费 `im.room.*` 或 `live.stream.*`，但全部绕过 HTTP 请求路径上的 `assert_room_access` 守卫。这意味着停用账户、被移出房间的成员仍然通过异步通路接收数据。对于企业级产品，这是不可接受的合规风险。

**核心挑战：**
1. 安全校验需要房间成员关系 + 账户活跃状态 + 2FA 状态——这些在总线消费时刻可能已与事件发生时不同
2. 在总线路径上添加 DB 查询会引入延迟，改变 at-least-once 语义的窗口
3. 不同 bot 需要的校验粒度不同——push_bot 需要 `can_notify`，agent_bot 需要 `can_interact_with_bot`

**预期架构变更：**

```
// 在总线解码和 bot 分发之间插入统一的授权层
[RoomEvent on NATS] → [decode + lift seq] → [AuthorizationFilter] → [per-bot dispatch]

// AuthorizationFilter 提供一组可组合的检查
pub struct AuthFilter {
    // 缓存成员关系（TTL 短），避免每个事件都查 DB
    membership_cache: moka::Cache<(RoomId, ParticipantId), MembershipStatus>,
}

impl AuthFilter {
    async fn can_receive(&self, room: &RoomId, participant: &ParticipantId) -> Result<bool> {
        // 1. 检查成员关系（缓存优先）
        // 2. 检查 account 是否 active
        // 3. 检查是否强制 2FA
        // 4. 检查 info_barrier 约束
    }
}
```

**对现有系统的影响：**
- 每个 bot 需要声明其需要的授权级别（`NeedRoomAccess`、`NeedNotifyAccess`、`NeedAnyInteraction`）
- 引入 `moka` 缓存依赖（已广泛用于 `participant_cache`）
- 当前 `run_bus_listener` 中的事件分发逻辑需要重构——目前是解码后直接扇出到 All recipients，需要改为对每个 recipient 做授权检查

### 方向 C：分布式限流与可观测性增强（P1）

**为什么需要：** 登录限流器是内存 `HashMap`，在 3+ 节点集群中每节点独立计数。攻击者可以在 10 秒内向每个节点发送 100 次登录尝试，总计 300 次——绕过 100 次/10秒 的单节点限制。此外，当前速率限制器的 observability 信号有限——没有按工作区、按 IP 的拒绝计数仪表盘。

**核心挑战：**
1. Redis 限流引入网络往返延迟——本地桶 + 异步 Redis 写入的混合模型需要精心设计
2. 登录限流对延迟极为敏感——每个登录请求再等一次 Redis RTT（~1ms）会累积
3. 现有 `check_cluster_rate` 使用 `INCR` + TTL，窗口边界有竞态（incr 和 expire 不是原子的）

**预期架构变更：**
```
// 滑动窗口 + Redis Sorted Set（精确但昂贵）
// 或 令牌桶 + 定期 Redis 同步（轻量、接近精确）

// 推荐方案：分片滑动计数器
// 将 60 秒窗口分为 6 个 10 秒分片
// 本地内存缓存 + 每 10 秒异步合并到 Redis
// 读时：本地计数 + Redis 前 N-1 分片计数 = 当前速率
// 优势：99% 的读操作无网络 I/O，1% 的读操作需一次 Redis mget
```

**对现有系统的影响：**
- `AuthService` 需要接入 Redis 集群（目前只有 HTTP 限流器有 Redis 路径）
- 需要引入一个共享的速率限制库（目前 HTTP 限流的 `check_cluster_rate` 是内联实现）
- 可观测性增强是纯添加，无侵入性

### 方向 D：软删除一致性保障系统（P1→P2）

**为什么需要：** 当前 `soft_delete_in_tx` 只清理 messages 表，留下 6 个关联表的孤儿行。这不仅影响查询性能（未读计数包含已删消息的 reactions），还影响 GDPR 合规（删除参与者数据时留下痕迹）。随着时间推移，这些孤儿行会成为不可忽视的存储和性能负债。

**核心挑战：**
1. 关联表跨越多个 crate——reactions 在 `aero-im-core`，pins 可能在另一个模块，message_history 又在别处
2. 软删除可能频繁发生（每分钟数百次），级联清理不能阻塞事务
3. 某些关联表需要硬删除（reactions），某些需要保留但标记（message_history 的审计需求）

**预期架构变更：**
```
// Option A：迁移中加 CASCADE 约束（简单，但有锁表风险）
ALTER TABLE reactions 
    ADD CONSTRAINT fk_reactions_message 
    FOREIGN KEY (message_id) REFERENCES messages(id) 
    ON DELETE CASCADE;

// Option B：异步清理队列（不阻塞主事务）
// soft_delete_in_tx 中只记录需要清理的消息 ID 到 cleanup_queue
// 后台 drain 循环扫 cleanup_queue，对每个 message_id 执行：

async fn cascade_soft_delete(tx: &mut Transaction, msg_id: MessageId) {
    sqlx::query!("DELETE FROM reactions WHERE message_id = $1", msg_id).execute(tx);
    sqlx::query!("DELETE FROM receipts WHERE message_id = $1", msg_id).execute(tx);
    sqlx::query!("DELETE FROM pins WHERE message_id = $1", msg_id).execute(tx);
    // message_history: soft-delete 保留审计
    sqlx::query!("UPDATE message_history SET deleted_at = NOW() WHERE message_id = $1", msg_id);
}

// Option C（推荐）：混合方案
// 迁移加轻量级索引 + 清扫定时器
// 每 60 秒 select 已软删消息的关联表孤儿
// 批量 delete，受 limit 约束避免长事务
```

**对现有系统的影响：**
- Option A 最简单但需要 `ACCESS EXCLUSIVE` 锁（PG 17 支持 `NOT VALID` + `VALIDATE` 减少锁时间）
- Option B 需要引入一个新的定时器（当前定时期清单见 AGENTS.md §2，blob_gc_drain 可作为模板）
- 对查询路径的影响最小——当前 `list_by_room` 已过滤 `deleted_at IS NULL`

### 方向 E：WebSocket 连接层的架构抽象（P2）

**为什么需要：** 当前 `Hub`（`hub.rs`）的核心数据结构是 `Arc<RwLock<HashMap<ParticipantId, Vec<ConnState>>>>`，管理 WebSocket 连接的注册、扇出、清理。这个实现在单进程中工作，但随着功能增长存在几个限制：
1. 每个 participant 可以有多个连接，但当前连接管理没有区分连接类型（WS vs SSE vs 移动端长轮询）
2. 扇出时对所有连接顺序写——一个慢连接会阻塞同一 participant 的其他连接
3. 缺少连接元数据（连接时间、用户代理、last_ping）影响运维能力

**核心挑战：**
1. 引入连接类型区分意味着 Hub 的扇出逻辑需要感知目标端能力（如移动端可能只需要摘要而非全量帧）
2. 慢连接隔离需要引入 per-connection bounded channel 或 write-side timeout
3. 向后兼容——当前 WS 帧格式（`ServerFrame`）不能破坏，SSE 需要新帧类型

**预期架构变更：**
```
// 当前
pub struct Hub {
    conns: Arc<RwLock<HashMap<ParticipantId, Vec<ConnState>>>>,
}

// 目标：连接抽象层
#[async_trait]
pub trait ConnectionSink: Send + Sync + 'static {
    fn participant_id(&self) -> ParticipantId;
    fn connection_type(&self) -> ConnectionType;  // WebSocket | SSE | MobilePush
    async fn send(&self, frame: &ServerFrame) -> Result<(), SendError>;
    fn is_alive(&self) -> bool;
}

pub struct Hub {
    conns: Arc<RwLock<HashMap<ParticipantId, Vec<Box<dyn ConnectionSink>>>>>,
    // per-connection bounded channel 用于慢连接隔离
    fan_out_workers: Arc<Vec<JoinHandle<()>>>,
}
```

**对现有系统的影响：**
- 当前 `ConnState` 结构体需要实现 `ConnectionSink` trait
- `fan_out_raw` 需要改为遍历 `dyn ConnectionSink`，调用各自的 `send` 方法
- 连接注册/注销 API 不变，只是内部存储多态化
- SSE 支持可以基于此抽象增量添加

---

## 3. 接口设计建议

### 3.1 架构关键接口决策原则

**原则一：总线消息契约应显式声明消费方。** 当前 `im.room.*` 上挂载了 7 个 durable consumer，每个通过不同的 subject filter 或 queue group 接收相同的事件流。这导致了**隐式耦合**——新增一个 `RoomEvent` variant 时，必须检查所有 consumer 是否兼容。建议引入消息路由表：

```
// 显式声明：哪些 consumer 关心哪些事件
EventRouteTable::new()
    .on::<Message>(|e| vec!["aero-bot", "aero-ooo", "aero-unfurl", "aero-transcribe", "aero-push"])
    .on::<Notify>(|e| vec!["aero-push"])
    .on::<Deleted>(|e| vec!["aero-moderation"])
    .build()
```

这个路由表可以在编译期检查 variant 和处理者的匹配，避免运行时静默跳过。

**原则二：授权检查应位于消息解码之后、业务逻辑之前。** 当前总线的授权缺口本质上是**解码-扇出**和**授权**的次序问题。解决方案是在 `run_bus_listener` 中的 `handle_room_event_sub` 之前插入一个授权中间层，而不是在每个 bot 内部重复实现授权。

**原则三：跨 crate 边界用 trait + 数据传递，不用共享状态。** 当前 `Hub` 中 `Arc<RwLock<HashMap>>` 的直接共享模式在工作。但如果要引入 `ConnectionSink` trait，那么 Hub 应该只依赖 trait，而不是具体的连接实现。同理，对于跨 crate 的事件类型，serde `tag="kind"` 的设计虽有隐患（撞名），但模式本身是正确的——总线的契约就是序列化协议。

### 3.2 需要引入的新抽象层

**1. ServiceLoop trait（生命周期管理）**
- 解决 P1 优雅关闭问题
- 统一 18+ 个常驻循环（2 bus listener + 8 bot + 4 timer + 2 heartbeat + blob_gc + embedding_backfill + observability gauges）
- 提供嵌套取消、阶段化关闭、健康状态上报

**2. AuthorizationGate trait（总线授权）**
- 解决 P1 安全守卫缺失
- 提供可组合的检查链（成员关系 → 账户状态 → 2FA → info barrier）
- 内置 TTL 缓存减少 DB 压力

**3. RateLimiter trait（共享限流接口）**
- 解决 P2 分布式限流不一致
- 统一 HTTP 路径和 Auth 路径的限流语义
- 后端可切换（in-process / Redis / 混合）

### 3.3 向后兼容性策略

- **ServiceLoop 迁移**：保留现有 `pub async fn run_*()` 函数，在新 wrapper 中调用它们。可以逐个循环迁移，不需要一次性全部改动。
- **授权层引入**：在 `run_bus_listener` 的分发路径中插入可选的 `AuthorizationGate`——新部署开启，旧部署保持当前行为。通过 feature flag 控制。
- **Hub 连接抽象**：`ConnectionSink` trait 的引入对现有连接管理代码是向后兼容的——只需让 `ConnState` 实现 trait，然后替换存储类型。

---

## 4. 技术选型

### 4.1 推荐的引入与改进

| 领域 | 推荐 | 替代方案 | 理由 |
|------|------|---------|------|
| 生命周期管理 | **自建 `ServiceSupervisor`** | tokio-utils `CancellationToken` 组合 | 项目已有 `CancellationToken` 依赖，不需要新框架。`ServiceSupervisor` 提供关闭阶段排序 + 超时熔断 |
| 总线授权检查 | **`moka` 缓存 + 自建 `AuthGate`** | Redis 缓存（引入额外 RTT） | 成员关系缓存在进程级是安全的（变更会通过 WS 帧通知），moka 的 TTL + 容量限制正好适合 |
| 分布式限流 | **共享 `RateLimiter` trait + Redis 滑动窗口** | 纯本地桶（不跨节点）；纯 Redis（高延迟） | 混合方案已经被 HTTP 限流验证可行，只需将其提升为共享抽象 |
| 连接管理 | **`ConnectionSink` trait（自建）** | Tower `Service` trait（过于泛化） | 需要 IM 领域语义（participant_id、连接类型），通用 RPC 框架不适合 |
| 软删除一致性 | **PG 迁移 FK CASCADE + 后台清扫 drain** | 应用层级联删除（N+1 查询） | PG 17 的 `NOT VALID` + `VALIDATE` 可以在线加外键。清扫 drain 作为兜底 |

### 4.2 不推荐的引入

| 候选 | 不推荐理由 |
|------|-----------|
| **Kubernetes Operator** 用于生命周期管理 | 项目是单体 server（虽然多实例），不是微服务。K8s 层面已经提供 Pod 生命周期管理。进程内优雅关闭是 server 自身的责任 |
| **gRPC** 替代 NATS | 架构核心是事件驱动，NATS subject 的发布-订阅模型天然匹配。gRPC 是请求-响应模型，不直观 |
| **OpenTelemetry SDK** 替代现有 Prometheus 监控 | 项目已经使用 Prometheus + OTLP，没有引入新 SDK 的必要。OTLP 导出已在配置中 |
| **Diesel** 或 **SeaORM** 替代 sqlx | rust 的 ORM 对复杂查询（如搜索的 hybrid merge、pgvector 的 `<=>` 算子）不够灵活。sqlx 补偿了这一缺陷 |
| **Tokio Console** 作为默认诊断工具 | 开发阶段有用，但生产部署的额外开销（`tracing` 层）需要审慎评估 |

### 4.3 自建 vs 采购决策

对于 **Graceful Shutdown 框架**，建议**自建**。原因：
- 需要领域特定的关闭阶段（drain NATS → 关 Hub → 关 HTTP → 关 DB）
- tokio 的 `CancellationToken` 已经提供了取消信号，只需要在其上构建阶段编排
- 这个框架应该只有 200-300 行核心代码，不值得引入第三方

对于 **速率限制**，建议**自建抽象层**而不是引入 `governor` 等现成 crate：
- 需要 Redis 后端支持集群协调，governor 的 Redis 扩展不够成熟
- 需要 per-workspace 和 per-user 两级限流，governor 的多层 key 模型不够灵活
- 项目已经有一个工作的 `check_cluster_rate` 实现，提取为 trait 的开销很低

---

## 5. 实施路线图

### 5.1 优先级矩阵

| 优先级 | 方向 | 工期估计 | 风险 | 业务价值 |
|--------|------|---------|------|---------|
| **P0** | A：Graceful Shutdown 框架 | 2-3 周 | 中（需要理顺关闭顺序依赖） | 消除滚动更新/重启时的数据丢失 |
| **P0** | B：总线安全守卫 | 2-3 周 | 低（设计清晰，实现机械） | 关闭合规漏洞 |
| **P1** | C：分布式登录限流 | 1-2 周 | 低（已有 HTTP 限流的 Redis 路径可复用） | 关闭放大攻击面 |
| **P1** | D：软删除一致性 | 2-3 周 | 中（CASCADE 锁表风险/异步队列需要 drain 可靠性） | 数据质量 + GDPR 合规 |
| **P2** | E：连接层抽象 | 3-4 周 | 中高（影响核心的 Hub 扇出路径，需要充分的回归测试） | 为 SSE/移动端连接铺路 |

### 5.2 阶段划分

**阶段一：安全与可靠性加固（4-6 周）**

专注于 P0 方向 A 和 B：

| 里程碑 | 交付物 | 验收标准 |
|--------|--------|---------|
| M1 | `ServiceSupervisor` 核心 + 关闭阶段编排 | 所有常驻循环在收到 cancel 后 ≤ drain_timeout 内退出；关闭期间无静默丢事件 |
| M2 | `run_bus_listener` 和 `run_live_bus_listener` 接入生命周期 | bus listener 在 Hub drain 完成后才停止消费；drain 期间收到的事件被正确 ack/nack，不丢 |
| M3 | `AuthorizationFilter` 总线层 | 每个总线推送前检查接收方：成员关系 active、账户 active、2FA 完成；缓存 TTL ≤ 5s |
| M4 | 所有 bot 声明授权级别 | push_bot → `NeedNotifyAccess`，agent_bot → `NeedRoomAccess`，… |
| M5 | 8 个定时器接入生命周期 | 所有 `interval` 循环持有 cancel token；`MissedTickBehavior::Skip` 统一行为 |

**阶段二：限流与数据一致（3-4 周）**

| 里程碑 | 交付物 | 验收标准 |
|--------|--------|---------|
| M6 | 共享 `RateLimiter` trait + Redis 滑动窗口实现 | 登录限流器在 3 节点集群中精确限制 100 req/10s；`check_cluster_rate` 重构为该 trait 的默认实现 |
| M7 | 软删除 CASCADE 迁移 | `reactions`、`receipts`、`pins` 加 FK `ON DELETE CASCADE`（`NOT VALID` + 后台 `VALIDATE`） |
| M8 | `message_history` 清扫 drain | 每 120 秒批量 soft-delete 关联的 history 行；可配置 batch size 和间隔 |
| M9 | GDPR 删除完整性审计 | `participant.rs` 的删除列表检查：所有 12 张 participant-keyed 表全覆盖 |

**阶段三：架构抽象与扩展（6-8 周）**

| 里程碑 | 交付物 | 验收标准 |
|--------|--------|---------|
| M10 | `ConnectionSink` trait 提取 | `ConnState` 实现 trait；Hub 存储从 `Vec<ConnState>` 改为 `Vec<Box<dyn ConnectionSink>>` |
| M11 | 慢连接隔离 | 每个 `ConnectionSink` 有独立的 bounded mpsc；写超时 5s 后断开慢连接 |
| M12 | SSE 连接支持（可选） | 新增 `SseConnectionSink` 实现，SSE 端点使用与 WS 相同的事件总线 |
| M13 | 运维仪表盘增强 | Prometheus 指标：按连接类型计数、按 participant 的连接数、drain 时长、关闭阶段耗时 |

### 5.3 风险点与缓解策略

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| `ServiceSupervisor` 关闭顺序死锁（A 等 B，B 等 A） | **中** | 高——关闭 hang 住，K8s SIGKILL | 关闭阶段使用 timeout + fallthrough；在 `ServiceLoop` trait 上声明 `drain_timeout`，超时后 log warn 强制退出 |
| `AuthorizationFilter` 缓存与 DB 不一致导致误禁 | **低** | 中——合法用户收不到推送 | 缓存 TTL 设 5s 等价于「最多延迟 5s 感知权限变更」；成员变更事件触发缓存失效（通过总线） |
| FK CASCADE 锁表 | **中** | 高——生产表无法加外键 | 使用 PG 17 的 `NOT VALID` + `ALTER TABLE … VALIDATE CONCURRENTLY`；在维护窗口执行 |
| 连接抽象重构破坏现有扇出路径 | **中** | 高——消息延迟或丢失 | 严格的分步重构：先提取 trait → 单测 → feature flag 切换 → 删除旧路径；每个步骤有 diff review |
| 分布式限流器在 Redis 故障时 fail-open 导致限流失效 | **低** | 中——攻击窗口 | 当前 HTTP 限流已经 fail-open；可以改为 fail-close（Redis 故障时拒绝登录），但需要业务决策 |

### 5.4 不推荐的阶段一改动

- **不要一次性重构 Hub 的连接管理**。Hub 是扇出路径的核心，任何改动都影响所有实时功能。等阶段三安全加固完成后再动。
- **不要引入新 crate 依赖**。阶段一和阶段二的所有改动都可以基于现有依赖（tokio、CancellationToken、moka、fred/Redis）实现。新依赖应该在阶段三之前只经过 POC 验证。
- **不要动 RoomEvent/StreamEvent 的序列化格式**。总线消息格式的变更会影响所有 durable consumer 的游标位置，需要协调的版本升级。现有 `tag="kind"` 的设计虽然不完美，但工作正常。

---

## 总结

Aero IM 的架构基础非常扎实——事件 DAG 设计、crate 分层、Redis 作为集群事实源、AI 退化设计都是正确决策。当前的主要问题不是架构方向错误，而是**成熟度缺陷**：进程生命周期管理被忽略（P1）、安全守卫未覆盖所有入口（P1）、分布式限流有一半缺失（P2）、数据一致性有遗漏（P2）。这些都是从 MVP 到生产级 IM 系统必经的成长痛。

五个扩展方向各有责权：方向 A 和 B 是堵住当前最危险的坑，方向 C 和 D 是加固基础设施，方向 E 是为未来增长铺路。按照 P0→P1→P2 的节奏，6-8 周内可以完成最关键的可靠性加固，届时该系统将具备生产级 IM 平台所需的弹性、安全和数据质量基线。

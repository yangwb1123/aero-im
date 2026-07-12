现在我已经全面了解了代码库的架构，包括启动/关闭顺序、后台任务、限流模式、WS 扇出和运行时契约。以下是完整的架构分析。

---

# 架构分析：Aero IM — 5 个生产级缺口与超越

## 1. 架构评估

### 1.1 优势——什么做对了

Aero IM 在**事件驱动单体**类别的架构决策上是得当的。核心模式——单一进程内通过 `Hub` 进行 NATS JetStream 扇出的模式——对于目标规模（数千并发参与者）来说是正确的权衡。具体来说：

- **Durable consumers for IM events**（`aero-server` consumer group）是 at-least-once 交付的正确抽象。使用 JetStream 而非核心 NATS 确保了从重启中恢复。
- **进程内 `Hub` + Arc 共享**避免了为每个进程内事件进行额外的序列化/反序列化——扇出是 `Arc<String>` 克隆，而不是 JSON 重解析。对于单进程工作负载，这比通过 Redis Pub/Sub 扇出更高效。
- **分层 crate 依赖**（`aero-common` → `aero-bus`/`aero-storage` → `aero-im-core` → `aero-server`）防止了循环依赖，并在 crate 边界处强化了关注点分离。
- **`assert_room_access` 守卫链**是访问控制的正确位置——它在一个函数中集中了房间存在性、工作区成员身份、停用和 2FA 检查。这个**模式**是正确的；执行**覆盖率**才是问题所在（缺口 #2）。
- **WS 限流已经使用 Redis**——说明团队在需要时了解分布式模式。缺口 #4 在于这种模式没有被一致地应用。
- **`FOR UPDATE SKIP LOCKED` 的 AI worker 轮询**是水平扩展工作者而不发生惊群效应的正确模式。

### 1.2 局限性——四个架构债务累积

5 个缺口揭示了四个递归架构债务：

| 债务 | 体现于缺口 | 根本原因 |
|------|--------|-------------|
| **生命周期不作为** — 系统假设基础设施就绪等价于业务就绪 | #1（启动/关闭） | 启动流水线是线性的，没有就绪门控；关闭顺序尊重 TaskTracker 但不等消费者完成 |
| **安全作为横切关注点事后考虑** — 安全不变量是在功能路径内联实现的，没有正式的横切关注点模型 | #2（守卫裂痕） | 没有 `SecurityPolicy` 特质或集中策略评估点；每个新通路（webhook、push、bot）必须手动重新发现守卫 |
| **数据生命周期没有模型** — 数据被创建、被软删除，但删除的语义从来没有向上传播到依赖行 | #3（孤儿数据） | 软删除是一个标志位翻转；没有处理级联删除或异步清扫的统一 `DataLifecycle` 特质 |
| **限流器实现不一致** — WS 使用 Redis；HTTP 使用进程内 HashMap；登录使用 Mutex<HashMap>；每个都需要不同的运维知识来调整 | #4（分布式盲区） | 没有 `RateLimiterBackend` 特质；每个限流器都是从零开始实现的 |

第五个缺口（推-轮询不匹配）是**架构迁移**而不是债务：系统从纯轮询开始，添加了 WS 实时，但从来没有完成通知/未读状态推送的迁移。这是一个处于半完成状态的产品特性。

### 1.3 关键设计决策评估

| 决策 | 裁决 | 理由 |
|--------|--------|---------|
| 单进程 Axum + Hub 与 sidecar 模型 | ✅ 对 1k-10k 并发正确 | sidecar 增加了运维复杂度，而对于这种规模没有价值；可以后期升级到 sidecar |
| NATS JetStream 用于事件持久性 | ✅ 正确 | 为 event-sourced 架构提供了持久的订阅者光标；比 Kafka 更轻量 |
| 软删除与 `deleted_at` | ⚠️ 可接受但有债务 | 写入路径简单，但如果没有级联清理，读取路径（`list_pins`、`reactions_batch`）会降级 |
| 进程内 Hub 扇出 | ✅ 对单节点正确 | bounded mpsc 通道防止了背压；跨节点扇出由 NATS 处理 |
| 主机守卫在模块内使用 `assert_room_access` | ✅ 模式正确，但覆盖率不足 | 该函数做正确的事情；问题在于 15+ 个路径没有调用它 |
| `tokio_util::CancellationToken` 用于关闭 | ⚠️ 部分正确 | 用于 AI worker 和 goroutine，但**不是**用于 bus listeners——这是缺口 #1 的直接原因 |
| 与 `TaskTracker` 的闲置桶清理 | ✅ 正确 | 防止了所有进程内映射（限流器、spam guard、slow-mode）的无限增长 |
| 线程订阅通知 | ⚠️ 缺口 #2 的源头 | 在通知扇出前不检查收件人状态（停用、info-barrier） |

---

## 2. 扩展方向

5 个缺口本质上是修复；架构扩展则关乎构建下一步。以下 4 个方向是架构级的，建立在修复所揭示的模式之上。

### 2.1 方向 A：统一生命周期管理器（P0 · 架构基础）

**为什么需要：**

启动/关闭缺口（#1）的根本原因是生命周期逻辑分布在 4 个模块之间：`main.rs`（编排）、`background.rs`（spawn）、`serve.rs`（AXUM + drain）、`shutdown.rs`（信号处理）。没有单一的权威来源来决定「我们准备好了吗？」或「我们可以安全地停止吗？」

**业务价值：** 零停机滚动更新的先决条件。没有它，任何 Kubernetes 部署都会在每次发布时丢失事件。对于企业 SLA 至关重要。

**技术挑战：**

1. **定义 readiness**：「HTTP 服务器正在监听」vs「Hub 有连接」vs「所有消费者都已经建立」。这些是不同的就绪状态，不同的消费者关心不同的边界。
2. **关闭顺序**：当前的关闭序列是 `shutdown_signal → sleep → cancel → serve returns → tracker.close() → tracker.wait()`。正确的序列应该是 `shutdown_signal → pause consumers → flush in-flight → drain Hub → close HTTP → close consumers → clean up`。每一步都必须等待上一步完成。
3. **消费者感知**：`run_bus_listener` 目前是一个没有取消点的 `loop`。要使其可优雅停止，它必须（a）监听 `CancellationToken`，（b）完成当前事件的处理，然后（c）在关闭信号前**不要**确认新事件。

**预期架构变更：**

```
┌─────────────────────────────────┐
│      LifecycleManager           │
│  ┌─────────────────────────┐    │
│  │  Gates:                  │    │
│  │  - infrastructure_ready │    │
│  │  - consumers_ready      │    │
│  │  - hub_has_connections  │    │
│  │  - shutting_down        │    │
│  └─────────────────────────┘    │
│  ┌─────────────────────────┐    │
│  │  Phases:                 │    │
│  │  - PreConnect            │    │
│  │  - Connect               │    │
│  │  - StartConsumers       │    │
│  │  - Serve                 │    │
│  │  - DrainConsumers       │    │
│  │  - Disconnect           │    │
│  └─────────────────────────┘    │
└─────────────────────────────────┘
```

- `main.rs` 变为 4 行：`LifecycleManager::new().run().await`
- `background.rs` 将 `CancellationToken` 传播到**所有**消费者
- `run_bus_listener` 和 `run_live_bus_listener` 获得 `&cancel` 参数，并检查 `cancel.is_cancelled()` 以断开外层 `loop`
- `Hub` 获得一个 `drain()` 方法，该方法刷新待处理的扇出但拒绝新事件
- NATS consumer 在 `drain` 期间的 ack 行为被延迟，直到事件被扇出

**对现有系统的影响：** 中等。函数签名变化（添加 `CancellationToken`），但内部事件处理逻辑不变。`Hub::fan_out_raw` 不需要更改——只需要知道何时停止接受。

**选项：**

| 选项 | 复杂度 | 完整性 | 推荐 |
|--------|----------|------------|----------|
| A：原子旗就绪门控（一个 `AtomicBool` + 消费者中的 `tokio::select!`） | 低 | 中等（处理启动但关闭顺序不严格） | **第一阶段** |
| B：Async Rust 中的有限状态机生命周期 | 中 | 高 | **最终目标** |
| C：从重启恢复的外部监督器（K8s 边车） | 高 | 最高 | 超出了当前需求 |

### 2.2 方向 B：集中安全策略引擎（P0 · 安全架构）

**为什么需要：**

缺口 #2 的原因不是开发者懒惰；而是每个新消费路径（webhook、bot dispatch、push notifications）都必须从零开始发现安全不变量。没有单一的评估点。

**业务价值：** 企业合规审计的可证明安全。合规官需要声明「发送前的每条出站通知都会检查 `is_active` AND `NOT info_barred` AND `is_opted_in`」——用代码证明这一点比在 7 个模块中有 7 次 `if` 检查更容易，如果使用集中策略引擎的话。

**技术挑战：**

1. **统一调用点**：所有出站通知路径（push bot、webhook dispatch、thread notification、reaction notification、bot reply）必须先调用一个函数，而不是各自为政。
2. **急性与惰性评估**：有些守卫（`is_active`）应该在事件消费时检查（投递前拒绝该事件）。其他守卫（`info_barrier`）需要两个参与者的上下文——事件**发送者**和**每个**收件人。
3. **缓存**：为每次通知查询 `is_active` 代价高昂。策略引擎必须缓存参与者状态，并在 TTL 后或通过观察事件使缓存失效。
4. **审计**：策略拒绝应该被审计，以便合规官可以证明「这些推送因为停用而被阻止」。

**预期架构变更：**

```rust
// New trait in aero-common or aero-auth:
#[async_trait]
pub trait SecurityPolicy: Send + Sync {
    /// Can `sender` send a notification to `recipient` in this context?
    /// Returns Ok(()) or a structured denial reason.
    async fn can_notify(
        &self,
        sender: ParticipantId,
        recipient: ParticipantId,
        context: NotificationContext,
    ) -> Result<(), DenialReason>;

    /// Quick check: is this participant still active?
    /// Used for early-filter in bulk fan-out (NotifyBatch).
    async fn is_active(&self, participant: ParticipantId) -> Result<bool>;
}
```

- `AppState` 获得一个 `policy: Arc<dyn SecurityPolicy>` 字段
- `push_bot`、`webhook/delivery.rs`、`thread_subs.rs`、`bot_dispatch.rs` 都调用 `state.policy.can_notify(...)` 而不是内联检查
- 实现将 `ImService` 方法（`is_active`、`is_info_barred`、`is_muted`）包装到单一的编排检查中

**对现有系统的影响：** 中等偏高。`push_bot.rs` 和 `thread_subs.rs` 需要重构以使用策略引擎。但模式是明确的——将内联 `if` 语句替换为特质调用。

### 2.3 方向 C：分层限流基础设施（P1 · 安全与可靠性）

**为什么需要：**

缺口 #4 不仅仅是「修复 HTTP 限流器」——它是「完成限流架构」。目前有三个半限流器，每个都有自己的后端：

| 限流器 | 后端 | 范围 |
|-----------|--------|-------|
| HTTP 限流器 | DashMap（进程内） | 单节点 |
| 登录限流器 | Mutex<HashMap>（进程内） | 单节点 |
| WS 限流器 | Redis | 全局 |
| 工作区限流器 `WsRateEnforcer` | Redis（通过 `WsRateStore`） | 全局 |

**业务价值：** 多节点部署的安全性。对于任何拥有超过 1 个实例的产品部署来说都是门槛。当企业审计要求「每个工作区每秒限制 X 个请求」并且验证多个节点时，这是必需的。

**技术挑战：**

1. **延迟**：Redis `INCR` 往返增加约 1-3ms 到热点路径。对于登录限流（应该严格），这是可以接受的。对于 HTTP API 限流（`rate_limit.rs` 中的每个请求），这可能将 p99 延迟增加 5-10ms。
2. **Redis 故障模式**：正确的架构是**故障开放**（缺口 #3 分析同意这一点），但故障开放必须被衡量和告警。
3. **两级（L1 进程内 + L2 Redis）**：L1 可以处理突发并在没有 RTT 的情况下拒绝明显违规者；L2 执行全局上限。但这增加了复杂度。

**预期架构变更：**

```rust
// Trait in aero-storage
#[async_trait]
pub trait RateLimiterBackend: Send + Sync {
    /// Check a request against the limit. Returns Ok(()) or RateLimited.
    async fn check(&self, key: &str, limit: u64, window_secs: u64) -> Result<(), ()>;
}

// Implementations:
pub struct InProcessRateLimiter { /* DashMap-based token bucket */ }
pub struct RedisRateLimiter { /* INCR + EXPIRE */ }
pub struct TwoTierRateLimiter { /* InProcess + Redis fallthrough */ }
```

- `GatewayConfig` 获得一个 `rate_limit_backend: RateLimitBackend` 字段（`env: AERO_RATE_LIMIT_BACKEND = "in-process" | "redis" | "two-tier"`）
- 当前的 `rate_limit.rs` 和 `login_throttle.rs` 被重构以使用 `RateLimiterBackend`
- `ws_rate.rs` 中的 Redis 模式保留作为参考实现

**对现有系统的影响：** 中等。`rate_limit.rs` 需要重构以使用特质而不是具体的 `DashMap`；`login_throttle.rs` 需要相同的重构。逻辑保持不变。

### 2.4 方向 D：用于衍生状态的事件驱动状态同步（P1 · 架构迁移）

**为什么需要：**

缺口 #5 不是技术修复——它是系统从纯 REST 到混合 REST+WS 的架构迁移的最后 10%。**未读计数、通知徽章和在线状态**都是「衍生状态」：它们是根据主要事件（消息已读、通知已发送）计算得出的。在当下推送给用户是语义上正确的做法。

**业务价值：** 用户体验一致性——消除 6.5 秒的盲区。减少轮询负载（10K 用户时从 3000 req/s 减少到约 100 req/s）。移动端电池寿命改善。

**技术挑战：**

1. **状态一致性与事件顺序**：当用户快速连续阅读两条消息时，需要发送两个 `UnreadEvent`——但如果第二个事件在第一个事件被处理前到达，状态可能暂时错误。服务端需要序列化每个用户的事件，或者客户端需要幂等地处理乱序事件。
2. **写入放大**：每次 `mark_read` 调用都触发一个 `UnreadEvent`。对于批量操作（`read_all`），只应发送一个合并事件。
3. **降级**：当 WS 断开时，客户端必须优雅地退回到轮询。当前的行为（始终轮询，永远不推送）在断开连接时是正确的；新的行为（推送，轮询作为备选）需要仔细的状态协调。

**预期架构变更：**

```
WS 服务端                                  WS 客户端
   │                                          │
   │  ← 'msg:message' (实时消息)              │
   │  ← 'msg:unread' { room, count }         │  ← 新增
   │  ← 'msg:notification_count' { count }   │  ← 新增
   │                                          │
   │  [连接断开]                               │
   │                                          │  ← 退回到 REST 轮询
   │  [连接恢复]                               │
   │  ← 'msg:sync' { 完整状态快照 }            │  ← 新增
```

- `RoomEvent` 获得一个新的 `UnreadCount` 变体（或使用小的临时 NATS subject）
- `Hub` 获得一个 `fan_out_user` 方法（与 `fan_out_raw` 的逻辑略有不同——按用户路由而不是按房间）
- 客户端 `app.js` 监控 `msg:unread` 和 `msg:notification_count`，在 WS 连接时禁用轮询

**对现有系统的影响：** 中高。WS 帧协议扩展（新事件类型）、新的 `Hub` 方法、新的客户端事件处理程序。服务端变更集中在 `ImService::mark_read`/`mark_unread`——目前写入 `message_receipts` 但不发布衍生状态事件。

---

## 3. 接口设计建议

### 3.1 需要的新抽象层

| 抽象 | 属于 | 填补缺口 |
|-----------|---------|-----------|
| `LifecycleManager` | `aero-server` 或新包 `aero-lifecycle` | #1 |
| `SecurityPolicy` 特质 | `aero-common`（或 `aero-auth`）| #2 |
| `DataLifecycle` 特质 | `aero-storage` | #3 |
| `RateLimiterBackend` 特质 | `aero-common` 或 `aero-storage` | #4 |
| `StatePublisher` 特质 | `aero-server`（Hub 侧）| #5 |

### 3.2 设计原则

1. **先有特质，再有函数**：在实现 Redis 后端之前定义 `RateLimiterBackend`。特质是契约；实现可以稍后交换。
2. **横切关注点不得假设它们被调用的路径**：`SecurityPolicy::can_notify` 不关心调用者是 push bot 还是 webhook dispatcher——它检查参与者状态并返回决策。
3. **生命周期接口是协程，而不是回调**：`LifecycleManager` 通过 `CancellationToken` 协调阶段。它不调用回调；它切换门控值，消费者通过 `tokio::select!` 进行轮询。
4. **衍生状态事件不应出现在主要事件流中**：`UnreadCount` 不是 `RoomEvent`——它是 UI 状态更新。它应该使用不同的 WS 帧类型（例如 `msg:unread`）或一个单独的临时 subject。
5. **幂等性是接口契约**：`DataLifecycle::sweep_orphans(before: DateTime)` 必须是幂等的——第二次运行不应该删除新数据。极限边界必须明确文档化（例如法务保全跳过）。

### 3.3 向后兼容

**添加而不是修改**：现有的 `rate_limit.rs` 中的 in-process 限流器保持默认。基于 Redis 的后端是一个可选替换，通过配置选择。`run_bus_listener` 签名添加一个可选的 `CancellationToken`（`None` = 旧行为）。测试验证两种模式。

**灰度迁移**：WS 上的未读事件推送应通过 `AERO_UNREAD_WS_PUSH` 特性标志控制。当禁用时，轮询作为主要机制继续工作。当启用时，轮询作为备选机制继续工作。

**弃用窗口**：纯进程内限流后端在添加 `RateLimiterBackend` 特质后应**不**被弃用——小部署将永远使用它。相反，该特质是推荐的默认选项；进程内是单节点部署的简化选项。

---

## 4. 技术选型

### 4.1 新依赖评估

| 依赖 | 为什么不需要 | 更好的方法 |
|----------|----------------|----------------|
| Redis 限流库（例如 `redis_rate_limiting`）| 增加了脆弱的泛型抽象；团队已经展示出编写 50 行正确的 Redis 限流器（`ws_rate.rs`）的能力。一个内部特质 = 更少的依赖 + 精确匹配系统的类型。 | 使用 `aero-storage::WsRateStore` 作为模板，提取一个 `RateLimiterBackend` 特质 |
| 策略引擎（例如 `oso`、`casbin`）| 对于当前的安全需求来说太重了。策略是不变的：检查 `is_active`、`is_info_barred`、`is_muted`、`require_2fa`。策略语言增加了解析/评估开销，而没有提供任何当前不变量。 | 一个 50 行的 `SecurityPolicy::can_notify` 实现，包装现有的 `ImService` 方法 |
| 状态机库 | 生命周期管理器是状态机，但 Tokio 的 `select!` + `CancellationToken` 模式在 Async Rust 中已经很好地工作。一个库增加了另一个映射层。 | 在 `LifecycleManager` 内部使用 `tokio::sync::watch` 用于门控值 + `select!` 用于阶段转换 |
| 分布式追踪（OpenTelemetry）| **已经存在**（`aero_common::telemetry`）| 扩展追踪到消费者：为 bus listener 中的每个 `handle_room_event_sub` 添加 span |
| 更多关系型存储（CockroachDB、Spanner）| 对于当前规模来说没有价值。PG 17 + pgvector + pg_trgm 很合适。只读副本（`pg_read`）在需要时可用。 | 在需要的地方添加只读副本路由，但保留 PG |

### 4.2 自建 vs 采购的决策框架

对于这 5 个缺口，**在这 5 种情况下自建都是正确的答案**。原因如下：

| 功能 | 如果采购 | 使用自建 |
|----------|---------------|-------------|
| 生命周期管理 | 无法处理特定于域的消费者；通用库不知道 NATS consumers | 40 行编排 + 每个消费者 5 行 `CancellationToken` 检查 |
| 安全策略 | `casbin`/`oso` 增加了策略语言解析，而简单的不变量可以通过 3 个 `if` 语句检查 | `SecurityPolicy` 特质 + 包装现有方法的实现 |
| 数据生命周期 | 没有处理法务保全豁免的 ORM | 一次 DB 查询 `sweep_orphans` + 一个 `legal_holds` 连接 |
| 分布式限流 | 库嗅探速率限制，而 WS 速率已经是 Redis-backed | 将 `ws_rate.rs` 模式提取到 `RateLimiterBackend` 特质 |
| WS 推送状态 | 没有库能处理「在 WS 上推送衍生状态，回退到轮询」 | 5 行客户端 JS + 5 行服务端 Hub 方法 |

### 4.3 框架演进

该项目的技术栈（Tokio、Axum、sqlx、NATS、Redis、PG）是合适的，并且**不应该改变**。

然而，我认为有几个值得关注的框架演进：

1. **从 sqlx 迁移到 SeaORM 或 Diesel**？**不**——sqlx 是针对此工作负载的正确选择。该架构使用每个功能的仓库（`XRepo`），这已经是存储库模式。ORM 会在热点路径（`UPDATE messages SET soft_deleted = true WHERE id = $1`）增加不必要的抽象。

2. **使用 Tonic/gRPC 替换内部 REST**？**也许对于媒体路径**（由缺口 #2 中提到但不在 5 个中的 bridge）。但对于 IM 控制面，REST + WS 是正确的模式。

3. **添加 Kubernetes Operator**？**不是现在**。生命周期管理器（方向 A）使 K8s 滚动更新安全，而无需 operator。Operator 是有道理的，一旦系统扩展到 10 个以上的微服务（目前：1 个）。

---

## 5. 实施路线图

### 5.1 优先级排序

| 优先级 | 方向 | 理由 | 时间线 |
|----------|---------|----------|---------|
| **P0—必备条件** | 方向 A：生命周期管理器 | 没有这个，缺口 #1 就无法修复，所有滚动更新丢失事件 | 第 1-2 周 |
| **P0—必备条件** | 方向 B：安全策略引擎 | 缺口 #2 → 企业客户签约门槛。P0 因为每个新 bot 都会增加攻击面 | 第 2-4 周 |
| **P1—高影响** | 方向 C：分层限流 | 缺口 #4 → 多节点部署的安全性。P1 因为突破 1 节点设置需要这个 | 第 4-6 周 |
| **P1—数据完整性** | 缺口 #3 修复（孤儿清扫） | 存储效率 + 合规。P1 但责任明确——委托给 2 周的 sprint | 第 1-2 周（与方向 A 并行） |
| **P2—UX** | 方向 D：WS 状态推送 | 缺口 #5 → UX 改进。取决于方向 A（WS 推送需要可靠的生命周期信令） | 第 6-8 周 |

### 5.2 阶段划分

**阶段 1：「不会丢失事件」（第 1-2 周）**

里程碑：在 Y 个节点的滚动更新中零事件丢失

- 为 `run_bus_listener`、`run_live_bus_listener`、所有 bot dispatcher 添加 `CancellationToken` 传播
- 在 `background.rs` 中实现就绪门控（`tokio::sync::Barrier` 或 `watch` 通道）
- 硬编码的软删除孤儿清扫（`sweep_orphan_reactions`、`sweep_orphan_pins`）包含在 retention sweep 中
- 为 `TaskTracker` drain 超时添加 `AERO_TASK_DRAIN_SECS` 配置（默认从 10s 增加到 30s）

**阶段 2：「没有裂痕」（第 3-4 周）**

里程碑：合规审计可以证明所有出站通知都会检查参与者状态

- 设计和实现 `SecurityPolicy` 特质
- 重构 `push_bot`、`thread_subs`、`webhook/delivery`、`bot_dispatch` 以使用策略引擎
- 为被阻止的通知添加审计日志记录
- 添加 Prometheus 指标 `security_policy_denials_total`（按原因标记）

**阶段 3：「全局上限」（第 5-6 周）**

里程碑：3 节点部署不能绕过速率限制

- 实现 `RateLimiterBackend` 特质，包含进程内和 Redis 实现
- 重构 `rate_limit.rs` 和 `login_throttle.rs` 以使用后端
- 添加配置切换（`AERO_RATE_LIMIT_BACKEND=redis`）
- 添加两级模式（L1 进程内 + L2 Redis）作为默认选项

**阶段 4：「实时状态」（第 7-8 周）**

里程碑：未读计数和通知在 200 毫秒内更新，而不是 6.5 秒

- 在 `ImService::mark_read`/`mark_unread` 中实现未读事件发布
- 将 `Hub::fan_out_user` 添加到 Hub（按用户路由）
- 向 WebSocket 帧协议添加 `msg:unread` 和 `msg:notification_count` 事件
- 重构 `app.js` 以在 WS 连接时禁用轮询
- 添加 WS 断开连接时的轮询回退逻辑

### 5.3 风险与缓解

| 风险 | 可能性 | 影响 | 缓解 |
|------|----------|----------|-----------|
| 生命周期管理者的关闭顺序竞态（Hub 在消费者暂停后扇出） | 中 | 高 | 在取消消费者令牌之前使用 `tokio::sync::Barrier` 集齐所有参与者 |
| 安全策略引擎在负载下引入延迟（每个 `can_notify` 一次 PG 查询）| 中 | 中 | 在策略引擎内部实现 TTL 缓存；在 `ParticipantRepo` 上使用 `psql` 通知以实现失效 |
| 分布式限流的 Redis 故障 → 所有限流暂时禁用 | 低 | 高 | 故障开放 + 指标 + 告警；进程内 L1 在 Redis 故障时为单节点提供保护 |
| WS 未读推送使开发者关闭了轮询但 WS 静默断开 | 中 | 高 | 客户端健康检查：如果 WS 在 15 秒内没有收到任何消息，则回退到轮询 |
| 孤儿清扫队基于合法保留的消息 | 低 | 高 | 在 `legal_holds` 中添加显式 JOIN，永远不删除被保全消息的行 |

### 5.4 长期架构演化

在 5 个缺口和 4 个扩展方向修复之后，系统将处于更稳固的基础上。接下来是什么？

1. **第十七个 crate：`aero-lifecycle`**（P3）— 将生命周期管理器提取到独立的 crate，与 `aero-common` 解耦。使其成为可重用的 Tokio 生命周期原语。
2. **跨节点桥接完成**（记录在 AGENTS.md §2 媒体缝中）— 一旦生命周期管理到位，`call_bridge_supervisor` 和 `SfuMediaSession` 可以安全地执行。在没有生命周期管理的情况下，这些组件会在重启期间泄漏媒体 session。
3. **微服务拆分准备**（P4）— 有了 `SecurityPolicy`、`RateLimiterBackend` 和 `LifecycleManager` 特质，将 `aero-server` 拆分为 `aero-api-gateway`、`aero-im-service`、`aero-push-service`、`aero-webhook-service` 是一个整洁的架构工作，而不是重组噩梦。特质是服务边界。

---

## 总结

5 个缺口不是随机 bug——它们是系统尚未从**功能性单体**发展到**架构就绪系统**的症状。修复模式是明确的：

| 缺口 | 根本问题 | 架构解决方案 |
|---------|-------------|-------------------|
| #1 生命周期 | 线性启动，无就绪门控 | `LifecycleManager` |
| #2 守卫裂痕 | 安全作为事后关注点 | `SecurityPolicy` 特质 |
| #3 孤儿数据 | 无数据生命周期模型 | `DataLifecycle` 清理 |
| #4 限流盲区 | 限流后端不一致 | `RateLimiterBackend` 特质 |
| #5 推-轮询不匹配 | 尚未完成的半状态架构 | `StatePublisher` + WS 帧扩展 |

这 5 个特质的共同主题是：**显式契约胜于隐式约定**。有了显式特质，下一个添加通知路径的工程师就不必发现安全不变量——他们调用 `SecurityPolicy::can_notify`。下一位添加限流路由的工程师——他们调用 `RateLimiterBackend::check`。架构在特质处得到强制执行，而不是在代码审查处。

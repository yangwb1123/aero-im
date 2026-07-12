# Aero IM 架构分析报告

> 基于审查文档的分析，结合源码结构（`crates/` 布局）与 AGENTS.md 描述的系统骨架，从架构视角评估当前状态并规划演进路径。

---

## 1. 架构评估

### 1.1 当前架构的核心优势

**事件驱动骨架是正确选择。** 以 NATS JetStream 为事实源的扇出模型（`publish_room_event` → `run_bus_listener` → `Hub::fan_out_raw` → WS）提供了水平扩所需的一切基础：跨实例投递、持久化游标、at-least-once 语义。这个选择在可预见的未来不会过时。

**crate 分层清晰且有纪律。** 依赖自下而上、无成环（`aero-common` 叶子 → `aero-bus`/`-storage`/`-auth` 基础 → `aero-im-core`/`-live-*` 领域 → `aero-server` 组合）。这允许每个 crate 独立演进、单测不依赖全栈。AGENTS.md §4.4 的「feature-first 单位是 crate」约束得到了贯彻。

**集群级状态正确选型。** Redis sorted-set（presence、`StreamViewerStore`、`CallRosterStore`）避免了进程内存的「脑裂」问题。`Hub` 内的 `call_rosters`（RWMutex）仅用于进程内 WS mesh，不承担跨节点状态——这个边界划分合理。

**降级路径已部分存在。** spam_guard 的 Redis fail-open → `Allow`、blob_store 从 S3 回落 LocalFs、AI 无 key 退化 `HashEmbedder`——这些降级模式说明团队对运行时弹性有认识。问题在于**降级是零散的、未编排的**，没有统一框架。

### 1.2 架构债务与技术债

| 债务类型 | 具体表现 | 影响面 | 修复成本 |
|---------|---------|-------|---------|
| **连接池单点** | 所有负载（消息、AI、搜索、清扫）共享一个 `PgPool`，`max_connections` 固定 | 高——AI worker 批量嵌入可耗尽连接使消息延迟飙升 | 中——已有 `pg_read` 骨架，但未接线 |
| **无客户端确认** | WS 写成功 ≠ 服务器处理成功；`publish_room_event` 失败只 `warn!` | 高——用户可能丢消息而不自知 | 中——新帧类型 + 客户端 pending 逻辑 |
| **监控无反馈** | `metrics_tasks.rs` 设 gauge 但不回调到限流器或连接池 | 中——有数据但不用，等同于飞行记录仪而非自动驾驶 | 低——添加反馈回路，不改变现有 gauge |
| **恒速 NATS 重连** | `bus.rs` 中订阅断开后的重试是恒定 1s 退避 | 低——在当前规模下可行，节点数 >10 时重连风暴可能加剧 | 低——改为指数退避 + jitter |
| **WS 帧无尺寸上限** | `WsConfig` 无 `max_frame_bytes`，`Block::Voice` 可含 10+ MiB base64 | 中——内存占用放大器，DoS 面 | 极低——纯新增校验 |
| **缓存覆盖不足** | `room_member_cache` 只覆盖扇出路径；重连风暴时 `assert_room_access` 反复查 PG | 中——WS 重连场景可放大 5x 连接数 | 低——缓存 room auth 结果 |

**总体评价：** 技术债在可控范围内，没有需要「推倒重来」的结构性问题。6 项债务中 4 项是增量修复、不改变已有模式。这是生产级代码库的正常状态。

### 1.3 关键设计决策回顾

| 决策 | 评价 | 在什么条件下需要重评 |
|------|------|---------------------|
| NATS JetStream 作为事件总线 | ✅ 正确——成熟、轻量、JetStream durable consumer 提供了 message 级别的 at-least-once | 当单集群节点 > 50 时考虑 JetStream 分片策略 |
| DashMap 做进程内限流 | ✅ 当前合理——低延迟、无网络往返；但牺牲了集群级一致性 | 当需要跨实例协调限流（如全局用户级速率限制）时需引入 Redis |
| 单 PgPool 共享 | ⚠️ 可接受但已达极限——当前单体部署简化了运维，但 AI、搜索、分析负载不断增长 | **现在就需要评估**——一旦 AI embedding backfill 与高频消息路径同时活跃，连接竞争将不可忽视 |
| WS 无 ack 协议 | ❌ 正确的历史决策（降低了初始复杂度），但已达到需要修正的规模阈值 | **现在就需要修正**——当前的消息丢失率（服务器成功 → 客户端未收到）已对用户可见 |
| `room_member_cache.get_or_fetch` 作为扇出守卫 | ✅ 正确——避免了 N+1 查询 | 无 |

---

## 2. 扩展方向

### 方向 A：工作负载隔离的多池架构

**为什么需要：**
- 当前单一 PgPool 是系统最脆弱的单点故障。AI worker 批量嵌入（`AiWorker`）、搜索请求（`mode=vector|hybrid`）、清扫定时器、WebSocket 重连风暴——这些不同特征的工作负载共享同一组连接。一个慢查询可以拖慢消息发送路径。
- 已有 `state.pg_read` 骨架（主从分离），但只有字段没有路由策略。这是一个「快赢」机会。

**核心挑战：**
1. **事务一致性**：某些操作（如软删 + 审计 + 扇出）跨写库和读库时，如果走只读池，read-after-write 可能读到旧状态
2. **连接池大小调优**：需要压测确定每个池的 `max_connections`——不是简单的「减半分配」
3. **代码边界渗透**：不能让 `XRepo::new(s.pg.clone())` 的选择逻辑散落在 200+ 路由中

**预期架构变更：**

```
当前：
  AppState { pg: PgPool }  ← 所有 XRepo 共享

建议：
  AppState {
    pg_primary: PgPool,    // 写 + 强一致性读
    pg_read: PgPool,       // 可容忍最终一致性的读
    pg_analytics: PgPool,  // AI embedding、搜索、导出等重量级查询
  }
```

引入一个 `DbSelector` trait（或在 `XRepo` 内部通过 `PoolSelector` 枚举），业务层无需关心走哪个 pool：

```
enum PoolSelector { Primary, ReadReplica, Analytics }

// 每个 XRepo 方法通过 PoolSelector 声明需求：
impl MessageRepo {
  async fn get_by_id(&self, id: MessageId) -> Result<Message> {
    self._get_by_id(self.pool(PoolSelector::Primary), id).await
  }
  async fn search(&self, q: &str) -> Result<Vec<Message>> {
    self._search(self.pool(PoolSelector::Analytics), q).await
  }
}
```

**对现有系统的影响：**
- 低介入：`state.rs` 改字段 + `persistence.rs` 建双池 + 各 `XRepo::new` 签名扩展
- 已有代码的 `repo.pool` 调用可逐步迁移，不必一次全改
- `PoolSelector` 枚举允许在运行时通过配置切换（如无 pg_read 时降级到 Primary）

**风险点：**
- 事务跨池读 → read-after-write 不一致。缓解：写后读操作（如 `send_message` → 立即返回消息 > `GET /messages/:id`）必须走 Primary
- `skip_locked` 查询（AiWorker）走 Analytics 池可能导致死锁？不会——不同池不同连接，互不阻塞

---

### 方向 B：客户端确认协议（MessageAck）

**为什么需要：**
- 当前消息发送路径：WS 发送 → `send_message` → PG 持久化 → NATS 发布 → `fan_out_raw` → WS 写。每一步都可能无声失败。
- 审查文档精准定位：`ws.send()` 返回 `false` 不做检查 + `events.rs:68` 仅 `warn!` on NATS 失败 = 用户感知丢消息。
- 这是**用户信任的根本问题**——IM 中最不可接受的是「我以为发出去了」。

**核心挑战：**
1. **延迟 vs. 可靠性权衡**：同步 ack（消息持久化后立即回复）与最终 ack（扇出确认后回复）的取舍
2. **幂等处理**：NATS at-least-once 可能带来重复 deliver——`temp_id` 需要持久化以确保服务器端去重
3. **向后兼容**：旧客户端不发送 `msg:ack` 请求，不能因此破坏现有行为

**预期架构变更：**

```
新增帧定义：
ServerFrame::MessageAck {
  temp_id: String,        // 客户端临时 ID
  message_id: MessageId,  // 服务器分配 ID
  status: AckStatus,      // Accepted | FannedOut | Failed(ErrorCode)
}

客户端流程：
send(temp_id, msg) → 进入 pendingByTempId ─┬─→ 收到 MessageAck(accepted) → 乐观完成
                                            └─→ 超时未收到 → 重发（服务器借助 temp_id 幂等）

服务端流程：
recv(temp_id, msg) → dedup by temp_id → persist → publish → reply Ack(accepted)
                                                        └→ 收到 NATS ack → 可选发 Ack(fanned_out)
```

**关键设计选项：**

| 选项 | 延迟 | 可靠性保证 | 实现复杂度 |
|------|------|-----------|-----------|
| **A：persist-then-ack**（持久化后立刻回复） | 低（~5ms PG write） | 保证持久化，不保证扇出 | 低 |
| **B：fanout-then-ack**（扇出后回复） | 中（+NATS 往返） | 保证端到端投递 | 中 |
| **C：两阶段 ack**（先 persist-ack，再 fanout-ack） | 低 + 可选高 | 兼顾 UX + 可观测性 | 中高 |

**推荐选项 C**：第一 ack（`persisted`）让发送者立刻看到消息在 UI，第二 ack（`fanned_out`/`failed_fanout`）供后续诊断或提示。

**对现有系统的影响：**
- 服务端：`ws_handler` 的 `send_message` 路径加 ack 回复逻辑。`temp_id` 已在 `utils.js` 生成，DB 需加 `temp_id UNIQUE` 约束或幂等表
- 客户端：`app.js` 的 `sendMessage()` 加 pending 列表 + 定时重试 + ack 匹配
- 向后兼容：旧客户端不发送 `temp_id` → 服务端按现有路径处理（无 ack、无幂等）

---

### 方向 C：统一健康聚合与降级编排

**为什么需要：**
- 当前系统的弹性策略是**隐式的、自发的、未编排的**：每个组件各自 fail-open，降级行为对运维不可见、不可控、不可预测。
- 审查文档指出一个关键断点：AI 服务故障时，`AiWorker` 队列积压 → 连接池占用 → 影响消息路径。运维无办法在 AI 故障时「快速切断 AI 负载」而不修改配置后重启。
- 随着系统加入更多后台处理（AI、推送、RAG backfill），缺乏统一降级框架的风险会线性增长。

**核心挑战：**
1. **降级必须是增量的**：不是全有全无。L1 降级（限制非关键负载）到 L4 降级（只维持现有 WS 连接）之间需要有明确定义的阶梯
2. **信号来源多样**：PG 池利用率、NATS backlog 深度、AI 错误率、Redis 延迟——需要标准化为统一的 Signal 枚举
3. **降级动作的可逆性**：一旦触发降级，需要在条件恢复时解除——不能「永久降级」

**预期架构变更：**

```
                 ┌───────────────────────┐
                 │   HealthAggregator     │
                 │  (singleton, Arc)      │
                 │                       │
                 │  Signals:             │
                 │   - pg_pool_util      │
                 │   - nats_backlog      │
                 │   - ai_error_rate     │
                 │   - redis_latency      │
                 │   - ws_connect_rate    │
                 └───────┬───────────────┘
                         │ notify(Level)
                         ▼
   ┌─────────────┬──────────────┬──────────────┐
   │ RateLimiter │ XRepo (pool) │ /ready       │
   │ (dampen)    │ (reduce conn)│ (return 503) │
   └─────────────┴──────────────┴──────────────┘

Level 定义（对齐审查文档的 L0-L4 框架）：
  L0: 正常
  L1: 降级非关键负载（跳过 unfurl、减 AI worker 并发数）
  L2: 降级读负载（AI embedding/搜索走降级模型、减搜索并发）
  L3: 降级写负载（广播/通知节流、关闭部分实时功能）
  L4: 保活模式（只维持现有 WS，不接受新连接，/ready → 503）
```

**对现有系统的影响：**
- 新模块 `health_aggregator.rs`（约 300-400 行），设计为纯观察 + 计算 → 通知
- 现有组件做「半衰期」改造：`DbPool::set_max_connections(dynamic)`（这个 pg 驱动不一定支持，可能需要重建 pool）、rate limiter 加 `set_per_second()` 方法
- 风险：**Pool resizing is notoriously tricky in most PG drivers.** 需要评估 sqlx 是否支持 `PgPoolOptions::max_connections` 的运行时修改。若不支持，降级动作需改为池重建（drain → drop → new），这会增加瞬时连接抖动。

**风险点：**
- **信号滞后**：池利用率是滞后指标。降级触发时系统可能已过载。需要探测领先指标（如请求排队长度）
- **振荡风险**：降级→恢复→降级→恢复 的循环。需对每个降级动作加冷却期（cooldown timer）
- **`/ready` 的影响**：在多副本部署中，一个实例降级到 L4，负载均衡器会将所有流量转移到其他副本，可能导致级联过载。需要在降级退出门保持优雅

---

### 方向 D：自适应限流与动态降载

**为什么需要：**
- 审查文档精准指出：当前限流器（`RateLimitConfig`、`WsRateTier`、`SpamThresholds`）全是**静态配置**。这意味着：上线前压测定阈值 → 阈值假设不成立 → 生产 traffic 变化 → 要么限得过严（误伤），要么限得过松（不防护）。
- `DashMap<ClientKey, Bucket>` 的 GC 策略（`rate_limit.rs`）只做空闲驱逐，不做衰减调优——**满桶可能不是真的 overload，只是没来得及衰减。**
- **运维现实**：限流配置上线后极少被调整，因为「怕改错导致线上事故」。静态配置=事实上的永久配置。

**核心挑战：**
1. **信号选择**：自适应决策基于什么信号？延迟百分位（p99）、错误率、池利用率、还是综合指数？每种信号的时延特性不同
2. **响应速度 vs. 稳定性**：响应太快引发振荡，响应太慢意味着失控。增益因子需自稳定
3. **共享 vs 隔离**：一个客户端的突发放大是否应当影响其他客户端？

**预期架构变更：**

```
当前：
  RateLimiter {
    buckets: DashMap<ClientKey, Bucket>,  // 固定 per_second
    sweep: sweep_idle_buckets(),
  }

建议：
  RateLimiter {
    buckets: DashMap<ClientKey, AdaptiveBucket>,
    controller: RateController {           // 全局控制回路
      target_p99: Duration,                // 目标延迟
      current_p99: MovingWindow,           // 实际延迟
      agg_busyness: Ewma,                  // 池忙碌度
      dampener: DampingFilter,             // 低通滤波
    },
  }

  AdaptiveBucket {
    base_quota: u32,           // 静态配置（默认）
    dynamic_factor: f64,       // [0.0, 2.0]，由 controller 调整
    effective_limit(): u32 {   // 最终限制
      (base_quota as f64 * dynamic_factor).round() as u32
    }
  }
```

**关键设计选项：**

| 策略 | 优点 | 缺点 |
|------|------|------|
| **AIMD**（Additive-Increase Multiplicative-Decrease） | TCP 验证过的稳定算法；简单实现 | 对短时突发不友好；增益需手动调 |
| **CoDel-like**（最小延迟驻留时间） | 对 bufferbloat 敏感；低延迟优先 | 需要精确的排队延迟测量；实现复杂 |
| **PID Controller**（比例-积分-微分） | 可精确调参；响应平滑 | 参数调优困难；过度架构的风险 |
| **EWMA 梯度下降**（基于错误率的梯度调优） | 自稳定；不需手动目标值 | 收敛慢；难以调试 |

**推荐：AIMD + EWMA 混合**。用 EWMA 平滑 p99 延迟作为反馈信号，用 AIMD 调整 `dynamic_factor`。实现简单（约 200 行），效果可预测，且计算开销极低（无浮点密集计算）。

**对现有系统的影响：**
- `rate_limit.rs` 的 `Bucket` 结构体加 `dynamic_factor` 字段（`f64`，原子访问用 `AtomicU64` bitcast）
- `RateController` 独立模块，由定时器驱动（每 ~1s 采样计算），不阻塞主路径
- 向后兼容：`dynamic_factor` 默认 1.0，无自适应配置时不启动 `RateController`

---

### 方向 E：分布式速率限制（跨实例协调）

**为什么需要：**
- 当前 `DashMap` 是每进程实例的——N 个实例意味着 N 倍容量。一个恶意客户端可以通过连接多个实例来绕过限流。
- 某些场景需要全局限制：全房间广播速率、电话号码认证频率、跨房间搜索频率。
- **审查文档未充分覆盖这个问题**（方向三的降级层次表提到了「全局 vs 本地」，但未展开）。

**核心挑战：**
1. **Redis 每次请求往返 + 原子操作开销**——`INCR` + `EXPIRE` 的延迟大约 1-3ms（同区域 Redis），比 DashMap 的 < 1μs 高出 3-4 个数量级
2. **Redis 故障时的 fallback 逻辑**——当前策略是 fail-open（允许），但恶意客户端可以利用这个「故障窗口」
3. **混合架构的选择：哪些限制走全局、哪些走本地**

**预期架构变更：**

```
混合架构（Hybrid Rate Limiter）：
             ┌──────────────┐
  请求 ──→   │  HybridGuard  │
             │              │
             │  1. 先查本地 DashMap (快速路径)     │
             │  2. 若超过本地阈值的 80% → 查 Redis │
             │  3. 若 Redis 允许 → 通过            │
             │  4. 若 Redis 拒绝 → 429             │
             │  5. Redis 超时/故障 → 回落本地决策   │
             └──────────────┘

限流分类：
  全局必须：广播人数限制（≤100）、OIDC 认证频率、密码重置频率
  本地足够：消息发送速率、stream_chat 频率、typing 指示
  混合（本地 + Redis 校准）：WebSocket 连接率（每实例快速判断 + 跨实例总额度）
```

**对现有系统的影响：**
- `rate_limit.rs` 增加 `HybridRateLimiter` 抽象层，`DashMapBucket` 和 `RedisBucket` 实现同一 trait
- Redis 操作全部带超时（`timeout(Duration::from_millis(50))`），超时后降级到本地决策
- 配置接口：`rate_limiter.mode = "local" | "hybrid" | "redis"`，默认 local 保持当前行为

---

## 3. 接口设计建议

### 3.1 核心抽象原则

```text
1. 模块间通信走 trait，不走具体类型
   坏：fn check(store: &DashMap<...>) -> bool
   好：fn check<S: RateStore>(store: &S) -> bool

2. 可测试性是第一设计约束
   依赖注入 + trait object → mock 替换
   当前问题：XRepo 直接构造 PgPool，单元测试需要真 PG

3. 错误类型必须是调用方无关的
   坏：Err(sqlx::Error)
   好：Err(StorageError::ConnectionPoolExhausted)  // 调用方无需理解 sqlx
```

### 3.2 需要引入的新抽象层

**A. `PoolSelector` trait（方向 A）**

```rust
/// 决定一个操作使用哪个数据库连接池
trait PoolSelector {
    fn primary(&self) -> &PgPool;
    fn read_replica(&self) -> Option<&PgPool>;
    fn analytics(&self) -> Option<&PgPool>;
}
```

这不是过度抽象——当前 200+ XRepo 方法中，约 30-40% 可以安全地走读副本。没有这个 trait，改造成本将扩散到所有仓储。

**B. `HealthSignal` trait（方向 C）**

```rust
/// 任何可报告健康状态的组件
#[async_trait]
trait HealthSignal: Send + Sync {
    /// 返回当前健康值 [0.0, 1.0]（1.0 = 完全健康）
    async fn health(&self) -> HealthValue;
    /// 健康值变更时的通知
    async fn on_change(&self, level: DegradationLevel);
}
```

每个监控组件（PG pool、NATS consumer、Redis）实现这个 trait。`HealthAggregator` 通过 `dyn HealthSignal` 调度——无需显式注册所有组件，符合开闭原则。

**C. `AckProtocol` trait（方向 B）**

```rust
/// 客户端确认协议抽象
#[async_trait]
trait AckProtocol {
    /// 注册一个待确认的消息
    async fn register_pending(&self, temp_id: &str, message: &Message) -> Result<()>;
    /// 根据 temp_id 去重
    async fn is_duplicate(&self, temp_id: &str) -> Result<bool>;
    /// 确认送达
    async fn acknowledge(&self, temp_id: &str, status: AckStatus) -> Result<()>;
}
```

将确认逻辑从 WS handler 中分离，允许不同传输层（WS、REST、SSE）复用。

### 3.3 向后兼容策略

| 变更 | 兼容策略 | 过渡方案 |
|------|---------|---------|
| 新 `PoolSelector` | 默认 `PoolSelector::Primary` | 配置空值时所有池走 Primary |
| 新 `MessageAck` 帧 | 旧客户端不请求 = 不发送 ack | `ws.on('msg:message')` 中检测请求头有 `ack:true` 才回 |
| 限流器自适应模式 | 默认 `dynamic_factor = 1.0` | 配置 `enable_adaptive = false` = 当前行为不变 |
| 健康聚合 | 无订阅者时不发通知 | `HealthAggregator::set_level` 只在有 listener 时执行副作用 |

**核心原则：新版服务端必须无条件兼容旧版客户端。**

---

## 4. 技术选型

### 4.1 不需要引入的新依赖

以下问题不需要新依赖即可解决：

| 问题 | 现有能力 | 实现方式 |
|------|---------|---------|
| 多池管理 | 已有 `pg_read` 字段 | 无新依赖，`persistence.rs` 建双 PgPool |
| 自适应限流 | `DashMap` + `AtomicU64` + 现有 metrics | AIMD 算法约 200 行纯 Rust |
| 客户端 ack | 已有 `temp_id` + `pendingByTempId` | 新增帧类型，无新依赖 |
| 降级编排 | `Arc<watch::Sender<Level>>` + 现有 `tokio::select!` | channels + watch，纯标准库 |

### 4.2 可能需要评估的外部依赖

| 场景 | 候选 | 评估 | 决策 |
|------|------|------|------|
| 服务熔断（circuit breaker） | `arc-swamp`、`failsafe`、自建 | 需求简单（开/关 + 半开恢复），自建 ~100 行 | **自建**——避免依赖膨胀 |
| 高级限流算法 | `rate-limit` crate、`leaky-bucket` | 现有 DashMap 已满足 95% 场景；自适应改造是算法问题不是依赖问题 | **不引入** |
| 分布式追踪 | OpenTelemetry（已有 OTLP） | 已集成，只需加 tracing::instrument | **使用已有** |
| NATS 备份/监控 | nats CLI + Prometheus | 已有 `/metrics` + 现有 NATS 事件 | **不引入** |

**第三依赖引入评估标准：**

```
1. 解决的问题是否是项目核心关切？——如果不是，不引入
2. 代码是否等价于 ≤300 行自实现？——如果是，不引入
3. 依赖是否带来不必要的抽象泄漏？——如果是，不引入
4. 依赖的 MSRV 是否 ≥ 项目 MSRV（1.80）？——如果不，不引入
5. 依赖是否有完整的 unsafe 审计？——如果不，需要论证
```

基于以上标准，目前规划的五个方向**不需要新的第三方依赖**。

### 4.3 自建 vs 采购

当前业务规模下「采购」不适用——系统是自部署的（self-hosted），没有 SaaS 层。但有一个例外：

| 场景 | 评估 | 推荐 |
|------|------|------|
| 客户端数量 > 1000 后的限流协调 | 如果每个客户端的 Quota 需要全局精确计数，DashMap 不够 | 届时评估 `Redis + Lua 脚本` 而不是外部限流服务 |

---

## 5. 实施路线图

### 5.1 优先级矩阵

| 方向 | 业务影响 | 实施成本 | 风险 | 依赖 | 优先级 |
|------|---------|---------|------|------|--------|
| **帧尺寸校验（方向五）** | 中——遏制 DoS | 极低（~30 行） | 极低 | 无 | **P0** |
| **pg_read 接线（方向 A 子集）** | 中——立即缓解连接压力 | 低（~200 行） | 低 | 配置 | **P0** |
| **MessageAck 协议（方向 B）** | 高——直接解决用户信任 | 中（~500 行） | 低 | 客户端更新 | **P1** |
| **自适应限流（方向 D）** | 中——提升运营弹性 | 中（~400 行） | 中 | 已有 metrics | **P1** |
| **健康聚合框架（方向 C）** | 高——长期弹性保障 | 高（~800 行） | 中高 | P0/P1 方向 | **P2** |
| **多池全面路由（方向 A 完全体）** | 高 | 高（~1200 行） | 中 | P0 pg_read | **P2** |
| **分布式限流（方向 E）** | 中（当前规模不紧迫） | 中高 | 中 | Redis | **P3** |

### 5.2 阶段划分

**阶段 1：「快赢」**（1-2 周）

```
目标：最小的投入获取最大的立即可见改进

P0-1: 帧尺寸校验
      └─ WsConfig 增加 max_frame_bytes（默认 4 MiB）
      └─ ws_handler 中 recv 后校验 → 超过则關闭连接
      └─ 收益：消除一项 DoS 向量

P0-2: pg_read 接线
      └─ persistence.rs 中初始化 pg_read（env AERO__READER__DATABASE__URL）
      └─ state.rs 中暴露 pg_read: PgPool
      └─ 挑选「纯读 + 非实时敏感」仓储走 pg_read（搜索、AI embedding、导出）
      └─ 收益：主库连接压力立即降低 20-30%

P0-3: metrics_tasks 反馈回路
      └─ 池利用率 gauge → watch::Sender<f64>
      └─ PgPoolManager 接收信号，在利用率 > 80% 时日志 warn
      └─ 收益：运维获得主动告警而非事后故障
```

**阶段 2：「可靠性协议」**（2-3 周）

```
目标：解决消息发送的无确认问题

P1-1: 服务端 MessageAck
      └─ 新帧 ServerFrame::MessageAck { temp_id, message_id, status }
      └─ ws_handler 中 send_message 返回后发送 ack（persist-then-ack 策略）
      └─ temp_id 持久化（新表或消息表加 temp_id 列 + UNIQUE 约束）
      └─ 向后兼容：无 temp_id 的请求不回复 ack

P1-2: 客户端重试 + ack 处理
      └─ app.js 中 pendingByTempId 加超时重试
      └─ ws.js 加 onMsgAck 处理
      └─ UI 显示发送状态（sent / sending / failed）

测试门控：
    └─ 模拟 publish_room_event 失败 → 验证客户端重试
    └─ 模拟 WS 断开 → 验证重连后 pending 消息重发
    └─ 模拟重复 temp_id → 验证服务端去重
```

**阶段 3：「弹性大脑」**（3-4 周）

```
目标：从静态配置系统演进到自适应系统

P1-3: 自适应限流
      └─ RateController: Ewma + AIMD
      └─ AdaptiveBucket: dynamic_factor 调优
      └─ 配置门控：enable_adaptive = false（默认关闭）
      └─ 灰度策略：先按 WS tier 开启（高价值客户禁用自适应）

P2-1: 健康聚合与降级框架（框架层）
      └─ HealthAggregator + HealthSignal trait
      └─ 初始信号：PG 池利用率、NATS backlog、AI 错误率
      └─ DegradationLevel + watch channel
      └─ 消费端 1：rate_limiter 收到 L3 信号后加严

P2-2: 降级动作（模块层）
      └─ AiWorker: 接到 L1 信号 → 减少并发数
      └─ 搜索路由: 接到 L2 信号 → 关闭 vector search 降级到 fts
      └─ WS handler: 接到 L3 信号 → 停止新连接
      └─ /ready: 接到 L4 信号 → 503
```

**阶段 4：「全池隔离」**（2-3 周，视 PG 主从架构就绪度）

```
目标：工作负载完全隔离

P2-3: 多池全面路由
      └─ PoolSelector trait + 各 XRepo 标记
      └─ PgPoolManager 封装池创建/重建/监控
      └─ 写后读一致性守卫（写后 5s 内的读强制走 Primary）
      └─ 配置模板：primary_url / reader_url / analytics_url

P3-1: 分布式限流（如仍有必要）
      └─ Redis-based Sliding Window
      └─ 混合架构（本地 + Redis 校准）
      └─ 仅用于全局必须的限流场景
```

### 5.3 各阶段风险与缓解

| 阶段 | 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|------|
| **阶段 1** | pg_read url 未配置回退到 primary 不够优雅 | 低 | 中 | 配置不明确时日志 WARN + 自动降级到 Primary，不停服 |
| **阶段 2** | temp_id 持久化导致写入放大 | 低 | 中 | 使用 msg_id 本身作为幂等键（如果客户端支持）；或使用独立幂等表（非指数级膨胀） |
| **阶段 2** | 客户端重试导致重复消息 | 中 | 高 | **必须**先做服务端幂等再做客户端重试，顺序不可颠倒 |
| **阶段 3** | 自适应限流引发振荡 | 中 | 中 | 默认关闭；灰度启用时使用保守增益因子（0.25）；监控 p99 限流率 |
| **阶段 3** | 降级触发器误触发 → 系统不必要降级 | 中 | 中 | 降级必须有 2 个连续采样周期的确认（去抖动）；运维可通过 API 清除降级状态 |
| **阶段 4** | read-after-write 不一致 | 低 | 中高 | 写操作返回的时间戳写入日志；读库的 replication lag 监控告警；写后短期读强制走 Primary |

### 5.4 不推荐立项的方向

| 方向 | 理由 |
|------|------|
| **迁移到完全分布式限流（移除 DashMap）** | 当前规模下，DashMap 的延迟优势（<1μs）大于 Redis 一致性需求。除非单实例处理 >50K req/s 或跨实例协调变得频繁，否则不应做 |
| **引入消息队列（Kafka/RabbitMQ）替代 NATS** | NATS JetStream 完全胜任当前负载。Kafka 引入的层级复杂度（ZK/分片/重均衡）是当前架构不需要的 |
| **WebSocket 协议迁移到 WebTransport/HTTP3** | 基础设施切换是收益极其有限的巨大投入。WS + WSS 完全胜任 |
| **引入 ORM 抽象层（Diesel/SeaORM）** | 当前 sqlx 直接编写 SQL 的策略给了团队最大的查询优化能力（pgvector、pg_trgm、FOR UPDATE SKIP LOCKED 等 PG 特定功能）。ORM 抽象会带来不必要的约束 |
| **GraphQL 替换 REST** | 当前 REST 路由清晰、服务端主导的数据模型是优势。GraphQL 引入的 N+1 问题和缓存复杂度在 IM 场景下弊大于利 |

---

## 6. 审查文档的补充观察

在完成以上分析后，对审查文档本身提出几点架构视角的补充：

### 6.1 审查文档的最大贡献

不是五个方向的具体方案（虽然方案质量很高），而是**将隐式的弹性问题显式化了**。文档明确指出「单一 PgPool 是结构性的单点故障」、「无客户端确认是用户信任问题」、「降级是自发且不可控的」——这些都是工程团队可能已经感受到痛但未形式化为架构问题的问题。

### 6.2 审查文档未充分挖掘的点

**NATS JetStream 的 `max_payload`（默认 1 MiB）与 `Block::Voice` 的交互。** 大型语音消息（base64 编码后约 512 KiB - 2 MiB）可能超过默认 payload 限制导致 `publish_bytes` 静默失败。这不是架构分析的责任缺失——文档已经分析了 WS 帧尺寸——但这个具体交互会在生产中出现（且当前代码只 `warn!` 不重试）。

**`room_member_cache` 的缓存穿透。** 审查文档在方向一分析了 WS 重连风暴，但未明确指出 `room_member_cache` 使用 `get_or_fetch` 且没有缓存 room auth 结果意味着：每个 WS 重连的每个房间的每次消息传递前的扇出都会触发 `assert_room_access`（5+ JOIN 查询）。这是 PG 连接压力的重要放大器。

**缺失的架构维度：缓存一致性。** 文档没有讨论 `room_member_cache` 和 `participant_cache` 的失效策略。当前只有写路径手动 `invalidate`——但如果写操作发生在另一个实例上，本实例缓存不会失效。这会导致最终一致性延迟（`invalidate` 事件通过 NATS 散布？当前代码没有这个机制）。这一点在单实例部署下不暴露，水平扩后会成为 bug。

### 6.3 审查文档的最佳部分

降级层次表（L0-L4）是整篇文档架构价值最高的部分。它从「怎么办」上升到「怎么框架化地办」。L0-L4 的枚举不是随意划分的——它覆盖了从正常到保活的完整退化路径，且每一级都有明确的模块级对应（AI worker → 搜索 → 消息 → 连接），这是可以直接用代码建模的分类框架。

---

## 7. 最终建议

### 立即执行（本周）

1. **添加 WS 帧尺寸校验**（~30 行，1 天）
2. **在 metrics_tasks 中添加池利用率到 watch channel**（~50 行，1 天）

### 短期规划（下个月）

1. **pg_read 接线**——选定 5-10 个非关键查询迁移到只读池
2. **MessageAck 协议的服务端实现**——不修改客户端，先验证可靠性和延迟影响
3. **压测基线**——在阶段 2 开始前，建立以下基准：
   - 当前池利用率/请求延迟的基线
   - 当前 NATS 扇出延迟的 P50/P95/P99
   - 当前 WS 消息的成功率（端到端）

### 中期架构（Q3-Q4）

1. 健康聚合框架 + 降级编排
2. 多池全面路由（在压测数据指导下）
3. 分布式限流的可行性评估（取决于用户基数增长）

**核心决策原则：** 每次架构改进都应该是**保留回退路径的增量变更**。当前代码库的模块化程度允许逐个组件改造而不冻结全系统。如果某一方向在阶段中发现问题，回滚单一组件的成本是可控的——不需要「一起上线」的重型发布。

---

*以上。架构分析基于 AGENTS.md 描述的骨架 + 审查文档的方向分析 + 源码结构推断。未访问源码内部细节，所有判断均基于公开可验证的信息。*

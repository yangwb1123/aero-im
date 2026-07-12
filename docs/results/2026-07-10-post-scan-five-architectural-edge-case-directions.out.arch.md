现在我有了完整的图景。这是全面的架构分析。

---

# 架构分析：Aero IM — 系统性架构评估与扩展方向

## 1. 架构评估

### 1.1 当前架构的优势

**事件驱动骨架是坚实的选择。** NATS JetStream 作为跨实例事实源，结合进程内 Hub 扇出，提供了一个简洁的水平扩展模型。架构在几方面做得很好：

- **清晰的 crate 分层。** 16 个 crate，依赖方向明确（基础 → IM/直播 → 组合），无循环依赖。每个 crate 是一个 feature 单元，这遵守了文档的「feature-first is crate」声明。
- **基于 Redis 排序集的集群状态。** presence、stream viewer、call roster 均使用 Redis，而非不可靠的进程内存。这是正确的分布式设计选择。
- **业务层和媒体层的关注点分离。** SFU（`aero-live-webrtc`）和通话编排（`aero-im-call`）有独立的 crate，注册表明确分离（`RtcConfig` vs `SfuRouter`）。
- **连接池已有进阶加固。** 与交叉验证文档声称的不同，代码库已经具备 `statement_timeout=10s`、`test_before_acquire(true)`、`acquire_timeout=30s` 和 `pg_read` 副本池。这比典型的创业/初创阶段系统更好地抵御慢查询。
- **幂等 ON CONFLICT DO NOTHING 已部署在关键路径上。** OOO bot 使用它来防止重复 DM、room `add_member` 使用它保护成员关系。但这不是一致的全局模式。

### 1.2 关键架构债务

| 债务 | 严重性 | 细节 |
|--------|----------|---------|
| **`room_member_cache.invalidate` 已定义但零调用者** | **P0 — 安全** | 在 `room_member_cache.rs:111` 中定义。从不从任何 `add_member`/`remove_member`/`leave_room`/`kick` 路径调用。缓存 TTL 为 60s，因此被踢出的成员仍会在 60s 内收到事件扇出，并通过 `assert_room_access`（信任缓存的 DB 查询，而非缓存的成员列表）继续操作。这违反了文档的不变量，该不变量声明「成员关系写入（添加/移除成员）使缓存的条目失效」。 |
| **`/health` 使用主连接池** | **P0 — 可靠性** | `routes/health.rs:34` 在 `s.participants.pool()` 上运行 `SELECT 1` —— 与所有 HTTP handler、AI worker 和 sweepers 共享的同一个池。当池被耗尽时（例如，缓慢的 PG 查询占用了所有槽位），健康检查会失败，返回假阴性，导致编排器终止一个完全健康的 pod。这是一种级联故障场景，在没有独立健康池的情况下无法被根本解决。 |
| **连接池无 workload 分区** | **P1 — 操作/可靠性** | 三个连接池「类别」缺失：(a) **维护池**用于 sweepers/AI worker/bots——这些运行长时间查询（嵌入、批处理），不应与 HTTP handler 争夺槽位。(b) **健康池**——在不需要让所有 slab 参与者共享的专用连接上进行探测 (c) **无 `application_name`**——无法在 `pg_stat_activity` 中按 workload 区分连接。 |
| **`pg_read` 副本广泛未使用** | **P1 — 容量** | 搜索、AI usage、analytics 正确使用 `pg_read`。但通知扇出、房间成员列表获取、存在检查、消息历史及其他大量只读查询仍使用主池。文档称 `pg_read` 存在，但 adoption 范围很小。 |
| **无幂等键头 / Saga 编排** | **P2 — 数据完整性（长期）** | 关键副作用路径（golive_bot 通知、webhook 投递、推送通知）没有端到端幂等性。付费/外发/计费侧效应缺少幂等键。文档的 `at-least-once 状态机` 约束已识别，但未系统部署：golive_bot 显式标记「非幂等」，push_bot 标记「非幂等」。webhook 投递有 `mark_failed_with_backoff`，但没有端到端幂等性。 |
| **多节点缓存一致性模式缺失** | **P2 — 正确性（多节点）** | `participant_cache` 和 `room_member_cache` 都是每进程 TTL 缓存，没有跨节点失效机制。`participant_cache.invalidate` 仅用于 2 个路由（`update_me`/`session`撤销）。AI 上下文失效（`cache_answer_invalidate_room`）是唯一使用 Redis 的失效机制，但它只覆盖 AI 上下文。缺乏通用的 pub/sub 失效模式。 |
| **WS 协议瓶颈** | **P3 — 性能（高流量时）** | 纯 JSON、无 deflate、无批处理、无多路复用。对于实时 IM 而言目前可以接受，但对于直播弹幕和大型频道则是显著的瓶颈。 |
| **无多租户 RLS** | **P3 — 治理（长期）** | 访问控制在应用层（*代码*强制），而不是数据库层（RLS 策略）。这意味着租户隔离完全取决于 Rust 代码中正确的 `assert_room_access` 调用。`authz_lint` CI 扫描有所缓解，但 RLS 将是一种纵深防御。 |

## 2. 扩展方向

### 方向 A：连接池分区 —— 按 Workload 隔离（P0）

**为什么需要：** 三个不同工作负载竞争同一个 PG 池：HTTP handler（快速、OLTP 型查询）、AI worker（嵌入/摘要——可能运行 2-10s）、定期 sweepers（批处理删除/保留）。当 AI worker 或 sweeper 消耗 max_connections 时，只有 2s 超时进行健康检查的 HTTP handler 也会失败。

**核心挑战：**
- 所有存储库都通过 `PgPool` 创建，没有池选择机制——每个 `XRepo::new(pool)` 使用传入的任何池。代码库需要在池选择方面有意识的架构，而非在每个调用点传入 `pool`。
- 分区需要配置默认值，以便单一连接设置「正常工作」（这正是目前的模式）。

**预期的架构变更：**
```
AppState {
    pg: PgPool,           // HTTP handler + 快速 OLTP（当前 50 个连接）
    pg_maintenance: PgPool, // sweepers、bots、AI worker（10 个连接，60s 超时）
    pg_health: PgPool,    // 独立探针池（2 个连接，1s 超时）
    pg_read: PgPool,      // 已存在（search、analytics、AI usage）
}
```
- 所有 `routes.rs` handler 保持使用 `s.pg`。
- `background.rs`（sweepers、bots）和 `AiWorker` 改为使用 `s.pg_maintenance`。
- `routes/health.rs` 使用 `s.pg_health` 进行 `SELECT 1`。
- `application_name` 通过 `after_connect` 按池设置。

**对现有系统的影响：** 每个存储库的构造位置都需要更改以选择正确的池。但无新 trait 或类型——只是将不同的 `PgPool` 值传递给 `XRepo::new()`。

### 方向 B：Redis 支持的跨节点缓存失效模式（P1）

**为什么需要：** 当前，节点 A 上的 `add_member` 会使节点 A 的 `room_member_cache` 失效（如果调用者记得），但不会使节点 B 的失效。同时，参与者在节点 B 上通过 WS 连接。节点 B 会继续将不存在的房间内成员的帧扇出给节点 B 上过时的 WS 连接。这增加了带宽浪费，并可能违反数据约束。

**核心挑战：**
- 需要轻量级——每次成员添加/删除时进行 Redis pub/sub（类似于 NATS subjects 但针对缓存粒度）。
- 不能阻塞——失效应该是 fired-and-forgotten。
- 节点应当优雅处理部分失效（处理重复、已离开的房间等）。

**预期的架构变更：**
```
ImService::add_member/remove_member 之后：
  1. DB 写入（当前）
  2. Redis PUBLISH "cache.inval.room_member" room_id
  3. 本地 room_member_cache.invalidate(room_id)

每个节点上的后台监听器：
  SUBSCRIBE "cache.inval.room_member"
    当收到时 → room_member_cache.invalidate(room_id)

对于 participant_cache 也是如此。
```
- `RoomMemberCache` 获得一个 `subscribe_and_invalidate(redis)` 方法，该方法生成一个后台任务。
- 对于每个进程路由——所有 `add_member`、`remove_member`、`kick`、`leave_room` 路径都需要失效。

**对现有系统的影响：** 仅添加；目前的路径已经需要失效。Redis pub/sub 的开销可忽略不计（每秒几千条消息）。如果 Redis 不可用，TTL 回退。

### 方向 C：端到端幂等性模式（P1-P2）

**为什么需要：** 文档的 `at-least-once 状态机` 约束正确识别：「付费/外发/计费侧效应必须具有幂等键」。目前，webhook 递送有重试，但每次重试都可能重新创建相同的 webhook 调用。golive_bot 显式标记为「非幂等」。推送通知缺少幂等性。没有 `Idempotency-Key` HTTP 头。

**核心挑战：**
- 引入幂等键会增加存储开销（每个幂等键一行）和延迟（检查+插入）。
- 需要确定幂等范围（每个端点？每个用户？每个付费意图？）。
- 需要优雅处理幂等键过期。

**预期的架构变更：**
```
中间层模式：
  1. 从 `Idempotency-Key` 请求头中提取（或对于内部作业，从 job_id 派生）
  2. 使用 ON CONFLICT DO NOTHING 的幂等性检查：
     INSERT INTO idempotency_keys (key, created_at) VALUES ($1, NOW())
  3. 在冲突时，返回缓存的结果（如果可用），或 409 Conflict。
  4. 在成功时，缓存结果在幂等性行中（可选）。
  5. 超过 24h 的键被后台清理。

发生变化的路径：
  - webhook 递送（幂等性密钥：webhook_id + delivery_attempt）
  - golive_bot 通知（每个 follower 幂等：stream_id + follower_id + status）
  - push_bot 通知（每个设备幂等：event_id + device_token）
  - POST /api/rooms/:id/messages（每个 `Idempotency-Key` 请求头）
```

**对现有系统的影响：** 增量的。`Idempotency-Key` 头的处理可以以每个路由为基础部署。内部幂等性（bots、workers）可以通过将现有的 `ON CONFLICT DO NOTHING` 模式系统化来处理，而不是创建一个新的幂等性表。

### 方向 D：使只读查询能够统一使用 Read Replica（P2）

**为什么需要：** `pg_read` 池已存在，但只有 3 个模块使用它。许多读查询（通知扇出、`list_since`、`rooms_for`、`member_role`、`is_member`、存在检查、消息历史）仍命中主池。当主池压力高时，只是读取而不写入的操作会加剧问题。

**核心挑战：**
- 写入后需要立即读取一致性的路径（例如，在 `add_member` + `room_member_cache.invalidate` + 读取之后）不能安全使用 replica。Replica 滞后意味着页面刷新显示空白成员列表。
- 存储库没有「使用 replica」/「使用 primary」的概念——它们接收单个 `PgPool`。

**预期的架构变更：**
```
在每个 XRepo 中添加一个方法对：
  XRepo {
      pool: PgPool,       // primary（写入 + 写入后读取）
      read_pool: PgPool,  // replica（仅读取，快照一致性）
  }
  // 实现时，只读方法使用 self.read_pool，写入方法使用 self.pool

哪些方法可以安全使用 replica：
  - history / list_since / list_recent
  - search（已使用 pg_read）
  - 分析 / AI usage 报告
  - 通知检索
  - 频道列表 / 房间枚举

哪些必须使用 primary：
  - is_member（在 assert_room_access 中）——需要立即一致性
  - add_member / remove_member 之后的成员列表
  - 消息插入后的消息检索
```

**对现有系统的影响：** 存储库构造需要 `(pool, read_pool)` 而非单个 `pool`。这是一个大规模的签名更改，但可以逐步完成（首先将 `read_pool` 作为可选添加，只有明确的读取路径切换到它）。

### 方向 E：WS 协议优化与多节点扇出效率（P3 但高 ROI）

**为什么需要：** JSON-only 扇出为高带宽直播场景（弹幕、礼物、观看者计数更新）创建了瓶颈。每个 `RoomEvent` 被分解为每接收者 JSON 帧（在 `NotifyBatch` 展开后）。随着消息频率和房间规模的增加，这向每进程 Hub 发送队列以及最终每 WS 连接的发送缓冲区施加了压力。

**核心挑战：**
- WebSocket 帧压缩（permessage-deflate）不是开箱即用的——需要 WS 栈支持（当前是自定义的，基于 axum/tungstenite）。
- 批处理需要更改服务端和客户端协议。客户端需要在发送帧时解包。
- 对于直播弹幕而言，多路复用不太相关（弹幕已经是多路复用的），但会减少连接的帧数。

**预期的架构变更：**
```
更现实的方法（而非完全协议重写）：
  1. 启用 permessage-deflate（如果 WS 栈支持的话）
  2. 每 tick 批处理通知（例如，每 50ms 将待处理帧合并到一个服务器发送的 JSON 数组中）
  3. 对于高容量事件（直播观看者计数、礼物可访问性），使用紧凑的帧格式：
     ServerFrame::StreamViewerCount { stream, count }
     ServerFrame::GiftBatch { stream, gifts: Vec<Gift> }
  4. 在直播直播场景中选择性地跳过 JSON 美化打印
```

**对现有系统的影响：** 对协议有影响——客户端必须解码批处理帧。可向后兼容（使用一个 `v` 协议版本头或一个 `batch: true` 字段）。这是一个高努力、中等影响的变更。

## 3. 接口设计建议

### 3.1 存储库模式 —— 需要池选择抽象

当前的存储库模式是 `XRepo { pool: PgPool }`，方法直接使用 `self.pool`。对于写路径来说这很好，但对于读副本选择来说就成了问题。

**建议：** 引入一个轻量级的 `RepoPool` 封装，指定 primary 和 replica 池选择：

```rust
// 概念性 API（非代码）
pub struct RepoPool {
    primary: PgPool,    // 写入 + 写入后立即读取
    replica: PgPool,    // 仅读取，最终一致
}
impl RepoPool {
    pub fn for_read(&self) -> &PgPool { &self.replica }
    pub fn for_write(&self) -> &PgPool { &self.primary }
}
```

每个存储库使用 `RepoPool`，写入方法使用 `for_write()`，读取方法根据一致性要求使用 `for_read()` 或 `for_write()`。这比让每个调用者传入一个池更加结构化和可审计。

### 3.2 缓存失效 —— 需要一个统一的机制

目前，有两种缓存（`ParticipantCache`，`RoomMemberCache`）具有相似的失效模式。存在一个 AI answer 缓存（`AiContextStore`），它使用 Redis 进行失效。

**建议：** 一个通用的 `CacheInvalidation` trait 或一个简单的失效总线：

```rust
// 概念
pub trait Invalidate {
    type Key: Display;
    fn invalidate(&self, key: &Self::Key);
}

// 本地实现：DashMap.remove(key)
// 分布式实现：Redis PUBLISH + LISTEN
// 复合实现：本地 + 分布式两者
```

这将允许逐步替换：目前，所有失效都使用本地实现。当部署跨节点失效时，存储库切换到复合实现，无需更改调用者。

### 3.3 `ImService` 与 AppState 大小

`AppState` 目前承载 35+ 个字段（PG 池、20+ 个存储库、5+ 个 rate limiter、AI 后端、缓存、Hub、总线等）。这很灵活（每个 handler 可以提取它需要的任何字段），但这使单元测试变得复杂（需要构建完整状态），并鼓励绕过有意抽象的导入路径。

**建议：** 增量地将「子域」分组到聚合状态对象中：

- `StorageState { pg, pg_read, rooms, messages, participants, ... }` —— 所有 PG 后端存储库。
- `CacheState { participant_cache, room_member_cache, redis_client }` —— 所有缓存。
- `AiState { ai_backend, ai_jobs, moderation_config }` —— AI 相关配置。

每个 handler 提取 `State(StorageState)`、`State(CacheState)` 等。这不是一个紧急的需求，但随着组件数量的增长（目前是合理的），这可以防止过度耦合。

## 4. 技术选型

### 4.1 不变的内容：无需替换

这些技术栈选择很稳重，不应该被替换：

| 组件 | 为什么坚持 |
|--------|----------------|
| **NATS JetStream** | 完美契合事件驱动架构。持久游标提供 at-least-once 投递，临时消费者提供直播扇出。没有更好的消息代理选择。 |
| **Postgres 17 + pgvector** | pgvector 消除了独立的向量数据库。trgm 支持模糊搜索。一个 DB 搞定所有 —— 无多数据库协调开销。 |
| **Redis 7** | 存在检查、rate limiter、缓存 TTL、StreamViewerStore、CallRosterStore。没有替代品能提供相同的每操作延迟。 |
| **str0m（纯 Rust WebRTC）** | 避免了对 libwebrtc 的 C++ 绑定的依赖。可以精确控制媒体面。对于 CI 测试至关重要（无浏览器依赖）。 |
| **sqlx（异步 PG 驱动）** | 编译时查询验证，合理的性能，tokio 原生的。无替代方案提供的优势能抵消失去编译时检查的成本。 |
| **tokio** | Rust 生态系统标准。 |

### 4.2 何时引入新的依赖项

**需要限制添加依赖项。** 16 个 crate 遵循严格的层结构。以下是一组针对新引入的决策标准：

1. **值得新依赖吗？** 如果少于 200 行 Wrapper 可以完成相同的工作（例如，如果只是围绕 Redis LIST 模式的 100 行包装器），则不需要新 crate。
2. **它能在 Cargo 树的许多部分免于争用吗？** 与 tokio/tower 等深度编织的框架不同，专用的工具（validate、uuid 的 cron 解析）是安全的。
3. **它需要 `unsafe` 吗？** 项目有 `unsafe_code = "forbid"`。具有 `unsafe` 的 crate 需经过详细的理由审查。
4. **它编译得快吗？** 庞大的 proc-macro crate（例如，与 JSON schema 生成器、OpenAPI 代码生成）对增量构建时间有实际成本。

### 4.3 具体建议：考虑引入的内容

- **`uuid`**（已存在）。很好。
- **`fred`**（已存在，适用于 Redis）。很好。
- **`serde`** / **`serde_json`**（深度使用，很好）。
- **无 OpenAPI / 契约驱动框架。** 文档已经手写，增量成本很低。自动生成的价值很低，但 CI 负担（同步问题、构建时间）很高。
- **无 gRPC / protobuf。** 逻辑位于进程内；只有 NATS 是跨实例的。NATS 已在使用 JSON。protobuf 将为事件序列化增加一个额外的模式编译步骤，没有任何运行时性能收益。
- **当扩展规模超过一个 PG 实例时，考虑连接池代理（PgBouncer、PgCat）。** 这将允许 `min_connections` 保持较低的值（每个进程 2 个），同时允许多个进程在没有连接耗尽的情况下运行。**在拥有 5 个以上节点之前，这是不必要的。**
- **如果 WS 压缩成为热点，考虑 axum/Tungstenite 之上的 `tungstenite` `WebSocket<MaybeTlsStream<TcpStream>>` 层。** 目前使用的自定义 WS 实现可能缺乏 permessage-deflate。检查现有的 WS 栈。

## 5. 实施路线图

### 阶段 1：立即（P0 漏洞 —— 安全和可靠性）—— 1-2 天

| 步骤 | 变更 | 风险 |
|------|--------|------|
| 1.1 | **`room_member_cache.invalidate` 在每个成员变更路径中**：`add_member`（routes.rs:1070）、工作区添加/删除成员通过级联的房间成员资格撤销、`ImService::add_member`、房间移除成员。在 ImService 中添加 `remove_member` 方法。 | 低——1 行代码插入。 |
| 1.2 | **`/health` 的健康专用 PG 池**：一个新的 `PgPool::new_with_options`，max_connections=2，acquire_timeout=1s。`AppState::new` 在启动时创建它。只有 health handler 使用它。 | 低——两个连接，明确隔离。 |
| 1.3 | **在 member 变更路径中添加 `participant_cache.invalidate`**。当前只有 2 个调用站点；需要添加：工作区成员添加/移除时会级联移除房间成员（因此必须使 `room_member_cache` 失效）。 | 低——遵循 1.1 的模式。 |

**验证：** `cargo test --workspace --lib` 绿色。手动在 `/health` 上进行冒烟测试。写入 test 以验证 is_member 在带/不带失效的情况下添加/移除成员后是否返回最新状态。

### 阶段 2：短 term（P1 工程 —— 池隔离、副本使用）—— 1 周

| 步骤 | 变更 | 风险 |
|------|--------|------|
| 2.1 | **创建 `pg_maintenance` 池**（max_connections=10，acquire_timeout=60s，statement_timeout=30s）。让 bots（`agent_bot`、`ooo_bot`、`unfurl_bot`、`transcribe_bot`）、worker（`AiWorker`、sweepers 在 `background.rs` 中）和 moderation_bot 使用这个池。 | 中等——需要跟踪每个存储库调用者的池创建。 |
| 2.2 | **添加 `application_name`** 到每个池的 `after_connect`，用于 `pg_stat_activity` 分析：`'aero-http'`、`'aero-maintenance'`、`'aero-health'`、`'aero-read'`。 | 低——三行更改。 |
| 2.3 | **审计只读路径以切换到 `pg_read`**。高价值的候选项是：`notification` 展开（bus 处理程序中的通知扇出）、`list_my_rooms`、`history`/`changes`（如果立即一致性不是关键的）、`member_role`/`is_member` 用于非写入路径。 | 低到中——需要小心不要使用 replica 来编写后续读取路径。有助于在开发副本时保留。 |

**验证：** 使用 `pg_stat_activity` 检查连接按预期分组。使用高读负载进行基准测试，验证 `pg_read` 是否减轻了主池的压力。

### 阶段 3：中期（P1-P2 —— 一致性、跨节点缓存）—— 2 周

| 步骤 | 变更 | 风险 |
|------|--------|------|
| 3.1 | **基于 Redis 的缓存失效总线**。在 `redis_client` 之上的一个轻量级包装器——`PUBLISH` / `SUBSCRIBE`——用于驱逐键。`RoomMemberCache` 和 `ParticipantCache` 获得一个可选的 `invalidation_receiver: watch::Receiver<Set<Key>>`。 | 中等——如果 Redis 不可用，则回退到 TTL。不会丢失事件，因为节点在重新连接时会恢复 TTL 行为。 |
| 3.2 | **端到端幂等性模式：`Idempotency-Key` HTTP 头支持**。Axum 中间件提取头、检查/存储密钥、处理结果。在可能发生冲突的端点选择加入：`POST /api/rooms/:id/messages`、`POST /api/streams/:id/gift`、`POST /api/webhooks/:id/trigger`。 | 低——增量部署。每个路由可以独立选择加入。幂等键表可以被后台清理定期清除（对于清理已经有一种模式）。 |
| 3.3 | **Bot/pusher 幂等性：** golive_bot、push_bot、webhook_delivery 获得每操作幂等键。 | 低到中——为幂等键引入可重用的 PG 模式，用于内部作业，而不是从头开始写 SQL。 |

**验证：** 集成测试，使用 `Idempotency-Key` 头重放请求，验证只产生一个副作用。跨节点测试（如果可能，或系统测试设计），其中在一个节点上添加用户对在另一个节点上结束的 WS 连接在 <TTL 内可见。

### 阶段 4：长期（P2-P3 —— 协议优化、RLS）—— 可选的，业务驱动

| 步骤 | 变更 | 风险 |
|------|--------|------|
| 4.1 | **WS 压缩（permessage-deflate）**。如果 WS 栈支持则添加。否则，考虑升级。 | 低——如果支持，则为零成本变更。如果不支持，则需要 WS 栈升级，可能带来突破性变更。 |
| 4.2 | **批处理通知事件**。Hub 获得一个可选的批处理模式：在 50ms 窗口内累积同一房间的帧，发送一个 `ServerFrame::Batch { frames: [...] }`。Web 客户端解码时解包它们。 | 中——协议变更。需要客户端支持，但可以优雅降级（客户端发送 `accept_batch: true` 以选择加入）。 |
| 4.3 | **RLS（Row-Level Security）策略用于深度防御。** 不需要迁移——可以创建 SQL 策略，引用当前的 `app.current_participant_id` 会话变量。`assert_room_access` 除现有的 Rust 检查外还设置 `SET app.current_participant_id = '...'`。CI `authz_lint` 调整为接受 Rust + RLS 双重防护。 | 低到中——PG 策略是增量式的。每个表可以独立添加。现有的 Rust 检查无论如何都会通过，因此风险是最小的。 |

**验证：** 允许 `authz_lint` 对于有 RLS 覆盖的表放宽 Rust-only 检查。基准测试显示在 RLS 策略就位时 `SELECT 1` 的额外开销。

### 风险矩阵

| 风险 | 轻微影响 | 缓解 |
|------|----------------|----------|
| 池分区增加了配置复杂性 | 如果默认值被错误设置，开发设置会失败 | 默认值反映单一连接设置；仅当配置了第二个池时才激活分区。 |
| 跨节点缓存失效增加了 Redis 负载 | 每个房间成员变更涉及一次额外的 Redis `PUBLISH` | 此负载可以忽略不计（每秒几百条跨集群消息）。 |
| `Idempotency-Key` 添加到写入延迟 | +1 DB 往返以检查/插入幂等键 | 幂等键表是 `ON CONFLICT DO NOTHING` 和基于内存索引的。写入路径无论如何都要进行多次 DB 往返（消息插入、事件发布等）。 |
| pg_read replica 滞后导致读取过时数据 | 用户看到陈旧的 UI 状态（例如，在退订频道后显示为成员） | 只有明确标记为「最终一致」的路径才使用 replica。写入后立即读取路径继续使用 primary。所有现有测试继续保持立即一致性。 |

### 总结

该项目总体上设计精良，但遭受两个典型的成长痛苦：（1）**连接管理**在共享的单一 PgPool 上混合了 OLTP 和批量工作负载，（2）**缓存失效**模式被定义但未完全部署，违反了自己的不变量。

P0 项目（`room_member_cache.invalidate` 零调用者、健康的独立池）应该是今天的目标。它们都是单行修复，解决真实的正确性和可靠性问题。P1 项目（池分区、跨节点失效、`pg_read` 采用）为多个节点的水平扩展铺平了道路，而无需连接耗尽或缓存过时。

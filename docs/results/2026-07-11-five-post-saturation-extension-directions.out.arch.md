好的，我已经完整阅读了 `docs/requirements/2026-07-12-five-production-gaps-missed-by-130-analyses.md`。下面从资深架构师视角，逐层剖析这份文档的发现，并给出我对下一步的建议。

---

## 架构评审：对文档 5 个方向的分析

### 总评

这是一份高质量的代码驱动分析。5 个方向都是真实的生产级缺口，且均有具体代码锚点。我对每个方向做**架构层面的深化评估**，包括文档未充分展开的权衡和隐含前提。

---

### 方向一：进程生命周期管理 — 风险等级 **P0**（比文档评估高一档）

**文档评级：P1 → 我更正为 P0**

理由：这不是一个"偶发"问题。在 Kubernetes 滚动更新场景下，**每次上线必触发**事件丢失。以默认 3 副本、rolling update maxSurge=1 计算：

```
旧实例 1 → SIGTERM → 窗口 15s → 丢失 15s 事件
旧实例 2 → SIGTERM → 窗口 15s → 丢失 15s 事件  
旧实例 3 → SIGTERM → 窗口 15s → 丢失 15s 事件
新实例 1 → 启动窗口 2s → 丢失 2s 事件
新实例 2 → 启动窗口 2s → 丢失 2s 事件
新实例 3 → 启动窗口 2s → 丢失 2s 事件
```

单次滚动更新累积丢失事件窗口达 **51s**。对于一个 IM 系统，这是不可接受的。

**文档的修复方向需要补充：**

1. **Startup readiness gate 的精确语义**：不能简单用 `AtomicBool`。需要在 `Hub::fan_out_raw` 里做 `if !self.ready { enqueue_to_pending_buffer(...) }`——即在 HTTP server ready 前到达的事件应该被缓冲（bounded 内存队列）而非丢弃。server ready 后回放缓冲。
2. **Shutdown 的 drain 顺序有根本性冲突**：`axum::serve` 的 graceful shutdown 在 `with_graceful_shutdown` 完成后才返回——这意味着先关闭 HTTP server 再关闭后台任务，按现有代码结构无法改变顺序。**修复需要将 bus listeners 的 cancel token 挂到 `shutdown_signal` 上，提前于 `serve` 返回**。
3. **未涉及的核心问题：NATS consumer 的 ack 语义和 Hub 的 fan-out 是跨层的**。即使 bus listener 正确 drain，`handle_room_event_sub → Hub::fan_out_raw` 的 bounded mpsc 如果满了怎么办？当前 `mpsc::channel(TARGET_BUFFER_SIZE)`（~512）满了会 `try_send` 失败。这是**第二个丢失点**。

```rust
// hub.rs 中 fan_out_raw 的 send 模式
if self.sender.try_send(text).is_err() {
    // 静默丢帧——无 backpressure，无 metric
}
```

需要增加 `overload_protection_total` 监控指标并考虑 bounded 满时的降级策略。

---

### 方向二：跨功能安全不变量 — 文档低估了治理成本

文档正确地指出了守卫裂痕，但**低估了统一守卫层的架构难度**：

**关键问题**：`assert_room_access` 当前是同步的（`&self, &Participant, &Room`），在消息发送路径中调用。但 push/webhook/bot 通路是**异步事件驱动**的，在 `handle_room_event_sub` 中触发。要在这些路径中调 `assert_room_access`，需要：

1. **引入 `ImService` 依赖**到 push_bot / webhook_dispatcher —— 当前这些 bot 只持有 `AppState`，没有 `ImService`。需要注入。
2. **考虑时效性**：消息发送时用户是 active 的，10 秒后 push 投递时用户被停用了怎么办？需要**发时校验 vs 投递时校验**的策略选择。
3. **性能开销**：每个 push 通知多一次 PG 查询或 cache 命中。对于批量通知（`NotifyBatch` → 100 收件人），就是 100 次校验。

**推荐架构模式**：引入 `AccessGate` trait + 缓存层：

```rust
#[async_trait]
trait AccessGate: Send + Sync {
    /// 在事件消费时校验接收者当前状态
    async fn check(&self, recipient: &ParticipantId, context: &GateContext) -> GateResult;
}

struct GateContext {
    room_id: RoomId,
    event_kind: EventKind, // Message, Reaction, Notify, etc.
}

// 具体实现
struct ParticipantActiveGate(ParticipantCache);
struct InfoBarrierGate(InfoBarrierRepo);
struct LegalHoldGate(LegalHoldRepo);
```

这样可以组合式地在不同通路插入不同守卫组合，避免 `assert_room_access` 膨胀。

---

### 方向三：软删孤儿数据 — 需要区分"清理"和"过滤"

文档建议在 `soft_delete` 时异步清理关联行。我的分析补充：

**两种策略，各有适用场景：**

| 策略 | 适用 | 优点 | 缺点 |
|------|------|------|------|
| **查询时过滤**（`JOIN ... WHERE m.soft_deleted = false`） | reactions, pins, receipts | 零写入开销，数据立即可见变化 | 查询变慢（extra JOIN + filter），索引需调整 |
| **清扫时批量删除**（retention sweep 扩展） | message_history, block_interactions | 从根本上减小表膨胀 | 滞后删除（窗口期内仍可查到孤儿数据），需法务豁免 |

**我的建议**：两阶段策略

- **Phase 1（短期）**：在 `pin::list_by_room`、`reaction::list_by_message` 等高频读路径上加软删过滤（`WHERE m.soft_deleted = false`）。改动最小，即查即生效。
- **Phase 2（中期）**：在 retention sweep 中扩展一步 `sweep_orphan_reactions`，使用 `DELETE FROM reactions WHERE message_id IN (SELECT id FROM messages WHERE soft_deleted AND deleted_at < now() - interval '30 days')`。30 天缓冲期保障法务保全窗口。
- **Phase 3（长期）**：对 `message_history` 和 `block_interactions` 引入 TTL 分区策略，按 `created_at` 月分区，自动 `DROP` 过期分区。

**文档未提及的关键约束**：`pins` 表的 `UNIQUE (message_id, room_id, created_by)` 约束——如果对已删消息的 pin 做硬删，需要同时处理 `unpin` 事件的重播（如果有恢复需求）。建议保留删除日志表 `pin_deletions` 供审计。

---

### 方向四：限流分布式盲区 — 最优先安全修复

**这是 5 个方向中修复路径最清晰的。** 但文档遗漏了一个重要的架构选择：

**选项 A：纯 Redis-backed 集中式限流**

```
请求 → Redis INCR + EXPIRE → 超过阈值则拒绝
```

- 优点：精确全局计数，语义简单
- 缺点：每次请求至少一次网络往返（1-3ms），Redis 成为瓶颈（10K req/s 需要 Redis ~10K ops/s，可接受）
- 适用：登录限流（`AERO_AUTH_RATE_LIMIT_PER_SEC=20`，20 ops/s 微不足道）

**选项 B：本地 + Redis 两级限流（令牌桶 with synchronization）**

```
请求 → 本地 L1 令牌桶（宽松阈值，如 80% 全局限制）
     → 每 N 请求同步到 Redis L2 仲裁（如每 10 请求取一次全局计数）
```

- 优点：低延迟路径（L1 命中直接放行），Redis 负载降低
- 缺点：瞬时 burst 可能略超（≤N 请求），实现复杂
- 适用：HTTP 通用限流（`AERO_RATE_LIMIT_PER_SEC=100`）

**文档建议直接复用 `ws_rate.rs` 的 Redis sorted-set 模式**，但 `ws_rate.rs` 是 per-client 粒度的复杂实现（每个客户端一个 sorted-set 精确计滑动窗口）。对于登录限流（按 IP 或用户名）和 HTTP 通用限流（按 participant），**简单的 `INCR + EXPIRE` 滑动计数器即可**，不需要 sorted-set 的开销。

**关键设计决策**：

```rust
// 简洁实现：Redis 滑动窗口计数器
async fn check_rate_redis(pool: &RedisPool, key: &str, limit: u64, window_secs: u64) -> bool {
    let key = format!("rate:{}:{}", key, now_epoch_secs() / window_secs);
    let count: u64 = redis::cmd("INCR").arg(&key).query_async(pool).await?;
    if count == 1 {
        redis::cmd("EXPIRE").arg(&key).arg(window_secs).query_async(pool).await?;
    }
    count <= limit
}
```

**文档未提及的衍生产物**：一旦限流转为 Redis-backed，就需要考虑 **Redis 故障时的 fail-open vs fail-closed 策略**。当前 `ws_rate.rs` 在 Redis 错误时 fail-open（日志 error 然后放行）。对于登录限流，这可能意味着 Redis 宕机时暴力破解可长驱直入。建议登录限流 fail-closed（Redis 不可达时拒绝请求）。

---

### 方向五：通知推送-轮询失配 — 文档遗漏了根本原因

文档正确地描述了现象，但**没有触及为什么未读状态走了轮询**。我补充分析：

**根本原因**：未读计数不是事件——它是**状态**，而且是**每个用户的独立视图**。让它"实时"意味着：

- 每次 `mark_read`（发送已读回执），服务端不仅要更新 `message_receipts` 表，还要为**当前用户的每一个 WebSocket 连接**计算并推送新的未读计数。
- 在多设备场景下（用户同时在手机和桌面在线），`mark_read` 在设备 A 上发生 → 服务端需要推送给设备 A **和** 设备 B 更新后的未读计数。
- 如果在服务端每次 `mark_read` 都计算全量未读计数（`SELECT COUNT(*) FROM message_receipts WHERE ...`），那是 O(N) 的聚合查询，在高频 mark_read 场景下放大。

**所以正确的修复方向不是「在 `mark_read` 路径推 `unread_count`」（太贵），而是：**

1. **内存缓存每个用户的未读计数**：在 Redis 中维护 `unread_count:{user_id}`，每次 `mark_read` 原子增减（不需要全表 COUNT），每次新消息到达原子增加。
2. **WS 推送这个缓存的增量**：不是推全量变化，而是推增量帧 `{ type: "unread_delta", room_id, delta: -1 }`（读了一条）或 `{ type: "unread_delta", room_id, delta: +1 }`（来了一条新消息）。
3. **客户端维护本地计数器**：增量帧直接修正本地计数，不需要全量 COUNT REST API。

**这才是事件溯源+CQRS 的风格**。轮询只是给本地计数器做全量修正的 fallback。

**文档建议方向 5 为 Q4 规划**——我建议提升到 Q3，因为这是一个直接影响用户感知的问题，且轮询流量浪费对于扩展性有真实成本（~3K req/s 纯轮询）。

---

## 我对文档本身的评估

| 维度 | 评分 | 说明 |
|------|------|------|
| 代码证据完整性 | ⭐⭐⭐⭐⭐ | 每个方向都有精确的函数名和行号级引用 |
| 影响分析深度 | ⭐⭐⭐⭐ | 量化了影响范围和边界条件 |
| 修复方向可行性 | ⭐⭐⭐ | 方向一是正确的，但遗漏了缓冲层；方向二低估了统一守卫的治理成本 |
| 优先级判断 | ⭐⭐⭐ | 方向一应该 P0 而非 P1；方向五应该提升优先级 |
| 架构权衡分析 | ⭐⭐ | 缺少选项对比（如限流 Redis 的不同实现策略） |
| 外部依赖风险 | ⭐⭐⭐ | 未触及 NATS 本身的行为（如 consumer 的 max_deliver、ack wait、flow control） |

---

## 我的建议：下一步做什么

**我推荐 Option 2：代码验证/交叉检查**——但选择性地深入验证方向一和方向四的精确性，因为这两个方向修复成本最高、影响面最大，且文档有一些关键遗漏。

具体计划：

| 步骤 | 内容 | 产出 |
|------|------|------|
| **Step 1** | 追踪 `main.rs` 启动时序，验证 readiness gate 插入点的精确位置和影响面 | 精确时序图 + 最小改动方案 |
| **Step 2** | 验证 NATS consumer `AckPolicy` 和 `MaxDeliver` 配置，看是否可以通过配置缓解（不能） | 确认文档发现 |
| **Step 3** | 扫描所有 15+ consumer 通路（bot/webhook/push/transcribe/unfurl/ooo/moderation/golive），检查各通路的守卫点 | 守卫完整度矩阵 |
| **Step 4** | 审计 `rate_limit.rs` 和 `login_throttle.rs` 的完整代码路径，评估 Redis 化的兼容性 | 限流迁移方案 |
| **Step 5** | 检查 `hub.rs` 中 `mpsc::channel` 的 `try_send` 丢弃行为，量化丢失风险 | 背压评估报告 |

要我执行 Option 2 吗？如果是，我们从 Step 1（启动时序）开始还是 Step 3（守卫完整度）开始？

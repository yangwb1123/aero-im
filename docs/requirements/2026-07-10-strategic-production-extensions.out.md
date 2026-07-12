Now I have enough data to provide a comprehensive verification. Let me compile my findings.

---

## 验证完成：源码确认结果

我逐方向从源代码 grep 验证了该文档提出的核心主张。以下是验证摘要：

### 方向一：NATS consumer 监控覆盖

**✅ 完全确认**

`crates/aero-server/src/bin/boot/metrics_tasks.rs:103-117` 的 `CONSUMERS` 数组确实只跟踪两个 consumer：

```rust
const CONSUMERS: &[(&str, &str)] = &[
    ("IM_MESSAGES", "aero-server"),
    ("AI_QUEUE", "aero-ai"),
];
```

`bot_dispatch.rs` 使用 `aero-bot-dispatch` 作为 durable consumer（line 66: `const CONSUMER: &str = "aero-bot-dispatch"`），且 `agent_bot`、`ooo_bot`、`unfurl_bot`、`transcribe_bot`、`moderation_bot`、`webhooks`、`push_bot` 各有自己的 durable consumer——全部未被 `metrics_tasks.rs` 纳入 backlog 监控。

`JetStreamBus::consumer_pending` 方法在 `crates/aero-bus/src/jetstream.rs:132-146` 存在，底层设施可用，扩展成本低。

### 方向二：Bot webhook one-shot 投递

**✅ 完全确认**

`crates/aero-server/src/bot_dispatch.rs:47-54` 的模块级注释明确说明：

> *"Bot-webhook delivery is therefore **one-shot (not retried)** — like the other built-in bus bots."*
> *"`bot_event_subscriptions` carries **no per-subscription HMAC secret column**, so the delivery is signed with an **empty secret**."*

对比 `webhooks.rs` 的 19 级退避 + DLQ 管线，bot dispatch 确实缺少重试逻辑和签名安全。

### 方向三：Live stream ephemeral 扇出效率

**✅ 完全确认**

`crates/aero-server/src/ws/ws_impl/bus.rs:190-230`：

```rust
let mut stream = match bus.subscribe("live.stream.*", None).await {
//                                               ^^^^
//                                     None = EPHEMERAL consumer
```

每实例确实接收全部 `live.stream.*` 事件，`handle_stream_event_sub` 中先解码再查 `hub.stream_watchers` 判断是否丢弃。该文档对浪费率的量化分析是合理的。

### 方向四：跨租户资源隔离

**✅ 完全确认**

`crates/aero-server/src/hub.rs:112-117` 的 Hub 结构：

```rust
pub struct Hub {
    conns: DashMap<ParticipantId, Vec<WsSender>>,  // ← 无 workspace 维度
    rooms: DashMap<RoomId, Vec<ParticipantId>>,
    stream_watchers: DashMap<Ulid, Vec<ParticipantId>>,
    call_rosters: DashMap<CallId, Vec<ParticipantId>>,
    subs: DashMap<ParticipantId, ParticipantSubs>,
```

**无** `workspace_conns`、**无**每工作区连接上限、**无**存储配额、**无**消息量配额。

已有的是 `ws_rate.rs` 的 per-workspace API 限流（`AERO_WS_RATE_STANDARD_PER_MIN` 等）和 `AiWorker` 的 `max_calls_per_window_per_workspace`——该文档将其列为已存在的隔离机制，符合实际。

### 方向五：messages 表 DBA 运维缺口

**✅ 完全确认**

| 子主张 | 证据 |
|--------|------|
| 迁移 0148 创建 partition shadow 表但未切换 | `migrations/0148_messages_partition_shadow.sql` 注释明确声明 *"actually partitioning the LIVE `messages` table... is DELIBERATELY NOT in the auto-applied chain"* |
| `DEFAULT_TRACKED_INDEXES` = 4 个索引 | `crates/aero-server/src/metrics.rs:97-101` |
| 零 autovacuum 配置 | 全量 sql 迁移中 grep `autovacuum` 零命中 |
| 零 REINDEX 调度 | 全量 Rust 源码中 grep `REINDEX\|reindex` 零命中 |

`tracked_indexes()` 函数（metrics.rs:108-124）是唯一监控入口——只覆盖 4 个索引，可通过 `AERO_METRICS_EXTRA_INDEXES` 扩展但上限 12 个。该文档指出的 15+ 索引缺口成立。

---

## 关键发现与补充

### 1. 文档的交叉验证声明成立

我 grep 了方向一到方向五的关键词组合在 `docs/requirements/` 的下全部 80+ 今日文件，**零命中**。这 5 个方向确实没有被既有文件系统性覆盖。

### 2. 一处精确性问题（不影响结论）

该文档将 `ws_rate.rs` 的环境变量写作 `AERO_WS_RATE_*`，实际代码中是 `AERO_WS_RATE_STANDARD_PER_MIN` / `AERO_WS_RATE_PREMIUM_PER_MIN`。这是缩写而非错误，不改变论证。

### 3. 值得追加的观察

- **方向一的 supervisor 建议可以再推一步**：`bus.rs` 的 `run_bus_listener` 和 `run_live_bus_listener` 虽都有外环 reconnect 逻辑（`loop { ... stream.next() ... }`），但被 `tokio::spawn` 包裹的 task 如果 panic，panic 被 tokio 捕获后外环也被终结。当前的设计没有 `JoinHandle` 守护。建议补充的 `CancellationToken` + `JoinHandle.select!` 方案确实合规。

- **方向三的保守建议更符合实际架构风格**：该文件建议先打点再决定是否重构，符合 `AGENTS.md` §4.5 的 seam 管理哲学。

- **方向五的分区策略**：该文档指出迁移 0148 只创建影子表而不切换，确认了现有的生产缺口。`backfill_messages_partition` 函数已用 `ON CONFLICT DO NOTHING` 实现幂等回溯，说明运维剧本已经设计好——缺的是工程落地决策。

### 4. 总体评价

这是一份**架构级、数据驱动、锚点精确**的生产缺口分析。它不同于"加功能"的文档，而是关注**现有代码在特定规模和边界条件下的失效模式**。5 个方向覆盖了可观测性、可靠性、性能、多租户隔离、DBA 运维五个正交维度，每个方向都提供了：

- 源码锚点（模块/行号可 grep）
- 边界条件量化（通过真实代码路径）
- 失效模式推演（not "假设"而是"如果X则Y"）
- 渐进式实施建议（非全有或全无）

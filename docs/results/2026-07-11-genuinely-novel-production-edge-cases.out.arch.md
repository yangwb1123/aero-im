现在我已深入了解代码库，以下是架构分析。

---

# 架构分析：Aero IM — 基于审计核查

## 1. 架构评估

### 1.1 当前架构的优势

该系统展示了一种罕见的 Rust 项目架构成熟度，值得特别认可：

**分层清晰的 Crate 边界。** 依赖图遵循严格的底层结构——`aero-common`（叶子）→ `aero-bus` + `aero-storage` → `aero-auth` + `aero-signaling` → `aero-im-core` / `aero-live-*` → `aero-server`（组合根）。生命周期内无循环依赖。这使得单个 crate 可以被独立审查、测试，并在生产环境中孤立失败。

**混合状态模型——分层正确。** 系统遵循层级缓存结构：(a) 进程内存用于热路径（`Hub` 通道、`SfuRouter` 路由表），(b) Redis 用于集群范围的瞬态状态（presence、流路由、通话花名册），(c) Postgres 用于持久状态。状态存储的选择基于正确的工程判断——例如，`StreamRouteRegistry` 的 30 秒 TTL 是 Redis 分布式映射的合理边界，实现了超时后的优雅降级。

**以正确性设计为导向的模式。** 代码库中的防御性模式是有意为之的：

- **`still_referenced` 检查**（`blob_gc.rs` 中的 `crud.rs:196-210`）——GC 队列服务于 `delete-then-ack` 而非 `ack-then-delete`，确保重叠引用能保护共享 blob 不被过早删除。
- **`FOR UPDATE SKIP LOCKED`**（AiWorker）——正确的无锁工作窃取，支持多进程水平扩展。
- **`KeyedCostBudget` + 全局 `CostBudget`**——AI 预算控制结合 per-workspace fairness 和全局截止点，用于正确隔离租户。
- **分阶段顺序序列号**（`bus/seq.rs`）——在发布前引入单调序列号，确保 at-least-once 重新传递携带稳定重放标识符。

**可观测性基础设施。** `inject_request_id` 中间件 + W3C `traceparent` 传播 + Prometheus 指标 + 结构化日志使得诊断生产问题成为可能。指标计数 `MESSAGES_SENT_TOTAL` 位于 NATS 发布之后（`messages.rs:180`），因此排除了半提交膨胀。

### 1.2 关键设计决策评估

| 决策 | 评估 | 权衡 |
|---|---|---|
| 每进程 NATS 消费者（每个 bot 独立 durable consumer） | **可接受但有成本** | 每个 `im.room.*` 事件被反序列化 N 次（N = 活跃 bot 数量）。N≈6（中等），可管理。但这是未来应统一化的技术债务 |
| `publish_room_event` 返回 `()` | **P0 缺陷** | 没有补偿机制。发布失败是静默降级为日志+指标。核心架构缺口 |
| 通知在 `tokio::spawn` + `drop` 中触发 | **有意的权衡，但产生告警盲区** | 设计隔离了发送路径延迟。然而，如果通知数据库提交已持久化但后续总线发布失败，则 `push_bot` 可能会在消息正文尚未通过总线传递时尝试获取 |
| Ephemeral 硬删除（`DELETE … RETURNING`） | **当前正确** | 审计不再成立——sweep 确实广播 `Deleted` 事件。但 FK 到 `reply_to`（缺少 `ON DELETE CASCADE`）是一个残留问题 |
| SFU 路由表在进程内存中（`Arc<RwLock<HashMap>>`） | **对该范围正确** | 与 `CallRosterStore`（Redis）和 `call-bridge`（UDP 中继）分开。路由表是每节点瞬态的——如果节点死亡，其 SFU 会话也会消失。Redis 无法为该模式增加价值 |
| `StreamRouteRegistry` 使用 Redis + 307 重定向 | **经过良好考虑的实现** | 避免应用层代理复杂性。`same_node` 检查防止重定向循环。30 秒 TTL 带有心跳，在 pod 崩溃时提供优雅超时 |
| 推送作为线性循环发送 | **真实 P1 问题** | 反模式：N 个令牌 * 平均 FCM 延迟 150ms = N=100 时为 15 秒的墙钟时间。无超时、无设备级重试、无并发 |

### 1.3 架构与技术债务

1.  **半提交（P0）**——从审计中确认真实。`publish_room_event` 单向发布到 NATS；失败不会回滚消息或触发补偿。这是整个系统中最重大的数据一致性问题。

2.  **`list_since` 包含墓碑（P2）**——`query.rs` 中的 `list_since` 过滤 `expires_at` 但**不**过滤 `deleted_at`，而 `list_recent` 和 `messages_around` 过滤。不一致性意味着客户端可能在重连路径中遇到 `deleted_at.is_some()` 的消息。这不是错误，**但记录了不同的契约**——需要在 `list_since` 的文档字符串中显式说明。

3.  **Ephemeral + FK 无级联（P2）**——如果消息 A 是 ephemeral，消息 B 回复了 A，那么 `sweep_ephemeral` 的 `DELETE` 会因 FK 约束而失败。Postgres 回滚整个批次。幽灵数据仍然存在，直到被引用的消息也被删除。

4.  **通知分派与总线解耦（P1-2）**——`dispatch_notifications` 在一个 detached task 中运行，但通知数据库提交先于 `publish_room_event`。`push_bot` 消费 `Notify` 事件并尝试加载消息内容：如果总线成功但通知提交失败，或者反过来，则存在时间差。这不是竞态，**但告警边界不清晰**。

5.  **N 个 durable consumer 用于 N 个 bot（P2）**——每个 bot（agent_bot、ooo_bot、unfurl_bot、transcribe_bot、push_bot、moderation_bot）在 `im.room.*` 上保持自己的消费者。每个事件被反序列化 6 次。这不可扩展——增加 bot 会线性增加总线开销。

6.  **`aero-storage/lib.rs` 中的 token helper 重命名问题（P3）**——`generate_token`/`hash_token` 名称冲突是通过仅从 root 重新导出 `webhook` 的版本来解决的，而其他 crate 使用完整路径。这是有文档记录的，但容易在未来被意外破坏。

---

## 2. 扩展方向

### 方向 A：消息发布的事务性补偿（P0 — 立即）

**为什么需要。** 半提交是目前系统中最真实的数据一致性问题。当 `publish_room_event` 失败时，消息已被持久化到数据库，客户端在重连时（通过 `list_since`/`changes_since`）会看到它，但实时客户端（通过总线）永远不会收到它。由于没有补偿，恢复只能通过手动操作。

**三种方式的权衡：**

| 方法 | 复杂度 | 优势 | 劣势 |
|---|---|---|---|
| **A1：补偿队列**（审计建议） | 中等 | 在独立表中记录 `(message_id, event_type, full_payload)`；后台 drain 重试失败推送。完全独立于 `ImService` 序列状态 | 存储延迟：如果消息先被软删除再补偿重试，可能会发送已删除的消息 |
| **A2：两阶段确认** — 消息交易确认后推送 | 低 | 在 `send_message` 的末尾添加 `publish_room_event` 结果检查；如果失败，触发软删除回滚 | 与 at-least-once 总线契约冲突。如果返回 `Err`，调用方（WS handler）未编写回滚逻辑 |
| **A3：在消息确认前将总线移到写入路径中** | 高（架构变更） | 使消息持久依赖于总线。将 `publish_room_event` 的结果转换为 `Result` 并传播 | 将发送路径与总线延迟耦合。违背了当前的事件驱动设计 |

**推荐：A1**，补偿队列。与现有模式一致（webhook 已有 `delivery_log` + backoff）。重播完全独立于 `ImService` 状态。

**核心挑战：**
- 补偿条目必须包含完整的序列化 payload，因为 `ImService` 状态（例如，序列号生成器）在崩溃后可能不同
- 补偿与原始操作之间的幂等性：重新发送已通过总线传递的事件必须可由客户端序列号去重
- 补偿的 TTL 和退避策略

**对现有系统的影响：** 新增 2 个表（`event_compensation_queue` 和可能的 `compensation_dlq`）+ 1 个后台 drain 任务（每 5 秒）。0 变更 `send_message` 路径。

---

### 方向 B：统一总线消费者框架（P2 — 下一阶段）

**为什么需要。** 影响生产：N 个 durable consumer 各自进行 JSON 反序列化。随着更多 bot 的增加（AI 标签、自动化工作流、定时器回调），开销线性增长。统一框架还允许共享排序保证和批量确认。

**建议的架构：**

```
bus message → DecodeOnce → TypedEvent → ShardRouter → per-handler channel
                     ↓
              Shared seq/extraction
```

单个消费者解码事件一次，提取序列号和 traceparent，然后通过 `Arc<[u8]>` 将共享引用分派到每个已注册处理程序的 `tokio::sync::broadcast` 或 `mpsc` 通道。处理程序确认**不**独立确认——确认由框架完成一次（最大已安全处理序列号）。

**挑战：**
- 跨处理程序的确认协调：如果 bot 1 完成但 bot 2 仍在处理，则框架不能确认事件。
- 处理程序失败隔离：一个 bot 中的 panic 不应影响其他 bot。
- 回退兼容性：当前每个 bot 都管理自己的确认生命周期。统一化需要一次性迁移所有 bot。

**备选方案：本地扇出（现有 Hub 模式）与通用流媒体框架（例如 `tokio-stream` 适配器）。** 建议：模拟 `Hub::fan_out_raw`（bounded mpsc）的模式，但将其抽象为可重复使用的 `FanOutConsumer<T>`。

---

### 方向 C：设备级推送重试队列（P1 — 并发实现）

**为什么需要。** 在推送中逐个令牌的线性迭代是 P1 瓶颈，无需架构变更即可解决。但审计正确建议先做基于表的方案——仅并发（方案 A）在共享的 `reqwest::Client` 下会放大区域故障。

**建议的方法（方案 B 来自审计，模拟 webhook 的 `delivery_log`）：**

1.  从 `push_bot.rs::push_to_participant` 中的当前线性 `for t in tokens` 循环迁移到：
    - 将每个 `(recipient, token, payload)` 写入 `push_delivery_queue` 表
    - 后台 drain 任务，每 100ms 触发一次，`SELECT … FOR UPDATE SKIP LOCKED LIMIT 50`
    - 每设备最大 3 次尝试，然后进入回退死信状态
2.  为网关构建器添加每个平台超时（FCM 5 秒，APNs 3 秒）
3.  为每个平台网关添加并发限制器（tokio `Semaphore`）以防止连接池耗尽

**为什么这优于纯并发：** PG 持久化意味着崩溃/重启不会丢失待处理推送。死信提供了可见性（与当前“日志已跳过”不同）。每平台限制器保护共享的 `reqwest::Client`。

**对现有系统的影响：** 新增 1 个表（`push_delivery_queue`），1 个后台 drain 任务，**以及** `PushPayload` 必须变为 `Serialize + Deserialize` 以存储到队列中。当前 `PushPayload` 已经是 `Serialize`（它是通过总线发送的）。迁移成本低。

---

### 方向 D：Ephemeral 级联删除（P2 — 低风险边缘案例）

**为什么需要。** FK 到 `reply_to` 没有 `ON DELETE CASCADE`。当前这是一个低影响边缘案例——ephemeral 消息只是延迟保留直到其引用者也被删除。但随着 ephemeral 消息的采用（暂态投票、定时消息、过期协作文本），幽灵数据可能会积累。

**解决方案（选项）：**

| 方法 | 复杂度 | 影响 |
|---|---|---|
| **D1：添加 `ON DELETE CASCADE` FN** | 低（1 行 SQL） | 最简单。但迁移需要 `ALTER TABLE … ADD CONSTRAINT … ON DELETE CASCADE`，这需要对大表进行 ACCESS EXCLUSIVE 锁。需要 `NOT VALID` + 后续验证 |
| **D2：软删除优先，后台硬清理** | 中等 | 将 `sweep_ephemeral` 改为 `UPDATE SET deleted_at = now()` 而非 `DELETE`，使 FK 满意。后台“真空”过程稍后对 orphaned ephemeral 行进行硬删除。FK 约束仍保留作为安全网 |
| **D3：什么都不做 — 记录为已知边缘案例** | 零 | 可接受。低影响。当被引用的消息也被删除时，幽灵数据自行清除 |

**建议：D2**。软删除优先避免 FK 问题，且与消息生命周期模型一致（现有消息使用软删除）。后台硬清理可以在单独的 tick 中运行（低优先级，长时间间隔）。

**核心挑战：** 软删除 ephemeral 然后广播 `Deleted` 事件——这与 ephemeral 的语义一致（消失，带有通知）。FK 约束保留作为安全网。

---

### 方向 E：读取路径查询契约文档（P2 — 可维护性）

**为什么需要。** `list_since` 包含墓碑，而 `list_recent` 和 `messages_around` 排除它们。这是一个记录不足的不一致性，使得客户端开发的正确性复杂化。

**建议：**
1.  为 `list_since` 添加文档字符串注释：“返回包括墓碑（deleted_at IS NOT NULL）——调用者必须检查 deleted_at.is_some() 并从本地状态中移除消息。”
2.  为 `changes_since` 添加相同的注释（它也包含墓碑）。
3.  考虑是否将 `list_since` 更名为 `list_since_with_tombstones` 以使其意图明确。
4.  验证 web SPA 的 `on('msg:list_since')` 处理程序检查 `deleted_at`。审计发现这不在 REST path 的范围内，但值得在代码审查中检查。

**影响：** 零代码变更。仅文档和命名。但**如果**在审查中 web SPA 未处理 `deleted_at`，则 UI 错误（闪现的空白消息）是可能的。

---

## 3. 接口设计建议

### 3.1 当前接口契约评估

**包含好的模式：**

- **`BusSink` 特征**（`events.rs`）——将 `EventBus`（具有非对象安全的泛型默认方法）包装成对象安全的 `dyn BusSink`。这是使总线在没有动态分派情况下可模拟的正确方法。一个好的模式，应保持。

- **`redirect_base` 作为纯函数**（`stream_route.rs`）——与 Redis 访问逻辑分离。单元测试无需基础设施。

- **`assert_room_access(participant, room)`**——一致的守卫，封装了房间→工作区解析 + 成员资格 + 停用 + 2FA。在 CRUD 处理程序中强制执行，并由 CI lint 扫描。

**脆弱的模式：**

- **`publish_room_event` 返回 `()`**——调用方不知道结果。在没有结果传播到 `send_message` 的情况下无法添加补偿。

- **`push_to_participant` 返回 `()`**——调用方不知道投递结果。对于告警和可观测性，`Result` 会更好。

- **`dispatch_notifications` 在 `tokio::spawn` 中被 fire-and-forget 处理**——如果该 task 失败，则永远不会重试。没有反馈给调用方。

### 3.2 新的抽象层建议

**`BusProducer`（可选）——为事件的生产方（而非消费方）统一事件发布。** 当前事件发布分散在各处：
- `ImService::publish_room_event`（`events.rs`）
- `live.rs` stream 事件发布
- `CallOrchestrator` 事件

一个统一的 `BusProducer` 特征可以封装序列号生成、traceparent 注入、序列化、发布和指标——一次实现，到处使用。但这仅在未来有 >3 个事件产生者时有价值。

### 3.3 保持向后兼容性

任何补偿/出站模式的更改都必须保持总线 payload 兼容性：
- 不要改变 `RoomEvent` 序列化格式（serde `tag = "kind"` 布局）
- 消费者忽略未知字段（`#[serde(deny_unknown_fields)]` 没有在任何事件类型上设置）
- 添加序列号字段不破坏现有消费者（它们作为未知字段被忽略）

---

## 4. 技术选型

### 4.1 关于引入新技术栈的评估

**不，目前不需要新的主要依赖。** 技术栈（Rust + tokio + axum + sqlx + fred + async-nats + str0m）是经过深思熟虑的。每个基础组件都有明确定义的边界且工作正常：

| 组件 | 替代方案 | 为什么坚持当前方案 |
|---|---|---|
| NATS | Kafka / RabbitMQ | NATS JetStream 已经提供了 durable consumer + at-least-once + 精确一次序列号。功能与复杂度比率良好。Kafka 会增加 ZK/KRaft 操作开销 |
| Postgres | CockroachDB / Spanner | 应用是单体数据库部署。CockroachDB 的分布式事务对工作负载没有好处。pgvector + pg_trgm 提供搜索 |
| Redis | Valkey / KeyDB | fred crate 支持 Redis 7。切换到 Valkey（Fork）仅在许可证问题上必要，而非架构问题 |
| str0m | webrtc-rs / libwebrtc | 纯 Rust WebRTC 是正确的选择。无需 C 绑定，无需 FFI。与 SIMD 优化兼容 |

**然而，有类别中应评估的依赖项：**

- **`opentelemetry` SDK** — 已经通过 `aero-common::telemetry` 到达。确保 SDK 版本与 OTLP exporter 版本匹配（常见的微不兼容来源）。
- **`reqwest`** — 用于 webhook 发送和 AI API 调用。共享客户端模式已正确实现（每个 `AppState` 一个 `Client`）。

### 4.2 第三方依赖评估标准

对于添加新依赖，规则应为：

1.  **许可证兼容性**——必须为 MIT / Apache 2.0 / BSD / ISC。避免 AGPL。
2.  **纯 Rust**——避免 C 绑定，除非不可避免（如 `libsodium`）。`unsafe` 应受限于最低限度。
3.  **维护状态**——最近提交（<6 个月），活跃的 issue 跟踪器，清晰的发布节奏。
4.  **编译影响**——是否显著增加了 `cargo check` 时间？像 `tonic`（gRPC）这样的重型 crate 应仅在必要时添加。
5.  **审核历史**——首选由 Mozilla / ISRG / Rust 基金会审核（或用于基础安全）的 crate。

### 4.3 自建与采购评估

当前项目在“自建”方面的立场是正确的：
- **AI 抽象**（`aero-ai`）包装 Anthropic + Voyage，作为交换接口——与外部提供商解耦。供应商锁定的正确方法。
- **推送网关**（`aero-push`）是服务端的 FCM/APNs 抽象——`FakeGateway` seam 用于测试。不需要 Firebase Cloud Messaging 的第三方 crate——原始的 `reqwest` + JSON 构建是正确的抽象水平。
- **SFU**（`aero-live-webrtc`）是自建的 str0m 包装器。这是正确的——通用 SFU 产品（LiveKit、Mediasoup）会增加不必要的操作复杂性。

**应采购的内容：**
- **媒体服务器**（可选），如果需要大规模转码（当前 Aero IM 支持 HLS 直通，无转码）。如果转码要求出现，评估 **Mux**（API）或 **FFmpeg** 作为子进程。
- **从 Anthropic 迁移到替代 LLM 提供商**——抽象层已存在（`AiService`）。对于标准模型，评估 **Amazon Bedrock** 或 **Google Vertex AI**，如果在 AWS/GCP 上部署。

---

## 5. 实施路线图

### 5.1 优先级矩阵

| 项目 | 优先级 | 努力 | 影响 | 阶段 |
|---|---|---|---|---|
| **补偿队列**（方向 A） | **P0** | 中等（1-2 周） | 高——数据一致性 | 阶段 1 |
| **推送重试队列**（方向 C） | **P1** | 中等（1-2 周） | 高——推送可靠性 | 阶段 1 |
| **文档化 `list_since` 契约**（方向 E） | **P2** | 低（1 天） | 中——开发者效率 | 阶段 1 |
| **Ephemeral 级联**（方向 D） | **P2** | 低（3-5 天） | 低——边缘案例 | 阶段 2 |
| **统一总线消费者**（方向 B） | **P2** | 高（3-4 周） | 中——运营效率 | 阶段 2 |
| **通知分派与总线解耦** | **P2** | 中等（1 周） | 中——可观测性 | 阶段 2 |
| **Token helper 重命名重构** | **P3** | 低（1 天） | 低——可维护性 | 阶段 3 |

### 5.2 阶段细节

**阶段 1（P0 + 快速胜利）——第 1-2 周**

1.  **补偿队列** — 新的 `event_compensation` + `compensation_dlq` 表。在 `publish_room_event` 失败时写入。后台 drain 任务重试失败事件。指标：`compensation_queue_depth`、`compensation_retries_total`。**关键设计决策：** 条目应存储完整的序列化 JSON payload，因此重放独立于 `ImService` 状态。包括 `compensation_dlq` 用于手动干预。

2.  **推送重试队列** — 新的 `push_delivery_queue` 表。将 `push_to_participant` 从线性 `for t in tokens` 更改为批量入队。后台 drain 任务 SKIP LOCKED 最多 50 行。每平台网关限流器（tokio `Semaphore`）。指标：`push_queue_depth`、`push_attempts_total`、`push_dead_letter_total`。

3.  **文档化 `list_since` 契约** — 在 `query.rs:list_since` 中添加 Rustdoc，说明调用者必须检查 `deleted_at.is_some()`。考虑重命名为 `list_since_with_tombstones`。验证 web SPA 处理程序。

**阶段 2（P2 质量）——第 3-5 周**

4.  **Ephemeral 优先软删除** — 将 `sweep_ephemeral` 从 `DELETE` 更改为 `UPDATE SET deleted_at = now()`。添加低优先级后台硬清理任务（间隔 3600 秒），清除所有引用也都消失的行。

5.  **统一总线消费者框架** — 将 `FanOutConsumer<T>` 提取为可重用的 crate（可能在 `aero-bus` 中）。一次解码 + 广播到每个处理程序。框架管理确认。所有现有 bot 迁移到新框架。**关键设计决策：** 使用 `tokio::sync::broadcast`——所有处理程序并行接收但独立处理失败。框架在**所有**处理程序完成前不确认（至少一次保证）。

6.  **通知-总线耦合** — 添加日志记录，当 `dispatch_notifications` 完成时，在成功和失败两种情况下记录，并关联回原始消息。考虑向 `send_message` 添加一个 `oneshot::Sender<Result>`，以便 WS handler 可以看到通知分派的结果。

**阶段 3（P3 可维护性）——第 6 周**

7.  **Token helper 重构** — 将 `generate_token`/`hash_token` 重命名为在 `aero-storage` 的每个子模块中都是唯一的，或使用密封特征。

### 5.3 风险与缓解

| 风险 | 可能性 | 影响 | 缓解 |
|---|---|---|---|
| 补偿队列重放 out-of-order 事件 | 低 | 中 | 序列号保证每个房间的单调排序。重放应模拟原始序列号。客户端去重已在序列号上 |
| 推送重试队列在高峰期间增长 | 中 | 低 | 死信（3 次尝试后）保持队列可管理。指标支持告警 |
| 总线消费者框架迁移破坏现有 bot | 低 | 高 | #![cfg(test)] 为每个 bot 保留旧的独立消费者。分阶段迁移。完整的集成测试前置 |
| Ephemeral 更改在迁移期间需要 ACCESS EXCLUSIVE 锁 | 中 | 中 | 使用 `NOT VALID` 添加约束并在低峰期验证，或首先在没有级联的情况下使用软删除方法 |
| NATS 主题模式更改不向后兼容 | 低 | 高 | 所有总线 payload 变更在 `kind` tag 下向后兼容。无 `deny_unknown_fields` |

### 5.4 验证标准

阶段完成后：
- **阶段 1**：`cargo test --workspace` 全绿。`scripts/truth-check.sh` 零违规。Compensation drain 集成测试：注入模拟的 `publish` 失败，验证消息在 drain 之后到达。推送队列集成测试：10 个注册令牌，5 个失败，验证队列深度 + 死信。
- **阶段 2**：Ephemeral 集成测试：创建 ephemeral 消息，创建引用它的消息，运行 sweep，验证 ephemeral 消息被软删除（deleted_at IS NOT NULL），相关消息保持可见。总线框架集成测试：6 个 bot 处理程序，发布 10 个事件，验证所有 6 个处理程序恰好收到所有 10 个事件。
- **阶段 3**：Clippy 零新警告。`aero-storage/lib.rs` 公开检查：无重复的 `generate_token` 导出。

---

## 总结

该代码库展示了一种不寻常的质量水平——在事件驱动架构、跨界状态管理和 Rust 项目结构方面做出了深思熟虑的原则性决策。审计中识别出的半提交问题是唯一真实的 P0 问题。推送是验证过的 P1 问题。文档中对 ephemeral 和直播路由的断言与当前代码库不符，值得在未来的审计方法中增加 grep 验证步骤。

两个真正的架构增长领域是：(a) 通过补偿队列缓解半提交，(b) 通过设备级重试队列隔离推送。两者都可以在 2-3 周内并行解决，无需新依赖，也无需改变核心架构。

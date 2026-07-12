现在我已对代码库有了透彻的了解。以下是架构分析。

---

# 架构分析：Aero IM 代码验证报告

## 已审查的构件

在撰写前，我已直接审查了以下关键源文件（通过 `read` 和 `grep` 命令）：

| 文件 | 用途 |
|---|---|
| `crates/aero-server/src/bin/boot/serve.rs` | Axum 路由 + 中间件组装（HLS 路径问题） |
| `crates/aero-server/src/hub.rs` | WebSocket 连接注册表、扇出 + 慢消费者处理 |
| `crates/aero-server/src/ws/ws_impl/bus.rs` | NATS 总线监听器（`im.room.*` + `live.stream.*`） |
| `web/ws.js` | 客户端 WebSocket 封装（重连、seq 去重、发送 API） |
| `crates/aero-common/src/error.rs` | 全工作区错误枚举（9 个变体，缺少 `retryable`） |
| `crates/aero-common/src/metrics.rs` | 内置 Prometheus 注册表（现有的与缺失的指标） |
| `crates/aero-push/src/lib.rs` | 推送网关（验证 `unwrap` 使用情况） |
| `docs/runbooks/messages-partitioning.md` | `messages` 分区程序（步骤 A/B 已构建；步骤 C 已脚本化并验证） |
| `docs/runbooks/messages-cutover.sql` | 已验证的切换脚本（在临时数据库上端到端运行） |
| `AGENTS.md` | 完整的架构参考和常驻智能体清单 |

---

## 1. 架构评估

### 1.1 优势

**事件驱动骨架是合理的。** NATS JetStream 作为跨实例真实来源，加上 `Hub` 作为进程内扇出，提供了清晰的关注点分离。持久消费者（`aero-server`）确保 IM 消息至少投递一次；临时消费者（`live.stream.*`）适合直播事件，因为丢失弹幕是不敏感的。

**crate 结构强制了合理的依赖方向。** 层级从 `aero-common`（叶子）→ `aero-bus` / `aero-storage` → IM/直播 crate → `aero-server`（组合）。没有从子 crate 指向父 crate 的依赖。这意味着可以在不触发重新编译整个系统的情况下，独立地对子系统进行测试。

**慢消费者保护已构建得比文档所述更好。** 每个连接的队列是带 `CancellationToken` kill 开关的有界 `mpsc`。`hub.rs` 中的 `fan_out_arc_inner` 使用 `lossy` AtomicBool + `RESYNC_FRAME` 机制，因此一次丢失插曲产生一个额外的帧（重同步信号），而不是静默丢弃。`disconnect_on_full` 模式提供了一种严格的替代方案。

**客户端重连是完整的。** `WsClient` 有指数退避，`_lastSeen` 游标通过 `?since=` 进行服务器回填（`ws.js` 第 54-58 行），`SeqGate` 通过每个主题的单调序列号防止至少投递一次重复（`ws.js` 第 15-48 行）。

**迁移 0148 是明智的。** 影子分区表 + 幂等回填函数 + 未来分区维护函数，所有这些都在不会触及实时 `messages` 表的情况下构建。该策略实现了无锁的在线预迁移（步骤 B），在写入暂停的短时间窗口中执行最终切换（步骤 C）。验证过的 `messages-cutover.sql` 脚本存在且通过了临时数据库的端到端测试（第 5a 节）。

**错误分类已经超越了`Error::Internal(String)`。** 该枚举有 9 个命名的语义变体（`NotFound`、`Unauthorized`、`Forbidden`、`Conflict`、`Invalid`、`RateLimited`、`Upstream`、`Database`、`Serde`、`Internal`），每个都带 `status_code()` 和 `code()` 方法。`Upstream` 变体甚至被映射到 502，正确地将上游故障与内部错误区分开来。

### 1.2 局限性

**HLS 缺乏逐流授权。** 尽管通过我的审查，中间件的排序问题（详见下文第 1.3 节）已得到纠正，但核心的安全缺口依然存在：一旦中间件通过，`ServeDir` 会无差别地提供任何流段。知道或能猜出流 ID 和段序号的任何用户都可以观看任何直播。不存在签名的 URL、基于令牌的段授权或 referrer 检查。

**缺少每个主题的总线处理延迟直方图。** `run_bus_listener` 和 `run_live_bus_listener` 都没有记录 `aero_bus_process_latency_seconds`。当 IM 消息处理变慢时（例如，因大型会议室导致成员解析变慢，或因 AI 审核预算耗尽），操作者只能通过 WS 连接指标的下游降级来推断问题，而非看到总线处理器本身的延迟升高。

**错误枚举缺少 `retryable` 维度。** 虽然上游 502 与内部 500 是分开的，但 `Database`、`Serde` 和 `Internal` 变体并不能表明重试是否安全。幂等操作应自动重试可重试的数据库错误（如序列化冲突）；非幂等操作则不应重试。今天，调用者必须进行领域特定的猜测。

**客户端发送队列未实现。** WS 离线时，`send()` 返回 `false`，但 `sendMessage()`（`ws.js` 第 188 行）和 `sendMarkdown()`（第 195 行）会忽略返回值。不会有草稿持久化到 `localStorage`。不会有乐观的 UI 更新。当连接中断时，消息会静默丢失，用户也看不到任何视觉反馈。

**遥测库已到位但指标覆盖有缺口。** 虽然 `aero-common/src/metrics.rs` 中有一个自制的 Prometheus 注册表，并且涵盖了基本信号（消息吞吐量、WS 连接数、AI 作业），但缺少一些关键的运营信号：
- 没有 NATS 消费者待处理消息的仪表盘（在 `run_bus_listener` 中）
- 没有 WebSocket 扇出延迟直方图
- 没有按主题类型划分的消息吞吐量标签（IM 对直播）
- 没有 WebSocket 消息丢弃计数器（超出连接丢失曲线上游的产出速率）

**`aero-im-core` 和 `aero-server` 之间的职责划分不明确。** `ImService::publish_room_event` 发布到 NATS，但 `run_bus_listener`（在 `aero-server` 中）负责解码和扇出。这意味着 IM 核心 crate 不知道连接拓扑，这是好的，但这也意味着核心无法决定哪个参与者看到哪个帧——展开总是发生在服务器层。

### 1.3 关键设计决策

**中间件排序（HLS 路径）。** 验证报告声称第 123-125 行显示 HLS 绕过了中间件。但实际代码是：
```rust
let app = aero_server::routes::build(state.clone())
    .nest_service("/hls", ServeDir::new(&hls_dir))
    .fallback_service(ServeDir::new(&cfg.server.web_dir))
    .layer(middleware);                          // wraps everything above
```
在 Axum 中，`.layer(middleware)` 包裹了完整的路由器栈——API 路由、HLS 和回退。中间件**确实**适用于 `/hls/*`。验证报告在这一点上错了，而且它的修复（将 `.nest_service` 移到 `.layer` 之后）实际上会将 HLS **置于**中间件之外。

这个错误很重要，因为它改变了优先级计算。HLS 已经拥有了基本的中间件保护（速率限制、IP 白名单、CORS、请求追踪）。缺失的不是中间件覆盖，而是**逐段授权**。一个正确的修复涉及实现签名 URL（或令牌检查），而不是重新排序中间件行。

**NATS vs Redis 作为真实来源。** 设计正确地使用 NATS 作为事件广播的真实来源，并使用 Redis 进行集群状态（心跳、通话名单）。Redis sorted-set 方法（`zadd` + `zremrangebyscore`）适合带有 TTL 过期的心跳。仅靠进程内状态无法做到这一点。

**`NotifyBatch` 展开。** 总线监听器（`bus.rs` 第 60-70 行）在消费方展开 `NotifyBatch`，而非在发布时展开，从而避免了 O(N) NATS 扇出。这是对 NATS 缺少内置多播的合理优化。

**`backfill_messages_partition` 函数有意省略了 MLS 列（第 3 行）。** 预窗口回填循环不会复制 MLS 负载；窗口内最终同步会捕捉到这些负载。这是一个正确的不变量，但很容易让人误以为回填已完成。

### 1.4 架构/技术债务

| 债务项 | 位置 | 影响 | 修复成本 |
|---|---|---|---|
| HLS 缺乏逐段授权 | `serve.rs` + HLS 路径 | **安全**：任何带 URL 的人可观看任何直播 | **中**：添加令牌验证中间件 + URL 签名 |
| 缺少每主题延迟直方图 | `ws/ws_impl/bus.rs` | **可观测性**：无法在客户注意到之前检测到下行压力 | **低**：在解码前后添加 2 个 `Instant::now()` 点 |
| `Error` 缺少 `retryable()` | `aero-common/src/error.rs` | **可靠性**：重试逻辑必须由调用者进行领域猜测 | **低**：一个方法 + 每个变体一个布尔值 |
| 客户端 `sendMessage` 忽略 `false` | `web/ws.js` | **UX**：离线时消息静默丢失 | **中**：发送队列 + 重试 + 草稿持久化 |
| 消息表物理 TOAST 未回收 | 保留清扫 | **存储**：软删除的消息在 TOAST 中占用空间 | **低**：`VACUUM` 调度或每批 `DELETE` |
| 无分区切换（步骤 C） | `messages_partitioned` 影子已存在 | **存储**：性能随表增长而下降；维护窗口已规划但未做 | **高**：规划过的 1 小时窗口 + DBA |

---

## 2. 扩展方向

### 2.1 HLS 流授权（安全加固）— P0

**为什么需要：** 目前，任何知道 HLS URL 的人都可以观看任何直播流。对于付费直播或私人直播来说，这是一个业务关键的安全漏洞。

**核心挑战：**
- HLS 段是按需生成的静态文件；需要按段、按用户访问控制
- 令牌验证不得增加显著的延迟（段在 ~2 秒的块中送达）
- 必须支持直播和点播（录播）场景

**预期的架构变更：**
- 添加一个 HLS 认证中间件，该中间件提取签名令牌（查询参数或 cookie）
- 复用 `aero-common` 中现有的 HMAC 签名原语，构建时间受限的流 URL
- 或者：通过经过身份验证的 Axum 路由代理 `/hls`，动态验证授权
- 考虑 `nginx_secure_link_module` 风格的签名 + 到期时间戳

**对现有系统的影响：**
- 无：这是一个纯附加层。现有客户端的流 URL 停止工作，直到添加签名
- `web/` 播放器中的 HLS URL 生成需要通过令牌签名逻辑进行更新
- 向后兼容：允许空令牌（或签名 cookie），用于开发/内部部署

### 2.2 WebSocket 传输优化（批量化 + 压缩）— P1

**为什么需要：** 大型房间（>1000 名参与者）会因逐条扇出到每个连接而导致总线监听器延迟。此外，移动代价需要压缩。验证报告正确地将此识别为 P1。

**核心挑战：**
- 批量化：`fan_out_arc_inner` 为每个参与者、每个连接地循环。对于 1000 个用户，这是 1000 次 `try_send` 调用。共享的 `Arc<String>` 可以减少分配，但不能减少系统调用
- 压缩：`permessage_deflate` 需要 Axum/tungstenite 支持，这取决于 `tokio-tungstenite` 的特性标志。从一侧启用它不会向前兼容现有的连接
- 延迟权衡：为 3 个用户批量发送的消息会增加额外的尾部延迟

**预期的架构变更：**
- 在 `hub.rs` 中引入可选的分批扇出：每 ~10ms 收集帧，并以 1 个 WebSocket 帧批量发送
- 在 WS 升级处理程序中通过 `WebSocketConfig` 启用 `permessage_deflate`
- 为每个连接添加 `WsSender::set_compression`，因此升级后可以打开压缩（向后兼容）

**对现有系统的影响：**
- 中：批量扇出不会改变语义；帧在客户端的 `ws.onmessage` 处解包
- 现有的 `RESYNC_FRAME` + `lossy` 原子操作必须适应批量化（逐帧标记丢失，而非每批）
- 压缩是透明的手风琴——在通过 `?since=` 进行回填期间，只需要在服务器上提高内存

### 2.3 客户端离线韧性（发送队列 + 乐观更新）— P1

**为什么需要：** 目前，离线时发送的消息会静默丢失。对于 IM 和直播产品来说，这是一个关键的 UX 缺陷。验证报告正确地识别了这一点。

**核心挑战：**
- 发送队列必须在客户端进程之外持久化（`localStorage` 或 `IndexedDB`），以便在页面重新加载后幸存
- 乐观更新需要从 `send()` 中分离出可观察的模型，因此消息立即出现，带有“待发送”标记，并最终被真实消息替换
- 去重：乐观消息必须有临时 ID，以避免在 WS 恢复后与真实消息重复

**预期的架构变更：**
- 在 `web/ws.js` 中添加一个 `SendQueue`（或独立的 `web/send-queue.js`），其中包含一个持久的 `Map<tempId, {blocks, replyTo, roomId}>`
- 修改 `WsClient.sendMessage` 以加入队列并乐观地调度帧
- 在 model 层（`web/app.js` 或独立的 state 模块）中，添加“待发送”消息状态，该状态保留到接收到具有规范 ID 的 `message` 帧
- 添加基于草稿的事件（`compositionstart`/`compositionend` 或去抖的 `setTimeout`）以自动保存 `localStorage`

**对现有系统的影响：**
- 中：新的 `web/send-queue.js` 模块；对 `web/ws.js` 的最小侵入性（包装 `send()`）
- 现有 UI 必须处理新的“待发送”消息状态（视觉指示器 + 禁用编辑/反应直到确认）
- 向后兼容：没有发送队列的旧客户端可以继续工作（只是静默丢失）

### 2.4 消息数据生命周期（分区切换 + 归档层）— P2

**为什么需要：** `messages` 表是系统中最大的表。没有分区，查询性能会随着时间的推移而下降，并且物理 TOAST 回收（方向四）需要分区。迁移 0148 已经完成了 70% 的工作；阻力最小的路径是完成它。

**核心挑战：**
- 步骤 C 切换（验证过的 `messages-cutover.sql`）需要维护窗口并暂停写入
- 引入暖/冷存储层增加了一个数据路径：历史查询路由到较慢的存储
- 暖/冷路由必须对 IM 核心 crate 透明（仓储抽象）
- 冷存储出口（例如，S3 Parquet 转储）需要新的批处理作业

**预期的架构变更：**
- **步骤 C 切换**：按照经过验证的运行手册执行 `messages-cutover.sql`；添加 `ensure_messages_partitions()` 到启动维护定时器
- **暖/冷路由**：在 `MessageRepo` 中添加 `StorageTier` 枚举；在 `aero-storage/src/message/archive.rs` 中添加 `ArchivedMessageRepo`，用于 S3/S3 兼容存储
- **查询路径更新**：修改 `MessageRepo::list_by_room` 和 `get_by_id` 以路由到适当的层（或进行联合）
- **TOAST 回收**：添加 `DELETE FROM messages WHERE deleted_at IS NOT NULL AND deleted_at < now() - interval '90 days'` 作为定期任务

**对现有系统的影响：**
- 高：切换需要经过规划的维护窗口，其中包含测试过的回滚
- 对于活跃/存档分离，中等：仓储层的变化对服务层和路由层透明
- 向后兼容：所有现有查询继续工作；较旧的数据迁移到较慢的存储，不会导致功能退化

### 2.5 错误分类 + 消费者健康端点和指标 — P2

**为什么需要：** 无法诊断总线消费者为何落后。如果 AI 作业预算已耗尽，或者总线监听器正在处理大量负载，则没有端点来查询健康状态。验证报告正确地将此识别为 P2。

**核心挑战：**
- 消费者健康意味着回答：“我的 NATS 消费者还活着吗？它的 lag 是多少？它的处理延迟是多少？”
- Per-subject 指标需要 Prometheus 直方图的时间窗口和标签基数管理
- `retryable` 维度不能只是布尔值——有些错误在有限次数内是可重试的，然后才应成为死信

**预期的架构变更：**
- **`Error::retryable()` 方法**：添加到 `aero-common/src/error.rs`，每个变体一个布尔值。示例：`Database` → 大多数情况为 true，`Serde` → false，`Upstream` → true，`RateLimited` → true（使用退避）
- **`GET /health/consumers` 端点**：查询 NATS 消费者信息（每个耐久消费者的待处理消息）；返回 JSON map
- **指标升级**：在 `run_bus_listener` / `run_live_bus_listener` 中计算和记录 `aero_bus_process_latency_seconds`；为每种事件类型添加按事件类型的计数器（`aero_bus_messages_total{subject="im.room.*",event="message"}`）
- **审计可重试映射**：审计所有 `Err 分支 → 决策` 站点，以确保可重试错误被清晰地路由到重试逻辑，而非静默丢弃或死信重试

**对现有系统的影响：**
- 低：所有变化都是附加的。`retryable()` 是 Error 上的一个新方法；默认是 `false`。端点是一个新路由。指标是新的 Prometheus 系列
- 向后兼容：现有错误处理代码继续工作，忽略 `retryable()`。

---

## 3. 接口设计建议

### 3.1 错误处理的 `retryable` 特征

不要添加一个布尔方法，而是添加一个 trait，以便可以干净地区分可重试、致命和有限重试错误：

```rust
enum RetryPolicy {
    /// 不可重试——应导致死信。
    Fatal,
    /// 带有建议退避的无限重试。
    Retryable { backoff: Duration },
    /// 最多 N 次重试，之后转为 Fatal。
    Limited { remaining: u32, backoff: Duration },
}
```

将其与 `Error` 上的 `retry_policy()` 方法结合使用。这使调用者无需处理领域特定的分类。

### 3.2 为消息归档层设计的仓储抽象

消息仓储应该有一个干净的 trait 界面对服务层透明：

```rust
#[async_trait]
trait MessageRepository: Send + Sync {
    async fn get_by_id(&self, id: MessageId) -> Result<Option<Message>>;
    async fn list_by_room(&self, room_id: RoomId, pagination: &Pagination) -> Result<Vec<Message>>;
    async fn insert(&self, msg: &Message) -> Result<()>;
    async fn soft_delete(&self, id: MessageId) -> Result<()>;
}

// 活动层（Postgres 分区表）
struct ActiveMessageRepo { pg: PgPool }

// 归档层（S3 Parquet / 只读 PG）
struct ArchivedMessageRepo { s3: S3BlobStore }
```

在切换之后，服务层通过 `ActiveMessageRepo` 进行读写。归档读取通过一个联合存储以只读方式添加，该联合存储首先检查活动存储，然后才检查归档存储。

### 3.3 客户端发送队列的接口

客户端发送队列应该作为一个独立模块存在，具有清晰的公共 API：

```javascript
// web/send-queue.js
class SendQueue {
  constructor(storage);
  enqueue(frame, onSent, onFailed);
  dequeue(tempId);
  flush(wsClient);     // 在重连时尝试发送所有待处理项
  hasPending();
  saveDraft(roomId, text);
  loadDraft(roomId);
  clearDraft(roomId);
}
```

这使 `web/ws.js` 保持清洁；队列的责任被隔离。UI 层通过回调得到通知。

### 3.4 HLS 令牌验证的接口

一个 Axum 中间件或层，位于 `/hls` 路径之前，验证查询参数中的签名令牌：

```rust
// 使用现有的 HMAC 基础设施
pub fn hls_auth_layer(secret: &[u8]) -> impl Layer { ... }  // 或 middleware::from_fn

// “签名令牌” = base64(expiry_timestamp + ":" + stream_id + ":" + HMAC(key, message))
// 验证：+ 检查结构 + 检查未过期 + 检查 HMAC 匹配
```

这应该作为一个独立的 crate（`aero-hls-auth`）存在，或直接位于 `aero-server` 中，依赖 `aero-common` 中的 `signing` 基础设施。

### 3.5 向后兼容指南

- **HLS 令牌**：空令牌或缺失令牌应作为错误拒绝，但应添加一个配置标志以允许未签名访问用于开发。
- **WS 压缩**：`permessage-deflate` 是 WebSocket 升级协商的一部分；旧客户端将声明 `"server_no_context_takeover"` 和 `"client_no_context_takeover"`，新客户端将请求压缩。服务器进行无放弃协商。无需在应用层进行版本检测。
- **归档存储**：消息 ID 在迁移过程中保持不变（它们是 ULID）。来自归档层的消息在其 JSON 中可能有不同的 `storage_tier` 字段，但旧的客户端会忽略未知字段。
- **`retryable` 方法**：默认是 `Fatal`。现有的 `match` 臂保持不变；新增的代码会检查 `retry_policy()`。

---

## 4. 技术选型

### 4.1 是否需要新的依赖项

| 候选 | 需要？ | 理由 |
|---|---|---|
| 用于消息归档的 **Parquet/Apache Arrow** | 可选 | 如果归档转储需要分析查询。对于简单的 S3 存储（每批次 JSON），不需要。**建议**：从纯 JSON 开始；稍后根据需要添加 Arrow。 |
| 用于分区管理的 **pg_partman** | 可选 | 自动化月度分区创建。但迁移 0148 已经提供了 `ensure_messages_partitions()`。值低；避免引入扩展依赖。 |
| WebSocket 帧的 **zstd 压缩**（vs permessage-deflate） | 无 | `permessage-deflate` 是 WebSocket 标准且被广泛支持。zstd 需要应用层帧包装。坚持使用标准路径。 |
| 消息归档的 **RedisJSON** | 无 | 对于暖层来说太重了。使用 Postgres JSONB 或 S3。 |
| **OpenTelemetry（OTLP）导出器** | 未来 | 当前自制注册表足够。如果部署需要 Grafana Agent 或 Datadog 集成，添加 `opentelemetry-rust`。但对于 P0/P1 问题，不是防止性的。 |

### 4.2 自建与采购决策

| 问题 | 决策 | 理由 |
|---|---|---|
| HLS 令牌认证 | **自建** | 需要业务逻辑（流授权实时检查）。没有现成的 Axum 中间件能满足需求。 |
| WS 批量化 | **自建** | 极特定于 Hub 的内部数据结构。没有第三方库来解决此问题。 |
| 客户端发送队列 | **自建** | 与 model/state 紧密耦合。没有“购买”选项。 |
| 错误分类 + 重试 | **自建** | 特定于领域；`thiserror` + 枚举可以处理。 |
| 推送通知（FCM/APNs） | **自建**（已完成） | `aero-push` crate 已经是一个干净的门户，支持真实网关 + `FakeGateway`。无需 Firebase Admin SDK 包装器。 |

### 4.3 依赖项评估标准

为 Aero IM 评估新 crates 时使用的标准：

1. **必须支持 tokio 运行时**。阻塞 I/O 是禁区。
2. **必须是纯 Rust**（没有 C 绑定，也没有 `openssl` 链接）。str0m 树立了先例。
3. **版本兼容性**：必须针对当前的 tokio（1.x）、axum（0.7.x）、sqlx（0.8.x）栈进行编译。
4. **MSRV ≤ 1.80**：与项目的 MSRV 匹配。
5. **安全审核**：`cargo audit` 通过；没有已知的 RUSTSEC 公告。
6. **测试覆盖率**：首选具有良好单元测试和集成测试覆盖率的 crates。
7. **维护状态**：最近 6 个月内活跃；由于这个项目是“永久的”，不稳定、很少的 crate 是风险。

---

## 5. 实施路线图

### 5.1 优先级排序（P0 → P2）

| 优先级 | 方向 | 工时 | 理由 |
|---|---|---|---|
| **P0** | HLS 流授权（修复，非中间件排序） | ~2 天 | 安全漏洞：任何知道 URL 的人可以观看任何直播流 |
| **P1** | WS 传输优化（批量化 + 压缩） | ~3 天 | 可扩展性：大型会议室受到扇出循环 O(n) 行为的瓶颈 |
| **P1** | 客户端离线韧性（发送队列 + 乐观更新） | ~4 天 | UX：离线时消息静默丢失 |
| **P2** | 消息数据生命周期（分区切换 + 归档层） | ~5 天 | 存储/性能：大表性能退化；迁移 0148 已完成 70% |
| **P2** | 错误分类 + 消费者健康端点 | ~2 天 | 可观测性：没有总线健康可见性；retryable() 缺失 |

按顺序优先推出 P0，然后是 P1，最后是 P2。P2 项目可以并行推进。

### 5.2 阶段划分和里程碑

**阶段 1：安全加固（第 1 周）**
- 里程碑：HLS 授权中间件 + 签名 URL 生成上线
- 所有直播流需要经过身份验证的令牌

**阶段 2：发送 + 可扩展性（第 2-3 周）**
- 里程碑：WS 批量化 / 压缩 + 客户端发送队列 / 乐观更新上线
- WebSocket 吞吐量针对大型房间进行了可测量的改进
- 离线时的消息不会静默丢失

**阶段 3：存储 + 可观测性（第 4-5 周）**
- 里程碑：消息分区切换（步骤 C）+ 消费者健康端点 + `retryable()` 上线
- 消息存储在分区表中；TOAST 回收开始
- 操作者可以在客户注意到之前检测到总线延迟和滞后

### 5.3 风险及缓解策略

| 风险 | 可能性 | 影响 | 缓解措施 |
|---|---|---|---|
| **HLS 令牌破坏现有播放器** | 中 | 高 | 添加一个开发模式标志 `AERO_HLS_ALLOW_UNSIGNED`；在过渡期间默认为 true，然后在生产部署中翻转 |
| **WS 批量化增加延迟** | 低 | 中 | 使批量窗口可配置（默认 10ms）；添加 `aero_ws_batch_latency_seconds` 直方图用于监控 |
| **消息分区切换花费的时间超过维护窗口** | 中 | 高 | 在非生产环境中练习切换（临时数据库已验证）；有一个经过测试的回滚；在开始之前检查磁盘空间 |
| **客户端发送队列草稿超出 storage 配额** | 低 | 低 | 按房间限制草稿大小（每房间 50KB）；在加载时修剪过期草稿 |
| **HNSW 索引构建在切换期间导致超时** | 中 | 中 | 在窗口之前预构建 HNSW 索引（步骤 B：在影子表上构建）；设置 `maintenance_work_mem` 足够高 |

### 5.4 最终建议

1. **立即修复 HLS 授权**。验证报告在中间件排序的*机制*上是错误的（中间件*确实*包裹了 `/hls`），但*本质问题*（任何 URL 都可以播放任何流）是正确的。这是一个必须在上线前解决的安全漏洞。目前的代码没有做错什么，但正确设置中间件并不能修复授权问题。

2. **在发送队列时利用现有的客户端工作**。WebSocket 重连 + `?since=` 回填 + `SeqGate` 去重已经在线。唯一缺失的是 `send()` 不会在返回 `false` 时缓冲。从解决静默丢失开始：让 `send()` 在 WS 关闭时加入 `localStorage` 队列。

3. **执行消息分区切换**。影子表（迁移 0148）、回填函数和验证过的切换脚本已经在那里了。运行手册非常完整——只需执行它。这是系统中风险最高但最有回报的存储改进，而且它所依赖的基础设施已完全到位。

4. **不要重造错误分类**。现有的 9 个变体枚举已经很健壮了。唯一缺少的是一个 `retryable()` 方法。添加一个 `RetryPolicy` 枚举（传输草案中的第 3.1 节）是一个小变化，能带来很大的运营收益。

5. **将自制指标注册表迁移到 OpenTelemetry** 只有在需要 OTLP 导出时才进行。现在，Prometheus `/metrics` 端点满足了需求。在决定“升级”之前，先衡量可观测性方案的差距。

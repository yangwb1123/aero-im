# 架构分析与扩展建议报告

## 1. 架构评估

### 1.1 现有架构的核心优势

**事件驱动骨架的选择正确**。NATS JetStream + 进程内 Hub 扇出模式做到了：

- **跨实例一致性与本地低延迟的解耦**：NATS 保证全局有序，Hub 内 bounded mpsc 保证本地快速。这种"外部持久化一次，内部零拷贝扇出"的设计，避免了典型微服务架构中每个事件多次序列化/反序列化的开销。
- **consumer 语义精当分化**：`im.room.*` 用 durable consumer（at-least-once, 游标持久），`live.stream.*` 用 ephemeral consumer（重启容忍丢帧）。这不是偶然，而是对两个场景 QoS 需求的准确判断——消息丢失不可接受，弹幕丢失可接受。这种分化的成本极低（一个 `None` 参数），但对可靠性的收益很高。
- **水平扩展路径清晰**：Hub 是进程内结构，多开实例即自动水平扩展。NATS consumer 组（queue group）均衡负载。不需要引入额外的服务发现层。

**crate 分层遵守了"依赖单向、entity 独立"原则**。`aero-common` 作为叶子 crate 承载了所有共享类型（Block, RoomEvent, StreamEvent, Error），上层 crate 只依赖下层。这种分层使得：

- 编译缓存友好（修改上层不影响下层重编译）
- 测试隔离性高（可以单独测试下层 crate 而不需启动全量依赖）
- 避免循环依赖（已在 AGENTS.md 明确标注）

**枚举驱动的事件模型设计得当**。使用 Rust tagged enum（`RoomEvent`, `StreamEvent`）而不是字符串类型的事件类型，带来的好处：

- 全事件类型在编译器可见，新增 variant 时所有 match 分支会强制覆盖检查
- 序列化采用 tag="kind" 的 JSON 表示，跨语言仍可解析
- 与 Hub 的泛化扇出逻辑配合良好——Hub 不关心事件内容，只关心目标 subject

### 1.2 架构债务与局限性

**观察一：监控覆盖面与 bot consumer 数量之间的剪刀差正在扩大。** 方向一揭示的问题本质上是架构演进的时序问题：总线消费者数量随着 bot 系统扩展在增长（agent_bot, ooo_bot, unfurl_bot, transcribe_bot, moderation_bot, push_bot, webhooks, bot_dispatch 等），但监控基础设施只覆盖了最早的两个 consumer（aero-server, aero-ai）。这在架构上属于**可观测性债务（Observability Debt）**——每新增一个 consumer 不增加 backlog 监控，意味着消费者的积压故障成为静默故障。

这不是代码质量问题，而是架构治理问题：缺少一条"新增 durable consumer 必须同时注册 backlog 监控"的纪律规则。当前架构中，consumer 注册点（`bot_dispatch.rs` 第 66 行的 `const CONSUMER` 声明）与监控注册点（`metrics_tasks.rs` 的 `CONSUMERS` 数组）物理分离，且无自动化的交叉校验。

**观察二：bot dispatch 的 one-shot 投递设计是合理的架构权衡，但接口契约缺少显式声明。** 方向二确认 bot dispatch 无重试、无 HMAC 签名。从架构角度看，这不是缺陷——它设计为与其他内置 bot 一致（one-shot fire-and-forget）。问题在于：当一个新开发者阅读 `bot_dispatch.rs` 时，这个约束只在注释中说明，不在类型系统或 trait 签名中体现。一个名为 `DeliveryStrategy` 的枚举（`OneShot | RetryWithBackoff { max_attempts: u8, backoff_base_ms: u64 }`）如果存在于 `BusConsumer` trait 中，就能在编译期防止误用。

**观察三：`Hub` 结构缺乏多租户维度是合理的早期简化，但已接近转折点。** 方向四指出 Hub 的 `conns: DashMap<ParticipantId, Vec<WsSender>>` 没有工作区维度、无连接上限、无存储配额。在系统早期（单个工作区或少量工作区）这不是问题。但当工作区数量增长到数百时，一个工作区的异常行为（恶意大批量连接、突发高吞吐消息）可能通过共享的 Hub 线程池影响其他工作区。当前架构依赖 `ws_rate.rs` 在 API 层做限流——这是 HTTP 层的防护，但 `Hub::fan_out_raw` 在总线解码后直接调用，不受 API 限流的保护。

这里的关键架构问题是：**限流边界与资源消耗边界不匹配**。API 限流保护了 HTTP handler 的 CPU，但总线消费者到 Hub 的路径是独立的代码路径，从 NATS 解码后直接扇出。如果一个工作区产生了大量的 `NotifyBatch`（比如一条消息 @ 了 500 人），Hub 的工作量由展开后的 recipient 数量决定，不是由 API 限流的请求数量决定。

**观察四：messages 表的分区迁移方案是"已设计、未落地"的架构搁置（Architecture Shelf）。** 方向五确认迁移 0148 创建了分区影子表但注释明确说明未切换。`backfill_messages_partition` 函数已经存在且幂等。这是典型的**架构搁置**：设计工作和部分代码已经完成，但由于生产切换风险或优先级原因被搁置。搁置本身不是问题——但当数据量增长到触发性能退化时，切换决策需要"一键执行"而无需重新设计。当前的问题在于：缺少一个自动触发的切换条件（比如：当 `messages` 表大小超过 X GB 或插入延迟超过 Y ms 时告警并提议执行分区切换），使得切换依赖人工判断。

### 1.3 关键设计决策评估

| 决策 | 评价 | 替代方案对比 |
|---|---|---|
| NATS 作为总线 (vs Kafka/Pulsar/Redis Streams) | **正确**。NATS JetStream 的 consumer 语义灵活（durable/ephemeral/queue-group）、部署轻量（单二进制）、Rust 客户端成熟。Kafka 对于这个体量的系统太重，Pulsar 的运维复杂度不匹配。Redis Streams 缺少 durable 消费者组在跨实例崩溃后的可靠恢复。 | 如果未来需要长期消息回溯（7+ 天），NATS JetStream 的存储引擎可能会成为瓶颈，届时可考虑引入 Kafka 作为冷路径存储。 |
| 进程内 Hub 扇出 (vs Redis PubSub/separate fan-out service) | **正确**。通过 NATS 实现跨实例，实例内 Hub 负责本地扇出。避免了额外网络跳数和序列化开销。 | 如果实例内参与者连接数超过百万级，Hub 的 DashMap 可能成为锁竞争热点，可考虑 sharded per-CPU 结构。 |
| Bot 系统用 durable consumer (vs HTTP callback/polling) | **正确**。WebSocket 同构（都使用 NATS subject 模型），无需额外部署，事务性消费保证。 | HTTP callback 会引入额外的网络脆弱点且难以保证 at-least-once 语义。 |
| 枚举驱动的事件模型 (vs protobuf/avro schema registry) | **适当但需注意扩展性**。Rust enum 在编译期安全，但跨语言消费（如 web SPA）需要 JSON 兼容，且版本演进时要求所有 consumer 同时更新。 | Protobuf 更适合跨语言/跨团队场景，但架构复杂度更高。对于单一 Rust 后端 + Web SPA 的当前架构，tagged JSON enum 更轻量。 |

## 2. 扩展方向

### 方向 A：可观测性覆盖自动注册机制（P0）

**为什么需要**：方向一暴露的问题本质上是系统组件注册点分散、缺少中央治理。每新增一个 durable consumer，运维人员需要手动更新 `metrics_tasks.rs` 的 `CONSUMERS` 数组才能获得 backlog 监控。在实践中这一定会被遗漏——不是因为开发者疏忽，而是因为"加监控"和"加 consumer"发生在不同开发会话中。

从业务价值看：缺少 backlog 监控导致的问题通常是**隐式蔓延**的——NATS consumer 积压时不会报错，只是延迟逐渐增大。用户感知为"消息变慢了"而非"系统挂了"，故障定位路径长。

**核心挑战**：
1. **编译期 vs 运行期注册的抉择**：如果要求 consumer 在类型系统中注册（类似 `inventory::submit!` 模式），需要在 Rust stable 中寻找合适的静态注册机制。
2. **历史 consumer 后补**：现有 consumer 的监控注册需要一次性的数据补齐操作。

**预期架构变更**：
- 引入 `ConsumerRegistry` trait 或 struct，在新 consumer 初始化时自动注册到全局 registry
- `metrics_tasks.rs` 的定时任务改为遍历 registry 而非硬编码数组
- 可选：增加一个 Prometheus gauge 记录"已注册但无监控的 consumer 数量"

**对现有系统的影响**：
- 改动范围：`aero-server` 内的 `bot_dispatch.rs`、`metrics_tasks.rs`，以及各 bot 的初始化点
- 向后兼容：新增 registry，旧 consumer 逐步迁移。过渡期 registry 和硬编码数组并行。

### 方向 B：跨租户资源隔离层（P0）

**为什么需要**：方向四指出 Hub 缺少工作区维度的隔离。在单一租户场景下这不是问题，但在多工作区 SaaS 场景下，一个工作区的突发流量可能影响其他工作区。这不是"如果"的问题，而是"何时"的问题——当某个工作区的 bot 触发了大量消息展开（`NotifyBatch` 展开为 500 个 recipient），Hub 的 `fan_out_raw` 会消耗大量 CPU 发送 WebSocket 帧，在此期间其他工作区的正常消息也会被延迟。

**核心挑战**：
1. **隔离边界的选择**：在 Hub 结构中加入工作区维度（`DashMap<WorkspaceId, Vec<Connection>>`）是最直接的方案，但会增加查找复杂度和锁粒度。另一种方案是每个工作区分配独立的 `tokio::spawn` task，通过 bounded channel 通信——隔离性更好但资源成本更高。
2. **配额维度与粒度的确定**：连接数上限、扇出带宽上限、消息量上限。这些阈值不同工作区可能不同（免费 vs 付费）。
3. **打通现有限流体系**：`ws_rate.rs` 已有 API 层的 per-workspace 限流，新的 Hub 层隔离需要与之协调，避免双层节流导致可用性下降。

**预期架构变更**：
- Hub 结构增加工作区维度：`workspace_conns: DashMap<WorkspaceId, WorkspaceQuotaState>`
- 引入 `WorkspaceQuotaState`：包含当前连接数、当前扇出队列深度、限流阈值
- 扇出路径增加配额检查：超过阈值时降级为"慢路径"（每消息增加小延迟）或丢弃非关键事件
- 新增管理 API：查询每工作区 Hub 资源使用情况

**对现有系统的影响**：
- 最大化向后兼容：默认配额为"unlimited"，保持与当前行为一致
- 现有代码（`hub.fan_out_raw`, `hub.add_connection` 等）调用签名不变，内部增加配额检查
- 风险：配额检查增加了扇出路径的额外查找，需注意性能（建议使用 per-core sharded counter 而非全局原子操作）

### 方向 C：消息表分区切换正式化（P0-P1）

**为什么需要**：方向五确认分区方案已设计但未落地。这是典型的"设计 Debt"——设计做完了、代码写好了、但生产切换没执行。分区对查询性能的影响是渐进式的：当 `messages` 表超过数亿行时，基于时间范围的查询（"获取最近 7 天的消息"）会持续退化，直到触发 DBA 应急。

从风险角度看最严重的问题还不是性能，而是**无 plan B 的运维压力**：如果某天主表膨胀到需要紧急分区，此时再设计分区方案、测试、切换，风险远高于在空闲期做完切换。

**核心挑战**：
1. **零停机切换方案**：迁移 0148 创建了 shadow 分区表但没有切换逻辑。需要设计一个从"写入主表 + 通过 trigger 写入分区"到"写入分区表"的原子切换过程。
2. **历史数据迁移的自适应速率**：`backfill_messages_partition` 已经存在，但需要 pace control 避免在迁移期间 IO 飙升影响在线流量。
3. **查询路由兼容**：迁移后所有读路径需要从"查 messages 表"改为"查分区视图或分区表"。如果某些查询写了 `FROM messages` 硬编码，需要改为视图或 CTE。

**预期架构变更**：
- 创建分区视图（或函数），作为旧查询的兼容层
- 实现一个 `tokio::spawn` 的 backfill coordinator：检查当前迁移进度，按可配置速率回填历史数据，完成后标记迁移完成
- 新增一个只读模式标志：迁移过程中允许写入新旧两套表，但读取优先从分区表查
- orchestration 脚本：`scripts/partition-migrate.sh` 执行渐进式切换

**对现有系统的影响**：
- 高风险操作，建议分 3 阶段（shadow 写入 → 双写 + backfill → 切换读路径），每阶段之间至少间隔一个观察周期
- 需要新增 Postgres 视图（或表函数）保持读路径兼容
- 仓储层可能需要新增 `PartitionAwareMessageRepo`（或现有 `MessageRepo` 的切换开关）
- 回滚方案：分区切换支持"反向合并"脚本（虽然生产几乎不会执行，但需要在计划中）

### 方向 D：Bot dispatch 的契约化与可审计性增强（P1）

**为什么需要**：方向二指出 bot dispatch 缺少重试和签名。这是经过设计的权衡（与内置 bot 保持一致），但有两个风险：
1. **缺少显式契约**：当前约束只在注释层面，新开发者容易误以为会重试
2. **缺少可观测性**：one-shot 投递失败后没有任何痕迹。如果 webhook endpoint 间歇性故障，失败事件完全丢失无法追溯。

**核心挑战**：
1. **重试策略的架构定位**：重试应该放在 bot dispatch 层（通用）还是放在每个 bot 内部（定制）？前者更易于统一管理，后者更灵活。建议采用"默认无重试，可选的配置化重试"的折中方案。
2. **空签名的影响范围**：HMAC secret 为空意味着任何知道 webhook URL 的人都可以伪造请求。这是否需要修复取决于 webhook URL 的保密性假设——如果 URL 包含足够熵的随机 token（类似 GitHub webhook secret），空签名可能可接受。但当前架构中 `bot_event_subscriptions` 没有 per-subscription secret 列，这意味着所有订阅共享同一个空签名，安全边界不够清晰。

**预期架构变更**：
- `BotDispatchConfig` 新增 `retry_policy` 字段（None / Limited { max_attempts, backoff_ms }）
- 失败事件可以写入一个可选的死信队列（复用 `aero-storage` 已有的 DLQ 模式）
- 日志记录投递结果（成功/失败），Prometheus counter 追踪 `bot_dispatch_delivery_total{status="success|failed"}`
- 签名方面：在 `bot_event_subscriptions` 表新增 `hmac_secret` 列（nullable），旧记录按空签名处理，新订阅生成随机 secret

**对现有系统的影响**：
- 低影响：bot dispatch 当前是独立的代码路径，改动不影响其他 bus consumer
- 重试策略的默认值保持 "None"（即当前行为），配置化可选升级
- 死信队列可选，不引入强制依赖

### 方向 E：Live stream ephemeral consumer 效率优化（P2）

**为什么需要**：方向三确认每实例接收全部 `live.stream.*` 事件，解码后判断是否需要丢弃。这是 ephemral consumer 的固有特性——没有消费组的概念，每个订阅者都收到全部消息。当前架构中，直播频道数量 * 弹幕频率决定了无效解码的开销。

从量化分析看：如果单个流每秒 100 条弹幕 + 5 个 live 实例 → 每实例每秒解码 500 条但实际扇出 100 条 → 效率 20%。效率随实例数和直播频道数线性下降。

**核心挑战**：
1. **去中心化的 watcher 聚合**：为了"只收到本地有 watcher 的流的事件"，需要每个实例向一个中央 registry 注册自己的 stream watcher 集合，NATS producer 侧或 NATS 侧做过滤。这破坏当前"publisher 不知道谁是消费者"的松耦合模型。
2. **注册状态的一致性**：如果实例崩溃，其 watcher 集合需要自动注销。Redis sorted-set 带 TTL 可以做到，但需要引入额外的基础设施依赖。

**预期架构变更**：
- 两阶段实施：
  - 阶段一（低投入）：打点，在扇出前记录 `live_events_received_total` 和 `live_events_discarded_total`，确认浪费率是否达到优化阈值
  - 阶段二（有条件）：引入 NATS subject 的动态 filtering，每个实例订阅其本地 watcher 正在观看的流的 subject，而非全部 `live.stream.*`
- 如果采用 filtering 方案，需要 `stream_watchers` 的定期快照推送到一个集中式 registry

**对现有系统的影响**：
- 阶段一无影响（只加 metrics）
- 阶段二是架构侵入性变更，`run_live_bus_listener` 的订阅逻辑需要重写
- 考虑到直播场景对弹幕延迟敏感，不要在 QPS 极低时做此优化（"不要优化不需要优化的东西"）

## 3. 接口设计建议

### 3.1 关键新增接口

#### ConsumerRegistry trait

当前缺少一个体系化的方式让新 consumer 注册自己的监控指标。建议新增：

```
trait MonitoredConsumer {
    fn consumer_name(&self) -> &'static str;
    fn consumer_type(&self) -> ConsumerType; // Durable | Ephemeral
    fn subject_pattern(&self) -> &'static str;
    fn pending_warning_threshold(&self) -> Option<u64>; // 可选告警阈值
}
```

每个 `run_*_listener` 函数的返回值可以实现此 trait，`metrics_tasks.rs` 的定时任务遍历所有已注册的 `MonitoredConsumer` 实例。

**设计原则**：
- 注册是隐式的（构造时自动注册），而非显式的（无需手动添加）
- 支持编译期注册（通过 `ctor` crate 或 `linkme`）和运行期注册
- 泛型化 consumer info 收集，避免每个 consumer 重复实现

#### IsolationQuotaProvider

用于多租户资源隔离的接口：

```
trait QuotaProvider {
    fn get_quota(workspace_id: WorkspaceId, resource: QuotaResource) -> QuotaLimit;
    fn report_usage(workspace_id: WorkspaceId, resource: QuotaResource, delta: i64);
}

enum QuotaResource {
    Connections,
    FanOutBandwidth,
    MessageRate,
}

struct QuotaLimit {
    hard_limit: u64,      // 硬限，超过即拒绝
    soft_limit: u64,      // 软限，超过即延迟
    burst_window: Duration, // 突发窗口
}
```

**设计原则**：
- QuotaProvider 与具体限流算法分离（provider 只提供配额和报告接口，限流算法在 Hub 内部）
- 默认实现（`NoQuotaProvider`）返回 unlimited，保持向后兼容
- 配额来源可配置（Redis / 配置文件 / 数据库）

### 3.2 需要引入的抽象层

**Consumer Backlog Monitor Registry**（方向一/A）

当前架构缺少一个"consumer 注册 → 监控自动生成"的管线。需要引入一个轻量的运行时 registry：

```
// 思路：全局 PERIODIC registry, not trait object overhead per message
static CONSUMER_MONITORS: Lazy<RwLock<Vec<ConsumerMonitor>>> = ...;
```

核心设计决策：**用运行时注册而非编译期注册**。原因是 Rust stable 的编译期注册机制有限（`linkme` 是 nightly），且运行时注册更灵活（允许按配置开关 consumer）。

**Delivery Strategy 枚举**（方向二/D）

为总线消费者引入显示的投递策略契约，替代当前注释层面的约束：

```
pub enum DeliveryGuarantee {
    /// 投递失败即丢弃（与现有 bot dispatch 行为一致）
    AtMostOnce,
    /// 可配置重试次数和退避策略
    AtLeastOnce {
        max_attempts: u8,
        backoff: Duration,
        dead_letter_queue: Option<DeadLetterConfig>,
    },
}
```

**设计原则**：
- 策略定义在 consumer 构造时，与 consumer 的消费逻辑分离
- 策略的变化不影响 consumer 的业务逻辑
- 默认值与当前行为一致，避免隐式变更

### 3.3 向后兼容策略

1. **监控注册**：新增 `MonitoredConsumer` trait 与现有硬编码数组并行。过渡期内，已注册的 consumer 自动纳入监控，硬编码数组作为 fallback。两个版本后移除硬编码数组。

2. **资源隔离**：隔离层默认返回 "unlimited"，不改变现有行为。配置化开启。`NoQuotaProvider` 作为默认实现，在性能关键路径上条件编译检查（release build 中空实现可以被内联消除）。

3. **消息分区**：分区视图与旧查询兼容。双写模式只在切换窗口启用，完成后所有读写通过视图/函数完成。

4. **投递策略**：枚举的默认值为 `AtMostOnce`，与现有行为 100% 一致。公开文档标注现有约束。

## 4. 技术选型

### 4.1 新引入技术栈评估

| 场景 | 推荐方案 | 备选方案 | 决策依据 |
|---|---|---|---|
| consumer 监控注册 | Rust 内置 `std::sync::OnceLock` + 运行时注册 | `ctor` crate / `linkme` / `inventory` | 运行时注册不依赖 nightly，且对 hot-reload 友好。`linkme` 优雅但需要 nightly。 |
| 资源配额存储 | Redis（复用现有 Redis 连接） | Postgres / in-memory | 配额需要在多个实例间共享（分布式限流），Redis 的 atomic ops 是正确选择。现状已有 Redis 7 连接。 |
| 分区迁移协调 | Rust 内 `tokio::spawn` + advisory lock | 外部脚本 / pg_cron | 与现有 boot 模式一致（定时器），advisory lock 防止多实例冲突。无需引入新依赖。 |
| webhook 签名 | HMAC-SHA256（标准库 `hmac` crate） | JWT / 自定义签算 | HMAC 是最小依赖方案，`hmac` crate 已经作为间接依赖存在。JWT 太重。 |

**关键原则：不引入新的基础设施**。当前系统已经依赖 Postgres 17 + Redis 7 + NATS。任何新的架构组件首先评估能否在既有基础设施上实现。例如：

- consumer 注册监控 → 纯 Rust，不依赖外部存储
- 跨实例 watcher 聚合 → Redis sorted-set（已有 presence 使用）
- 分区迁移状态 → Postgres advisory lock（已有类似先例）

### 4.2 第三方依赖评估标准

对于 Aero IM 的架构上下文，新增第三方依赖应满足以下条件（按优先级排列）：

1. **无需守卫**：纯 Rust 实现，C 绑定仅做 FFI（如 `sqlx`/`fred`），无 `build.rs` 编译依赖（如 `openssl-sys`）
2. **不过度抽象**：解决具体问题，不是"全家桶"框架。`axum` 是例外——路由层的框架抽象是必要的
3. **维护活跃度高**：最近 release 在 6 个月内，issue 响应及时
4. **API 稳定**：semver 兼容性，避免频繁 breaking change
5. **与现有依赖兼容**：不引入与既有 crate 冲突的 tokio 版或 hyper 版

**应避免引入**：
- `apache-kafka` 客户端（引入 librdkafka C 依赖）
- `redis`（使用 `fred` 而非 `redis-rs`，`fred` 已是选择）
- `reqwest`（已在依赖中，不在业务模块重复引入）
- 大型 protobuf codegen 管线（当前 JSON 序列化够用）

### 4.3 自建 vs 采购/引入的决策

**推荐自建**的场景：
- consumer 监控注册：系统特有的组件，无现成库；且需要与既有 NATS API 深度集成
- 资源配额逻辑：业务语义（按工作区 tier 差异化配额）决定了无法直接复用通用限流库

**推荐引入**的场景：
- HMAC 签名：标准算法，`hmac` + `sha2` crate 即可，不应自建
- backoff 退避计算：`backoff` crate（或更轻量的自行实现指数退避函数）
- JSON schema 校验（如未来需要）：`jsonschema` crate

**混合模式**：
- 分区迁移：借助 `sqlx` 的 `migrate!` 能力（已有，自建迁移脚本）+ 自定义协调逻辑
- 投递重试：通用重试轮子（`tokio-retry` 或自建 `loop + sleep`）配合业务死信队列逻辑

## 5. 实施路线图

### 5.1 优先级排序

```
P0 ───┤ 方向 A（可观测性覆盖自动注册）—— 消费者 backlog 监控是运维基线，不解决等于盲跑
       │ 方向 B（跨租户资源隔离）—— 如果有多工作区部署，先于 SaaS 上线前完成
       │
P1 ───┤ 方向 C（消息表分区正式化）—— 数据量到达性能拐点前必须完成，但可暂不切换
       │ 方向 D（bot dispatch 契约化）—— 安全性和可审计性改进，但现有行为可接受
       │
P2 ───┤ 方向 E（live stream 扇出效率优化）—— 需要打点数据确认浪费率，在此之前不投入
```

**排序依据**：
- P0 的依据是"不解决 = 有盲区"（监控缺失意味着问题不可见，不可见的问题等于累积风险）
- P1 的依据是"数据到了一定规模必有问题"（分区不落地意味着 DBA 只能硬扛）
- P2 的依据是"当前浪费率未知"（先打点再决策，避免优化不需要优化的东西）

### 5.2 阶段划分

#### 阶段一（2-3 周）：可观测性补齐 + 分区就绪

**交付物**：

- 方向 A：`ConsumerRegistry` 实现，所有既有 consumer 迁移至自动注册
- `metrics_tasks.rs` 改为遍历 registry 而非硬编码数组
- Prometheus alerting rules 更新：对每一个注册的 durable consumer 自动生成 backlog 告警
- 方向 C：分区切换的 orchestration 代码完成 + staging 环境验证通过
- `scripts/partition-migrate.sh` 脚本完成
- 分区切换的回滚方案文档化

**验收标准**：
- 新增一个用于测试的 durable consumer，其 backlog 监控自动出现在 `/metrics` 端点
- staging 环境执行完整的分区切换演练（创建 → 双写 → 回填 → 切换 → 验证 → 回滚）
- 分区切换全过程无消息丢失、无中断时间 > 1 秒

#### 阶段二（2-3 周）：资源隔离层 + bot dispatch 契约化

**交付物**：

- 方向 B：`QuotaProvider` trait + 默认的 `NoQuotaProvider`
- Hub 层增加连接数配额检查和扇出带宽配额检查（默认 unlimited）
- 配置化开启：`AERO_HUB_QUOTA_ENABLED=true` 时自动加载 Redis-based QuotaProvider
- 方向 D：`BotDispatchConfig` 增加投递策略字段
- 失败事件的死信队列（可选，基于现有 DLQ 模式）
- HMAC 签名：`bot_event_subscriptions` 表新增 `hmac_secret` 列，新订阅自动生成

**验收标准**：
- 配额开启 + 压测一个工作区超限，验证其他工作区不受影响
- bot dispatch 失败后日志记录清晰，Prometheus counter 可观测
- 旧订阅（无 `hmac_secret` 列）向后兼容

#### 阶段三（1-2 周）：直播效率打点 + 分区生产切换

**交付物**：

- 方向 E：`live_events_received_total` 和 `live_events_discarded_total` 计数
- 打点数据驱动的决策报告：如果浪费率 > 60% 则启动效率优化设计
- 方向 C：生产环境分区切换执行
- 切换后的 7 天监控期（观察查询延迟、写入延迟、autovacuum 行为）

**验收标准**：
- 打点数据展示每实例效率比，辅助决策是否继续优化
- 分区切换后 7 天无与分区相关的 P0/P1 告警
- 查询延迟 <= 切换前基线

### 5.3 风险点与缓解策略

| 风险 | 概率 | 影响 | 缓解策略 |
|---|---|---|---|
| **分区切换导致写入中断** | 低 | 极高（消息不可写） | 分阶段切换，每阶段可回滚；双写期间任一系统故障可切回单写；生产切换选低峰期 |
| **监控注册增加 consumer 初始化延迟** | 中 | 低（毫秒级） | builder 模式 + 注册操作异步化（`tokio::spawn` 注册，不阻塞 consumer 就绪） |
| **资源配额检查成为扇出路径热点** | 中 | 中（~5% 延迟增加） | per-core sharded counter 减少原子操作争用；`NoQuotaProvider` 条件编译为无开销 |
| **bot dispatch 重试引入重复投递** | 中 | 中（下游去重负担） | `idempotency_key` 头传给 webhook endpoint；文档标明 at-least-once 语义要求下游去重 |
| **直播扇出优化设计过度** | 中 | 低 | 阶段一彻底打点，浪费率低于 40% 即不投入阶段二；AGENTS.md §4.5 的 seam 管理哲学适用 |

### 5.4 实施原则

1. **每次改动只解决 1-2 个场景**。不尝试在单次迭代中同时修复方向 A 到 E。每个方向独立可交付、可验证、可回滚。

2. **监控先行，隔离在后**。没有可观测性就去加资源隔离，等于"加了门但看不到门后有什么"。

3. **向后兼容不是可选，是强制。** 每个新增 trait/接口/配置项都有默认行为，且默认行为与现有系统一致。

4. **分区切换采用"可逆提交"原则。** 在切换前确保切换脚本和回滚脚本都经过 staging 验证，且两个脚本的执行时间都在可控范围内（< 30 分钟）。

5. **不在 P2 项目上投入 P0 级别的测试覆盖。** 直播效率优化（方向 E）在打点数据出来之前只投入设计文档和 metrics 代码，不投入架构重构。

## 总结

Aero IM 的架构基础是健康的——事件驱动骨架、crate 分层、枚举驱动的事件模型都是合理且经住验证的设计决策。五个方向的缺口本质上是**架构演进的自然褶皱**：系统从单租户到多租户、从有限监控到全面可观测、从单一 consumer 到 bot 生态系统扩张的过程中，原有简化假设开始达到边界。

最紧急的两个工作是**可观测性覆盖的自动注册机制**（消除盲区）和**跨租户资源隔离层**（防止级联故障）。这两个工作在架构上独立、在工程上可并行。消息分区切换虽然风险大，但设计与代码已就位，执行只是时间问题。直播效率优化是唯一需要先打点才决定是否投入的方向，符合"不优化不需要优化的东西"的原则。

最终建议：以 P0 的两个方向为起点，启动一个为期 6-8 周的"架构韧性冲刺"（Architecture Resilience Sprint），集中补齐可观测性和隔离性缺口，然后在常规迭代中完成分区切换和其他改进。

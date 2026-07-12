现在我已经充分了解了代码库。以下是架构分析。

---

# 架构分析：Aero IM 平台

## 1. 架构评估

### 现有架构的优势

当前的架构展示了一些非同寻常的品质，这些品质在“功能丰富的演示”（功能矩阵庞大）中并不常见，但往往为*可运营的产品*奠定基础。

**1. 意图明确的、分层的事件管道。** 设计（NATS 作为跨实例事实来源 → 每个进程内的 Hub 作为本地扇出）是一种久经考验的模式，它将持久性（JetStream 耐久消费者）与低延迟本地广播（有界 mpsc）分离开来。这种分离使得水平扩展无需对应用层代码进行任何修改：添加更多实例即可用。这是一个正确的决策。

**2. 用于共享基础设施 crate 的抗循环依赖规则。** `aero-common` 作为叶节点（叶子 crate），除了共享类型之外不依赖任何东西，这不仅仅是一种组织选择——它强制执行了良好的模块化。不能轻易地在层级结构中引入环，这使得架构保持可推理。许多代码库一开始是干净的，但最终会出现循环引用；这里的意愿性护栏将能长期维持这种健康状态。

**3. `AGENTS.md` 中用于添加功能的严格配方。** 迁移 → 仓库 → 路由 → 认证 → 实时流程是程式化的，并且有据可查。这对于一个 16 个 crate 的工作区来说不是小事——它降低了新贡献者的认知负荷，并确保新功能不会意外地跳过认证检查（`authz_lint` 在 CI 中兜底）。

**4. 承认假设并明确边界。** 缓存模块中的文档（`participant_cache.rs`“60 秒的陈旧性对显示名称是可以接受的”，`room_member_cache.rs` 类似的承认）是不同寻常的。大多数代码库都默默地忽视了一致性；这里的事实陈述意味着作者*理解*其取舍。`AGENTS.md` 中的“禁区”列表（无联邦，无 MLS 状态机，无移动端原生 SDK）防止了范围蔓延。

**5. 多协议感知：不仅仅是 HTTP。** 该系统原生支持 RTMP（TCP :1935）、SRT（UDP）、WHIP/WHEP（WebRTC 信令通过 REST）以及 HTTP/REST + WebSocket。能够将直播摄入（RTMP/SRT）与实时信令（WS）结合在一个进程中是一种架构上的优势——它避免了在多个服务之间编排流时出现的“跨服务媒体管线”问题。

### 主要局限性

**1. 缓存一致性模型不完整（以文档形式存在，但未强制执行）。**
当前对陈旧性的容忍（“60 秒的陈旧性对显示名称是可以接受的”）在权限边界处是错误的。如果一个节点更新了用户的角色（`member_role` 从 `member` 变为 `admin`），或者强制启用了双因素认证（`requires_2fa`），另一个节点可能会在长达 60 秒的时间内为该用户提供*授权决策*。这是一个安全缺陷，而不仅仅是 UX 问题，并且在任何规范的操作中都应该被消除。
*深度问题*：缺乏跨节点失效总线意味着集群最终会以不一致的授权状态运行。与 UX 目录（显示名称）不同，授权缓存是不安全的。

**2. 对 PG 主库的依赖是单点风险。** “水平扩展”的故事是通过 NATS + 额外实例来实现的，但数据访问层将所有查询指向单个 PG 主库。即使在低并发情况下（`max_connections=16`），`room.members()` 查询与回执查询之间的资源竞争也是一个可预测的瓶颈。数据层不存在“水平扩展”——它存在物理限制。

**3. 持久投递不存在。** 没有 `delivery_cursor`——没有每个参与者的游标来记录“谁收到了什么”。重连完全依赖于按房间的背填（`list_since`，上限 200），这具有 O(N) 复杂度，且 N（离线期间的消息数）会随着房间和设备的数量膨胀。多台设备会多次重新扫描同一数据，从而在故障恢复期间加倍 DB 负载。没有可投递记账，该平台无法为“消息已投递”提供确定性证明——这是 IM 的核心承诺。

**4. 每项 AI 查询都是顶级请求，无论其复杂程度如何。** “成本分层”缺失：一个简单的“现在几点钟？”和一个复杂的“总结本季度的研发支出”都调用相同的 `ANTHROPIC_MODEL`（通常是 Sonnet，这是最昂贵的层级）。缺少对难度的估计，也缺少语义缓存来捕获近似重复的问题。AI 成本是最大的单用户运营费用——如果不进行分层，系统实际上是在以每个查询的最高价格补贴每个琐碎的查询，从而侵蚀毛利。

**5. 用于运营的可观测性虽已实现，但仍有间隙。** 虽然追踪并未像初始扫描声称的那样被“破坏”（`traceparent` 确实通过 NATS 信封传播），但仍存在间隙：没有每个工作区的延迟服务等级目标（SLO），没有持久消费者滞后告警的自动化中断阈值，并且日志是纯文本格式，而不是可以使用追踪 ID 进行过滤的结构化 JSON。

### 架构债务

| 债务 | 位置 | 影响 | 处理难度 |
|--------|----------|--------|----------|
| 无统一缓存抽象—每个缓存（参与者、房间成员、AI 答案）都有自己的失效策略 | 跨 `storage/` 和 `aero-ai/` 的多个模块 | 新缓存在没有失效路径的情况下被添加，或者具有不一致的行为 | 高（需要一个通用的 `CacheBackend<T>` trait） |
| `EventBus::publish` 签名无标头 | `aero-bus/traits.rs` | 追踪上下文被塞进有效载荷，而不是使用 W3C 标准标头 | 中等（需要破坏性变更或带有 `Option<Headers>` 的 trait 默认实现） |
| 日志是纯文本 stdout | 每个二进制文件 | 无法按 `trace_id` 在日志行之间进行关联 | 低（格式化配置，使用 `tracing-subscriber` 的 JSON 层） |
| 没有模式注册表；WS 帧和 webhook 有效载荷是无版本的 | 跨 `ws/ws_impl/` 和 `server/webhook.rs` | 破坏性变更是一个事件 | 中等（需要存储模式和客户端如何处理模式版本的约定） |
| 连接池配置（`max_connections=16`）是硬编码的缺省值 | `common/config.rs` | 未针对部署进行调整；高流量场景下的连接饥饿 | 低（只是一个配置默认值） |

---

## 2. 扩展方向

以下六个方向建立在上一轮全面的代码扫描和分析的基础上，但优先考虑*我的评估*，即哪些方向将系统地解锁最大的架构价值。

### 方向 1：统一的失效总线（P1 — 架构完整性）

**为什么需要它。** 当前的缓存失效策略是零散的：一些缓存使用 TTL（被动），一些使用内联无效（写入时），没有一个跨节点工作。失效总线（在 NATS 上使用 `cache.invalidate.{key}` subject）将提供单一事实来源，告诉我们“此键已过时”。这对于授权缓存（角色、2FA 状态、权限）的跨节点一致性至关重要，这些缓存目前有一个 60 秒的安全漏洞窗口。

**核心挑战。**
- **因果排序**：失效事件必须相对于它们所针对的写入事件进行排序。如果一个写入操作将 `role=admin`，然后立即写入 `role=member`，则顺序错误的无效操作可能会使缓存处于陈旧状态。
- **幂等性**：NATS 是至少一次投递。`cache.invalidate.{key}` 是幂等的（“此键已过时”是一个幂等操作），但消费者必须处理好重复。
- **启动风暴**：节点重启后，所有缓存条目都会老化（TTL）。依赖 TTL 作为兜底是可行的，但针对授权缓存的主动失效应该预填充。

**预期的架构变更。**
```
Proposed new subjects: cache.invalidate.{key}       (durable consumer "aero-cache")
                        cache.warm.{key}.{value}     (ephemeral, for cache warming)
```
- 向 `CacheBackend` 特性添加一个 `subscribe_invalidations(bus: &EventBus)` 方法。
- 写入缓存时，同时在 NATS 上发布失效事件。
- 所有实例都订阅 `cache.invalidate.*` 并立即将失效条目标记为过时（或将其删除）。

**对现有系统的影响。** 对现有代码的影响很小，前提是有一个一致的 `CacheBackend` 特性。迁移路径：用一个使用失效总线的统一实现替换现有的临时缓存（参与者、房间成员），将遗留代码库保留在旧模块中。

---

### 方向 2：持久投递台账 + 紧凑离线同步（P1 — 核心 IM 承诺）

**为什么需要它。** 没有持久投递（`delivery_cursor`），IM 的关键承诺——“消息已送达”——就无法得到证明。重连是 O(N) 扫描，并且多设备会每个房间重复放置背填，从而在重新连接事件期间导致 DB 负载成倍增加。这将限制重度用户（成千上万条消息、多个设备、长时间离线）的可靠性，并阻止“离线优先”的桌面客户端。

**核心挑战。**
- **并发游标推进**：两台设备都收到消息 A，都立即发送 ack。日志结构合并树（LST）只在游标上向前推进：`WHERE last_seq < $1` 进行原子更新可防止设备 B 撤销设备 A 的确认，但可能需要一个咨询锁来防止竞态条件。
- **序列空间**：`delivery_cursor` 需要全局单调的每个房间序列号（与 `aero-bus/seq.rs` 中现有的每个 subject 序列号不同）。序列必须稳定跨节点（使用 Redis `INCR` 或 PG `SEQUENCE`），而不是使用本地原子操作。
- **法务保全交互**：被保全的消息不能从游标中跳过，即使它被“软删除”了。游标必须跳过被清扫的消息，但保留被保全的消息。

**预期的架构变更。**
```
New table: delivery_cursors (
  participant_id  UUID NOT NULL REFERENCES participants(id),
  room_id         UUID NOT NULL REFERENCES rooms(id),
  last_seq        BIGINT NOT NULL DEFAULT 0,
  updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (participant_id, room_id)
)
```
- WS 帧：新的 `ack_delivery`（客户端 → 服务器）和 `backfill_delta`（服务器 → 客户端，带有 gap 内的总消息数和序列范围）。
- 重连协议：`?since=<seq>` 返回 `seq > cursor` 的消息，O(log N) 而不是 O(N)。
- 背压：未确认数量 >1000 → 发送 `backpressure_for_rooms` 帧，以便客户端可以丢弃低优先级的订阅。

**对现有系统的影响。** 中等到高。现有的背填协议（`list_since`，上限 200）必须与新的游标驱动重连共存，至少在过渡期内是如此。Web SPA 必须处理新的 `ack_delivery`/`backfill_delta` 帧，并且必须遵守背压信号。服务器端必须处理游标推进和向后兼容。

---

### 方向 3：AI 成本分层 + 语义缓存（P1 — 经济可持续性）

**为什么需要它。** AI 是 Aero IM 的*最大单一用户成本*，也是其*核心差异化因素*。如果没有分层路由，每个平凡的查询都会按 Sonnet 的全价计费。对于一个将 AI 作为默认交互模式的系统来说，这个成本的差距会造成单位经济效益为负，除非进行修复。语义缓存（针对近似重复查询）在此基础上进一步增加了 20-30% 的增益。

**核心挑战。**
- **难度估计**：如何在不调用该昂贵模型的情况下预测一个查询是“简单”（可以由 Haiku 回答）还是“困难”（需要 Opus）？试探法：令牌计数、领域关键词（“总结”/“分析” → 困难，“什么是”/“何时” → 简单）、检索到的文档质量（向量相似度分数 >0.9 表示答案可能很简单）。
- **多租户缓存隔离**：工作区 A 的缓存答案绝不能命中工作区 B。缓存键必须包含 `workspace_id` + `participant_bounds`（成员身份边界）。当被引用的消息被编辑或删除时，缓存失效会变得复杂：订阅 `Edited`/`Deleted` 事件会扩大影响范围，但会增加复杂性。
- **预算锚定**：当前的成本预算系统（`CostBudget` + `KeyedCostBudget`，按种类加权）使用估算值，而 ledger（提议的）记录*实际*成本。这两者可能会产生分歧，从而导致预算过高或过低。需要一个校准过程。

**预期的架构变更。**
```
New modules:
  aero-ai/router.rs:       DifficultyEstimator → model_selector → fallback chain
  aero-ai/cache.rs:        SemanticCache (Redis + pgvector for similarity >0.95)
  aero-ai/ledger.rs:       UsageLedger (tenant_id, job_kind, real_cost_micros, ts)
```
- 模型选择：`Difficulty::Easy → haiku`, `Medium → sonnet`, `Hard → opus`。回退链：如果 opus 超出限额，则退回 sonnet；如果 sonnet 也超出限额，则提供一个默认答案，而不是拒绝。
- 缓存：使用 Voyage 嵌入的归一化查询 → 在 Redis 中查找 `similarity > 0.95` → 命中则直接返回。对于未命中的情况，运行 RAG → 缓存答案。每个工作区的 TTL（例如 24 小时）。
- Ledger：每个任务完成时的追加插入。REST API：`GET /api/billing/usage?workspace_id=&from=&to=`。

**对现有系统的影响。** 中等。现有的 `AnswerQuestion` 路径（`service_impl.rs`）需要重构以使用路由层，而不是直接调用 `anthropic::complete`。该 ledeger 可以透明地添加到现有任务中，无需重复工作。缓存是一个纯新增功能。

---

### 方向 4：数据层韧性（读副本 + 分区 + 热键分片）（P1 — 规模就绪）

**为什么需要它。** 第一道扩展瓶颈将在 5-20k 并发处：单个 PG 主库达到连接池限制，`messages` 表膨胀到全表扫描变得昂贵的程度，并且 presence/viewer 热键在涌入期间会变为串行化。读副本、自动分区和热键分片都是已知的解决方案，但每个都需要架构上的刻意努力来实施。

**核心挑战。**
- **读已写一致性**：副本路由必须确保用户*写入*的消息在从副本*读取*时立即可见。解决方案：写操作后有一个短暂的“主粘性”窗口，或者关键路径强制使用主库（例如，`send_message` 在完成前不需要从副本读取）。
- **分区切换的零停机时间**：影子双写 + 追赶 + 交换需要缓慢的转换（例如，7 天的双写窗口）和回滚能力。`AGENTS.md` 正确禁止在活跃的重构之上叠加分区。
- **热键分片计数一致性**：256 个分片中的 presence/viewer 计数必须聚合。分片可以容忍最终一致性（一个过期条目在几秒钟内被注销是可以接受的），但交叉分片驱逐必须按预定计划进行。

**预期的架构变更。**
```
New modules:
  storage/query_router.rs:   Thin wrapper over two PgPool instances
  storage/partition.rs:      Automated range partitioning by created_at month
  storage/presence_shard.rs: 256-shard presence by user_id % 256
```
- `QueryRouter`：作为枚举的简单 `enum Target { Primary, Replica }`，以及一个通过 `Option<replica_url>` 处理单个副本的薄包装。所有只读查询（消息、回执、反应、搜索）都去往副本。关键路径强制要求主库。
- `messages_partitioned`：按 `created_at` 进行范围分区。影子双写通过配置进行（`AERO_MESSAGE_DUAL_WRITE=on`）。零停机切换：使用 `ALTER TABLE ... RENAME TO` + RENAME。
- `presence:room:{id}:shard:{uid % 256}`：使用 `SMEMBERS` 的 N 读取，而不是对整个集合进行 `SMEMBERS`。聚合：256 次读取（可行，因为 reading/presence 可以容忍最终一致性）。

**对现有系统的影响。** 高。`storage/` 中的几乎每个查询都需要进行审计，以确定它应该去往主库还是副本。分区需要双写和追赶逻辑。分片需要对 presence 读取路径进行大量更改。

---

### 方向 5：企业合规纵深（数据驻留 + 审计不可变性 + KMS 加密）（P2 — 市场拓展）

**为什么需要它。** 如果没有数据驻留、不可变审计和 KMS 加密，高端企业交易（FedRAMP、SOC2、ISO27001 买家）是无法达成的。这些买家需要*证明*数据不会离开其区域、审计轨迹无法被篡改，以及静态数据已使用其密钥进行加密。每项都是阻止交易的阻碍因素。

**核心挑战。**
- **多区域 blob 路由**：`BlobStore` 必须根据工作区的 `region_code` 选择区域 S3 端点（或自定义端点）。这需要 `BlobStore` 成为一个由 `Vec<Arc<dyn BlobBackend>>` 支持的工厂，而不是单一的后端。
- **审计不可变性与 GDPR 删除之间的矛盾**：被删除用户的审计轨迹必须保留（不可变），但个人身份信息（PII）必须被删除。解决方法是留下去标识的记录：“USER_X 在 T 时间执行了操作 Y”，不带 PII。
- **密钥轮换与历史 blob**：信封加密（每个工作区的数据加密密钥（DEK），由密钥管理服务（KMS）加密）。轮换密钥后，在读取时使用旧 DEK 解密旧 blob。密钥版本存储在 blob 元数据中（或作为信封的一部分）。

**预期的架构变更。**
```
New modules:
  storage/kms.rs:         EnvelopeEncryptionLayer (per-workspace DEK, KMS wrap/unwrap)
  storage/data_region.rs: RegionRouter (region_code → S3 endpoint)
  storage/audit_immutable.rs: Append-only audit sink (sign + seal)
```
- 数据驻留：在工作区表上添加 `region_code` 列。使用一个 `HashMap<RegionCode, Box<dyn BlobBackend>>` 来路由 blob I/O。
- KMS：在每个 blob PUT 上 `seal()`，在每个 GET 上 `unseal()`。密钥轮换策略：每 90 天轮换一次。历史 blob 使用旧 DEK 版本。
- 审计不可变性：将 `audit_events` 中的 `deleted_at` 替换为追加写入（通过 `NOT FOR REPLICATION` 或类似的 PG 机制锁定，或者定期签名+密封导出）。

**对现有系统的影响。** 非常高。现有 `BlobStore` trait 的全部接口都需要一个加密步骤。审计代码库需要一次重大的重新架构。建议先处理方向 4（分区），然后再进行这项，以避免同时进行数据层更改。

---

### 方向 6：模式治理 + 版本化 API 契约（P1 — 长期可持续性）

**为什么需要它。** 没有模式注册表，WebSocket 帧和 webhook 有效载荷都是无版本的。这意味着对 `RoomEvent` 的破坏性变更会同时破坏所有 webhook 消费者和 Web 客户端。模式注册表（`GET /api/dev/schemas` + CI 差异检查）提供：
1.  针对新模式变更的自动化兼容性检查。
2.  关于接收哪种模式的版本协商（通过 `Accept` 标头或 `webhook_subscriptions.accept_version`）。
3.  面向开发者的文档（“这是 `RoomEvent` 在 v1 中的样子”）。

**核心挑战。**
- **枚举变体演化**：Rust 的 serde 无法内省地生成 `tag="kind"` 联合体的 JSON 模式。`schemars` 对于简单结构良好，但对于像 `RoomEvent` 这样高度修饰的结构来说，输出可能很差。解决方法：第一阶段手动编写模式 + CI 差异检查，第二阶段逐步实现自动化。
- **向后兼容性差异**：哪些变更被认为是“破坏性的”？该模式必须定义一个兼容性策略（例如，添加字段 = 非破坏性，删除字段 = 破坏性，重命名字段 = 破坏性）。
- **Consumer 声明**：webhook 订阅必须声明它们支持的版本。`webhook_dispatcher` 构建有效载荷时必须遵守 `accept_version`。

**预期的架构变更。**
```
New directory:  schemas/room_event/v1.json, schemas/stream_event/v1.json, ...
New module:     server/schema_registry.rs (GET /api/dev/schemas)
New table:      webhook_subscriptions.accept_version TEXT DEFAULT 'latest'
```
- CI 检查：提交钩子（或工作流步骤）检查 `schemas/` 目录是否与 `RoomEvent`/`StreamEvent` 枚举中的当前变体数量一致。一个简单的启发式方法：计算 `#[serde(tag = "kind")]` 变体，并与模式数量进行比较。

**对现有系统的影响。** 低到中等。模式注册表本身是一个纯新增功能，不会影响任何现有系统。版本协商需要修改 webhook 调度程序以检查 `accept_version`，但这可以逐步完成。

---

## 3. 接口设计建议

### 原则

**1. Trait 优先的抽象，而非具体类型。**
`current` 对具体类型（`ImService`、`XRepo`）的使用效果很好，因为每个 crate 都负责自己的领域。但是，跨领域边界（例如，`aero-server` 调用 `aero-im-core` 和 `aero-live-core`）将从 `trait` 边界中受益——不仅仅是为了测试，而是为了保持间接依赖。建议：

```
// 而非依赖具体的 ImService：
pub fn routes(im: ImService) → Router

// 使用一个 trait（在 aero-server 中定义，在 aero-im-core 中实现）：
pub trait ImApi: Send + Sync {
    fn send_message(&self, …) → impl Future<Output=Result<…>>;
    fn publish_room_event(&self, …) → impl Future<Output=Result<…>>;
}
```

这种变化将使 `aero-server` 在测试期间能够注入模拟实现，并防止在 crate 边界的接口实际发生变化时出现编译错误。

**2. 调用者显式指定一致性。**
不是所有查询都去往主库或副本，而是将一致性要求作为查询签名的一部分进行编码：

```rust
pub enum Consistency { Eventual, ReadYourWrites, Strong }

impl MessageRepo {
    pub fn list_since(&self, room_id, since, consistency: Consistency) → …
}
```

实现：读路径检查一致性，并相应地在主库或副本上执行查询。这种显式性避免了关于“这个查询是否足够安全，可以走副本”的隐式约定。

**3. 后台任务的生命周期是配置的，而不是代码的。**
目前，后台任务（bot、计时器）是在 `bin/boot/` 中使用硬编码的 `tokio::spawn` 启动的。随着系统的发展，这些任务应该是：

- **可热插拔的**：任务应该有一个生命周期（启动、关闭、健康检查）。
- **可重新启动的**：任务崩溃时，应该重新启动（带有退避）。
- **可配置的**：哪些任务在特定运行中处于活动状态，应该由配置驱动（不仅仅是环境门控）。

建议：一个 `BackgroundTask` trait：

```rust
#[async_trait]
pub trait BackgroundTask: Send + 'static {
    fn name(&self) -> &'static str;
    async fn run(self: Box<Self>, shutdown: CancellationToken) -> Result<(), Error>;
    async fn health_check(&self) -> HealthStatus;
}
```

以及一个 `TaskRunner`，它注册任务，在启动时运行它们，并在任务失败时重新启动它们。这为当前的临时 `tokio::spawn` 列表增加了一个编排层。

---

### 新的抽象层

**1. `CacheBackend<K, V>` trait。**
一个统一的缓存接口，提供：

```rust
#[async_trait]
pub trait CacheBackend<K: CacheKey, V: CacheValue>: Send + Sync {
    async fn get(&self, key: &K) -> Option<V>;
    async fn set(&self, key: &K, value: V, ttl: Duration) -> Result<(), Error>;
    async fn invalidate(&self, key: &K) -> Result<(), Error>;
    async fn invalidate_pattern(&self, pattern: &str) -> Result<(), Error>;
    fn key_namespace(&self) -> &'static str;
}
```

这为缓存失效总线提供了一个单一的集成点：`invalidate()` 在无效化本地条目之前发布 `cache.invalidate.{key}`。

**2. `QueryRouter` 用于数据库访问。**
一个薄包装器，根据一致性要求将查询路由到主库或副本：

```rust
pub struct QueryRouter {
    primary: PgPool,
    replica: Option<PgPool>,
}

impl QueryRouter {
    fn pool_for(&self, consistency: Consistency) -> &PgPool { … }
}
```

所有仓库（`MessageRepo`、`ParticipantRepo` 等）都接收一个 `QueryRouter` 引用，而不是一个 `PgPool`。

**3. `SchemaRegistry` 用于序列化契约。**
一个内存注册表，其中包含一个 `HashMap<SemVer, JsonSchema>`，以及一个获取当前模式的 API：

```rust
#[derive(Serialize)]
pub struct SchemaEntry {
    pub name: &'static str,          // "room_event"
    pub version: SemVer,             // v1.0.0
    pub schema: serde_json::Value,   // the JSON Schema
}
```

REST 端点：`GET /api/dev/schemas` 和 `GET /api/dev/schemas/:name/:version`。

---

## 4. 技术选型

### 保持：当前栈

| 技术 | 原因 |
|--------|--------|
| **NATS JetStream** | 它完美地满足了“跨实例事实来源”的要求。与 Kafka（更重的操作）或 RabbitMQ（更弱的流语义）不同，NATS 很轻量，为 Rust 原生，并且与事件驱动架构完美匹配。保持。 |
| **Postgres + pgvector** | 现有的嵌入实现（Voyage 非对称 + pgvector 的 HNSW）是可靠的。不应迁移到专门的向量数据库——pgvector 提供了足够的召回率（该文档引用了高相似度的 RRF 融合），并且避免了需要第二个数据存储的操作负担。保持。 |
| **Redis（相对 fred）** | 用于集群状态（presence、rate limiter、call rosters）的 sorted-set 模式是经过验证的。单个热键问题（方向 4）通过分片解决，而不是通过替换 Redis。保持。 |
| **Axum 0.7** | 这是 Rust 中开发体验最好的 HTTP 框架。它的提取器模式（`AuthUser`、`AppState`）与当前的路由模式完美契合。没有理由迁移到 actix-web 或 warp。保持。 |
| **str0m** | 纯 Rust WebRTC（DTLS-SRTP）是 Aero 的一个差异化因素。使用 webrtc-rs（C 绑定）会引入不安全的 FFI 协调问题。保持。 |

### 引入：有条件的

| 技术 | 条件 | 原因 |
|----------|-------------|--------|
| **schemars** | 仅适用于简单结构，不适用于 `tag="kind"` 联合体 | 用于 API 响应模式（方向 6），但对于事件联合体，它需要手写的模式文档。保持务实的态度。 |
| **OTEL OTLP 导出器** | 如果 Jaeger/Grafana Tempo 已经部署 | 已经有 OpenTelemetry 集成；OTLP 导出器应该是从 `AERO_TRACE_EXPORTER` 配置的（不仅仅是 stdout）。考虑使用 `opentelemetry-otlp`。 |
| **Tonic/gRPC** | 仅适用于内部/集群 RPC | gRPC 对于跨节点 RPC（例如，call-bridge 信令）来说不是必需的——NATS 请求-回复模式覆盖了这一点。但是，如果 Aero IM 在未来需要面向外部的 gRPC API（用于机器人），那么 Tonic 是一个不错的选择。（收益递减：低优先级。）|

### 不引入

| 技术 | 原因 |
|-----------|--------|
| **专门的向量数据库（Pinecone/Milvus/Weaviate）** | 运营负担，并且对于 Aero 的规模（每个工作区数十万条消息，而不是数十亿）来说，没有必要的额外召回率。pgvector 的 HNSW 就足够了。 |
| **消息队列（Kafka/Pulsar）** | NATS JetStream 在 Aero 的规模下提供了更好的延迟和更简单的操作。迁移到 Kafka 只会在 Aero 每天发送数百万条消息时才有意义。尚未达到那个规模。 |
| **Kubernetes Operator** | Docker Compose 对于单个部署来说已经足够了。Kubernetes 增加了编排开销，却没有带来明显的收益（Aero 不是一个每天有数千次部署的微服务平台）。 |
| **GraphQL** | 现有的 REST + WS 架构很好地服务于客户端。GraphQL 会引入解析器组合、N+1 和缓存方面的复杂性，而不会有太多收益。方向 5 中的 `?fields=` 投影在没有完整 GraphQL 层的情况下解决了 80% 的负载问题。 |

### 自建决策框架

对于方向 1（缓存失效总线）、方向 2（交付游标）和方向 3（AI 分层），“自建”的理由很明确：这些是 Aero 领域的核心业务逻辑，并且涉及紧密集成，而现成的产品无法提供。

对于方向 5（KMS 加密），值得考虑使用 **HashiCorp Vault** 作为 KMS 后端，而不是从头开始构建 DEK 管理。Vault 提供了密钥轮换、审计日志和 HSM 支持，而 Aero 团队不需要自己构建。API 兼容层（`KmsBackend` trait）将允许 Vault 或 AWS KMS 作为可插拔的后端。

对于方向 4（数据分区），Postgres 的声明式分区内置于 PG 17 中。无需外部依赖——只需规划分区方案和迁移回滚策略。

---

## 5. 实施路线图

### 优先级排序

根据上一轮跨代码库扫描的输入，以及我对架构缺口的分析，修订后的优先级排序为：

| 方向 | 优先级 | 依据 |
|-----------|----------|---------|
| 方向 1：统一失效总线 | **P1** | 跨节点安全（授权缓存）是一个被确认的薄弱环节，但可以通过 60 秒的 TTL 安全地缓解。*先做缓存抽象，然后再做失效总线。* |
| 方向 2：持久交付 | **P1** | 这是 IM 的核心承诺。该缺口在架构级别影响 UX（重连 O(N)）和可靠性（故障恢复）和成本（多设备背填）。 |
| 方向 3：AI 成本分层 | **P1** | 如果不进行分层，每单位的 AI 成本就是 Sonnet 满价，这在经济上是不可持续的。*最早的直接效益：分层路由可以独立实现。* |
| 方向 4：数据层韧性 | **P1** | 第一个扩展瓶颈出现在 5-20k 并发处。该代码库需要提前准备好这些模式，而不是等到那一天才进行。 |
| 方向 6：模式治理 | **P1** | 在第一批上游 bot 启动*之前*就位，以防止未来的重大变更成为事故。模式注册表单元（手动编写模式 + CI 差异检查）的工程量很小。 |
| 方向 5：企业合规 | **P2** | 受监管的市场是高价值的，但依赖于基础的数据层韧性（方向 4）和分片就位。在 P1 交付之前，不要开始 P2。 |

### 阶段划分

**阶段 0（2 周）：基础 + 首先获得最高收益。**

| 周 | 内容 | 依赖项 | 风险 |
|-----|----------|----------|------|
| 1 | 方向 6 阶段 A：手动编写 `schemas/room_event/v1.json` + CI 差异检查。在 `webhook_subscriptions` 上添加 `accept_version`。 | 无 | 低（仅新增） |
| 1 | 方向 3 阶段 A：难度估计 + 模型分层路由（`DifficultyEstimator` → `model_selector`）。启用语义缓存（提取-问题-嵌入 → Redis 哈希）。 | `aero-ai/` | 中等（现有的 `AnswerQuestion` 路径必须重构） |
| 2 | 方向 4 阶段 A：`QueryRouter` — 为主库/副本路由提供薄包装器。将所有只读查询路由到副本。 | 无（假设主库/副本 URL） | 低（如果应用了复制滞后检查，则无破坏性变更） |

**阶段 1（4 周）：核心 IM 可靠性和 AI 经济性。**

| 周 | 内容 | 依赖项 | 风险 |
|-----|----------|----------|------|
| 3 | 方向 2 阶段 A：`delivery_cursors` 表 + 仓库。WS `ack_delivery` 帧。背压：`backpressure_for_rooms` 帧。 | 无 | 高（协议更改；Web SPA 更新；向后兼容） |
| 4 | 方向 2 阶段 B：重连 `?since=<seq>`（从 O(N) 变为 O(log N)）。多设备游标共享。 | 方向 2 阶段 A | 高（现有背填路径仍然存在；过渡逻辑） |
| 5 | 方向 3 阶段 B：`UsageLedger` — 真实成本核算。`GET /api/billing/usage`。阈值的成本告警。 | 方向 3 阶段 A | 低（纯新增功能） |
| 6 | 方向 1 阶段 A：`CacheBackend<K, V>` 特性。统一 `ParticipantCache` 和 `RoomMemberCache` 以使用它。 | 无 | 中等（重构现有缓存；现有的 TTL 失效仍然有效） |

**阶段 2（3 周）：可扩展性 + 模式自动化。**

| 周 | 内容 | 依赖项 | 风险 |
|-----|----------|----------|------|
| 7 | 方向 4 阶段 B：`messages_partitioned` — 影子双写 + 追赶 + 零停机切换。 | 方向 4 阶段 A（QueryRouter） | 高（分区双写在 7 天内是可逆的，但追赶具有数据依赖风险） |
| 8 | 方向 4 阶段 C：Presence/viewer 热键分片（256 个分片）。 | 方向 4 阶段 A | 中等（分片读取可以容忍最终一致性） |
| 9 | 方向 1 阶段 B：NATS 失效总线（`cache.invalidate.{key}` 耐久消费者）。用活跃失效替换 TTL 失效。 | 方向 1 阶段 A | 中等（NATS subject + 消费者；TTL 仍然作为兜底） |
| 9 | 方向 6 阶段 B：使用 `schemars` 实现自动化模式生成（如果支持 `tag="kind"` 则逐步采用）。 | 方向 6 阶段 A | 中等（schemars 输出可能需要手动调整） |

**阶段 3（3 周）：BFF + 企业基础。**

| 周 | 内容 | 依赖项 | 风险 |
|-----|----------|----------|------|
| 10 | 方向 5 阶段 A：`region_code` + `RegionRouter`（基于工作区区域的 blob 路由）。 | 方向 4 阶段 A（QueryRouter 已经在数据路径中） | 中等（blob 路由；现有的 `BlobStore` trait 必须被包装） |
| 11 | 方向 5 阶段 B：用于 blob 的 KMS 信封加密（`EnvelopeEncryptionLayer`）。 | 方向 5 阶段 A | 高（所有 blob I/O 路径现在都需要加密/解密；密钥轮换策略） |
| 12 | 方向 5 阶段 C：审计不可变性（追加写入 sink、签名 + 密封、从 swee 中豁免 `audit_events`）。 | 方向 4 阶段 B（分区） | 高（审计代码库重构；与 GDPR 删除的交互） |

### 风险与缓解策略

| 风险 | 可能性 | 影响 | 缓解 |
|------|----------|--------|----------|
| 交付游标（方向 2）协议更改破坏了现有的 web/移动 SPA。 | 高（除非有适配层，否则协议更改是破坏性的） | 关键（所有已部署的客户端都会中断） | 首先将背填协议保留为旧版（当客户端不发送 `ack_delivery` 时回退到 `list_since`）。游标驱动路径是仅新增的。在过渡期间，现有客户端继续使用旧协议。 |
| 分区双写（方向 4）引入了未检测到的不一致。 | 中（双写逻辑错误） | 高（消息丢失或重复） | 双写期间的影子比较——将双写新表的结果与旧表的结果进行比较，并在不一致时发出警报。在切换之前，在一次性测试数据库上运行迁移烟雾检查。 |
| AI 分层（方向 3）错误地将困难问题分类为简单问题，并返回了糟糕的答案。 | 中（难度启发式方法可能会失败） | 中等（用户信任度下降） | 分层路由必须包含一个“逃生舱口”——如果 Haiku 答案的置信度得分较低，则重新提交给 Sonnet 或 Opus。默认情况下启用车速限制（简单查询 → 最快模型），并带有可选的“准确模式”覆盖。 |
| 读副本（方向 4）在压力下落后于主副本。 | 低（PG 流式复制是健壮的） | 中等（用户在写入消息后可能会看到过时的视图） | 写操作后短暂的主库粘性窗口（例如，500 毫秒）。标有 `READ_YOUR_WRITES` 的查询强制使用主库。 |
| KMS 加密（方向 5）将 blob I/O 延迟增加了不可接受的程度。 | 中（信封加密涉及 KMS 网络往返，至少对于初始 DEK 获取而言） | 中等（用户体验下降） | 在内存中缓存 DEK（TTL 为 1 小时）。批量 blob I/O 共享相同的 DEK 以减少 KMS 往返。使用 XChaCha20-Poly1305（在 Rust 中非常快，受 `chacha20poly1305` crate 支持）而不是 AES-GCM 进行信封加密。 |

---

## 总结

Aero IM 的架构在核心事件驱动设计方面很稳健，但存在三个根本性缺口，阻碍了其从“功能丰富的演示”发展到“可运营的产品”：
1.  **没有跨节点缓存一致性**（在授权缓存边界处存在安全漏洞）。
2.  **没有持久交付**（在没有 O(N) 扫描的情况下，重连和多设备无法扩展）。
3.  **AI 成本没有分层**（每个查询支付 Sonnet 的全价，侵蚀了单位经济效益）。

修订后的路线图优先考虑通过模式治理（方向 6）和一致的缓存抽象（方向 1）来建立架构基础，然后解决 IM 可靠性的核心承诺（方向 2），最后通过读副本、分区和热键分片（方向 4）为扩展做好准备。企业合规（方向 5）很重要，但它在路线图的末尾按正确顺序进行。

*最关键的单一建议*是方向 6 阶段 A（手动编写模式 + CI 差异检查）——在第一批上游 bot 启动**之前**就完成它，这样未来的模式变更就不会成为中断所有集成的重大事件。两天的工程工作量，但却规避了极大的未来痛苦。

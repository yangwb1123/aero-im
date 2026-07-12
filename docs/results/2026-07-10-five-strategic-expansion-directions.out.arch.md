# 架构分析报告：Aero IM 战略方向评估

---

## 1. 架构评估

### 1.1 当前架构的优势

**事件驱动骨架（NATS JetStream + Hub）**是系统的核心强点。从`AGENTS.md`可以看出，全系统复用同一模式（业务事件→NATS→本地扇出→WebSocket）降低了认知负荷，且 durable/ephemeral consumer 的针对性选择（IM 事件用 durable、直播事件用 ephemeral）体现了对数据重要性的差异化处理。

**Crate 分层清晰**：自下而上的依赖关系（基础层→IM→直播→组合层）避免了循环依赖，每个 crate 的职责边界明确。特别是将 `aero-common` 定位为纯叶子 crate（无业务逻辑、仅类型/ID/事件/trait），使得上层 crate 的编译依赖稳定。

**智能体（Bot）模式成熟**：标准化的事件消费、显式的幂等守卫（`ON CONFLICT DO NOTHING`、`transcript.is_none()` 等）、fail-open 策略，是经过实战检验的扩展模式——新 bot 只需实现「gate → 业务逻辑 → 副作用 → 错误处理」模式即可。

**限流与可观测性务实**：`KeyedCostBudget` + `CostBudget` 双层限流（per-ws + 全局）、`Semaphore` 并发控制、Prometheus gauge 采样，体现了"系统可以慢、不能死"的防护哲学。

### 1.2 当前架构的局限性

**领域事件散落**：`RoomEvent` 和 `StreamEvent` 两个 enum 正在膨胀（消息、反应、审核、投票、通话等全部挤在一个 tag 里）。随着方向③④⑤的推进，单一事件类型将变得越来越难以管理。当前 `kind` 标签的 `rename` 规避处理（`call_kind`/`notify_kind`）说明已意识到这个问题，但只是打补丁。

**序列化耦合**：如输入文档方向②指出的——`Message` 序列化走 `#[derive(Serialize)]` 大 struct，无法按需投射字段。这在当下不影响功能，但对搜索/导出/事件湖等需要控制响应体大小的场景会逐步成为瓶颈。

**媒体 seam 的状态定义不精确**：`AGENTS.md` §2 明确标注 call-bridge 和 SFU 为「已建+已测，生产未接线」。这是合理的 seam 管理，但"待联调"/"未接线"的边界需要更严格的技术屏障（例如 feature gate 或编译期标记），防止 bin 段无意中触发。

**Bot 装配隐式**：`bin/boot/background.rs` 中逐一手动 spawn 各个 bot/worker/timer。随着 bot 数量增长（当前 7+ 个总线 bot + 5+ 个定时器），装配逻辑会越来越容易遗漏或顺序敏感。缺少声明式注册机制。

**事务边界跨多个资源**：部分操作（如 moderation_bot 的软删+审计+广播）跨 PG + NATS，不是分布式事务。当前靠"先 PG 后 NATS"的朴素质序，在 PG 提交后、NATS 发布前崩溃会丢失事件。已有 `at-least-once` 兜底，但语义不够精确。

### 1.3 架构债务与技术债

| 债务类型 | 位置 | 影响程度 | 建议 |
|---|---|---|---|
| 迁移编译期嵌入 | `aero-storage/db.rs` | 高（新晋开发者易踩坑，§4.2） | 文档已足够；考虑 CI 强制检查 `_sqlx_migrations` 哈希 |
| Bot 装配顺序隐式依赖 | `bin/boot/background.rs` | 中（增加新 bot 时可能遗漏） | 引入 `BotRegistry` 或声明式 `#[bot]` 宏 |
| 单一 `Message` struct 无投影 | `aero-im-core` | 低→中（随方向②推进递增） | 按需引入 DTO |
| 事件 enum 膨胀 | `common/src/model/` | 中（方向③启动前需重构） | 拆分领域事件 |
| 无幂等键的操作 | golive_bot（非幂等） | 低（目前可接受） | 记录但不追 |

---

## 2. 扩展方向

### 方向 A：领域事件平台（重构方向③的事件基础）

**为什么需要**  
方向③（事件湖）和方向⑤（计费 event sourcing）共同要求一个统一的事件 ingestion 层。当前每个 bot 独立拉 NATS subject，事件格式没有统一 schema，也没有存储契约。事件湖不只为了可观测性——它是计费、审计、业务流程回放的基础。

**核心挑战**
- 现存的 `RoomEvent`/`StreamEvent` 是 Rust 编译期类型系统的一部分，事件湖需要 schema 演化能力（向后兼容）
- 需要区分「业务事件」（消息已发）和「系统事件」（消息被审核、TTL 清扫）。当前混在一个 enum
- 历史事件的 schema 迁移策略（300+ 万行消息的数据回填）

**预期架构变更**
```
当前: 多个 durable consumer 各自消费 im.room.*
      ↓
目标: 事件湖消费者 (aero-event-lake) 作为唯一持久化 sink
      |→ 原始 JSON + schema version 存入事件表
      |→ schema-on-read 或 Avro/Protobuf 逐版本演化
      |→ 推送到 ClickHouse / TimescaleDB 用于分析查询
```

**对现有系统的影响**
- 现有 bot 不需要改变——事件湖是**额外** consumer，不是替换
- `RoomEvent` 需要添加 `schema_version` 和 `raw_payload`（当前 serde 丢未知键，需要改为保留）
- 事件湖表需要成为只追加（append-only）表，减少 PG 写放大
- 监控：事件湖延迟、入湖/出湖速率

### 方向 B：声明式 Workflow Engine（方向①的替代方案）

**为什么需要**  
方向①的工作流引擎（定时消息、自动回复、条件触发）与现有的 `scheduled.rs`/`recurring.rs` 功能重叠。需要评估是扩展现有模块还是引入独立引擎。

**核心挑战**
- 状态持久化：工作流实例可能运行数天/数月（定期消息），状态机需支持序列化/反序列化
- 分布式调度：多实例下避免重复执行（`FOR UPDATE SKIP LOCKED` 模式可复用）
- DSL vs 编程语言内嵌：用户可配置 vs 开发者可扩展

**架构选项对比**

| 方案 | 优点 | 缺点 | 推荐度 |
|---|---|---|---|
| **A: 扩展现有 `scheduled.rs`** | 改动小、类型安全、复用既有限流/幂等模式 | 灵活性有限、不支持条件分支 | ⭐⭐（短期可行） |
| **B: 内嵌 lightweight orchestration（状态机 + Actix/tokio 原生）** | 类型安全、与时序引擎集成好、无外部依赖 | 学习曲线、需自研 DSL | ⭐⭐⭐⭐（推荐） |
| **C: 集成 Temporal / Inngest / 类似框架** | 功能完整、分布式事务 | 引入外部系统、运维复杂度增加 | ⭐（对当前平台过重） |

**推荐方案 B**：在 `aero-ai` crate 的预算队列模式基础上泛化——定义一个 `Workflow` trait（类似 `AiWorker` 的 `kind` 变体），状态存 PG（复现 `ai_jobs` 模式），调度复用 `should_flush`/`defer` 模式的 deferred 策略。

### 方向 C：Message Serialization 重构（方向②的深层修复）

**为什么需要**  
`?fields=` 投影只是表象——真正的问题是 `Message` struct 承担了过多职责：持久化、序列化、API 响应、WS 帧负载全部用同一结构。这导致：
- 搜索/导出/列表各需要字段子集，但都传输整个结构
- 增加字段时必须考虑所有下游的向后兼容
- 不能按上下文选择序列化策略（如 event lake 需要 raw JSON，API 需要 typed struct）

**核心挑战**
- 现有 50+ 个字段，散落各处消费，识别每个字段的真实使用者需要全面审计
- 序列化路径有多个（HTTP 响应、WS 帧 `ServerFrame::Message`、事件湖、搜索索引）
- 现有代码大量直接使用 `message.field_name` 模式匹配，引入 DTO 需要改所有消费者

**架构变更建议**

```
当前: Message struct
         ├─ HTTP response (全部字段)
         ├─ WS frame (全部字段)
         ├─ Search index (部分字段)
         └─ Database row (全部字段)
         
目标: Message struct (核心领域，不可变)
         ├─ MessageDto::for_api(fields: &[Field])  → HTTP
         ├─ MessageDto::for_ws()                    → WS 帧
         ├─ MessageDto::for_search()                 → 搜索文档
         └─ 直接 sqlx row                           → 数据库
```

**实施策略**：分阶段进行。第 1 阶段只加 `MessageDto`（新结构），第 2 阶段逐步迁移 API 端点，第 3 阶段废弃 `Message` 上的 `Serialize`。保持向后兼容意味着必须先让新老路径并行。

### 方向 D：计费/Billing 网关（方向⑤）

**为什么需要**  
计费是 SaaS 平台的收入命脉。402 语义在 REST 中正确，但系统挑战在「计费事件捕获与聚合」而非 HTTP 状态码。

**核心挑战**
- 计费事件必须是准确的且恰好一次（付费场景下 at-least-once + 幂等键是必需组合）
- 计费系统不可用时的降级策略（fail-open = 放行？只能接受短期收入损失）
- 用量计费 vs 固定订阅：两种模式的数据模型不同
- 计费事件的数据来源散落在各 bot（push_bot 推送计数、AiWorker 代币消耗、live_bytes 传输量）

**架构建议**

```
计费架构分层:
  采集层: 各 bot/worker 产出 BillingEvent → 写入 billing_events 表
  聚合层: 定时器/触发器聚合 → 用量快照
  判定层: API 网关拦截 → 检查配额 (402/403)
  执行层: Stripe/自建调用 → 限额升降
```

**关键设计决策**
- **是否事件 sourcing 计费**：推荐是——计费表只追加，不更新（`UPDATE billing_usage SET amount = amount + 1` 是并发瓶颈）。用 `INSERT billing_usage_item` + 物化视图聚合
- **fail-open 行为**：计费系统不可用时，管理端操作（创建工作区、修改订阅）**必须 fail-closed**；用户端操作（发消息、推流）**fail-open**（允许但标记）
- **402 覆盖范围**：限制在 REST 管理端点（创建 API、增加存储），不在 WS 路径（消息/弹幕）返回 402——那会破坏实时性

### 方向 E：Bot 装配框架与健康管理

**为什么需要**  
当前 bot 数量 7+（总线） + 5+（定时器） + 2（worker）= 14+ 常驻任务，全部在 `background.rs` 手动 spawn。随着方向①②③的推进，这个数字可能翻倍。需要引入：
1. 声明式注册（如 `#[agent]` 属性宏）
2. 生命周期钩子（`on_start` / `on_stop` / `on_health_check`）
3. 健康监测与自动重启

**核心挑战**
- 每个 bot/worker/timer 的生命周期语义不同（总线 bot 依赖 NATS 连接、worker 依赖 PG、timer 无外部依赖）
- 关闭顺序重要（应先停止消费新事件，再 drain 在途任务）
- 部分 bot 有 OPT-IN env gate，需要在装配时做条件判断

**架构建议**

```rust
// 概念设计（不是代码）
trait Agent: Send + Sync {
    fn name(&self) -> &'static str;
    fn dependencies(&self) -> &[AgentId];  // NATS, PG, Redis, etc.
    fn start(self: Box<Self>, ctx: AgentContext) -> impl Future<Output = Result<()>>;
    fn health(&self) -> impl Future<Output = HealthStatus>;
    fn shutdown(&self) -> impl Future<Output = ()>;
}

// 装配点
AgentRegistry::new()
    .register(agent_bot::Agent)        // 无条件
    .register_opt(transcribe_bot::Agent, "AERO_TRANSCRIBE")  // 条件
    .register(ooo_bot::Agent)
    .start_all(cancel_token).await?;
```

**对现有系统的影响**
- 低——可渐进迁移：新 bot 直接注册，老 bot 保持原样，逐期迁移
- 可复现 `ai_jobs` 的 `SKIP LOCKED` + 超时恢复模式给总线 bot
- 统一 `health` 端点可自动集成到 `/health/ready`

---

## 3. 接口设计原则

### 3.1 领域事件的版本化

**现状问题**：`RoomEvent`/`StreamEvent` 用 serde `deny_unknown_fields` 或 `deny_unknown_fields` 缺失。方向③的事件湖需要 schema 演化的前提是消费者不因未知字段而崩溃。

**建议方案**：
```rust
// 添加 schema version 到每个事件
struct RoomEvent {
    // 新增
    schema_version: u32,       // 默认为 1，事件湖消费时记录
    // 现有字段不变
}
```

所有新消费者须按 `schema_version` 路由反序列化逻辑；所有生产者首次发布时在原始 JSON 里注入 `schema_version`。

### 3.2 序列化策略接口

**方向②的根本解决**：定义序列化策略 trait，而不是在每个调用处做字段白名单。

```rust
/// 按上下文定义投射规则
trait MessageProjection {
    fn fields(&self) -> &'static [&'static str];
    fn as_dto<'a>(&self, msg: &'a Message) -> Value;
}
```

三种内建策略：`FullProjection`（WS/详情）、`ListProjection`（列表/搜索）、`ExplicitProjection(Vec<String>)`（?fields=）。

**为什么不用手写 Serializer**：权重过高。项目当前有 50+ 字段、20+ API 端点，手写 Serializer 的维护成本 > 定义 3 个 DTO 的策略模式成本。

### 3.3 Bot 插件接口

为方向①⑤做准备，Bot 定义接口需要纳入：

- **事件过滤器**：`fn interested(&self, event: &RawBusEvent) -> bool`——当前所有 bot 是"匹配 subject 就全部收"，需要按事件类型预过滤
- **幂等键提取**：`fn idempotency_key(&self, event: &RawBusEvent) -> Option<String>`——替代各 bot 自建的幂等守卫
- **限流分类**：`fn budget_key(&self) -> &'static str`——让框架层统一执行 per-bot 限流

### 3.4 向后兼容策略

- **新字段总 optional**：API 响应和 WS 帧中，新字段用 `Option` 或 `#[serde(skip_serializing_if = "Option::is_none")]`
- **老客户端不崩溃**：web SPA 端的 `event.call_kind || event.kind` 兜底模式是正确的，所有新事件字段须在 web 端同样实现回退
- **数据库迁移**：`ALTER TABLE ... ADD COLUMN IF NOT EXISTS`（sqlx 迁移已如此，继续保持）
- **方向③的事件湖**：不建议回填历史数据（避免 300 万行迁移），schema-on-read 容忍历史记录的缺失字段

---

## 4. 技术选型

### 4.1 评估新依赖的标准

| 标准 | 权重 | 说明 |
|---|---|---|
| 纯 Rust / first-class binding | 高 | 优先选 Rust 原生库（如 reqwest、rimecraft/valkey-rs），避免 C 绑定（如 librdkafka） |
| Tokio 生态兼容 | 高 | 必须 tokio-compatible（async + Send + Sync + Clone），与既有 async-nats/fred 一致 |
| MSRV 不高于 1.80 | 中 | 当前 MSRV 1.80，新依赖的 MSRV 不应更高 |
| 无太多传递依赖 | 中 | 避免引入 100+ 个新 crate（如引入 Kafka 客户端会大量增重） |
| 维护活跃度 | 低 | 项目当前依赖中有长期未更新的库（rml_rtmp），只要关键功能稳定可接受 |

### 4.2 各方向的技术选型分析

**方向③事件湖存储**：

| 方案 | 优点 | 缺点 | 评估 |
|---|---|---|---|
| **ClickHouse** | 列存、高压缩率、实时聚合 | 新基础设施、运维复杂 | 长期方向 P1 |
| **PG + 分区表 + JSONB** | 复用现有 PG、零新组件 | 扫描性能差、横向扩展受限 | 短期过渡 |
| **Redis Streams** | 低延迟、复用现有 Redis | 容量有限、无复杂查询 | 不推荐（事件湖需要持久化查询） |

**推荐**：先 PG JSONB（`events` 表分区），有用户量后再评估 ClickHouse。NATS JetStream 本身不是存储——它提供 at-least-once 交付但不提供历史查询。

**方向⑤计费网关**：

| 方案 | 优点 | 缺点 |
|---|---|---|
| **Stripe** | 成熟、合规、免 PCI | 外部依赖、PII 出境的合规审查 |
| **Lemon Squeezy** | Stripe 替代、更友好的开发者体验 | 覆盖率不如 Stripe |
| **自建（metering + 限额）** | 完全控制、适合 SaaS 场景 | 需自研支付集成、合规投入大 |

**推荐**：用量聚合自建（Postgres + Redis sorted-set），支付对接 Stripe（用其 metering API），402 判定层在 `aero-server` 实现。自建部分可控、支付部分外包合规。

**方向①工作流引擎**：

| 方案 | 优点 | 缺点 |
|---|---|---|
| **自研（基于既有 `scheduled.rs`）** | 无新增依赖、类型安全 | 灵活性不如框架 |
| **Temporal** | 生产级工作流引擎 | SDK 无 Rust 正式版、架构过重 |
| **tokio + 状态机宏** | 轻量、贴合 tokio 生态 | 需要自研 DSL 和持久化 |

**推荐**：tokio + 状态机宏（方案 B）。项目已有 `AiWorker` 的状态机模式（`Pending→Claimed→Processing→Done/Dead`），只需泛化为通用工作流 trait。不要引入 Temporal——它对当前平台来说是一台屠宰牛刀。

### 4.3 自建 vs 采购决策矩阵

| 能力 | 推荐 | 理由 |
|---|---|---|
| 事件湖存储 | 自建（PG → ClickHouse） | 与现有架构贴合，无合适的外部 SaaS |
| 计费支付 | 采购 Stripe | 合规需求、已有成熟 SDK |
| 工作流引擎 | 自建（基于现有模式） | 需求简单、自建开销远低于外部依赖 |
| 日志/可观测 | 采购 Grafana Cloud / Datadog | 不要自己写日志聚合 |
| 消息推送 FCM/APNs | 自建 | 已实现、是业务逻辑的一部分 |

---

## 5. 实施路线图

### P0（必须做，阻碍其他方向）

| 项目 | 工作量估计 | 风险 |
|---|---|---|
| **事件 schema 版本化** | `RoomEvent`/`StreamEvent` 添加 `schema_version` 字段 | 低，但不做则事件湖和计费的数据语义不可靠 |
| **Message DTO 策略接口** | 定义 MessageProjection trait + 3 种内建策略，逐步迁移 API | 中，改 20+ 端点，但可以边用边改 |
| **Bot 注册框架（v1）** | `AgentRegistry` + 生命周期钩子 + 健康检查 | 中，但增益高；可以渐进迁移 |

**里程碑 M1 （第 1-2 周）**：事件 schema 版本化上线，`Message` 新增 `MessageDto::for_api` 方法。

### P1（高价值，下一阶段）

| 项目 | 工作量估计 | 依赖 |
|---|---|---|
| **事件湖消费者（PG JSONB）** | 基于 `run_bus_listener` 模式，加 `aero-event-lake` durable consumer | P0 的事件 schema 版本化 |
| **计费事件采集层** | `BillingEvent` 定义 + bot/worker 埋点 + 聚合表 | 事件湖的 schema 一致性 |
| **402 判定层中间件** | 配额检查 middleware + Redis counter | 计费事件采集完成 |
| **工作流引擎 v1（定时消息）** | 基于 `scheduled.rs` 的状态机泛化 | P0 的 Bot 注册框架 |

**里程碑 M2 （第 3-6 周）**：事件湖上线，计费事件采集完成，`POST /api/messages` 显示余额。

### P2（有前置条件后推进）

| 项目 | 工作量估计 | 前置条件 |
|---|---|---|
| **工作流引擎 v2（条件分支）** | DSL 设计 + 状态机扩展 | P1 的 v1 上线 |
| **事件湖迁移 ClickHouse** | 数据迁移 + 双写 | 事件湖稳定运行 1 个月 |
| **计费聚合物化视图** | 按工作区/按时间聚合 | 计费事件采集量足够 |
| **Bot 装配框架 v2（自动重启）** | 崩溃检测 + backoff + 告警 | P0 的注册框架 |

### 风险矩阵

| 风险 | 可能性 | 影响 | 缓解 |
|---|---|---|---|
| 事件 schema 遗漏版本化 | 中 | 高（事件湖数据不可对齐） | CI 强制新 variant 带 `schema_version` |
| 计费 fail-open 导致收入损失 | 低 | 中 | 仅用户端 fail-open，管理端 fail-closed |
| 工作流状态机过于复杂 | 中 | 低 | v1 只支持线性序列（定时消息），v2 才加条件分支 |
| 事件湖 PG 写入压力 | 低（初期） | 高（如果预判错误） | 先分区表，延期 ClickHouse 迁移 |
| 方向② DTO 重构引入回归 | 中 | 中 | 增量迁移 + `#[cfg(test)]` 新旧序列化对比测试 |

---

## 总结

当前的 Aero IM 架构是经过实战推敲的——事件驱动骨架、bot 模式、限流/预算控制体系都是正确的设计决策。五个扩展方向中，**P0 的三项（事件版本化、Message DTO、Bot 注册框架）是架构基础设施**，不直接产生业务价值但解除后续方向的阻塞。

**最大架构风险不是技术债务，而是一次推进过多方向**：方向①②③⑤在某种程度上都依赖事件 schema 版本化和序列化重构。建议严格执行「P0 → P1 → P2」节奏，P0 完成前不同步启动 P1 方向的编码工作。

**架构审查周期**：每完成一个里程碑，由不同 agent 做跨目录回归审查（`AGENTS.md` §4.5 的 seam 检查 + `truth-check.sh` 死代码检测 + file-size-check.sh 尺寸守卫），确保重构不引入回归。

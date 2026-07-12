# 架构分析：Aero IM 系统性断层与扩展路径

> 基于源码扫描（16 crates / 157 migrations / ~25K Rust / ~5.9K Web SPA）、git diff 当前在途更改（36 文件，±1,865 行）、以及 docs/requirements/ 多轮分析文档的综合交叉验证。
>
> **方法**：不重复已有分析中已经充分论证的方向，聚焦于「多条分析线共同标记的断层」以及「分析线与当前 git 更改的交叉点」。

---

## 一、架构评估

### 1.1 核心优势

Aero IM 的架构质量在同规模 Rust 项目中属于第一梯队，主要体现在以下三点：

**事件 DAG 的层次清晰度**：业务层 → NATS subject → 进程内 Hub → WebSocket 的路径是单向、无环的。`RoomEvent` / `StreamEvent` 两个枚举在两个独立 subject 空间中各自有单调 seq，这种结构使得「水平扩展 = 加实例」成立的代价极低——不需要分布式共识、不需要协调者、不需要再平衡。这是从单体 IM 走向集群 IM 最经济的路径，157 次迁移中没有一次破坏过这个模型。

**Fail-open/fail-close 的显式选择**：每个模块在面对外部依赖故障时的行为都是显式选择的，不是无心结果。SPAM guard 对 Redis 错误 `Allow`（fail-open）、call bridge 不可达时 `None` 全休眠、blob store 的 delete-then-ack——这些决策写死在代码里，而不是退化为 panic 或无限重试。AGENTS.md §4.2 的「at-least-once 状态机」规则已经写进工程的 DNA 了。

**媒体面与信令面的解耦**：SFU 的 `SfuRouter` 用 `Arc<RwLock<HashMap>>`（进程内，非 Redis）、call roster 用 Redis sorted-set（集群级）——两个名册系统被刻意隔离，避免「一个系统的拓扑泄漏到另一个」。这种隔离让通话编排（`CallOrchestrator`）和媒体转发（`SfuForwarder`）可以独立 scale。

### 1.2 结构性债务

债务不在代码质量层面（CI 有 clippy、truth-check、file-size-check 三道门），而是**架构层面的三个倾斜**：

**债务一：Web SPA 的「调试客户端」定位与后端成熟度之间的剪刀差**。后端已经 P0–P11 全部就位，Web 端 `index.html:26` 自称为「debug client · 联调专用」。这导致前端缺失从企业治理（SCIM/2FA/法务保全）到创作者经济（礼物/订阅榜/hype train/predictions）到交互式组件（Button/Select 无 click handler）的全部 UI 层。每个方向在 ROADMAP 上的估算都要乘以一个「前端先补基础架构还是先补这个方向」的选择因子。

**债务二：PG 连接池作为单体容量瓶颈**。消息发送、AI 作业、搜索、认证、审计、清扫——全部通过一个共享 `PgPool`，且 AI Worker 默认 50 并发 & 消息发送每条消息涉及 `insert` → `members` 查询 → `publish_room_event` → `dispatch_notifications` → `enqueue × 2`。这不是代码质量问题，而是容量规划的盲区——每个新 crate 或新定时器都从这个池子里取连接，但没有任何机制保证核心路径（消息发送）在压力下不被非核心路径（清扫/AI 作业）挤出。

**债务三：限流器不感知后端容量**。三层限流（per-client token bucket / per-workspace tier / per-sender spam window）全部用静态启动时配置。`metrics_tasks.rs` 已有的 DB 池水位 gauge 只写 Prometheus 但不反馈回准入决策。这形成一个信息回路断裂——系统有容量信号（PG 连接池水位、NATS backlog、AI 延迟）但没有执行机构。

### 1.3 关键设计决策评估

| 决策 | 评价 | 建议 |
|------|------|------|
| 事件总线用 NATS JetStream durable consumer | ✅ 正确。at-least-once + 游标持久化 = 重启不丢消息 | 注意 durable 名称 `aero-server` + ephemeral `live.stream.*` 的区分设计 |
| Hub 扇出用 bounded mpsc + try_send | ✅ 正确。有背压边界，slow consumer 被剪枝 | 当前已加 `Arc<String>` 共享 + 大房间并行扇出，覆盖了第二次分析的方向五 |
| AI 预算用 per-ws 120 + global 120 all-or-nothing | ⚠️ 逻辑正确但攻击面开放 | 第 61 条消息耗尽 per-ws 预算后第 62 条绕过审核。需要人类兜底队列（见方向二） |
| 跨节点媒体用明文 RTP + 自编 bridge_frame | ⚠️ seam 标注明确（「未接线」），这可以接受 | 生产接线前需要 DTLS 加密|
| 消息编辑用乐观锁 (version 字段, migration 0157) | ✅ 正好堵住当前编辑路径的 TOCTOU | git diff 已显示这次 sprint 就在接这个 |

---

## 二、扩展方向

以下基于多轮分析 + git diff 交叉验证，筛选出**三条分析线共同指向但当前代码和 ROADMAP 都未覆盖**的高价值方向。

---

### 方向 A（P0）：自适应容量管理与过载保护回路

**为什么需要**：方向一（PG 连接池饥荒）、方向四（静态限流不感知后端）、方向三（降级缺乏编排）在三次独立的分析中反复出现，而且当前 git diff 中 `rate_limit.rs` 已经加了集群级 Redis INCR 限流——说明团队已经在往这个方向走，但只做了最外层（跨实例入口限流），没有做内层（容量感知反馈回路）。

**核心挑战**：

1. **信号采集**：当前的 metrics gauge（`pool.size`、`pool.num_idle`、NATS backlog）存在但零消费者。需要一个聚合器把多个信号融合为一个 0.0-1.0 的容量系数。
2. **反馈注入**：限流器（`RateLimiter::check_at`）需要接受一个动态因子，当前硬编码 `burst` + `per_second` 在构造时固定——这意味着限流器实例需要持有 `Arc<AtomicF64>` 引用而非固定值。
3. **滞后/恢复**：容量信号快速抖动时，限流器不能跟着跳（施密特触发器）。恢复时需要「慢启动」而非瞬间全开。

**架构变更**：

```
                  ┌──────────────────┐
PG pool gauge ──►│                  │
NATS backlog  ──►│  HealthAggregator │──► Arc<AtomicF64> capacity_factor
AI latency    ──►│  (EWMA + window)  │
Redis latency ──►│                  │
                  └──────────────────┘
                           │
                           ▼
              ┌──────────────────────────┐
              │  RateLimiter              │
              │  check_at(key, factor)    │
              │  = tokens >= 1 * factor   │
              └──────────────────────────┘
```

`HealthAggregator` 是新的单体模块（~200 行），放在 `aero-server` 或 `aero-common/src/capacity.rs`。不需要新的 crate。

**对现有系统的影响**：
- `RateLimiter` 需要从「构造时固定参数」改为「运行时动态参数」。这是一个**向后兼容的破坏**——但限流器只在 `rate_limit.rs` 内部实例化，外部是 `Arc<RateLimiter>` 引用，实例化点有限。
- `metrics_tasks.rs` 不需要改造，现有 gauge 继续写，`HealthAggregator` 订阅这些 gauge（或自己查询 `PgPool::num_idle`）。
- AI worker 的 `Semaphore` 并发数可以挂钩到同一个因子——当容量紧张时减少 AI 并发。

**风险**：反馈回路振荡。需要实测窗口大小和 EWMA alpha。建议从「只读不控」起步（先聚合指标、暴露 `/health/capacity` 端点、记录决策日志），观察一个月再启用自动调整。

---

### 方向 B（P1）：审核管线的人类兜底与预算攻击防御

**为什么需要**：`moderation_bot.rs` 的 `KeyedCostBudget(60)` + `CostBudget(300)` 耗尽后的行为是 `continue`（跳过审核）——这是纯自动化系统特有的盲区：攻击者可以在 60s 内发 61 条消息耗光预算，第 62 条不经过 AI 审核。当前已有的 `message_reports`（举报）和 `user_reports`（用户举报）表已经有 `status` 字段（pending/resolved/dismissed），可以把「预算耗尽后的降级」路由到「延迟审核」而非「跳过审核」。

**核心挑战**：

1. **延迟审核队列**：预算耗尽后的消息不经过 AI 直接存储并广播，但标记为 `unmoderated`。`moderation_bot` 在预算恢复后回扫未审核消息。这是一个「先发后审」的异步状态机。
2. **与 `message_reports` 的汇合**：如果审核队列的路由设计能直接对接现有的 `message_reports` 表——预算耗尽的消息自动插入一条 `message_report`（报告者是系统自己，`reason = "budget_exhausted"`）。这复用现有表结构而不需要新建审核表。
3. **触发面**：不仅是 budget exhaust，`AiWorker` 的 `MAX_ATTEMPTS=5` 后进入 `dead` 的消息也应该路由到人类审核队列。

**架构变更**：

```
moderation_bot
    │
    ├─ budget OK → AI moderate → pass/fail
    │
    └─ budget exhausted ─→ insert message_reports (system as reporter)
                                  │
                                  ▼
                            人类审核界面
                            (挂接到 approvals.rs 类似的 decide 端点)
```

对现有系统的**最小影响**：
- `moderation_bot.rs` 的 `handle_event` 在 `continue` 处改为 `insert_report` + `metric`
- `message_reports` 增加 `auto_moderation_failed` 标记字段（或利用现有 `status` + `reason` 文本）
- 人类审核端点是新的 REST 路由（可复用 `approvals.rs` 的审批模式：审核员 approve/reject/dismiss，对应决议后触发软删或放行）

**风险**：审核队列堆积的运营压力。建议初始阶段只对 `AERO_AI_MODERATION` 开启且有高信任评分的 workspace 启用「先发后审」。

---

### 方向 C（P1）：消息生命周期策略冲突优先级规范化

**为什么需要**：`message/sweep.rs` 的清扫逻辑使用 `COALESCE` 做 retention 优先级（频道级 > 工作区级），但 `legal_hold`（法务保全）和 `expires_at`（ephemeral/阅后即焚）之间的优先级是隐式的——当前 `sweep.rs` 代码中 `legal_hold` 是 boolean 跳过，`expires_at IS NOT NULL` 的消息如果在 legal hold 列表中，legal hold 胜出。但这个优先级（法务保全 > 阅后即焚 > 频道 retention > 工作区默认）从未被写成规范文档，导致了：

1. 用户困惑：「我设了阅后即焚为什么还在？」——答案是有合法保全指令
2. 消息编辑的旧版本（`message_history` 表）不受 `sweep.rs` 管理
3. `forward.rs` / `broadcast.rs` 的转发副本与原始消息生命周期脱钩

**核心挑战**：

1. **优先级规则显式化**：把「legal_hold > starred > expires_at > channel_retention > workspace_defaults > global_defaults」写进 `sweep.rs` 的函数注释和 AGENTS.md 作为不可违反的规范。
2. **`message_history` 的 retention**：编辑旧版本目前无自动清理。可以统一到 sweep 逻辑中，检查旧版本的创建时间是否超过源消息所在频道的 retention 窗口。但需要小心——旧版本是法务保全的证据链的一部分，不应在 legal_hold 下被清除。
3. **转发副本与源消息的关联**：`forward.rs` 创建的副本与原始消息没有 `forwarded_from` 关联列，导致源消息删除后转发副本成为「无源引用」——在 AI 问答和搜索 RAG 中会表现为死链接。

**架构变更**：

- `messages` 表增加 `forwarded_from` 可选列（nullable FK）→ 迁移 0158
- `sweep.rs` 增加对 `expires_at` 的额外检查：仅当 `legal_hold = false` 且 `starred = false` 才清除
- `message_history` 的清理逻辑统一到 `sweep.rs`（当前在独立的 `sweep_history` 路径，不与 retention 配置共享）

**对现有系统影响**：`forwarded_from` 列是 DB schema 变更，影响 `send_message` 写入路径。其余是纯逻辑变更。

---

### 方向 D（P1-P2）：Bot 平台 Bot Config 凭证加密

**为什么需要**：`bot.config` 的 JSONB 字段明文存储 API keys / tokens / secrets。任何有 DB 只读权限的查询都能读取所有 bot 的凭证。且与方向五（多租户数据运维）有交叉依赖——如果方向四独立推进，需要在 `workspaces` 表添加 `encryption_key_salt` 列作为密钥派生基础。

**核心挑战**：

1. **密钥层级**：每个 workspace 需要一个加密密钥（从 `encryption_key_salt` + workspace_id 派生），bot config 用这个密钥加密存储。这意味着 `aero-server` 启动时需要持有 master key（环境变量 `AERO_BOT_CONFIG_KEY`），在工作区级别派生工作密钥。
2. **密钥轮换**：bot config 写入时加密，读取时解密。轮换需要逐个 bot 读取-解密-重新加密-写入。这是一个可批量但非原子的操作。
3. **已有数据的迁移**：存量 bot.config 是明文 JSONB。需要一次性的 backfill 加密全部已有 config，标记已加密列。这需要停机或双读兼容。

**架构变更**：

```
启动时: master_key = env("AERO_BOT_CONFIG_KEY")
写入时: ws_key = HKDF(ws.encryption_key_salt, master_key) → AES-256-GCM → config JSONB
读取时: ws_key = HKDF(ws.encryption_key_salt, master_key) → AES-256-GCM → 解密 config
```

**依赖关系**：方向五需要先在 `workspaces` 表添加 `encryption_key_salt` 列。方向四在这一列的基础上建立 bot config 加密。如果方向五不先行，方向四需要自建密钥派生锚点（比如在 `bots` 表加 `encryption_salt`）。

---

### 方向 E（P2）：多租户数据导出与存储配额

**为什么需要**：当前 `aero-cli migrate` 只建表不验证多租户隔离。存储使用（`blobs.size`）已经存在，但没有按 workspace 的配额限制。`migration 0070` 的 export_job 管线已有工作区数据导出的基础设施，可以复用。

**核心挑战**：

1. **配额校验点**：`blob_upload` 路径上增加 `SELECT SUM(size) FROM blobs WHERE workspace_id = $1` → 超配额返回 429。这是一个简单查询，但需要在热路径上增加（可能慢在 `SUM` 扫描大表）。
2. **导出一致性**：export_job 管线导出时，如果导出的消息包含尚未完成的 AI 审核，导出中不应该包含 AI 标注（或标注应标记为 "pending"）。这是导出快照在并发写入下的一致性保护。
3. **导入新实例的校验**：导出 → 导入新实例 → 校验一致性，这条路径需要停机或迁移窗口。

**对现有系统影响**：最小。`blob_upload` 的配额检查是纯新增 + 一个可单独捕获的 429。export_job 的路径修改局限于查询逻辑。

---

## 三、接口设计建议

### 3.1 需要一个 `CapacityClient` trait

当前系统没有一个统一的方式来**查询服务依赖的健康状态**。每个模块各自调用各自检查。建议引入：

```rust
#[async_trait]
trait CapacityClient: Send + Sync {
    /// Current capacity ratio 0.0 (down) .. 1.0 (full capacity).
    /// None = unknown (don't adjust).
    async fn capacity(&self) -> Option<f64>;
}
```

实现者：`PgCapacity { pool: PgPool }`、`NatsCapacity { client: NatsClient }`、`RedisCapacity { client: RedisClient }`、`AiCapacity { http: Client }`。

这个 trait 不强迫所有服务实现——未实现 = `None` = 不参与反馈回路。

### 3.2 `RateLimiter` 需要接受动态因子

当前 `RateLimiter::check_at(key, now)` 的签名隐含了「窗口内容量是静态的」。需要扩展为：

```rust
impl RateLimiter {
    pub fn check_at(&self, key: ClientKey, now: Instant, factor: f64) -> RateLimitStatus;
}
```

`factor` 的默认值 `1.0` 使调用侧不需要改动。这一步改造的破坏面是整个 `rate_limit.rs` 的调用链——约 3 个调用点（`rate_limit.rs` 自己的 `layer` 中间件 + 测试用例）。

### 3.3 `Hub::fan_out_raw` 的 Arc 共享化已经做了

git diff 已经显示 `hub.rs` 加了 `fan_out_arc()` 和 `fan_out_arc_inner()`，接受 `Arc<String>` 来避免 N 次克隆。这是正确的方向。下一步是让 `RoomEvent` 的处理路径统一使用 `fan_out_arc`——当前 `fan_out_raw` 内部做 `Arc::from(text.to_owned())` 已经覆盖了大部分调用方。

### 3.4 审核管线需要一个新的抽象

当前 `moderation_bot.rs` 直接调用 `ai.moderate()`，没有「审核策略」的抽象层。建议：

```rust
enum ModerationDecision {
    Allow,                           // 通过
    Reject(String),                  // 拒绝（原因）
    Defer(String),                   // 延迟（原因如 budget_exhausted）
}

#[async_trait]
trait ModerationStrategy: Send + Sync {
    async fn moderate(&self, msg: &Message) -> ModerationDecision;
}
```

实现：`AiModeration`（现有路径）、`KeywordModeration`（已有 `AERO_BLOCKED_WORDS`）、`BudgetAwareModeration`（新：包含预算检查 + 降级逻辑）、`HumanReviewFallback`（新：延迟审核 + 举报表写入）。

这种策略组合模式允许不修改 `moderation_bot.rs` 的情况下切换审核引擎。

---

## 四、技术选型

### 4.1 不需要引入的新框架

当前技术栈（Rust 2021 / tokio / axum / sqlx / fred / async-nats / str0m）已经覆盖了所有**必须**的功能。以下场景**不需要**新框架：

- **消息队列**：NATS JetStream 胜任。不需要 Kafka / RabbitMQ 追加。
- **缓存**：DashMap（进程级）+ Redis（集群级）已经两极覆盖。不需要 Memcached 或额外的缓存层。
- **搜索**：pg_trgm + pgvector 已经在用。不需要 Elasticsearch / Meilisearch。
- **RPC**：NATS request-reply 或 HTTP 内部路由够用。不需要 gRPC / tarpc。
- **配置管理**：figment + `AERO__*` 环境变量已够用。不需要 Consul / etcd。

### 4.2 需要评估的依赖

| 需求 | 候选 | 评估 |
|------|------|------|
| Bot config AES-256-GCM 加密 | `aes-gcm` crate | 已审核（纯 Rust，forbid unsafe，审计友好） |
| CRDT 协作编辑 | Yrs (Rust) / y-websocket | 如果是前端做 CRDT（建议），则 Rust 端只需要 WebSocket bridge |
| SMTP | lettre（已引入，`mailer.rs` 已写） | 已完成。无二次评估必要 |
| SAML 2.0 SP | `bergshamra`（已引入，`Cargo.toml`） | 已标注 `forbid(unsafe_code)` + `pre-1.0` + 有 `AERO_SAML_EXPERIMENTAL_VERIFY` 门控。建议在前端 SAML 页面完善前保持实验性门控 |
| 会话吊销/远程登出 | Redis pub/sub + JWT blacklist | 不需要新 crate。当前的 `sessions.rs` 已有 `delete_session` 端点 |

### 4.3 自建 vs 集成的决策框架

| 场景 | 自建 | 集成 | 理由 |
|------|------|------|------|
| Bot 交互式 Block 前端渲染 | ✅ | | 当前 render.js 已有骨架，只是缺少 click handler。不需要框架 |
| AI 预算熔断 | ✅ | | 当前 `AiWorker` 已有限流，熔断是同一模块的扩展 |
| 容量自适应限流 | ✅ | | ~200 行 `HealthAggregator` + 限流器因子 hook，不值得引入新 crate |
| SAML 2.0 SP 集成 | | ✅ | bergshamra 已引入，SAML 协议是标准化，自建 XML 签名验证是安全灾难 |
| 运营面板/可视化 | | ✅ | Grafana dashboard as code（jsonnet/grafana-analyst），不在 aero 代码库中实现 |
| Web 前端框架 | | ✅ | 评估 Svelte 5（编译时框架，包体积小，Rust-like 响应式声明）vs 维持零框架并增加测试覆盖率 |

---

## 五、实施路线图

### 阶段划分

```
Phase 3.1 (当前 sprint 完结前)
├── ✅ 集群级 Redis INCR 限流（rate_limit.rs — git diff 已做）
├── ✅ AI worker 作业级别超时（worker/mod.rs — git diff 已做）
├── ✅ Anthropic 重试 + 退避（anthropic.rs — git diff 已做）
├── ✅ blob download 安全响应头（routes.rs — git diff 已做）
├── ✅ 消息编辑乐观锁（messages.rs — git diff 已做）
├── ✅ Hub Arc<String> fan-out（hub.rs — git diff 已做）
├── ✅ 邮件通讯（mailer.rs — git diff 已做）
├── ✅ 集群限流测试（rate_limit.rs — git diff 截断处可见测试）
└── 待定: SAML 2.0 SP 实验性支持（saml.rs + Cargo.toml bergshamra）

Phase 3.2 (容量保护)
├── P0: HealthAggregator 模块（~200 行，纯新增）
│   └── 先观察不控制：聚合指标 + /health/capacity 端点 + 日志
├── P0: RateLimiter 动态因子（改造现有 3 个调用点）
├── P1: AI worker Semaphore 挂钩 capacity factor
└── P1: 慢启动恢复 + 施密特触发器抗抖

Phase 3.3 (审核韧性)
├── P1: moderation_bot 预算耗尽路由到 message_reports
├── P1: 人类审核决策端点（复用 approvals.rs 的审批模式）
├── P2: AiWorker dead 作业自动路由到审核队列
└── P2: 先发后审的 message_reports 管理面板（Web 端最小 UI）

Phase 3.4 (生命周期 + Bot 加密)
├── P1: message_history sweep 统一化
├── P1: forwarded_from 列（迁移 0158）
├── P1: workspaces.encryption_key_salt 列（迁移 0159）
├── P2: bot.config AES-256-GCM 加密 + backfill
└── P2: 转发副本与源消息生命周期脱钩修复

Phase 3.5 (数据治理)
├── P2: blob_upload 存储配额检查
├── P2: export_job 一致性快照（pending AI 标注处理）
└── P2: 导出 → 导入校验管线的文档（非代码）
```

### 风险矩阵

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| HealthAggregator 反馈回路振荡导致限流器抖动 | 中 | 用户体验下降（被无辜限流） | 先观察不控制（Phase 3.2 第一个子项就是只读模式） |
| Bot config 加密 backfill 期间暴露密钥 | 低 | 凭证泄露 | backfill 在单次迁移中完成，窗口 < 5min；记录审计事件 |
| forwarded_from 列导致旧 forward 失效 | 低 | 已有转发副本无源引用 | NULL = 无引用（与当前行为相同），不产生新 bug |
| 审核队列堆积 → 未审核消息暴露 | 中 | 有害内容传播 | 仅「先发后审」的 workspace 有此风险；初始默认关闭 |
| SAML SP 的 XML 签名验证漏洞 | 低（bergshamra 标注已读） | SSO 冒充 | 保持 `AERO_SAML_EXPERIMENTAL_VERIFY` 门控直到第三方审计 |

### 优先级排序逻辑

1. **方向 A（容量保护）**定在 P0 是因为它是阻塞性的——启动后每次消息量上涨都会触发 PG 连接池争抢，且当前已经在 `rate_limit.rs` 做了集群级限流但缺反馈回路。
2. **方向 B（审核韧性）**定在 P1 是因为它是安全缺陷但不阻塞消息发送——预算攻击是 DoS 级别的风险但非数据泄露。
3. **方向 C（生命周期）**定在 P1 是因为它不会产生新 bug 但会产生数据留存合规缺口。
4. **方向 D（Bot 加密）**定在 P1-P2 是因为已有 `workspaces` 表需要列添加，如果 Phase 3.2 先做 `encryption_key_salt` 则方向 D 可以提前。
5. **方向 E（配额）**定在 P2 是因为没有客户诉求的压力，且 export_job 管线已经就位，扩展成本低。

---

## 一句话总结

当前代码库最需要的东西不是新 crate、不是新框架、不是新功能——而是一个**容量感知层**（~200 行代码在 rate_limit 和 metrics 之间的信号回路），以及一个**审核降级预案**（~150 行在 moderation_bot 和 message_reports 之间的桥接）。这两样的组合能堵住的漏洞比任何单个功能方向都大。

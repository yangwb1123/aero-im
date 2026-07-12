Now I have a thorough understanding of the complete architecture. Let me provide the architectural analysis.

---

# 架构分析：Aero IM 的技术评估与扩展方向

## 1. 架构评估

### 1.1 核心架构优势

**Aero IM 的架构成熟度显著高于同类 Rust 实时系统的平均水平。** 以下几个设计决策构成了坚实的技术底座：

**事件驱动骨架（NATS JetStream + 进程内 Hub）**。系统采用「业务层产事件 → NATS 按 subject 跨实例投递 → 每进程 Hub 本地扇出」的 DAG 模型。这个选择的精妙之处在于：NATS 只承担「事实源」职责（持久化 + 跨实例投递），而 WebSocket 扇出是纯粹的本进程 bounded mpsc。这避免了将 NATS 作为实时消息代理的常见陷阱（连接数爆炸、消息放大），同时 NATS durable consumer 提供了天然的重放和 at-least-once 保证。

**Bot/Agent 系统是真正的差异化优势**。当前架构将 bot 作为一等公民对待——它们有专用的 ParticipantKind、NATS durable consumer group、幂等守卫、预算控制、fail-open 策略。这不是事后附加的 webhook 体系，而是从事件总线层面内置的扩展模型。`agent_bot`、`unfurl_bot`、`transcribe_bot`、`golive_bot`、`moderation_bot`、`push_bot` 等形成了可组合的「反应式微服务」模式。

**预算驱动的 AI 成本控制**。`AiWorker` 的预算模型（全局 120 按权重 + per-ws 120 over 60s）是实用的架构设计——它避免了 AI 调用无限增长，同时在不丢任务的情况下实现了平滑的准入控制。`KeyedCostBudget` + `Semaphore` 限并发 + `FAILS-CLOSED on spend` 的组合保证了预算不被超额。

**Redis 的正确使用边界**。系统严格区分「跨节点集群状态走 Redis sorted-set」和「单进程状态走内存」。Presence、直播观看数、通话 roster 这些跨实例共享状态走 Redis 的 zadd/zremrangebyscore，而连接注册、路由表等走 DashMap。这个边界清晰，避免了 Redis 过度依赖。

**crate 依赖图的严格分层**。从叶子 crate（`aero-common`）到基础层（`aero-bus`/`aero-storage`/`aero-auth`）到业务层（`aero-im-core`/`aero-ai`）到组合层（`aero-server`），箭头单向向下。直播 crate 自成体系（`aero-live-core` → `-rtmp`/`-hls`/`-whip`/`-webrtc`/`-srt`）。这为独立测试和未来提取独立服务提供了可能性。

### 1.2 关键设计局限

**没有外部开发者入口（Platform Surface）。** 当前 280+ 路由全部是内部 API。外部集成只能通过 webhook（outgoing，被动推送）和 SCIM（仅入站）。没有：
- 公开 REST API + 正式版本化 + SDK
- Scope 细粒度 PAT（当前 PAT 是 session 替代品，all-or-nothing）
- OAuth 2.0 授权码流（想为第三方 app 做 Aero SSO？当前只能 OIDC JIT）
- 事件订阅的正式 catalog

**Bot 投递体系存在不对称性。** 对比 `webhooks::run_webhook_dispatcher`（有完整 retry/DLQ/circuit-breaker 管线）和 `bot_dispatch.rs`（one-shot delivery，无重试），两者的可靠性差异显著。bot subscription delivery log 虽然记录了出价，但没有重试引擎。这会造成「webhook 可靠，bot webhook 一次投递」的运维认知负担。

**NATS subject 命名空间缺乏治理。** 当前有 `im.room.*`、`live.stream.*`、`im.broadcast.*`（规划中）、`im.incident.*`（规划中）。随着功能扩展，缺乏以下治理：
- 无 subject 命名约定文档
- 无 consumer 命名规范（`aero-server`、`aero-bot`、`aero-ooo`、`aero-unfurl`、`aero-transcribe`、`aero-golive`、`aero-push`、`aero-moderation`——8 个 durable consumer 都在 `im.room.*` 上）
- 无 subject 生命周期管理（废弃的 subject 如何标记/清理）

**配置系统的双重标准。** Figment 用 `AERO__SECTION__KEY` 双下划线，但关键配置如 `AERO_RATE_LIMIT_PER_SEC`、`AERO_TRACE_SAMPLE_RATE`、`AERO_S3_*` 用单下划线。这不是大问题，但新开发者会困惑于「我该用什么风格？」

**媒体面存在明确的「已建未接线」间隙。** `SfuMediaSession` 的 `bind`/`run`、`call_bridge_supervisor` 的 `ensure_egress`、SRT 的 socket loop——这些组件有单测覆盖但生产未接线。架构上这可以接受（提前建 seam、推迟联调是好的做法），但在文档和代码中需要更显式的标注：当前 `AGENTS.md` §4.5 的标记方式正确，可以推广。

### 1.3 技术债评估

| 领域 | 债务类型 | 严重度 | 建议 |
|---|---|---|---|
| `routes.rs` 持续膨胀（3000 行上限即将触达） | 结构债 | 中 | 按功能域拆分（现有子模块路由已独立，但注册链集中在 `routes.rs::build()`） |
| OpenAPI 文档手写且可能过时 | 文档债 | 中 | 考虑 `utoipa` 或 `okapi` 自动生成（但需评估 Rust 宏的编译开销） |
| Bot dispatch 与 webhook dispatch 的 retry 不一致 | 功倍债 | 高 | 统一 retry 管线：将 `bot_event_subscriptions` 的 delivery 纳入同一 retry/DLQ 框架 |
| 依赖版本锁定（str0m 0.19、async-nats 0.36）但无定期升级策略 | 依赖债 | 低 | 建立季度依赖升级节奏，重点跟踪 nats-rs 和 str0m 的 API 不稳定期 |
| `webhook::breaker::BreakerState` 在 DB 中存储 `(i32, Option<timestamptz>)`，无自动半开探测 | 设计债 | 中 | 当前 breaker 是「断开后手动或等待超时重试」，缺少半开放探测（send one probe request）。可复用已有的 `requeue` 模式 |

---

## 2. 架构扩展方向

### 方向 A：开发者平台（Public API + SDK）—— P0

**为什么需要。** 这是从「工具」到「平台」的转折点。Aero IM 当前有 bot 生态（bot_dispatch）、webhook 出站、SCIM、call-bridge 等扩展点，但它们都服务于「单租户内部」。要成为企业协作平台，必须有正式的公开 API 供第三方开发者集成。

**核心挑战。**
- **API 版本化与向后兼容。** 现有 280+ 路由深度耦合于内部类型。`RoomEvent`、`Block`、`ParticipantId` 等类型从 `aero-common` 直接暴露。API 版本化意味着需要定义公共数据合约（独立于内部类型），并建立版本映射层。
- **鉴权的 scope 模型。** 当前 PAT 是全局 bearer，无 scope 限制。OAuth 2.0 所需 scope 模型需要重构鉴权中间件——从「有 token = 有权限」到「有 token + scope 匹配 = 有权限」。这影响 `AuthUser` extractor、`assert_room_access` 守卫链、以及所有 API handler 的 `AuthUser` 使用方式。
- **SDK 生成与维护。** 选择 OpenAPI Generator vs 手写 SDK 的权衡。Rust 生态的 OpenAPI 工具（utoipa、progenitor）仍不够成熟。

**预期架构变更。**
```
┌────────────────────────────────────────┐
│  当前（内部 API）                        │
│  /api/rooms/:id/messages → 直接内部类型 │
│  PAT → all-or-nothing                   │
│  Webhook → 被动推送                     │
│  Bot → one-shot webhook                 │
└────────────────────────────────────────┘
                ↓
┌────────────────────────────────────────┐
│  目标（公开 API + SDK）                  │
│  /api/v1/messages → 公共合约           │
│  /api/v1/rooms → 公共合约              │
│  OAuth 2.0 + scoped PAT               │
│  Rust / JS / Python SDK               │
│  事件订阅 catalog（正式暴露）            │
│  API 变更管理 + 弃用策略                │
└────────────────────────────────────────┘
```

**实施路径。**
1. Phase A：定义公共 API 合约（从现有内部类型提取，如 `PublicMessage`、`PublicRoom`）→ 建立版本映射层（`mapper` 函数集）
2. Phase B：引入 OAuth 2.0 中间件 + scope 枚举 → 重构 `AuthUser` 为 trait（支持 PAT / OAuth / JWT session 三种）
3. Phase C：生成 SDK + 编写文档 + 建立 API 变更管理流程
4. Phase D：公开事件订阅 catalog，统一 bot dispatch 和 webhook dispatch 的 retry 管线

### 方向 B：多租户运行时隔离——P1

**为什么需要。** 当前 Aero IM 是单租户架构。虽然 `workspace_id` 贯穿数据模型（`rooms.workspace_id NOT NULL`），但运行时资源（AI 预算、限流、内存、goroutine）缺乏隔离。企业 SaaS 化需要「一个租户的突发流量不损害其他租户」。

**核心挑战。**
- **资源隔离的多个层面。** CPU/内存/连接池无法在 Rust 进程内原生隔离（Rust 无 cgroup 感知）。可行的路径是：每个租户独立进程（容器化）vs 单进程 weighted semaphore。前者运维成本高，后者隔离度不够。
- **AI 预算的租户级独立。** 当前 `KeyedCostBudget` 按 workspace 限流（per-ws 120 over 60s）。这是正确的方向，但需要扩展到：租户级并发数上限、日预算上限、超额后的 degrade 策略（限速 vs 降级 vs 拒绝）。
- **可观测性的租户标签爆炸。** `AERO_PER_TENANT_METRICS` 已有，但开启后 label 基数随租户数线性增长，可能导致 Prometheus 性能问题。

**架构决策选项。**

| 方案 | 隔离度 | 运维复杂度 | 资源效率 | 建议 |
|---|---|---|---|---|
| 单进程 + weighted semaphore | 低 | 低 | 高 | Phase A 可行 |
| 每租户独立进程（同机容器） | 高 | 中 | 中 | Phase B |
| 每租户独立 NATS subject 空间 | 中 | 低 | 高 | 立即做 |
| 每租户独立 PG schema | 高 | 高 | 低 | 不建议 |

**预期架构变更。**
- 资源管理器抽象层（`ResourceAllocator<W>` trait）：`PerProcessSemaphore` → `WeightedFairQueue`
- NATS subject 命名空间加入 `{workspace_id}` 前缀：`im.room.{ws}.{room_id}`
- 租户级 AI 日预算表（`tenant_ai_budgets`）
- 可观测性层的 label 基数控制（aggregated 指标 + sampled per-tenant 指标）

### 方向 C：工作流自动化引擎——P1

**为什么需要。** 这是方向 5 的核心——从「bot 订阅式反应」到「管理员可配置的工作流」（IFTTT/Zapier for Aero）。企业客户的核心诉求之一是自动化审批、自动标签、自动通知等流程。

**核心挑战。**
- **循环检测。** 当前 `agent_bot` 的「非自回守卫」可以复用，但工作流引擎的循环可能涉及更复杂的多步链：工作流 A 发消息 → 工作流 B 响应改标签 → 工作流 A 再次触发。需要 `_workflow_execution_id` 元数据冒泡和基于 hash 的循环检测（`(trigger_fingerprint, workflow_id)` 幂等键）。
- **动作原子性。** 工作流动作（发消息、改角色、调 API）需要事务性保证：要么全部成功，要么不执行任何动作。当前 bot 系统是「逐动作 fail-open」模式，工作流引擎需要「事务块」语义。
- **执行顺序与并发控制。** 同一工作流的两个实例不应并行执行（竞态条件）。需要 `SELECT ... FOR UPDATE` 或乐观锁（`version` 列）。

**最佳架构参考。** 复用现有基础设施：
- **触发器匹配** → `bot_dispatch.rs` 的订阅式模型（`BotRepo::subscribe` + `(event_type, filters)`）
- **定时触发器** → `bin/boot/` 的 interval timer 模式（`embedding_backfill` 的 300s 轮询）
- **动作执行** → `ImService` 的直接调用（bot 已有 `ImService::send_message` 的 seam）
- **NATS 事件分发** → `im.workflow.*` new subject namespace

**实施路径。**
1. Phase A：工作流定义表（`workflows` + `workflow_triggers` + `workflow_actions`）+ 触发器匹配引擎（复用 bot dispatch）
2. Phase B：动作执行器（发消息、调用 webhook、改角色）+ 幂等执行保证
3. Phase C：Web UI 的低代码配置界面 + 执行日志可视化 + 错误诊断

### 方向 D：紧急广播系统——P2

**为什么需要。** 这是方向 4 的核心。企业级 IM 的差异化能力之一是「关键消息必须送达」——灾难通知、安全警报、系统维护。当前 Aero IM 的 `announcements.rs` 是横幅公告，不保证送达；`golive_bot` 的 fan-out 是 best-effort。

**核心挑战。**
- **投递确认的可靠性。** 广播的确认回执（confirmations）不适合走 NATS（每个回执都是独立小消息，会放大事件量）。最佳路径是 REST API 直接写 DB（`broadcast_confirmations` 表），单独一个 sweep 定时器检查未确认人员。
- **二次确认的原子性。** 防止意外发送——需要两阶段提交：
  ```
  POST /api/workspaces/:id/broadcasts → 201 (status: "pending")
  POST /api/workspaces/:id/broadcasts/:bid/confirm → 200 (status: "sent")
  ```
  创建后若不确认，2 分钟后自动取消（NATS 的 `TTL` 或定时器扫描）。
- **升级链。** 对于 P1 广播，如果特定人员（如值班工程师）5 分钟内未确认，自动升级到其 manager（`org_chart.rs` 的 `reporting_chain`）。

**预期架构变更。**
```
NATS subject:   im.broadcast.{workspace_id} (durable)
DB tables:      broadcast_messages, broadcast_confirmations, broadcast_escalation_policies
Timers:         broadcast_pending_cancellation (2min sweep)
                broadcast_unconfirmed_escalation (every 60s)
Bot reuse:      broadcast_bot (durable consumer on im.broadcast.*)
API endpoints:  POST broadcasts, POST broadcasts/:id/confirm, GET broadcasts/:id/status
```

### 方向 E：跨节点弹性与单元化——P2

**为什么需要。** 当前架构假设单集群。当部署扩展到多 region 或多云时，需要处理：
- 媒体面不可 NAT 穿越（跨 region 的 WebRTC/SFU）
- 事件总线的分区（NATS 跨 region 同步延迟）
- 有状态资源的位置感知（sticky routing）

**核心挑战。**
- **媒体面的 region 亲和性。** `call-bridge`（明文 RTP）可以跨节点，但跨 region 的延迟和丢包会严重劣化通话质量。需要 region 感知的 `SfuRouter`——优先在用户所在 region 内建媒体面，仅当需要时跨 region 转发。
- **事件总线的跨 region 复制。** NATS JetStream 支持跨 cluster mirroring，但不可在同步链路上做 at-least-once 保证。需要业务层的幂等去重（已有 `seq` 单调递增，可跨 region 扩展为 `(origin_region, seq)` 复合去重键）。
- **数据主权。** 企业客户可能要求数据驻留在特定区域。需要 per-workspace 的数据位置策略——决定消息/附件/AI 数据存储于哪个 region 的 PG/Redis/S3。

**实施路径。**
1. Phase A：region 感知的 SFU peer 选址（client `geo-ip` → 最近 region → 本地 SFU）
2. Phase B：NATS 跨 region mirroring + 去重键扩展
3. Phase C：每工作区数据位置策略（`workspaces.data_region`）+ 迁移工具

---

## 3. 接口设计原则

### 3.1 内部 vs 外部接口的分离

这是当前架构最紧迫的接口设计议题。原则是：

**内部接口（crate 间）** 可以继续使用 `aero_common` 的强类型（`RoomEvent`、`Block`、`ParticipantId`）。这些类型优先考虑类型安全和表达力，不保证序列化稳定性。

**外部接口（HTTP API、SDK）** 需要独立的公共合约。建议：

```
crates/aero-public-api/
├── api/              # OpenAPI spec (YAML)
├── types/            # 公共数据类型（PublicMessage, PublicRoom, etc.）
│   ├── v1.rs
│   └── v2.rs
├── mappers/          # 内部类型 ←→ 公共类型的转换
│   ├── message.rs
│   ├── room.rs
│   └── participant.rs
├── middleware/        # OAuth 2.0 extractor
├── routes/           # 版本化路由（与内部路由并行）
└── sdk/              # 可选的 SDK 生成
```

**向后兼容原则。** 公共 API 合约一旦发布，只能加字段不能减字段（Jackson 兼容性）。内部类型 `Block` 的变体可以新增，但公共合约的 `PublicBlock` 需要标记新增变体为 `@since v1.2`。

### 3.2 认证与授权的统一抽象层

当前有三个认证路径（JWT session + PAT + OIDC）和至少两个授权守卫（`assert_room_access` + `member_role`）。随着公开 API 的引入，需要统一：

```rust
trait Authenticator {
    async fn authenticate(req: &Request) -> Result<AuthContext, AuthError>;
}

struct AuthContext {
    participant: ParticipantId,
    method: AuthMethod,        // Session | Pat | OAuth | BotToken
    scopes: HashSet<Scope>,    // 公开 API 时使用
    workspace: Option<WorkspaceId>,
}
```

`AuthUser` extractor 可从 `AuthContext` 派生，保持向后兼容。`assert_room_access` 可以加入 scope 校验：

```rust
// 当前
pub async fn publish_message(auth: AuthUser, ...) {
    assert_room_access(&auth, &room).await?;
    // ...
}

// 扩展后
pub async fn publish_message(ctx: AuthContext, ...) {
    ctx.assert_scope(Scope::MessageWrite)?;
    assert_room_access(&ctx, &room).await?;
    // ...
}
```

### 3.3 DSL 式的触发器匹配

工作流引擎（方向 C）和 bot subscription 都需要事件匹配。当前 `bot_dispatch.rs` 用 Rust 代码硬编码匹配逻辑。如果要支持管理员配置，需要一个轻量级 DSL 或声明式匹配器：

```rust
// 声明式匹配器（不引入完整 DSL）
struct TriggerMatcher {
    event_type: EventType,        // "message_posted" | "member_joined" | ...
    filters: Vec<Filter>,         // room_id = ?, workspace_id = ?, has_mention = ?, ...
    condition: Option<Condition>, // AND/OR 组合
}

// 执行时
fn matches(ev: &RoomEvent) -> bool {
    filters.iter().all(|f| f.eval(ev))
}
```

这个模式可以统一 bot subscription、webhook outgoing、workflow trigger 的匹配逻辑。

---

## 4. 技术选型

### 4.1 不建议引入的技术

| 技术 | 为什么不建议 | 替代方案 |
|---|---|---|
| gRPC 替代 REST | 当前纯协议面在 axum 上工作良好，gRPC 增加运维负担（HTTP/2 负载均衡） | 当前 REST + WS 足够 |
| Kubernetes Operator | 早期引入算子增加运维复杂度；Aero IM 当前是单进程单体 | docker-compose / systemd 足够 |
| 独立消息队列（RabbitMQ/Kafka） | NATS JetStream 已满足 at-least-once + 持久化需求 | NATS 就是正确的队列 |
| wasm 插件引擎 | 工作流/机器人目前不需要 sandboxed 运行时 | 解释型 DSL（TOML/YAML 声明）+ Rust 动作 |

### 4.2 值得评估的技术

| 技术 | 用途 | 评估重点 |
|---|---|---|
| `utoipa` | OpenAPI 自动生成 | 宏编译时间、正确率、与现有 axum 路由的兼容性 |
| `pgmq` | PG 原生消息队列 | 对比 NATS：简化运维（少一个中间件），但缺 JetStream 的跨实例扇出能力 |
| `tower` 中间件层重构 | 统一认证/鉴权/限流/审计 | 当前中间件散布在 route handler 内；提取为 tower Layer |
| `quickjs` / `rhai` | 工作流 DSL 的运行时 | 比 wasm 轻量，但安全隔离是问题；rhai 更 Rust 原生 |
| `opentelemetry` 增强 | 租户级追踪采样 | 需要有损采样策略（Head-based + Tail-based 配合） |

### 4.3 自建 vs 采购决策框架

对于新功能，以下条件满足任意两条即考虑采购：

1. **属于行业标准协议**（OAuth 2.0、SCIM 2.0、OpenID Connect）—— 标准协议有成熟库
2. **安全敏感**（认证、加密、密钥管理）—— 不应自建
3. **运维成本 > 实现成本**（SMTP 网关、SMS 网关）—— 采购/接入外部服务

适用于自建的条件：
1. **核心业务逻辑**（消息路由、bot 调度、SFU 转发）
2. **需要深度定制**（AI 预算控制、内容审核策略）
3. **数据主权要求**（企业客户数据驻留）

按这个框架，方向 C（工作流引擎）应自建（核心业务 + 深度定制），方向 A（公开 API）应基于 `tower` + `axum` 自建鉴权层（OAuth 2.0 用库，scope 模型自建）。

---

## 5. 实施路线图

### 优先级排序

```
P0（必须）：开发者平台 Phase A-B    — 平台转型基础，决定生态天花板
P1（重要）：多租户隔离 Phase A       — SaaS 化基础
            工作流引擎 Phase A       — 企业差异化
P2（增益）：紧急广播系统              — 企业关键场景
            跨节点弹性 Phase A       — 多 region 扩展
P3（长期）：开发者平台 Phase C-D
            多租户隔离 Phase B-C
            工作流引擎 Phase B-C
```

### 阶段划分

**Phase 1（3-4 个月）—— 根基加固**

| 模块 | 产出 | 风险 |
|---|---|---|
| 公共合约定义 | `aero-public-api/` crate：PublicMessage/PublicRoom/PublicParticipant + mappers | 合约设计不当导致后续大量 break change |
| 统一 AuthContext | 重构 `AuthUser` → `AuthContext` trait + Scope 枚举 | 影响 200+ handler 的签名变更 |
| Bot dispatch retry 统一 | 将 bot_event_subscriptions delivery 接入 webhook_delivery 的 retry/DLQ 框架 | `bot_event_subscriptions` 无 FK 链到 `webhook_delivery_log` |
| NATS subject 治理 | 文档化 subject/conumer 命名约定 + 废弃 subject 清理流程 | 存量的 8 个 consumer 需逐个迁移 |

**Phase 2（3-4 个月）—— 核心能力**

| 模块 | 产出 | 风险 |
|---|---|---|
| OAuth 2.0 | 授权码流 + scope 验证 + token 交换 | 与现有 JWT session 体系的共存策略 |
| 工作流引擎 Phase A | workflow/trigger/action 表 + 匹配引擎 + 动作执行器 | 循环检测不够完备 |
| 多租户资源隔离 | 租户级 AI 预算 + 租户级连接池加权 | 单进程内无法 CPU 隔离 |
| 紧急广播 | broadcast 表 + 二次确认 + 升级链 | 升级链的 Manager 查找（`reporting_chain`）性能 |

**Phase 3（4-6 个月）—— 平台成熟**

| 模块 | 产出 | 风险 |
|---|---|---|
| SDK 生成 | Rust/JS SDK，基于 OpenAPI 规范 | SDK 维护成本高于价值 |
| API 版本化 | `/api/v1/` + `/api/v2/` 路由并行 + 弃用策略 | 旧 bot/webhook 回调 URL 的迁移 |
| 工作流 Phase B | Web UI 低代码配置界面 + 执行日志 | UX 复杂度 |
| 跨节点弹性 | Region 感知 SFU + 跨 region NATS mirror | 网络延迟下的媒体质量劣化 |

### 关键风险矩阵

| 风险 | 概率 | 影响 | 缓解 |
|---|---|---|---|
| 公开 API 合约设计的「冻结」效应 | 高 | 高 | Phase 1 先做「内部预览」（`/api/internal/v1/`），收集反馈再发布 |
| OAuth 2.0 的 token 刷新与现有 JWT 刷新冲突 | 中 | 中 | 统一 token 类型为 `AeroToken { kind, scopes, ... }`，两套流程生成同一类型 |
| 工作流循环导致消息风暴 | 中 | 高 | 严格 `(trigger_fingerprint, workflow_id)` 幂等键 + 每工作流最大触发次数（circuit breaker） |
| 单进程多租户的 OOM | 低 | 高 | 为每个租户设置独立 `mpsc::channel` bound，租户级背压隔离 |
| WebRTC/SFU 媒体联调周期超出预期 | 高 | 中 | CI 用模拟 RTP 流（已有单测覆盖），人工联调作为 Release Blocking 项 |

---

## 结语

Aero IM 的架构基础扎实，事件驱动骨架、bot 体系、AI 预算控制都是 90 分以上的设计。当前最关键的架构决策不是技术栈选型（技术栈已经足够好），而是 **「作为平台公开的时机和方式」**。

方向 A（开发者平台）应该在 Phase 1 就启动，不是因为它是功能需求最高的，而是因为它约束后续所有的架构变更——版本化决策影响路由设计、scope 模型影响鉴权重构、公共合约影响内部类型的变更自由度。**错过这个约束窗口，后续所有扩展都会背负「不破坏现有内部调用」的兼容包袱。**

方向 C（工作流引擎）和方向 B（多租户隔离）是企业级关键差异点，它们的架构设计可以复用 bot dispatch 的既有 seam，不应重造轮子。

方向 D（紧急广播）和方向 E（跨节点弹性）是 P2 级增强——它们不影响核心价值主张，但对特定客户场景至关重要。紧急广播可以快速复用 `golive_bot` 模式在 2-3 周内完成 Phase A。

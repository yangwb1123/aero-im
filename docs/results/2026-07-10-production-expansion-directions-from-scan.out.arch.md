基于对项目代码库（16 个 crate、~146 个仓储、~125 个 server 模块、157 次迁移）、现有架构分析文档（特别是 `ROADMAP.md`、`2026-07-02-expansion-directions.md` 和 `2026-07-12-architectural-analysis-operational-maturity.md`）以及所提供的源码验证结果的全面审查，以下是结构性架构分析。

---

# 架构分析：Aero IM——事件驱动的 AI-native IM 与直播平台

## 一、架构评估

### 1.1 架构现状的核心优势

项目在若干关键方面展现出了卓越的架构成熟度：

**事件驱动骨干是正确的、也是务实的选择**

```
RoomEvent → ImService::publish_room_event → NATS JetStream
→ run_bus_listener → Hub::fan_out_raw → WebSocket
```

这一拓扑在概念上是完备的：NATS 充当跨实例事实源（durable consumer），Hub 充当进程内单 writer 有界 mpsc 扇出。这种设计经 Slack 和 Discord 等平台的生产环境验证。两个命名空间（`im.room.*`、`live.stream.*`）各自配备 per-subject 单调序列号与 at-least-once 投递，构成了接收端去重与排序的坚实基础。

**Crate 分层清晰，具备独立可替换性**

依赖 DAG 的组织形式健康且易于理解：

```
aero-common (叶子)
  → aero-bus, aero-storage, aero-auth, aero-signaling (基础层)
    → aero-im-core, aero-im-call, aero-live-*, aero-ai (领域层)
      → aero-server (组合层)
```

每个 crate 的职责边界清晰。新增功能可在单一 crate 内追加模块，不影响其他 crate 的公共接口。这与 AGENTS.md §4.2 中明确的约束（"feature-first 单位是 crate，不是 src/{domain}/"）保持一致。

**AI 预算系统是深思熟虑的设计**

per-workspace `KeyedCostBudget`（60 单位）与全局 `CostBudget`（300 单位）在 60 秒滑动窗口上的组合，配合基于 kind 的权重（Embed=1、Moderate=2、Summarize=3、Answer=5）和 `FALLS-CLOSED on spend` + `defer`（而非失败）的背压策略，体现了对 AI 成本治理的成熟理解。能够在 SaaS 环境中防止单一租户独占全局 AI 预算——这是少数 AI-native 产品能正确处理的关键机制。

**监控基础设施已部分就位（比文档声称的更完善）**

与文档声称的 "monitoring/ 为空" 相反，源码验证发现系统包含生产级的 Prometheus recording rules（多窗口错误率计算）、burn-rate SLO 告警（99.9% 可用性目标）、Grafana RED 看板和 Alertmanager 配置。指标层已就位——"最后一公里"（scrape 配置接入、告警路由到 PagerDuty/钉钉等）尚未完成。

**关键路径上的降级策略定义明确**

系统在多处边界定义了 fail-open 行为：AI 无 key 时退化为 `HashEmbedder`（确定性哈希嵌入，维度对齐到 1024 维以匹配 Voyage）、Redis 故障时限流 fail-open、迁移失败时进程退出（fail-fast）。这些是经过深思熟虑的工程取舍。

### 1.2 架构局限性与技术债

**A. 迁移生命周期管理是最严重的架构债务（P0）**

157 次迁移全部为单向，`down.sql` 数量为零。`sqlx::migrate!("../../migrations")` 在启动阶段、HTTP 服务器启动前运行。`CREATE INDEX CONCURRENTLY` 在 `sqlx::migrate!` 的事务包装内会静默失败或导致锁升级。

**后果**：部署不可逆、蓝绿部署有风险（不兼容的 schema 变更会导致旧进程崩溃）、迁移执行期间服务无响应。

**B. 80+ 个仓储共享单一连接池——无仓储级隔离**

```rust
pub struct XRepo { pool: PgPool }
```

所有仓储都通过 `XRepo::new(s.pool.clone())` 从同一个 `PgPool` 派生。一个慢查询（例如在未分区的大表上执行全表 `pg_trgm` 搜索）可能耗尽整个连接池，导致消息发送、AI 嵌入、认证等完全不相关的功能路径被阻塞。当前的 `statement_timeout = 10s` 是全局兜底，并非细粒度防护。

**C. 错误处理采用三层混合模式**

| 模式 | 适用范围 | 问题 |
|------|----------|------|
| `thiserror` 枚举 | 少量仓储 | 良好实践，但不统一 |
| `anyhow` | 大量路由 handler | 调用链中丢失类型信息，无法按错误类型做差异化处理 |
| `ApiError`（自定义包装） | axum handler 返回类型 | `code` 字段是 `String`，客户端无法做结构化处理 |

`ApiError.into_response()` 中的 `code` 字段来自 `self.0.code()`（`aero_common::Error` 的 `code()` 方法），返回的是 `String`。不存在 `#[non_exhaustive] ErrorCode` 枚举。这意味着 API 消费者无法对错误码进行穷举匹配——客户端只能做字符串比较。

**D. 模块边界在膨胀，但缺少内部 API 契约**

`aero-storage` 有 146 个 `.rs` 文件，`aero-server/src/` 有 125 个 `.rs` 文件。增长模式是"遇到新功能 → 新建仓储 → 新建 handler 模块"，缺乏定期的模块边界审查。`routes.rs` 的 `build()` 方法长达 ~2854 行（接近 3000 行硬上限），单一函数内通过 `.merge()` 组合了 100+ 个子模块路由——这是架构的脆弱点：修改任意路由的认证策略都需要理解并修改同一个巨型函数。

**E. CI 管线存在执行缺口**

CI 配置中的 `integration-test` 和 `coverage` job 被注释。`.github/workflows/ci.yml` 中明确有文档注释："当前无 CI runner，此配置作为'就位准备'。" 这意味着所有与数据库交互的逻辑（35+ 个 `#[ignore]` 测试）从未在 CI 中执行。每次合并到 main 都可能引入数据库交互逻辑的回归——开发者只有在本地配置完整环境后才能发现。

**F. 无 per-user 投递台账（inbox）**

缺乏 `delivery_cursor(participant_id, room_id, last_acked_message_id)`。这导致重连时完全依赖按房间从 PG 回放历史（`list_since` keyset scan），而非 O(log N) 的 seq-based 增量同步。后果包括：长离线窗口（数天）需要按房间全量回放、多设备各自独立重放同一批 1000 条消息（故障恢复时放大三倍 DB 负载）、以及缺乏"已投递 vs 待投递"的紧凑追踪。

### 1.3 关键设计决策评估

| 决策 | 评估 | 理由 |
|------|------|------|
| NATS JetStream 作为跨实例事件总线 | ✅ 正确 | 相比 RabbitMQ/Kafka，NATS 更轻量、operator 模式成熟、consumer cursor 管理简单。at-least-once + 幂等键是合理的取舍 |
| 集群状态走 Redis sorted-set，而非进程内存 | ✅ 正确 | 进程内存 + 心跳广播在大规模集群中会爆炸（O(N²)）。Redis sorted-set + `zadd`/`zremrangebyscore` 是直播在线观众数等场景的行业标准模式 |
| HLS 切片 + WHIP/WHEP 作为直播传输 | ✅ 正确 | HLS 是兼容性最好的分发协议。WHIP/WHEP 是 IETF 标准（正在 RFC 化过程中）。str0m 是 Rust 生态中最成熟的纯 Rust WebRTC 栈 |
| AI 预算系统（per-ws + global combined，`FALLS-CLOSED on spend` + `defer`） | ✅ 正确 | 防止单一租户独占全局 AI 预算。结合 defer 策略的背压比 fail 更优 |
| 迁移编译期嵌入 | ⚠️ 权宜之计 | `sqlx::migrate!("../../migrations")` 是 sqlx 的推荐用法，但将全部 157 次迁移嵌入二进制，任何迁移修改都需要全量重新构建和部署 |
| 纯文本日志格式 | ⚠️ 开发期可接受 | JSON 日志格式应在生产环境启用。但当前的 `tracing_subscriber` 配置是纯文本，切换到 JSON 属于运行时配置变化，不应成为生产障碍 |
| 模式匹配中用 `panic!()` 处理"不可能的分支" | ❌ 不可接受的工程习惯 | `commands.rs`、`channels.rs` 等处的 panic 本质上是**用 crash 替代错误处理**。Rust 的 panic 应仅限于 `FatalError`（连接池初始化失败、配置文件缺失）。消息格式不匹配属于 `ServiceError`，而非 `FatalError` |

---

## 二、扩展方向

以下 5 个方向按优先级排序。每个方向均包含业务/技术价值、核心挑战、预期架构变更以及对现有系统的影响。

### 方向 1（P0）：仓储层连接池隔离与查询级可观测

**为什么需要**

目前 80+ 个仓储共享同一连接池。一个慢查询（如未分区消息表上的 `pg_trgm` 全表模糊搜索）可能耗尽所有连接，导致消息发送、AI 嵌入、认证等关键路径阻塞。`statement_timeout = '10s'` 的一刀切方式不足以防范级联故障。

**核心挑战**

1. **池大小配置**：按什么粒度拆分？按 crate 域（IM / Live / AI / Auth / System）分为 5–6 个池，每个池设定独立的 `max_connections` 和 `acquire_timeout`，这需要在"足够细粒度以获得隔离"与"足够粗粒度以避免连接数膨胀"之间权衡。
2. **PG 连接上限**：Postgres 有 `max_connections` 限制（默认 100）。5 个池 × 20 = 100，已达 PG 默认上限。多实例水平扩展会使问题加倍。需要配合 `PgBouncer` 或 `Odyssey` 做连接池的池。
3. **无侵入查询监测**：在不修改每个 `XRepo` 调用签名的情况下，为每个查询挂 timer 和 tracing span。sqlx 的 `PgPoolOptions` 没有直接提供查询中间件 hook。需要自定义 `ConnectOptions` 或在 `XRepo` 构造时注入 `QueryLogger` 层。

**预期的架构变更**

```
当前:
  PgPool (shared, max_conns=80)
    ├─ MessageRepo::new(pool.clone())
    ├─ AiUsageRepo::new(pool.clone())
    ├─ StreamRepo::new(pool.clone())
    └─ ...80+ more repos...

目标:
  PgPoolMap (按域):
    ├─ Pool (im, max=20) — MessageRepo, ThreadRepo, ReactionRepo...
    ├─ Pool (live, max=10) — StreamRepo, GiftRepo, ViewerRepo...
    ├─ Pool (ai, max=10) — AiUsageRepo, EmbedRepo, JobRepo...
    ├─ Pool (auth, max=10) — SessionRepo, TokenRepo, PatRepo...
    └─ Pool (system, max=5) — ConfigRepo, AuditRepo...

  QueryLogger { slow_threshold: Duration }
    └→ per-query tracing span + histogram metric
```

**对现有系统的影响**

- 每个 `XRepo::new(pool.clone())` 的调用改为 `XRepo::new(pools.im())`——接口兼容（`PgPool` 类型不变），但注入点变化
- `AppState` 需要存储 `PoolMap` 而非单个 `PgPool`
- 配置扩展：`[database.pools]` 段，每个池独立 `max_conns` 和 `acquire_timeout`
- **向后兼容**：`Pools::new_from_single_url()` 作为默认构造器，未配置分域池时回退到共享池

### 方向 2（P0）：CI/CD 管线完整化——从"手动 smoke"到"可信 pipeline"

**为什么需要**

当前 CI 仅运行 hermetic 测试（819 个）、clippy、size-check、truth-check。35+ 个数据库门控测试从未在 CI 中执行。这意味着**每次合并到 main 都可能引入数据库交互逻辑的回归**——开发者只有在本地配齐完整环境后才能发现。AGENTS.md §4.3 中要求的"提交前必过"检查（`cargo test --workspace --lib` + 完整测试）在 CI 中无法验证。

**核心挑战**

1. **CI runner 资源消耗**：运行集成测试需要 PG、Redis、NATS 三个服务。GitHub Actions 的 service container 支持这些，但启动/停止成本约 10–20 秒。
2. **测试数据隔离**：集成测试需要"创建数据库 → 运行迁移 → 插入种子数据 → 执行测试 → 删除数据库"的隔离策略。`Makefile` 中的 `migrate-smoke` 模式（使用一次性库）需要标准化为 CI 脚本。
3. **测试稳定性**：依赖外部服务的测试天然比 hermetic 测试脆弱（网络延迟、服务不可用）。需要重试逻辑和 flaky test 检测机制。

**预期的架构变更**

```
CI Pipeline 分层:

Layer 1 (每 commit, 5m):
  ├─ check (cargo check + clippy)
  ├─ test (单元测试, hermetic)
  ├─ size-check / truth-check / web-check
  └─ dependency-check

Layer 2 (main push + PR, 15m):
  ├─ integration-test (PG + Redis + NATS)
  │  ├─ cargo test -- --include-ignored
  │  └─ smoke: startup → HTTP → WS → verify
  ├─ migration-test (throwaway DB, full replay)
  └─ security-audit (cargo deny)

Layer 3 (manual trigger / release, 30m):
  ├─ benchmark (dedicated runner)
  ├─ fuzz (30 min per target)
  └─ e2e (multi-instance NATS cluster test)
```

**对现有系统的影响**

- 零代码变更——纯 CI 配置和脚本层变化
- `.github/workflows/ci.yml` 需要取消注释 + 添加 service container 定义
- `Makefile` 添加 `ci-smoke` / `ci-migrate` 目标
- 文档中 AGENTS.md §4.3（提交前必过）需要更新，将 CI 检查纳入前置条件

### 方向 3（P1）：结构化错误体系与生产路径 panic 消除

**为什么需要**

当前混合错误模式（thiserror / anyhow / ApiError String code）+ 约 15 处非测试 panic = 生产 crash 风险 + 客户端无法自动化处理错误。AGENTS.md §4.2 中的 workspace lints（clippy `all` + `pedantic` 均设为 `warn`）虽然有助于代码质量，但无法捕获运行时 panic。

**核心挑战**

1. **定义 `ErrorCode` 枚举的破坏范围**：引入 `#[non_exhaustive] ErrorCode` 枚举后，当前所有返回 `code: String` 的 API 响应需要改为 `code: ErrorCode`。现有客户端如果依赖 `code` 的字符串值，可能发生中断。
2. **每个 `panic!()` 的降级策略需要业务语义分析**：`commands.rs:394` 的 panic 表明"slash 命令的第一个 block 必须是 Text"——如果收到非 Text block，合适的降级策略是什么？跳过该消息？回退默认值？向用户返回错误消息？不同的 panic 需要不同的降级策略，无法统一替换。
3. **anyhow → thiserror 迁移**：大量路由 handler 使用 `anyhow::Result<T>` 配合 `.context()`。迁移到 `thiserror` 枚举需要逐个枚举每个 handler 可能返回的错误类型——这是中等工作量的重构，不应一次性完成。

**预期的架构变更**

```
ErrorCode 枚举:
  #[non_exhaustive]
  pub enum ErrorCode {
      ValidationError,     // 400
      Unauthorized,        // 401
      NotFound,            // 404
      RateLimited,         // 429
      InternalError,       // 500 (默认)
      DatabaseError,       // 500
      BusError,            // 502 (NATS 不可达)
      AiQuotaExceeded,     // 503
      Unknown(String),     // fallback
  }

ApiError 响应格式变化:
  当前: {"code": "rate_limited", "msg": "..."}
  目标: {"code": "RATE_LIMITED", "msg": "...",
         "retry_after": 30, "request_id": "uuid"}

Panic 消除策略:
  ┌─ FatalError ──┐  ┌─ ServiceError ─┐  ┌─ ClientError ──┐
  │ 连接池初始化   │  │ 消息格式异常   │  │ 参数校验失败   │
  │ 配置加载失败   │  │ NATS 发布失败  │  │ TOTP 验证失败  │
  │ 端口绑定失败   │  │ AI 服务不可达  │  │ 权限不足       │
  └──→ panic!()   ┘  └──→ error + fallback ┘ └──→ 4xx 响应 ┘
```

**对现有系统的影响**

- `error.rs` 需要添加 `ErrorCode` 枚举 + 修改 `ApiError.into_response()`
- `aero_common::Error` 的 `code()` 方法需要改为返回 `&'static str`（向后兼容）或 `ErrorCode`
- 非测试 panic 点需要逐个审计并替换——不影响其他代码
- `anyhow` 到 `thiserror` 是渐进式过程：每次遇到某个路由的维护需求时重构其错误类型

### 方向 4（P1）：AI 成本治理与检索质量

**为什么需要**

AI 是产品的**宣称的差异化优势**（"AI-native IM"），也是**最大的单一用户成本中心**。当前架构中，每个琐碎的问答请求都按 Sonnet 全价计费，缺乏分层路由、语义缓存和按租户的计费台账。不进行 AI 成本治理，产品的经济模型无法在规模化后成立。

**核心挑战**

1. **难度估计的准确性**：将查询分类为 easy/medium/hard 需要启发式规则（查询 token 数、领域关键词、检索命中质量）。分类错误会导致：easy 查询被发送到 haiku（答案质量下降）或 hard 查询被发送到 sonnet（成本失控）。需要校准和持续监测。
2. **缓存失效的复杂性**：语义缓存的 key 必须包含 workspace + 成员边界（防止跨租户缓存命中）。如果被引用的消息被编辑或删除，相关缓存条目必须失效——这需要挂接到现有的 `Edited`/`Deleted` 事件。
3. **预算口径对齐**：现有的 `CostBudget` 使用加权估计（Embed=1, Answer=5），而新的计费台账需记录真实 token 消耗。两者必须口径一致，避免"预算系统认为还剩 100 单位，实际计费已超支"。

**预期的架构变更**

```
现有 AI 路径:
  User Query → RAG retrieval → Anthropic Sonnet → Response

扩展后 AI 路径:
  User Query → Difficulty Estimator → Model Router
       │                              │
       ├─ easy ──────────→ Haiku ─────┤
       ├─ medium ────────→ Sonnet ────┤
       └─ hard ──────────→ Opus ──────┤
                                       │
                              Semantic Cache (workspace-scoped)
                                       │
                              ┌────────┴────────┐
                              │                 │
                         Cache hit       Cache miss → RAG + LLM
                              │                 │
                              └────────┬────────┘
                                       │
                              Usage Ledger (tenant_id, kind, micros, ts)
                                       │
                              ┌────────┴────────┐
                              │                 │
                        /billing/usage    Threshold Alerts
```

**对现有系统的影响**

- 新增难度估计器模块（可独立开关，默认将全部请求归类为 medium，保持当前行为）
- 新增语义缓存层（Redis，workspace-scoped TTL 24h）
- 新增 `ai_usage_ledger` 表 + 计费 API 端点
- 对现有 AI 路径零影响——所有新增功能为 opt-in：未配置时退化为当前行为（单一模型，无缓存，无台账）

### 方向 5（P2）：每用户持久投递台账 + 紧凑增量同步

**为什么需要**

IM 产品与创作者平台的核心承诺是：**高移动性下的无缝恢复**（蜂窝切换 / Wi-Fi 抖动 / App 后台）与**长离线追平**（离线一天的创作者返回后面对跨房间的 50k+ 条消息）。当前系统强制按房间、按设备、按重连全量回放，成本随 **房间数 × 设备数 × 重连次数** 乘积膨胀。

**核心挑战**

1. **游标推进的幂等性**：at-least-once 重新投递会携带同一 seq，LKG（Last-Known-Good）游标必须只进不退。需要使用原子 `UPDATE delivery_cursors SET last_seq = $1 WHERE participant_id = $2 AND room_id = $3 AND last_seq < $1`。
2. **多设备竞态**：同一用户的两台设备并发 ack 同一个 seq 时，以 `max(seq)` 收敛。需要 advisory lock 或原子 `WHERE last_seq < $1` 条件。
3. **与法务保全/留存清扫的交互**：被软删除的消息不应从游标中跳过——客户端收到 `deleted` 事件后更新本地状态。被法务保全保护的消息必须确保客户端即使使用游标同步也不会跳过。

**预期的架构变更**

```
新的 delivery_cursor 表:
  delivery_cursors (
    participant_id UUID NOT NULL,
    room_id UUID NOT NULL,
    last_acked_message_id UUID,
    last_seq BIGINT NOT NULL DEFAULT 0,
    last_cursor_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (participant_id, room_id)
  )

重连协议变化:
  当前: GET /api/rooms/:id/messages?since=<message_id>&limit=200
        → keyset scan, O(N)

  目标: GET /api/rooms/:id/messages?seq=<last_seq>&limit=200
        → index scan on (room_id, seq), O(log N)

增量同步帧:
  Server → Client: backfill_delta {
    room_id,
    total_available: u32,        // gap 内总消息数
    messages: Vec<Message>,       // 本次送达的消息（上限 200）
  }
  Client → Server: ack_delta {
    room_id,
    last_seq: i64,
  }

多设备共享游标:
  - 用户三台设备共享同一个 per-room LKG 游标
  - 第二台设备重连时，仅看到第一台已确认之后的新消息
  - 消灭 per-device 重复回放
```

**对现有系统的影响**

- 新增 `delivery_cursors` 迁移和仓储
- Hub 的背压机制需更新：当某用户未确认消息超过阈值（如 1000 条）时，发送 `backpressure_for_rooms` 帧
- 重连路径需要同时支持旧协议（`?since=`）和新协议（`?seq=`），以保持客户端向后兼容
- 法务保全/留存清扫逻辑需新增 `CASCADE` 处理：被保全消息软删除时，不清除 delivery_cursor 中的引用

---

## 三、接口设计建议

### 3.1 仓储层接口抽象——局部 trait 化，而非全量

**当前问题**：146 个仓储文件，每个都是 `pub struct XRepo` + `impl` block，没有统一的 trait 约束。无法 mock。

**建议**：不引入全局 `Repo` trait（那将导致 146 个方法声明在单一 trait 中膨胀）。改为**在需要单元测试的消费端进行局部 trait 化**。

| 方案 | 优点 | 缺点 |
|------|------|------|
| **A. 全量 trait 化**：`trait MessageRepo { fn get_by_id()... }` + mockall | 测试隔离最佳，依赖反转清晰 | 146 × ~5 方法 ≈ 730 个 trait 方法声明，维护成本高 |
| **B. 局部 trait 化**：仅在需要 mock 的消费端定义 `trait MessageRepo for XRepo` | 按需引入，成本可控 | trait 定义分散在各 handler 模块，阅读代码需要跳转 |
| **C. 保持现状 + 集成测试**：不引入 mock，依赖 CI 集成测试覆盖 | 零代码变更，最低维护成本 | 集成测试慢、不稳定（flaky）、无法覆盖边缘路径 |

**推荐**：**方案 B + 方案 C 混合**。对关键模块（AI 预算、消息发送、认证）使用局部 trait 化以支持单元测试；对其他模块依赖集成测试覆盖。不要仓促进行全量 trait 化——这是在"架构纯洁性"与"务实交付"之间的平衡。

### 3.2 路由装配重构——将单一巨型 build() 拆分为多个 RouterBuilder

**当前问题**：`routes.rs::build()` ~2854 行，单一函数通过 `.merge()` 组合了 100+ 个子模块路由。接近 3000 行硬上限。修改任意路由的认证策略都需要理解并修改同一个函数。

**建议**：

```rust
// 当前
pub fn build() -> Router<AppState> {
    Router::new()
        .merge(crate::auth::routes())
        .merge(crate::im::routes())
        .merge(crate::live::routes())
        // ...100+ more merges...
        .layer(middleware_1)
        .layer(middleware_2)
        // ...
}

// 建议
pub fn build() -> Router<AppState> {
    Router::new()
        .nest("/api/v1", api_v1_routes())
        .nest("/ws", ws_routes())
        .route_layer(GlobalRateLimit::new())
        .layer(GlobalMiddlewareStack::new())
}

fn api_v1_routes() -> Router<AppState> {
    Router::new()
        .nest("/auth", crate::auth::routes())
        .nest("/rooms", crate::im::routes())
        .nest("/live", crate::live::routes())
        .nest("/ai", crate::ai::routes())
        // ...grouped by domain...
        .layer(ApiMiddleware::v1())
}
```

这是纯结构性的重组，不影响路由行为或认证策略。可以分步执行（每天拆分一个域组）。

### 3.3 API 响应格式的版本兼容策略

引入 `ErrorCode` 枚举后的向后兼容策略：

**两阶段迁移**：

1. **Phase 1**（向后兼容）：API 响应同时包含 `code: String`（现有字段）和 `error_code: ErrorCode`（新字段）。客户端自动迁移到使用新字段。
2. **Phase 2**（破坏性变更）：移除 `code: String` 旧字段。

类似的兼容策略适用于 `RoomEvent` 序列化。当前使用 `tag = "kind"`，新增 variant 时需要在 web 端有对应的 `ws.on('msg:..')` 处理。建议在 `RoomEvent` 上添加 `#[serde(deny_unknown_fields)]`（当前不存在），以防止未来因字段名称冲突导致静默解析 bug——AGENTS.md §4.2 中已记录 `kind` 字段撞名陷阱。

### 3.4 连接池隔离的接口封装

```rust
#[derive(Clone)]
pub struct Pools {
    im: PgPool,    // IM 操作 (MessageRepo, ReactionRepo, etc.)
    live: PgPool,  // 直播操作 (StreamRepo, GiftRepo, etc.)
    ai: PgPool,    // AI 操作 (AiUsageRepo, EmbedRepo, etc.)
    auth: PgPool,  // 认证操作 (SessionRepo, TokenRepo, etc.)
    sys: PgPool,   // 系统操作 (migration, admin, audit)
}

impl Pools {
    // 从单一 URL 构建（向后兼容）
    pub fn new_from_single_url(url: &str, max_conns: u32) -> Self { ... }
    // 从分域配置构建
    pub fn new_from_config(cfg: &PoolConfig) -> Self { ... }
}
```

每个 `XRepo` 仍然接受 `PgPool`，但调用方从 `pools.im()` 获取对应的池。这种接口变化是透明且可逆的——如果分域池化被证明收益不大，可以回退到单池模式。

### 3.5 关键接口设计的向后兼容原则

在实施以上所有变更时，遵循以下接口设计原则：

1. **新增字段必须可选**：在枚举和结构体中新增字段时，使用 `#[serde(default)]` 确保旧客户端不会因反序列化失败而崩溃。
2. **避免破坏性重构**：任何将接受签名从 `PgPool` 改为 `PoolMap` 的变更，都应先新增重载方法，保持旧方法可用（标记为 `#[deprecated]`）。
3. **Feature flag 隔离**：每个扩展方向使用独立的 feature flag（如 `feature = "pool-isolation"`），实现渐进式部署和快速回滚。
4. **监控优先于治理**：在引入连接池隔离、错误枚举等治理措施之前，先添加可观测性（延迟直方图、错误率指标），确保变更的收益可衡量。

---

## 四、技术选型

### 4.1 不需要引入的新技术

以下技术不在建议范围内——不是因为技术不好，而是当前阶段不需要：

| 技术 | 不推荐的理由 |
|------|-------------|
| **Kafka 替代 NATS** | NATS JetStream 对当前规模足够。Kafka 的运维复杂度（ZooKeeper/KRaft、分区重平衡）不适合 16 个 crate 的项目规模。IM 场景需要的是低延迟扇出，而非 Kafka 擅长的日志聚合。 |
| **gRPC 替代 HTTP** | 当前 axum + JSON 对客户端（Web SPA 和移动端）友好。gRPC 引入 protobuf 编译 + HTTP/2 复杂性，收益有限。内部服务间通信（多实例）已经通过 NATS 解决。 |
| **Service Mesh（Istio/Linkerd）** | 单进程 Axum 网关，不需要 sidecar 代理的 mTLS/流量管理。多实例部署在 K8s 上时，Service Mesh 是合理的后续演进，但当前阶段不应引入。 |
| **Elasticsearch / Loki 作为日志平台** | 当前纯文本日志 → Loki 是有价值的，但这属于部署环境决策，而非代码层面的引入。应在引入 JSON 日志格式后由运维团队决定。 |
| **SQLite 作为开发替代** | pgvector/pg_trgm 依赖 PG 扩展，SQLite 不可替代。引入双数据库后端会增加 CI 和测试矩阵的复杂度。 |
| **OpenAPI 自动生成（utoipa）** | 当前手写 OpenAPI 文档覆盖 ~7% 的端点。引入 utoipa 需要为所有 100+ 路由添加宏标注——这是中等工作量，但收益（自动生成客户端 SDK）在当前单 SPA 客户端架构下有限。长期来看（当存在移动端时），这是值得的。 |

### 4.2 有价值的第三方引入

| 库/工具 | 用途 | 评估 |
|---------|------|------|
| **cargo-fuzz / libfuzzer-sys** | WS 帧解析 + RoomEvent 反序列化 fuzzing | ✅ 高价值。添加为 dev-dependency，不增加生产依赖。需要 nightly 工具链（仅在 fuzz CI job 中使用） |
| **criterion / divan** | 关键路径 benchmark | ✅ 高价值。dev-dependency。核心场景：Hub 扇出（100/1000/10000 人房间）、消息序列化、AI 嵌入计算 |
| **mockall** | 局部仓储 trait 的 mock 生成 | 🤔 可选。仅在采用"方案 B：局部 trait 化"时使用。替代方案：手动 mock（对 2–3 方法的小 trait 更简单） |
| **sea-query / sea-schema** | 迁移兼容性检查和 schema 比较 | 🤔 可选。替代方案：手写 `down.sql` + CI 检查 `_sqlx_migrations` 表一致性 |
| **loom** | 并发模型测试 | 🤔 可选。仅对 `hub.rs`（并发 register/unregister/fan-out）有高价值。学习曲线中等偏高 |
| **cargo-watch** | 开发体验改善 | ✅ dev-only 工具。通过 Makefile 集成，不修改 Cargo.toml |

### 4.3 自建 vs 采购的决策依据

| 功能 | 推荐 | 理由 |
|------|------|------|
| **错误代码枚举** | 自建 | 这是业务特定的（`RateLimited`、`AiQuotaExceeded` 等），不可能采购现成方案 |
| **迁移生命周期工具** | 自建 + 复用 `sqlx::migrate` | sqlx 提供了基础框架。需要包装的是兼容性标记、分阶段执行、反向迁移——这是轻量级自建（2–3 个 Rust 模块 + CLI 命令） |
| **查询级可观测** | 自建轻量包裹 | sqlx 没有提供原生查询中间件，但可以自定义 `ConnectOptions` 或包装 `PgConnection`。不需要引入 OpenTelemetry SDK 的全 SQL 解析 |
| **Grafana 仪表盘** | 手写 JSON + 版本化 | 已经做了——`monitoring/grafana/aero-red-dashboard.json` 已就位。只需要持续维护 |
| **告警路由** | 采购（PagerDuty/Opsgenie/钉钉） | 告警引擎（Prometheus Alertmanager）已有，告警路由到通知渠道属于已有商业解决方案的市场，不值得自建 |
| **SLO/告警规则定义** | 自建 | SLO 是多窗口错误率计算 + burn-rate 告警——这是 Prometheus recording rules + alert rules 的纯配置工作，现有 `monitoring/` 中已有基础，只需对齐到实际业务指标 |

---

## 五、实施路线图

### 5.1 优先级概览

| ID | 方向 | 优先级 | 估算工作量 | 风险 | ROI | 依赖 |
|----|------|--------|-----------|------|-----|------|
| A1 | CI 集成测试启封 | P0 | 2 天 | 低（纯 CI 配置） | 极高（最低成本最高收益） | 无 |
| A2 | 非测试路径 panic 消除 | P0 | 3 天 | 中（降级策略需设计） | 高（防止生产 crash） | 无 |
| A3 | 连接池隔离 + 查询级可观测 | P0 | 5 天 | 低（接口兼容） | 高（防止级联故障） | A1（验证隔离效果） |
| B1 | 结构化错误枚举 | P1 | 3 天 | 中（API 兼容性） | 中（客户端受益） | A2（消除 panic 后定义错误语义） |
| B2 | AI 成本治理 v1（分层路由 + 计费台账） | P1 | 8 天 | 中（经济模型校准） | 极高（直接影响毛利） | 无 |
| B3 | Fuzz target 建立 | P1 | 3 天 | 低（fuzz crate 不影响生产） | 中（长期质量） | 无 |
| B4 | 路由装配拆分 | P1 | 2 天 | 低（纯代码重组） | 中（可维护性） | 无 |
| C1 | 每用户投递台账 | P2 | 8 天 | 高（与法务保全/留存交互） | 高（核心 UX + 多设备） | A3（需要连接池隔离来保护同步路径） |
| C2 | 迁移分级管理 v1 | P2 | 3 天 | 低 | 中（部署安全性） | 无 |
| C3 | 反向迁移（最近 10 次） | P2 | 3 天 | 中（数据恢复策略） | 中（合规需求） | C2 |
| C4 | JSON 日志格式 | P2 | 1 天 | 低 | 中（运维效率） | 无 |

### 5.2 阶段划分

#### 阶段 1：「止血」（第 1–2 周）

**目标**：消除最紧急的生产风险和 CI 盲区。不重构，只加固。

1. **[A1] CI 集成测试启封**（2 天）
   - 取消注释 `.github/workflows/ci.yml` 中的 `integration-test` job
   - 添加 postgres/redis/nats service container 定义
   - 创建 `docker-compose.ci.yml`（轻量级，不含 jaeger/minio 等非必需服务）
   - 验证 35+ `#[ignore]` 测试在 CI 中通过
   - **交付物**：CI 配置变更 + `scripts/ci-smoke.sh`

2. **[A2] 非测试路径 panic 消除**（3 天）
   - 审计并替换以下文件中的非 fatal panic：
     - `commands.rs:394/396/443/445/463/474/534`——slash 命令解析器：格式错误时返回 `ClientError` 而非 panic
     - `channels.rs:743`——频道变更处理：格式错误时记录错误并跳过
     - `bin/main.rs:146`——启动时配置缺失：保持 fatal panic（这是正确的 panic 用法）
     - `routes.rs` 中所有非 fatal unwrap/expect
   - 每个替换点附带降级策略设计文档 + 测试用例
   - **交付物**：panic→error 替换（10–15 处），CI 中新增 `panic-free` checker

3. **[A3] 连接池隔离 v1**（5 天）
   - 实现 `Pools` struct（5 个域池：im/live/ai/auth/system）
   - 配置层支持 `[database.pools]` 段
   - `Pools::new_from_single_url()` 作为向后兼容的默认构造器
   - 逐步迁移关键路径（IM、AI）到域池，次要路径（live、auth、system）延后
   - 添加 `slow_query_duration_seconds` 直方图指标
   - **交付物**：连接池隔离 + 慢查询可观测

**里程碑**：**"CI 变绿，服务器不会因消息格式异常而 crash，关键路径有了连接级隔离"**

#### 阶段 2：「加固」（第 3–4 周）

**目标**：系统级加固——错误体系、AI 成本治理、fuzz 防护。

4. **[B1] 结构化错误枚举**（3 天）
   - `#[non_exhaustive] ErrorCode` 枚举（覆盖所有 4xx + 常见 5xx）
   - 修改 `ApiError.into_response()` 同时输出 `code: String` 和 `error_code: ErrorCode`
   - 添加 `request_id` 到所有错误响应
   - **交付物**：API 响应新增 `error_code` + `request_id` 字段

5. **[B2] AI 成本治理 v1**（8 天）
   - 难度估计器 + 模型分层路由（默认行为不变：全部分类为 medium → sonnet）
   - 语义缓存（Redis, workspace-scoped, TTL 24h）
   - `ai_usage_ledger` 表 + 每次 job 完成时记录 `(workspace_id, kind, real_cost_micros, ts)`
   - `/billing/usage` 端点（按 kind 拆分、按时间序列聚合）
   - **交付物**：AI 成本分层 + 计费台账 + 用量 API

6. **[B3] Fuzz target 建立**（3 天）
   - `fuzz/Cargo.toml`（workspace member, dev-only）
   - 3 个 fuzz target：WS frame deserialize / RoomEvent deserialize / Block deserialize
   - CI manual trigger job 运行 fuzz 30 分钟
   - **交付物**：fuzz crate + CI fuzz job

7. **[B4] 路由装配拆分**（2 天）
   - `routes.rs::build()` → 4 个 `routes_group` 函数（API v1、WS、Admin、Static）
   - `routes.rs` 从 ~2854 行降至 ~500 行
   - **交付物**：重构后的路由装配

**里程碑**：**"后端有了结构化错误码、AI 成本分层、fuzz 防护，路由装配可维护"**

#### 阶段 3：「可持续」（第 5–6 周）

**目标**：提升长期开发效率和运维效率。

8. **[C1] 每用户投递台账**（8 天）
   - `delivery_cursors` 迁移 + 仓储（唯一索引 `(participant_id, room_id)`）
   - Hub 中的游标推进逻辑（`WHERE last_seq < $1` 原子更新）
   - 重连路径新增 `?seq=` 参数（保持 `?since=` 向后兼容）
   - 多设备共享 LKG 游标
   - 与法务保全/留存清扫的交互（被保全消息不跳过游标）
   - **交付物**：增量同步 + 多设备共享游标

9. **[C2] 迁移分级管理 v1**（3 天）
   - 迁移文件命名规范扩展：`NNNN_auto_*.sql` / `NNNN_gated_*.sql`
   - `migrate()` 改为多阶段执行：先自动、再告警列出待手动迁移
   - 添加逐迁移 timing metric + tracing span
   - **交付物**：迁移分级执行 + 观测

10. **[C3] 反向迁移（最近 10 次）**（3 天）
    - 为最近 10 次迁移生成 `down/*.sql`
    - CI 检查：无 `down.sql` 的新迁移报 warning
    - 关键业务表（`messages`、`ai_usage_ledger`、`stream_gifts`）确保可回滚
    - **交付物**：反向迁移脚本 + CI 检查

11. **[C4] JSON 日志格式**（1 天）
    - 运行时开关 `AERO_LOG_FORMAT=json`
    - JSON 输出包含 `trace_id`、`span_id`、`request_id`
    - **交付物**：JSON 日志格式

**里程碑**：**"每用户增量同步可用，迁移可回滚，日志可搜索"**

### 5.3 风险点与缓解策略

| 风险 | 概率 | 影响 | 缓解策略 |
|------|------|------|----------|
| 连接池隔离导致新的配置错误 | 中 | 中 | `Pools::new_from_single_url()` 作为默认构造器，未配置分域池时回退到共享池。配置校验在启动时 Fail-fast |
| 替换 panic 时降级策略设计不当 | 中 | 高 | 每个 panic 替换前先写设计文档 + 测试：降级行为不应导致状态不一致。重点关注 `commands.rs`（slash 命令解析） |
| 集成测试不稳定（服务不稳定性） | 高 | 中 | CI 中加重试逻辑（`--retry 2`）。对持续不稳定的测试标记为 `#[ignore]` 并创建 issue 跟踪 |
| AI 成本治理导致响应延迟增加 | 中 | 高 | 语义缓存命中的 P50 延迟应 < 50ms（纯 Redis get）。难度估计器应在 10ms 内完成。对缓存/分层路由设置独立延迟直方图，超过阈值时触发降级（跳过缓存，回退单一模型） |
| 每用户投递台账与法务保存在的交互遗漏 | 中 | 高 | 在法务保全 IR 和留存清扫设计文档中明确标注 `delivery_cursor` 交互。对应事务性测试：保全消息软删除时不断游标、留存清扫软删除时推进游标 |
| Fuzz 发现大量 panic | 低 | 高 | 先运行 10 分钟 fuzz 做"烟雾测试"——如果发现 panic，优先修复。将 fuzz 作为安全网而非质量门，不阻塞 CI |
| 团队对架构变更的抵触（"这不影响功能"） | 中 | 中 | 每个方向都做 ROI 分析：这个变更在系统 X 次 crash / Y 次部署 / Z 小时调试中 recover 的成本。用数据说话 |

### 5.4 明确不做的原则

本路线图明确排除以下方向：

1. **全量 trait 化仓储层**（不做）：146 个仓储 × 每个 ~5 个方法 = 730 个 trait 方法声明，徒增维护成本。只在确需 mock 的点做局部 trait 化。

2. **Kubernetes 化部署配置**（不做）：当前是 docker-compose 单节点部署。K8s 配置是运维团队的后续工作，不应由架构分析推动。AGENTS.md §4.3 中提到的 "server 只能前台跑" 在 K8s 下需额外的 sidecar/lifecycle 处理，属于独立的工作流。

3. **多租户 SaaS 化改造**（不做）：当前假设是单租户部署。多租户涉及数据隔离策略（schema-per-tenant vs row-level security）、租户感知的限流、租户级 AI 预算——这是产品级变化，不是工程基础设施。AGENTS.md 中的 `workspace_id` 列和 `assert_room_access` 守卫为多租户提供了基础，但 SaaS 化改造本身不在本路线图范围内。

4. **移除 `anyhow` 全量替换为 `thiserror`**（不做）：这个工作量的收益在现阶段不够高。仅在新代码中推荐 `thiserror`，存量代码逐步替换。AGENTS.md §4.2 中的 CI check（`cargo clippy --workspace --all-targets` 不得新增警告）已确保新代码符合规范。

5. **Web 前端完整化**（不做为独立方向）：根据文档确认，`web/` 是 "debug client · 联调专用"，后端有约 50 个管理功能模块"有口无面"。这确实是产品缺口，但它是产品管理/前端团队的职责，不属于后端架构分析。方向 1（Bot/App 集成平台）和方向 5（每用户投递台账）为前端补全提供了基础设施支持。

6. **架构重构中途叠加新功能**（不做）：AGENTS.md §4.2 明确禁止"在另一次重构中途（git status 有大量在途删除）叠加结构迁移"。分区切换、双写迁移、路由拆分等变更绝不叠加执行——每个变更独立 PR、独立测试、独立验证。

---

## 总结：关键洞察

Aero IM 的架构在**事件驱动骨干**、**crate 分层**、**AI 预算系统**和**监控基础设施**方面展现了远超普通项目的成熟度。架构债主要集中在**基础设施侧**（迁移管理、连接池隔离、CI 盲区、错误处理混合模式），而非**领域逻辑侧**。这是一个好信号——核心业务逻辑（消息投递、AI 检索、直播流处理）的设计是健康的，需要加固的是承载这些逻辑的底座。

**建议的按优先级行动顺序**：

1. 本周末：CI 集成测试启封（2 天，成本最低收益最高）
2. 下周：消除非测试 panic + 连接池隔离（8 天，防止生产故障）
3. 下月：AI 成本治理 + 结构化错误枚举 + fuzz（14 天，控制成本 + 完善 API 契约）
4. 两个月内：每用户投递台账（8 天，核心 IM UX 提升）
5. 持续：迁移分级管理 + 反向迁移 + JSON 日志（7 天，长期可维护性）

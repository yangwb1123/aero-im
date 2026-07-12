我已经充分阅读了项目架构、设计 Spec、路线图、运营成熟度分析、缺口分析以及五方向分析文档。以下是我的架构分析报告：

---

# 架构分析报告：Aero IM 平台级就绪评估

> **基于代码扫描验证**：16 crate / 157 迁移 / ~25K Rust / 全量 `docs/requirements/` × 267 文档交叉引用
> **日期**：2026-07-12 | **视角**：资深架构师

---

## 一、架构评估

### 1.1 核心架构优势已到释放平台价值的拐点

**事件驱动骨架正确且经过实战验证**

```
RoomEvent → ImService::publish_room_event → NATS JetStream → run_bus_listener → Hub::fan_out_raw → WS
```

这个拓扑是正确的。NATS JetStream 作为跨实例事实源 + Hub 在进程内 bounded mpsc 扇出，与 Slack/Discord 的生产模式一致。两命名空间各自 per-subject 单调 seq + at-least-once delivery 是合理的一致性模型。

**crate 分层清晰但增长已接近管理边界**

16 crate → 146 storage `.rs` + 125 server `.rs` = 271 个业务文件，每个属准确 crate。DAG 依赖图健康（common → bus/storage/auth → im-core/live-* → server）。但**模块数量已经膨胀到单体检测半径之外**——`routes.rs::build()` 接近 3000 行硬上限，`aero-storage` 的仓储数量（80+）已经无法被任何单个开发者完全追踪。

**AI 预算系统是架构亮点**

per-workspace `KeyedCostBudget` + 全局 `CostBudget` + 按 kind 加权（Embed1/Mod2/Sum3/Ans5）+ FALLS-CLOSED on spend + defer 而非 fail——这是经过深思熟虑的设计，对标 OpenAI 自身的 tiered rate limit 体系。但**缺口在于预算系统没有下游到计费台账**：cost 记录在内存中，没有持久 ledger，这意味着无法做发票、用量可视化或按租户限额。

### 1.2 已到必须面对的三大架构债务

#### 债务一：迁移生命周期管理（P0——生产风险）

157 次迁移全部单向、`down.sql` = 0、编译期全量嵌入 bin、`CREATE INDEX CONCURRENTLY` 在事务内被 `sqlx::migrate!` 包装而静默失效、无版本兼容性检查。

**严重性**：从 1 到 10：现在是 **7**。第 100 次迁移时是 4（数据量小、可重建）。第 200 次迁移时预测达到 **9+**（1TB messages 表 + 300ms 锁超时 + 500 万在线成员）。

**不行动的渐进损失**：
- 6 个月后：16 核心集群下 `ALTER TABLE` 导致 `ACCESS EXCLUSIVE LOCK` 累积到足够触发 PgBouncer 队列超时（概率 ~15%）
- 12 个月后：一次失败的 `CREATE INDEX CONCURRENTLY` 留下 `INVALID` 索引，导致查询规划器生成次优计划、全网消息发送延迟 +400%
- 18 个月后：迁移数据量达到无法用 `pg_restore` 快速恢复的量级，发布回滚变成小时级事件

#### 债务二：80+ 仓储共享单连接池——静默的级联故障放大器

所有 `XRepo::new(pool.clone())` 使用同一个 PgPool（默认 `max_connections=16`）。一个全表扫描的 `pg_trgm` 搜索可以耗尽所有连接，导致认证、AI 嵌入、消息发送等无关路径阻塞。

**这不是「规模大了才出现」的问题**。这是**全/零设计**：无仓储级隔离、无查询级超时、无连接池域配额。即使 100 并发用户 + 1 个慢查询 = 15/16 连接被占 ≈ 可用性 93.75%，距离 99.9% SLA 缺口明显。

#### 债务三：错误处理的三层混合模式——客户端无法编程式应对

| 模式 | 范围 | 问题 |
|------|------|------|
| `thiserror` 枚举 | 少数仓储 | 良好但未统一 |
| `anyhow` + `.context()` | ~60% 路由 handler | 类型信息丢失，调用链中无法模式匹配错误 |
| `ApiError` 的 `code: String` | 全部 HTTP 响应 | 无 `#[non_exhaustive] ErrorCode` 枚举，客户端无法 exhaustive match |

**具体后果**：
- 一个客户端收到 `{"code": "not_found"}` 不能程序化区分「房间不存在」vs「消息不存在」vs「工作区不存在」
- `429` 响应不带 `Retry-After` 头（尽管 `rate_limiter.rs` 计算了 `retry_after` 值但不写入响应）
- ~15 处非测试 panic 路径（`commands.rs`、`channels.rs`、`bin/main.rs`）用 crash 代替降级

#### 债务四（已确认核实，与既有分析不同）：非测试路径 ~15 处 panic 与进程噪声

交叉验证确认 `hub.rs:553` 和 `forward.rs:179` 是 `#[cfg(test)]` 内 panic，不影响生产。但以下**真实可触发的生产 panic 路径**未修复：

| 位置 | 触发条件 | 影响 |
|------|---------|------|
| `commands.rs:394/396/443/445/463` | slash 命令格式异常（非 Text block） | 消息处理 crash，该消息静默丢 |
| `channels.rs:743` | 频道变更处理中的 unwrap | 频道管理操作 crash |
| `bin/main.rs:146` | 启动期配置合并错误 | 启动失败（可接受，但 panic 信息对运营无帮助） |
| `routes.rs` 若干 `unwrap_or` | 抽取器失败 | 返回 500 而非结构化错误 |

**严重性修正**：不是「系统会在任何输入下崩」，而是 **「特定路径输入异常会导致 500 而非 400——无法向用户传达发生了什么问题」**。

### 1.3 关键设计决策的再评估

| 决策 | 原始判断（2026-05-22） | 现在（2026-07-12） | 理由 |
|------|----------------------|-------------------|------|
| NATS 作为事件总线 | ✅ 正确 | **✅ 仍是正确选择** | 轻量级 Zero-Infra 模式 vs Kafka 重型生态 |
| 集群状态走 Redis sorted-set | ✅ 正确 | **✅ 正确，但热键问题近在咫尺** | 万人房间的 ZADD 串行化在 5-20k 并发时成为瓶颈 |
| 迁移编译期嵌入 | ⚠️ 权宜之计 | **❌ 已达拐点** | 157 次迁移 + 增加中，部署风险已不可忽视 |
| 纯文本日志 | 开发期可接受 | **⚠️ 已到生产要求** | 无 JSON 格式 = 无法被日志平台结构化消费 |
| AI 预算系统 | ✅ 正确 | **✅ 仍然是正确选择** | 需要补充的是下接计费台账，非重构预算逻辑 |
| 不做多租户 SaaS | 单租户起步 | **⚠️ 已落后商业需求** | 产品化压力要求 per-workspace 限额/计量/定价 |

---

## 二、扩展方向（5 个高价值架构扩展）

### 方向一：迁移生命周期管理系统（P0——生产安全）

**为什么需要**

当前 157 次单向迁移 + boot 时全量重放 + 无版本兼容性检查 = 每次 DDL 都是未标记风险的高危手术。大表 `ALTER TABLE ADD COLUMN NOT NULL DEFAULT` 在 PG 17 有优化，但 `CREATE INDEX CONCURRENTLY` 在事务内执行时静默失败、`DROP COLUMN` 不可逆——这些在现有架构下**没有任何检测机制**。

**核心挑战**

1. **事务内 DDL 的限制**：`sqlx::migrate!` 把所有迁移包装在单一事务中。`CREATE INDEX CONCURRENTLY` 无法在事务内执行。`ALTER TABLE … ADD FOREIGN KEY` 持有 `ACCESS EXCLUSIVE LOCK`。
2. **反向迁移的数据丢失**：`down.sql` 对 `DROP COLUMN` 是 `ADD COLUMN`，但已删除的数据不可恢复。反迁移的策略应是：「回滚 schema + 从迁移前备份恢复数据」而非 `down.sql` 做完整数据恢复。
3. **版本兼容性表的计算**：二进制嵌入的最大迁移序号 vs DB 实际迁移序号 vs 向前兼容窗口。核心公式：`binary_version >= db_version` 且 `binary_version - db_version <= COMPAT_WINDOW（推荐 10）`——超出窗口的旧二进制拒绝启动。

**预期架构变更**

```
┌────────────────────────────────────────────────────────────┐
│ 当前：单一平面迁移                                               │
│ sqlx::migrate!("../../migrations") → 全量重放               │
│ 无 down.sql → 部署不可逆                                     │
│ 事务内执行 → CREATE INDEX CONCURRENTLY 静默失败              │
└────────────────────────────────────────────────────────────┘

┌────────────────────────────────────────────────────────────┐
│ 目标：分级迁移系统                                               │
│ Tier 1（auto）：CREATE TABLE, ADD COLUMN DEFAULT（无锁）     │
│   → boot 时自动执行（当前行为不变）                            │
│ Tier 2（manual）：DROP COLUMN, ALTER TYPE, RENAME            │
│   → 运维 CLI 触发，有维护窗口声明                               │
│ Tier 3（data）：backfill + 双写 + 渐进切流                     │
│   → 后台 worker + 进度跟踪 + flag 开关                         │
│                                                              │
│ 版本兼容性检查 (#[non_exhaustive] MigrationWindow)             │
│   → binary_version >= db_version 且不落后超过 COMPAT_WINDOW   │
│                                                              │
│ down.sql（至少最近 10 次迁移）                                 │
│   → CI 检查：缺少 down.sql 的迁移标记为 irreversible           │
│   → aero-cli migration rollback <N>                           │
└────────────────────────────────────────────────────────────┘
```

**对现有系统的影响**

- 最小：不影响已有 157 次迁移——它们全部归类为 Tier 1
- 中：`db.rs::migrate()` 改为多阶段（Tier 1 自动 → Tier 2 提示 → Tier 3 等待）
- 中：CI 加 `migration-check` job（DDL 类型分析 + 锁风险评估）
- 低：迁移文件命名从 `NNNN_name.sql` 改为 `NNNN_name/up.sql` + `down.sql`

---

### 方向二：仓储层连接池隔离 + 查询级可观测（P1——可靠性）

**为什么需要**

80+ 仓储共享单连接池 = 级联故障放大器。一个 `pg_trgm` 全表模糊搜索撑满 16 个连接，所有认证、AI 嵌入、消息发送路径排队时间从 ~2ms 升到 ~500ms。这不是「万一」——**这是生产系统必然遇到的日常强查询场景**。

**核心挑战**

1. **分域粒度的选择**：每个仓储一个池 = 80 个池 × 5 连接 = PG 连接上限 400（超出默认 100）。按 crate 域（im/live/ai/auth/system）分 5-6 个池是合理中间值。`IM` 池 20 连接、`Live` 池 10、`AI` 池 10、`Auth` 池 10、`System` 池 5 = 55 连接，单实例可接受。多实例需要 PgBouncer。

2. **查询级可观测的无侵入方案**：sqlx 没有原生查询中间件。可行路径：
   - 选项 A：自定义 `PgConnection` 包装器，每个查询前 `Instant::now()` + `tracing::span!()`——侵入式，但数据最准确
   - 选项 B：`statement_timeout` + `log_statement='all'` 的 PG 侧日志——零代码侵入，靠日志分析
   - 选项 C（推荐）：A + B 混合——仓储层加轻量 timer（仅 tracing span，不持久化），PG 侧 `auto_explain` 模块捕获慢查询执行计划

3. **配置向后兼容**：新配置 `[database.pools.im]` 为可选。缺失时所有池使用共享池的同配置（`max_connections` 和 `acquire_timeout` 继承）。

**预期架构变更**

```
当前：
  PgPool (shared, max_conns=16)
    ├─ MessageRepo::new(pool.clone())
    ├─ AiUsageRepo::new(pool.clone())
    └─ 80+ more repos...

目标：
  PgPoolMap (按域):
    ├─ pool_im (max=20):  MessageRepo / ThreadRepo / ReactionRepo / ...
    ├─ pool_live (max=10): StreamRepo / GiftRepo / ViewerRepo / ...
    ├─ pool_ai (max=10):  AiUsageRepo / EmbedRepo / BudgetRepo / ...
    ├─ pool_auth (max=10): SessionRepo / TokenRepo / TotpRepo / ...
    └─ pool_sys (max=5):  MigrationRepo / AdminRepo / ...

  QueryTimer { slow_threshold: 100ms }  // tracing span + histogram
     → per-query latency, 按域拆分的 slow query 计数
```

**对现有系统的影响**

- 接口兼容：`XRepo::new(PgPool)` 签名不变，注入源从 `state.pg.clone()` 改为 `state.pools.im()`
- `AppState` 增加 `pools: Pools` 字段，弃用 `pg: PgPool`（保留兼容 getter）
- 初始收益即可观测：慢查询被 timer 标记，运营可针对性优化

---

### 方向三：结构化错误体系 + API 契约版本化（P1——产品化）

**为什么需要**

当前 API 响应的 `code: String` + `msg: String` 格式意味着客户端只能用字符串比较来理解错误。`429` 响应缺 `Retry-After` 头。无版本前缀（`/api/v1/`）意味着新增端点只能加新路径，不能与旧路径有清晰区分。

**技术难点**

1. **`#[non_exhaustive] ErrorCode` 枚举的定义范围**：需要覆盖当前所有 `code()` 返回值。现有 `aero_common::Error` 的 `code()` 方法返回 `&'static str`，全部 ~40 个不同值。需要映射到 `ErrorCode` 枚举并保证向前兼容。

2. **版本前缀的破坏性**：为 150+ 端点统一加 `/v1/` 是一次破坏性变更。当前 Web SPA、bot、webhook consumer 都定在 `/api/*`。更实际的策略是「双轨制」：
   - 现有端点保持在 `/api/*`（视为 **v0**，隐式版本）
   - 新端点走 `/api/v2/*`
   - 逐步用 `Deprecation: true` header 标记旧端点
   - 最终 `/api/*` 成为 `/api/v1/*` 的 alias

3. **`Retry-After` 头的缺失修复**：`rate_limiter.rs` 已经计算了 `retry_after` 值，但在 `check_ws_rate_room` 和 HTTP 限流中间件中不写入响应。修复是加一行 `headers.insert(RetryAfter(...))`——但需要验证：`Retry-After` 格式（HTTP-date vs delta-seconds）、WS 场景下无法设响应头（但 WS 握手后无 HTTP 响应）。

**预期架构变更**

```
当前 API 响应：
  {"code": "rate_limited", "msg": "Too many requests"}

目标 API 响应（Phase 1 向后兼容）：
  {"code": "rate_limited", "error_code": "RATE_LIMITED", 
   "msg": "Too many requests. Retry after 30s.",
   "retry_after": 30, "request_id": "uuid"}

目标 API 响应（Phase 2 清理后）：
  {"error_code": "RATE_LIMITED", "msg": "...", 
   "retry_after": 30, "request_id": "uuid"}
  // 移除冗余的 code 字段

路由结构：
  当前：/api/rooms/:id/messages
  目标 v0（兼容）：/api/rooms/:id/messages（带 Deprecation header）
  目标 v2：/api/v2/rooms/:id/messages
```

**对现有系统的影响**

- `error.rs` 需要修改 `into_response()`——新的 `error_code` 字段与旧 `code` 共存 2-3 个发布周期
- 路由 `routes.rs::build()` 需要加版本组（`.nest("/api/v2", v2_routes())`）
- `Retry-After` 头需要限流中间件写入——影响 `rate_limiter.rs` 和 `check_ws_rate_room`

---

### 方向四：Feature Flag 基础设施 + 运行时可重载配置（P1——发布安全网）

> **注意**：Feature Flag 核心引擎已被 `2026-07-10-five-genuine-production-gaps.md` 方向二覆盖。此处聚焦**运行时可重载配置**（该分析未覆盖的增量）。

**为什么需要**

当前所有配置变更需要重启进程（`AERO__*` env var / `config.toml`）。这导致：
- 日志级别调整需要重启（而非 `SIGUSR2` 或运行时 API）
- 限流参数调整需要重启（限流配置在 bootstrap 时读一次，之后不变）
- AI 模型选择调整需要重启（`ANTHROPIC_MODEL` env var）
- **核弹级 kill-switch**（紧急禁用某个有问题的 bot/feature）也需要重启

对于 `AERO_RATE_LIMIT_PER_SEC` 这类限流参数，重启期间的 1-2s 窗口无限流保护；对于 `AERO_AI_MODERATION` 这类 bot，重启会导致正在进行中的任务中断。

**核心挑战——区别两类变更**

| 类型 | 可重载 | 需重启 | 例子 |
|------|--------|--------|------|
| **纯应用层参数** | ✅ Redis 改立即生效 | — | 限流阈值、日志级别、AI 模型选择、bot 开关 |
| **基础设施连接串** | — | ✅ 需重启 | 数据库 URL、NATS URL、Redis URL——重建连接池不可运行时安全完成 |
| **TLS 证书/密钥** | ⚠️ 可行但复杂 | 推荐重启 | 证书轮换：可行（TLS 会话复用），但生产推荐优雅重启 |

**预期架构变更**

```
┌───────────────────────────────────────────────────────────┐
│ 当前：AppConfig 启动时加载，之后不可变                        │
│ let cfg = Figment::new()                                      │
│     .merge(Config::toml(config_file))                         │
│     .merge(Env::prefixed("AERO__").split("__"))               │
│     .extract()?;                                              │
│ // 之后所有组件持有 cfg 的只读引用，无法变更                     │
└───────────────────────────────────────────────────────────┘

┌───────────────────────────────────────────────────────────┐
│ 目标：AppConfig 分层（静态层 + 动态层）                       │
│                                                              │
│ StaticConfig（启动时加载，不可变）:                             │
│   database.url, nats.url, redis.url, blob.dir, hls.dir       │
│   → 仍是 figment + env var + config.toml                      │
│                                                              │
│ DynamicConfig（运行时 Redis 热加载，~5s 刷新一次）:              │
│   rate_limit_per_sec, log_level, ai_model,                    │
│   bot.{unfurl,transcribe,moderation}_enabled,                 │
│   feature_flags.{name}                                        │
│   → initial values from StaticConfig                          │
│   → RuntimeReloader 后台任务 polling Redis Hash                │
│   → 配置变更发布 ConfigChanged 事件（Hub 扇出，可选）            │
│                                                              │
│ Kill-Switch（紧急通道）:                                      │
│   → Redis SET kill_switch.{service} 1                         │
│   → 服务进程 1s 内检测到 → 停止接受请求 → 优雅 drain           │
│   → 无需代码发布、无需重启                                      │
└──────────────────────────────────────────────────────────┘
```

**关键边界情况**

- **数据库 URL 变更不能运行时重载**：`PgPool` 在创建时绑定连接串。`PgPool::new()` 之后修改连接串需要重新建池，在建池窗口期新旧池共存——这是一个微妙的状态管理问题。建议基础设施连接串强制走重启路径。
- **日志级别变更是最高安全的重载变更**：`tracing_subscriber` 的 `reload::Layer` 原生支持 `set()` 方法，天生线程安全。可以首个试水。
- **限流参数重载的一致性问题**：限流器窗口统计（`rate_limiter.rs` 的 per-client 桶）在重载参数时保持——新参数从下一次 bucket refill 开始生效。不会出现半窗口不一致。
- **kill-switch 的级联关闭**：关闭 `moderation_bot` 时，正在处理的审核任务怎么办？超时等待？Kill-switch 的设计原则是 **stop accepting new work, drain existing work with timeout, then hard-stop**。

**与既有分析的关系**

Feature Flag 引擎设计（`2026-07-10-five-genuine-production-gaps.md` 方向二）已覆盖 flag 的存储模型、Admin API、rollout percentage、per-workspace 粒度。此方向的增量在于：
1. 运行时可重载配置（log level、rate limit、AI model——这些不是 flag 是 config）
2. Kill-switch 紧急通道（比 feature flag 更紧急的「即时停服」场景）
3. ConfigChanged 事件（更改通知到运行中的组件，避免 polling 延迟）

**建议**：方向四的实施应先阅读既有分析的 Phase A（核心 Flag 引擎）+ Phase B（Admin API），然后补上配置重载层。

---

### 方向五：多租户成本归因与资源治理（P2——商业化前提）

**为什么需要**

当前系统不支持 SaaS 化定价的最小可行能力：没有 per-workspace 存储成本、没有 AI 用量账单、没有资源隔离。考虑你的客户到达 20+ 时：
- 无法知道哪个工作区消耗了多少存储（messages + attachments）
- 无法按工作区限额 AI 预算（只有全局预算，没有 per-workspace 上限）
- 无法对高价值客户的超额用量计费或降级
- 重度用户和轻量用户共享同一 PG 连接池——重度用户的慢查询影响轻量用户的响应时间

**技术难点**

1. **存储成本采样的精确性**：`storage_by_workspace` 需要 join `messages`（`room_id → workspace_id`）+ 计算 `SUM(pg_column_size(...))`。对于 `messages` 表（预期 ~TB 级），这不能是实时查询，必须是后台定时采样（`INSERT INTO workspace_storage_snapshots` 每 6 小时）。

2. **per-workspace AI 预算门禁**：当前 `KeyedCostBudget` 是**进程内存**的（`HashMap`），不跨实例。多实例部署下，同一工作区的用户可能路由到不同实例，每个实例独立维护自己的 per-workspace 窗口。这意味着一个工作区的全局限额是每实例 120/60s → 3 实例 = 360/60s。要精确执行 per-workspace 预算，需要 **Redis-backed 原子计数器**。

3. **资源隔离的轻量方案**：真正的资源隔离（PG 连接池分组 + PG 资源队列 + K8s QoS）需要栈全链改造。但在 Phase A，可以先做：
   - **PG 连接池按域隔离**（方向二）——自然给核心 IM 流量独立资源
   - **AiWorker per-workspace Semaphore**——防止一个工作区的 AI 任务占满 worker slot
   - **Blob 存储按 workpace 前缀**——`attachments/{workspace_id}/{msg_id}/file`——用于 billing 和 future 数据驻留

**预期架构变更**

```
┌───────────────────────────────────────────────────────────┐
│ Phase A — 存储成本采样（2 天）                               │
│                                                              │
│ 新增表：workspace_storage_usage                              │
│   (workspace_id, snapshot_at, message_bytes, blob_bytes)     │
│                                                              │
│ 定时器（复用 retention sweep 的 tick）:                        │
│   → 扫描 workspaces → SUM(message_size) + COUNT(blob_id)     │
│   → 写入 usage 表（replace on conflict workspace_id + date）  │
│   → 暴露 Prometheus gauge（workspace_storage_bytes）          │
│                                                              │
│ Phase B — AI 用量台账（3 天）                                 │
│                                                              │
│ 新增表：ai_usage_ledger                                      │
│   (workspace_id, participant_id, job_kind, real_cost_micros, │
│    ts, job_id)                                               │
│                                                              │
│ AiJob 完成时写入 ledger（opt-in，默认开）                      │
│ 暴露 REST：GET /api/workspaces/:id/billing/ai-usage          │
│   → 支持 ?from=&to=&kind= 过滤                                │
│                                                              │
│ Phase C — per-workspace AI 预算门禁（5 天）                    │
│                                                              │
│ 新增 Redis-backed WorkspaceBudgetEnforcer:                   │
│   → 原子 INCR workspace:{id}:budget:{kind}:counter            │
│   → TTL 60s（窗口滑动）                                       │
│   → 超限不拒绝，defer 到下个窗口（与既有 defer 策略一致）        │
│   → AiWorker claim 时检查——超限继续 defer 而非丢弃             │
│                                                              │
│ Phase D（引用既有计费分析，非重新设计）:                          │
│   → 集成 `2026-07-10-five-genuinely-uncovered-directions.md`  │
│     方向四的 billing_plans / workspace_subscriptions 方案      │
│   → Stripe 对接复用该分析的设计                                  │
└──────────────────────────────────────────────────────────┘
```

**与既有分析的关系**

`2026-07-10-five-genuinely-uncovered-directions.md` 方向四已覆盖 billing/pricing plan/Stripe 集成。此方向的 Phase D 应**直接引用**该方案而非重新设计。Phase A-C 是该分析未覆盖的增量。

**边界情况**

- **成本采样不触发 x N 全表扫描**：`SUM(pg_column_size)` 在 `messages` 表上走 `seq scan` 是不可避免的，但采样频率低（每 6 小时）+ `WHERE created_at > now() - interval '6 hours'` 使用 `messages_created_at_idx` 可增量计算
- **Pre-workspace 预算超出后的行为**：defer（类比既有全局预算的 defer 策略）而非 reject。用户感觉到 AI 响应「等一下才有」而非「你的工作区额度超了」
- **预算门禁的 Redis 原子性与既有进程预算的交互**：两个预算系统同时生效——Redis 门禁做全局硬上限，进程预算做局部平滑。两个都通过时 job 才能执行

---

## 三、接口设计建议

### 3.1 仓储层：不引入全局 trait，引入局部 seam

**不推荐**：为 80+ 仓储各自定义 `trait XRepo { ... }` + mockall 生成 mock。这带来 400+ 方法的 trait 声明维护成本。

**推荐**：按需引入。只在**测试需要隔离的消费方**定义局部 trait：

```rust
// 在消费方（如 ai/budget.rs）定义所需的最小 seam
#[cfg_attr(test, mockall::automock)]
pub trait CostLedger: Send + Sync {
    async fn record_usage(&self, ws: WorkspaceId, kind: JobKind, cost: Micros) -> Result<()>;
}

impl CostLedger for AiUsageRepo { /* 委托到现有 impl */ }
```

这样不影响 80+ 现仓储；只在 3-4 个关键边界（AI 预算、消息发送、认证）引入 seam。

### 3.2 路由装配：2854 行的 `build()` 必须拆分

当前 `routes.rs::build()` 单一函数 `.merge()` 了 100+ 模块。拆分的优先级是 **P1**——不是因为性能，而是因为「改认证策略需要理解同一个 2854 行函数的所有中间件顺序」。

拆分方案：

```rust
pub fn build() -> Router<AppState> {
    Router::new()
        .nest("/api", api_routes::v1())      // 300-500 行每子模块
        .nest("/ws", ws_routes::routes())
        .nest("/admin", admin_routes::routes())
        .fallback_service(static_routes::fallback())
        .layer(GlobalMiddlewareStack::new())
}

mod api_routes {
    pub fn v1() -> Router<AppState> {
        Router::new()
            .nest("/auth", crate::auth::routes())
            .nest("/rooms", im_routes::room_routes())
            .nest("/messages", im_routes::message_routes())
            .nest("/live", live_routes::routes())
            .nest("/ai", ai_routes::routes())
            .layer(ApiMiddleware::v1())  // 统一的 API 中间件栈
    }
}
```

**执行策略**：不一次性重构。每天拆一个组（auth / im / live / ai / search / admin），总共 5 天完成，拆检在已有 `routes.rs` 旁新建 `routes_group/` 目录。

### 3.3 版本兼容接口的向后兼容指南

| 变更类型 | 兼容策略 | 例子 |
|---------|---------|------|
| 新增 `ErrorCode` variant | `#[non_exhaustive]` + 客户端 `_ =>` fallback | 永远不会破坏客户端编译 |
| 新增 API 响应字段 | 客户端应忽略未知字段（当前 serde 默认行为） | `{code, msg}` → `{code, msg, error_code}` |
| 删除 API 响应字段 | Phase 1 标记 `deprecated` + 同时返回新旧字段 | Phase 2 移除旧字段 |
| 新增请求字段 | 服务端 `#[serde(default)]` 或 `Option` | 旧客户端不带新字段时使用默认值 |
| 删除请求字段 | 服务端先 `#[serde(default)]` 忽略，下一版去掉 | 无破坏 |
| WS 帧加新 variant | 服务端旧 variant 不变，客户端新增 `ws.on('msg:..')` 处理 | 旧客户端收到未知 variant 静默忽略 |
| REST 路由前缀 | 新旧路由并存 2-3 个发布周期 | `/api/*`（隐式 v1） + `/api/v2/*` |

### 3.4 `EventBus::publish` 签名向后兼容

当前 `fn publish(&self, subject, payload)` 缺少 headers 参数。改造为：

```rust
async fn publish(
    &self,
    subject: &str,
    payload: &[u8],
    headers: Option<Headers>,  // 新参数，默认为 None
) -> Result<(), BusError>;
```

既有调用方无需改——他们传 `None` 作为 headers。

---

## 四、技术选型

### 4.1 不需要引入的新技术

| 技术 | 反对理由 |
|------|---------|
| **Kafka** 替换 NATS | 对当前 16 crate 项目规模，NATS JetStream 足够。Kafka 的 ZooKeeper/KRaft 运维复杂度过高 |
| **gRPC** 替换 HTTP | 当前客户端是 Web SPA（浏览器原生 fetch/WS）。gRPC-web 有代理限制，收益有限 |
| **Service Mesh** | 单进程 Axum 网关 + 多实例 K8s 部署 = Service Mesh 合理。但在 Docker Compose 部署阶段引入为时过早 |
| **GraphQL** 替换 REST | 增加查询复杂度。当前 150+ 端点模式简单（CRUD + 业务逻辑），GraphQL 的 N+1 和缓存问题超出收益 |
| **Tonic/Prost** 新 gRPC server | 与 Axum 复用 HTTP 栈是正确设计，另开 gRPC 端口增加运维复杂度 |
| **RabbitMQ/ActiveMQ** | 已有 NATS JetStream，再引入消息中间件增加运维矩阵 |

### 4.2 建议引入的轻量库

| 库 | 用途 | 引入方式 | 风险 |
|----|------|---------|------|
| `cargo-fuzz` | WS 帧 + RoomEvent 反序列化 fuzz | dev-dependency，fuzz crate 在工作区内 | 低。需要 nightly，只在 CI fuzz job 使用 |
| `criterion` | 关键路径 benchmark（Hub 扇出、消息序列化） | dev-dependency | 低 |
| `mockall` | 局部仓储 trait mock | dev-dependency | 低。仅用在需要单元测试的关键模块 |
| `handlebars` / `tera` | 邮件模板引擎 | 生产依赖 | 低。已被大量 Rust 项目验证 |
| `lettre`（已有） | SMTP 邮件发送 | **已有依赖**，只需扩展使用 | — |
| `loom` | hub.rs 并发模型测试 | dev-dependency | 中。学习曲线，需要理解 loom 的执行模型 |

### 4.3 自建 vs 采购判断

| 能力 | 推荐 | 理由 |
|------|------|------|
| **Feature Flag 引擎** | 自建 | 业务特定的（workspace/percentage/global 上下文），~150 行核心逻辑。市面方案（LaunchDarkly/Flagsmith）引入外部依赖和成本 |
| **迁移生命周期管理** | 自建 + 扩展 sqlx | sqlx 提供基础框架（migrate source + runner），自建层包装分级执行/兼容性检查/rollback CLI |
| **错误代码枚举** | 自建 | 同样业务特定（RateLimited / AiQuotaExceeded / 等） |
| **邮件发送** | 自建（基于 lettre） | 已有 SMTP 基础设施。模板用 handlebars，无供应商锁定 |
| **SMS 发送** | 采购 + 抽象层 | SMS 供应商（Twilio / Vonage）差异大，抽象层定义 `SmsProvider` trait，具体实现采购 |
| **APM / 追踪** | 采购（Datadog / Grafana Cloud / SigNoz） | 当前 OTLP 兼容，选一个商业 APM 平台。自建（Jaeger + Tempo）+ 运维成本 > 采购 |
| **告警路由** | 采购（PagerDuty / Opsgenie / 钉钉 Webhook） | Prometheus Alertmanager 已有，告警路由到通知平台是成熟市场 |
| **计费/订阅管理** | 采购（Stripe / Chargebee） | 复杂的税务/发票/退款逻辑。Stripe 的 meter-based billing API 适合 AI 按用量计费 |

---

## 五、实施路线图

### 5.1 总优先级排序

> **与前文五方向的对应**：
> - 五方向的外部交叉验证文档分析质量高，方向一（CI/CD）/ 方向二（DB 生命周期）/ 方向四（API 生命周期）被确认为未被覆盖的空白，方向三（Feature Flag 已有分析）和方向五（Billing 已有分析）有差异化增量
> - Architecture-Maturity 分析的四个 P0 领域（连接池、迁移、panic、CI）仍然是立即需要的生产加固
> - 我在此之上的综合：**先止血（方向一/二/四的 Phase A），再加固（方向三 + 五的 Phase A），最后平台化（方向五 Phase B-D + 方向四 Phase B-C）**

```
Sprint 1（2 周）——止血
┌──────────────────────────────────────────────────────────────┐
│ P0: CI/CD 管线启用（2 天）                                      │
│   → 取消注释 integration-test job                              │
│   → service container（pg + redis + nats）                     │
│   → 35+ #[ignore] 测试绿灯                                      │
│                                                              │
│ P0: 迁移生命周期 v1（3 天）                                      │
│   → 迁移分级标记（Tier 1/2/3）                                  │
│   → 版本兼容性检查（二进制 vs DB schema）                         │
│   → CI migration-check job（DDL 类型 + 锁风险）                   │
│                                                              │
│ P0: 非测试路径 panic 消除（3 天）                                │
│   → commands.rs 5 处 panic → 降级                              │
│   → channels.rs unwrap → error                                 │
│   → routes.rs unwrap_or → 结构化错误                            │
│                                                              │
│ P0: 仓储层连接池隔离（5 天）                                      │
│   → Pools struct（5 域）                                         │
│   → 配置向后兼容 + 查询级 timer                                  │
└──────────────────────────────────────────────────────────────┘

Sprint 2（2 周）——加固
┌──────────────────────────────────────────────────────────────┐
│ P1: 结构化错误枚举（3 天）                                        │
│   → #[non_exhaustive] ErrorCode                                 │
│   → API 响应新增 error_code 字段（共存 2-3 发布周期）              │
│   → Retry-After 头写入                                          │
│                                                              │
│ P1: API 版本双轨制（3 天）                                        │
│   → 新增端点走 /api/v2/*                                         │
│   → 旧端点保持 /api/* 加 Deprecation header                       │
│   → routes.rs 拆分为 api_routes / ws_routes / admin_routes      │
│                                                              │
│ P1: 数据生命周期可观测 Phase A（2 天）                             │
│   → workspace_storage_usage 定时采样                             │
│   → Prometheus gauge（workspace_storage_bytes）                  │
│                                                              │
│ P1: Feature Flag 核心引擎 + 运行时可重载配置（5 天）                │
│   → Redis + Arc<RwLock<DynamicConfig>>                          │
│   → 首个试水：日志级别运行时重载                                    │
│   → Admin API（list/create/set flag）                           │
└──────────────────────────────────────────────────────────────┘

Sprint 3（2 周）——平台化
┌──────────────────────────────────────────────────────────────┐
│ P1: per-workspace AI 用量台账 + 预算门禁（5 天）                 │
│   → ai_usage_ledger 表                                          │
│   → Redis-backed WorkspaceBudgetEnforcer                        │
│   → GET /api/workspaces/:id/billing/ai-usage API                │
│                                                              │
│ P2: WS 扇出并行化 + 房间级快照（3 天）                            │
│   → fan_out_arc_inner 分块并行                                   │
│   → rooms: DashMap 第二索引                                      │
│                                                              │
│ P2: 迁移反向迁移 v2（5 天）                                       │
│   → down.sql 模板 + aero-cli rollback                           │
│   → 零停机 DDL runbook                                          │
│   → 向后兼容性窗口声明                                           │
└──────────────────────────────────────────────────────────────┘

Sprint 4+（持续）——纵深
┌──────────────────────────────────────────────────────────────┐
│ P2: 通知渠道抽象 + 邮件通知（方向④ Phase A-B）                    │
│ P2: 蓝绿/金丝雀发布 runbook                                     │
│ P2: fuzz target 建立                                            │
│ P2: benchmark + 性能回归                                        │
│ P2: Billing/Stripe 集成（引用既有分析设计）                        │
└──────────────────────────────────────────────────────────────┘
```

### 5.2 风险与缓解

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| Sprint 1 的 4 个 P0 互相依赖 | 低 | 中 | 无依赖关系，可并行执行；只有仓储层连接池隔离改 `AppState` 可能有冲突 |
| Feature Flag 引擎的 Redis 一致性 | 中 | 中 | Redis 不可用时 fallback 到「默认关闭」+ 日志告警。不阻塞其他方向 |
| API 版本双轨制导致路径混乱 | 中 | 高 | 明确策略：新端点必须 `/api/v2/`，不允许例外；旧端点 `Deprecation` header 写在中间件中不移除 |
| per-workspace 预算的 Redis 原子性 | 中 | 中 | Redis `INCR` + `EXPIRE` 是原子操作。窗口滑动不一致在 ±1 request 范围内可接受 |
| 连接池拆分后 SQL 被路由到错误池 | 低 | 高 | 显式命名：`pool_im` 只允许 IM 仓储使用，`pool_live` 只允许 Live 仓储。CI 不检查但 code review 约束 |

### 5.3 关键决策点：选项与权衡

| 决策 | 选项 A | 选项 B（推荐） | 理由 |
|------|--------|---------------|------|
| 迁移向下兼容检查 | 全部 migrations 标记兼容性版本 | **仅标记不可逆迁移 + 声明向前兼容窗口（二进制 N 次）** | 157 次迁移补标记工作量太大。仅标记不可逆的，新的天然有标记 |
| 错误代码枚举命名 | `SNAKE_CASE`（与既有 `rate_limited` 一致） | **UPPER_SNAKE_CASE**（`RATE_LIMITED`） | `rate_limited` 在客户端可能被错误解释为 snake_case 变量名。UPPER 是常量惯例 |
| API 版本路径 | `/api/v1/*` vs `/api/*` | **双轨制：/api/*（v0，隐式） + /api/v2/*（显式）** | 重命名 150 端点为一次性破坏变更不可接受 |
| 运行时可重载配置的存储 | 全部放 Redis | **配置文件（config.toml）+ Redis 覆盖** | Redis 故障时配置参数有文件级 fallback。配置文件和 Redis 启动时合并 |
| AI 预算门禁 | PG 行级锁（`SELECT … FOR UPDATE`） | **Redis INCR + EXPIRE 窗口** | PG 锁在预算路径上增加延迟和死锁风险。Redis INCR 是原子且低延迟 |

---

### 总结

Aero IM 的架构骨架是正确的（NATS event bus + Hub 扇出 + AI 预算系统），但在三个维度已到必须加固的拐点：

1. **运维安全**：157 次单向迁移 + 无版本兼容性检查 = 发布风险呈指数增长
2. **可靠性隔离**：80+ 仓储共享单连接池 = 级联故障放大器的静默存在
3. **产品化**：无结构化错误码 + 无 API 版本 = 无法支撑第三方集成和企业采购

建议的 Sprint 1（止血阶段）4 个 P0 方向无依赖关系，可并行推进。**CI 集成测试启封（2 天）的 ROI 最高**——1 次配置变更解锁 35+ 数据库门控测试，在合并到 main 之前捕获回归。**迁移生命周期 v1（3 天）次之**——157 次迁移不需要一次性解决回滚，只需分级 + 兼容性检查即可消除最大风险。

以下是基于验证文档和项目源码的架构分析。

---

# 架构分析：Aero IM — 四方向工程债与架构演进路径

## 1. 架构评估

### 1.1 当前架构的优势

Aero IM 在 crate 层面实施了**严格的依赖倒置**（aero-common / aero-bus / aero-storage 为叶子，无向上依赖），这是 Rust 单体仓库中极难保持的纪律。事件 DAG（NATS → Hub → bounded mpsc → WebSocket）在逻辑上是正确的：每实例一个 durable consumer 保证 at-least-once，Hub 的本地扇出消除 N² 网络级联。crimes 极少——这是成熟代码库。

**值得称赞的工程决策**（按影响排序）：

1. **迁移编译期嵌入**（`sqlx::migrate!("../../migrations")`）。这消除了部署时 K8s initContainer 顺序依赖，且是唯一能保证迁移版本与二进制匹配的机制。很少有项目认真做这一点。
2. **CRATE = FEATURE UNIT**（16 crate）。比按 `src/domain/` 分层更不易出现循环依赖，且 `cargo build -p aero-live-srt` 只编译该 crate + 叶子——CI 缓存收益显著。
3. **`tag="kind"` + `#[serde(rename)]` 规避碰撞**。这是 serde 实践中一个难以发现的陷阱，文档捕获了它。
4. **`FOR UPDATE SKIP LOCKED` 轮询 + DEAD-LETTER**（AiWorker）。这是 Postgres 作为 job queue 的正确技术栈：无 Redis 丢失风险、无第三方 broker 依赖、水平扩自然。
5. **`statement_timeout` 缺位时 CI 仍干净**——说明无查询在测试中挂起 2 小时？不，这只说明测试数据量太小；生产数据规模下（`messages` 表数百万行 + 无分区 + 无 `statement_timeout`）单次 FTS 查询就可能挂住一个连接数小时，而 pool 连接耗尽会级联到全站不可用。这不是代码的胜利——而是负债尚未会计入账。

### 1.2 当前架构的局限性（重点）

我将四个方向的发现归类为三层债务（按修复成本递增）：

**L1 — 配置级（小时级修复）**
- 无 `statement_timeout`（在连接 URL 追加 `?options=--statement_timeout%3D30s` 即可）
- 无 autovacuum 定制
- 无 `pg_stat_statements` 暴露 → 无法从 SQL 面诊断慢查询
- 无 PG 持久化 volumes（`docker-compose.yml`）

这些不需要代码更改，不需要 RFC，不需要 feature branch——一小时内完成，但阻止了数百万行级灾难。

**L2 — 客户端架构级（周级修复）**
- 无界 `messagesByRoom` Map + `replaceChildren()` 全量重建 + 每条消息的独立事件监听器。这不是「性能优化」——这是**可扩展性上限**。5000 条消息不只是一个慢渲染问题；5000 条消息 × 每条 ~5 个闭包 = 25000 个未释放的闭包，在 SPA 生命周期内永不释放。当前 SPA 架构假设房间消息数不会超过几百条，这是一个明确的设计假设，在小型工作空间下合理，但对大型社区频道（可能数万条）会失效。
- 无 `scrollAnchor` 保持：`loadHistory` 手动算 scroll position diff（`app.js:280`），但 `switchRoom` 不存/不恢复——导致跨房间切换后丢 scroll 位置。

**L3 — 领域建模级（月级修复）**
- 搜索仅限 messages 表——文件/画布/投票/书签/VOD 无搜索索引
- 无全局实体搜索端点（`/api/search` 确实存在但仅限消息）。
- 经济杠杆系统（predictions / hype trains / raids / points）完全缺少可信执行层：无 stakes 分布可见性、无 hype train 流控、无 raids 真实性校验、无 points 异常审计。这是**最危险的债务**——它不是性能或体验问题，而是可信度问题。一旦接入 Stripe 购买 points，这变成真金白银的攻击面。

### 1.3 关键设计决策评估

| 决策 | 评价 | 当前状态 |
|------|------|---------|
| SPA 零依赖 ES2020 | ✅ 正确的早期决策（减少构建依赖攻击面）。但到了需要虚拟滚动和响应式状态管理的规模，这一决策已成为生产力瓶颈 | 需要战略性 re-evaluate |
| NATS JetStream durable consumer 作跨实例事实源 | ✅ 正确的选择——比 Redis Stream 更可靠的持久化、比 Kafka 更轻量的 ops | 不需要改动 |
| Redis sorted-set 作集群状态 | ✅ 正确的选择——自动过期、O(log N) 的 `zrangebyscore` | 不需要改动 |
| 迁移编译期嵌入 | ✅ 正确的决策 | 不需要改动 |
| 无 ORM（sqlx 直接 SQL） | ✅ 正确的决策——`statement_timeout` 缺位暴露了 ops 盲点而非 ORM 问题 | 需要补 ops 配置 |
| `FOREIGN KEY` 级联删除 vs 应用层 soft-delete | ⚠️ 文档提到的 GDPR 删除是应用层逐表清理，无 cascade——这是正确的（避免不小心级联删哨兵数据）但需要显式维护删除清单 | 已在 AGENTS.md §3 治理中提及 |
| SPA 的 `Map` / `replaceChildren` / 独立闭包 | ❌ 在原型阶段正确，但在当前功能密度下成为可扩展性瓶颈 | 需要 L2 修复 |

---

## 2. 扩展方向（高价值架构改进）

### 方向 A：SPA 渐进式架构迁移（P0）

**为什么需要**：当前 SPA 架构在小型团队（<50 人）场景下完全够用。但在社区频道（500+ 成员，10K+ 消息）场景下，当前实现会导致：
- 切换房间时 100-300ms 的主线程阻塞（`replaceChildren` + 25000 闭包挂载）
- 内存泄漏（消息 DOM 节点从不卸载，`messagesByRoom` Map 只增不减）
- 事件监听器泄漏（每条消息 ~5 闭包 × 存量消息数，永不 GC）

**核心挑战**：在「仍是零依赖 SPA」和「需要更复杂的状态管理」之间找到渐进的中间点。不应一次性引入 React/Vue（2-3 周的前端重构 + 回归 regressions 风险）。

**建议的三阶段接口演进**（不是代码，是设计模式）：

**阶段 A1：渲染层优化（2-3 天）**
- 在 `switchRoom` 和 `rerenderCurrentRoom` 中引入分帧渲染：每 `requestAnimationFrame` 帧 append ~20-50 条消息，不阻塞主线程。
- 引入 `msgNodeCache`：`Map<msgId, HTMLElement>`，在 `rerenderCurrentRoom` 中优先复用，只有新消息才 `renderMsgWithReactions`。
- 引入**事件委托**：在 `els.msgList` 挂一个 `click` handler，按 `data-msg-id` + `data-action` 属性分派，消除 N 个独立 `addEventListener`。
- 这三者可以串联叠加，每步 2-4 小时，无架构侵入。

**阶段 A2：虚拟滚动引入（1 周）**
- 引入 IntersectionObserver 驱动虚拟滚动，仅在 DOM 中保留可见区域 ±2 屏高度的消息节点（~50-80 条）。
- `messagesByRoom` 仍作为数据层全量保留（无 LRU 限制也可，因为虚拟滚动后 DOM 节点不再泄漏）。
- 需要实现 `scrollAnchor` 保持——每次 O(1) 算相对偏移而非 O(N) 搜。

**阶段 A3：状态管理抽象（可选，2 周）**
- 将 `context.js` 的 global `state` 对象封装为一个 `Store` 类，支持 `subscribe`/`batch`/`selector` 模式——但不必是 React，可以是一个 200 行的纯函数订阅器（已存在于许多生产应用如 Preact Signals 的轻量版）。
- 这为未来引入 Web Components 或渐进式 React 集成留接口，但不强制。

### 方向 B：数据库运营就绪化（P0）

**为什么需要**：当前项目中有 157 次迁移、CI 全绿、代码质量极高——但**缺少让 Postgres 在生产中存活的基础设施配置**。没有 `statement_timeout`，慢查询会挂住连接直到 pg 杀死它或 operator 手动 cancel。没有 autovacuum 定制，频繁 INSERT/UPDATE 的消息表会产生死元组膨胀。没有 `pg_stat_statements`，DBA 面对异常时盲飞。

**建议的变更**（非代码，是配置化和可观测性架构）：

1. **连接 URL 参数化**：`db.rs` 的数据库 URL 构建应支持层次化配置，读取 `AERO__DATABASE__STATEMENT_TIMEOUT`（默认 30s）、`AERO__DATABASE__IDLE_TIMEOUT`（默认 10s）等参数，通过 `?options=` URL 参数注入。这不需要 PG 配置更改，完全由应用层控制。

2. **定期 `REINDEX CONCURRENTLY` 调度**：在 boot 的定时器清单中加一个低优先级 timer（`AERO_DATABASE_MAINTENANCE_SECS`，默认 3600，0 禁），运行 `SELECT schemaname, tablename, indexname, pg_size_pretty(pg_relation_size(indexrelid)) FROM pg_stat_user_indexes WHERE idx_scan > 0 AND NOT indisvalid` 等维护语句，上报到 metrics。

3. **连接池健康仪表**：暴露 `pool.num_idle()` / `pool.num_size()` 到 `/metrics`——在 `statement_timeout` 触发连接耗尽时优先告警。

### 方向 C：实体搜索统一层（P1）

**为什么需要**：当前 `POST /api/search` 仅搜索 messages 表。用户期望的「统一搜索」应返回：消息（加权最高）+ 文件/图片 + 投票 + 书签 + 画布页面 + 直播 VOD。Cmd+K / Quick Switcher 是 Slack 和 Discord 用户的核心肌肉记忆。不同实体类型跨库查询在 Rust 中可以在一个请求内 `tokio::join!` 多路并发查询，然后 Rust 侧 merge + ranking。

**核心挑战**：
- 不同实体在不同 crate 中（`aero-storage` 的 `MessageRepo`、`aero-live-core` 的 VOD 等）——需要统一的 `SearchableEntity` trait 协调。
- 排序统一：消息 FTS 得分 vs 文件名称 bm25 vs 投票标题 trigram——需要归一化 score 或分层排序（消息优先、文件/投票次之）。
- 鉴权统一：跨实体搜索需要 `JOIN room_members` or `JOIN room_participants` 来确保结果只来自用户有权限的房间。

**建议的 trait 设计（接口级，非实现）**：

```rust
// 在 aero-common 或新 crate aero-search 中
#[async_trait]
pub trait SearchableEntity: Send + Sync {
    /// 实体类型标签（用于排序和 UI 分组）
    fn entity_kind(&self) -> EntityKind; // Message, File, Poll, Bookmark, Canvas, Vod

    /// 搜索这个实体的索引
    async fn search(
        pool: &PgPool,
        query: &str,
        participant_id: ParticipantId,
        workspace_id: Option<WorkspaceId>,
        limit: u32,
    ) -> Result<Vec<SearchHit>>;

    /// 归一化得分
    fn normalised_score(&self) -> f64;
}
```

路径 `POST /api/search` 接收 `query` + 可选的 `types` 过滤器（`["message","file","poll"]`），用 `tokio::join!` 并行查询每个启用的实体类型对应的 repo，Rust 侧 merge、dedup、ranking，返回统一结构。这保持向后兼容——现有客户端只解析 `results` 数组，新客户端可以读 `kind` 字段分组渲染。

**对现有系统的影响**：最小。路由已存在（`search.rs` 的 `/api/search`），改动限于：
1. 定义 `SearchableEntity` trait 和 `EntityKind` enum
2. 为各 entity 实现该 trait（在对应 crate 或集中式 search 模块）
3. `search_all` handler 改为 `tokio::join!` 调用
4. web `search.js` 和 `app.js` 的 `renderSearchResults` 改为按 `kind` 分组显示

### 方向 D：创作者经济诚信层（P1 → P2）

**为什么需要**：文档已经指出——predictions / points / hype trains / raids 是（或最终将是）与真金白银挂钩的系统。当前实现完全信任客户端提供的数值，无审计日志，无异常检测。这不是未来问题——这是当前的攻击面。

**建议的架构变更**：

1. **P0 先做（当天可完成）**：在 `predictions::stake` 路径添加审计 INSERT 到 `prediction_stakes_audit` 表，记录 `participant_id` + `outcome_id` + `amount` + `ip_address` + `user_agent`。这不需要 UI，不需要新路由，是一条 INSERT + 一条迁移。这为后续所有防操纵措施提供 forensics 基础。如果 0day 发生后没有审计日志，事后调查不可能。

2. **P1 接着做（1 周）**：
   - HyperTrain 添加频控：每个 `stream_id` 的 `on_gift` 调用级联到 `RateLimiter`，限制 `gift_type` + `amount` 的组合在 `WINDOW_SECS` 内不超过 `MAX_CONTRIBUTION`。
   - Raid 的 `viewer_count` 在设置时与 `StreamViewerStore::count_viewers(stream_id)` 交叉验证——差值超过 20% 则拒绝或标记（对 UI 显示灰色警告图标，但允许主播 override）。
   - Predictions 添加 `GET /predictions/:id/stakes` 路由（匿名化：只显示 `outcome_id` + `total_staked` 分布，不暴露参与者的 identity）。

3. **P2 远期**：Points 交易引入可信时钟。当前 points 完全是 volatile 的（服务器重启后可以重算？不——points 持久化但无快照校验）。应在每天 UTC 00:00 对每个 participant 的 points 余额做快照到 `points_snapshots` 表，与当日的 stakes/spent 校准——任何不一致触发告警。

**对现有系统的影响**：各组件独立升级，无系统性变更。预测审计表是纯追加，无 SQL 迁移回滚复杂度。频控复用现有 RateLimiter 基础设施（`aero-storage` 已有 `RateLimitStore` + per-ws 和全局 budget）。

### 方向 E：可观测性基础设施和 SLO 框架（P1）

**为什么需要**：从 AGENTS.md 中可看出，项目已有 Prometheus gauges（DB 池、WHIP、AI DLQ、NATS backlog），这是很好的起点。但缺少 SLI/SLO 框架来回答「我们是否健康」——当前是告警各个独立度量的串联，没有聚合的健康视图。

**建议的架构变更**：
1. **定义 5 个核心 SLI**（服务端 + 实时 + 媒体 + 数据库 + 推送），每个 SLI 附一个 Prometheus gauge。
2. **在 `/health/ready` 中加入 SLI 聚合**：不只是「PG/Redis/NATS 可达」，还要「messages 处理延迟 P99 < 2s？」「AI 作业延迟 P99 < 30s？」。
3. **添加端点 `GET /api/admin/sli`**（admin bearer 门控），返回当前 SLI 状态和最近 1h/24h 的 SLO 达成率——这是运营手册的输入，也是 incident response 的起点。

**对现有系统的影响**：基本上纯新增，无重构。`observability_gauge_samplers` 三个定时器已是类似架构，按同一模式添加即可。

---

## 3. 接口设计建议

### 3.1 关键模块接口原则

当前代码库中，`aero-storage` 的 Repo 模式（`XRepo` 包 `PgPool`，方法返回 `Result<Vec<X>>`）是良好的统一模式。但跨 crate 汇聚（如统一搜索）暴露了当前模式的不足：每个 Repo 各自定义自己的 `search` 方法、参数签名不同、返回值结构不同。

**建议补充的原则**：

| 原则 | 理由 |
|------|------|
| **查询返回同构结构** | `SearchHit { kind, id, score, headline, payload_json }`——统一搜索跨实体时不必 each entity 自定义反序列化 |
| **Limit/offset 统一在公共参数层** | 每个 `search` 方法都用 `limit: u32` + `offset: u32` 参数，由调用方担保 ≤ `MAX_SEARCH_LIMIT` |
| **tenant 边界统一在仓储层** | 所有跨实体查询的 `WHERE room_id IN (SELECT room_id FROM room_members WHERE participant_id = $1)` ——不依赖调用方记住 |
| **禁止在 handler 层拼接 SQL** | 保持当前好做法（SQL 在 Repo，handler 只调用和 response 组装） |

### 3.2 是否需要新的抽象层

当前 16 个 crate 的划分是 feature-first 的，这很好。但以下两个新抽象层是合理的：

**`aero-search`（新 crate）**：提供统一的 `SearchableEntity` trait + `SearchRegistry`，实体注册后自动将 `/api/search` 的 `tokio::join!` 调用分发。新 crate 而非插入既有 crate 的原因：
- 不打破现有 crate 的依赖倒置（aero-common ← aero-storage ← aero-search ← aero-server）
- 统一搜索涉及跨 crate 类型导入，集中更好管理

**`aero-observability`（新 crate 或 aero-common 扩展）**：SLI 框架 + SLO 聚合 + 健康聚合。建议在 aero-common 中新增一个 `sli` 模块而非新 crate，因为 SLI 定义需要被所有 crate 引用。

### 3.3 向后兼容性

所有四个方向的改动均设计为向后兼容：

| 方向 | 兼容策略 |
|------|---------|
| SPA 渲染优化 | 仅内部函数重构，无 API 契约变动。`msgNodeCache` 对外不可见 |
| 数据库运营 | 环境变量缺省值为「不做额外操作」或「与当前行为一致」 |
| 统一搜索 | 现有 `/api/search` 结果数组结构不变，新增 `kind` 字段在 `results[]` 元素中，旧 client 忽略未知字段 |
| 创作者经济诚信 | 审计表是纯 INSERT（无 SELECT 影响），新路由（`/stakes`）是加而非改 |
| 可观测性 | SLI gauges 是对既有 metric 的聚合，不影响现有 metric 路径 |

---

## 4. 技术选型

### 4.1 是否需要引入新的技术栈

核心判断：**在当前阶段不需要引入任何新的外部技术栈**。所有四个方向都可以用现有技术栈（Postgres + NATS + Redis + ES2020 SPA + Rust）解决。

| 方向 | 假设「可能需要新技术」 | 实际证明不需要 |
|------|-----------------------|---------------|
| SPA 渲染 | 可能需要 React 或 Vue 来解决状态管理 | ❌ 200 行 Store 类 + 分帧渲染 + 事件委托 = 同等工作量且无构建步骤 |
| 统一搜索 | 可能需要 Elasticsearch 或 Meilisearch | ❌ 157 次迁移已展示 pg 的适应力；pgvector + pg_trgm 对当前数据量级足够（pg_trgm 索引可支持百万行级 trigram 匹配，pgvector HNSW 可支持百万级向量最近邻）。仅当达到 1 亿行时才需考虑专用搜索引擎 |
| 创作者经济 | 可能需要规则引擎或 fraud detection SDK | ❌ 频控复用现有 RateLimiter；审计用 INSERT（Postgres）；异常检测可以用带时间窗口的 SQL 窗口函数 |

**保留性建议**：6-12 个月后，如果用户数增长 10 倍、消息表超过 1 亿行、FTS 查询 >500ms 时，**才**考虑引入：
- **Meilisearch**（自建、Rust 编写、内存友好）作为搜索专用引擎——替代性：接近零，因为 Postgres FTS/trigram 对 <5KW 行足够
- **ClickHouse** 作为审计和可观测性数据的分析引擎——替代性：`pg_stat_statements` + Postgres 分区表对 <5 亿事件行/月足够

### 4.2 第三方依赖评估标准

当前 `str0m` 被正确限制在 `aero-live-webrtc` / `aero-live-whip`。这是依赖治理的榜样。建议在 `deny.toml` 中补充一条规则：**任何在 root `Cargo.toml` 新增的依赖必须有 RFC 理由**（防止 root 变成脆弱的隐式公共依赖）。

### 4.3 自建 vs 采购决策

所有四个方向都建议**自建**，且理由一致：

| 方向 | 自建理由 |
|------|---------|
| SPA 渲染 | 零外部依赖是当前的强项；200 行纯函数就能解决的性能问题不值得引入千行级框架 |
| DB 运营 | 全是配置和少量 Rust 定时器——无采购对象 |
| 统一搜索 | 在现有 pg + Rust 层融合即可；引入 Elasticsearch = 增加 ops 面（集群维护、索引备份、数据同步延迟问题） |
| 创作者经济 | 审计和频控是领域特定逻辑——通用规则引擎不如 Rust 代码直接 |
| 可观测性 | 当前 Prometheus + gauges + `/health/ready` 模式与项目配合良好；标准 SLI/SLO 框架会在中期引入 OpenTelemetry，但仍建议在当前 gauges 基础上扩展而非替换 |

---

## 5. 实施路线图

### 5.1 优先级排序

```
P0（此 sprint）            P1（下个 sprint）            P2（Q3）
─────────────────────────────────────────────────────────────
statement_timeout          统一搜索 search trait        滚动锁/乐观锁
autovacuum 配置            predictions/stakes 审计表     points 快照校验
分帧渲染                   hype train 频控              事件溯源预热
事件委托                   raids viewer_count 校验      SLI 自动化
msgNodeCache               pg_stat_statements 路由      Cmd+K 面板
AI 审核预算（已有）
```

### 5.2 阶段划分

**阶段 A（3 天）— 止血**
1. 数据库 URL 追加 `statement_timeout=30s`，`idle_in_transaction_session_timeout=10s`。行数 ≤5，时间 ≤10 分钟。
2. `virtual-text`：在 `rerenderCurrentRoom` 和 `switchRoom` 中引入分帧渲染。行数 ~30，不触碰任何其他函数。
3. 事件委托：`els.msgList.addEventListener('click', handleMsgClick)`，匹配 `data-msg-id` + `data-action`，保留 `wireMsgActions` 作为 fallback（渐进）。行数 ~40。
4. `msgNodeCache`：`Map<msgId, HTMLElement>` + 在 `rerenderCurrentRoom` 中复用已渲染节点。行数 ~20。

**阶段 B（1 周）— 可观测性 & 可维护性**
1. `statement_timeout` / `idle_timeout` 参数化（CockroachDB 友好；大多数云 PG 也支持 `?options`）。
2. Migration `0158_pg_stat_statement_permissions.sql` + `metrics` 端点暴露 top-N 的 `query` + `mean_time` + `calls`。
3. autovacuum 配置（`ALTER SYSTEM SET autovacuum_vacuum_scale_factor = 0.01` 等——这是 `ALTER SYSTEM` 而非 ALTER TABLE——侵入性最小，仍需 PG 重启确认）。
4. 预测审计表 `prediction_stakes_audit` + INSERT 路径，零新路由。

**阶段 C（2 周）— 统一搜索**
1. 定义 `SearchableEntity` trait + `EntityKind` enum。
2. 为 Message、Poll、Bookmark 实现（Files/Canvases/VODs 可在后续阶段逐一添加）。
3. `POST /api/search` handler 改为 `tokio::join!` 调用注册的实体查询。
4. web `search.js` 渲染改为按 `kind` 分组 UI。

**阶段 D（2 周）— 创作者经济加固**
1. HyperTrain 频控：`RateLimiter` 封装到 `HypeTrainRepo`。
2. Raid viewer_count 校验：在 `create_or_update_raid` 前调用 `StreamViewerStore::count_viewers`。
3. 路由 `GET /predictions/:id/stakes` ——匿名化 stakes 分布。

**阶段 E（C 和 D 并行）— 数据库维护自动化**
1. 定时器 `database_maintenance_timer`：每小时 `current_schemaname || '.' || current_tablename` 运行 `REINDEX CONCURRENTLY` 于 `idx_scan > 0 AND (idx_scan::float / GREATEST(idx_tup_read,1) < 0.1)` 的低效索引。
2. 定时器 `points_snapshot_timer`：daily UTC 00:00 `INSERT INTO points_snapshots`。
3. 跨阶段持续：向 `observability_gauge_samplers` 添加上述 gauge。

### 5.3 风险点和缓解策略

| 风险 | 可能性 | 影响 | 缓解 |
|------|--------|------|------|
| 分帧渲染引入视觉闪烁（部分消息先出现在 DOM 中，剩余在后续帧中出现） | 中 | 低-中 | 在帧之间维持 `min-height` 占位符，防止 scroll 跳跃 |
| 事件委托遗漏 `dblclick` / `contextmenu` | 低 | 中 | 保留 `wireMsgActions` 对 `dblclick` 和长按的绑定（这些事件无法通过 `click` delegate ），仅在 `click` 上委托 |
| `statement_timeout` 杀死只读长事务（AI 推理 < 30s） | 中 | 高 | AI `ask` 和 `summarize` 路径用单独的数据库连接（复用 `s.pg_read` 但如果 replica 配置了不同 timeout？）；最简单的缓解：将 LLM 调用的 timeout 设定为 >= 预期 longest completion + 5s 裕量 |
| `REINDEX CONCURRENTLY` 和 DDL 冲突（并发迁移） | 低 | 高 | 定时器设 `lock_timeout = 100ms`，争锁即跳过该索引、留下日志、继续下一个 |
| Predition stakes 审计写入增加主表 INSERT 延迟 | 低 | 低 | 审计 INSERT 在 `predictions_audit` 表上，与 `predictions` 表无外键；`autovacuum` 各自独立 |
| 统一搜索 `tokio::join!` 放大数据库连接使用 | 中 | 中 | 每个实体查询限制 `limit ≤ 10`（总返回 ≤ 50），并设置 `tokio::time::timeout`（5s）防单个实体拖垮全部 |
| web SPA 零外部依赖策略被方向 A3 的选择性状态管理妥协 | 低 | 高（策略层面） | 200 行 Store 类 + `subscribe` 模式 = 零外部依赖，与当前策略兼容。仅当走向 Web Components 或 `lit-html` 时才需权衡——但那是 6 个月后的决策 |

---

## 总结

四个方向有一个共同特征：**它们都是基础设施债务而非功能缺失**。项目中没有人「忘记」做 `statement_timeout` 或虚拟滚动——这些是在快速迭代中、在功能密度增加后、在客户端负载触达隐式上限后才会暴露的债务。

最推荐的启动顺序是 **P0 阶段的三天止血**——先从 DB 配置和 SPA 分帧渲染入手，两者均可在三小时内完成且零副作用。这会立竿见影地提高生产稳定性和大型房间的用户体验。然后依次推进可观测性（阶段 B）、统一搜索（阶段 C）、创作者经济加固（阶段 D）、数据库维护自动化（阶段 E）。

核心建议：**不要为这些方向引入新的外部技术栈**。当前的技术栈（Rust + Postgres + NATS + Redis + ES2020 SPA）对项目的生命周期完全足够。下一轮架构更新应在产品增长 10× 后再触发，届时优先考虑的是专用搜索引擎（Meilisearch）和审计分析引擎（ClickHouse），而非对前端框架的灾难性重构。

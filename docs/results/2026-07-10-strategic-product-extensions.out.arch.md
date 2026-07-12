好的，我已通读分析文档及当前系统架构（设计 Spec + AGENTS.md + 实际代码结构），下面从架构师视角输出完整分析。

---

# 架构分析：Aero IM 五战略方向

## 1. 架构评估

### 1.1 当前架构的核心优势

Aero IM 当前架构在 **事件驱动** 和 **模块化** 两个维度处于非常健康的状态，这是五个方向能够落地的根本前提。

| 优势 | 具体体现 | 对战略方向的价值 |
|------|---------|----------------|
| **NATS 事件总线是普适的事实源** | 所有业务事件（消息、直播状态、通话信令）统一流过 `im.room.*` / `live.stream.*` subject | 方向①（自动化）无需新增事件埋点即可匹配触发条件；方向③（数据管道）直接挂载 consumer 即可归档 |
| **Bot 架构是 NATS 消费者的统一部署模式** | 8 个常驻 Bot 共享同一套 durable consumer 模式 + `BotRepo` + `send_message` | 方向①（自动化引擎）的动作执行器可直接复用 bot 既有的「系统身份发消息」路径 |
| **Hub 扇出模式解耦了 NATS 与 WebSocket** | `Hub::fan_out_raw` 是进程中 bounded mpsc，不阻塞 NATS ack | 方向④（SLO）可以在此处插桩 `fan_out_latency` 而不影响现有路径 |
| **crate 依赖方向严格自下而上** | 无成环依赖，`aero-common` 是纯叶子 | 加新 crate（如 `aero-automation`）不会造成重构风险 |
| **Redis 状态可水平扩展** | Presence / viewer / roster 全部走 Redis sorted-set，非进程内存 | 方向⑤（计费）的用量计数可以安全地基于 Redis 原子操作 |

### 1.2 制约五个方向的架构债务

以下是我识别的、会直接阻碍或增加成本的既有架构债务：

#### 债务 ①：事件 Schema 无版本号、无持久化归档
```
当前：NATS JetStream 有消息体但无 schema registry
      consumer 消费后即丢弃原始事件
      RoomEvent / StreamEvent serde 使用 tag="kind" 但跨版本兼容为零
```
- **影响方向③（数据管道）**：历史事件格式未知，无法回填分析
- **影响方向⑤（计费）**：计费需要精确的历史用量溯源，但当前 `ai_usage` 表只有聚合无事件级证据
- **建议修复**：每个 NATS 消息加 `"schema_version": 1` 顶层字段；事件湖 consumer 写入时保留原始 JSON

#### 债务 ②：API 面无版本标识、无字段投影、无增量同步
```
Evidence: GET /api/rooms/:id/messages → 永远返回完整 Message 对象
          无 Accept-Version / X-API-Version 头
          无 ?fields= 投影参数
```
- **影响方向②（Mobile API）**：移动端带宽敏感，每次全量加载不可接受
- **建议修复**：先加 `?fields=`（50 行）和 `?since=`（100 行），再考虑版本化

#### 债务 ③：延迟/错误率指标为零
```
metrics.rs 暴露的 20+ 指标全部是 counters/gauges
无 histogram（延迟分布）、无 error_rate（错误率）
```
- **影响方向④（SLO）**：无延迟直方图无法计算 p95/p99 SLI
- **建议修复**：优先加 `aero_api_latency_ms`（端点级 histogram）和 `aero_api_errors_total`（按 status 分）

#### 债务 ④：计费所需的「资源消耗归因」数据散布且不精确
```
存储：blobs 表有 size，但无 per-workspace 聚合视图
AI：ai_usage 有 prompt_tokens + completion_tokens，但无 cost_in_cents
带宽：零计量
通话：零计量（SFU 消耗 CPU + 带宽）
```
- **影响方向⑤（计费）**：无法输出可信账单
- **建议修复**：先走估算（`cost_estimate` 端点），再逐步精确

#### 债务 ⑤：SPA 是单体调试客户端，非产品级 UI
```
app.js 全量加载消息到内存
无虚拟滚动、无分片加载、无增量 DOM 更新
```
- **跨方向影响**：方向①的自动化规则 UI 需要组件化前端，当前 SPA 架构不支持
- **建议**：不在此次分析范围内，但方向①若要做可视化编辑器，前端架构需先重构

### 1.3 关键设计决策的合理性评估

| 决策 | 当时合理性 | 今日约束 | 建议 |
|------|-----------|---------|------|
| **NATS JetStream 而非 Kafka** | 运维简单、单二进制部署、Rust 生态友好 | 方向③的数据管道需要 Kafka Connect 生态（Schema Registry / Kafka Connect / ksqlDB）——NATS 没有等价物 | 可维持 NATS 做实时总线，新增事件湖 consumer 写 S3 Parquet，批处理走 DuckDB/ClickHouse |
| **无 API 版本化** | 单 SPA 客户端、全控制部署 | 方向②的移动端需要稳定的 API 契约 | 现在加版本化为时未晚——`Accept: application/vnd.aero.v1+json` |
| **8 个 Bot 全部硬编码** | 快速交付、行为确定 | 方向①要求用户可配置规则 | 不冲突——硬编码 Bot 作为系统内置规则，自动化引擎作为用户自定义规则 |
| **WS 作为唯一实时通道** | AXum 原生支持、双向通信 | 移动端后台连接不可靠 | Push 通道已建，需补充 Push inline reply + delta sync 减少 WS 依赖 |
| **单租户起步（设计 Spec 明确）** | 聚焦 MVP 验证 | 方向⑤的计费面向多租户 SaaS | 架构上已有 workspace 隔离（rooms.workspace_id NOT NULL），计费层可以叠加 |

---

## 2. 扩展方向分析

本节对文档提出的五个方向进行架构层面的深入评估，每个方向给出：**架构适应性评分**、**核心设计决策**、**技术方案选项**与 **权衡分析**。

### 2.1 方向①：零代码工作流自动化引擎（P1）

**架构适应性评分：★★★★☆（4/5）**

NATS 事件总线的存在使此方向的**触发器侧接近零成本**——每个业务事件已经流过 `im.room.*` subject，只需新增一个 durable consumer 即可捕获所有候选事件。动作侧复用 `BotRepo::send_message` 路径。

#### 核心设计决策

**决策 1：规则引擎的架构位置——进程内 vs 独立服务**

| 选项 | 优点 | 缺点 |
|------|------|------|
| **A. 进程内引擎**（`aero-server` 内新增模块，类似 `agent_bot.rs`） | 零网络开销、一致的状态视图（共享 `AppState`）、部署简单 | 规则复杂度上升后可能影响主进程；无法独立扩缩 |
| **B. 独立 crate + 独立进程**（`aero-automation`，作为消费者组运行） | 水平扩展、失败不影响主服务、可独立发布 | 需要 RPC 调用主服务执行动作（增加延迟 + 复杂度）；需要处理分布式一致性 |

**推荐：阶段 A → 阶段 B**。MVP 用进程内引擎（类似 `moderation_bot` 的有界 mpsc 模式），规则量 > 500 后抽离为独立进程。

**决策 2：规则表达式的持久化格式——DSL vs DSL-less 结构化 JSON**

| 选项 | 优点 | 缺点 |
|------|------|------|
| **A. JSON 结构化**（`{trigger: {type: "message", pattern: "keyword"}, action: {type: "send_message", room_id, blocks}}`） | 无需解析器、安全（无注入）、前端表单直接映射 | 表达能力有限（不支持复合条件 "A AND (B OR C)"） |
| **B. 嵌入式 DSL**（Rhai / TL 表达式） | 表达能力强、灵活 | 安全风险（沙箱逃逸）、调试困难、测试成本高 |
| **C. Webhook + 外部函数**（条件满足时调外部 HTTP） | 无限灵活、生态兼容 | 延迟增加、依赖外部服务可用性 |

**推荐：A（MVP）+ C（进阶）**。JSON 结构化覆盖 80% 场景（关键词匹配、成员加入、定时触发）。复合条件做 `"condition": {"and": [{"type": "keyword", "pattern": "退款"}, {"type": "room", "room_id": "..."}]}` 的嵌套结构即可，无需做 DSL。

**决策 3：循环检测机制**

```
风险：规则 A 发消息 → 规则 B 匹配该消息 → 规则 B 发消息 → 规则 A 再匹配...
```
- **必须层叠保护**：每次执行注入 `x-automation-depth` 请求头，深度 ≥ 3 即中止；同时设 `x-automation-chain-id`（UUID），相同 chain-id 在 60s 内的重复动作跳过
- **执行日志**：每次规则执行都记录（规则 ID + 触发事件 ID + 动作结果 + 耗时），用于调试循环

**决策 4：规则与 Bot 的关系——替代还是互补？**

**推荐互补**：硬编码 Bot 保持系统级确定性行为（agent_bot RAG 回答 / moderation_bot 审核），自动化引擎处理用户可配置规则。两者通过 `kind` 区分——规则执行使用 `system` participant 身份，Bot 使用各自的 Bot participant 身份。当自动化规则需要 AI 能力时，可以调用 `agent_bot` 的已有 RAG 路径。

#### 架构变更

```
新增：
┌─────────────────────────────┐
│  automation_rule_engine     │  ← 核心模块
│  ├─ RuleRepository (PG)     │  ← 规则 CRUD
│  ├─ TriggerMatcher         │  ← 从 NATS 事件→匹配规则
│  ├─ ActionExecutor          │  ← 执行动作（发消息/创建任务/调 webhook）
│  ├─ ExecutionLog            │  ← 执行记录
│  └─ CycleDetector           │  ← 循环检测
│
│  automation_rules 表         │  ← 迁移新增
│  execution_logs 表           │  ← 迁移新增
│
│  Rust ~400 行 + JS ~500 行   │  ← 体量估计与文档一致
└─────────────────────────────┘

复用：
- BotRepo::send_message         → 动作执行器
- RoomEvent::Message            → 触发器事件
- scheduled.rs / recurring.rs   → 定时触发器
- approvals.rs / tasks.rs       → 动作目标
- webhook.rs                    → 外部动作
```

#### 技术选型建议

- **规则缓存**：Redis 哈希表 `automation:rules:by_trigger:{trigger_type}` ——匹配时只加载相关类型的规则，避免全表扫描
- **执行队列**：复用已有的 `moderation_bot` 的有界 mpsc 模式（512 容量），超限 skip 并记日志
- **无沙箱需求**：JSON 结构化规则不需要沙箱，变量替换（`{{sender.display_name}}`）用 `%s` 格式化即可

---

### 2.2 方向②：Mobile-First API 面（P2）

**架构适应性评分：★★★☆☆（3/5）**

当前 API 设计为单一 SPA 客户端服务，**无版本、无投影、无增量、无离线优先**。移动端需要的不是「一个新端点」，而是一整套 **API 契约升级**。

#### 核心设计决策

**决策 1：API 版本化的时机与策略**

```
现状：路由全部 `GET /api/rooms/...` / `POST /api/messages/...`
      无版本前缀，无 Content-Type 版本协商

选项：
A. URL 前缀版本化：GET /api/v1/rooms/:id/messages
   → 显式、易缓存、运维清晰
   → 代价：需双维护路由组一段时间

B. Header 版本化：Accept: application/vnd.aero.v1+json
   → 优雅、符合 REST 惯例
   → 代价：调试困难、CDN 缓存不友好

C. 渐进式不加版本：
   → 只加 ?fields= 和 ?since= 参数，不加版本号
   → 代价：长期 API 腐化
```

**推荐：先用 C（零成本快速出移动端）+ 半年内过渡到 A**。阶段 C 只加 `?fields=` 和 `?since=` 参数，这些是**向后兼容的参数扩展**，不构成破坏性变更。阶段 A 在方向②的第二批（P1）实施，届时在 `routes.rs` 做 `nest("/api/v1", routes_v1).nest("/api/v2", routes_v2)`。

#### 架构变更

```
增量新增（非新 crate）：
  移动端优化端点（~400 行）：
  ├─ GET /api/me/unread-rooms       → 未读房间聚合
  ├─ GET /api/rooms/:id/delta       → 增量 sync
  ├─ GET /api/rooms/:id/messages?fields=  → 字段投影
  ├─ POST /api/push/reply/{notif}   → Push inline reply
  └─ GET /api/blobs/:id/thumb       → 缩略图（用 image crate）
```

**决策 2：增量 sync 的设计**——cursor 类型

| 选项 | 优点 | 缺点 |
|------|------|------|
| **A. 基于时间**（`?since=<ISO8601>`） | 简单、人类可读 | 时间精度问题（同一毫秒多条消息可能丢） |
| **B. 基于消息 ID/UUID**（`?cursor=<ulid>`） | Ulid 自带时序、精确 | 客户端需要维护游标状态 |
| **C. 基于 seq**（NATS per-room seq） | 与事件总线对齐、精确 | seq 是每个 subject 独立命名空间，跨房间 sync 复杂 |

**推荐：B（Ulid cursor）**。`messages.id` 已经是 Ulid（带时间戳），天然支持 `WHERE id > ?cursor ORDER BY id`。客户端只需保存最后一条消息的 ID 即可做增量拉取。

**决策 3：缩略图管线的实现策略**

- **建议**：`GET /api/blobs/:id/thumb?w=200` 做 on-the-fly 生成 + `Cache-Control: public, max-age=86400`。首次生成慢，后续 CDN 缓存。不需要后台任务预生成。
- **库选**：`image` crate + `image-webp`（输出 webp 格式减小体积），编译时间增长约 30s，可以接受。
- **不做**：视频缩略图（需要 ffmpeg）、多尺寸预生成（空间换时间，暂不需要）

---

### 2.3 方向③：事件数据管道与分析引擎（P2）

**架构适应性评分：★★★★☆（4/5）**

NATS 已经在承载所有事件，多一个 durable consumer 做事件归档是**成本最低的架构扩展**。真正的工作量在分析查询层。

#### 核心设计决策

**决策 1：数据湖存储格式**

| 选项 | 优点 | 缺点 |
|------|------|------|
| **A. JSONL → S3**（每行一个 JSON 事件，按日期分区） | 零依赖、人类可读、写入极快（append-only） | 查询必须全表扫描；压缩比低 |
| **B. Parquet → S3**（列式存储，按日期+事件类型分区） | 高压缩比（10x vs JSON）、按列查询只扫必要数据、DuckDB/ClickHouse 原生支持 | 写入需要批处理（小文件问题）；需依赖 `arrow`/`parquet` crate |
| **C. ClickHouse 直接写入**（NATS → ClickHouse Materialized View） | 实时查询、SQL 兼容、列存高效 | 运维成本 + 1 个新 DB；ClickHouse 不适合小规模部署 |

**推荐：B（Parquet 落地 S3/MinIO）+ 可选 C 层**。流程：

```
NATS JetStream → event-lake consumer
  → 按 subject 分组（im.room.* → `events/im/`、live.stream.* → `events/live/`）
  → 每 5 分钟或每 1000 条 flush 一个 Parquet 文件到 S3
  → 分区：dt=YYYY-MM-DD/event_type=Message/
```

**关键原则**：写入路径必须**零依赖新服务**——event-lake consumer 是 aero-server 内部的 `tokio::spawn`，使用 `object_store` crate（已依赖 `aws-sdk-s3` 或 MinIO 兼容 API）直接写 S3。

**决策 2：分析查询的引擎选择**

| 选项 | 优点 | 缺点 |
|------|------|------|
| **A. DuckDB 直接查 S3 Parquet** | 嵌入式（无服务）、零运维、SQL 完整 | 单机、并发查询能力有限、不适合高频查询 |
| **B. ClickHouse 服务** | 高并发、实时、列存、适合时间序列 | 运维复杂度显著增加、小规模部署过重 |
| **C. PG 物化视图 + JSONB 查询** | 零新增依赖、复用既有 `analytics.rs` | PG 不适合 OLAP 大规模扫描；JSONB 查询性能差 |

**推荐：A（DuckDB MVP）+ B（ClickHouse 生产）**。DuckDB 可以嵌入到 `aero-cli` 管理工具中，管理员运行 `aero-cli analytics --from 2026-06 --to 2026-07` 即可在 CLI 输出报表。生产环境需要 ClickHouse 做用户自服务的分析看板。

**决策 3：数据脱敏策略**

```
GDPR 要求：被删除用户的数据不能在分析系统中留存
方案：
A. 写入时脱敏：将 participant_id → hash(participant_id + salt)
   → 不能反查到具体用户，但可以统计分析（如按用户去重计数）
   → 推荐 ✓

B. 查询时过滤：保留原始数据，查询时排除已删除用户
   → 需要同步删除状态，复杂度高
   
C. 不保留用户级数据：只聚合不保留明细
   → 无法做用户行为分析，方向③的价值大幅降低
```

**推荐：A + C 混合**——事件湖中保留脱敏后的明细（可做按用户聚合），不保留 PII（display_name、email、avatar_url、IP 地址在写入时 strip）。

---

### 2.4 方向④：SLO 框架与运维闭环（P2）

**架构适应性评分：★★★★★（5/5）**

这是五个方向中**工程投入最低、产出最直接**的方向。现有 Prometheus + Grafana 骨架 + `metrics.rs` 已有暴露点，只需要增加 SLI 埋点 + 告警规则。

#### 核心设计决策

**决策 1：SLI 埋点的代码位置**

```
metrics.rs 当前只有计数器（counter）+ 仪表盘（gauge）
需要新增：
  - 直方图（histogram）：aero_api_latency_ms{endpoint,method}
  - 错误率计数器：aero_api_errors_total{status_code, endpoint}
  - 扇出延迟：aero_messages_fanout_latency_ms

埋点位置：
  - API 延迟：axum middleware（before/after 记录 Duration）
  - 扇出延迟：hub.rs 的 fan_out_raw 入口到 WebSocket send 完成
  - 消息总线延迟：NATS publish → consumer receive 时间差（利用消息中的 seq 时间戳）
```

**决策 2：SLO 目标的合理性**

| SLI | 初始目标 | 窗口期 | 备注 |
|-----|---------|--------|------|
| API 可用性 | **99.9%**（月 ~43 分钟） | 30 天滚动 | P0，这个必须第一个完成 |
| API p95 延迟 | **≤ 500ms** | 5 分钟窗口 | P0 |
| 消息扇出 p99 | **≤ 200ms** | 5 分钟窗口 | P1，本地网络可达 |
| WS 断连率 | **≤ 5%** | 30 分钟窗口 | P1，区分主动断开 vs 异常断开 |
| AI 任务成功率 | **≥ 99%** | 30 天滚动 | P1，区分预算耗尽 vs 服务端错误 |

**不设 P0 的 SLI**：DB 延迟（与 PG 实例强相关、非应用层可控制）、NATS 延迟（基础设施层）。

**决策 3：错误预算消耗与部署门控**

```
错误预算 = 1 - 实际可用性 / SLO 目标可用性

示例：SLO 99.9%，窗口期 30 天
  → 允许的不可用时间 = 43.2 分钟/月
  → 每消耗 33%（~14 分钟）触发 Warning 告警
  → 每消耗 100% 触发 Critical 告警
  → 消耗 ≥ 50% 时自动暂停部署（金丝雀阶段）
```

**初始阶段不做自动门控**——先让错误预算面板跑 2 个窗口期（2 个月），观察指标基线，再配置自动化门控。

---

### 2.5 方向⑤：多租户用量计费基础设施（P2）

**架构适应性评分：★★★☆☆（3/5）**

计费基础设施是**顺序依赖最强**的方向——它强依赖方向③（准确用量数据）和方向②（移动端订阅管理）。没有精确用量数据之前做计费，会产生不可信账单。

#### 核心设计决策

**决策 1：计费数据源——估算 vs 精确**

```
阶段 1（发布日可用）：估算模式
  GET /api/workspaces/:id/usage/cost-estimate
  存储：SUM(b.size) * $0.01/GB/月（来自 blobs 表）
  AI：SUM(ai.prompt_tokens + ai.completion_tokens) * $0.00001（来自 ai_usage 表）
  消息：COUNT(m.id) * $0.0001（来自 messages 表）
  → 误差 ±20%，但零新增基础设施

阶段 2（方向③的 Parquet 事件湖就绪后）：精确模式
  所有用量来自事件湖 replay，与阶段 1 交叉校验
  → 精度达到 ±1%
  
阶段 3（收入规模 > $10K/月）：Stripe 集成
  Stripe Webhook → 订阅状态同步 → 用量硬门控
```

**推荐**：不要跳阶段。阶段 1 做估算可以快速验证定价模型（用户愿意付多少钱？每消息 $0.0001 是否合理？）。阶段 2 做精算。阶段 3 做自动扣费。

**决策 2：超量处理策略——软限 vs 硬限**

| 策略 | 用户体验 | 收入影响 | 实现复杂度 |
|------|---------|---------|-----------|
| **A. 软限**（超量后仪表盘显示警告、功能可用但加延迟） | 最好 | 最低（用户可无视） | 低（只需检查+记日志） |
| **B. 硬限-只读**（超量后不能发消息、不能上传文件，但可读历史） | 较差但可接受 | 中（促转化） | 中（中间件校验） |
| **C. 硬限-完全关停**（超量后服务降级到 Free 层） | 最差（数据不可读） | 最高（强制转化） | 中（降级状态机） |
| **D. Grace Period + 降级**（超量后 7 天宽限期 → 降级到 Free 层） | 大多数场景可接受 | 高 | 中（定时器 + 状态机） |

**推荐：A（初始）+ D（生产）**。MVP 只在 `cost-estimate` 端点显示估算金额，不做任何门控。有营收后再做 D。

## 3. 接口设计建议

### 3.1 API 版本化契约

```
建议新增一个极轻的版本化层，不破坏现有路由：

// routes.rs 当前结构：
pub fn build() -> Router<AppState> {
    Router::new()
        .route("/api/rooms/:id/messages", get(room_history))
        .route("/api/me", get(get_me))
        // ... 100+ 路由
}

// 建议演进方式——阶段 0（零成本，方向②覆盖）：
// 只加可选参数，不改路径
GET /api/rooms/:id/messages?fields=id,blocks,sender_id&since=<ulid>

// 阶段 1（方向② P1）：
// 加版本前缀，v1 是现有路由的 alias，v2 是移动优化版
.nest("/api/v1", v1_routes())
.nest("/api/v2", v2_routes())
```

**关键原则**：
- 现有路由永远可以被移动端调用（降级路径）
- `v2` 只在确实需要破坏性变更时才建立
- 版本头的传播：`X-Aero-API-Version` 响应头始终标注，即使请求无版本标识

### 3.2 事件 Schema 契约

当前 `RoomEvent`/`StreamEvent` 使用 serde `tag="kind"`，但缺乏：

```rust
// 当前（无版本）：
#[serde(tag = "kind")]
pub enum RoomEvent {
    Message { message: Message, ... },
    // ...
}

// 建议（加版本）：
#[serde(tag = "kind")]
pub struct Envelope {
    pub schema_version: u8,        // ← 新增，当前为 1
    pub seq: u64,                  // ← 已有
    pub timestamp: i64,            // ← 微秒级 Unix 时间戳
    pub event: RoomEvent,          // ← 原事件体
}
```

事件湖写入器检查 `schema_version`，版本不匹配时写死信队列（DLQ）。这提供 schema 演化路径——版本 2 的 consumer 可以在同一管道中与版本 1 共存。

### 3.3 自动化规则接口

```
POST /api/workspaces/:id/automation-rules
{
  "name": "欢迎新成员",
  "enabled": true,
  "trigger": {
    "type": "member_joined",
    "room_id": "room_xxx"          // 限定到特定房间，留空 = 所有房间
  },
  "conditions": [                  // 可选，所有条件为 AND
    { "type": "role", "in": ["member"] }
  ],
  "actions": [
    {
      "type": "send_message",
      "room_id": "room_yyy",
      "blocks": [
        { "type": "text", "content": "欢迎 {{trigger.member.display_name}} 加入！" },
        { "type": "mention", "participant_id": "{{trigger.member.id}}" }
      ]
    }
  ]
}

响应：
{
  "id": "rule_ulid",
  "status": "active",
  "execution_count": 0,
  "last_executed_at": null
}
```

**设计原则**：规则是工作区级的资源（`workspace_id` 外键），跨房间可见。这允许管理员创建跨频道自动化规则。

## 4. 技术选型建议

### 4.1 各方向所需的新依赖

| 方向 | 新依赖 | 评估 | 替代方案 |
|------|--------|------|---------|
| ② Mobile API | `image` + `image-webp` | ✅ 纯 Rust、成熟、2000+ 星 | 调用外部 ImageMagick（运维成本） |
| ③ 数据管道 | `object_store`（已有 `aws-sdk-s3`，无需新增） | ✅ 零新增依赖 | 无 |
| ③ 分析查询 | `duckdb` (crate) | ✅ 嵌入式、零运维、Apache 2.0 许可 | ClickHouse（运维重） |
| ④ SLO | `prometheus` 直方图（已有 `metrics-exporter-prometheus`） | ✅ 无新增依赖 | 无 |
| ⑤ 计费 | `stripe` (crate) | ✅ Rust 绑定、维护活跃、产品成熟 | 自建支付引擎（不推荐） |
| ① 自动化 | 无新外部依赖 | ✅ 纯胶水代码 | 无 |

**核心结论**：五个方向**都不需要引入新的核心技术栈**。所有新增依赖都是成熟的、窄范围的库。这是当前架构设计质量的有力证明——基础设施已就位，只需要业务流程层叠加。

### 4.2 自建 vs 采购的决策矩阵

| 方向 | 自建 | 采购/集成 | 推荐 |
|------|------|----------|------|
| ① 自动化引擎 | 规则引擎、动作执行器、执行日志 | 可视化拖拽编辑器（可以有第三方 JS 库） | **自建引擎 + 采购 UI 组件**。引擎是核心竞争力；UI 可以基于 React Flow 等开源库 |
| ③ 数据管道 | event-lake consumer、Parquet flush | 分析查询入口可以用 Metabase / Grafana（已有） | **自建数据湖 + 复用 Grafana**。Grafana 已部署，不需要自建看板 UI |
| ④ SLO | 指标埋点、告警规则 | Grafana（已有） + PagerDuty / Opsgenie | **自建指标 + 采购告警分派**。开源的 Alertmanager 可以满足初始需求 |
| ⑤ 计费 | 用量计算、配额检查、状态机 | Stripe（支付处理）+ Recurly / Chargebee（订阅管理） | **自建计量 + Stripe。** Stripe 不做的用量聚合和配额检查需要自建 |

### 4.3 特定技术领域的评估

**事件湖的序列化格式**：建议使用 `serde_json::to_string` 写入 JSONL（初期），后期迁移到 `apache-avro` crate 写 Avro（有 schema 的二进制格式）。不推荐 Cap'n Proto / FlatBuffers——团队对 JSON 和 Avro 更熟悉，运维负担低。

**缩略图管线**：`image` crate + `webp` feature，输出 200px 宽的 WebP 缩略图。不做多尺寸、不做渐进式加载（P2 阶段，优先级低于基本缩略图）。

**Stripe 集成最小路径**：只处理 `checkout.session.completed` 和 `customer.subscription.updated` 两个 webhook 事件。不处理发票、退款、争议。`stripe` crate 的 `Webhook` 类型已经封装了签名验证。

## 5. 实施路线图

### 5.1 整体优先级视图

```
时间轴 →
├──────────────────────────────────────────────┤
│ P0（本月）                                     │
│  ├─ 方向④：SLI 埋点（~150 行）                  │ ← 立即启动
│  ├─ 方向⑤：cost_estimate 端点（~80 行）          │ ← 允许快速验证定价
│  └─ 方向②：?fields= + ?since=（~80 行）          │ ← 零成本移动端优化
│                                               │
├──────────────────────────────────────────────┤
│ P1（下个月）                                    │
│  ├─ 方向①：MVP 自动化引擎（~400 行 Rust）        │ ← 核心差异化竞争力
│  └─ 方向③：event-lake consumer（~150 行）       │ ← 计费前置依赖
│                                               │
├──────────────────────────────────────────────┤
│ P2（季度内）                                    │
│  ├─ 方向①：自动化 UI（~500 行 JS）               │ ← 用户面完整
│  ├─ 方向②：delta sync + unread-rooms（~150 行） │ ← 移动端完整
│  ├─ 方向③：分析看板 API（~200 行）               │ ← 数据驱动决策
│  ├─ 方向④：错误预算面板 + 告警规则（YAML）        │ ← 运维闭环
│  └─ 方向⑤：Stripe Webhook（~300 行）            │ ← 收入闭环
│                                               │
└──────────────────────────────────────────────┘
```

### 5.2 阶段里程碑与并行 Track

```
Track A（产品差异化——方向①）
  Sprint 1: 规则 CRUD API + automation_rules 表迁移 + 规则持久化
  Sprint 2: 触发器匹配器（Message created, Member joined）
  Sprint 3: 动作执行器（send_message + create_task）+ 执行日志
  Sprint 4: 自动化 UI（表单驱动的规则创建页面）
  Sprint 5: 高级触发器（定时、直播状态变更、Webhook inbound）
  Sprint 6: 规则模板市场（预置 10 个常见模板）

Track B（数据与计费——方向③→⑤）
  Sprint 1: event-lake consumer (NATS → S3 Parquet)
  Sprint 2: DuckDB 分析查询 CLI + cost_estimate 端点（估算）
  Sprint 3: 分析看板 API + Grafana dashboard
  Sprint 4: plan_tiers 表 + 配额检查中间件（软限）
  Sprint 5: Stripe Webhook 集成
  Sprint 6: Grace Period + 自动降级

Track C（运维与移动端——方向④+②）
  Sprint 1: SLI 埋点（延迟直方图 + 错误率计数器）
  Sprint 2: SLO 文档 + 错误预算 Grafana 面板
  Sprint 3: 告警规则（PrometheusRule）+ 燃烧率告警
  Sprint 4: 移动端优化（delta sync, push inline reply, 缩略图）
  Sprint 5: API 版本化（v1 → v2 过渡方案）
  Sprint 6: On-Call runbook 文档化
```

### 5.3 风险点与缓解策略

| 风险 | 影响方向 | 概率 | 严重度 | 缓解 |
|------|---------|------|--------|------|
| 自动化规则数量膨胀导致事件处理延迟 | ① | 中 | 高 | 触发器索引（只加载匹配类型的规则）+ Redis 缓存活跃规则 + 规则执行超时（5s 硬限） |
| 计费系统故障导致正确用户被错误降级 | ⑤ | 低 | 极高 | 计费门控 fail-open（计费不可达时放行，记异常日志）+ 所有降级操作需人工确认（初始阶段） |
| 事件湖 Parquet 文件小文件过多 | ③ | 中 | 中 | 5 分钟/1000 条 flush 策略 + 定期 compaction（aero-cli compact） |
| SLO 告警疲劳导致告警被忽略 | ④ | 高 | 中 | 燃烧率告警（而非固定阈值）+ 告警必须持续 5 分钟才触发 + 初期只设 3 个核心告警 |
| 移动端 API 版本化导致现有客户端兼容性问题 | ② | 中 | 高 | 所有新功能用可选参数（非破坏性），版本化只用于确实需要破坏性变更时 |
| 方向①和方向⑤并行时 Rust 开发者资源不足 | ①⑤ | 中 | 高 | 方向①（自动化引擎）需要 Rust 深耕；方向⑤的 Stripe 集成可以外包或使用 Node.js 独立服务（不耦合现有 Rust 代码） |

### 5.4 关键依赖图

```
方向④ (SLO) ─── 独立，无前置依赖，随时可启动
方向① (自动化) ─── 依赖: 无（复用既有 NATS + BotRepo）
方向② (Mobile) ─── 依赖: 无（现有 API 上加参数）
方向③ (数据湖) ─── 依赖: 无（新增 NATS consumer）
方向⑤ (计费) ─── 依赖: 方向③的精确用量数据 → 但 MVP 估算模式不依赖

实际启动顺序建议：
  1. 方向④ P0（1 周，零风险快速赢）
  2. 方向① Track A + 方向③ Track B 并行启动（互不阻塞）
  3. 方向② Track C Sprint 4 之后插入（与 Track A/B 不争共享模块）
  4. 方向⑤ Track B Sprint 4 之后（需要方向③的准确数据，但估算模式可以在方向③之前就启动）
```

### 5.5 退出/止损标准

每个方向应有明确的 Go/No-Go 决策点，避免投入过大但未达预期：

| 方向 | 决策点 | 通过标准 | 止损条件 |
|------|--------|---------|---------|
| ① 自动化 | Sprint 2 结束时 | 有 5 条内部测试规则可正常触发+执行 | 规则匹配器延迟 > 50ms/事件（表明 O(N) 不可行） |
| ② Mobile | Sprint 4 结束时 | delta sync 端到端延迟 < 1s | 增量 sync 的数据一致性无法验证（漏消息） |
| ③ 数据湖 | Sprint 1 结束时 | Parquet 文件可被 DuckDB 正确查询 | 事件丢失率 > 0.1%（表明 consumer 不可靠） |
| ④ SLO | Sprint 1 结束时 | Grafana 面板显示有意义的延迟分布 | 指标噪音过大（p95 = p99，表明粒度不够） |
| ⑤ 计费 | Sprint 5 结束时 | Stripe checkout 可完成订阅创建 | Stripe Webhook 签名验证无法通过（安全风险） |

---

## 总结

**总体评价**：这是一个非常务实的战略规划文档。五个方向的区分边界清晰，与既有的 56+ 份分析逐项对照确保无重叠。更重要的是，**每个方向的可行性分析都锚定在代码级证据上**（哪些模块可以复用、哪些路径已经存在、哪些是新增），这种接地气的分析方法使文档的可执行性非常高。

**最大的架构优势**：NATS 事件总线作为统一的事实源，使得方向①（自动化引擎的触发器）和方向③（数据管道的事件归档）几乎可以零成本起步。

**最大的架构风险**：方向⑤（计费）的顺序依赖被低估了。文档提到的「方向③是方向⑤的前置依赖」是正确的，但实际还有一层——没有方向②（移动端 API），计费的管理界面（订阅管理、用量查看、发票下载）无法在移动端完成。建议在计划中增加这个依赖关系。

**核心建议**：立即启动方向④的 SLI 埋点（1 周内可完成 P0），这不会与任何其他方向争抢开发资源，且为后续所有方向提供运维基线。方向①和方向③可以作为并行双 Track 立即启动。

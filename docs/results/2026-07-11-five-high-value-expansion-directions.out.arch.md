以下是我的架构分析。

---

# 架构分析：Aero IM 五个高价值扩展方向

> 基于对 `docs/requirements/2026-07-11-five-high-value-expansion-directions.md` 的代码验证结果和全量代码审查。

---

## 一、架构评估

### 1.1 当前架构的显著优势

**事件驱动骨架（NATS JetStream → Hub → WebSocket）是系统最坚固的决策**。整个系统的实时性、水平扩展性和解耦能力均源于此。具体而言：

- **进程内扇出 vs 直接 NATS 扇出**：`Hub::fan_out_raw` 用 bounded `mpsc` 在本进程扇出，避免 NATS 成为 O(N) 瓶颈。`run_bus_listener` 的 `NotifyBatch` 展开策略（在消费侧展开而非发布侧），正确避免了 NATS subject 爆炸。
- **Durable vs Ephemeral consumer 的分层隔离**：`im.room.*` 用 durable（at-least-once 保证消息不丢），`live.stream.*` 用 ephemeral（丢几条弹幕可接受）— 精准对应两类数据的不同可靠性要求。
- **Crate 划分遵循依赖方向**：`aero-common`→`aero-bus/storage/auth/signaling`→`aero-im-*`/`aero-live-*`→`aero-server`，无成环，每 crate 职责单一。

**集群状态走 Redis sorted-set 而非进程内存**，这是正确的分布式架构决策（presence、viewer、roster），避免了多实例间的状态不一致。

**预算队列（CostBudget + AiWorker）** 的 fail-closed on spend 设计，防止 AI 成本失控，同时用 defer 而非丢消息实现优雅降级。

### 1.2 关键架构债务

| 类别 | 问题 | 影响域 |
|------|------|--------|
| **数据模型** | `Block::File` 无 `metadata` 字段 | 方向四（多模态理解）需要先迁移数据模型，工作量比文档估计大 |
| **领域渗透** | 代码重构中重叠 (`minimize-codebase` 在途)，文件删除与创建同时出现 | 叠加新变更风险高，违反 `AGENTS.md` §4.2 的「禁止在重构中途叠加结构迁移」 |
| **单点依赖** | `ai_jobs` 表作为唯一 AI 作业队列，`AiWorker` 轮询模式在 PG 事务中 `FOR UPDATE SKIP LOCKED` | PG 在高 AI 负载下可能成为瓶颈；无内置背压到 NATS 或 Kafka 层 |
| **媒体 seam 未接线** | `call_bridge_supervisor` + `SfuMediaSession` 已建已测但生产未接线 | 跨节点通话仍为半成品，`ensure_egress` 无调用方 |
| **编译时嵌入迁移** | `sqlx::migrate!("../../migrations")` 将整个 `migrations/` 目录编译进二进制；加迁移后需先 build 再 migrate，**但 CI 无自动化校验步骤** | 部署流程脆弱，易出现「迁移被静默忽略」的生产事故 |
| **IDS 定义** | `define_id!` 宏定义在各个 crate 内（`common/src/ids.rs`），增量加 ID 时需要修改公共 crate | 新功能开发中隐含依赖 |

### 1.3 关键设计决策复盘

**决策：使用单一 PG 库（Postgres 17 + pgvector + pg_trgm）而不是引入专门的搜索引擎（Elasticsearch）或向量数据库（Pinecone/Weaviate）**

- 评估：**正确决策**。在项目当前阶段（~100 张表），单一存储降低了运维复杂度和启动时间。pgvector 的 HNSW 索引对 1024 维嵌入向量性能可接受。但需要注意：当 `ai_jobs` 队列深度持续 > 10 万条时，`SKIP LOCKED` 轮询会带来显著 PG 锁争用。

**决策：纯 Rust 实现 SFU（str0m）而非使用成熟的媒体服务器（mediasoup/LiveKit/Janus）**

- 评估：**高风险高回报决策**。str0m 是全纯 Rust DTLS-SRTP 实现，与现有的 `aero-live-*` 技术栈（rml_rtmp、mpegts）一致。优势是零 C 依赖、零 FFI 开销、与 tokio 异步模型自然契合。风险是 `SfuMediaSession` 生产无实例化，真正的 SDP 握手 + DTLS 密钥协商 + SRTP 流的路由从未在真实浏览器对端中验证过。**方向二（告警事件）不依赖此决策，但方向四（多模态视频理解）和方向五（工作流）可能间接依赖直播基础设施的成熟度。**

**决策：NATS JetStream 作为唯一的事件总线，不引入 Kafka**

- 评估：**与团队规模和运维能力匹配**。NATS 的运维复杂度远低于 Kafka，足够支持当前 ~16 crate/181 模块的规模。但在方向二（告警/事件）中，如果事件产生速率突然暴增（如监控告警风暴），NATS 的无持久化消费者（ephemeral）可能在积压场景下产生不可预测的行为。建议在方向二的架构设计中，为告警通道使用独立的 subject 和 consumer 配置。

---

## 二、扩展方向：深入架构分析

基于代码验证报告修正后的五个方向，从架构层面重新评估。

### 方向一（P0）：组织网络智能

**当前状态评估**：
验证确认了文档主张——`analytics.rs` 确实只有工作区级粗粒度聚合（`overview()`/`workspace_summary()`），无任何社交网络指标。`recommendations.rs` 基于频道活跃度排序，无社交图特征。但文档低估了一个关键问题：**数据的分布性**。社交图谱数据散落在多个表中：
- `messages`（6 亿行量级）— 回复关系通过 `thread_root_id` 表达
- `reactions` — 反应互动
- `receipts` — 已读回执
- `stream_follows` — 关注关系
- `org_chart` — 汇报关系
- `user_groups` — 用户组
- `mentions` — @提及（内嵌在消息 blocks 中，需要解析）

**真正的架构挑战不在计算，而在数据提取**。特别是 `mentions` 需要解析 `Block::Mention` 的 JSONB 数组，在大表上做 JSONB 路径提取的查询性能堪忧。

**建议方案**：不要做实时社交图计算。采用**物化+增量维护**策略：

```
Option A（物化视图路径）：在 PG 中建 social_graph 物化视图，CRON 定时 REFRESH
  → 优势：零新代码、零新存储
  → 劣势：REFRESH MATERIALIZED VIEW 会锁表，大表下不可接受

Option B（事件驱动增量路径）：在 run_bus_listener 中旁路写 social_graph_edges 表
  → 每条消息/反应/提及 → 异步 INSERT/UPDATE 边表（src, dst, edge_type, weight, last_seen）
  → 优势：低延迟、增量更新、不阻塞主流程
  → 劣势：需要额外事务逻辑、审计一致性成本
  → 推荐度：★★★★★

Option C（独立图数据库路径）：引入 RedisGraph / Neo4j 作为社交图存储
  → 优势：原生图查询、最短路径、社区发现等分析能力
  → 劣势：引入新基础设施、运维复杂度增加、小团队难维护
  → 推荐度：★★☆☆☆（P2 后再评估）
```

**数据模型设计建议**：

```sql
CREATE TABLE social_graph_edges (
  workspace_id UUID NOT NULL,
  src_participant_id UUID NOT NULL,
  dst_participant_id UUID NOT NULL,
  edge_type SMALLINT NOT NULL, -- 1=message, 2=reply, 3=reaction, 4=mention, 5=follow
  weight REAL NOT NULL DEFAULT 1.0,
  last_seen TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  PRIMARY KEY (workspace_id, src_participant_id, dst_participant_id, edge_type)
);
```

关键点：PK 同时作为 UPSERT 键，每个互动事件执行 `INSERT ... ON CONFLICT UPDATE weight=weight+1, last_seen=NOW()`。以写优化换读简化。

**产品架构影响**：
- `aero-storage` 新增 `social_graph_repo.rs`（写边表）和 `org_analytics_repo.rs`（读指标）
- `aero-server` 新增 `org_analytics.rs`（REST 端点）+ `org_graph_worker.rs`（后台定时任务：计算中心性/桥接/孤岛）
- Web 端新页面（`org-dashboard.html`/`.js`），可复用现有 `web/app.js` 的渲染基础设施
- `routes/routes.rs` `.merge(crate::org_analytics::routes())`

### 方向二（P0）：告警与事件响应

**当前状态评估**：
验证确认了 webhook 接收（`webhooks.rs`）存在但只做消息转发，无事件语义。`scheduled.rs` 支持定时发送但无语义化。`notifications.rs`（实际为 `notif_prefs.rs`）支持 `NotificationKind` 和 `importance_score`，但方向二的 on-call 调度需要**全新的数据模型和调度引擎**，不能仅靠复用现有基础设施。

**真正的架构挑战**：

1. **On-Call 排班是带状态的复杂调度问题**（轮转规则、覆盖、交接、时区、节假日），不是简单的 CRUD。正确的状态机设计比创建事件频道复杂一个数量级。
2. **告警风暴防护**：当监控系统在一分钟内产生 1000 条告警时，系统不能创建 1000 个事件频道。需要告警去重、聚合、抑制（dedup/aggregation/suppression）逻辑。
3. **分级升级的幂等性**：P0 告警 5 分钟未 ACK → P1 升级 → 再次未 ACK → 电话升级。每条升级路径的幂等键需要精确设计，防止并发升级。

**建议方案**：

```
On-Call 排班引擎：
  ┌─────────────────────────────────────────────────┐
  │  ScheduleStore (PG)                              │
  │   ├── on_call_schedules (workspace_id, name,     │
  │   │     timezone, rotation_type, config_jsonb)   │
  │   ├── on_call_rotations (schedule_id, starts_at, │
  │   │     ends_at, participant_id)                 │
  │   └── on_call_overrides (rotation_id, date,      │
  │         replaced_by)                             │
  └─────────────────────────────────────────────────┘
                         ↓
  ┌─────────────────────────────────────────────────┐
  │  ScheduleEvaluator (read-through cache in Redis) │
  │  → get_on_call(schedule_id, timestamp) → UUID[]  │
  │  → 使用 Redis Sorted Set 过期缓存                 │
  └─────────────────────────────────────────────────┘
                         ↓
  ┌─────────────────────────────────────────────────┐
  │  IncidentOrchestrator                            │
  │  → 接收 webhook → 去重(alert_fingerprint)        │
  │  → 匹配规则 → 创建事件频道 → @值班人              │
  │  → 启动 ACK 计时 → 升级计时器                     │
  └─────────────────────────────────────────────────┘
```

**关键设计决策**：告警路由器和事件频道管理是否拆分为独立 crate？

- **选项 A：放在 `aero-server` 中作为 routes 扩展**
  - 优势：快速上线、复用 Auth/WS 基础设施
  - 劣势：`aero-server` 已经在 181 模块上增长，方向二可能再加 20-30 模块
  - 推荐度：★★★☆☆ 短期可行

- **选项 B：新 crate `aero-incident`**
  - 优势：独立测试、独立演进、不与 IM 核心耦合
  - 劣势：需要自己的 `routes()` 装配、需要引用 `aero-server` 的 Hub 和 Auth
  - 推荐度：★★★★☆（方向二成熟后应该走此路径）

- **选项 C：放在 `aero-im-core` 旁的新 crate `aero-ops`**
  - 优势：命名语义清晰、与 `aero-im-core` 并行
  - 劣势：同上 + 需要明确 ops 与 IM 的边界
  - 推荐度：★★★★★（长期推荐，ops 作为独立产品线）

### 方向三（P1）：数据仓库与嵌入式分析

**当前状态评估**：
验证确认 `export_job.rs` 不存在，实际是 `me_export.rs` / `conversation_export.rs`。文档准确识别了「零结构化导出」的缺口。

**关键设计决策**：

**不应每条消息写入时都向数据仓库同步（CDC 过度设计）**。企业客户需要的不是实时数据湖，而是**可操作的批量导出**。推荐采用：

```
Option A：Scheduled Parquet Export（推荐）
  → 定时（每 6/12/24h）扫描上次导出后的增量数据
  → 生成 Parquet 文件（列存，压缩率 ~5x over JSON）
  → 上传到客户指定的 S3/GCS bucket 或 Snowflake Stage
  → 优势：实现简单、不增加写路径延迟、客户可直接用任何 BI 工具
  → 劣势：不是实时（对 IM 数据而言完全可接受）
  → 工作量：中

Option B：NATS → 连接器（概念正确但工程量过高）
  → 为每个实体实现 outbox 模式 changefeed
  → 每个写操作多发一条 NATS 消息
  → 实现 Snowflake/BigQuery/Parquet 三个适配器
  → 优势：实时、架构优雅
  → 劣势：增加主路径延迟和复杂度、三个连接器的维护成本
  → 工作量：极大（不推荐 P1 阶段实施）

Option C：Postgres FDW（外部数据包装器）
  → 客户在数据仓库中创建 FDW 指向 Aero 的 PG
  → 优势：零代码、零 ETL
  → 劣势：性能差、安全性问题（需要外网 PG 端口暴露）
  → 推荐度：★★☆☆☆
```

**架构影响**：
- 新 crate 或现有 crate 扩展：`aero-export`（含 Parquet 编解码依赖 `arrow`/`parquet` crate）
- 定时器 `run_export_sweep` 在 `bin/boot/` 注册
- 速率控制：控制每轮导出的行数上限，防止 PG I/O 打满

### 方向四（P1）：多模态内容理解

**代码验证发现的关键事实**：文档声称 `Block::File` 有 `metadata` 字段（JSONB）可直接存储 AI 标签，**事实是 `Block::File` 只有 `{blob_id, kind, name, size}`**，无 `metadata` 字段。

**这是架构层面的重要发现**。方向四的数据模型前置工作量不等于「在现有管线中插一个新步骤」，而是：

1. **更改 `Block::File` 结构**（加 `metadata: Option<serde_json::Value>`）→ 影响：线格式（WS 帧、NATS 消息体）和存储格式（PG JSONB 列）。需要版本化兼容或对旧行做 fallback。
2. **或者创建独立的 `file_metadata` 关联表**（`file_id → metadata JSONB`，1:0..1 关系）→ 不影响现有 Block 线格式，但查询时需要 JOIN。
3. **OCR 文本存储**：文档建议的 `message.editing_context` 不存在。正确做法是在 `Message` 或 `Block` 上加新字段，或使用 `Message.metadata`（已有 `serde_json::Value` 字段）来承载 OCR 结果。

**推荐方案**：

```
┌────────────────────────────────────────────────────────┐
│                    Upload Pipeline                      │
│                                                         │
│  用户上传图片                                              │
│    → content_sniff（字节嗅探，现有）                         │
│    → av_scan（病毒扫描，现有）                              │
│    → 存储为 Block::File { blob_id, kind, name, size }    │
│    → 返回给客户端（现有流程终止于此）                        │
│                                                         │
│  ═══ 新增步骤（异步，不阻塞上传响应）══════════════════════│
│    → 视觉审核（Anthropic Vision / ONNX NSFW 分类器）       │
│    → AI 标注（标签 + 描述）                                │
│    → 截图 OCR（PaddleOCR / Tesseract）                    │
│    → 写入 Message.metadata 或 file_annotations 关联表    │
│    → 更新搜索索引                                         │
└────────────────────────────────────────────────────────┘
```

**两种数据模型选项**：

| | 选项 A：扩展 Block::File | 选项 B：独立关联表 |
|---|---|---|
| **变更影响** | 改线格式 + 改存储 + 兼容旧行 | 新增表，不影响现有线格式 |
| **查询性能** | 单表查询，无 JOIN | 需要 LEFT JOIN |
| **迁移成本** | 需处理存量 Block::File 的 serde 反序列化兼容 | 只需增量写新表 |
| **推荐度** | ★★★☆☆（P2 中期架构升级时可考虑） | ★★★★★（P1 快速上线） |

推荐采用选项 B：新增 `file_annotations` 表（`blob_id UUID PK, tags TEXT[], ai_description TEXT, ocr_text TEXT, nsfw_score REAL, moderation_action TEXT, created_at TIMESTAMPTZ`）。后续 PG 大版本升级或架构重构时再考虑合并回 `Block::File`。

**AI 模型选型**：

| 任务 | 推荐方案 | 备选方案 | 延迟预算 |
|------|---------|---------|---------|
| NSFW 检测 | ONNX 本地部署（CLIP-based） | Anthropic Vision API | < 500ms |
| 图像标注 | Anthropic Vision（复用现有 aero-ai） | BLIP-2 / Git (ONNX) | 1-3s |
| OCR | PaddleOCR（Rust bind 或其他 HTTP 服务） | Tesseract / Google Vision API | < 1s |
| 场景描述 | Anthropic Vision | LLaVA (ONNX) | 2-5s |

### 方向五（P2）：流程自动化引擎

**当前状态评估**：
验证确认了审批流、任务、定时消息、auto-mod 各自孤立，无 `workflow_rules` 表或 `WorkflowEngine`。文档的判断准确。

但文档低估了一个架构挑战：**这些孤岛模块的触发点不一致**。

| 模块 | 触发方式 | 输出方式 |
|------|---------|---------|
| 审批流 | REST API（`POST /api/approvals`） | 直接 DB 写 + 消息通知 |
| 任务系统 | REST API | DB 写 + WS 帧 |
| 定时消息 | tokio interval | DB 写 + NATS publish |
| 周期消息 | tokio interval | DB 写 + NATS publish |
| auto-mod | `run_bus_listener` 旁路 | 直接 DB 写 + WS 帧 |
| bot_dispatch | NATS consumer（`aero-bot`） | HTTP Webhook |

**要让它们可组合，需要为每个模块的「状态变更」发标准化总线事件**。这涉及对既有模块的侵入式修改。

**推荐架构**：

```
┌─────────────────────────────────────────────────────┐
│                  事件总线 (NATS)                       │
│  subjects: im.workflow.* / im.room.* / live.stream.* │
└────────────────────┬────────────────────────────────┘
                     │ 订阅
┌────────────────────▼────────────────────────────────┐
│  WorkflowEngine                                      │
│                                                      │
│  1. 规则注册中心（workflow_rules 表）                   │
│     - workspace_id, name, enabled                    │
│     - trigger: { type: "message_pattern" |           │
│                  "scheduled_cron" | "webhook" |       │
│                  "task_status" | "reaction_added" },  │
│                config: { ... }                       │
│     - conditions: [{ field, operator, value }]       │
│     - actions: [{ type, params }]                    │
│                                                      │
│  2. 触发器匹配器（事件→规则索引）                        │
│     - 预编译条件表达式 → Boolean 求值                   │
│     - 短路优化：按 workspace 分区索引                   │
│                                                      │
│  3. 动作执行器                                        │
│     - send_message → ImService                        │
│     - create_task → TaskRepo                         │
│     - start_approval → ApprovalRepo                   │
│     - call_webhook → reqwest                         │
│     - update_channel_topic → ChannelRepo              │
│     - add_reaction → ReactionRepo                     │
│     - invoke_bot → BotDispatch                        │
│                                                      │
│  4. 执行监控                                          │
│     - execution_log 表 (workflow_id, trigger_event,   │
│       action_index, status, error, started_at)        │
│     - 失败重试（衰减退避 1s/5s/30s/5m → dead_letter）  │
│     - 通知规则创建者                                   │
└─────────────────────────────────────────────────────┘
```

**关键设计决策**：WorkflowEngine 应该是独立 crate 还是 `aero-server` 内模块？

- **独立 crate `aero-workflow`**：推荐。理由：
  1. 规则模型和执行引擎与 IM 核心解耦
  2. 可独立单元测试（不依赖 Hub/WS 基础设施）
  3. 未来可暴露 gRPC API 给外部系统
  4. 与 §4.1 的「feature-first 单位是 crate」原则一致

**规则 DSL 设计建议**（JSON 表达式树）：

```json
{
  "trigger": {
    "type": "message_pattern",
    "config": {
      "room_ids": ["<uuid>"],
      "pattern": "紧急|P0|SEV1|incident",
      "match_mode": "regex"
    }
  },
  "conditions": [
    { "field": "sender.role", "operator": "in", "value": ["admin", "owner"] },
    { "combine": "any", "rules": [
      { "field": "message.blocks[].type", "operator": "contains", "value": "Text" },
      { "field": "message.has_file", "operator": "eq", "value": true }
    ]}
  ],
  "actions": [
    { "type": "create_task", "params": {
      "title": "处理紧急消息: {{message.searchable_text | truncate(50)}}",
      "assignee_resolver": "on_call", 
      "schedule_id": "<uuid>",
      "priority": "P0"
    }},
    { "type": "send_message", "params": {
      "room_id": "<operations-room-uuid>",
      "text": "🚨 @here 已创建事件任务, 请查看 #{{task.id}}"
    }},
    { "type": "call_webhook", "params": {
      "url": "https://hooks.example.com/incident",
      "method": "POST",
      "body_template": "{{message | to_json}}"
    }}
  ]
}
```

**模板引擎**：需要简单变量插值（`{{...}}`），推荐 `tinytemplate` 或 `tera` crate，避免引入完整的 JS 引擎。

---

## 三、接口设计原则

### 3.1 新方向应遵循的接口契约

基于系统现有的良好做法，强制要求：

1. **新功能必须有 `pub fn routes() -> Router<AppState>`**
   - 在 `routes/routes.rs` 中 `.merge()` 装配
   - 不得在 `routes.rs` 中出现新功能的具体路由逻辑
   - 这一模式已在 ~30 个子模块中验证稳定

2. **新数据访问必须通过 `XRepo` 仓储模式**
   - 仓储位于 `aero-storage/src/x.rs`
   - 构造：`XRepo::new(s.pg.clone())` — 不引入额外的状态传递
   - 方法签名：必须 owner/room/workspace-scoped
   - 事务控制：在仓储层提供 `with_tx` 方法可供路由层包事务

3. **新总线消费者（bot/worker）必须注册在 `bin/boot/` 下**
   - bot 注册（`agent_bot`, `ooo_bot` 等）在 `background.rs`
   - 定时器在 `bin/boot/timers.rs` 或其他定时器模块
   - 必须支持 env gate（OPT-IN 模式）
   - 必须实现 `MissedTickBehavior::Skip`（对定时器而言）
   - 必须 fail-open log-skip（对 bot 而言）

4. **新事件类型必须遵循 `tag="kind"` 序列化约定**
   - 避免命名冲突（已有 `call_kind` / `notify_kind` 作为 rename 先例）
   - 前端兜底：`event.call_kind || event.kind`

### 3.2 是否需要新的抽象层

**是的，方向二和方向五需要**：

| 方向 | 需要的新抽象层 | 原因 |
|------|--------------|------|
| 一（组织网络） | 无。重用现有 `Repo` + `routes()` 模式 | 仅增加仓储和路由即可 |
| **二（告警事件）** | **Scheduler trait**：需要抽象 on-call 排班的存储后端的接口（PG vs Redis），以便未来支持不同背衬 | 排班评估是高频操作（每次 webhook），需要缓存层 |
| **二（告警事件）** | **Alert Router trait**：告警来源适配器接口 | 支持 Prometheus/Grafana/Datadog/Zabbix 等不同格式 |
| 三（数据仓库） | **ExportConnector trait**：`trait ExportConnector { fn write_batch(&mut self, rows: &[Record]) -> Result<()>; fn close(&mut self) -> Result<()>; }` | 不同目标（S3 Parquet / Snowflake / BigQuery）实现不同 |
| 四（多模态） | **ContentAnalyzer trait**：`trait ContentAnalyzer { fn analyze(&self, blob_id, &[u8], kind) -> AnalyzerResult; }` | 支持不同模型后端（Anthropic Vision / ONNX / Google Cloud Vision）的热切换 |
| **五（工作流）** | **规则引擎 DSL 解析器/编译器和动作调度器** — 这是五个方向中最大的新抽象层 | 需要支持条件树预编译、动作链编排、幂等执行 |

### 3.3 向后兼容性策略

1. **`Block::File` 扩展**（方向四前置条件）：
   - 必须保持旧 JSON 格式的 `deserialize` 兼容性（使用 `#[serde(default)]` + `Option`）
   - 新格式 `{"blob_id":..., "kind":..., "name":..., "size":..., "metadata": {...}}` 或使用独立关联表
   - 写路径：所有新写入使用新格式
   - 读路径：旧行反序列化时 `metadata` 为 `None`

2. **新增总线事件类型**：
   - 只加 variant、不改既有 variant 的结构
   - 消费者逻辑：`if let Err(e) = handle_new_event(...)` — 不中断既有事件处理

3. **Route 路径规划**：
   - 新功能使用独立路径前缀：`/api/analytics/org/...`、`/api/incidents/...`、`/api/workflow/...`
   - 不修改现有 REST API 的路径和响应结构

---

## 四、技术选型

### 4.1 需要引入的新依赖

| 方向 | 需要的依赖 | 理由 | 风险评估 |
|------|-----------|------|---------|
| 一（组织网络） | `petgraph` (Rust 图处理库) | 计算中心性/桥接/社区发现等图指标 | 纯 Rust 库，零风险 |
| 三（数据仓库） | `arrow` + `parquet` crate | 生成列存 Parquet 文件 | 原生 Rust 实现，稳定；编译时间增加 ~30s |
| 三（数据仓库） | `object_store` | 抽象 S3/GCS/Azure Blob 写入 | Arrow 生态库，稳定 |
| 四（多模态） | PaddleOCR HTTP service（非 Rust） | OCR 文本提取 | 需要部署额外服务；可选 Google Vision API（无自建服务） |
| 五（工作流） | `tinytemplate` / `tera` | 规则动作中变量插值 | 零风险，纯模板引擎 |
| 五（工作流） | `rhai` / `boa_engine`（可选） | 高级规则条件表达式 | WASM/Rhai 沙箱化执行；仅高性能需要时引入 |

### 4.2 外部服务依赖决策

| 方向 | 自建 vs 采购 | 决策建议 |
|------|-------------|---------|
| 方向二 On-Call 排班 | **自建** | On-Call 调度的核心逻辑是 IM 产品差异化的关键，不宜外包给第三方时间表服务。PagerDuty API 集成可作为消费端（发送升级通知），而非「PagerDuty 是 On-Call 数据源」。 |
| 方向四 NSFW 分类 | **自建 + ONNX** | 开源 NSFW 分类器（如 `nsfwjs` 的 TensorFlow 模型 → ONNX 转换）准确率足够高（>95%），且无 API 调用成本。Anthropic Vision 作为备选（高准确度低延迟要求时使用）。 |
| 方向四 OCR | **自建（PaddleOCR）** | PaddleOCR 在印刷体 OCR 上准确率领先，可 Docker 化部署为独立 HTTP 服务。Google Cloud Vision API 作为备选。 |
| 方向三 Parquet/S3 | **自建** | 使用 `arrow`/`parquet` Rust 库，输入 `object_store` 写入 S3，纯自建。 |
| 方向五规则引擎 | **自建** | 不要引入规则引擎（Drools / rule-engine）。自建 JSON 表达式树 + 简单 DSL 在 Rust 中实现难度低，且与系统现有 JSON 配置风格一致。 |

### 4.3 不建议引入的技术栈

| 技术 | 为什么不建议 | 替代方案 |
|------|------------|---------|
| **Elasticsearch** | 运维复杂度高；pgvector + pg_trgm 在现阶段够用 | 方向四的图片搜索通过 `file_annotations.ai_description` 的 pgvector 嵌入实现 |
| **Kafka** | 小团队运维成本高；NATS JetStream 在现阶段可满足 | 方向三的 CDC 可通过定时轮询而非实时 changefeed 实现 |
| **Neo4j / RedisGraph** | 图数据库引入时机过早（方向一的数据量级远未达到图数据库的需求） | PG 物化视图或增量表 + `petgraph` 内存计算 |
| **WebAssembly (Wasm) 插件系统** | 方向五不需要用户自定义代码执行，只需要配置化规则 | JSON 表达式树即可满足 |
| **Temporal / Cadence 工作流引擎** | 太重。方向五需要的只是 IF-THEN-ACTION 的简单自动化，并非长时间工作流 | 自建 WorkflowEngine |

---

### 五、实施路线图

### Phase 1（P0，2-3 个月）

```
Phase 1: 并行启动方向一 + 方向二

方向一（组织网络智能）
├── M1 (2周): social_graph_edges 边表 + 写路径（在总线消费者旁路写边）
│   ├── 仓储：SocialGraphRepo (UPSERT edges)
│   ├── 总线旁路：在 run_bus_listener 中注入 SocialGraphRepo::record_interaction
│   └── 迁移：CREATE TABLE social_graph_edges
├── M2 (3周): 聚合指标后台任务
│   ├── OrgAnalyticsWorker (定时计算中心性/桥接/孤岛)
│   ├── 指标模型：OrgMetrics (WorkspaceId → OrgInsight)
│   └── 存储：org_analytics 表
├── M3 (3周): REST API + 前端仪表盘（V1）
│   ├── GET /api/org/workspace/:id/overview
│   ├── GET /api/org/workspace/:id/teams
│   ├── GET /api/org/workspace/:id/influencers
│   └── Web UI：组织健康仪表盘（基础版）
└── 风险：社交边表数据量 → 预计 6 个月后 1-2 亿行，需提前 MONTHLY PARTITION

方向二（告警事件响应）
├── M1 (2周): 数据模型 + 迁移
│   ├── on_call_schedules, on_call_rotations, on_call_overrides
│   ├── incident_event_types, incident_rules, incidents, incident_timeline
│   └── CREATE TABLE + SCHEMA
├── M2 (4周): On-Call 排班引擎
│   ├── ScheduleStore (PG) + Redis read-through cache
│   ├── POST /api/incidents/schedules (CRUD)
│   ├── POST /api/incidents/schedules/:id/overrides
│   └── GET /api/incidents/on-call?schedule_id=&timestamp=
├── M3 (4周): 告警路由 + 事件管理（V1）
│   ├── POST /api/incidents/webhook (通用入口，支持 HMAC 签名验证)
│   ├── 告警去重 (fingerprint = hash(alert.source + alert.name + alert.labels))
│   ├── 自动创建事件频道 + @值班人
│   ├── ACK 计时器 + 升级引擎
│   └── 前端：事件列表 + 活跃事件画布 + 时间线
└── 风险：告警风暴 → 引入告警聚合窗口（相同 fingerprint 30 秒内合并）
```

**Phase 1 验证标准**：
- 方向一：能回答「哪个团队协作密度最高」「谁是最具影响力的人」两个问题
- 方向二：能接收 Prometheus webhook → 创建事件频道 → @值班人 → 未 ACK 升级

### Phase 2（P1，2-3 个月）

```
Phase 2: 方向四（多模态理解）+ 方向三（数据仓库）并行

方向四（多模态内容理解）
├── M1 (2周): 数据模型
│   ├── file_annotations 表（不修改 Block::File 线格式）
│   └── 迁移：CREATE TABLE file_annotations
├── M2 (4周): 后端管线
│   ├── VisualModerationBot (复用现有 moderation_bot 预算队列模式)
│   │   ├── NSFW 分类器 ONNX 集成
│   │   └── 图片标注（Anthropic Vision）
│   ├── OcrBot (OCR 管线)
│   │   ├── PaddleOCR HTTP 客户端
│   │   └── OCR 结果写入 file_annotations.ocr_text
│   ├── 审核动作：命中 NSFW → 软删 + 审计 + 广播 Deleted
│   └── 搜索集成：ocr_text → 纳入 messages.searchable_text 或单独搜索索引
├── M3 (2周): Alt-Text 生成
│   ├── ai_description → Web 渲染 alt 属性
│   └── 无障碍合规
└── 风险：AI API 成本（NSFW 检测用本地 ONNX，标注用 Anthropic Vision 按需）

方向三（数据仓库）
├── M1 (1周): Parquet 导出管线
│   ├── ExportConnector trait + S3ParquetConnector 实现
│   ├── ExportRepo：记录导出任务进度（checkpoint 游标）
│   └── 定时器 run_export_sweep
├── M2 (2周): 分租户计费数据
│   ├── workspace_usage 表（消息量 + 存储量 + API 调用 + AI 用量）
│   └── 计费数据导出
└── 风险：Parquet 列存对宽 JSONB 字段（如 message.blocks）的 schema 定义挑战
```

**Phase 2 验证标准**：
- 方向四：上传 NSFW 图片被自动检测并软删；图片内容可被搜索
- 方向三：能生成工作区全量消息的 Parquet 快照到 S3

### Phase 3（P2，2-3 个月）

```
Phase 3: 方向五（流程自动化引擎）

├── M1 (3周): 内核（独立 crate aero-workflow）
│   ├── workflow_rules 表 + WorkflowRuleRepo
│   ├── 触发器系统（统一事件总线订阅）
│   │   ├── 消息匹配触发器（复用 auto_mod 的 regex 编译）
│   │   ├── 定时触发器（cron 表达式 → tokio interval）
│   │   ├── Webhook 触发器
│   │   └── 事件触发器（任务变更/成员加入/反应添加/通话结束）
│   ├── 条件系统（JSON 表达式树求值）
│   ├── 动作系统（动作 trait + 内建动作实现）
│   └── 执行引擎（规则匹配 → 条件求值 → 动作编排）
├── M2 (3周): 动作实现 + 幂等性
│   ├── send_message → ImService::send_message（幂等 key = workflow_run_id + action_index）
│   ├── create_task → TaskRepo
│   ├── start_approval → ApprovalRepo（动态审批人解析：on_call / role / manager）
│   ├── call_webhook → reqwest
│   ├── set_channel_topic → ChannelRepo
│   └── invoke_bot → BotDispatch
├── M3 (4周): Web 可视化编辑器
│   ├── 拖拽式规则编辑器（参考 Slack Workflow Builder 布局）
│   │   ├── 左侧：触发器池 + 条件池 + 动作池
│   │   ├── 画布：连线 + 配置面板
│   │   └── 实时预览 + 语法校验
│   └── 执行日志面板 + 失败重试
└── 风险：规则递归 → 引入执行深度限制（max 10）和重复检测（循环引用检测）
```

**Phase 3 验证标准**：
- 能定义规则：「频道 A 出现关键词'紧急' → 创建任务指派给值班人 → @频道 B → 发 Webhook」
- 规则执行失败自动重试 3 次后通知创建者

---

## 总结性建议

### 对文档的修正建议

| 错误点 | 修正 | 影响评估 |
|--------|------|---------|
| `Block::File` 有 metadata | `Block::File` 无 metadata 字段，需要先扩展或建关联表 | 方向四的工作量 +30%（需要数据模型变更） |
| `message.editing_context` 存在 | 该字段不存在；OCR 结果应写入 `Message.metadata` 或 `file_annotations` 表 | 不影响方向四的可行性和价值判断 |
| `aggregate_stats()` | 实际函数名为 `overview()` / `workspace_summary()` | 不影响分析逻辑 |
| `notifications.rs` | 实际模块为 `notif_prefs.rs` | 不影响分析 |

### 整体优先级建议

**文档的原优先级判断基本正确，但我建议微调**：

```
原文档: 一(P0) = 二(P0) > 四(P1) > 三(P1) > 五(P2)
我的建议: 二(P0) > 一(P0) > 四(P1) > 五(P1) > 三(P2)
```

理由：
1. **方向二升至 P0 首位**：告警事件响应的商业价值最大（对标 $1B 市场）、与现有基础设施的复用度最高（webhook + 通知 + bot + 审批）、且 On-Call 排班引擎的工期确定性高（纯数据模型+调度逻辑，无 AI 不确定性）。
2. **方向五升至 P1**：一旦方向二的告警事件引擎和方向四的 Bot 平台就绪，工作流引擎的触发器和动作池自然充实。如果等到方向二和方向四完全成熟再启动方向五，会错过平台锁定的窗口期。方向五的核心引擎可以和方向二并行开发（共享 `aero-workflow` crate 的基础设施）。
3. **方向三降至 P2**：数据仓库集成是企业客户的「需要」但非「渴望」。在方向一和二未验证之前，投入工程资源构建 BI 连接器会分散核心产品差异化方向的精力。Parquet 导出作为基础能力可以在 P1 末期开发，但完整的 BI 连接器套件应推迟。

### 最终检视清单

每个方向在进入实施前，应该满足以下检查：

- [ ] 存在的代码验证：确认代码中的模块名、函数名、字段名与文档一致
- [ ] 数据模型迁移：迁移 SQL 已写，`cargo build` 后能 `aero-cli migrate`
- [ ] 仓储（Repo）：新数据访问已包在 `XRepo` 内
- [ ] 路由（Route）：`pub fn routes() -> Router<AppState>` + 在 `routes.rs` 中 `.merge()`
- [ ] 鉴权（Auth）：所有 mutating 路由使用 `assert_room_access(participant, room)` 或等价守卫
- [ ] 实时（Real-time）：如果需要，在对应总线 consumer 中广播事件
- [ ] 前端（Web）：WS 帧 handler 已注册，DOM ID 与后端 smoke 测试对齐
- [ ] 测试冗余：DB 门控测试 `#[ignore]` + `DATABASE_URL`
- [ ] 清理重复：检查并删除重复文档

以上分析基于代码验证报告、全量代码审查和架构文档。实施时建议每方向按「迁移 → 仓储 → 路由 → 鉴权 → 实时 → 前端」的顺序推进，与 `AGENTS.md` §4.1 的加功能配方一致。

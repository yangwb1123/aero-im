# 架构分析：Aero IM — 五个高价值扩展方向

## 1. 架构评估

### 1.1 现有架构的核心优势

Aero IM 的当前架构具备几个难得的先天优势，这五个扩展方向能成立的前提正是这些优势：

**事件驱动骨架是最大的架构资产。** NATS JetStream 作为跨实例事实源，配合 Hub 进程内扇出，形成了一套成熟的事件总线模式。这意味着：

- 新能力可以以"新 consumer 订阅既有 subject"的方式插入，无需修改已有生产者的代码。方向二（告警事件）可以直接订阅 `im.room.*` 来拦截消息中的告警关键词，方向五（工作流引擎）可以用同样模式匹配触发器。
- Durable consumer 提供 at-least-once 语义，天然支持需要可靠执行的场景（方向一的分析指标聚合、方向二的告警升级、方向三的 CDC 导出）。

**Crate 边界清晰，依赖方向自下而上无环。** 这是 Rust 项目中最容易被忽视的架构优势。`aero-common` → `aero-bus`/`aero-storage` → `aero-im-core` → `aero-server` 的依赖链意味着：

- 新增功能可以放在已有 crate 的新模块中，或创建新 crate 依赖既有 crate，而不需要重构已有的依赖图。
- AI 基础设施（`aero-ai`）已经独立存在，方向四（多模态理解）只需在管线中插入新步骤，数据结构已在 `Block::File.metadata` 预留。

**Redis 作为集群级状态 + Postgres 作为持久层，职责分离清晰。** 方向一的社交图指标需要高性能读写（边/权重的增量更新），Redis 的 sorted-set/hash 天然适配；方向二的 On-Call 排班需要 LWW 语义，Redis 也合适。Postgres 则负责不可变的审计日志和执行记录。

### 1.2 架构局限性（阻碍这些方向的关键瓶颈）

**瓶颈一：分析层缺失。** 系统没有统一的分析数据管道。目前 "analytics" 是存储层的一个 repo 方法（`analytics.rs` 的 `aggregate_stats`），返回的是实时的 SQL 聚合——这对方向一（组织网络智能）是不足够的。组织网络指标需要在离线/近线状态中计算（度中心性、社区检测），不能放在主请求路径中。

**瓶颈二：事件类型模型中缺少「元事件」机制。** 当前 `RoomEvent` 和 `StreamEvent` 是消息级事件，但系统没有一个"事件产生事件"的机制。方向五的工作流引擎需要这种能力——"任务完成" → 触发工作流 → 产生"通知创建者"事件。目前的架构需要为每对触发-被触发关系手写胶水代码。

**瓶颈三：Bot/Agent 体系缺少统一的注册和执行契约。** §2 列出了 7 个 bot，每个都有自己的 consumer group、自己的匹配逻辑、自己的失败处理。这种"各管各"的模式在 bot 数量少时可行。方向二和方向五会引入大量新 bot（告警路由 bot、事件生命周期 bot、工作流执行 bot），如果没有统一的注册/调度/监控契约，维护成本会非线性增长。

**瓶颈四：Webhook 接收端缺少事件升维能力。** 当前 `webhooks.rs` 只是把 HTTP 载荷转成房间消息。方向二需要将多个相关告警合并为一个事件，需要 dedup、聚合、升级——这需要 webhook 端的事态感知，目前不存在。

### 1.3 架构债务（需在扩展前或并行中偿还）

| 债务项 | 位置 | 影响方向 | 建议偿还时机 |
|---|---|---|---|
| Bot 无统一注册表 | server/src/bot_dispatch.rs | 二、五 | Phase 1 前 |
| `analytics.rs` 只有粗粒聚合 | storage/src/analytics.rs | 一、三 | Phase 1 |
| 无消息变更的 CDC 事件 | 全系统（仅总线有 RoomEvent） | 三 | Phase 2 |
| `Block::File.metadata` 定值但无写入 | common/src/model/block.rs | 四 | Phase 2 |
| 审批流的动态分配人能力缺失 | server/src/approvals.rs | 二、五 | Phase 1 |

---

## 2. 扩展方向深入分析

### 2.1 方向一：组织网络智能（P0）

**为什么需要。** 技术上最轻、商业上最具差异化。Slack/Teams 有"分析"页面，但都止步于活跃度/消息量这种浅层指标。真正的组织网络分析——谁在沟通、谁在孤立、哪两个团队之间有沟通鸿沟——是竞品盲区，也是 Aero IM 建立企业护城河的最佳切入点。

**核心挑战：**

- **社交图数据的时效性与计算成本的权衡。** 全量社交图（N 个节点，O(N²) 条可能边）的度中心性、中介中心性、社区检测需要全局扫描。在工作区规模达到数万人时，全量计算不可接受。
- **隐私与透明度的平衡。** 「谁在工作区中影响力最高」这个指标如果直接暴露，可能引发政治问题。需要脱敏/匿名视图，以及粒度的可见性控制。

**建议的架构方案（两个选项）：**

| 选项 | 描述 | 优点 | 缺点 |
|---|---|---|---|
| **A: 增量图 + 定时全量快照** | 在 Redis 中维护增量边集合（互动即 write），定时（如每小时）触发全量计算，结果写回 PG 存储，API 读 PG | 写入路径极轻（O(1) Redis INCR），全量计算不做热点，读路径稳定 | 全量计算期间指标可能滞后，小时级延迟不够实时 |
| **B: 纯增量指标（不计算全图）** | 只维护单节点维度的聚合指标（入度/出度/回复率/跨房比例），不做全局社区检测 | 无全量计算代价，实时性好 | 无法回答全局性问题（孤岛检测、子图识别） |

**推荐：** 选项 A。选项 B 的指标用户可以在任何 IM 中看到，不具备差异化价值。全量计算可以通过在 `AiWorker` 中加一个 `NetworkAnalysis` 任务类型来执行（复用现有预算管控和并发控制），牺牲一点实时性换取差异化能力。

**架构变更：**

```
新增：
  aero-storage/src/social_graph.rs   — Redis 边存储（Follower/Message/Reaction 三种图）
  aero-ai/src/network_analysis.rs    — 度中心性 / 社区检测 / 孤岛识别 算法
  aero-server/src/org_insights.rs    — REST API (/api/workspaces/:id/insights/*)
  aero-server/src/org_dashboard.rs   — Web 前端（仪表盘页面）
  
修改：
  aero-storage/src/analytics.rs      — 下沉到更细粒度的预聚合，而非实时 SQL
  aero-common/src/model/ — 新增 OrgMetrics / NetworkEdge 类型
```

**对现有系统的影响：** 极低。分析管线是纯读的，不修改已有写入路径。`analytics.rs` 的细化是向下兼容的扩展。

---

### 2.2 方向二：告警与事件响应平台（P0）

**为什么需要。** 这是从"聊天工具"到"运维平台"的跃迁。PagerDuty 的市值曾达 ~200 亿美金，核心价值就是 IM 内嵌的事件管理。而 Aero IM 已经有 Webhook 接收、Bot 框架、通知推送、审批流——这些是 PagerDuty 花了多年搭建的基础设施。

**核心挑战：**

- **On-Call 排班的数据模型是经典的调度问题。** 支持轮换（周/天/自定义）、覆盖（切换/暂替）、层级升级（P0→P1→Manager）、按团队维度（SRE 团队 vs 后端团队各自排班）。数据模型设计不当会导致后续维护噩梦。
- **告警 dedup 和聚合。** PagerDuty 的核心智能在于将 1000 条同类告警归并为 1 个事件，并识别告警风暴。如果 Aero IM 在这个层面只是简单转发每条告警，产品价值会大幅缩水。
- **事件频道的生命周期管理。** 事件发生时自动创建频道，事件结束后归档或保留。频道名、访问权限配置、Bot 加入等需要自动化。

**建议的架构方案：**

引入 **三个新 crate** 以保持 crate 边界清晰：

1. **`aero-alerting`** — 告警接收与事件编排
   - `AlertIngest` — Webhook 接收层（兼容 Prometheus/Grafana/Datadog/自定义）
   - `EventOrchestrator` — 告警聚合、dedup、升级、事件频道创建
   - `OnCallSchedule` — 排班引擎（Redis LWW 存储 + 后端定时重新生成轮换）

2. **`aero-incident`** — 事件生命周期管理
   - `IncidentLifecycle` — 事件状态机（Open → Acknowledged → Mitigated → Resolved → Reviewed）
   - `PlaybookEngine` — 运行手册执行引擎（步骤序列编排）
   - `PostmortemTemplate` — 事后复盘模板 + 自动消息聚合

3. **`aero-statuspage`** — 状态页
   - 复用已有的 Web 静态页能力，新增 REST API 用于组件状态更新

**关键决策：事件频道 vs 专用事件容器。** 一个开放的设计问题是：事件是表现为"一个专用房间 + 内部消息"，还是"一个专用事件对象 + 关联的消息引用"？

| 选项 | 优点 | 缺点 |
|---|---|---|
| **事件即频道** | 复用所有消息能力（实时、历史、搜索、通知、@提及） | 频道数量膨胀（每事件一频道），事件结束后需清理 |
| **事件即聚合对象** | 独立于频道基础设施，清理轻量 | 需要重实现消息时间线、协作编辑等能力 |

**推荐：事件即频道，但使用临时频道 + TTL 自动归档。** 这样可以零成本获得所有 IM 协作能力，归档后的频道自动变成只读历史。NATS 的 `im.room.*` subject 模式已经支持这个方案——事件频道也只是一个 `room`，仅生命周期管理不同。

**对现有系统的影响：**

- **中等侵入。** 需要扩展 `rooms` 表加 `room_type` 字段（`chat` vs `incident`）或标签系统区分事件频道。
- 需要修改 `webhooks.rs` 的接收逻辑，从"全部转消息"改为"匹配告警模式则走告警管线"。
- `approvals.rs` 的审批人逻辑需要支持动态解析（On-Call 排班查询）。

---

### 2.3 方向三：数据仓库与嵌入式分析管线（P1）

**为什么需要。** 这不是方向一那种差异化竞争，而是**准入资格**。大型企业客户一定会问："你们的 IM 数据能导出到 Snowflake 吗？"如果答案是"请用 REST API 自行轮询"，就可能失单。Snowflake 和 BigQuery 的连接器是云原生应用的企业采购清单中的必备项。

**核心挑战：**

- **CDC（Change Data Capture）的语义。** 消息会编辑、删除、软删。如果数据仓库只收到 INSERT 事件，会和数据库状态不一致。需要捕获 UPDATE/DELETE，并在目标端正确体现。
- **增量导出的断点续传。** 如果导出连接中断，重启后不能重复/遗漏数据。NATS JetStream 的 consumer cursor 天然支持，但每个目标系统有各自的 offset 管理方式。
- **分租户隔离。** 每个工作区的数据应独立导出，不能交叉泄露。

**建议的架构方案（三个选项）：**

| 选项 | 描述 | 成本 | 维护性 |
|---|---|---|---|
| **A: NATS Sink Connector** | 在 NATS subject 上挂 sink consumer，将事件转为柱状文件（Parquet/CSV），批量上传至 S3 → Snowpipe/BigQuery | 中（新 crate `aero-export`） | 高（复用 NATS 既有机制） |
| **B: PG Logical Replication** | 用 Postgres 的 `pgoutput` 插件监听 WAL，推送到外部目标 | 低（可用 `debezium` 或 `pgcopydb`） | 低（依赖外部工具，需要 PG 超级权限） |
| **C: 双写** | 业务写入 PG 时同步写入外部数据服务 | 高（侵入所有写入路径） | 低（事务风险高） |

**推荐：选项 A。** 理由：
- 事件数据已经存在于 NATS 总线中——这不是"额外代码"，而是复用已有的可靠事件流。
- 关键实体（`messages`, `reactions`, `notifications`）的变更已经以 RoomEvent 的形式在总线上流动。需要补的只是 message UPDATE/DELETE 事件——这些总线已有（`Edited`/`Deleted` variant）。
- 新增 `aero-export` crate 独立于主业务逻辑，可单独部署、水平扩展。

**架构变更：**

```
新增：
  aero-export/                      — 新 crate
  ├── Cargo.toml
  ├── src/lib.rs
  ├── src/snowflake.rs              — Snowflake PUT + COPY INTO 适配器
  ├── src/bigquery.rs               — BigQuery Storage Write API 适配器
  ├── src/parquet.rs               — Parquet 文件生成（arrow + parquet 库）
  ├── src/s3_uploader.rs           — S3 分段上传
  └── src/stream_consumer.rs       — NATS consumer 连接 + offset 管理

修改：
  aero-server/src/export_job.rs    — 扩展 GDPR 导出为工作区管理导出
  aero-server/src/analytics.rs     — 新增分页/参数化的查询端点
```

**对现有系统的影响：** 低。`aero-export` 是独立 crate，只读取总线事件和 PG 数据，不写入生产表。对已有 API 的影响为零。

---

### 2.4 方向四：多模态内容理解管线（P1）

**为什么需要。** 当前审核管线在文本层面是完备的（关键词 + AI 异步），但在视觉层面是盲区。这在企业合规场景中是致命缺口——如果用户在工作聊天中分享了违规图片，系统没有任何检测和阻止能力。

**核心挑战：**

- **多模态推理的成本与延迟。** 每一张上传的图片都走 Anthropic Vision API 在成本上不可承受，在延迟上也不可接受（用户希望图片上传后立即看到，而不是等 2 秒后才有结果）。
- **管线编排的位置。** 审核应该发生在哪个环节？同步（上传时阻塞等待结果）还是异步（先上传，后审核，违规则撤回）？
  - **同步** -> 延迟不可接受，用户体验差。
  - **异步** -> 有"违规内容短暂可见"的时间窗口，合规审计可能质疑。
- **模型的本地 vs 云端选择。** NSFW 分类器有开源模型（CLIP、nsfwjs、DeepSparse），但精度不如闭源 API。企业客户对数据不出境的合规要求又可能禁止云 API。

**建议的架构方案：**

采用**异步两步管线**：

```
上传 → content_sniff(字节验证) → av_scan(病毒) → blob 存储 ✅
                                  ↓ (异步，不阻塞)
                         1. 轻量本地分类器（CLIP/nsfwjs ONNX）
                            → 高风险 → 标记待审 + 通知管理员
                            → 低风险（Pass）
                         2. AI 标注（Anthropic Vision 或本地 VLM）
                            → tags + ai_description → 存入 metadata
                         3. OCR（如果需要）
                            → 文本 → 纳入搜索索引
```

**关键决策点：轻量分类器的位置。**

| 选项 | 延迟 | 隐私 | 成本 | 精度 |
|---|---|---|---|---|
| **A: ONNX 本地推理（推荐）** | 低（<50ms/图） | 数据不出境 | 一次 GPU 采购 | 中等（足够做 NSFW 初筛） |
| **B: Anthropic Vision API** | 中（200-500ms/图） | 数据出境 | 按 token 计费，高 | 高 |
| **C: 混合（本地初筛 + 云端复审）** | 低（90% 图只走本地） | 仅疑图出境 | 平衡 | 高（疑图云端再审） |

**推荐：选项 C。** 本地运行 NSFW 分类器（ONNX 部署的 CLIP 或 nsfwjs）做第一关，拦截 90% 以上的明确违规内容。仅对"疑似"案例走 Anthropic Vision API 做精确判断。这样延迟和成本都可控。

**架构变更：**

```
修改：
  aero-ai/src/vision.rs             — 新增：多模态视觉分析模块
  aero-ai/src/service/mod.rs       — 扩展 AiService 接口：analyze_image()
  aero-server/src/content_sniff.rs — 插入异步后处理 hook
  
新增（在已有的 moderation_bot 模式内）：
  aero-server/src/vision_bot.rs    — 订阅 im.room.* 检测 Block::File 含图片
  aero-common/src/model/block.rs   — Block::File.metadata 写入 tags/ai_description
  
可选（ONNX 推理）：
  aero-common/src/onnx.rs           — ONNX Runtime 封装
```

**对现有系统的影响：** 低-中。已有的 `moderation_bot` 提供了预算队列、并发控制、错误处理的模版——视觉审核可以复用同样的模式。`Block::File.metadata` 已经预留 JSONB，写 tags 和 ai_description 是安全扩展。

---

### 2.5 方向五：流程自动化引擎（P2）

**为什么需要。** 这是五个方向中**架构影响最大**的一个，也是产品锁定效应最强的一个。当用户在工作流中配置了大量自动化规则后，迁移成本急剧上升。同时也是**最危险**的一个——如果设计不当，会变成一个没有人能维护的"规则泥潭"。

**核心挑战：**

- **规则模型的设计。** 触发器(T) - 条件(C) - 动作(A) 的三元组是所有工作流引擎的基础。但坑在细节：触发器需要参数化（例如"关键词'紧急'只在#ops频道有效"）；条件需要支持组合（AND/OR/NOT）；动作需要参数化（"创建任务 → 截止日 = 当前时间 + 2h"）。这些如果在设计阶段没有枚举，后期只能靠加 JSONB hack 来修复。
- **执行的可观测性和调试。** 当一天有数千次工作流执行时，用户需要知道"为什么这个规则没有触发""为什么这个动作失败了"。这需要完整的执行日志 + 重试记录 + 失败原因。不能是黑盒。
- **循环检测和风暴防护。** 如果工作流 A 的动作为"发消息"，而工作流 B 的触发器是"消息匹配"，则 A→发消息→B 触发→发消息→A 触发→…… 形成无限循环。必须有循环检测机制。

**建议的架构方案：**

**第一步（Phase 1）：不建新引擎，先建规则注册中心。** 在 Phase 1（与方向二并行），只做三件事：
1. 定义 `workflow_rules` 表（JSONB 存储规则，不解析执行）
2. 提供 REST API 做 CRUD
3. 提供一个简单的事件 → 规则匹配的消费者（仅匹配，执行用已有的 bot 能力）

**这是"最小可行"的执行方案——用 JSON 表达规则，不引入 DSL、不引入可视化编辑器。**

**第二步（Phase 2）：执行引擎。** 在 Phase 2（方向二的事件引擎运行稳定后），引入 `WorkflowEngine` 作为正式的执行引擎：

```
Core 抽象：
  trait Trigger: Send + Sync { fn matches(&self, ctx: &EventContext) -> bool; }
  trait Condition: Send + Sync { fn evaluate(&self, ctx: &EventContext) -> bool; }
  trait Action: Send + Sync { async fn execute(&self, ctx: &EventContext) -> Result<ActionOutput>; }

引擎结构：
  WorkflowEngine {
      rules: Vec<WorkflowRule>,
      triggers: HashMap<TriggerType, Regex>,
      executors: HashMap<ActionType, Arc<dyn Action>>,
  }
  
  工作流：
  - NATS consumer 接收事件
  - 规则匹配（触发器+条件）
  - 执行动作（幂等 key = hash(rule_id, event_id)）
  - 记录执行日志（成功/失败 + 耗时）
  - 失败重试（3 次，指数退避）
```

**关键决策：DSL vs JSON vs 代码生成。**

| 选项 | 灵活度 | 安全性 | 用户学习成本 |
|---|---|---|---|
| **JSON 规则树** | 中 | 高（解析即校验） | 低（开发者友好） |
| **手写 DSL** | 高 | 中（需沙箱执行） | 高（需学新语言） |
| **可视化拖拽 → JSON** | 中 | 高（JSON 后端，前端约束） | 最低 |

**推荐：JSON 规则树 + 可视化编辑器（第二步）。** JSON 规则自描述、易存储、易版本控制。可视化编辑器只是 JSON 的前端呈现。这样核心引擎依赖 JSON，不被前端 UI 绑定。

**架构变更（两阶段）：**

```
Phase 1（最小可行）：
  新增：
    aero-im-core/src/workflow/mod.rs    — WorkflowRule 数据模型
    aero-storage/src/workflow.rs        — workflow_rules 仓储
    aero-server/src/workflow_rules.rs   — REST CRUD API (/api/workflows/*)
    aero-server/src/workflow_consumer.rs — NATS 消费者（匹配规则→分派动作）

Phase 2（引擎完整）：
  新增：
    aero-workflow/                      — 新 crate（避免 aero-im-core 膨胀）
      ├── src/engine.rs                 — WorkflowEngine 核心
      ├── src/trigger/mod.rs            — 各触发器实现
      ├── src/condition/mod.rs          — 条件表达式求值
      ├── src/action/mod.rs            — 各动作实现
      ├── src/execution_log.rs         — 执行日志
      └── src/anti_loop.rs            — 循环检测

  web/ 新增 workflow-editor/ — 可视化工作流编辑器
```

**对现有系统的影响：** **大。** 方向五会影响多个已有的孤立模块：
- `approvals.rs`/`tasks.rs`/`scheduled.rs` 需要暴露动作接口给工作流引擎
- Bot dispatch 需要为工作流动作提供执行上下文
- `auto_mod.rs` 的匹配规则需要能与工作流引擎合并或迁移
- 所有已有功能需要新增 `idempotency_key` 支持（防重入）

---

## 3. 接口设计建议

### 3.1 核心原则

1. **总线优先。** 任何新功能需要"执行"的，都应该从消费 NATS subject 开始，而不是从 HTTP API 开始。这样天然获得 at-least-once 语义、水平扩展能力、异步解耦。

2. **统一「Bot 注册」契约。** 当前 7 个 bot 各自为政。在 Phase 1 中应该引入一个 `BotRegistry` trait：

```rust
#[async_trait]
trait Bot: Send + Sync {
    fn name(&self) -> &'static str;
    fn subjects(&self) -> Vec<&'static str>;
    fn consumer_group(&self) -> Option<&'static str>;  // None = ephemeral
    async fn handle(&self, ctx: BotContext) -> Result<BotOutcome>;
}

enum BotOutcome {
    Ack,                     // 处理完成，正常确认
    Nack(String),            // 处理失败，请求重试
    Skip,                    // 不匹配，跳过
    Defer(Duration),         // 预算不足，稍后重试
}
```

所有 bot（现有的 agent_bot、ooo_bot、unfurl_bot 等 + 新加的方向二告警 bot、方向五工作流引擎）都实现这个 trait，由 `run_bus_listener` 统一调度。这样可以获得：

- 统一的监控（每个 bot 的执行次数/失败率/延迟）
- 统一的失败降级策略
- 统一的 consumer group 管理（避免 subject 冲突）

3. **动作即 trait。** 方向五的 WorkflowEngine 和方向二的 PlaybookEngine 都执行"动作"。应该定义 `Action` trait，让 `send_message`、`create_task`、`call_webhook`、`start_approval` 等所有动作实现它。这样工作流引擎、告警 playbook、Bot 动作可以共享同一个执行后端。

### 3.2 是否需要新的抽象层

| 抽象层 | 必要性 | 说明 |
|---|---|---|
| **Bot 注册中心** | **必要（Phase 1）** | 解耦 bot 的创建和执行，统一监控 |
| **动作执行器** | **必要（Phase 1.5）** | 方向二和方向五共享动作，避免两套实现 |
| **规则引擎** | 可选（Phase 2） | JSON 规则树 + trait 条件即可，不引入规则引擎框架（如 drools） |
| **外部连接器 SD** | 可选（Phase 3） | 方向三的 CDC 导出连接器可做插件架构，但初期定死 Snowflake/BQ 即可 |

### 3.3 向后兼容性策略

- **所有新 REST API 以 `/api/v2/` 路径发布**，但存量 `events.rs` 中的 RoomEvent 变体只新增不修改。方向二的告警事件新增 `IncidentEvent` enum，不修改 `RoomEvent` 现有变体。
- **方向一的组织洞察接口**是全新的命名空间（`/api/workspaces/:id/insights/`），不与现有 API 冲突。
- **方向五的工作流规则 CRUD** 是全新 API（`/api/workflows/`），不存在兼容问题。
- **方向三的导出连接器**需要新增的 message UPDATE/DELETE 事件是总线协议扩展，不破坏已有的 consumer（serde 忽略未知字段的策略允许安全扩展）。

---

## 4. 技术选型

### 4.1 是否需要引入新的技术栈

| 方向 | 需要引入 | 可选引入 | 无需引入 |
|---|---|---|---|
| 方向一 | — | — | ✓ 复用 Redis/PG |
| 方向二 | — | — | ✓ 复用 NATS/Bot |
| 方向三 | Arrow + Parquet（写入 Parquet 文件） | — | ✓ 复用 NATS |
| 方向四 | ONNX Runtime（本地 NSFW 推理） | VLM 模型 | ✓ 复用 aero-ai |
| 方向五 | — | — | ✓ 复用现有动作 |

严格来说，**五个方向都不需要引入新的基础设施组件**。Parquet 写入需要 `arrow` 和 `parquet` Rust crate（非新基础设施），ONNX Runtime 需要 `ort` crate（只在新部署 GPU 节点时增加运维成本）。

这是很有利的：五个方向的架构扩展都可以在现有 Postgres/Redis/NATS 栈上完成。

### 4.2 第三方依赖评估标准

对于方向三（Parquet 导出）和方向四（ONNX 推理）：

| 依赖 | 评估 |
|---|---|
| **arrow + parquet** (方向三) | Rust `arrow` 和 `parquet` crate 由 Apache 维护，社区活跃度高，`aero-export` crate 的唯一新增依赖。安全。 |
| **ort** (方向四) | ONNX Runtime 的 Rust 绑定，微软维护。需要 GPU 节点上的 CUDA 驱动。如果选择纯云端方案（Anthropic Vision），可以不引入。建议作为可选项，编译期 feature gate。 |
| **VLM 模型选择** | 不引入模型依赖——模型是运行时下载的 ONNX 文件，非编译期依赖。CI 不需要 GPU。 |

### 4.3 自建 vs 采购决策

| 场景 | 选项 | 决策 |
|---|---|---|
| **NSFW 图片审核** | 自建 CLIP/nsfwjs ONNX 推理 vs 采购 AWS Rekognition / Google Vision API | **自建 + 云端后备**。企业客户有数据出境合规要求，自建 ONNX 网关可以承诺数据不出境。采购 API 是后备。 |
| **OCR** | 自建 PaddleOCR ONNX vs Google Cloud Vision / Azure OCR | **采购 API**。OCR 精度差距大，Tesseract 质量不足，自建 PaddleOCR 的 CI 集成复杂。初期采购 Azure OCR（成本极低，每千次 <$0.01）。后期可切换。 |
| **数据仓库连接器** | 自建 vs 采购 Airbyte / Fivetran | **自建**。连接器数量有限（Snowflake/BigQuery/S3 Parquet），自建成本低且可深度集成到产品中作为付费模块。Airbyte 部署运维成本高于自建。 |
| **On-Call 排班** | 自建 vs 采购 PagerDuty API 集成 | **自建**。排班模型不复杂，PagerDuty 集成方案意味着客户仍需购买 PagerDuty——不是我们要的。自建排班引擎是产品差异化所在。 |

---

## 5. 实施路线图

### 5.1 优先级排序（最终建议）

```
P0（Phase 1，并行启动）
├── 方向一：组织网络智能
│   └── 快赢路径：复用已有数据，新增分析管线 + 前端仪表盘
│
├── 方向二：告警与事件响应平台（子集）
│   ├── P0.1: On-Call 排班 + Webhook 告警 → 事件频道
│   └── P0.2: 事件生命周期 + Playbook 引擎（与方向五共享动作执行器）
│
└── 架构基础设施（前置依赖）
    ├── BotRegistry trait 统一化（改造现有 7 个 bot）
    └── `Action` trait 定义 + 首批动作实现

P1（Phase 2，方向一稳定后）
├── 方向二完成：状态页 + 事后复盘 + 计费
├── 方向四：多模态内容理解（复用 moderation_bot 模式）
│   └── 先做 NSFW 初筛（ONNX CLIP），再做 OCR + 自动标注
└── 方向五 Phase 1：规则注册中心 + 匹配消费者

P2（Phase 3，平台基础设施完备后）
├── 方向三：数据仓库导出（客户驱动）
├── 方向五 Phase 2：WorkflowEngine 正式版 + 可视化编辑器
└── 方向一增强：Leaderboard、离职预测、组织健康趋势
```

### 5.2 风险点与缓解策略

| 风险 | 影响方向 | 概率 | 缓解 |
|---|---|---|---|
| **社交图计算在 10K+ 用户工作区性能不可接受** | 一 | 中 | 增量聚合 + 预计算 + 按需刷新。如果 HNSW/Cosine Dist 计算成本高，可回退到简化指标（仅度中心性，不计算中介中心性）。 |
| **告警事件管线的误报率导致用户反感** | 二 | 高 | 严格设计告警到事件的聚合逻辑（分组窗口、频率阈值）。提供"静音规则"功能，允许用户按 source/tag/severity 静音。 |
| **工作流引擎的循环问题** | 五 | 中 | 在引擎中实现 TTL 和深度限制（最大执行深度 = 5）。每条执行链携带 `workflow_chain: Vec<RuleId>` 防止自环。 |
| **多模态管线成本失控** | 四 | 高 | 本地 ONNX 初筛拦截大部分流量。OCR 和 AI 标注走低优先级队列（复用 AiWorker 的 budget controller）。设置工作区级别的日预算上限。 |
| **数据仓库导出的数据一致性问题** | 三 | 中 | CDC 事件中的 seq 字段确保有序性。目标端使用 upsert 语义（Snowflake MERGE / BigQuery DML）。定期全量校验 as a safety net。 |
| **五个方向并行开发导致 crate 依赖混乱** | 全 | 中 | 每个方向独立 crate，避免 cross-crate cyclic dep。方向五的 Phase 1 在 `aero-server` 内完成，Phase 2 才拆为独立 `aero-workflow`——这个节奏不能跳。 |

### 5.3 阶段检查和里程碑

**Gate 1（Phase 1 完成，约 6-8 周）：**
- [ ] BotRegistry trait 统一切换完成，7 个现有 bot 全部迁移（无行为变化）
- [ ] 方向一：社交图指标在 Redis 中持续写入，至少 3 个组织网络指标（影响力分、跨团队协作密度、活跃网络图）对工作区管理员可见
- [ ] 方向二：On-Call 排班（周轮换 + 覆盖）可用；Webhook 接收 → 告警 → 自动创建事件频道 → @值班人 → ACK 计时 → 超时升级，端到端联调通过

**Gate 2（Phase 2 完成，约 +8-10 周）：**
- [ ] 方向四：图片上传 → ONNX NSFW 分类 → 低风险跳过 / 高风险标记 + 管理员通知，管线稳定运行 1 周无漏报
- [ ] 方向二：状态页 + 事后复盘模板上线，至少一个 beta 客户接入
- [ ] 方向五 Phase 1：workflow_rules CRUD API + 消息模式匹配 → 执行动作（发消息 / 创建任务 / 发 Webhook）端到端通过

**Gate 3（Phase 3 完成，约 +12-16 周）：**
- [ ] 方向三：至少一个目标连接器（Snowflake 或 S3 Parquet）在 beta 客户环境上线，增量数据同步 24 小时无偏差
- [ ] 方向五 Phase 2：WorkflowEngine 正式版 + 可视化编辑器 MVP，支持 5+ 触发器 × 8+ 动作组合
- [ ] 方向一增强：组织健康趋势图（周/月/季变化），至少一个企业客户基于数据做出管理决策

---

## 附录：关于风险对冲的建议

这五个方向中，**方向一是风险最低、回报最快的**——它不修改任何写入路径，只在读路径上做文章，可以快速验证"企业客户是否愿意为组织洞察付费"的假设。

**方向二和方向五有互惠关系**——告警引擎的事件频道、Playbook、排班系统和通用工作流引擎共享同一个"触发器-条件-动作"模型。建议将方向二视为方向五的"特化实例"，先在告警场景中打磨 TCA 模型，再泛化为通用引擎。这比独立开发两个引擎再合并更安全。

**方向四和方向三的优先级可以根据客户需求动态调整**——如果第一个企业客户来自受监管行业（金融/医疗），方向四（多模态合规）应升级为 P0；如果客户是数据驱动型互联网公司，方向三（数据仓库）应提前。这两个方向的架构预留（aero-ai 管线扩展 + NATS CDC 事件）已经在前面的设计中做了，切换优先级不需要重做架构。

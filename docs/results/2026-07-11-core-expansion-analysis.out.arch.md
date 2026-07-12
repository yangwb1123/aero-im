Migration 0133 存在，`bwe.rs` 存在 — 代码库状态与文档一致。`routes()` 模式使用 `.merge(crate::<mod>::routes())`，与 AGENTS.md §3 描述吻合。以下是我作为架构师的综合分析。

---

# Aero IM 架构分析报告

> **日期**: 2026-07-12 | **视角**: 资深架构师
> **范围**: 16 crate / 157 迁移 / ~46K Rust / ~5.9K Web SPA / 200+ 模块
> **基础**: 通读 `docs/requirements/` 下 130+ 份分析 + 代码级验证

---

## 一、架构评估

### 1.1 设计优势（What's Right）

| 维度 | 优势 | 判断依据 |
|------|------|---------|
| **事件总线拓扑** | NATS JetStream 作为跨实例事实源，Hub 做本地扇出，双命名空间隔离（`im.room.*` durable / `live.stream.*` ephemeral） | 这是正确的异构扇出模型：IM 需 at-least-once 保证，直播可容忍丢帧 |
| **Crate 依赖方向** | 从 `aero-common` → `aero-bus`/`aero-storage` → `aero-im-core` → `aero-server` 单向依赖，无成环 | §1 crate 地图已严格执行，这是长期可维护性的基石 |
| **集群状态外置** | Redis sorted-set 存 presence/viewer/roster，单进程内存不保存集群状态 | 正确决策。避免 JVM 式脑裂（如 Hazelcast），每实例重启后从 Redis 重建 |
| **Seam 显式标记** | AGENTS.md §2 明确标注 `call_bridge_supervisor` 的 `ensure_egress` 已建但未接线，§4.5 标注媒体 seam 需真对端 | 这种诚实比「所有功能已完成」更有工程价值 |
| **幂等键文化** | `ooo_bot` 的 `ON CONFLICT DO NOTHING`、AiWorker 的 `SKIP LOCKED`、`blob_gc_drain` 的 delete-then-ack | at-least-once 语义下的正确模式，覆盖了 80% 的重复投递场景 |

### 1.2 架构债务与技术债

按严重度排列：

#### 🔴 P1 — 架构级债务（影响正确性/可用性/安全性）

**债务 1：全有或全无的故障模型（D1）**

当前系统对依赖的假设是"各组件都健康"。Redis 宕机 → presence 查询 500 → 可能级联到消息发送被拒。NATS 中断 → 总线监听自愈循环继续但扇出停摆 — 用户以为消息送达了（HTTP 200），实际对端没收到。

这不是"缺少功能"，而是**系统边界契约未定义**。需要：
- 每个外部依赖（PG/Redis/NATS/AI）的读路径降级策略
- 写路径的写缓存 + 恢复回放
- `GET /health/degraded` 暴露当前降级模式
- AGENTS.md 已有 fail-open 要求（transcribe_bot/agent_bot），但缺统一降级框架

**债务 2：Web 前端是「联调专用」客户端（D2）**

`web/index.html` 第 26 行自称 "debug client · 联调专用"。这不是谦虚，是事实：
- 零测试（eslint no-undef 是唯一静态检查）
- 零打包（裸 ES2020 modules，无 tree-shaking/压缩/代码分割）
- 零 PWA / Service Worker / 离线支持
- 零生产级错误监控 / 用户反馈
- 零 a11y（ARIA、键盘导航、屏幕阅读器）
- SPA 渲染性能问题：所有消息全量渲染，无虚拟滚动

这是最大的产品化缺口。后端能力远超前端呈现。

**债务 3：协议帧的断头路（D3）**

服务端完整发送 `Interaction` / `MessageSeen` / `Welcome` 帧，客户端**零 handler 注册** — 帧到达后静默丢弃。这不是"没实现"，而是**协议不完整** — 数据产生了、广播了、到达了，但客户端不消费。产品角度：用户看不到谁读了消息、交互式按钮点击无人感知。

#### 🟡 P2 — 扩展性债务（影响规模化/效率）

**债务 4：O(N) 扇出缺少分层优化（D4）**

`send_message` → `publish_room_event` → `run_bus_listener` → `Hub::fan_out_raw` 的复杂度是 **O(房间成员数)**。在万人房间中：
- `RoomMemberCache` 展开全部成员 → O(N) memory
- `dispatch_notifications` 批量 mute 查询 → O(N) DB round-trip（虽已 batch，但 per-message）
- 推送 bot → O(N) 设备
- 通知表 → O(N) 行插入

`NotifyBatch` 已解决 NATS level 的 O(N)，但节点内仍是 O(N)。没有 @everyone 门控、没有在线优先扇出、没有离线惰性回填。

**债务 5：多设备已读状态断裂（D5）**

服务端 `delivery_cursors` 表 + REST API 齐全，但客户端从不读写。A 设备已读的消息在 B 设备始终显示未读。这是一个"90% 完成但卡在最后 10%"的典型债务。

#### 🔵 P3 — 运维债务（影响效率/可观测性）

- 迁移计数依赖建后 `cargo build` 再 migrate（§4.2）— 每次加迁移多一步心智负担
- `routes.rs` 约 3000 行（接近硬限），但尚未拆分到子模块 `routes()` 模式
- 组件健康链式监控缺失 — 无法快速判断集群当前降级状态

---

## 二、高价值扩展方向（TOP 5）

综合 130+ 份分析，筛选 5 个**既有代码已覆盖但未系统性对齐**的方向，按 P0→P2 排序。

### 方向一（P0）：组织网络智能 — 从 IM 数据到组织洞察

**为什么需要**

项目已有丰富的社交图谱数据（communication graph / attention graph / follow graph / org chart / content graph with vector embeddings），但均为"流程副产品"，未被聚合为组织级指标。**企业客户无法问出以下问题**：
- "哪个团队的跨部门协作密度最高？"
- "谁是我们团队实际的意见领袖（按回复/反应/提及入度排名）？"
- "A 团队和 B 团队之间有信息孤岛吗？"

**核心挑战**

1. **社交图计算的冷启动**：首次全量构建社交图需要扫描全表消息/反应/已读记录 — 对 PG 是 heavy query。需要设计增量更新 + 首次离线批量 + 后续订阅 CDC 的混合模式。
2. **指标定义的产品化**："协作密度"不是标准 SQL 聚合 — 需要设计合理的有意义的度量（如：跨团队 DM/频道比、响应时间中位数、子图连通性分数）。
3. **隐私边界**：组织网络分析天然暴露"谁和谁在聊" — 需要设计聚合/匿名层（管理员看趋势不看明细）。

**架构变更**

```
新 crate: aero-analytics (独立，不依赖 aero-server)
├── src/social_graph.rs      # 社交图构建 + 节点/边更新
├── src/metrics.rs           # 指标引擎（度中心性/桥梁分数/孤岛检测）
├── src/compute_pipeline.rs  # 定时 + CDC 触发的增量计算
└── src/storage.rs           # 结果写入 analytics 表

aero-server 集成:
├── routes/analytics_org.rs  # GET /api/workspaces/:id/org-network
├── routes/analytics_team.rs # GET /api/workspaces/:id/teams/:id/collaboration
└── web/analytics/           # 前端 Organisational Network 仪表盘

复用:
- aero-storage::analytics（已有聚合框架）
- aero-common::metrics（已有 Prometheus 指标）
- aero-common::ids（已有 WorkspaceId / ParticipantId）
```

**对现有系统的影响**

- 零破坏性变更 — 全部是只读分析查询
- `analytics` 表已有 workpace-level 聚合框架，扩展为团队级/个人级
- 定时器已存在 `embedding_backfill`（300s），可复用同一 timer 模式

### 方向二（P0）：告警与事件响应平台 — 从聊天到运维指挥中心

**为什么需要**

Slack 最大的企业付费场景是 Incident Response（与 PagerDuty/Opsgenie 合计 ~10B 美元年收入）。当前系统有 Webhook 接收、通知、推送、Bot、审批流 — 但**零事件编排逻辑**。

**核心挑战**

1. **On-Call 排班的数据模型**：轮转、覆盖、交接、节假日日历 — 比简单的 CRON 复杂一个数量级。建议复用时区库 + 时间序列库（如 `chrono` + `time_calc`），不自建日历引擎。
2. **告警去重与聚合**：Prometheus 告警风暴（同一故障触发 100 条告警）→ 需要智能聚合到 1 条事件。这是 PagerDuty 最值钱的部分。
3. **Playbook 执行引擎**：checklist 的逐项进度追踪、自动动作（@人/运行命令/发 Webhook）、半自动（需人工确认）— 这是工作流引擎（方向五）的子集。

**架构变更**

```
新 crate: aero-incident (独立)
├── src/on_call.rs           # 排班引擎（schedule/rotation/override/escalation）
├── src/incident.rs          # 事件生命周期（create/ack/resolve/postmortem）
├── src/playbook.rs          # 运行手册模板 + 执行实例
├── src/webhook.rs           # 告警接收器（Prometheus/Datadog/Grafana webhook normalize）
└── src/storage.rs           # incident 相关仓储

aero-server 集成:
├── routes/incidents.rs      # REST + WS 事件
├── bin/boot/incident_listener.rs # NATS consumer（? new subject or reuse im.room.*）
└── web/incident/            # 事件指挥中心 UI

复用:
- aero-server::webhooks（现成告警接收入口）
- aero-server::notifications（通知分级）
- aero-server::push_bot（移动推送）
- aero-storage::approvals（审批流可适配为事件升级审批）
```

### 方向三（P1）：多模态内容理解管线 — 从盲区到智能化

**为什么需要**

当前上传管线：用户上传图片 → `content_sniff` 验字节模式 → `av_scan` 病毒扫描 → 存储为 `Block::File` → **停在这里**。图片内容一问三不知 — 不可搜索、不可审核、不可访问。

**核心挑战**

1. **视觉审核的延迟**：AI 视觉模型比文本审核慢一个数量级。不能阻塞上传路径 — 必须异步（复用 `moderation_bot` 的有界预算队列模式）。
2. **ONNX 部署 vs API 调用**：Anthropic Vision 是准确的但成本高、延迟不确定。本地 ONNX（如 `nsfwjs`/`CLIP`）延迟低但准确率略低。建议双轨：本地快速过滤 + API 深度分析的级联架构。
3. **OCR 的多语言支持**：截图 OCR 需要覆盖中文（CJK 场景）、英文、代码截图 — PaddleOCR 在中文场景领先，但依赖较大。

**架构变更**

```
在 aero-ai crate 中扩展:
├── src/vision/mod.rs        # 视觉管线编排
├── src/vision/image.rs      # 图片审核/标注/OCR
├── src/vision/video.rs      # 视频关键帧提取 + 场景分析
└── src/vision/alt_text.rs   # Alt-Text 生成

管线集成点:
- content_sniff.rs 后插入: 文件类型判断 → 如果是图片/视频 → enqueue VisualAnalysisJob
- 复现 moderation_bot 的 CostBudget + bounded mpsc 模式
- 结果写入 Block::File.metadata (tags / ai_description / ocr_text)

对现有系统影响:
- Block::File.metadata 已有 JSONB，向前兼容
- AiWorker 现有 Embed/Moderate/Answer 种类，扩展 VisualAnalysis 种类
- 向量搜索索引已有 voyage embedding，OCR 文本直接复用
```

### 方向四（P1）：数据仓库与嵌入式分析管线 — 让数据离开孤岛

**为什么需要**

企业客户几乎一定要求"IM 数据能导入我们的数据仓库"。当前零结构化导出 — 客户只能手动调 REST API 轮询。这是每次 PoC 都会遇到的问题。

**核心挑战**

1. **CDC 管线的选择**：PG logical replication → NATS → connector？或者 outbox pattern（`messages_outbox` 表 → sweep 到 NATS）？PG logical replication 更实时但运维负担重（slot 管理/版本兼容），outbox 模式更可控但延迟 1-5 秒。
2. **目标系统多样性**：Snowflake / BigQuery / S3 Parquet 各有不同写入协议。建议抽象 `ExportConnector` trait，先做 S3 Parquet（最通用），再加 Snowflake/BigQuery。
3. **分租户数据隔离**：不能一个工作区导出的数据能混入另一个工作区的 bucket。

**架构变更**

```
新 crate: aero-export (独立)
├── src/connectors/mod.rs    # ExportConnector trait
├── src/connectors/s3.rs     # S3 Parquet 批次写入
├── src/connectors/snowflake # Snowflake PUT + COPY INTO
├── src/connectors/bigquery  # BigQuery Storage Write API
├── src/changefeed.rs        # CDC 事件消费（NATS 或 outbox sweep）
└── src/schema.rs            # 事件 schema 注册 + 版本管理

对现有系统影响:
- 零破坏性 — 只读已有表，不修改写路径
- 分租户计费数据（消息存储量/API 调用次数）已在 `usage_report.rs` 框架内
```

### 方向五（P2）：流程自动化引擎 — 从孤岛到跨域工作流

**为什么需要**

当前有审批流、任务、定时消息、周期消息、提醒、Auto-mod 规则、Bot 事件 — **但彼此隔离**。不能定义："频道 A 出现关键词'紧急' → 创建任务指派给值班人 → 提醒频道 B → 发 Webhook 给监控系统"。

**核心挑战**

1. **规则引擎不求通用但求够用**：不需要 Drools/正则 DSL。JSON 条件树（`{"field":"message.body", "match":"regex", "value":"P0|SEV1"}`）+ 预编译到 AST 即可满足 95% 场景。
2. **幂等保证**：每个动作执行需要幂等 key（`(workflow_id, trigger_event_id, action_index)`），防止重投导致重复执行。
3. **可视化编辑器**：这是最大的工程投入。建议 Phase 1 先用 JSON/YAML DSL，Phase 2 再上拖拽编辑器。

**架构变更**

```
业务模块: aero-server/src/workflow/
├── mod.rs                  # WorkflowEngine（规则注册 + 匹配 + 调度）
├── trigger.rs              # 触发器注册中心 + 事件绑定
├── condition.rs            # 条件树解释器
├── action.rs               # 动作执行器 + 幂等 + 重试
└── executor.rs             # 执行引擎（NATS consumer + 定时 sweep）

存储: aero-storage/src/workflow.rs
├── workflow_rules 表       # rule_id, workspace_id, triggers JSONB, conditions JSONB, actions JSONB, enabled
├── workflow_logs 表        # execution_id, rule_id, event_id, status, error

复用:
- aero-server::auto_mod（文本匹配原有条件评估模式）
- aero-server::bot_dispatch（事件→动作的分发骨架）
- aero-server::approvals（审批动作的目标模块）
- aero-server::tasks（创建任务动作的目标模块）
```

---

## 三、接口设计原则

### 3.1 新模块的接口契约

基于 AGENTS.md §4.1 的加功能配方，建议统一定义如下 trait 模式：

```rust
// 每个新业务模块暴露 3 个构造点
pub trait FeatureModule {
    /// 1. 仓储构造（aero-storage 或自带）
    type Repo;
    fn repo(pool: &PgPool) -> Self::Repo;

    /// 2. 路由挂载（aero-server routes.rs merge 点）
    fn routes(state: AppState) -> Router<AppState>;

    /// 3. 后台任务注册（bin/boot/ background.rs 装配点）
    fn spawn_background(state: AppState, shutdown: CancellationToken) -> JoinHandle<()>;
}
```

当前 `routes()` 模式已标准化，但缺 `spawn_background()` 模式。建议在 `bin/boot/background.rs` 中统一注册点（currently scattered as `tracker.spawn(...)` in various boot scripts）。

### 3.2 向后兼容策略

| 变更类型 | 兼容要求 | 示例 |
|---------|---------|------|
| **新增 RoomEvent variant** | Web SPA 未注册 handler 时静默丢帧 | `Interaction` / `MessageSeen` 已是这个模式 — 客户端不处理则无感知 |
| **新增 Block kind** | 渲染 fallback 显示 `Unsupported block type` | 当前 `render.js` 有 `default: renderFallback(block)` |
| **新增 API 端点** | 无影响 | 新路由 `routes()` 模式 |
| **修改 API 响应** | 只加字段不加删 | 避免客户解析时 panic |
| **修改 WS 帧结构** | 版本前缀 subject 或 `{"v":2}` 字段 | 当前无版本化 — 这是风险点 |
| **新增依赖组件（如 social graph DB）** | 懒初始化，不可用时跳过 | 参考 `blob_store_from_env` 的 fallback 模式 |

### 3.3 需要引入的新抽象

| 抽象 | 用途 | 所在层级 |
|------|------|---------|
| **`DegradationStrategy<D>` trait** | 每个外部依赖（Redis/NATS/AI）的降级策略 | `aero-common` |
| **`ExportConnector` trait** | 数据仓库连接器抽象 | `aero-export`（新 crate） |
| **`SocialGraphBackend` trait** | 社交图存储后端（Redis vs PG vs 专用图库） | `aero-analytics`（新 crate） |
| **`WorkflowRuleMatcher` trait** | 条件评估抽象（文本匹配 / 角色检查 / 时间窗口） | `aero-server::workflow` |
| **`BoundedTimedCache<K,V>`** | 限时+有界缓存（当前 `LruCache` + `tokio::time` 已散装） | `aero-common` |

---

## 四、技术选型建议

### 4.1 需要评估的新依赖

| 方向 | 候选 | 评估标准 | 建议 |
|------|------|---------|------|
| 社交图分析 | petgraph / rustworkx-core | 纯 Rust 图算法库，petgraph 更轻但算法少，rustworkx-core 算法多但依赖重 | **petgraph**：网络指标只需度中心性/连通分量，petgraph 足够且零新 unsafe |
| 视觉审核 | ONNX Runtime (ort) / Anthropic Vision API | 本地 ONNX 延迟低（<100ms）但需模型文件，API 延迟高（1-5s）但准确 | 双轨：ORT 部署 nsfwjs + CLIP 做快速预审，Anthropic Vision 做深度分析。ORT 的 Rust binding `ort` crate 稳定 |
| OCR | PaddleOCR（C++ lib + FFI） / Tesseract (leptonica) | PaddleOCR 中文领先但 FFI 重，Tesseract 纯 C 但多语言精度不如 PaddleOCR | 看客户群体：国内优先 PaddleOCR，国际优先 Tesseract。初期用 Google Cloud Vision API（零本地依赖） |
| 事件导出 | Arrow/Parquet (`arrow` + `parquet` crate) | 写 Parquet 列存需要 `arrow` + `parquet` crate 约 1MB 编译 | **需要引入**。Parquet 是数据仓库标准格式，Snowflake/BigQuery 原生支持 |
| On-Call 排班 | `time_calc` / 自建 | `time_calc` 支持 recurrence 规则但不支持轮转 | 建议自建（数据模型简单：schedule → rotation → entry），自建代码 <500 行 |
| 工作流 DSL | 自建 JSON 条件树 / serde + enum | 当前 auto_mod 已有 JSON 规则模型可借鉴 | 完全自建。JSON 条件树 + serde 反序列化 + 递归求值，不需要新依赖 |

### 4.2 不引入的技术

| 技术 | 理由 |
|------|------|
| **图数据库（Neo4j / Dgraph）** | 组织网络分析的边查询可用 Redis sorted-set + PG adjacency list 承载，新数据库的运维负担 > 收益。只在社交图 > 10M 边时考虑 |
| **消息队列去 NATS 化** | NATS JetStream 当前覆盖 IM + Live 场景，引入 Kafka 的成本（运维 + 学习曲线）不匹配当前规模 |
| **前端框架（React/Vue/Svelte）** | 当前零依赖 SPA 是项目刻意的"极致轻量"选择。引入框架需同步引入打包工具链、类型系统、测试框架 — 是重大产品决策，非增量改进。建议先补齐 i18n/a11y/错误处理/虚拟滚动等用户体验改进，再评估框架 |
| **WebAssembly 客户端** | 过度工程。当前 SPA 性能瓶颈在渲染（大量 DOM 操作），非计算密集型 |

### 4.3 自建 vs 集成决策矩阵

| 功能 | 自建 | 集成第三方 | 决策 |
|------|------|----------|------|
| 视觉审核 | ONNX 模型部署 | AWS Rekognition / Google Vision API | **自建 ONNX + API 双轨**：数据不出公有云区域是合规要求，纯 API 依赖不可接受 |
| On-Call 排班 | 简单 recurrence + 轮转 | PagerDuty OpsGenie | **自建**：集成第三方意味着用户额外付费购买，产品价值降低 |
| 数据仓库连接 | S3 Parquet + connector | Fivetran / Airbyte | **自建 S3 Parquet**：Fivetran 等维护成本高且数据路径不可控 |
| 组织网络分析 | petgraph + PG data | 无成熟竞品 | **自建**：这就是差异点本身，集成第三方反而同质化 |
| 事件 Playbook | Markdown + 自动动作模板 | ServiceNow / FireHydrant | **自建**：Playbook 模板 + 自动动作是产品核心功能 |

---

## 五、实施路线图

### 5.1 优先级排序

```
P0 ──────────────────────────────────────
  方向一：组织网络智能        | 4-6 周  | 高商业价值，低技术风险
  方向二：告警与事件响应      | 6-10 周 | 高商业价值，高工程投入
  债务 D1：优雅降级框架       | 2-3 周  | 必修，阻塞 P0 方向
  债务 D3：协议断头路修复     | 1-2 周  | 低投入高影响（Interaction/MessageSeen）
  
P1 ──────────────────────────────────────
  方向三：多模态内容理解管线   | 4-6 周  | 合规刚需，复用 AI infra
  方向四：数据仓库管线         | 6-8 周  | 企业销售阻塞项
  债务 D2：Web 前端生产化      | 8-12 周 | 最大产品缺口，持续投入
  债务 D5：多设备已读收敛      | 1 周    | 低投入高影响
  
P2 ──────────────────────────────────────
  方向五：流程自动化引擎       | 10-16周 | 依赖方向二 + Bot 平台
  债务 D4：O(N) 扇出优化      | 4-6 周  | 规模后触发，当前不阻塞
```

### 5.2 阶段划分

**Phase 1（2026 Q3 — 8 周）: 地基硬化**

| 周次 | 目标 | 交付物 |
|------|------|--------|
| W1-W2 | 优雅降级框架（D1） | `DegradationStrategy` trait、Redis/NATS/AI 降级实现、`/health/degraded` 端点、客户端黄色横幅帧 |
| W3-W4 | 协议断头路修复（D3） | `msg:interaction` handler、`msg:message_seen` handler、`msg:welcome` 校验 |
| W3-W4 | 多设备已读收敛（D5） | 客户端写/读 delivery-cursor、`localStorage` 持久化 `_lastSeen` |
| W5-W8 | 组织网络智能 MVP（方向一） | petgraph 社交图构建、入度/桥梁分数指标、`GET /api/workspaces/:id/org-network`、前端仪表盘（网络图 + 团队树） |

**Phase 2（2026 Q3-Q4 — 10 周）: 产品化加速**

| 周次 | 目标 | 交付物 |
|------|------|--------|
| W9-W14 | 告警与事件响应 MVP（方向二） | On-Call 排班引擎、告警接收器（Prometheus webhook）、事件频道自动创建、ACK/升级/Postmortem |
| W9-W12 | Web 前端生产化（D2·第一期） | 错误监控集成（Sentry/Rollbar）、a11y audit + 修复、虚拟滚动消息列表、i18n 框架 |
| W15-W18 | 多模态内容理解 MVP（方向三） | 图片审核管线（ONNX NSFW + Anthropic Vision）、OCR（截图→文本→搜索索引）、Alt-Text 生成 |

**Phase 3（2027 Q1 — 12 周）: 平台化**

| 周次 | 目标 | 交付物 |
|------|------|--------|
| W19-W24 | 数据仓库管线 MVP（方向四） | CDC outbox `messages_outbox` 表、S3 Parquet 导出器、Snowflake connector |
| W19-W24 | Web 前端生产化（D2·第二期） | PWA / Service Worker、离线消息缓存、ESBuild/Vite 打包 + tree-shaking |
| W25-W30 | 流程自动化引擎 MVP（方向五） | JSON 条件树规则模型、`WorkflowEngine`（NATS consumer + 定时 sweep）、6 种触发器 + 6 种动作、可视化编辑器 Phase 1（JSON 编辑器） |

### 5.3 风险点与缓解策略

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| **前端生产化变成重写**（D2 范围蔓延） | 中 | 高 | 严格分阶段：Phase 2 只做 audit 修复 + 虚拟滚动 + i18n，不加框架。Phase 3 再评估打包工具和 PWA。**禁止在 Phase 2 讨论 React 迁移** |
| **方向二编排逻辑膨胀**（On-Call 排班 + 告警聚合复杂度超出预期） | 高 | 中 | MVP 只做固定周轮转（周一→周五 9-5）+ 临时覆盖。日历节假日、智能聚合（Dedup）、自定义轮转放到 Phase 2。**先给用户 80% 的价值** |
| **ONNX 部署运维负担**（模型版本更新、GPU 驱动、跨平台兼容） | 中 | 中 | Phase 1 纯 API（Anthropic Vision），ONNX 本地推理作为 Phase 2 性能优化。**API 先跑通业务流程** |
| **组织网络分析隐私争议**（管理员滥用"谁在和谁聊"数据） | 中 | 高 | **默认脱敏**：团队级聚合不暴露个人、个人排名可 opt-out、审计日志记录谁查看了分析报表。设计隐私白皮书 |
| **方向五和工作流引擎范围太大** | 高 | 高 | 拆分：Phase 1 = JSON 规则引擎 + 6 种动作（无 UI），Phase 2 = 可视化编辑器。"规则引擎"作为独立交付物，不是"完整 Workflow Builder" |

---

## 六、与 AGENTS.md §4 约束的交叉验证

| 约束（§4.x） | 本报告方向 | 合规情况 |
|-------------|-----------|---------|
| §4.1 加功能配方（迁移→仓储→路由→鉴权→实时） | 方向一~五全部 | ✅ 每个方向都遵循此链路。新 `aero-analytics`/`aero-incident`/`aero-export` crate 也遵循 `Cargo.toml` 声明 + `routes()` merge 模式 |
| §4.2 迁移编译期嵌入 | 方向一~五（涉及新表时） | ✅ 无规避。加迁移后必 `cargo build` 再 migrate |
| §4.2 `assert_room_access(participant, room)` 前置 | 所有方向 | ✅ 方向一（组织分析）不涉及 mutating 路由，只读查询也用此守卫确保只看到自己工作区的数据 |
| §4.2 tagged-enum `kind` 撞名 | 方向三（新 RoomEvent variant） | ✅ 遵循 `#[serde(rename=...)]` 规避 |
| §4.2 token helper 同名不 root re-export | 方向四（新 `generate_token` for export） | ✅ 走子路径 `aero_export::connectors::s3::generate_token` |
| §4.2 workspace lints（无新警告） | 全部 | ✅ 新代码 CI 强制执行 `cargo clippy --workspace --all-targets` |
| §4.2 AI 无 key 退化 | 方向三视觉审核 | ✅ 无 Anthropic Vision key 时跳过图片理解，不退化为 blocking path。参考 `HashEmbedder` 模式 |
| §4.2 项目结构治理（根最小化） | 新 crate | ✅ `aero-analytics`/`aero-incident`/`aero-export` 放 `crates/` 下，不污染根目录 |
| §4.2 缓存写读两面 | 方向一（社交图缓存） | ✅ 写路径 `participant_cache.invalidate` + 读路径 `get_or_fetch` |
| §4.2 at-least-once 状态机 | 方向三/五（异步审核/工作流动作） | ✅ 视觉审核复用 `moderation_bot` 的 cost budget + MAX_ATTEMPTS→dead；工作流每个动作有幂等 key `(rule_id, event_id, action_idx)` |
| §4.4 禁区（MLS E2E/联邦/移动 SDK） | 全部 | ✅ 无一触及禁区 |
| §4.4 设计边界（SCIM 仅入站/VOD 仅切片/OpenAPI 示意） | 全部 | ✅ 方向四数据仓库导出不反写 SCIM；方向三不重造转码管线 |
| §4.5 Staging seams（媒体 seam 不强行接线） | 全部 | ✅ 不涉足媒体 seam |

---

## 总结

Aero IM 的架构骨架（事件 DAG + NATS + Hub 扇出 + Redis 集群状态）是正确的，crate 依赖方向是健康的。当前最关键的架构债务是**故障降级策略缺失（D1）**和**协议帧断头路（D3）** — 这两个是 P0 级必修，阻塞一切上层产品扩展。

五个高价值扩展方向中：
- **方向一（组织网络智能）** 是快赢：数据已存在，分析代码是增量的，产品差异化价值极高
- **方向二（告警与事件响应）** 是平台锁定的战略投入：对标 PagerDuty/Opsgenie，复用率最高
- **方向三（多模态理解）** 是合规刚需：企业客户对 NSFW 图片零容忍
- **方向四（数据仓库管线）** 是销售门槛：打通企业数据湖是 PoC 的必需品
- **方向五（工作流引擎）** 是长周期投入：依赖方向二的前置，建议放在 Phase 3

建议从 Phase 1 的"地基硬化"（降级 + 协议修复 + 多设备已读）开始，同步启动方向一 MVP 作为快赢。对于方向二（事件响应），先用 60% 功能（固定轮转 + Prometheus webhook + 事件频道）跑通 PMF，再补充智能聚合和自定义排班。

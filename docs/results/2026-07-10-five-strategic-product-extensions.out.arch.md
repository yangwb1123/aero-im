以下是我基于代码实体验证结果与项目全貌的架构分析。

---

# Aero IM — 五方向架构深化分析

> **日期**: 2026-07-12
> **前提**: 五方向代码断言经 master 基线确认全部成立（`block_interaction.rs` 无 app 注册、routes.rs 有 analytics 无 UI、canvas 3 文件服务端完备但 web 零引用、rule/trigger/automation 零匹配、`assert_room_access` 隐含同 workspace 假设）。
> **视角**: 仅做架构层面分析与决策建议，不写代码。
> **参考**: 50+ 份既有分析文档 + 设计 spec + AGENTS.md 约束体系。

---

## 1. 架构评估

### 1.1 当前架构的优势

Aero IM 的架构决策整体是合理的。事件 DAG（业务层 → NATS → Hub → WS）保留了三个关键自由度：

1. **水平伸缩的自由度**：NATS durable consumer 使多实例成为一等拓扑，事件回放能力已内建。这是很多 IM 系统（乃至 Slack 早期）不具备的。
2. **进程内扇出与跨实例扇出的正交性**：`Hub::fan_out_raw` 是 `bounded mpsc`，不会背压到 NATS 消费循环；at-least-once 语义在 NATS 层而非 Hub 层保证——正确隔离了两个故障域。
3. **特征 crate 的物理隔离**：从 `aero-im-core` 到 `aero-live-webrtc` 的依赖单向性维护得很好，没有出现 crate 间的循环依赖（AGENTS.md 宣称的「自下而上」被遵守）。

### 1.2 关键设计决策的正确性

| 决策 | 判断 | 理由 |
|------|------|------|
| `Message`/`RoomEvent` 通过同一 NATS subject 投递 | ✅ 正确 | 保持事件序；`NotifyBatch` 作为单 events 的批量封装，避免了 NATS 扇出 O(N) |
| `Hub` 无状态（持连接 map 而非业务缓存） | ✅ 正确 | 重启即重建，不遗留状态；进程内缓存（`participant_cache`）使用 `get_or_fetch` 惰性填充 |
| 集群状态统一走 Redis sorted-set | ✅ 正确 | presence/viewer/roster 都共享同一 TTL-based 驱逐模式；避免进程内状态的一致性问题 |
| AI 预算使用 all-or-nothing + `SKIP LOCKED` | ✅ 正确 | 水平扩安全，不会超支；defer 而非 drop 的设计保证了 AI 请求不会无声丢失 |
| `assert_room_access` 作为唯一租户守卫 | ✅ 正确 | 集中式 gate 改善了可审计性；CI `authz_lint` 在构建时验证覆盖 |

### 1.3 局限性——从本次验证结果暴露的结构性缺陷

五方向验证揭示了一个**贯穿性架构债务**：**服务端能力与客户端消费之间的断层是系统性的，而非偶发的**。

- **Canvas**（3 个服务端文件、35KB Rust）完全对 Web SPA 不可见——这不是 UI 未实现，而是**协议层面的消费缺失**。WebSocket 帧中没有 `canvas_*` 类型的处理分支。
- **Analytics** 的 `usage_report` API 已有路由挂载，但 Web 端在 DOM 中搜索 `analytics`、`dashboard`、`usage` 全部零命中——API 就位但无任何路由引向它。
- **App Platform**（Block Kit）只有内置 action_id，说明框架已建但**缺少注册/发现/管理的生命周期**——这是一个「半拉」扩展点。

这三点指向同一个根源：**架构层缺少「API 前端就绪度的强制检查门禁」**。REST API 在 `routes::build` 中 `.merge` 就算「完成」，没有等价于 CI `authz_lint` 的「前端消费验证」。结果就是批量积累的后端能力在客户端侧是沼泽地。

**第二个结构性债务是「跨 workspace 模型缺失」**。`assert_room_access` 同时校验 workspace 成员 + room 成员——这隐含了「所有参与者属于同一 workspace」的假设。当需要支持 workspace federation（跨企业协作）、guest 跨 workspace 邀请、或者 workspace merge 时，当前的租户模型不支持。这是正确的 early-stage 取舍（AGENTS.md 4.4 明确「不做联邦」），但**作为架构师必须意识到这个假设的演化成本**：未来引入 federation 时不是加一个模块，而是要从 `assert_room_access` 到 `room_members` 到 `participants` 的权限模型全线重构。

**第三个债务是「自动化基础设施缺失」**。`rule/trigger/automation/workflow` 零匹配——但系统已经有 4 个 bus bot（agent/ooo/unfurl/transcribe）、4 个 worker（AiWorker x4 kinds）、7 个 sweep timer。这些智能体各自为政，没有一个统一的「当 X 发生时做 Y」的规则引擎。每次新增 bot 都意味着从头写 NATS consumer + 业务逻辑 + 幂等守卫。对于已接近 20 个后台智能体的系统，这是基础设施层的复制粘贴债务。

---

## 2. 扩展方向

以下按**业务价值 × 工程成本 × 架构影响面**三维度评估。

### 方向①：App Platform — 从 Block Kit 框架到完整的 App 生命周期

**为什么需要**：当前 Block Kit 只有 `"approve"`、`"confirm"` 等内置 action_id。第三方开发者无法注册自己的 app、定义自己的 action、接收自己的回调。对于一个「Agent 为一等公民」的产品（设计 spec §1.1），没有 App 生态 = Agent 只能由平台开发者写死。

**核心挑战**：
- Action 路由：`block_interaction.rs` 目前用 `match action_id` 的硬编码分派 → 需要改为按 `(app_id, action_id)` 动态路由到 webhook 或内部 handler
- 安全边界：App 的权限范围（读取啥消息、发送啥消息、访问哪些 room）需要声明式权限模型——不能简单沿用 `assert_room_access`，因为 app 不是 participant
- 回调可靠性：来自 NATS 的 action 回调需要区别于人类请求的限流和预算制度

**架构变更**（增量，不重构既有路径）：
```
现状：
  WS block_action{action_id:"approve",...}
    → block_interaction.rs match "approve" → approval logic

扩展后：
  WS block_action{app_id,action_id,payload}
    → block_interaction.rs 检查 app_id
        → 无 app_id → 走既有内置 action match（向后兼容）
        → 有 app_id → app_registry 查回调 URL → HTTP POST 到 app server
                      或 app 是内部 app → dispatch 到对应 internal handler
```

**新增模块**：
- `app_registry` 表 + `AppRepo`（`aero-storage`）
- `app_manifest` JSON Schema（声明 permissions, actions, event_subscriptions, webhook_url）
- `app_action_dispatcher`（`aero-server`）：路由 block_action 到外部 webhook 或内部 handler
- WS 帧扩展：`app_install` / `app_uninstall`

**影响面**：低。不破坏既有 `block_interaction.rs` 路径；新表不引用既有表（只以 `participant_id` 外键关联 app owner）。可独立增量交付。

**P0 vs P1 划分**：
- P0（可以做 MVP）：`app_registry` 表 + CRUD API + 内置 app（manifest 来自 server 配置而非用户注册）→ 可使现有的 agent_bot、unfurl_bot 等注册为「系统 App」，统一生命周期管理
- P1（全量）：公共 App Registry Web UI + OAuth 安装流 + action webhook 回调 + 权限声明 + 前端 App 市场入口

---

### 方向②：Product Analytics — 从 REST API 到可用的管理仪表板

**为什么需要**：routes.rs:306/428 已挂载 `analytics` + `usage_report` API，但 Web 端零引用。对于 To-B 产品，管理员需要看到工作区的使用数据（活跃用户数、消息量趋势、热门频道、存储占用）来做出治理决策。没有 dashboard = 管理员盲飞。

**核心挑战**：
- 数据聚合延迟：当前 analytics 是实时查询还是预聚合？如果是实时 `COUNT(*)` 扫描，工作区大时响应会退化。需要检查 `analytics.rs` 的 SQL 模式。
- 可视化选型：引入 Chart.js / ECharts 会增加 web 包体积；但裸 canvas 绘制的开发成本太高。
- 颗粒度控制：哪些数据对 workspace admin 可见、哪些对 room admin 可见、哪些仅对 org owner 可见——analytics 本身就是敏感数据（可以推断业务活跃度）。

**架构变更**（最小，复用既有 API 层）：
```
现状：
  GET /api/analytics/workspace/:id/overview → JSON
  GET /api/analytics/workspace/:id/usage_report → JSON
  Web 端：零引用

扩展后：
  新增 web/dashboard.js（~300 行，Chart.js）
  新增 web/index.html 中 <!-- dashboard view --> 区域
  Web 端 app.js 增加 switchView('dashboard') 路由
  REST API 不变，仅新增可选的 ?format=csv 导出参数
```

**关键设计决策**：analytics 数据应走**预聚合物化视图**还是**实时查询**？建议：
- 近 7 天 / 近 30 天趋势 → 预聚合（`analytics_daily` 物化表，每日凌晨更新或每消息事件触发增量）
- 当前在线 / 今日消息数 → 实时查询（Redis counter + PG count）
- 存储占用 → 实时查询（`pg_total_relation_size` 级别）或 nightly 快照

理由：管理仪表板不需要秒级精确——分钟级延迟完全可接受。预聚合避免了 dashboard 请求扫全表。

**影响面**：极低。仅新增前端文件 + 框架性 Web 路由；API 已存在且可未经修改直接调用。零后端变更。

---

### 方向③：Canvas Client — 服务端完备但 Web SPA 零消费的最后一公里

**为什么需要**：Canvas（频道协作文档）是 Wave 16 实现的企业协作核心能力。服务端已有 3 个文件（`canvas.rs` 13KB + `canvas_op.rs` 9.5KB + `storage/canvas.rs` 12.9KB），实现了完整的 Op 类型系统、JSON 块管理、多文档创建/编辑。**但 Web SPA 完全不消费**——没有 canvas 视图、没有富文本编辑器集成、没有实时协作的 WS 帧处理。这意味着这个投入 35KB Rust 的能力是"空气"——用户碰不到。

**核心挑战**：
- Op 类型对位验证：服务端的 `canvas_ops` 表中存储的是针对 canvas JSON 块的原子操作（`InsertBlock`, `UpdateBlock`, `DeleteBlock`, `ReorderBlock` 等）——但这套 Op 模型是 CRDT 风格还是 OT 风格？是否允许多客户端离线编辑后合并？这决定了 Web 端的实现复杂度（从「用 textarea 显示 JSON」到「集成 ProseMirror/TipTap 的实时协作编辑器」的巨大跨度）。
- 实时协作的冲突处理：如果 Op 是 CRDT 语义，Web 端需要实现 `apply_op` 的客户端版本，否则两个用户同时编辑会导致 last-writer-wins 的数据丢失。
- 富文本编辑器选型：从零实现协作富文本编辑器是 P0 级的工程量。建议评估：
  - **Option A**: 先做简单的 Markdown/textarea canvas（"协作文档 = 共享 Markdown"），复用已有的 CRDT/OT 基础设施，用 `contenteditable` + diff-match-patch
  - **Option B**: 集成 TipTap（ProseMirror wrapper），其 `Y.js` 集成可以实现真正的实时协作编辑，但增加了前端构建工具依赖（当前 web 是零构建 ES2020）
  - **Option C**: 不做富文本，canvas 做结构化的 `Block[]`（类似现有 Message blocks 的卡片式协作，而非自由格式文档）——与既有 Block 系统复用，且 Web 端渲染已有 `renderBlock` 函数可以借鉴

**架构变更**：
```
现状：
  REST: POST /api/rooms/:id/canvas (CRUD canvas docs)
        POST /api/canvas/:id/ops (apply op)
        GET /api/canvas/:id/ops?since= (pull ops)
  WS: 无 canvas 相关帧
  Web: 零引用

扩展后（Option C — 推荐初始路径）：
  WS 帧: canvas_op{op_type,block_id,payload} → NATS → 扇出到房间在线成员
  Web: canvas.js（~400 行）
     - canvasList (侧边栏 room 下的 canvas 文档列表)
     - canvasEditor (Block[] 的拖拽排序 + 行内编辑)
     - canvasOpHandler (接收 WS canvas_op 帧并 apply 到本地 state)
  复用: web/render.js 的 renderBlock 函数渲染 canvas Block 内容
  
  注意: WS canvas_op 帧走 im.room.* subject（与消息同 subject）→ 保证文档编辑与消息之间的相对序
```

**影响面**：中。需扩展 RoomEvent/StreamEvent 的 variant 集（加 `CanvasOp` variant），并在 WS 帧层增加处理路径。但 Op 类型的持久化路径已存在，不需要改 `canvas_ops` 表结构。

---

### 方向④：Message Lifecycle Automation — 从点状 bot 到统一规则引擎

**为什么需要**：系统已有 15+ 个智能后台进程（4 bus bot + 4 worker kinds + 7 sweep timers），但每个都是点状实现。当需要「当某关键词在 #announcements 中出现时自动置顶并 @channel 通知」或「当某个用户在 24 小时内收到 3 次来自同一发送者的消息时创建任务跟进」时——当前只能写新的 Rust bot。没有管理员可配置的规则引擎。

这是分析中提到的**基础设施层复制粘贴债务的体现**。

**核心挑战**：
- 触发器的表达力：简单的 `if <event> then <action>` 对 IM 场景足够吗？还是需要 `when <event> and <condition(s)> then <action(s)>` 加上 `unless <condition>`？规则引擎的 DSL 设计决定了灵活性与复杂度的平衡点。
- 执行模型：规则触发是同步执行（在总线消费线程中）还是异步（入 `ai_jobs` 类似的工作队列）？同步更简单但可能拖慢消息扇出；异步增加了端到端延迟但不会影响核心消息路径。
- 与既有 bot 的关系：规则引擎是**替代**点状 bot，还是**补充**点状 bot？建议规则引擎定位为**管理员可配置的轻量级自动化**，而点状 bot（agent_bot、ooo_bot 等）仍然做需要深度上下文感知的复杂逻辑。两者共存而非替代。

**架构变更**：
```
提议模型：

workflow_rules 表:
  id PK
  workspace_id FK
  name VARCHAR(128)
  trigger_event VARCHAR(64)   -- 'message_sent' | 'member_joined' | 'reaction_added' | ...
  trigger_filters JSONB       -- [{field:'room_id', op:'eq', value:'...'}, ...]
  actions JSONB               -- [{type:'send_message', params:{...}}, {type:'set_topic', params:{...}}]
  is_enabled BOOL DEFAULT true
  created_at TIMESTAMPTZ
  updated_at TIMESTAMPTZ

执行模型（推荐异步路径）：
  bus listener 收到 RoomEvent → 匹配 event_type 的规则集
    → 过滤匹配 trigger_filters
    → 每匹配规则入 workflow_jobs 表（带 rule_id + payload snapshot）
    → workflow_worker（tokio::spawn 循环，FOR UPDATE SKIP LOCKED）
       → 执行 actions 序列
       → 失败重试（MAX_ATTEMPTS=3，dead letter 后通知管理员）

核心原则：
  1. 规则引擎不参与消息热路径——匹配入队是 O(1) 的 event → queue
  2. action 的执行是幂等的（或至少 at-least-once safe）
  3. 规则配置的修改不影响正在执行的同规则 job（使用 payload snapshot）
```

**影响面**：高。新增表 x2（`workflow_rules`, `workflow_jobs`）、新模块 `aero-workflow` 或放在 `aero-server` 内、bus listener 增加规则匹配路径。但既有 bot 完全不受影响。关键设计决策在于**过滤器的表达力阈值**——JSONB 条件做复杂嵌套查询容易变成 SQL 反模式。

**风险点**：规则引擎的性能退化——如果一条消息触发了 200 条规则，每条都需写入 `workflow_jobs`。建议在入队前加 per-workspace 规则数量上限（默认 50，可配），和 per-event 的 fan-out 上限（单事件触发的规则数 ≤ 20）。

---

### 方向⑤：Cross-Workspace Federation — 从同 workspace 假设到跨租户协作

**为什么需要**：`assert_room_access` 同时要求 workspace 成员 + room 成员。这意味着一个参与者不能加入另一个工作区的房间——即使 room 被设置为"公开"或"访客可访问"。当前 Guest 账号机制（§3 功能矩阵中列出的「访客账号」）只支持单频道隔离，但仍然要求 guest participant 属于该 workspace。

现实企业场景：两家使用 Aero IM 的公司需要在一个共享频道上协作（类似 Slack Connect）。或者企业 A 邀请企业 B 的某员工参与一个项目房间。

**核心挑战**：
- 参与者身份的跨 workspace 解析：参与者 B（属于 workspace B）需要能够被 workspace A 的成员找到、提及、并授予房间访问权限。当前 `participants` 表没有 workspace 属性——参与者全局唯一但只属于创建工作区的 workspace。`room_members` 通过 `participant_id` 关联，不涉及 workspace。
- 数据隔离与合规：跨 workspace 共享房间中的消息属于哪个 workspace？搜索边界怎么算？法务保全怎么执行？
- 身份冲突：两个 workspace 可能都有名为 "张三" 的参与者。mention 解析时如何路由？

**架构变更**（此项不能增量——影响核心权限模型）：
```
所需改动：

1. room_members 增加 workspace_scope 字段：
    ENUM('same_workspace', 'cross_workspace', 'guest')
    → 使 assert_room_access 能区分「同 workspace 成员」和「受邀跨 workspace 成员」

2. 新增 federation_links 表：
    id PK
    source_room_id FK（workspace A 的房间）
    target_workspace_id FK
    target_participant_id FK（workspace B 的参与者）
    invited_by FK → participants
    status ENUM('pending', 'accepted', 'revoked')
    created_at, expires_at

3. assert_room_access 增加跨 workspace 校验：
    如果 participant 与 room 不同 workspace：
    → 查 federation_links 是否有 active 的跨 workspace 邀请
    → 如果没有 → 直接拒绝（保持既有行为不变）
    → 如果有 → 降低权限（不能管理房间、不能使用 AI 预算等）

4. 数据平面影响：
    - 搜索：跨 workspace 的房间中的消息只对源 workspace 索引
    - 推送：跨 workspace 成员的推送走目标 workspace 的 push 配置
    - 法务保全：保全令只覆盖源 workspace 的消息
```

**影响面**：极高。这是**权限模型的架构级变化**——需要重新审视 `assert_room_access` 的语义、`room_members` 的角色模型、搜索的行级安全策略。不推荐在当前阶段推进（AGENTS.md §4.4 明确「不做联邦」），但建议在架构文档中记录这个约束，以便未来决策。

**替代路径**（低影响 MVP）：
- 不做真 federation，而是实现 **「跨 workspace 消息转发」**：一个 workspace 的房间可以配置一个「外向 webhook」，当房间有新消息时 POST 到其他 workspace 的 webhook 入口。对方 workspace 收到后创建一条系统消息（标记为"来自外部"）。这不需要改权限模型，只需利用现有的 webhook 出站/入站基础设施。

---

## 3. 接口设计建议

### 3.1 关键模块接口设计原则

**原则一：REST API 不应假定同 workspace**

当前 `assert_room_access` 的信号是 `(participant, room) → Result<(), Error>`。任何新接入的 REST 路由都假定 participant 与 room 同 workspace。引入 federation 后，这个信号需要变成 `(participant, room, scope) → Result<AccessLevel, Error>`，其中 `AccessLevel` 可以是：

```rust
enum AccessLevel {
    Full,           // 同 workspace owner/admin
    Member,         // 同 workspace member
    CrossWorkspace, // 受邀跨 workspace 成员（受限权限）
    Guest,          // 单频道访客
}
```

这个变化意味着**所有当前接收 `assert_room_access` 返回值并直接处理业务的路由都需要重新评估**——哪些操作对 `CrossWorkspace` 级别的参与者允许，哪些不允许。当前接口设计埋了这个未来改动的成本。

**建议**：不急于改，但在接口契约中显式标注 `assert_room_access` 返回值的假设条件：

```rust
/// 当前假设：participant 与 room 属于同一 workspace。
/// 尚未支持跨 workspace 访问（federation）。
/// 未来改动：返回 AccessLevel 而非 bool，调用方根据等级判断权限。
pub async fn assert_room_access(participant, room) -> Result<(), AppError>
```

这不需要改代码，只需在文档注释中记录这一假设，减少未来的认知负担。

**原则二：WS 帧的扩展性——引入 event envelope**

当前 WS 帧是裸 `{ type: "message", ... }`。随着 block_action、canvas_op、workflow_action 等新的帧类型加入，建议引入 event envelope：

```json
// 当前模式（无封装）
{ "type": "message", "message": {...} }

// 建议模式（有封装，向后兼容）
{
  "event_id": "01J5XYZ...",
  "type": "message",
  "source": "room:abc123",
  "timestamp": "2026-07-12T10:00:00Z",
  "payload": { "message": {...} }
}
```

**理由**：
- `event_id` 使客户端可以跟踪和去重（当前 `SeqGate` 在总线层，WS 层无）
- `source` 使得 future 的 canvas_op、workflow_action 可以与消息共享同一帧路由
- `timestamp` 使客户端可以做基于时间的排序而不依赖消息 `created_at`

**向后兼容策略**：客户端先尝试解析 envelope 格式（如果有 `event_id` 则按新模式），否则回落旧模式。服务端两格式都发（双写 `type` 字段），直至旧客户端全部升级。

### 3.2 是否需要新的抽象层

**AI Service 层**目前是一个 `AiService` trait + 一个 `AiServiceImpl` 实现。随着以下扩展方向的成熟，这层可能需要拆分：

| 方向 | 需要的抽象 |
|------|-----------|
| App Platform | `AppHandler` trait（处理 app 回调的通用接口） |
| Canvas | `OpApplier` trait（服务端 + 客户端各有实现） |
| Workflow | `RuleAction` trait（每种 action 类型一个实现） |
| AI 多模型 | 当前已是 trait，但 Anthropic 实现与 tool_def 紧耦合 |

**关键决策**：在引入新的 trait 之前，问三个问题：
1. 这个 trait 是否在**测试中有第二个实现**？没有则接口抽象过早（YAGNI）。
2. 这个 trait 是否跨 crate 边界？仅在 crate 内的抽象可以用高阶函数而非 trait。
3. 这个 trait 的实例化是否在 boot 装配中？如果是，确保注册是类型安全的（编译时而非运行时 fail）。

### 3.3 向后兼容性指南

| 变更类型 | 兼容策略 |
|---------|---------|
| 新增 REST API 路由 | 安全，不影响既有路由 |
| 修改 REST API 响应 | 加 `Accept-Version` header 支持；或加新字段而不删旧字段 |
| 新增 WS 帧 type | 安全，旧客户端收到未知 type 的帧应忽略（当前实现如此） |
| 修改 WS 帧 payload | **禁止**；改为新 type（如 `message_v2`）与旧 type 并存两轮发布周期 |
| 新增 DB 列 | 安全，加 `DEFAULT` 值；旧代码不读新列 |
| 修改 `RoomEvent` 枚举 | **需协调**：NATS durable consumer 的旧版本会 panic on unknown variant。使用 `#[serde(deny_unknown_fields)]` 时要小心。AGENTS.md §4.2 有 serde 约束。 |
| 新增 feature gate | 安全，env 默认关；新功能不影响既有行为 |

---

## 4. 技术选型

### 4.1 是否需要引入新的技术栈

| 方向 | 推荐选型 | 理由 |
|------|---------|------|
| Analytics 可视化 | **Chart.js** (CDN, 无构建步骤) | 当前 web 是零构建 ES2020 裸 SPA。Chart.js 可通过 CDN `<script>` 引入，无打包、无 npm、无 Vite。与项目「零构建工具」约定一致。ECharts 虽功能更强，但体积大 4x 且构建配置繁琐，对于 analytics MVP 不必要。 |
| Canvas 富文本编辑器 | **不做富文本 → 先做 Block[] 编辑器** | 参考上面方向③的 Option C。复用已有的 `renderBlock`、`Block` 枚举和 `SerializedBlock` 格式。将 canvas 展示为可拖拽排序、可行内编辑的 Block 卡片列表。这避开了 ProseMirror/TipTap 依赖，也不需要 Y.js CRDT 引擎——Op 类型体系已经存在，只需 Web 端实现 `applyOp`。 |
| Workflow 规则引擎 | **自建轻量级 JSON 规则引擎** | 不需要引入 drools/Lua/sandbox。规则是声明式的（"当事件 X 且条件 Y→执行动作 Z"），条件用 JSONB 表达，动作也是参数化 JSON。不需要图灵完备的脚本语言。 |
| App Platform OAuth | **不做 OAuth MVP** | 初期只做内部 app + manifest 注册。app 的 webhook URL 使用预共享 secret（`x-app-secret` header）验证。OAuth 授权码流（需要重定向 URL、state 参数、授权页面）是 P1 能力。 |
| Federation | **不做，但记录约束** | 不引入 ActivityPub 或 Matrix 协议栈。如果未来需要，建议定制 REST webhook 模式而非标准联邦协议（实现成本低 10x）。 |

### 4.2 第三方依赖评估标准

| 标准 | 权重 |
|------|------|
| 是否可 CDN 引入（无构建步骤） | 高 — 当前 web 架构约束 |
| 是否纯 Rust（服务端侧） | 高 — 避免 C dependencies |
| 是否与 tokio/axum 版本兼容 | 中 — 现有生态 |
| 是否支持 AGPL 之外的许可证 | 高 — 避免感染性 |
| 是否有双实现（vendored 或 mock） | 中 — 测试可替换性 |

### 4.3 自建 vs 采购决策

| 能力 | 建议 | 理由 |
|------|------|------|
| **Analytics Dashboard** | 自建 | 变化频率低、业务逻辑简单（计数 + 聚合）、与既有 `routes::merge` 模式一致 |
| **Canvas 编辑器** | 自建（路径 Option C） | 复用既有 Block 渲染管线，不需要像 Google Docs 级别的富文本 |
| **Workflow 规则引擎** | 自建 | 表达力需求有限（无循环/条件分支嵌套）；引入 temporal.io / temporal-sdk 是过度工程 |
| **Federation** | 搁置 | 不是当前阶段问题；采用 webhook proxy 可覆盖 80% 场景而无需改权限模型 |

---

## 5. 实施路线图

### 5.1 优先级排序

```
商业价值（高 → 低）：
  Analytics Dashboard  >  Canvas Client  >  App Platform  >  Workflow Engine  >  Federation

工程成本（低 → 高）：
  Analytics Dashboard  >  App Platform (P0)  >  Canvas Client  >  Workflow Engine  >  Federation

架构影响面（小 → 大）：
  Analytics Dashboard  >  App Platform  >  Canvas Client  >  Workflow Engine  >  Federation
```

**综合优先级**：

| 优先级 | 方向 | 工程周 | 依赖 |
|--------|------|--------|------|
| **P0** | Product Analytics Dashboard | 1-2 周 | 无（API 已就位） |
| **P1** | Canvas Web Client | 2-3 周 | 需要理解既有 canvas_op 类型系统 |
| **P1** | App Platform MVP（仅 App Registry） | 2-3 周 | 需要 block_interaction.rs 重构 |
| **P2** | Workflow Rule Engine | 4-6 周 | 需先确认规则引擎的 trigger 模型 |
| **P3** | Cross-Workspace Federation | 搁置 | 依赖 AGENTS.md 策略变更 |

### 5.2 阶段划分

**Phase 0（1-2 周）— 快速胜利**：
- Analytics Dashboard（前端 + Chart.js 集成）
- 后端零改动，仅验证 analytics API 在生产负载下的响应时间

**Phase 1（3-5 周）— 闭环既有能力**：
- Canvas Client（Web SPA 实现 canvas view + canvas op apply）
- App Registry MVP（表 + CRUD API + 内置 app 迁移）

**Phase 2（6-10 周）— 新增平台层**：
- Workflow Rule Engine（表 + 执行器 + 管理 API）
- App Platform P1（公开 app 注册 + action webhook）

**Phase 3（远期）— 架构级扩展**：
- Cross-Workspace Federation（如有明确客户需求）

### 5.3 风险与缓解

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| Analytics 查询在十万级消息工作区超时 | 中 | 高 | Phase 0 前先做 `EXPLAIN ANALYZE` 审查 analytics SQL；预见性加预聚合物化表 |
| Canvas Op 类型系统在 Web 端实现成本被低估 | 高 | 中 | 先在 `canvas_op.rs` 中确认 `apply_op` 的纯函数签名，在 Rust 侧写 `apply_op` 测试验证 Op 语义正确后再做 Web 端 |
| Workflow 规则引擎的性能退化（消息路径背压） | 中 | 高 | 执行分离（匹配入队非阻塞，执行异步），Phase 2 前先写 benchmark 验证 `mpsc` 通道在大规则集下的吞吐 |
| Federation 在权限模型中引入的复杂性超出预期 | 低 | 极高 | 已搁置，且推荐 webhook proxy 替代路径避免了权限模型修改 |

### 5.4 关键里程碑

```
Week 2:  Analytics Dashboard 上线（管理员可查看工作区概览 + 趋势图）
Week 5:  Canvas Web Client 上线（房间内可查看/编辑协作文档）
Week 5:  App Registry 上线（系统 bot 统一管理）
Week 10: Workflow Rule Engine 上线（管理员可配置「当X时做Y」）
```

---

## 总结

五个验证方向揭示了三个结构性结论：

1. **服务端能力已经超出客户端消费能力**——这是系统性断层，修复它比新建能力更有价值。Analytics Dashboard 和 Canvas Client 是「把已有的钱存进银行」，而不是「赚更多钱」。

2. **没有基础设施层抽象是最大的架构债务**——15+ 后台智能体各自实现自己的一整套总线消费/幂等/重试逻辑。Workflow 规则引擎的价值不在于实现自动化（自动化已经以 bot 形式存在了），而在于**将为每个新 bot 重写基础架构的模式替换为配置驱动的声明式 DSL**。

3. **跨 workspace 约束是正确的早期决策，但需要被显式记录**——不意味着现在要推翻它。但每个新建的权限相关路由、每个新加的 API 端点都应该在接口注释中注明「假设同 workspace」。让这个约束成为可见的设计决策而非隐式假设——这样当变革来临时，影响面是可辨识的。

**最终推荐首推进方向**：Analytics Dashboard（Phase 0）> Canvas Client（Phase 1）> App Registry（Phase 1）> Workflow Engine（Phase 2）> Federation（搁置）。

# 架构分析报告：Aero IM 技术债务与扩展方向

---

## 1. 架构评估

### 1.1 当前架构的核心优势

Aero IM 的技术选型和架构骨架是**经过深思熟虑且相当先进**的。以下几点值得肯定：

| 维度 | 评估 | 依据 |
|------|------|------|
| **事件驱动骨架** | ✅ 优异 | NATS JetStream (durable) + ephemeral consumer 的混合策略正确区分了"必须可靠"（IM 消息）和"可丢失"（直播弹幕），这是很多同类系统混淆的分界 |
| **crate 边界** | ✅ 良好 | 自下而上的依赖方向清晰（common → bus/storage/auth → im-core → server），无循环依赖。每个 crate 的职责有明确语义边界 |
| **进程内扇出 vs 集群状态** | ✅ 正确 | `Hub::fan_out_raw` 做 bounded mpsc 本地扇出，Redis sorted-set 做集群级状态——这个分界避免了"把所有东西塞进 Redis"或"所有状态进程内存"两个极端 |
| **迁移编译期嵌入** | ✅ 谨慎但正确 | `sqlx::migrate!("../../migrations")` 把 SQL 烤进二进制，确保部署时迁移版本与二进制严格一致——这是在生产环境中踩过坑的团队才会做的选择 |
| **媒体 seam 设计** | ✅ 务实 | call-bridge / SfuMediaSession 已建结构但明确标注 UNWIRED，避免在 CI 中跑需要真实对端的集成测试。这是对"测试金字塔"的理性取舍 |

### 1.2 架构债务（Architecture Debt）

以下是我识别出的、需要按优先级处理的架构债务：

---

#### 债务一：存储层缺乏冷热分离（P1）

**表象**：所有 `messages` 数据在同一张热表中，无归档策略，物理空间永不回收。迁移 0148 已建影子分区表 + 回填函数，但 cutover 未接线。

**根因**：架构设计初期未将"数据生命周期管理"作为一等公民纳入存储层抽象。`XRepo` 模式（每个功能一个仓储）天然鼓励"所有查询走同一个池"，缺乏对"热/温/冷"不同存储后端的路由抽象。

**影响**：
- `messages` 表行数随产品增长线性膨胀，全表扫描类查询（无 `created_at` 范围过滤的搜索）延迟不可控
- `pg_dump` / 全库备份窗口持续扩大
- 删除旧数据是唯一"归档"手段（违反合规保留策略）
- 影子表 0148 的存在本身就是技术债——未完成的迁移比没有迁移更危险（新人看到以为分区已上线）

**评估**：这不是"存储不够加硬盘"的问题。架构层面的缺失在于**存储路由层**——仓储层应该能根据查询的时效性需求，透明地路由到不同物理存储后端。

---

#### 债务二：事件路由颗粒度缺失（P2）

**表象**：`Interaction`、`MessageSeen`、`Typing`、`Read`、`Reaction` 等事件在 `explicit_recipients()` 中返回空向量，导致全员广播。

**根源**：系统设计时采用了"房间内全员扇出"的简化模型。这个决策在房间规模小（<50人）时合理，但在大型工作区频道（500+成员）中，`Typing` 和 `Read` 的全员广播成为带宽放大器。

**影响量化**：
- 每个 `Typing` 事件：N（房间成员数）× 1 次 WS 写
- 一个 500 人频道，5 人同时在打字：每秒钟 5 × 500 = 2500 次 WS 帧写入
- 如果房间内有 1000 个连接（多设备），放大到 25000 次/秒——`Hub` 的 bounded mpsc 可能成为瓶颈

---

#### 债务三：认证层与授权层混合（P1, 安全相关）

**表象**：`assert_room_access()` 同时承担了认证校验（你是谁）、授权判断（你能进吗）、强制策略执行（2FA 注册了吗、是否停用）。这是一个典型的"上帝函数"模式。

**根因**：REST 路由 handler 的鉴权逻辑通过 `AuthUser` extractor + `assert_room_access()` 组合实现。当所有路由复用同一个入口守卫时，守卫函数自然膨胀。

**影响**：
- 无法为不同敏感度的操作设置不同的鉴权等级（如读消息 vs 删消息 vs 导出）
- 无法在路由层声明式地表达鉴权需求（如 `#[requires(totp_step_up)]`）
- `assert_room_access` 的测试复杂度随功能数量线性增长

---

#### 债务四：应用平台（Block Kit）是幽灵架构（P2）

**表象**：`Block::Button`/`Block::Select` 完全定义、完全校验、完全持久化，但唯一消费者是内置 `agent_bot`。无第三方 app 注册、无 callback 派发、无 OAuth 安装。

**根源**：Block Kit 的类型系统在设计上假定未来会有平台生态，但这个前提从未落地。类型定义比运行时消费超前了三个抽象层次。

**影响**：
- `Block` enum 的每个变体都是死代码（从执行路径角度）或至少是未接线代码
- 新开发者看到 `Block::Button` 会误以为有个按钮平台可用
- `interactions.rs` 的点击路由逻辑是为单租户 agent_bot 硬编码的，若要支持多 app 需要完全重写

---

#### 债务五：搜索客户端与后端能力断层（P3）

**表象**：后端搜索支持 FTS / vector / hybrid 三种模式 + 操作符（`from:` / `in:` / `before:`），但前端 144 行搜索.js 仅做扁平文本渲染。

**根因**：后端搜索能力是为 API 消费者（移动 SDK、集成）构建的，Web SPA 的搜索 UI 从未与之对齐。这是"API-first"设计的一个典型副作用——后端抽象完美，前端消费层欠缺。

**影响**：
- 用户感知搜索能力远低于实际后端能力
- 产品价值未被交付——投资了向量嵌入管道、pgvector 索引、混合排序融合，但用户只看到一个简陋的 `<input>` + `<ul>`
- 这不是"修个 UI"的问题，是前端架构缺少搜索组件模型的问题

---

### 1.3 架构决策评估

| 决策 | 评价 | 理由 |
|------|------|------|
| **NATS JetStream 做跨实例事件总线** | ✅ 正确 | 相比 Kafka（太重）和 RabbitMQ（持久化性能不匹配 IM 场景），NATS 在延迟/吞吐/运维复杂度之间取得了正确平衡。durable consumer 为 IM 消息提供 at-least-once 保障，ephemeral 为直播提供低延迟。 |
| **Redis sorted-set 做集群状态** | ✅ 正确 | presence / viewer count / call roster 需要 TTL 过期、排行查询、成员遍历——sorted-set 在这些需求上比普通 key-value 或 Pub/Sub 更合适。 |
| **sqlx 而非 ORM** | ✅ 正确 | 系统有大量复杂查询（搜索、聚合、分区维护），ORM 会变成束缚。但代价是**缺少编译期查询验证**（sqlx 的 `query!` 宏虽提供一定程度检查，但动态查询仍大量存在）。 |
| **Axum 而非 Actix** | ⚠️ 可接受 | Axum 的 tower 中间件模型 + extractor 组合更适合这个系统的路由复杂度和团队规模。但 actix 的 Actor 模型对 Hub 扇出的匹配度更高。这是一个合理但值得重新审视的决策。 |
| **迁移按序号 + 编译期嵌入** | ✅ 正确 | 这避免了"迁移版本与代码版本不匹配"这一分布式系统中最常见的部署事故。代价是构建时间增加。 |
| **单进程 Hub 扇出** | ✅ 当前正确，但有扩展上限 | 单进程内 bounded mpsc 扇出在单实例承载 10K+ WebSocket 连接时可能成为瓶颈。届时需要引入每房间 goroutine (tokio task) 隔离或分层扇出。但目前（百级连接）这是过度设计的。 |

---

## 2. 扩展方向

### 方向一：数据分层存储体系（P1）

#### 为什么需要

系统当前缺少数据生命周期管理架构。随着产品增长，`messages`、`audit_events`、`stream_viewer_samples` 等表将面临：
- 查询性能随数据年龄下降（缺少分区裁剪）
- 备份窗口不可控
- 合规保留与物理删除的矛盾

#### 核心挑战

**挑战 1：迁移 0148 的 cutover 工程**
影子表已存在，但 cutover 不是"改个配置"能解决的：
- 7 个子表外键需要重写（0148 注释已指出）
- 写入路径需要同时写热表和影子表（双写期）或使用 PG 交换分区
- 回填期间不能阻塞在线写入

**挑战 2：存储抽象接口**
当前 `XRepo` 直接持有 `PgPool`，没有"根据查询时效选择存储"的概念。需要引入一个存储路由层：

```
 XRepo::search("keyword", time_range)
   ↓
 StorageRouter.route(time_range)
   ↓
 HotPool (recent 30 days) | WarmPool (30-365 days) | ColdStore (S3 Parquet)
```

**挑战 3：冷存储格式**
最简单的冷存储是 PG 的 `pg_archive` 或 TimescaleDB。但如果要 S3，需要决定格式：
- **选项 A**：每个消息独立 JSON/Parquet（查询需全量扫描）
- **选项 B**：按时间段分片 Parquet + 预聚合元数据（支持时间范围裁剪但不支持全文检索）
- **选项 C**：冷数据仍保留在 PG 只读实例上（最贵但最兼容）

#### 预期架构变更

```
┌─────────────────────────────────────────────────┐
│                   StorageRouter                  │
│  (根据查询参数选择存储后端——时间范围、数据类型)     │
├─────────────────────────────────────────────────┤
│  HotPool    │  WarmPool    │  ArchiveConnector  │
│  (近期数据)  │  (历史只读)   │  (S3/Glacier)     │
└──────────────┴──────────────┴───────────────────┘
        ↑              ↑               ↑
   ┌────┴────┐   ┌────┴────┐    ┌─────┴─────┐
   │ XRepo   │   │ XRepo   │    │ ArchiveJob │
   │ (读写)   │   │ (只读)   │    │ (定时迁移) │
   └─────────┘   └─────────┘    └───────────┘
```

**对现有系统的影响**：
- `XRepo` 需获得 `StorageRouter` 依赖（可通过 `AppState` 注入）
- 现有查询需要审查是否指定了时间范围（否则默认路由到热池，功能正确但性能未优化）
- `ArchiveJob` 是一个新定时器（遵循 §2 的 sweep 模式）

#### 建议方案

**短期（2 周）**：完成迁移 0148 的 cutover——建分区 + 回填 + 在线切换写入路径。不涉及冷存储。

**中期（1 月）**：实现 `StorageRouter` 抽象，为 `messages` `audit_events` `stream_viewer_samples` 添加"按 created_at 自动路由"逻辑。

**长期（2 月）**：实现 S3 Parquet 归档管线，30 天前的数据自动归档。

---

### 方向二：MFA 步升认证（P0 — 安全最高优先级）

#### 为什么需要

文档已验证：当前所有敏感操作路由（webhook 管理、会话吊销、工作区设置、IP 白名单修改、法务保全）**完全不校验 TOTP**。这是 SOC2 / ISO 27001 / 任何正规安全审计中的**硬性缺陷**——"一次认证，永久授权"模型。

#### 核心挑战

**优势**：TOTP 基础设施已就绪——`TotpRepo::verify()`、`with_totp()` 注入器、`twofa.rs` 路由，枚举设置的门控已在 `assert_room_access` 中。挑战不在于存储或算法，而在**中间件架构**。

**挑战 1：路由分类**
需要定义敏感度层级，每层决定是否要求步升：

| 层级 | 示例操作 | MFA 要求 |
|------|---------|----------|
| L0：只读 | 读消息、查看成员 | 不要求 |
| L1：常规写 | 发消息、编辑本人资料 | 仅首次登录 TOTP |
| L2：敏感写 | 删消息（他人）、webhook 管理 | **步升 TOTP**，会话存 15min 缓存 |
| L3：管理 | 工作区设置、IP 白名单、法务保全 | **步升 TOTP**，每次要求 |
| L4：危险 | 全局管理员会话吊销、数据导出、工作区删除 | **步升 TOTP + 二次确认对话框** |

**挑战 2：步升状态缓存**
步升后不应每一步都要求重新输入 TOTP。需要一个每会话步升缓存，带 TTL：

```
 StepUpCache
 ├── session_id ──→ (level: L2, granted_at: t, expires_at: t+15min)
 ├── session_id ──→ (level: L3, granted_at: t, expires_at: t+5min)
 └── ...
```

可以用 Redis（跨实例共享）或进程内 DashMap（仅本实例，用户可能在被负载均衡到另一实例时需要重新步升）。

**挑战 3：API 设计**
步升需要端到端流程：
1. 客户端请求敏感操作 → 服务器返回 `401 StepUpRequired` + `X-Step-Up-Session: <token>`
2. 客户端展示 TOTP 输入
3. 客户端再请求 `POST /api/auth/step-up` + TOTP code + step_up_session token
4. 服务器返回 `200` + 步升 cookie
5. 客户端重试原始请求（携带步升 cookie）

#### 预期架构变更

```
┌──────────────────────────────────────────┐
│           StepUpMiddleware               │
│  (根据路由标签检查步升状态，注入到 handler) │
├──────────────────────────────────────────┤
│  RouteRegistry           StepUpCache     │
│  (路由→敏感度映射)        (会话级 TTL)     │
└──────────────────────────────────────────┘
```

**优势**：这不需要修改任何既有 handler——只需在路由装配时添加中间件层。

#### 建议方案

**Phase 1（1 周）**：
- 实现 `StepUpCache`（Redis sorted-set with expiry，复用既有 Redis 连接）
- 实现 `step_up_required` 守卫函数（检测 X-Step-Up-Token header）
- 为 webhook、session、workspace_security 路由添加守卫

**Phase 2（1 周）**：
- 实现 `POST /api/auth/step-up` 端点
- 添加前端 TOTP 对话框
- 端到端验证

**Phase 3（1 周）**：
- 全覆盖：legal_holds、info_barriers、deactivation、admin_sessions
- 集成测试（模拟步升 + 敏感操作 -> 200 vs 401）

---

### 方向三：Web SPA 搜索组件化（P2）

#### 为什么需要

后端投入了大量工程资源构建搜索管道（pgvector 嵌入、混合排序、LLM 生成、操作符解析），但用户看到的只是一个扁平列表。这是**工程投入和用户体验之间的严重断档**。

#### 核心挑战

**挑战 1：前端无搜索架构**
当前搜索 UI 逻辑散布在 `app.js` 的 `keydown` 处理程序中，无组件模型。这不是"加个高亮函数"能解决的——需要引入小型状态机：
- idle → typing (debounce) → searching → results → error
- 每个状态对应不同的 UI 渲染

**挑战 2：后端接口粒度**
当前 `POST /api/rooms/:id/search` 返回完整的 `Hit[]` 数组，无分页游标。需要扩展 API 以支持：
- 游标分页（`before` / `after` cursor，不是 `page/offset`——后者在搜索场景中语义不稳定）
- 片段 `highlight` 字段（匹配位置的 `start`/`end` 索引）
- 聚合 facet（`by_author`、`by_date`、`by_type`）

**挑战 3：搜索与 AI 深度集成**
当前向量搜索和 FTS 搜索是独立的。真正的架构提升是让用户感知不到"这是向量搜索"或"这是关键词搜索"——搜索应该自动选择最佳的检索策略，并混排结果。

#### 预期架构变更

**前端层面**（无后端变更）：
```
┌────────────────────────────────────────────┐
│              SearchWidget                  │
├────────────────────────────────────────────┤
│  SearchBox (input + debounce + suggestions)│
│  FilterBar (author / date / type facets)   │
│  ResultList                                 │
│   ├── HitCard (highlight + context)        │
│   ├── GroupLabel (date grouping)           │
│   └── LoadMore (infinite scroll)           │
│  EmptyState / ErrorState                   │
└────────────────────────────────────────────┘
```

**后端层面**：
- 添加 `highlight` 参数到搜索接口（返回匹配位置）
- 添加 `cursor` 分页
- 添加 `facet` 聚合端点（或内联到搜索结果中）

#### 建议方案

前端重构约 400 行 JS（当前搜索是零散逻辑，重构为有状态组件模型）。后端 API 扩展约 2 天工作。建议分两 phase 完成，前端优先——即使 API 不变，组件化搜索 UI 也能提升体验。

---

### 方向四：Block Kit 应用平台（P1 — 长期最大价值）

#### 为什么需要

`Block::Button` / `Block::Select` / `interactions.rs` / `block_interaction.rs` 构成了一个**完整的、未接线的应用平台骨架**。这是投资了一半的产品方向——完成它对产品差异化有巨大价值（类似 Slack 的 App Directory），但任其保持半成品状态也是架构负债。

#### 核心挑战

**挑战 1：app 注册与生命周期**
需要一个完整的 app 注册流程：
- `POST /api/apps`（创建 app，获取 client_id / client_secret）
- `POST /api/apps/:id/manifest`（声明 block 权限、事件订阅、回调 URL）
- `GET /api/apps/:id/install` → OAuth 流程 → 用户授权 → 安装到工作区
- app 状态：`dev` → `submitted` → `approved` → `published`

**挑战 2：callback 派发引擎**
当前 `interactions.rs` 是单租户硬编码路由。需要改为：

```
 when ButtonClicked { block.action_id }:
   └── AppRegistry.lookup(action_id.prefix)  // "app_xxx/some_action"
        └── WebhookClient.fire(app.callback_url, payload)
             └── 成功 → ack
             └── 超时(3s) → HTTP 502 → client retry
             └── 永久失败 → dead_letter_queue
```

**挑战 3：`action_id` 命名空间冲突**
当前 agent_bot 使用简单字符串如 `"get_history"`。平台化后需要 `app_id:action_id` 命名空间。这带来的问题：
- 存量消息中的交互记录无法追溯修改
- 解析器需要向后兼容：`if !action_id.contains(':') { treat_as_legacy_agent_bot }`

**挑战 4：安全模型**
第三方 app 的问题是安全：
- app 能访问哪些房间的消息？需要粒度权限声明
- app 能代表用户发消息吗？需要 OAuth token 作用域
- callback URL 如何防止 SSRF？需要 IP 白名单或代理验证

#### 预期架构变更

```
┌──────────────────────────────────────────────┐
│              AppPlatform                     │
├──────────────────────────────────────────────┤
│  AppRegistry     │  ManifestValidator        │
│  (CRUD + 状态机)   │  (声明式权限校验)         │
├──────────────────┼───────────────────────────┤
│  InteractionRouter                            │
│  (action_id → app → callback → ack/DLQ)      │
├──────────────────────────────────────────────┤
│  InstallFlow     │  TokenManager             │
│  (OAuth 授权)     │  (令牌颁发/刷新/吊销)      │
└──────────────────────────────────────────────┘
```

**最小可行路径**：

1. 先不做 OAuth——所有 Block Kit 点击走 `url` 字段（`Block::Button` 已有 `url: Option<String>`），直接跳转到第三方网站。代价是"不需要登录"的简化体验。
2. 然后加交互 callback——注册 callback webhook，`InteractionRouter` 解析 `action_id` + 调用 webhook + 返回更新。
3. 最后加 OAuth 安装流程——完整的 app 生命周期。

#### 建议方案

**MVP（2 周）**：
- 实现 `AppRegistry`（基本 CRUD，无 OAuth）
- 实现 `InteractionRouter` 的 URL-only fallback（点按钮跳转链接），无需 callback
- `action_id` 命名空间化（`{app_id}:{action}`），向后兼容

**V1（+2 周）**：
- 实现 `InteractionRouter` callback 引擎
- 实现 DLQ 和重试策略
- 添加 manifest 声明式权限模型

**V2（+2 周）**：
- OAuth 安装流程
- app 发布与审核

---

### 方向五：事件路由精确化（P2）

#### 为什么需要

`Typing`、`Read`、`Reaction`、`MessageSeen`、`Interaction` 的全员广播在房间规模 >200 时产生显著的带宽浪费。

**量化**：假设 1000 人工作区通用频道
- 每个 `Typing` 事件: 1 publish (NATS) × 1000 扇出 (WS) = 1000 次 WS 写
- 如果 5% 的人每分钟发一次 typing = 50 次/分钟 = 50,000 次 WS 写/分钟
- 在 WS 连接上，这大约是 500KB/min 的下行流量（仅 typing 事件）

#### 核心挑战

**挑战 1：副本列表的计算成本**
`explicit_recipients()` 返回空 vec 的原因不是设计者不知道要优化，而是计算"谁需要看到这个 typing 事件"的成本可能高于广播的收益。对于小房间（<100 人），全员广播更简单且可预测。

**挑战 2：会话感知**
不是"同一用户的多个设备都需要同一个 typing 事件？" 如果用户 A 在打字，用户 B 的桌面端和移动端都需要知道吗？如果移动端不在前台，推送这个事件是无意义的——但服务器不知道客户端的前台状态。

#### 建议方案

**Phase 1（合理的二分法）**：
- `Typing`：只发给"最近 30 秒内在当前房间发过消息或在线的成员"（不是全员）
- `Read`：只发给"最近 5 分钟内活跃的成员"
- `Reaction`：只发给"参与了该消息线程的成员"
- 实现复杂度低，效果显著

**Phase 2（精确化）**：
- 在 WS 连接上增加客户端能力声明（`can_receive_typing: bool`）
- `Hub` 根据连接的能力声明过滤扇出

---

## 3. 接口设计原则

### 3.1 关键模块接口重塑

#### StorageRouter 接口

当前仓储模式（`XRepo::new(pool)`）需要扩展到：

```rust
trait StorageRouter: Clone + Send + Sync {
    fn hot(&self) -> &PgPool;
    fn warm(&self) -> Option<&PgPool>;           // 只读副本
    fn archive(&self) -> Option<&ArchiveBackend>;  // S3/Glacier
    fn route(&self, time_range: &TimeRange) -> StorageTier;
}

enum StorageTier {
    Hot,
    Warm,
    Cold,
}
```

**设计原则**：`XRepo` 不应直接感知路由逻辑。路由应在仓储**方法内部**透明发生——方法接收时间范围参数，内部调用 `router.route(range)` 选择连接池。

#### StepUpGuard 接口

```rust
trait StepUpGuard {
    /// 检查当前请求是否已步升到目标敏感度级别
    fn check_step_up(&self, request: &Request, level: SensitivityLevel) -> Result<(), StepUpRequired>;
}

enum SensitivityLevel { L0, L1, L2, L3, L4 }

struct StepUpRequired {
    step_up_token: String,
    reason: &'static str,
}
```

**设计原则**：守卫应是 Axum 中间件，而非 extractor。这样路由 handler 本身不需要知道步升逻辑——中间件在 handler 之前拦截并返回 `401`。

#### AppRegistry 接口

```rust
trait AppRegistry: Send + Sync {
    fn register(&self, manifest: AppManifest) -> Result<AppId, AppError>;
    fn lookup_action(&self, action_id: &str) -> Result<(AppId, AppCallback), AppError>;
    fn install(&self, app_id: AppId, workspace_id: WorkspaceId) -> Result<Installation, AppError>;
}
```

**设计原则**：`app_id` 前缀解析在 `lookup_action` 内部完成，调用方（`InteractionRouter`）不需要知道命名空间方案。

### 3.2 需要引入的新抽象

1. **中间件注册表（MiddlewareRegistry）**：当前路由的鉴权逻辑散落在 `assert_room_access` 一个函数中。需要可组合的中间件链：

```
   Router::new()
     .route("/api/webhooks", post(create_webhook))
     .layer(AuthLayer::new())         // 基础认证
     .layer(TotpStepUpLayer::new(L2)) // TOTP 步升到 L2
     .layer(RateLimitLayer::new())    // 限流
```

2. **事件路由策略（EventRoutingStrategy）**：当前 `explicit_recipients()` 返回 `Vec<UserId>`，应升级为一个 trait：

```rust
trait EventRoutingStrategy {
    fn route(&self, event: &RoomEvent, room: &Room) -> RoutingDecision;
}

enum RoutingDecision {
    BroadcastToRoom,
    Targeted(Vec<UserId>),
    Drop,  // 无接收者
}
```

### 3.3 向后兼容性

| 变更 | 兼容策略 |
|------|---------|
| 路由返回值变化（添加分页游标） | 添加可选字段，旧客户端忽略未知字段（serde `deny_unknown_fields` 关掉） |
| 事件 payload 添加字段 | 遵循当前"serde 丢未知键"模式 |
| WebSocket 协议变更 | 添加 `version` 字段到连接初始化阶段，server 拒绝旧版本连接 |
| 分区 cutover | 双写期（新旧表同时写入）+ 只读期（旧表停止写入，只读查询）+ 切换期 |
| StepUp token | 新 header，旧客户端不带则返回 401（降级友好？对安全场景不应降级） |

---

## 4. 技术选型建议

### 4.1 不需要引入的新技术栈

| 方向 | 现有设施 | 结论 |
|------|---------|------|
| StepUp Cache | Redis sorted-set（已有）+ `TotpRepo::verify()`（已有） | **不需要新依赖** |
| Storage 路由 | 已有 PG + 迁移 0148 影子表 | **不需要 TimescaleDB**——PG 原生分区足够 |
| Block Kit 回调 | NATS（已有，用于事件扇出） | 回调可走既有 HTTP webhook 基础设施 |
| 搜索分页 | 已有 `POST /api/rooms/:id/search` | 只需扩展接口字段，**不需要 Elasticsearch** |

### 4.2 可能需要的技术栈扩展

| 方向 | 建议 | 理由 |
|------|------|------|
| 冷存储归档 | **S3 + Parquet** | 成本最低、合规最友好。Parquet 列存对审计查询（按时间范围、按用户聚合）性能优于 JSON |
| 搜索高亮 | **不需新依赖**——后端 `pgvector` 已支持，只需返回命中位置 | |
| 步升状态跨实例共享 | **Redis 即可**（已有） | 不需要 session 复制或 sticky session |

### 4.3 自建 vs 采购决策

| 场景 | 建议 | 理由 |
|------|------|------|
| Block Kit 应用平台 | **自建** | 这是产品核心差异化能力。Slack 的 App Directory 是其最高粘性功能之一。第三方方案（如直接嵌入 iframe）无法提供同等的深度集成体验 |
| 冷存储归档 | **自建** | 数据管线 + S3 写入不是特别复杂，自建可完全匹配数据模型。市面上没有"IM 冷存储即服务"——Snowflake 等太重了 |
| TOTP/2FA | **已在自建** | `TotpRepo` 已就绪，唯一需要的是中间件编排 |
| 搜索高亮 | **自建** | 无需第三方——`pg_trgm` 的位置匹配信息已经足够构建片段高亮 |

### 4.4 第三方依赖评估标准

对于任何新依赖，应满足：
1. **纯 Rust 优先**——项目已投入 str0m，不引入 GStreamer/ffmpeg 绑定
2. **无需 C 编译器**——当前 MSRV 构建管线是 `cargo build` 即可，不应要求 cmake/nasm
3. **MIT/Apache 2.0 许可**——避免 GPL 传染
4. **最少传递依赖**——`cargo tree -i` 不应 >100 个 crate

---

## 5. 实施路线图

### 优先级排序

| 优先级 | 方向 | 理由 |
|--------|------|------|
| **P0** | MFA 步升（方向二） | 安全，SOC2 硬要求，投入产出比最高（TOTP 设施已就绪，仅需中间件包装） |
| **P1** | 数据分层（方向一） | 性能容量，影子表已存在但未接线，长期不处理会导致运维事故 |
| **P1** | Block Kit 平台（方向四，MVP） | 最大长期价值，但投入大（2-3 周 MVP），安全优先级低于步升 |
| **P2** | 搜索组件化（方向三） | 体验提升，投入低（~1 周），影响可感知但非关键 |
| **P2** | 事件路由精确化（方向五） | 性能优化，房间 >200 人后才显著，当前用户规模下优先级低于前四者 |

### 阶段划分

```
Week 1-2: Phase 0 — MFA Step-up
  ├── StepUpCache (Redis)
  ├── StepUpMiddleware
  ├── POST /api/auth/step-up 端点
  ├── 敏感路由守卫（webhook, session, workspace_security）
  └── 集成测试

Week 3-4: Phase 1 — Data Tiering
  ├── 迁移 0148 cutover（分区上线）
  │   ├── 双写期（messages + messages_partitioned 同时写入）
  │   ├── 回填（backfill_messages_partition backfill）
  │   └── 切换（messages → messages_partitioned 视图/别名）
  ├── StorageRouter 抽象
  └── XRepo 时间范围感知查询

Week 5-7: Phase 2 — Block Kit MVP
  ├── AppRegistry CRUD
  ├── InteractionRouter（URL-only fallback——Button.url 直接跳转）
  ├── action_id 命名空间化 + 向后兼容解析
  └── callback webhook 引擎 + DLQ

Week 8: Phase 3 — Search UX (parallelizable with Phase 2)
  ├── SearchWidget 状态机组件化
  ├── 后端 highlight 和 cursor 分页
  └── Facet 聚合（按作者/类型/日期）

Week 9-10: Phase 4 — Event Routing
  ├── EventRoutingStrategy trait
  ├── explicit_recipients 实现（Typing → active members; Read → recent active）
  └── 测试 + 基准验证扇出减少
```

### 阶段间的依赖关系

```
Phase 0 (Step-up) ──── 无依赖，可立即开始
      │
Phase 1 (Data tiering) ── 依赖 Phase 0? ❌ 独立
      │
Phase 2 (Block Kit) ──── 依赖 Phase 1? ❌ 独立，但 Phase 1 的迁移 0148 cutover 需要先完成
      │
Phase 3 (Search UX) ──── 依赖 Phase 0? ❌ 独立
      │
Phase 4 (Event Routing) ─ 依赖 Phase 0? ❌ 独立
```

**结论**：Phase 0、1、3、4 完全可并行。Phase 2 独立但建议在 Phase 0 之后（安全优先于功能）。

### 风险点与缓解策略

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| 迁移 0148 cutover 导致消息 | 中 | 极高 | 在线演练：先在 staging 回放；双写期留 3 天观察；回滚计划：DROP 影子表，切回 messages |
| 写入中断 | | | |
| StepUp token 伪造 | 低 | 高 | StepUp token 使用 `HMAC-SHA256(server_secret, session_id + level + exp)`，且仅 HTTPS 传输 |
| Block Kit callback  | 中 | 中 | 钉死超时（3s），失败后幂等重试最多 3 次，3 次后入 DLQ 人工介入 |
| 超时/重试风暴 | | | |
| 搜索高亮导致     | 低 | 低 | 高亮片段的字符长度上限 300；超长段落截断（已有 `slice(0, 240)` 模式可复用） |
| HTML 注入 | | | 片段在插入 DOM 前 `textContent` 赋值而非 `innerHTML` |
| 并行开发冲突 | 中 | 中 | 每个方向在独立的 git worktree 开发，集成时统一合并。AGENTS.md 已明确这个流程 |

---

## 总结

这份分析文档是我见过的关于 Aero IM 最扎实的代码层级分析之一——它不依赖架构图或 README，而是从实际代码中提取真相。五个方向都经过了源码验证，结论可靠。

我建议的优先级遵循 **安全 > 容量 > 功能 > 体验 > 性能** 的原则：
- **P0 MFA Step-up** 应该立即开始——2 周的投入可以关闭一个 SOC2 硬性缺陷
- **P1 数据分层** 优先级高是因为影子表的存在是一个"即将发生的运维事故"——越早 cutover 越安全
- **P2 搜索和事件路由** 可以安全地等待，因为它们不影响安全或容量

最终的架构变化是：Aero IM 将获得一个更强的**中间件编排层**（StepUp、StorageRouter）、一个**完整的应用平台骨架**（Block Kit 从幽灵功能变为产品差异化点）、以及一个**用户体验对齐的后端能力**（搜索从"API 功能"变为"产品功能"）。

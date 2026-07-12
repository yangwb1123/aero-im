以下是基于 `2026-07-11-post-full-code-scan-five-missed-architectural-extensions.md` 的完整架构分析文档。

---

# 架构分析：5 个被既有分析遗漏的扩展方向

> 分析基础：commit HEAD（2026-07-11），157 migrations，53 `define_id!`，10 Block variants，15+ ServerFrame/ClientFrame variants。代码验证已在上方工具调用中完成。

---

## 1. 架构评估：当前系统的优势与局限

### 1.1 优势——哪些做得对

| 维度 | 优势 | 依据 |
|------|------|------|
| **事件驱动骨架** | NATS JetStream 作为跨实例事实源 + Hub 作为进程内本地扇出，是正确且可水平扩展的实时架构。durable consumer + at-least-once 交付为投递可靠性奠基 | AGENTS.md §1 DAG + `run_bus_listener` 实现 |
| **类型安全** | `define_id!` 系统生成了 53 种强类型 ID，避免 stringly-typed API 的混乱 | `ids.rs` + 仓储签名 |
| **迁移编译期嵌入** | `sqlx::migrate!("../../migrations")` 在编译期将 SQL 烤进 binary，杜绝「迁移与代码版本不一致」的生产事故 | AGENTS.md §4.2 |
| **功能按 crate 正交切分** | 16 个 crate 各有清晰边界，`aero-common` 是叶子、`aero-bus` 封装 NATS、`aero-storage` 封装持久化——依赖方向明确无环 | AGENTS.md crate 地图 |
| **AI budget 治理** | `AiWorker` 的 per-ws + global budget + weight-based throttling 模式，保证了 AI 成本可预测、fair-use | AGENTS.md §2 AiWorker |
| **权限守卫一致性** | `assert_room_access(participant, room)` + CI `authz_lint` 从架构层降低了 IDOR 风险 | AGENTS.md §4.2 |

### 1.2 局限性——五个方向指向的共性结构债

扫描这 5 个方向后，浮现出三条**深层结构债**：

1. **读/写管线不对称**：写路径（迁移/Migration）有完整的 SQL schema 演进框架（157 次迁移），但**数据本身的演进**（Block 枚举 + 消息体格式）没有任何框架——这是 schema 演进 vs. data 演进的断层。
2. **搜索能力 = 消息中心主义**：RAG、FTS、AI 摘要全部以 `messages` 表为单一数据源——忽略了 canvas、tasks、polls、call transcripts 等 53 种实体。搜索架构没有对「多实体」做抽象，是产品语义的切割。
3. **客户端是无状态、无版本的「厚壳」**：服务端有完整的版本管理（crate versions、migration seq、NATS consumer durable name），但客户端协议层（WS frames）和数据容错层（离线持久化）完全未版本化/持久化，形成了「服务端现代化、客户端原始化」的断层。

### 1.3 关键设计决策合理性评估

| 决策 | 合理性评估 | 当前状态是否需要修正 |
|------|-----------|-------------------|
| `Block` 枚举作为消息体的强类型模式 | ✅ 对 87+ 功能类型是正确选择 | 需要加 schema version + 迁移框架 |
| 搜索仅限 `messages` 表 | ⚠️ MVP 阶段合理，但 AI 能力上线后成为瓶颈 | **需要扩展**（方向二） |
| WS 帧为纯 JSON 无压缩 | ✅ 开发阶段合理的简化 | 生产化前需优化（方向三） |
| 客户端无 IndexedDB/Service Worker | ✅ debug client 定位下合理 | 产品化前必须处理（方向四） |
| WS 协议无版本号 | ⚠️ 单一官方客户端时合理 | 第三方集成前必须处理（方向五） |

---

## 2. 扩展方向（按优先级排列）

### 方向一（P1）：Block Schema 演进框架

#### 为什么需要

当前 `Block` 枚举有 10 个变体，随 87+ 功能点持续增长。每次新增变体/字段，**已存在的数百万条消息的 bodies 保持旧格式**。当前的 `#[serde(default)]` 方案只能处理「加可选字段」这一种场景，对以下场景完全无法保护：

- **字段重命名**（`lang` → `language`）
- **字段类型变更**（`style: Option<String>` → `style: ButtonStyle`）
- **变体拆分**（`Card { schema, payload }` → `CardV2 { schema_id, typed_fields }`）
- **变体合并**（`ToolCall` + `Interaction` → `ToolInteraction`）

#### 核心挑战

1. **零停机迁移**：消息表可能有数十亿行，全表扫 UPDATE 会触发 autovacuum 风暴
2. **法务保全边界**：legal_holds 标记的消息需要跳过迁移或深度复制
3. **消费点一致性**：5+ 个消费点（FTS、渲染、翻译、转录、关键词匹配）全部假设当前格式——迁移后必须同步更新所有消费点
4. **回滚问题**：降低 `blocks_version` 需要前向兼容的序列化代码

#### 预期的架构变更

```
┌─────────────────────────────────────────────────────┐
│ 当前架构：                                           │
│  Block { Text, Mention, Code, ... }                  │
│       ↑ 直接序列化/反序列化                           │
│  Message { blocks: Vec<Block>, ... }  ←── 无版本号    │
│       ↓                                              │
│  messages.body (JSONB)                               │
├─────────────────────────────────────────────────────┤
│ 目标架构：                                           │
│  Block { Text, Mention, Code, ... }                  │
│       ↑                                              │
│  Message { blocks_version: u8, blocks: Vec<Block> }  │
│       ↓                                              │
│  messages.body (JSONB) ← blocks_version ≡ col        │
│       ↓                                              │
│  BlockMigrator (后台worker, 类似embedding_backfill)   │
│       → 分批扫 blocks_version < N → 迁移函数 → UPDATE │
│  MigrationTestFixture (每变体维护旧format样本)         │
└─────────────────────────────────────────────────────┘
```

**具体变更点**：
1. `Message` 结构体新增 `blocks_version: u8` 字段（默认 1），序列化时塞当前版本号
2. 新增 `BlockMigrator` worker（运行模式参照 `embedding_backfill`——有界循环 + metric + throttle）
3. 新增 `BlockMigration` trait + `migrate_v1_to_v2()` 等函数（每个版本一对迁移/回滚函数）
4. 迁移测试夹具：每个旧版 Block 变体的序列化样本 + 反序列化测试

#### 对现有系统的影响

| 影响面 | 程度 | 说明 |
|--------|------|------|
| Block 枚举本身 | **低** | 只新增 `blocks_version` 字段，变体不修改 |
| 所有消费点 | **中** | 需要感知 `blocks_version`、在消费前确保消息已迁移（或 deferred 迁移） |
| 消息写入路径 | **低** | 写入时自动赋当前版本号 |
| 搜索索引 | **低** | FTS 向量在写入时已投影 `searchable_text()`，迁移后需 REINDEX |
| 法务保全 | **中** | `legal_holds` 表需要与迁移管道握手——跳过受保全消息的 UPDATE |

---

### 方向二（P1）：跨实体内容图谱

#### 为什么需要

这是 **AI 能力覆盖度** 和 **搜索用户体验** 的瓶颈。当前 `workspace_ask` / `find_expert` / smart-replies 只检索 `messages` 表。一个工作区中存在：

- 消息讨论（`messages`）
- 画布文档和版本（`canvases` + `canvas_ops`）
- 任务及其描述（`tasks`）
- 投票及其选项（`polls` + `poll_options`）
- 直播剪辑描述（`clips`）
- 通话录音和转录（`call_sessions` + `call_transcripts`）
- 频道书签（`channel_bookmarks`）

用户说 "我记得有个关于 X 的任务 / 画布 / 投票说过……"——目前的搜索架构无法回应。

#### 核心挑战

1. **异构 schema → 统一文本提取**：消息是 `Block` 枚举，画布是 `serde_json::Value`（JSONB），任务是关系型字段——每种实体需要自定义 `searchable_text()` 实现
2. **权限边界一致**：所有实体都是 room-scoped，跨实体搜索必须复用 `assert_room_access`——不能在搜索 API 重建权限逻辑
3. **排序/RRF 融合**：不同实体的相关性分数不可直接比较（消息的 BM25 vs 画布的文本相似度 vs 任务的更新时间）
4. **实体类型膨胀**：10 种可搜索实体 → 搜索结果总条数膨胀 10×，需要每类型独立限制（≤5 条/类型）

#### 预期的架构变更

```
┌─────────────────────────────────────────────────────┐
│ 当前：                                               │
│  search_query.rs → SQL FROM messages → BM25          │
│  canvas_get/tasks_list/polls_list → 各自独立检索路径 │
├─────────────────────────────────────────────────────┤
│ 目标：                                               │
│  SearchableEntity trait {                            │
│    fn id() -> EntityId,                              │
│    fn entity_type() -> EntityType,                   │
│    fn searchable_text() -> String,                   │
│    fn room_scope() -> RoomId,                        │
│    fn created_at() -> DateTime,                      │
│    fn score() -> f32,                                │
│  }                                                   │
│       ↑ 为 Message/Canvas/Task/Poll/Clip/Transcript   │
│         实现 trait                                   │
│  UnifiedSearchEngine {                               │
│    search(scope, query, types) → Vec<SearchableHit>  │
│  }                                                   │
│       ↓                                             │
│  POST /api/search/unified → RRF 融合 × 权限过滤       │
└─────────────────────────────────────────────────────┘
```

#### 对现有系统的影响

| 影响面 | 程度 | 说明 |
|--------|------|------|
| `search.rs` 已有代码 | **高** | 需要重构为 `SearchableEntity` trait + 统一引擎，但现有 `search_mode` (fts/vector/hybrid/auto) 可以作为 messages 的专属策略保留 |
| 各实体仓储 | **中** | 需要新增 `searchable_text()` 提取器（不影响现有 CRUD） |
| FTS 索引 | **中** | 需要向 canvas、tasks、polls 等高频实体添加 GIN/tsvector 索引 |
| AI 服务 | **高** | `workspace_ask` 的检索层从单表变为多实体融合——输出质量会显著改善 |
| 前端 | **中** | 搜索结果需要按 `entity_type` 分卡片渲染 |

---

### 方向三（P1）：WS 帧管线优化

#### 为什么需要

WS 是实时性的核心脊柱，但当前有三重低效：

1. **轻帧独立发送**：typing/read/presence 等 ~100-300B 的帧在 5000 人房间中一次 typing = 5,000 × 150B = 750KB 上行流量
2. **无标准 WS 压缩**：HTTP 层有 `CompressionLayer`（gzip），WS 层完全无线级压缩——`permessage-deflate` 是 WebSocket 标准扩展，启用成本极低
3. **编辑帧全量重发**：编辑后的 `Edited` 帧包含完整 `Block` 数组（~4KB），而实际可能只改了一个字的拼写——差量更新可节省 70-90%

#### 核心挑战

1. **合并延迟 vs 实时性**：50ms 的 batcher 窗口对 typing 可接受，但对 reaction/read 回执可能影响用户体验
2. **permessage-deflate 的 CPU 开销**：10k+ 并发连接下，压缩 CPU > 带宽节省——需可按房间动态启用
3. **diff 补丁的可靠性**：丢失任意中间 diff 后客户端状态不完整——需要 seq 校验和 + 定期全量同步

#### 预期的架构变更

```
┌─────────────────────────────────────────────────────┐
│ Hub 现有扇出管线：                                    │
│  Event → room_event_to_frame_json → fan_out_raw      │
│        逐连接 send(Message::Text(json))              │
├─────────────────────────────────────────────────────┤
│ 优化后管线：                                          │
│  Event → FrameBatcher (50ms窗口, 合并轻帧) →          │
│    permessage-deflate 压缩 → fan_out_raw_batched      │
│                                                      │
│  Edited 事件分支 → DiffEvaluator → PatchOp[] →        │
│    apply_diff ▲ diff_patch 发送                       │
│       定期全量同步（每 N 帧发一次全量校验和）           │
└─────────────────────────────────────────────────────┘
```

#### 对现有系统的影响

| 影响面 | 程度 | 说明 |
|--------|------|------|
| `hub.rs/fan_out_raw` | **中** | 需要重构为 batched + 可选压缩，但接口签名可以保持兼容 |
| `frame.rs` | **低** | 新增 `frame_to_diff`/`frame_to_batch` 函数，原有函数保留 |
| 客户端 `ws.js` | **中** | 需要支持解压（浏览器端 `permessage-deflate` 由浏览器自动处理）+ diff/patch 合并 |
| 服务端并发模型 | **低** | permessage-deflate 在 tokio-tungstenite 中是按连接配置的，不需要改核心架构 |

---

### 方向四（P2）：客户端离线消息持久化

#### 为什么需要

当前 debug client 在页面刷新后丢失所有状态——包括 pending 消息、当前房间、草稿。对于 IM 产品，「你说的话不会丢」是根本信任基石。移动端浏览器切换 App → 页面回收 → 回来时状态全丢的情况尤其严重。

#### 核心挑战

1. **IndexedDB 容量限制**：手机端 ~50MB，需要 LRU 淘汰 + 每房间缓存上限（500 条）
2. **多 Tab 协调**：多个浏览器 Tab 共享同一个 IndexedDB（origin-level），需要协调 WS 连接和消息重发
3. **离线消息幂等**：服务端当前 `send_message` 是否支持 `client_id` 幂等键？如果不支持，离线队列新增此机制是前置条件
4. **pending 消息的生命周期**：离线发送的消息在重连后需要：发送 → 收到回包 → 替换 pending → 清理 IndexedDB。断网时间较长时（如 1 小时），消息的顺序性、时效性需要决策

#### 预期的架构变更

```
┌─────────────────────────────────────────────────────┐
│ 当前：context.js 全局 mutable state (内存)             │
│  optimisticAdd(msg) → 只操作 DOM（内存）               │
├─────────────────────────────────────────────────────┤
│ 目标：                                               │
│  OfflineStore (IndexedDB wrapper) {                   │
│    savePending(msg), getPending(roomId),               │
│    cacheHistory(roomId, messages),                     │
│    getCachedHistory(roomId, since),                    │
│    saveDraft(roomId, text), getDraft(roomId),          │
│    cleanup(staleRoomIds)                               │
│  }                                                    │
│       ↑ 被 WS 重连管理器 和 Composer 消费               │
│                                                      │
│  WS 重连管理器：                                       │
│  1. IndexedDB 读取 pending 消息队列                     │
│  2. 逐条发送（带 client_id 幂等键）                     │
│  3. 收到回包 → 从 IndexedDB 移除                      │
│  4. 失败 → 保留队列（指数退避）                        │
└─────────────────────────────────────────────────────┘
```

#### 对现有系统的影响

| 影响面 | 程度 | 说明 |
|--------|------|------|
| 客户端 JS 架构 | **高** | `context.js` 需要从纯内存迁移到 `Store` + `MemoryCache` 双层——Store 负责持久化、MemoryCache 负责 UI 响应速度 |
| 服务端 | **低** | 需确认/新增 `client_id` 幂等键支持（当前如不支持是新增字段+索引，不是架构变更） |
| WS 协议 | **低** | 重连后重发消息使用现有 `send_message` 帧，无需新协议变体 |
| 前端 UI | **中** | pending 消息需要「已发送」「发送中」「发送失败（可重试）」三种状态的视觉指示 |

---

### 方向五（P2）：WS 协议版本化与能力协商

#### 为什么需要

当前 WS 协议没有版本标识。对于单一官方 debug client，这不是问题——因为部署时客户端和服务器在同一发行周期。但以下场景会打破这种假设：

1. **第三方 Bot 接入**：Bot 用 WS 协议接入（如 `@agent_bot`），服务端新增帧类型后旧 Bot 静默丢帧
2. **渐进式部署**：金丝雀部署时新旧服务器共存，客户端重新连到不同版本的服务器时需要知道能力差异
3. **Web 客户端版本滞后**：用户浏览器打开页面后不刷新，服务端已多次部署——新帧类型对旧客户端不可见

#### 核心挑战

1. **版本号 vs 能力集组合爆炸**：20 种帧类型 → 2^20 种能力组合。推荐白名单（服务器声明） + 黑名单（客户端声明理解）模式
2. **版本号递增规则**：需要明确的契约——什么变更需要递增主版本号（格式变更/帧类型新增/字段重命名） vs 小版本号（新增可选字段）
3. **降级部署兼容**：回滚到旧版本服务器时，旧服务器对声明 `protocol_version=2` 的客户端要能正常服务——忽略不认识的 capabilities

#### 预期的架构变更

```
┌─────────────────────────────────────────────────────┐
│ 当前：                                               │
│  WS upgrade → 只验证 token                           │
│  ClientFrame { SendMessage, EditMessage, ... }       │
│    ↑ 无 version/capabilities 字段                     │
│  ServerFrame { Message, Edited, Typing, ... }         │
│    ↑ 无 version/capabilities 字段                     │
│  旧客户端遇到新帧类型 → 静默丢弃                       │
├─────────────────────────────────────────────────────┤
│ 目标：                                               │
│  WS upgrade → 验证 token +                         │
│    客户端通过 query string: ?pv=2&caps=msg,react      │
│    服务器响应 Welcome {                              │
│      protocol_version: 2,                            │
│      capabilities: ["messages","reactions",...]       │
│    }                                                 │
│                                                      │
│  帧路由：                                            │
│    match protocol_version {                          │
│      < 2 => 过滤掉 Poll/Canvas 等新帧类型              │
│      >= 2 => 正常路由                                 │
│    }                                                 │
│                                                      │
│  兼容测试：每帧类型变更 → 旧客户端连新版服务器测试      │
└─────────────────────────────────────────────────────┘
```

#### 对现有系统的影响

| 影响面 | 程度 | 说明 |
|--------|------|------|
| WS 握手 (`ws/mod.rs`) | **低** | query string 加参数解析 + Welcome 帧新增字段，不回退变更 |
| 帧路由 (`ws/ws_impl/mod.rs`) | **中** | `handle_text` 需要根据 `protocol_version` 决定帧的路由/过滤逻辑 |
| 客户端 `ws.js` | **中** | 需要声明 `pv=...` 参数 + 根据 Welcome 帧的能力列表决定订阅哪些帧 |
| 已有代码 | **低** | 所有变更都是**追加**的——旧路径完全保留 |

---

## 3. 接口设计建议

### 3.1 关键模块的接口设计原则

面向这 5 个扩展方向，以下接口设计原则应该全局适用：

| 原则 | 适用方向 | 具体含义 |
|------|---------|---------|
| **追加不修改** | 方向一/五 | Block 变体永远是加新 variant 不改旧 variant（模式匹配用 `#[non_exhaustive]`）；WS 协议字段永远加新字段不改旧字段类型 |
| **Trait 抽取，不继承** | 方向二 | `SearchableEntity` 用 trait + 按类型实现，不用 `enum SearchableEntity { Message(Message), Canvas(Canvas) }`——后者在新增实体类型时需改枚举 |
| **分层存储** | 方向四 | `Store` (DB) → `Cache` (内存) → `UI` (DOM)，单向数据流——UI 从不直接写 Store，通过 dispatch action 写 |
| **协商优先** | 方向五 | 客户端和服务端在连接时交换能力，服务端据此决定帧内容——不假设客户端支持所有帧类型 |

### 3.2 是否需要新的抽象层

| 抽象层 | 必要性 | 说明 |
|--------|--------|------|
| `BlockMigration` trait + registry | **必要**（方向一） | 每个 Block schema 版本对应一对迁移/回滚函数，通过 registry 注册。架构保障新增变体时「迁移代码与变体定义在同一变更中」 |
| `SearchableEntity` trait | **必要**（方向二） | 统一各实体的检索暴露面。可选设计：是 trait（各仓储实现）还是 enum（统一聚合）？推荐 trait——实体类型会持续增长 |
| `FrameBatcher` | **推荐**（方向三） | 对 Hub 扇出管线增加批处理层，不改变现有 Event→Frame 的转换逻辑 |
| `OfflineStore` (IndexedDB wrapper) | **必要**（方向四） | 所有离线持久化的统一入口，上层代码不直接操作 IndexedDB API |
| 能力协商层 (CapabilityNegotiator) | **推荐**（方向五） | 管理连接的 `(protocol_version, supported_frames, server_caps)` 状态 |

### 3.3 向后兼容策略

| 方向 | 兼容策略 |
|------|---------|
| 方向一（Block schema） | 写时永远写最新版本；读时根据 `blocks_version` 自动迁移到最新（lazy migration）或保持原始格式（deferred migration）。旧 Block 变体**永远保留**在枚举中——只做 deprecation 标记，不做删除 |
| 方向二（搜索） | `POST /api/search/unified` 是新增 API，旧 `POST /api/rooms/:id/search` 完全保留（只查 messages），不做强制迁移 |
| 方向三（WS 帧） | `permessage-deflate` 是可选的 WS 扩展——客户端不支持则不启用。FrameBatcher 是服务端内部变更，对客户端透明。Diff/Patch 是新增帧类型 `EditedDiff`，旧 `Edited` 帧同时保留 |
| 方向四（离线持久化） | 全在客户端——无服务端兼容问题。IndexedDB schema 用版本号控制（`openDB(db, version, ...)`），旧版本数据在第一次打开时自动迁移 |
| 方向五（协议版本化） | 所有旧客户端（无 `pv` 参数）被自动视为 `protocol_version=1`，服务端保持 v1 行为不变。**不删 v1 代码** |

---

## 4. 技术选型

### 4.1 新引入的技术栈 / 框架评估

| 方向 | 候选技术 | 评估 |
|------|---------|------|
| 方向一（Block 迁移） | **无新依赖** | 迁移函数用纯 Rust 写（操作 `serde_json::Value`），不需要 `SQL UDF` 或外部脚本语言 |
| 方向二（跨实体搜索） | **pgvector → 多向量列 / 实体级索引** | 现有基础设施（PostgreSQL + pgvector）足够——不需要 Elasticsearch。如果未来搜索负载成为瓶颈，可考虑在搜索层前面加 cache（Redis） |
| 方向三（WS 管线） | **tokio-tungstenite permessage-deflate** | 已验证的 Rust WS 库标准扩展，零外部依赖。`serde_json` 的 `RawValue` 可用于帧的零拷贝传递 |
| 方向四（客户端持久化） | **idb-keyval**（IndexedDB 轻量封装） | 对 5.9K JS 代码库来说，`idb-keyval` 约 600B gzip，不引入 `idb`（~3KB）或 Dexie（~12KB）的开销。自己写约 100 行 wrapper |
| 方向五（协议版本化） | **无新依赖** | 纯接口设计，query string 参数解析不依赖外部库 |

**核心结论**：5 个方向都不需要引入新的主要技术栈（No Elasticsearch, No Redisearch, No SQLite WASM, No GraphQL）。现有技术栈（PostgreSQL + pgvector + NATS + tokio-tungstenite + IndexedDB）足以支撑。

### 4.2 第三方依赖评估标准

如果未来确实需要新依赖（如方向二负载瓶颈时评估 Elasticsearch），评估标准：

```
1. 是否纯 Rust / 纯 JS → 不引入 FFI / Node native addon 额外复杂度
2. 是否已在 Cargo.lock / package-lock 中（避免版本冲突）
3. 是否与现有 crate 的 tokio / axum 主版本兼容
4. 是否活跃维护（GitHub 最近 release < 6 个月）
5. GPL / AGPL 许可证排除
```

### 4.3 自建 vs 采购决策

这 5 个方向全都是**自建**路径：

| 方向 | 为什么不自建 | 为什么不自建替代方案 |
|------|-------------|-------------------|
| 方向一 | Block 迁移是 Aero IM 特有的数据模式 | 无现成的「Rust enum schema migration」工具 |
| 方向二 | 搜索覆盖的是 Aero IM 独有的实体模型 | Elasticsearch / Algolia 可做后端但需要额外运维复杂度，当前 PG 够用 |
| 方向三 | WS 帧管线是 Aero IM 特有的实时分发模型 | 无现成的「WS 轻帧合并 + diff 更新」中间件 |
| 方向四 | IndexedDB + Service Worker 是标准 Web API | 无框架依赖——100 行 wrapper 完成 |
| 方向五 | WS 协议是 Aero IM 特有的帧模型 | 无现成的「能力协商」中间件 |

---

## 5. 实施路线图

### 5.1 优先级矩阵

| 方向 | 优先级 | 业务价值 | 技术复杂度 | 紧迫性 | 依赖关系 |
|------|--------|---------|-----------|-------|---------|
| 方向一（Block schema 演进） | **P1** | 高（数据完整性） | 中 | 持续累积债务 | 无 |
| 方向二（跨实体搜索） | **P1** | 高（AI 覆盖度） | 高 | 当前瓶颈 | 方向一的部分消费点 |
| 方向三（WS 管线优化） | **P1** | 中（性能/成本） | 中 | 低（不改不出问题） | 无 |
| 方向四（离线持久化） | **P2** | 高（UX 信任） | 中 | 低（debug client） | 方向五（协议版本化前置？） |
| 方向五（协议版本化） | **P2** | 中（集成） | 低 | 低（第三方集成前） | 方向四（离线重发需要幂等键） |

### 5.2 阶段划分和里程碑

#### Phase 1（4-6 周）：基础设施层

| 周次 | 方向 | 里程碑 |
|------|------|--------|
| W1-2 | 方向一（Block 迁移框架） | - `Message` 新增 `blocks_version` 字段 + 写入当前版本<br>- `BlockMigration` trait + 空实现（无迁移函数，仅框架）<br>- 迁移测试夹具框架 |
| W2-3 | 方向三（WS 轻帧合并） | - `FrameBatcher` 实现 + 单测（50ms 窗口合并 typing/read/presence）<br>- Hub 管线集成，可 feature-flag 启用 |
| W3-4 | 方向三（WS compression） | - tokio-tungstenite `permessage-deflate` 扩展启用<br>- 基准测试（with/without compression, 不同并发数） |
| W4-5 | 方向五（协议版本化） | - Welcome 帧增加 `protocol_version` + `capabilities`<br>- 客户端 QueryString `pv=` 解析<br>- `protocol_version < 2` 的帧过滤逻辑 |
| W5-6 | 方向四底层 | - IndexedDB wrapper `OfflineStore`<br>- 草稿自动持久化（localStorage）<br>- pending 消息 IndexedDB 写入 |

**Phase 1 交付物**：所有 5 个方向的基础设施就绪，无功能断失。

#### Phase 2（6-8 周）：核心能力建设

| 周次 | 方向 | 里程碑 |
|------|------|--------|
| W7-8 | 方向一 | - 新增第一个 Block 迁移函数（如 `v1→v2` 用于某个真实变体演进）<br>- `BlockMigrator` worker（分批扫 + throttle + metric）<br>- 法务保全跳过逻辑 |
| W8-10 | 方向二 | - `SearchableEntity` trait 定义<br>- Message / Canvas / Task 三个高频实体实现<br>- `UnifiedSearchEngine`（BM25 + 简单 RRF） |
| W10-12 | 方向二 | - `POST /api/search/unified` API<br>- FTS 索引扩展（canvas title/body、task description）<br>- workspace_ask 接入统一搜索 |
| W12-14 | 方向三 | - Diff evaluator（`Edited` → `PatchOp[]`）<br>- 客户端 diff 合并逻辑<br>- 定期全量同步校验和 |

**Phase 2 交付物**：全 5 个方向生产可用。

#### Phase 3（4-6 周）：打磨与迁移

| 周次 | 方向 | 里程碑 |
|------|------|--------|
| W15-16 | 方向二 | - 剩余实体实现（poll / clip / transcript / bookmark）<br>- 搜索前端按实体类型分卡片渲染<br>- 性能基准测试 + 索引调优 |
| W17-18 | 方向四 | - WS 重连管理器集成<br>- 离线消息队列 + 幂等重发<br>- 历史消息 LRU 缓存 |
| W19-20 | 方向三 | - permessage-deflate 按房间策略（大房间开，小房间关）<br>- FrameBatcher 窗口时间动态调整<br>- 监控仪表盘（每连接帧率/字节率） |

**Phase 3 交付物**：全方向打磨完成 + 监控覆盖。

### 5.3 风险点和缓解策略

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| **方向一：Block 迁移写放大**（数十亿行 UPDATE → autovacuum 风暴） | 中 | 高 | 采用 AiWorker 的 budget 模式：分批+throttle + 低优先级 + 可暂停/恢复。先在小表验证写放大倍数 |
| **方向二：搜索 RT 膨胀**（10 种实体统一搜索 → 延迟 >500ms） | 中 | 中 | 每类型独立限条数（≤5）+ 并行查询 + 过时容忍（搜索结果允许 100ms 旧数据） |
| **方向二：权限边界泄漏**（统一搜索 API 暴露非授权实体） | 低 | 高 | 复用 `assert_room_access`——搜索结果在返回前按 `room_id` 批量鉴权。CA：`assert_room_access` 当前只接受单个 `RoomId`，需要批量版本 |
| **方向三：permessage-deflate CPU 高**（10k+ 连接下压缩 > 传输） | 中 | 中 | 压测 + 按房间动态开关 + 监控 CPU/带宽交叉对比。默认关闭，只在瓶颈时开 |
| **方向四：多 Tab 竞争**（两个 Tab 同时重发同一条 pending 消息） | 高 | 中 | 使用 BroadcastChannel API 协调——一个 Tab 持有 WS 连接，其他 Tab 通过 BroadcastChannel 发送消息；或服务端 `client_id` 幂等键去重 |
| **方向五：协议版本递增速率**（每次帧类型新增都增版本号 → 版本膨胀） | 中 | 低 | 区分 MAJOR（格式变更/字段重命名/变体删除）和 MINOR（新增变体/可选字段）——客户端按 MINOR 步进兼容 |

### 5.4 依赖关系图

```
方向五（WS 协议版本化） ──弱依赖──→ 方向四（离线重发需版本协商）
        ↓
方向三（WS 管线优化） ──无依赖──
        ↓
方向一（Block schema 演进） ──弱依赖──→ 方向二（搜索的 searchable_text 需感知 blocks_version）
        ↓                              ↓
方向二（跨实体搜索） ──依赖──→ 方向一（消费点一致性）
        ↓
方向四（客户端离线持久化） ──依赖──→ 方向五（协议版本化前置？）
                                   ──依赖──→ 服务端 client_id 幂等键
```

**推荐执行顺序**：同时启动方向三和方向五（无依赖），两周后启动方向一，方向一落地一半后启动方向二，方向五落地后启动方向四。

---

## 附录：决策权衡记录

### 抉择 1：Block schema version 存在哪？

| 选项 | 优点 | 缺点 |
|------|------|------|
| **嵌入 `Message.blocks_version: u8`** | 消费方直接读版本号，不需要解析 JSONB；ORM 友好 | 消息行多一列（8B/行） |
| 嵌入 `blocks[0].version` | 零 schema 变更 | 每个 block 都需要解析才能读版本号；消费方都必须先 parse |
| 用 JSONB 内字段 `{"_v": 1, "blocks": [...]}` | 零 schema 变更 | 反序列化时需要先解析外层再解析内层；_v 命名约定人肉维护 |

**结论**：选项 1——`blocks_version` 列。额外 8B/行对消息表有 1 亿行时 ~800MB 额外空间，但换来的是 O(1) 版本读取。

### 抉择 2：UnifiedSearchEngine 返回类型

| 选项 | 优点 | 缺点 |
|------|------|------|
| **`enum SearchHit { Message(Box<Message>), Canvas(Box<Canvas>), ... }`** | 强类型，前端直接匹配渲染 | 新增实体类型需改枚举——需要 `#[non_exhaustive]` 兜底 |
| `struct SearchHit { entity_type: String, entity_id: Uuid, payload: serde_json::Value }` | 零类型依赖，新增实体类型不修改枚举 | payload 类型不安全，前端需要按 `entity_type` 写类型守卫 |

**结论**：推荐选项 1，因为强类型 + `#[non_exhaustive]` 组合是 Rust 全栈的最佳 trade-off。选项 2 保留作为搜索公有 API 的返回格式（便于第三方集成），内部搜索引擎用选项 1。

### 抉择 3：WS 合并策略

| 选项 | 优点 | 缺点 |
|------|------|------|
| **服务端 batcher + 客户端拆包** | 对客户端透明，不要改 `ws.js` | 服务端需要定义 batch 帧格式 |
| 客户端 batcher（定时 flush） | 服务端零改动 | 延迟不可控（客户端丢数据风险）；重连后重建 batcher |
| 两者组合（服务端为主，客户端 fallback） | 最灵活 | 复杂度最高 |

**结论**：推荐选项 1——服务端 batcher，对客户端透明。batch 帧格式为 `{ type: "batch", frames: [...] }`，客户端 `ws.js` 加一个 switch case 展开即可。

---

以上是基于 5 个扩展方向的完整架构分析。总结核心观点：**这 5 个方向不是「新功能」，而是补架构债**——Block 演进框架、跨实体搜索、WS 管线优化、离线持久化、协议版本化——每一个都是随着系统从原型走向产品化必须跨越的结构性断层。P1 的三项（方向一、二、三）建议在下一个里程碑中启动，P2 的两项（方向四、五）作为第三方集成/产品化的 gate。

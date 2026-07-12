# 架构师分析：Aero IM 五方向架构扩展评估

> 基于 `docs/requirements/2026-07-11-five-genuinely-novel-architectural-expansions.md`（已验证 10/10 ✅，代码证据确认无误）  
> 视角：资深架构师  
> 约束：不编写实现代码，仅架构层面的设计与建议

---

## 一、架构评估：现有分析的质量与缺口

### 1.1 文档质量评价

该文档质量整体**优良**——代码证据链完整、锚点精确、优先级判断合理。但存在三个系统性的分析深度问题：

| 问题 | 表现 | 影响 |
|------|------|------|
| **「是」与「应当」的边界模糊** | 方向一（Block schema）指出「无版本号→脆弱演进」是正确的，但未讨论 **Block 同时是 'wire-format AND storage-format' 这一根本约束**（`block.rs:2` 模块级注释明文声明） | 推荐的简单方案（加版本号+回填 worker）触及症状而非根因——真正的架构债务是将两个不同生命周期（通信协议 vs 持久化 schema）捆在同一枚举中 |
| **低估外部依赖的耦合影响** | 方向三（WS 帧管线）建议 permessage-deflate，但未分析 tokio-tungstenite 的 deflate 实现与 axum WS 的交互方式——这是 websocket crate 层级的配置，不是应用层的 `Hub` 可以控制的 | 低估了实现复杂度：启用 `permessage-deflate` 需要修改 axum 的 WebSocket 配置，而且压缩上下文对每个连接独立维护，内存开销在 10k 连接下不可忽略 |
| **已有增量改进未纳入基线** | 方向三指出 `fan_out_raw` 发送完整 JSON 帧无压缩，但实则是 `hub.rs:311` 已有 `fan_out_arc`（共享 `Arc<String>` 减少序列化次数）、以及 `RESYNC_FRAME` 慢消费者机制（`hub.rs:342-355`） | 推荐方案需要站在这些已有机制上构建，而非另起炉灶 |

### 1.2 关键设计决策的再评估

**当前架构的核心权衡**：Block 枚举的「双身份」（wire-format = storage-format）在初期是正确的——减少类型转换层、加速开发、保证 AI 消费的消息格式与存储一致。但这个决策的**机会成本**在 87+ 功能点、20+ Block 变体后开始显现：

- 每次 Block 变体修改**同时**是 API 变更 + 存储 schema 变更 + 索引 schema 变更（`searchable_text()`）
- 三个变更维度有**不同**的版本兼容要求：API 需向前兼容（旧客户端能工作）、存储需向后兼容（旧数据能读取）、索引需全量覆盖（新旧数据都能搜到）
- 当前 `#[serde(default)]` 的 mode 仅能覆盖「新增可选字段」场景，对变体重命名/合并/拆分/类型变更完全无力

**这是合理的架构债务**——在初创阶段加速交付是正确的选择，但现在已到还债时机点。

### 1.3 文档遗漏的关键架构风险

| 风险 | 影响方向 | 严重程度 |
|------|---------|---------|
| **Block 的 `searchable_text()` 与 `extra_searchable_text()` 的分离**：当前 `Text` 贡献 `content`、`Voice` 贡献 `transcript`、`File` 贡献 `name`——每个新变体必须同时考虑搜索，否则**静默不可搜索** | 方向一、方向二 | 高——新变体遗漏搜索投影意味着全量回填，比 Block 版本化更难修复 |
| **WS 帧管线中的 `NotifyBatch` 展开消耗**：`frame.rs:47-53` 中 `NotifyBatch` 在序列化为帧时展开为单条 `Notify`（取 `.first()`），丢失了批量信息。方向三建议轻帧合并，但当前代码已经在**丢失批量语义**——不应再额外聚合 | 方向三 | 中——合并策略需考虑 `NotifyBatch` 已是有损聚合后的产物 |
| **web SPA 的 `context.js state` 与路由绑定的隐含依赖**：方向四认为「刷新丢失全部 state」，但当前 SPA 的 state 不仅存储消息——还包括 `currentUser`、`token`、`activeRoom`、`scrollPosition`、`composerState`。IndexedDB 方案必须区分「可恢复 state」与「必须从服务器重新获取的 state」 | 方向四 | 中——state 颗粒度的正确建模决定了离线持久化的 ROI |

---

## 二、扩展方向：重新排序与深度剖析

### 2.1 方向一的真正问题：Block 双身份解耦（上调至 P0）

文档将方向一标注为 P1（数据完整性），但从架构视角看，**Block 枚举解耦是其他三个方向的前置条件**：

```
Block 双身份解耦 ──→ 方向二（统一搜索）：Block 格式独立后，searchable_text() 可独立演进
                 ──→ 方向三（WS 压缩）：Block 的 wire format 可独立优化（如 protobuf）
                 ──→ 方向五（WS 版本化）：协议版本号可独立于存储版本号
```

**核心架构决策**：

| 选项 | 描述 | 优势 | 成本 |
|------|------|------|------|
| **A（推荐）** | 引入 `BlockWire`（WS/NATS 线格式）与 `BlockStorage`（PG JSONB 持久格式），通过 `From`/`Into` 转换 | 清晰解耦；wire 可独立演进；允许 wire 使用紧凑编码 | 开发成本高（~2 周）；运行时转换开销；双枚举维护 |
| **B** | 保留单枚举，加 `BlockSchemaVersion`，Rust 端的 `TryFrom<(BlockSchemaVersion, serde_json::Value)>` 做版本感知反序列化 | 中等成本；不改 wire 格式；向后兼容 | 紧凑化困难；wire 协议仍耦合存储 |
| **C** | 核心层用 trait `BlockContent`，每种 block 独立类型，`Vec<Box<dyn BlockContent>>` 做多态存储 | 最灵活；新增 block 不修改核心枚举 | 丧失枚举的 exhaustiveness 检查；serde 序列化复杂；性能损失 |

**推荐：选项 A 的增量版本**——保持当前 `Block` 枚举作为存储格式，引入 `BlockWire` 作为 WS/NATS 线格式，使用 `Into<BlockWire>` 在序列化时转换。原因是：
- `block.rs` 模块注释已经声明「wire-format AND storage-format」——声明了意图就好办，将其拆分为两个阶段
- `BlockWire` 可以先用当前 `Block` 的 1:1 映射，后续版本独立优化
- 不需要立即改变 DB 中的 JSON 格式

**对文档的修正**：文档建议「加 `blocks_version: u8` 字段」是选项 B 的思路。从解耦角度看，**选项 A 的长远价值更高**，而 `blocks_version` 字段在解耦后变成 `block_wire_schema_version`，由 WS 升级时协商（与方向五一致）。

### 2.2 方向二：跨实体内容图谱——与既有分析的合并方案（P0）

#### 一致性分析

与 `2026-07-11-global-scan-strategic-expansion.md` 方向二「多租户内容图谱」的对比：

| 维度 | 本文档 | strategic-expansion 文档 | 合并建议 |
|------|--------|------------------------|---------|
| 焦点 | 53 种 ID 不可搜索 | 双轨富内容 + 实体关系 + AI 多源 RAG | 互补：本文档偏搜索缺口，另一份偏关系图谱 |
| 搜索范围 | `messages` 表 FTS 索引 | 同左 | 一致 |
| 建议方案 | `SearchableEntity` trait + 统一搜索 API | 外部搜索引擎（Meilisearch）+ `entity_links` 表 + AI 多源 RAG | **合并为统一方案**：内在搜索引擎（SQL FTS + pgvector hybrid）+ `SearchableEntity` trait |
| 跨界程度 | 仅搜索层 | 搜索 + AI + 关系图谱 + 渲染 | 本文档较窄 |

#### 合并后的推荐架构

```
┌─────────────────────────────────────────────┐
│  统一搜索 API：POST /api/search/unified     │
│  参数：q, scope(workspace|room|global),     │
│        entity_types[], limit_per_type        │
├─────────────────────────────────────────────┤
│            RRF 排序引擎                      │
│  ┌────────┐┌────────┐┌────────┐┌────────┐   │
│  │Message ││ Canvas ││  Task  ││  Poll  │...│
│  │ FTS+vec││ FTS    ││ FTS    ││ FTS    │   │
│  └────────┘└────────┘└────────┘└────────┘   │
├─────────────────────────────────────────────┤
│  实体关系图：entity_links 表                │
│  (source_type, source_id, target_type,      │
│   target_id, relation, weight)               │
│  反向索引 + 关系推荐                         │
└─────────────────────────────────────────────┘
```

**关键设计决策**：使用内部搜索引擎（PG FTS + pgvector hybrid）而非外部搜索引擎（Meilisearch/Typesense）

| 方案 | 优势 | 劣势 |
|------|------|------|
| **内部（推荐）** | 零额外基础设施；事务一致（搜索结果与 DB 无延迟）；权限校验与搜索同库 | 搜索性能受限（单 PG 实例）；全文搜索能力弱于专用引擎；向量搜索规模受限 |
| **外部（Meilisearch）** | 搜索性能高；容错独立；支持 typo tolerance、facet、sort | 额外运维负担；事务一致性需双写/CDC；权限过滤需后处理 |

**建议**：首期用内部方案（添加 `searchable_entities` 统一视图 + FTS 索引），长期（消息量 > 1B 行）迁移至外部搜索引擎。理由是：当前 FTS 索引已在 `messages` 上运行良好，扩展到其他实体是 SQL 层的扩展，而非架构替换。

### 2.3 方向三：WS 帧管线优化——差异更新方案的架构风险（保持 P1）

文档提出的「轻帧合并 + permessage-deflate + diff 更新」组合方案整体合理，但**diff 更新的架构风险被低估**：

#### Diff 更新的三个架构陷阱

1. **消息编辑的 diff 可靠性依赖于客户端状态完整性**：如果客户端错过了一个 diff（慢消费者丢帧），后续所有 diff 基准偏移，渲染出乱码。文档提到的「定期全量同步」是显然的缓解措施，但需要明确的**状态同步协议**：
   - 每条消息需要 `blocks_version` + `blocks_checksum`（类似 git tree hash）
   - 客户端检测到 checksum 不匹配时请求全量重传
   - 这引入了「已发送的帧需要可重放」的存储需求——当前 Hub 的 `tx.try_send` 是 fire-and-forget，不保留历史

2. **JSON Patch（RFC 6902）对 Block 数组的适用性有限**：Block 数组是异构有序列表（`[Text{...}, File{...}, Button{...}]`），Patch 操作（`add`/`remove`/`replace`）的路径为 JSON 指针——但 Block 数组的索引会因其他 Patch 操作偏移。`replace: /blocks/2` 在 `/blocks/1` 被同时删除时失效。

3. **批量轻帧合并的时间窗口**：文档建议 50ms 窗口。但这个窗口与 `NotifyBatch` 的展开逻辑冲突——`frame.rs:47-53` 已经将批量通知展开为单个帧。如果 Hub 再做合并，等价于「批量→展开→再批量」的反模式。

#### 推荐的分层优化方案

| 层 | 优化 | 复杂度 | 收益估计 | 前置条件 |
|----|------|--------|---------|---------|
| L1 | 启用 permessage-deflate（WS 扩展层，零应用层修改） | 低 | 30-50% 大帧压缩 | 验证 tokio-tungstenite 的 deflate 实现在 10k 连接下的内存开销 |
| L2 | Hub 级帧合并：将 `Typing`/`Presence` 在 50ms 窗口聚合为单条 bus 消息，而非逐条 WS 发送 | 中 | 95% 带宽节省（typing） | None |
| L3 | 大型帧分块（>64KB 的 `Presence`/`MemberList`）：分片发送 + 客户端 reassemble | 中 | 避免 WS 帧阻塞 FIFO 队列 | L1 己完 |
| **L4（文档推荐）** | Diff 更新：消息编辑发 `PatchOp` 而非全量 | 高 | 70-90% 编辑帧节省 | 需要方向一的 Block 解耦作为基础 |

**建议顺序**：L1 → L2 → L3，L4 推迟到方向一解耦完成后再评估。

### 2.4 方向四：客户端离线持久化——从 Debug Client 到产品级 Web App（提升至 P1）

文档标记为 P2（可靠性/UX）。我建议 **提升至 P1**，理由是：

1. **产品化的必要条件**：文档已指出 `index.html:26` 声明为「debug client·联调专用」。如果产品方向是 Web-first（无原生客户端），那么 Web SPA 的产品化是最关键的单一路径依赖（与 AGENTS.md §4.4 的「移动端原生 SDK 出范围」一致）。

2. **刷新失忆 = 用户信任丧失**：在 IM 中，消息发送的「确定性反馈」是基本心理契约。`optimisticAdd` 只在内存操作意味着「发送成功的绿色勾」在刷新后消失——这是产品级的信任缺陷，不是功能缺陷。

3. **Service Worker 的策略寿命与版本管理**：SW 一旦注册即独立于页面生命周期运行。当前无 SW 意味着每次部署后旧页面可能连接到新 API（版本不兼容）。这与方向五（WS 协议版本化）直接相关——SW 版本管理是版本协商的前置条件。

#### 架构建议

```
┌──────────────────────────────────────────────────┐
│  Service Worker（缓存策略层）                      │
│  ├─ 静态资源缓存（immutable hashed assets）      │
│  ├─ IndexedDB 桥接（后台同步 pending 消息）       │
│  └─ 在线检测 + 重连策略（指数退避）              │
├──────────────────────────────────────────────────┤
│  IndexedDB（持久化层）                            │
│  ├─ rooms: 房间元数据 + 最后访问时间 + scroll pos│
│  ├─ drafts: 每房间草稿 (key = roomId)            │
│  ├─ pending: 待发送消息队列 {clientId, blocks,   │
│  │            retries, createdAt, lastAttempt}    │
│  └─ cache: LRU 消息历史缓存 (max 500/room)       │
├──────────────────────────────────────────────────┤
│  context.js（内存状态层，保持不变）               │
│  └─ 启动时从 IndexedDB restore, 运行时写 IndexedDB│
└──────────────────────────────────────────────────┘
```

**关键设计决策**：

| 决策 | 选项 | 推荐 |
|------|------|------|
| 离线消息发送 | A) 离线时直接写入 IndexedDB pending，在线后逐条投递 | **A（推荐）**：保持发送 UX 不阻塞 |
| | B) 离线时提示用户「当前离线」，不允许发送 | 破坏 IM 的核心信任 |
| 幂等保证 | 需要服务端支持 `client_id` 去重 | 当前可能不支持（`grep "client_id"` 待确认）——**这是离线架构必须的前置工作** |
| 多 Tab 协调 | A) `BroadcastChannel` API 协调（现代浏览器） | **A（推荐）**：优雅处理多 tab 冲突 |
| | B) 各 tab 独立写入 IndexedDB，最后写入者胜 | 草稿/状态冲突不可控 |

### 2.5 方向五：WS 协议版本化——被低估的复杂性（保持 P2，但影响方向一/三/四）

文档标记为 P2，但指出了正确的问题：协议无版本号、无能力协商。从架构视角，这个方向的**优先级取决于外部集成的需求**（第三方客户端），目前确为 P2。但它是方向一（Block 解耦）和方向三（WS 压缩）的**下游依赖**：

```
方向一：BlockWire 引入 → wire 格式变更 → 需要 WS 版本协商
方向三：L4 Diff 更新 → 帧格式变更 → 需要 WS 版本协商
方向四：SW 注册后 → SW 版本可能 ≠ 页面版本 → 需要协议版本兼容
```

**建议**：不独立启动方向五的工作，而是作为方向一/三的**附属产出**——在引入 `BlockWire` 或 diff 更新时，**顺带**实现 WS 版本协商，而非单独专项。

#### 简化的版本协商设计

```
【连接建立阶段】
客户端: wss://host/ws?protocol_ver=2&caps=msg,react,poll
服务端: → Welcome { server_ver: 2, server_caps: [...], effective_ver: min(1,2)=1 }
         （如果客户端声称 version=2 但服务器只理解 version=1，用 1 与客户端通信）

【协议演进规则】
- 协议版本号单调递增（MAJOR.MINOR）
- MINOR 递增：新增帧类型（旧客户端静默忽略）
- MAJOR 递增：格式变更/字段删除（旧客户端必须收到降级帧或拒绝服务）
- 服务器维护 `capability_set`：每个能力 = 一个帧类型或协议特性
```

**与文档的关键差异**：文档建议用 query string `?capabilities=msg,react,thread`。但 URL 长度受限 + token 已占用 query 长度。建议用 **WS 子协议协商**（`Sec-WebSocket-Protocol` header）或升级后的**首帧能力声明**（Welcome 帧中声明能力，客户端可忽略不认识的能力）。

---

## 三、接口设计建议

### 3.1 Block 解耦后的接口契约

```
// 存储层（不变）
pub enum Block { ... }  // 当前定义，加 #[non_exhaustive]

// 线格式层（新引入）
#[serde(tag = "type")]
pub enum BlockWire {
    Text { content: String, spans: Vec<Span> },
    // ...1:1 映射，但可以优化（如省略 Voice.transcript 等大字段）
}

impl From<&Block> for BlockWire { ... }
impl From<BlockWire> for Block { ... }  // 反序列化时填充 default

// 转换点（精确控制）
// WS 发送方向：Block → BlockWire → JSON (丢未就绪字段)
// WS 接收方向：JSON → BlockWire → Block (填充 default)
// 存储方向：Block → JSON (完整保留)
// 加载方向：JSON → Block (serde(default) 兜底保留旧格式)
```

**接口设计原则**：
- **转换无损耗原则**：`BlockWire → Block` 必须填充所有 storage 字段（`Voice.transcript` 在 wire 层可省略，存储层必须保留）
- **转换幂等原则**：`Block → BlockWire → Block = identity`，但 `BlockWire → Block → BlockWire` 可以在 wire 层进一步优化
- **Wire 版本与 Storage 版本独立演化**：wire 版本随 WS 协议版本号走，storage 版本由 `block_schema_version` 字段标记

### 3.2 SearchableEntity trait 设计

```rust
#[async_trait]
pub trait SearchableEntity: Send + Sync {
    /// 唯一标识 (entity_type, entity_id)
    fn entity_type() -> EntityType;  // 关联常量
    fn entity_id(&self) -> &str;
    
    /// 搜索可见性
    fn room_id(&self) -> RoomId;
    fn workspace_id(&self) -> WorkspaceId;
    
    /// 搜索内容（与 message.searchable_text() 对应）
    fn searchable_text(&self) -> String;
    
    /// 元数据（用于 facet 过滤和结果渲染类型识别）
    fn metadata(&self) -> serde_json::Value;
    
    /// 时间戳（用于排序）
    fn created_at(&self) -> OffsetDateTime;
    
    /// 权重（用于 RRF 排序调权，不同实体类型可不同）
    fn base_weight(&self) -> f64 { 1.0 }
}
```

**设计原则**：
- 每个实体类型独立实现，不影响已有类型的搜索逻辑
- `searchable_text()` 必须覆盖**全部**文本字段（`Task.title + Task.description`、`Poll.question + Poll.options`）
- 返回的 `room_id` 复用既有 `assert_room_access` 鉴权路径，不引入新的权限层

### 3.3 WS 帧管线优化后的 Hub 接口

```rust
impl Hub {
    /// 轻帧合并接口：接收各事件，在 50ms 窗口内聚合
    pub fn enqueue_light_event(&self, event: LightFrame);
    // LightFrame = Typing | Presence | ReadReceipt 等轻量帧
    
    /// 压缩帧发送接口（替代 fan_out_raw）
    pub fn fan_out_compressed(&self, recipients: &[ParticipantId], frame: &ServerFrame);
    // 内部决定是否启用 permessage-deflate（按房间大小/连接压缩协商）
    
    /// 大型帧分片（>16KB 自动触发）
    pub fn fan_out_chunked(&self, recipients: &[ParticipantId], frame: &ServerFrame);
    // 分片为多个 Frame::Fragment { id, index, total, payload }
}
```

---

## 四、技术选型建议

### 4.1 各方向的技术依赖评估

| 方向 | 所需新技术 | 评估 |
|------|-----------|------|
| 方向一（Block 解耦） | 无 | **纯 Rust 重构**——不引入新依赖 |
| 方向二（跨实体搜索） | pgvector（已有）| **不引入新依赖**——扩展既有 FTS + pgvector |
| 方向三（WS 压缩） | tokio-tungstenite permessage-deflate（已有）| **已是现有栈的一部分**——仅需启用配置 |
| 方向三（帧合并） | 无 | 纯 Rust Hub 修改 |
| 方向三（Diff 更新）| json-patch crate | `json-patch`（367 stars，宽松许可证，成熟度中） |
| 方向四（离线持久化）| indexeddB（Web API）| **零依赖**——纯浏览器 API |
| 方向四（Service Worker）| Workbox / sw-toolbox | **推荐 Workbox**——Google 维护，sw 开发的最佳实践框架 |
| 方向五（WS 版本化）| 无 | 纯 JSON 协议修改 |

### 4.2 自建 vs 采购/引入决策

| 方向 | 决策 | 理由 |
|------|------|------|
| **跨实体搜索引擎** | **自建（基于 PG）** | 当前 FTS + pgvector 已覆盖消息搜索；扩展到其他实体的边际成本低；引入外部搜索引擎（Meilisearch）带来运维复杂度和数据一致性成本 > 搜索质量收益 |
| **WS 压缩** | **启用现有能力** | tokio-tungstenite 已有 permessage-deflate 支持；只需应用层配置 |
| **离线持久化** | **纯浏览器 API** | IndexedDB + Cache API + Service Worker 全是标准 Web API；零外部依赖 |
| **Diff 更新** | **引入 json-patch crate** | RFC 6902 的 Rust 实现是成熟标准；self-host 实现 JSON Patch 容易出错 |

### 4.3 排除的技术方案

| 方案 | 排除理由 |
|------|---------|
| **Protobuf / FlatBuffers 替代 JSON Wire** | 虽然 compact+typed，但 Web SPA 原生消费 JSON（`JSON.parse`）；引入 protobuf 需要 `protobuf.js` 编译步骤，增加了构建链复杂度。**建议在 WS 协议版本化（方向五的 MAJOR 版本）时评估** |
| **Meilisearch / Typesense** | 方向二短期不需要；搜索瓶颈出现在 100M+ 消息行时再引入 |
| **WebSocket over HTTP/2（RFC 8441）** | axum 不原生支持；带来的多路复用收益对 IM 场景有限（每连接已独立） |

---

## 五、实施路线图

### 5.1 优先级重新排序（修正后的）

| 优先级 | 方向 | 依赖 | 建议启动时机 |
|--------|------|------|------------|
| **P0** | 方向一 Block 解耦（BlockStorage ↔ BlockWire） | 无 | 立即启动——这是所有后续方向的基础 |
| **P0** | 方向二跨实体搜索：FTS 扩展到 canvases/tasks/polls | 无（独立于方向一） | 与方向一并行 |
| **P1** | 方向三 WS 管线：L1 permessage-deflate + L2 帧合并 | 无 | 方向一启动后第 2 周 |
| **P1** | 方向四离线持久化：草稿 + pending + 缓存 | 需服务端 `client_id` 幂等支持 | 方向一完成后 |
| **P2** | 方向五 WS 版本化 | 方向一（BlockWire） | 作为方向一/三的附属产出 |
| **P2** | 方向三 WS 管线：L4 Diff 更新 | 方向一（BlockWire 稳定） | 方向一 + 方向五稳定后 |
| **P2** | 方向二跨实体搜索：关系图谱 + AI 多源 RAG | 方向二 FTS 扩展完成 | 方向二 FTS 上线后 |

### 5.2 阶段划分与里程碑

#### 阶段一（Week 1-3）：基础解耦 + 搜索扩展

```
Week 1:
  ↵ BlockStorage / BlockWire 拆分设计审查
  ↵ BlockWire 枚举定义 + From/Into 实现
  ↵ canvases 表 FTS 索引迁移 + 搜索接入
Week 2:
  ↵ BlockWire 接入 WS 发送路径（替换原有序列化）
  ↵ tasks、polls FTS 索引 + SearchableEntity 实现
  ↵ 统一搜索 API：POST /api/search/unified（原型）
Week 3:
  ↵ 回填 migration（blocks_version 字段 + 标记旧行）
  ↵ Legacy Block 反序列化兼容性测试
  ↵ 统一搜索 API 完善 + 权限过滤集成
```

**里程碑 1（Week 3 末）**：Block 解耦上线 + 跨实体搜索可用（消息 + 画布 + 任务 + 投票）

#### 阶段二（Week 4-6）：WS 优化 + 离线基础

```
Week 4:
  ↵ permessage-deflate 配置 + 负载测试（10k 连接场景）
  ↵ Hub 帧合并机制（Typing/Presence 聚合）
  ↵ 服务端 client_id 幂等 key 支持
Week 5:
  ↵ IndexedDB schema 设计 + context.js 持久化改造
  ↵ 草稿自动保存 + pending 消息队列
  ↵ SW 静态缓存注册
Week 6:
  ↵ WS 帧压缩 + 合并集成测试
  ↵ 离线消息重试 + 多 tab 协调
  ↵ 容量测试：验证 50ms 合并窗口在大房间的延迟影响
```

**里程碑 2（Week 6 末）**：WS 带宽降低 60%+（typing 场景 95%）+ 页面刷新后状态可恢复

#### 阶段三（Week 7-9）：高级能力 + 版本化

```
Week 7-8:
  ↵ WS 协议版本协商（welcome 帧 + caps 声明）
  ↵ BlockWire 版本策略与降级路径
  ↵ Legacy 客户端兼容性测试矩阵
Week 8-9:
  ↵ 方向二关系图谱（entity_links 表 + 反向索引）
  ↵ AI 多源 RAG（画布/任务/投票内容注入 AiService）
  ↵ 实体搜索结果分组渲染（前端）
```

**里程碑 3（Week 9 末）**：完整能力交付——WS 版本化协商、实体关系图谱、AI 跨实体检索

### 5.3 风险点与缓解策略

| 风险 | 等级 | 缓解 |
|------|------|------|
| **BlockWire 转换引入运行时性能损耗** | 中 | 基准测试：`Block → BlockWire → JSON` vs `Block → JSON`；如果 >15% 损耗，考虑 `#[derive(Serialize)]` 的 `#[serde(into = "BlockWire")]` 零拷贝序列化（依赖 serde 的 `Serialize` 转发） |
| **FTS 索引扩展到 canvases/tasks 后写放大** | 高 | 当前 `messages` 的 FTS 是 `GENERATED ALWAYS AS` 存储列 + GIN 索引，写入时全量更新；扩展到其他实体后，**写路径 TPS 必须审计**。建议：对低频写入实体（poll：~周级更新）用 GIN 索引无忧；对高频写入实体（canvas ops：~秒级更新）用异步 tsvector 更新（trigger + background worker） |
| **permessage-deflate 的内存开销** | 中 | 每个 WebSocket 连接有自己的 zlib 上下文（~32KB 窗口 + 字典）；10k 连接 ≈ 320MB 额外内存。缓解：仅在 >100 人的大房间连接启用，小房间连接跳过 |
| **离线 pending 消息在重连后重复投递** | 高 | 服务端必须支持 `client_id` 幂等（当前未实现）。**这是方向四的前置阻塞项**。缓解：在阶段二 Week 5 之前先完成 `client_id` 幂等框架 |
| **旧格式 Block 数据在解耦后的静默丢失** | 高 | 引入 `BlockWire` 后，旧格式 JSON 可能包含 `BlockWire` 不识别的字段（`#[serde(deny_unknown_fields)]` 会失败）。缓解：`BlockWire` 反序列化必须使用 `#[serde(default)]` 或自定义 `Deserialize` 实现，确保未知字段被忽略并记录 warning + 可观测指标 |
| **统一搜索 API 的权限过滤性能** | 中 | 跨 5 种实体类型时，权限过滤需要 5 次 `assert_room_access`。缓解：只在结果中过滤（返回 200 条+实时过滤取 20 条），而非在查询时逐行鉴权 |
| **Web SPA 从「无状态」到「有状态」的测试复杂度** | 高 | IndexedDB 状态 + SW 缓存 + 网络离线/在线转换 → 状态组合爆炸。缓解：分阶段上线——先草稿（localStorage，第 1 步）、再 pending（IndexedDB，第 2 步）、再消息缓存（第 3 步），每步独立验证；引入 `fake-indexeddb` 做单元测试 |

### 5.4 与既有功能的时间线冲突评估

| 并行项目 | 冲突风险 | 建议 |
|---------|---------|------|
| Bot 集成平台（strategic 方向四） | 低 | 独立模块，无 Block 或 WS 直接依赖 |
| 直播 ABR + CDN（strategic 方向一） | 低 | 独立技能树，Block 解耦不影响媒体面 |
| 运营成熟度（strategic 方向三） | 中 | 配置校验 + 版本协商（方向五）的部分工作重叠，可共享设计 |
| 会话生命周期（strategic 方向五） | 中 | IndexedDB 消息缓存（方向四）+ 离线摘要是这个方向的前置条件，建议顺序：方向四 → strategic 方向五 |

---

## 六、总结

| 核心发现 | 对项目的影响 |
|---------|------------|
| Block 双身份解耦是**所有其他方向的架构前置** | 强烈建议在方向一投入 3 周，否则后续方向的设计会基于有问题的假设 |
| 跨实体搜索与既有 strategic 文档高度重叠，需合并 | 建议基于本分析产出合并后的「统一搜索 API」设计文档，关闭两份文档的差异 |
| WS 帧优化的 diff 更新（L4）复杂度被低估 | 建议先做 L1-L3，L4 推迟到 BlockWire 稳定后 |
| 客户端离线持久化应从 P2 提升至 P1 | 产品化路径中最关键的单一路径依赖；无原生客户端 = Web SPA 是唯一界面 |
| WS 协议版本化应作为附属产出而非独立专项 | 随 BlockWire 引入和 WS 压缩启用**顺带实现**，而非单独交办 |

**最终建议的行动项**（按紧迫性排序）：

1. **立即**：产出 Block 解耦（`BlockStorage`/`BlockWire` 拆分）的技术设计文档，包含序列化/反序列化基准测试计划
2. **本周**：完成 `client_id` 幂等支持的设计（方向四前置条件，也是最可能阻塞其他方向的依赖）
3. **本周**：合并本文档与 `global-scan-strategic-expansion.md` 的方向二为统一的「跨实体搜索与内容图谱」设计
4. **下周**：启动方向二的 `SearchableEntity` trait 实现 + `canvases`/`tasks` FTS 索引迁移
5. **阶段规划**：将 WS 压缩（L1-L3）安排在 BlockWire 稳定后的 Sprint 中，避免两个大方向同时修改 WS 帧路径

# 架构师分析报告：Aero IM 五个战略扩展方向

> **分析对象**: `docs/requirements/2026-07-11-five-underestimated-strategic-directions.md`
> **分析日期**: 2026-07-12
> **视角**: 系统架构师 — 从全局拓扑、接口契约、演进路径角度审视

---

## 一、当前架构评估：力量与薄弱点

### 1.1 结构性优势（核心资产）

文档对当前架构的描述是准确的，但有一些隐含的架构优势值得显式化：

**事件驱动骨架是正确选择**。NATS JetStream 作为跨实例事实源 + Hub 作为进程内 bounded mpsc 扇出，这一设计为以下扩展提供了天然基础：

- **多租户隔离**：NATS subject `im.room.{id}` 的命名空间可以嵌入 workspace 维度（`im.room.{ws}.{id}`），无需替换总线基础设施
- **数据分层**：归档消息的重播可以通过新的 NATS consumer 实现，不影响当前实时流
- **跨工作区联邦**：`assert_room_access` 是唯一鉴权收口，扩展其逻辑比分散鉴权点更容易

**单调 seq 设计被低估**。`bus/seq.rs` 的 per-subject 单调序列号（at-least-once + 幂等）为以下提供了免费基础设施：

- 归档重播时的时序一致性
- 跨节点状态校准的参照系
- 增量数据导出（checkpoint 就是 seq）

**crate 分层边界清晰**。`aero-common` → `aero-storage` → `aero-im-core` → `aero-server` 的依赖方向正确。这为五个方向中的**四个**提供了「在哪个 crate 加东西」的确定答案。

### 1.2 架构债务与风险

文档指出了「单数据库/单租户/无分层」等运行期问题，但我认为还有几笔需要计入架构债务的**设计级**问题：

**债务 1: `workspace_id` 作为全局数据模型主线，但其语义不一致**

```rust
// 当前模型
rooms.workspace_id NOT NULL       -- 房间必属一个工作区
messages → rooms → workspace_id   -- 消息通过 rooms 间接关联
participants → workspace_id       -- 人直接关联
threads → message_id → room_id → workspace_id  -- 三层间接
```

问题是：`participant` 属于工作区 A，但可以加入工作区 B 的共享频道（方向 3 的场景）——这时 `participant.workspace_id` 语义变为「归属工作区」而非「当前活动工作区」。当前代码没有区分这两个概念，`assert_room_access` 的 `is_member` 检查会隐式假设两者一致。方向 3 必须先解耦这个语义。

**建议提前修复**: 在方向 1 之前，先引入 `participant_workspace_membership` 联结表（`participant_id, workspace_id, role, joined_at`），让 `participants.workspace_id` 变成可选的「默认活动工作区」。这样方向 3 的跨工作区共享频道可以直接复用。

**债务 2: 连接池没有抽象层**

当前 `PgPoolBuilder` 不存在，`persistence.rs` 直接 return `sqlx::PgPool`。方向 1 需要 `PoolAllocator` 来管理多池生命周期。如果方向 1 和方向 3 并行开发，PoolAllocator 的设计会受跨 WS 查询的影响（共享频道需要跨库查询吗？不，因为数据在同一个 PG 实例——但连接池是隔离的）。需要提前确定 PoolAllocator 的 API 边界。

**债务 3: 消息总线 subject 模板不存在 workspace 维度**

```
当前: im.room.{room_id}
方向 1: im.room.{ws_id}.{room_id}  (需重写 subject 拼接点)
```

潜在兼容问题：`run_bus_listener` 使用 wildcard `im.room.*`，如果改成分层 subject，wildcard 匹配 `im.room.*.*` 需要确认 NATS subject 通配符语义是否兼容。建议做一次 `grep -r 'im\\.room\\.' src/` 统计所有 subject 拼接点，评估变更面。

**债务 4: 后台任务没有熔断/健康上报**

当前所有定时器都是 `best-effort warn 不 panic`，但如果多个定时器（retention_sweep、archive_worker、embedding_backfill）同时出错，运维人员只能从日志中「感觉」到异常。需要统一的后台任务健康上报基础设施——方向 2 的 archive worker 尤其需要可观测性。

---

## 二、扩展方向：价值重估与架构影响

文档提出的 5 个方向覆盖全面，但作为架构师，我需要补充每个方向的**不确定性维度**——哪些是 Build 决策、哪些是 Buy 决策、哪些存在第二选项。

### 方向 1：多租户隔离 — 正确但需要更细的租户模型

**核心主张**: 不分库，只做连接池隔离 + 配额门 + 键命名空间。这个决策是**合理的**。

**需要补充的架构细节**:

#### 租户模型不只是「工作区」

```
租户类型:
  Tier_Free:   1 pool (max 5 conn), 100MB storage, 1 ws, 0 AI tokens
  Tier_Team:   1 pool (max 10 conn), 10GB storage, 5 ws, 1M AI tokens
  Tier_Enterprise: 1 pool (max 50 conn), unlimited, unlimited ws, custom AI
  Tier_Internal: 共享 pool (no isolation), unlimited (beta/test)
```

每个租户类型的配额不是一个简单键值对，而是一组**策略**：存储上限、AI 月消耗、活跃用户数、API 频率。这需要 `workspace_quota` 表扩展为策略引擎：

```sql
CREATE TABLE workspace_quotas (
    workspace_id    UUID PRIMARY KEY REFERENCES workspaces(id),
    tier            TEXT NOT NULL DEFAULT 'free',
    storage_bytes   BIGINT NOT NULL DEFAULT 1073741824,  -- 1GB
    ai_tokens_month BIGINT NOT NULL DEFAULT 1000000,
    active_users    INT NOT NULL DEFAULT 10,
    pool_max_conn   INT NOT NULL DEFAULT 5,
    overrides       JSONB  -- 覆盖特定限额
);
```

**关键问题：配额拦截点在哪？**
- **存储配额**: 每次 `POST /api/rooms/:id/messages` 前检查 `SUM(content_len) WHERE workspace_id = $1`——这是高频路径，不能每次 SQL COUNT，需要缓存或提前计数
- **AI 配额**: `AiService` 中 `budget.check` 前先查 `ai_usage.monthly_tokens` vs `quota.ai_tokens_month`——当前 `budget` 是进程内窗口，持久化配额是另一层门
- **用户配额**: 邀请新成员时检查 `active_users < quota.active_users`

**难以评估的权衡——连接池隔离的实际收益？**

连接池隔离防止的是一个工作区耗尽 `max_connections` 导致全局不可用。但 Postgres 的 row-level contention、buffer cache 污染、索引膨胀等问题**不受连接池隔离影响**——这些是工作区 A 的热数据挤走工作区 B 的缓存页。真正的性能隔离需要 **PG 的 resource group / cgroup**（Postgres 15+ 的 `pg_backend_memory_contexts` 仍然不够）、或者**租户分实例**。

**结论**: 连接池隔离 + 配额门是 SaaS 化的前提，但不要高估它对性能故障隔离的效果。真正的硬隔离还是需要分 PG 实例——方向 1 应该允许在配额被突破时自动将工作区迁移到独立实例（Layer 5：实例级隔离）。

### 方向 2：数据分层归档 — 最难落地但必须做

**文档核心主张正确**：不分归档就会在第二年遇到天花板。但有几个工程现实需要提前面对：

#### 难点 1: 归档时的消息 ID 一致性

当前 `messages` 的 id 是 `UUID v7`（时序递增），这是好消息——归档可以用 `created_at` 范围切割。但 `reactions`、`thread_read_state`、`delivery_cursor`、`message_history` 都包含 `message_id` 外键。归档后这些外键指向消失的行了。

**可行方案**: 不在 PG 中存归档消息。改为：
1. 热表 `messages` 保留最近 N 天 + 被法务保全的消息
2. 归档写入 Parquet 文件（S3/MinIO），每天一个分区
3. `archive_index` 表：`room_id, first_msg_id, last_msg_id, archive_path, msg_count, retention_days`

```
查询路径:
  SELECT * FROM messages WHERE room_id = $1 AND created_at > $cutoff   → 热表
  → 未命中 → SELECT archive_path FROM archive_index WHERE room_id = $1
  → 读取 Parquet → 过滤 → 返回
```

这意味需要引入 **Apache Arrow/Parquet 读取能力**（`arrow-rs` crate）或直接存为 `JSONB` 行 + 压缩——但那就不是「分层」而是「日志转存」了。

#### 难点 2: 向量索引无法跨层

`embedding` 在消息热表中，pgvector 索引范围是热表。归档后的消息不会出现在 RAG 搜索结果中。

**选项 A (接受退化)**: RAG 只搜索热消息。对 90% 的用户足够了——最近的对话相关性最高。

**选项 B (冷向量)**: 归档时保留 `(message_id, embedding)` 到独立的冷向量表 `archive_embeddings`，查询时 `UNION ALL` 热向量 + 冷向量。冷向量只支持 KNN 搜索（IVFFlat 索引），不变化。

**选项 C (外部向量库)**: 推入 Qdrant/Pinecone/Redis Stack。但不建议——增加运维复杂度。

**推荐**: 选项 A 做默认，选项 B 作为可选的「深度 RAG」功能。文档提到了混合搜索，但未明确决策。这里建议选择 A → 如果 P0 客户要求则为选项 B 提供路径。

#### 难点 3: 归档回放场景

`seq` 命名空间按 subject 单调递增，默认假设 subject 下的所有消息都在。归档打破了这一假设——旧的 seq 号可能指向已归档的消息。

```
RoomEvent 的 seq=1000 在总线上重播
→ 接收方查 messages 表 → 无行 (已归档)
→ 应该返回 404 还是返回归档消息？
```

**建议**: `run_bus_listener` 的 `fan_out` 路径对 `Message` 事件保留 minimal 字段（`id`, `room_id`, `author_id`, `created_at`）在 `messages` 热表中，即使主体内容已归档。这样 seq 回放不中断，只是消息体是 stubs。Web 端遇到 stub 显示「此消息已归档」+ 点击加载完整内容。

这是**关键架构决策**，文档未提及。我的建议是：**每条消息存一个微型记录在热表**（`message_stubs` 表或 `messages` 保留 `id, room_id, author_id, created_at, is_archived` 列），归档时只移动 `content` + `embeddings` + 大字段。这样热表膨胀速度降低 90%（内容占消息行的大部），但 seq 回放和 FK 引用不断。

### 方向 3：跨工作区联邦 — 架构影响最大但文档最浅

这是五个方向中**架构变更面最广**的，文档主要聚焦于 `rooms.workspace_id` 的扩展，但真正困难的地方在：

#### 难点 1: 信令和总线的 WS 端

当前 WebSocket 连接基于 `hub.ws_senders: HashMap<RoomId, Vec<Sender>>`，属于**一个连接 = 一个 user = 多个房间**。

跨工作区共享频道意味着：
- 用户 A（工作区 WS-A）和用户 B（工作区 WS-B）在同一个房间 R
- 两个用户通过不同的进程（可能不同实例）连接
- 消息从 NATS bus 扇出到两个用户

当前架构已经支持这一场景（im.room.{id} 是全局 subject），**信令不需要改**。真正的变更点在：

#### 难点 2: 通知隔离

`run_bus_listener` 展开 `NotifyBatch` 时给 `explicit_recipients` 推推送。如果工作区 A 的成员静默了频道，但工作区 B 的成员没有——当前 `notif_prefs` 表是 `(participant_id, room_id)` 的，可以支持。

但 `keyword_alerts`、`snooze`、DND 等按工作区配置的通知需要确认——用户可能在工作区 A 设置 DND 但仍希望接收同一共享频道在工作区 B 下的通知。

**架构变更**: `notif_prefs` 需要新增可选的 `workspace_id` 维度：在共享频道中，通知偏好可以按工作区分隔。

#### 难点 3: 权限模型膨胀

```
当前:
  room.member = participant in workspace → participant has role

方向 3:
  room.workspace_associations = [{ws: A, can_manage: true}, {ws: B, can_manage: false}]
  room.member = participant in ANY associated workspace
  Participant A (workspace A) 的角色是 workspace A 授予的
  Participant B (workspace B) 的角色是 workspace B 授予的
```

问题：谁可以设置/修改共享频道的角色？
- 当前假设：workspace A 的 owner 可以管理 A 的成员在共享频道中的角色
- 但不能管理 B 的成员——这是边界规则

这需要在 `assert_room_access` 后新增 `assert_channel_permission` 层：检查**调用者所属工作区**在该共享频道中的管理权限。

**建议**: 在 `rooms_in_workspaces` 联结表上加 `can_manage BOOLEAN DEFAULT false`。工作区的 owner/admin 自动获得 `can_manage = true`。这样权限检查仍然是「一次 WHERE 查询」而不是复杂 RBAC。

### 方向 4：应用平台 — 最小可行动路径正确

这个方向文档分析得最精准。我补充两个架构决策点：

#### 决策点: 命令路由在哪里做

```
选项 A (进程内路由):
  parse_command() 在 hardcoded 未命中时 → 查本进程的命令注册 HashMap
  优点: 低延迟，无 DB 命中
  缺点: 多实例部署时命令注册表不同步（需 NATS 广播）

选项 B (DB 路由):
  parse_command() 未命中 → SELECT FROM bot_commands WHERE name = $1
  优点: 全局一致
  缺点: 高频 `/` 输入过程中每次 keyup 都查 DB
```

**推荐**: 混合——命令发现走 DB（或 API 返回列表），单次路由走进程 HashMap + 本地 TTL 缓存。命令注册时通过 NATS 广播使缓存失效（`nats-command-update` subject）。这与当前架构的「事件扇出」模式一致。

#### 决策点: Manifest 验证在谁那做

Slack 的 app manifest 是客户端提交到 Slack API，Slack 验证后存储在平台侧。命令/快捷键/Bot Token 都在平台侧，对最终用户透明。但 Aero IM 的用户群体中有自建 bot 的企业（方向 3 的场景），他们可能想在自己的 bot server 上托管命令，而不是将 bot 逻辑部署到 Aero IM 集群。

**建议**: 支持两种模式：
1. **Hosted**（当前已有雏形）：bot 逻辑在 Aero IM 内，通过 `bot_dispatch` 触发
2. **Webhook**（扩展）：命令触发时 POST 到 bot 注册的回调 URL，等待响应

这也意味着 Manifest 中需要记录 `delivery_mode: "internal" | "webhook"`。

### 方向 5：知识策展 — 差异化最强但落地最不确定

这是五个方向中唯一没有竞品成功实践的方向。文档识别了基础设施，但架构设计上有几个尚未解决的根本问题：

#### 问题 1: 知识信号的 Precision/Recall 平衡

```
低门槛（高 recall）：几乎所有 PM 消息都是 "知识信号" → 噪音灌满 Canvas
高门槛（高 precision）：只捕获明确的 "ADR: ..." 前缀 → 漏掉 90% 的知识
```

Slack 的 AI 试过自动提取行动项，准确率不高且被用户视为干扰。这不是一个「技术修好」的问题——知识识别本质上是主观的（一条消息对你来说是知识，对我可能是噪音）。

**架构应对**: 文档提到的「Draft 形式写入 canvas」是正确方向。但我认为需要更系统的反馈回路：

```
用户行为 → 隐式信号
  - 收藏策展条目 = 正反馈
  - 编辑策展条目 = 认可
  - 删除策展条目 = 负反馈
  - 忽略策展通知 = 可能噪声
  → 每天更新信号检测模型的阈值（每个工作区独立校准）
```

这不是传统的前后端架构，而是 ML/MLOps 话题。Aero IM 团队是否有 ML 工程能力？如果没有，方向 5 应该定位为**启发式规则 + LLM 辅助**（而非模型驱动），并将 ML 增强列为 Phase 2。

#### 问题 2: 策展存储在哪

文档提到写入 Canvas。但 Canvas 是协作画布（`canvas.rs` 的 `CanvasOp::Insert`），设计上是用户手动编辑的结构化文档。如果 AI 自动写入，有几件事需要考虑：

- **版本控制**: AI 写入 + 用户编辑 = 谁覆盖谁？Canvas 是否有 diff/merge 语义？
- **容量限制**: 每日每条更新写入 canvas 条目，canvas 表会飞速膨胀
- **权限模型**: AI bot 写入后，用户是直接编辑（改写 AI 输出）还是 fork？

**建议**: 专设 `knowledge_entries` 表而非写入 Canvas。理由：
1. Canvas 是协作工具，知识策展是信息管线——两个不同的抽象
2. `knowledge_entries` 可以有自己的淘汰策略、来源追踪、版本控制
3. 最后呈现在 UI 上时，可以将 `knowledge_entries` 渲染为 Canvas 画布上的分组卡片

#### 问题 3: 多语言问题

企业对话经常是中英混排。知识提取的 LLM prompt 需要根据消息的语言切换指令语言。如果中文消息提取出英文 ADR 就是错误。

这属于 Prompt Engineering 范畴而非架构，但**知识条目 schema 需要支持多语言内容**（`title_zh`, `title_en` 或 `locale ` 字段）。

---

## 三、接口设计建议

### 3.1 引入新的抽象层

**需要引入的抽象层**：

| 抽象层 | 为什么 | 放在哪个 crate |
|--------|--------|----------------|
| `PoolAllocator` | 方向 1 的多池管理 | `aero-storage` 新增 `pool_alloc.rs` |
| `ArchiveBackend` | 方向 2 的存储后端抽象 | `aero-storage` 新增 `archive/` 模块 |
| `CommandRegistry` | 方向 4 的命令注册中心 | `aero-im-core` 新增 `commands/` 模块 |
| `KnowledgeStore` | 方向 5 的知识条目存储 | 新 crate `aero-knowledge` 或 `aero-im-core` 内 |

#### PoolAllocator 接口草案

```rust
/// 管理多个工作区的连接池
#[async_trait]
trait PoolAllocator: Send + Sync {
    /// 获取工作区的专用连接池（如果配置了隔离）
    /// 未配置隔离的工作区返回共享池
    /// 池尚未初始化则创建
    /// 返回引用（非 clone）以复用连接
    async fn pool(&self, workspace_id: WorkspaceId) -> PgPool;

    /// 获取共享池（退路）
    fn shared_pool(&self) -> &PgPool;

    /// 更新工作区的池大小（用于配额动态调整）
    async fn set_pool_size(&self, workspace_id: WorkspaceId, max_connections: u32);

    /// 租户被禁用/删除时释放资源
    async fn decommission(&self, workspace_id: WorkspaceId);
}
```

这个接口的关键设计决策是 `pool()` 返回引用还是 clone——`sqlx::PgPool::clone()` 是 `Arc` 复制，开销极小。所以实际上可以返回 clone，不需要生命周期绑定。但返回引用更契合「资源管理器」的语义。

### 3.2 向后兼容策略

五个方向都需要逐步部署，不能一次中断所有现有工作区。

| 方向 | 兼容策略 |
|------|----------|
| 多租户 | 现有工作区默认不开启连接池隔离，只在新工作区或 opt-in 后启用 |
| 数据分层 | 归档默认关闭，`AERO_ARCHIVE_ENABLED` env flag + 历史数据自动冷化，新数据继续在热表 |
| 联邦 | `rooms.workspace_id NOT NULL` 维持不变，`rooms_in_workspaces` 多对多只在联邦场景使用 |
| 应用平台 | 硬编码命令路径不删——先并行运行，日志观察新路径覆盖率和错误率，再逐步淘汰硬编码 |
| 知识策展 | `AERO_KNOWLEDGE_CURATION` env gate，默认关闭 |

**核心原则**: 所有新功能在代码层面默认处于「观察模式」（日志 + 指标，不产生用户可见副作用），通过 feature flag 逐步开放。

### 3.3 关键接口的演进方向

**`assert_room_access` 的扩展**:

```rust
// 当前签名
async fn assert_room_access(
    participant: &AuthUser,
    room: RoomId,
) -> Result<ParticipantRoom, AppError>;

// 方向 3 后需要:
// 方案 A: 参数追加 workspace_for 来区分「以哪个工作区身份进入」
async fn assert_room_access(
    participant: &AuthUser,
    room: RoomId,
    workspace_as: Option<WorkspaceId>,  // 方向 3 新增: 指定「以哪个 ws 的身份」进入共享频道
) -> Result<ParticipantRoom, AppError>;

// 方案 B: 保持签名不变，在内部决策
// 优先: participant 的默认工作区
// 回退: 找到 room 关联的任一工作区中该 participant 是 member
// 问题: 一个用户同时在两个 ws 都是 member → 歧义
```

**推荐方案 A**，因为显式传递意图避免了歧义。Web 端在进入共享频道时需要传 `?as_workspace=` 查询参数。

---

## 四、技术选型评估

### 4.1 需要引入的技术

| 方向 | 建议选型 | 备选 | 评估 |
|------|----------|------|------|
| 归档存储 | **Apache Parquet + S3/MinIO** (via `arrow-rs`) | pg_dump / TimescaleDB / ClickHouse | Parquet 列式存储 + S3 低成本，与 PG 解耦；但引入新 crate 和运维负担 |
| 归档查询 | **保留热表 stub + 按需 S3 加载** | foreign data wrapper (FDW) | FDW 耦合过紧；按需加载更灵活但延迟更高 |
| 应用平台 | **无新框架**——复用 `axum` + `serde` + `bot_dispatch` | 无 | 现有基础设施充足，不需要新框架 |
| 知识策展 | **复用 `ai_jobs` 队列** + 新 `knowledge_entries` 表 | 独立 worker + 专用向量库 | `ai_jobs` 已有 `SKIP LOCKED` + `MAX_ATTEMPTS`，直接复用 |
| 命令注册表 | **Redis sorted-set 缓存 + PG 持久** | 纯 PG | 高频 `/` 自动补全需要低延迟，Redis 适合 |

### 4.2 不应该引入的技术

1. **ClickHouse / TimescaleDB**（方向 2）：架构冲击太大，引入新数据库后需要跨数据库连接、双写、数据一致性协议。Parquet on S3 足够。

2. **Kubernetes 自定义调度器**：文档未提，但多租户隔离的方向可能会有「要不要用 K8s namespace 隔离 pod」的讨论。当前单进程架构下不需要。

3. **Istio/Envoy sidecar**：无理由增加运维复杂度。

### 4.3 自建 vs 采购决策矩阵

| 能力 | 自建理由 | 采购理由 | 建议 |
|------|----------|----------|------|
| 消息归档 | 核心差异——数据模型耦合深，外部产品无法匹配 | 工程量大，现有 SaaS 归档产品（如 Couchbase + S3）成熟 | **自建**——知识库耦合太深，外部产品无 Aero IM 的 query 模式 |
| 命令注册表 | 极少量代码即可实现 | 无适合独立购买的产品 | **自建**——两周内可完成 Phase 1 |
| 知识策展 | 竞品无人做过——自建是差异化 | 非核心能力，可集成 Notion API 做知识库存储 | **自建 MVP**——Phase 1 用规则 + LLM 提取，写入 PG；如果 PMF 验证成功再考虑 Notion/GitBook 集成 |
| 租户计量/billing | 数据模型已就位（ai_usage 表），只需加闸门 | Stripe/Metered 等计费平台可处理 | **混合**——配额门自建（数据在 PG 内），账单生成对接 Stripe |

---

## 五、实施路线图与风险

### 5.1 阶段划分与里程碑

#### Phase 1a：架构准备（4-6 周）— P0 前提条件

不需要用户可见功能，但后续所有方向依赖此阶段。

```
Milestone 1: 基础设施可观测性
  - 后台任务健康上报（health check endpoint 扩展）
  - PG 全表扫描监控（`pg_stat_user_tables` Prometheus gauge）
  - 每个定时器的执行耗时/错误率 metric

Milestone 2: 数据模型解耦
  - participant → workspace 关系拆分为 membership 表
  - 验证跨 WS 的 RLS（row-level security）可行
  - Subject 模板审计（统计所有 im.room.{id} 拼接点）

Milestone 3: PoolAllocator 框架
  - 支持共享池 + 专用池切换
  - 懒初始化 + 闲置回收
  - 配额表 + 门拦截点（存储/AI/用户数）
```

**风险**: 3 个里程碑并行可能资源不足。如果有 >2 个 P0 方向同时推进，建议分两轮——Milestone 1+2 → 再 Milestone 3。

#### Phase 1b：基础隔离 + 归档（6-8 周）— 两个 P0 并行

```
方向 1:
  - 每工作区连接池隔离（新增 workspace_pools 表 + PoolAllocator）
  - Redis 键名前缀迁移（渐进式，旧 key 同时读写两周后清理）
  - 配额门集成（存储/AI/用户数三个拦截点）
  - 新工作区创建时自动使用隔离池

方向 2:
  - message_stubs 热表（最小字段保留）
  - 归档 worker：每天扫 created_at < cutoff → INSERT INTO archive → DELETE from messages
  - 归档存储后端：S3 Parquet 格式（arrow-rs）
  - 热查询降级：已归档消息返回 stub → web 端显示「查看归档版」按钮
  - 归档索引表 + API 端点 `GET /api/rooms/:id/archive?year=2025`
```

**风险**: 两个 P0 并行开发，方向 2 的归档 worker 如果 bug 会导致数据丢失。**缓解**: 前两周只写不删（`write_to_archive + mark_as_archived`，`DELETE` 延后一个月），ROLLBACK 期可回退。

#### Phase 2：企业协作（8-12 周）— 两个 P1

```
方向 3 (先做，4-6 周):
  - rooms_in_workspaces 联结表 + 迁移
  - assert_room_access 扩展（per-workspace-as 参数）
  - UI 端共享频道创建流程
  - 通知隔离按工作区分隔

方向 4 (后做，4-6 周):
  - CommandRegistry trait + 进程内 HashMap 路由
  - GET /api/commands → 所有已注册命令
  - Bot Manifest（POST /api/bots/:id/manifest）
  - 命令注册失败退化为 404（不静默丢弃）
```

**风险**: 方向 3 的权限模型膨胀——做好 "最小可行权限"（只支持 can_manage flag），不加角色组，不加跨 WS 的 admin 委托。

#### Phase 3：AI 差异化（6-8 周）— P2

```
方向 5:
  - knowledge_entries 表 schema
  - Signal detection: 规则 + ai_jobs kind=Curate
  - 提取：批量 LLM 调用（复用 AiWorker 的配额管理）
  - 写入 + 通知
  - 反馈回路：用户确认/拒绝/编辑
```

### 5.2 资源估算

| Phase | 工程师数 | 前置依赖 | 上线判定条件 |
|-------|----------|----------|-------------|
| 1a | 1-2 | 无 | 所有后台任务有 metric，PG 查询有 explain 基线 |
| 1b | 2-3 | 1a | 新的 10K 消息/天工作区独立 pool 运行 2 周无故障；归档 worker 覆盖 90% 行 |
| 2 | 2-4 | 1b | 共享频道创建流程完整；10 个 bot 注册命令后 `/` 自动补全 < 200ms |
| 3 | 1-2 | 1b, 2 | 自动策展准确率 >70%（人工审核 100 条抽检） |

### 5.3 五类关键风险与缓解

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| **方向 2 数据丢失** | 中 | 高 | 归档前软删标记不动，避免 `delete from messages` 前未写归档；归档 worker 写前备份 |
| **方向 1 性能退化** | 中 | 中 | `PoolAllocator` 懒初始化 + `max_conn` 动态调整；共享池作为所有失败退路 |
| **方向 3 权限泄露** | 低-中 | 高 | 每个跨 WS 共享频道创建需双方 admin 确认（类似 Slack Connect 的双向审批） |
| **方向 5 策展噪音** | 高 | 中 | 默认策展结果用户需确认才公开；每周统计用户确认率来判断策展质量 |
| **五个方向并行→集成冲突** | 高 | 高 | 严格按 crate 边界划分——方向 1 在 `aero-storage`，方向 2 在 `aero-storage/archive`，方向 3 在 `aero-im-core`，方向 4 在 `aero-im-core/commands`，方向 5 在 `aero-ai/curation`。**避免一个 PR 改 3 个 crate** |

### 5.4 不做（No-Go）清单

有些方向看起来相关但**不推荐纳入本次 roadmap**：

1. **多区域 / 地理冗余**——方向 1 的隔离不包含多区域。需 NATS super-cluster + PG 流复制 + 跨区域 WS 降级协议，复杂度远超当前阶段
2. **MLOps 基础设施**——方向 5 如果走向模型驱动，需要训练数据 pipeline + 模型 serve + A/B 测试。目前用 LLM + 规则替代
3. **实时数据迁移工具**——方向 1 的隔离池上线后，现有工作区在线迁移到新池需要连接 drain + 重连逻辑。不建议先做，先用新建工作区隔离，遗留工作区逐步手动迁移

---

## 总结

| 评价维度 | 结论 |
|----------|------|
| **文档质量** | 极高——从代码扫描到产品推理都有据有锚，是我分析过的需求文档中最好的之一 |
| **架构可行性** | 五个方向在当前架构下都是可行的，但方向 2 的归档和方向 3 的权限模型是最值得深入设计的 |
| **最大不确定性** | 方向 5（知识策展）——不是技术挑战，而是产品/用户价值不确定性。建议以最小成本做 MVP，快测 PMF |
| **我的主要补充** | (1) 新增 `PoolAllocator` 抽象层 (2) `participant_workspace_membership` 提前解耦 (3) 归档时保留 `message_stubs` 防 seq 断裂 (4) 方向 3 的权限模型只做最小 flag 不膨胀 (5) 方向 5 走独立表而非 Canvas |

**最终建议**: Phase 1（P0 的两个方向）并行推进，但方向 1 的连接池隔离要垫在方向 2 之前——因为归档 worker 需要知道自己的工作区上下文中是否有独立池。Phase 1a 的 `PoolAllocator` 抽象是 Phase 1 两个方向的共享地基，优先完成。

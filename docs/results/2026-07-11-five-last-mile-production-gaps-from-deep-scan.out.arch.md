现在我已掌握足够的上下文，能够撰写全面的架构分析。

---

# Aero IM 架构分析报告

**日期**: 2026-07-12 | **基于**: 生产代码验证和设计文档回顾

## 1. 架构评估

### 1.1 核心优势

| 方面 | 现状 | 影响 |
|---|---|---|
| **事件驱动 DAG** | `Hub` + NATS JetStream 扇出，具有进程内 `mpsc` 背压 | 优雅的水平扩展 —— 每个节点独立扇出，不共享内存。bounded 通道防止 OOM |
| **存储层分离** | `aero-storage` 中的 `XRepo` 模式有 ~146 个文件 | 关注点分离明确；每个功能在其 repo 中可独立测试 |
| **背压处理** | `fan_out_raw` 使用 `try_send` + 慢消费者断开机制 | 一个故障客户端无法拖垮整个广播器 |
| **幂等模式** | `ON CONFLICT DO NOTHING` 在所有关键路径中使用（OOO bot、upserts） | at-least-once 投递的稳健安全网 |
| **无 unsafe 代码** | 工作区 lint 禁止 `unsafe_code` | 内存安全保证 —— 在涉及媒体处理（str0m 是纯 Rust）的系统领域中至关重要 |
| **面向能力的 Bot 架构** | 每个 bot（unfurl、transcribe、moderation、ooo、golive）都是独立的 NATS 消费者 | 独立扩展、独立失败、独立部署 |

### 1.2 架构债务（技术债）

我根据严重性和影响范围对每个发现进行分类：

#### 类别 A：数据完整性风险（高影响，应规划为 P1）

1.  **草稿无版本/乐观锁**
    - **文件**: `draft.rs:58-72` — `ON CONFLICT ... DO UPDATE SET` 无 `version` 列
    - **风险**: 如果同一用户从两个设备发送草稿，最后一个写入者获胜。草稿文本可能静默丢失。
    - **缓解**: 当前仅影响草稿（短暂数据，非消息）。但用户会注意到“我保存的草稿不见了”。
    - **修复**: 向 `drafts` 表添加 `version` 列，在 `WHERE version = $N AND version = version + 1` 上执行乐观锁。

2.  **附件无每用户/每工作区配额**
    - **文件**: `routes.rs:1604` — `MAX_BLOB_BYTES = 32 MiB` 仅限制单次上传
    - **风险**: 一个用户可以上传 32 MiB × N 个 blob，消耗所有磁盘空间，影响同一工作区中的其他用户。
    - **缓解**: 无 —— 这是一个 DoS 向量，无论存储后端如何。
    - **修复**: 添加 `SUM(size) WHERE owner_id` 在每个上传上的检查。考虑按工作区聚合（`SUM(size) WHERE room_id IN (rooms in workspace)`）。

3.  **读取不保证读写一致性（RYW）**
    - **文件**: `search.rs:79`、`ai_usage.rs:120`、`analytics.rs:66` — 使用 `pg_read`（读副本）
    - **风险**: 用户发送消息后立即搜索，可能因为 PG 复制延迟而看不到。对于企业 SLA 来说是一种糟糕的体验。
    - **缓解**: 对于大多数部署，同步复制 + 低延迟意味着罕见。但一旦部署多 AZ，就会成为问题。
    - **修复**: 选项 A：使读写路径在同一连接上具有粘性（会话亲和性）。选项 B：对于“刚写入者”，在 `pg_read` 查询前强制执行 `COMMIT` + `wait`。

#### 类别 B：运维风险（高影响，应规划为 P1-P2）

4.  **FTS / Embedding 索引无 REINDEX cron**
    - **文件**: 迁移 0075、0136 创建索引但无后续维护
    - **风险**: pgvector HNSW 索引和 GIN 索引在大量写入后会随时间碎片化。查询会在数月内静默减速。
    - **缓解**: 在多数部署中，每日 `REINDEX CONCURRENTLY` cron 是标准操作。此处缺失。
    - **修复**: 添加一个可配置的定时器（如 `AERO_REINDEX_HOUR`，默认禁用），在低峰期运行 `REINDEX INDEX CONCURRENTLY`。

5.  **消息嵌入编辑后通过 `SET embedding = NULL` 正确重建**
    - **已验证准确**: `crud.rs:99` — 编辑将 `embedding = NULL`，然后 `enqueue(Embed)` 触发重建。
    - **结论**: 此路径正确。**无债务** —— 验证报告中的“不准确”声明在此被推翻。

#### 类别 C：可扩展性瓶颈（影响中等，应规划为 P2）

6.  **跨频道通知无去重**
    - **文件**: 不存在像 `source_path` / `channel_delivered` 这样的机制
    - **风险**: 一个用户属于 20 个频道，一条 @mention 广播可能导致 20 个重复的推送通知。
    - **缓解**: 当前隐式地依赖客户端去重，但推送提供商（FCM/APNs）按设备收费。
    - **修复**: 每个通知需要一个 `dedup_key`（例如 `(recipient, source_message_id)`），以便推送 bot 可以跳过已送达的通知。

7.  **可观察性仪表无 REINDEX 的聚合查询**
    - **文件**: `ai_usage.rs`、`analytics.rs` — 对 PG 跑 `SUM`/`COUNT` 大表
    - **风险**: 没有一个提示即可提供统计信息的物化视图或专用计数器表。
    - **修复**: 对于分析，添加物化视图（`REFRESH MATERIALIZED VIEW CONCURRENTLY` 按定时器）。对于 AI 使用量，使用 Redis 计数器 + 后台 flush。

#### 类别 D：设计错误（影响较低，未来规划）

8.  **嵌入命名空间未标记 `embedding_model`**
    - **风险**: 如果嵌入模型提供商切换（如 Voyage v2 → v3），新旧嵌入在 `embedding` 列中混合，导致混合搜索质量下降。
    - **缓解**: 需要一个 `embedding_model` 列（或 `metadata` JSONB 字段），以便 `search.rs` 可以按模型过滤。添加一个扫地僧来重新嵌入旧数据。

9.  **FTS 分析器变更在迁移中无回填**
    - **风险**: 迁移 0128/0131 添加分析器，但现有消息的 `search_tsv` 列仅在通过 DML 触发器修改时更新。旧消息索引不匹配。
    - **修复**: 重建分析器的迁移应始终以 `UPDATE messages SET searchable_text = searchable_text WHERE ...`（触碰触发器）结束，或显式的 `REINDEX`。

### 1.3 关键设计决策合理吗？

| 决策 | 裁决 | 理由 |
|---|---|---|
| 每个 crate 一个功能单位（16 个 crate） | ✅ 正确 | 允许独立编译、隔离测试、清晰的依赖边界。与 `scripts/file-size-check.sh` — 1200 行硬限制配合良好 |
| NATS JetStream 作为跨节点事实来源 | ✅ 正确 | at-least-once 投递、持久消费者偏移、水平扩展。全系统的正确选择 |
| Hub 中进程内 bounded mpsc 扇出 | ✅ 正确 | 防止 OOM，强制实施消费者纪律。带 `RESYNC_FRAME` 的慢消费者策略很优雅 |
| Presence/CallRoster 使用 Redis sorted-set | ✅ 正确 | 集群范围的一致性，无需节点间协调。TTL 过期自然 |
| AI 使用 fail-open 降级（HashEmbedder） | ✅ 正确 | 没有 API 密钥时系统不会崩溃。但用户需要知道搜索质量正在下降（当前无声） |
| 无客户端加密（MLS 只是脚手架） | ✅ 正确 | 首先运送产品价值。加密稍后可以作为透明层添加 |
| 每个 bot 一个 NATS 消费者 | ⚠️ 部分正确 | 隔离很好，但 7 个消费者 = 7 个连接 + 7 个确认流。对于更大规模，可能考虑消费者多路复用 |

## 2. 扩展方向

### 方向 A：多区域部署

**为什么需要**：当前架构假设单个 NATS 集群 + 单个 PG + 单个 Redis。对于全球多区域部署，这成为 SPOF（单点故障）。

**核心挑战**：
- NATS JetStream 跨区域复制（NATS 超级集群）需要谨慎的网络拓扑
- PG 逻辑复制延迟破坏 RYW 保证
- Redis 跨区域复制是异步的 —— 可能存在脑裂场景
- 媒体面 SFU + call-bridge 需要区域亲和性（将观众路由到最近的 SFU）

**预期的架构变更**：
- 引入区域路由层（基于 DNS 或基于 HTTP 标头的 `x-region`）
- 将 `presence` 和 `call_roster` 等内容移动到每个区域一个 Redis 实例，并通过区域关联 API 处理
- `aero-bus` 的 `EventBus` trait 需要区域感知实现（本地消费者优先，跨区域后备）
- `blob_store` 需要跨区域复制或回源读取

**影响**：跨领域 —— 影响所有 crate。存储（复制策略）、总线（区域拓扑）、信令（SDP 优先级）、SFU（区域连接）

**优先级**：P2 — 在单区域运行已验证之前无需。但**现在的架构决策**应该避免使多区域变得不可能（目前还可以）。

### 方向 B：统一通知收件箱 + 跨通道去重

**为什么需要**：当前，用户会从每个通道获得重复的推送 + 收件箱条目。企业用户报告“通知疲劳”。这是在建立信任方面的高影响力 UX 改进。

**核心挑战**：
- 去重需要 `dedup_key` 跨 bot 消费者可见（需要共享 Redis 集或 PG 表）
- bundle 超时（当前 30-40 秒）意味着在 bundle 截止日期之前，收件箱条目不会被提交。如果在截止日期之前处理了其他通知，用户会看到延迟
- 已读回执机制需要是跨通道的 —— “全部标记已读”应清除所有通道的通知，而不仅仅是一个

**预期的架构变更**：
- 添加 `notification_dedup` 表（或 Redis 集）与 `(recipient_id, source_message_id)` 上的 UPSERT
- 引入 `NotificationService` 编排器，它代替直接推送到通道。编排器：
  1. 接收原始通知请求
  2. 检查去重
  3. 在 `notifications` 表中插入
  4. 分派到 push、email、inbox 通道
- `push_bot` 和 `email_bot` 变为 `NotificationService` 的策略，而不是独立的 NATS 消费者

**影响**：`aero-push` 重构。需要新的 `aero-notify` crate。`notification_bundle.rs` 逻辑集成。

**优先级**：P1 — 高用户可见性影响，前期架构影响低，后期更难添加。

### 方向 C：多级缓存层

**为什么需要**：当前，`participant_cache` 和 `room_member_cache` 是每个节点上的 TTL 缓存。在大型工作区（10K+ 成员），每个房间事件触发成员列表数据库查询。按工作区限流也会命中数据库。

**核心挑战**：
- 缓存失效 —— 当管理员在节点 A 上添加成员时，节点 B 的缓存会过时，直到 TTL 到期
- 写入放大 —— 每个房间事件的成员查询可能压倒 PG
- 缓存一致性 vs 最终一致性 —— 对于 @here @everyone 广播，可以容忍过时的成员列表吗？

**预期的架构变更**：
- 添加 Redis 支持的房间成员缓存（排序集 `room:{id}:members`）
- 使用 Redis pub/sub 进行缓存失效通知（`invalidate:room:{id}:members`），以便所有节点都收到通知
- 用两层模型（L1 本地 TTL + L2 Redis）替换 `participant_cache`
- 限流器 (`rate_limit.rs`) 也受益于 Redis 后端，用于跨节点协调

**影响**：`aero-server` 中的 `hub.rs`、`participant_cache.rs`、`rate_limit.rs`。`aero-storage` 从 Redis 读取成员身份。

**优先级**：P1 — 在当前规模下已经感受到痛点（读取路径上的成员查询）。

### 方向 D：静态分析 / 异步预算治理

**为什么需要**：AI 预算系统（`CostBudget`、`KeyedCostBudget`）经过了深思熟虑但很脆弱 —— 它依赖于每个操作正确记账。审计日志目前写在 `ON CONFLICT` 上。当一个 bot 或一个用户行为不当时，不存在“断路器”模式。

**核心挑战**：
- AI 预算的失败关闭 vs 失败打开 —— 如果 Redis 在预算检查时宕机会发生什么？目前失败打开（允许通过）可能会让按使用付费的 AI 账单变得疯狂
- 无后端协调的速率限制 —— `AERO_RATE_LIMIT_PER_SEC` 是单进程。在 N 个节点上，有效速率是 N 倍
- 没有“震惊”保护 —— 如果用户编写了一个循环来调用 AI，系统应优雅降级，而不是生成 50 个 AI 调用然后才限流

**预期的架构变更**：
- 用于 AI 预算的 Redis 后端（不仅仅是 `KeyedCostBudget` 中的进程 `HashMap`）
- 跨节点速率限制（Redis `INCR` + `EXPIRE` 滑动窗口）
- 断路器模式 —— 如果在 60 秒窗口内超过 X 次错误，AI 调用会立即失败，无需击中 API
- `AiWorker` 预算逻辑变为可插拔（`BudgetProvider` trait），本地或 Redis 支持

**影响**：`aero-ai`（预算逻辑）、`aero-common`（速率限制类型）、`aero-server`（状态初始化）

**优先级**：P2 — 当前是功能性的。使 Redis 成为可选项（在测试中保持单进程）。

### 方向 E：内容寻址 Blob 存储 + CDN 集成

**为什么需要**：当前 Blob 下载使用 `blob_store.get(id)` 是一个代理模型 —— 服务器下载整个 blob 然后提供给客户端。对于大文件（32 MiB 上限），这意味着服务器 RAM 压力 + 客户端延迟。

**核心挑战**：
- 预签名 URL（S3 `PresignedGetObject`）可以从服务器卸载传输，但需要客户端到 S3 的路径（安全考虑 —— 泄漏的 URL 可以共享）
- 基于内容的寻址（blake3 哈希作为存储键）启用自然去重 —— 相同文件不重复存储
- 如果强制通过服务器代理（用于访问控制），CDN 集成更复杂
- `Cache-Control` 和 `ETag` 标头缺失（已验证），意味着浏览器从不缓存 blob

**预期的架构变更**：
- 添加 `blob_hash` 列作为 `(size, blake3_hex)` 的唯一约束
- 在 `blob_store` trait 中添加 `store_with_hash(bytes, hash) -> BlobId` 和 `get(blob_id) -> bytes`
- 添加 S3 预签名 URL 生成作为一个选项（配置文件切换）
- 添加 `Cache-Control: public, max-age=31536000` + `ETag: <hash>` 到 blob 下载
- 对于恶意内容检测，内联 `av_scan` 在存储之前（已经存在）

**影响**：`aero-storage`（`BlobRepo`、`BlobStore` trait）、`aero-server`（`routes.rs` 中的 `blob_download`）、`aero-common`（BlobId 类型）

**优先级**：P2 — 高带宽收益，但短期部署不需要。

## 3. 接口设计建议

### 3.1 当前接口评估

项目在 trait 设计方面总体上做得不错：

| Trait / 抽象 | 评估 | 建议 |
|---|---|---|
| `EventBus` (aero-bus) | 干净，最小 —— `publish` + `subscribe` | ✅ 无需改变 |
| `BlobStore` trait | 存在但简单 —— `get` + `put` + `delete` | 扩展以添加 `presigned_get_url(blob_id, ttl_secs)` 用于 S3 卸载 |
| `AiBackend` (state.rs) | 正确抽象 —— 允许交换实现 | 添加 `BudgetProvider`（如下所述） |
| `LiveIngest` trait | 存在但仅用于 RTMP | 使其通用 —— WHIP 和 SRT 应实现相同的 `LiveIngest`，以便下游（HLS）不关心来源 |
| `XRepo` 模式 | 146 个仓储，组织良好 | 考虑添加 `RepoExt::with_pg(pool)` 用于获取只读复制品的通用模式（解决 RYW） |
| `Hub::fan_out_raw` | 经过良好测试的签名 | ✅ 无需改变 |

### 3.2 需要的新抽象

**1. `BudgetProvider` trait**
将当前的 `CostBudget` 逻辑（在 `aero-ai` 中）提取到一个 trait 中：

```rust
#[async_trait]
pub trait BudgetProvider: Send + Sync {
    /// 对于 `kind` 种类的作业，检查 `ws` 在预算内。
    /// 返回剩余预算（或“继续”/“延迟”）。
    async fn check(&self, ws: WorkspaceId, kind: AiJobKind) -> Result<BudgetDecision>;
    /// 花费 `amount` 记账。
    async fn spend(&self, ws: WorkspaceId, kind: AiJobKind, amount: Weight) -> Result<()>;
}
```

这允许：
- 同一路径下的 `LocalBudgetProvider`（当前进程 HashMap）和 `RedisBudgetProvider`
- 测试中的模拟 —— 不再需要硬编码的睡眠等待预算重置

**2. `CacheLayer<K, V>` trait**
将 `participant_cache` 和 `room_member_cache` 抽象为一个通用缓存层：

```
(participant_id -> Participant) 的 L1 = 本地 TTL HashMap
(room_id -> Vec<ParticipantId>) 的 L2 = Redis 排序集
失效 = Redis pub/sub 通道
```

这应当包含在 `aero-storage` 内部，而不是每个模块都有自己的专用缓存。

**3. `NotificationChannel` trait（未来）**
当添加统一收件箱时：

```rust
#[async_trait]
pub trait NotificationChannel: Send + Sync {
    fn kind(&self) -> &'static str; // "push", "email", "inbox"
    async fn deliver(&self, recipient: ParticipantId, notif: &Notification) -> Result<DeliveryStatus>;
}
```

### 3.3 向后兼容性

- 所有新的 trait 应当具有默认实现，因此现有代码无需更改
- 添加新的 `RoomEvent` 变体（如修改 `kind` 标签）必须记入 `AGENTS.md` 的 §4.2 中（警告 `kind` 字段序列化陷阱）
- 迁移应当 `CREATE TABLE IF NOT EXISTS` 幂等
- `XRepo` 构造函数的签名不应改变 —— 通过 `Option<T>` 或可选的 builder 模式添加新依赖

## 4. 技术选型

### 4.1 当前栈评估

| 组件 | 裁决 | 理由 |
|---|---|---|
| **Rust 2021 + tokio** | ✅ 完美匹配 | 高性能、低资源占用、内存安全。Axum 0.7 是生产级的。 |
| **Postgres 17 + pgvector + pg_trgm** | ✅ 完美匹配 | FTS + 向量搜索 + 关系型 ACID。工作区应用都需要。 |
| **Redis 7** | ✅ 完美匹配 | 用于 sorted-set 的 Presence，会话缓存，速率限制。 |
| **NATS JetStream** | ✅ 完美匹配 | at-least-once 直接匹配扇出架构。用于持久消费。 |
| **str0m** | ✅ 正确选择 | 纯 Rust ICE/DTLS/SRTP。无 C 绑定，无 `unsafe`。与项目的无 unsafe 策略一致。 |
| **rml_rtmp** | ✅ 可以 | 有用的 RTMP 摄入。 |
| **ClamAV (clamd)** | ✅ 正确的安全层 | 内联病毒扫描在 blob 存储之前。 |

### 4.2 考虑添加的

| 技术 | 目的 | 优先 | 决定因素 |
|---|---|---|---|
| **Redis Stack / RedisGears** | 跨节点速率限制 + 分布式计数器 | P2 | 可选的。回退到进程本地。当 Redis 宕机时，速率限制应失败打开（如 aws sigv4）。 |
| **物化视图 (PG)** | 分析仪表板（没有 `COUNT(*)` 大表扫描） | P1 | SQL 级，无新依赖。用 `REFRESH MATERIALIZED VIEW CONCURRENTLY` 和定时器实现。 |
| **S3** | blob 存储和预签名 URL | P2 | 已经通过 `BlobStore` trait 作为可选项支持。只需连接它。 |
| **OTLP / Jaeger** | 分布式追踪 | P1 | 已经在 `common/src/telemetry.rs` 中使用。确保跨 NATS 边界追踪（已验证 — `bus_consume_span` 已经提取 W3C `traceparent`）。 |
| **CDN（CloudFront / Fastly）** | 静态 blob + HLS 段卸载 | P3 | 当 blob 传输成为瓶颈时。影响架构 —— 需要将 `Cache-Control` 和 `ETag` 添加到 blob 响应中。 |
| **FCM / APNs** | 移动推送 | P1 | 已经通过 `aero-push` 作为可选项支持。保持 `FakeGateway` 用于测试。 |
| **Anthropic / Voyage** | AI 推理 + 嵌入 | P1 | 已经连接。`HashEmbedder` 回退在没有密钥时提供合理的降级。 |

### 4.3 自建 vs 购买

对于这个项目，决策框架应为：

| 决策 | 推荐 | 理由 |
|---|---|---|
| **AI 推理** | 购买（Anthropic/OpenAI API） | 自建 LLM 推理需要 GPU 基础设施 + MLOps。对于项目范围来说是不成比例的。 |
| **嵌入** | 购买（Voyage API）与本地后备 | Voyage 对于 RAG 质量更好。`HashEmbedder` 在测试/开发中没有密钥时提供 0 成本降级。 |
| **SFU / WebRTC** | 自建（str0m） | 核心差异化因素。纯 Rust、无 unsafe、与 Axum 无缝集成。无现成的替代品能匹配这三个特性。 |
| **消息队列** | 购买（NATS） | JetStream 是同类最佳。自建 = 重新发明 Raft/Paxos。 |
| **推送通知** | 购买（FCM/APNs） | 移动操作系统强制这些。无有意义的自建替代方案。 |

## 5. 实施路线图

### 阶段 1：基础加固（P0-P1）— 估算 2-3 周

| 项目 | 优先级 | 风险 | 缓解措施 |
|---|---|---|---|
| **草稿乐观锁** | P0 | 低 —— 仅影响草稿。DDL + ~20 行 Rust | `ON CONFLICT` 路径会优雅失败，因此添加版本后旧客户端获得 `Conflict`，他们会重试。 |
| **每用户/每工作区附件配额** | P0 | 中等 —— 需要新索引 + 运行时检查 | 首先对用户禁用（0 = 无限制）。使用 `SELECT COALESCE(SUM(size), 0) FROM blobs WHERE owner_id = $1`。添加 GUC `aero.max_blob_bytes_per_user`。 |
| **FTS + 向量索引的 REINDEX 定时器** | P1 | 低 —— 配置默认禁用 | 添加 `AERO_REINDEX_HOUR`（默认 0 = 禁用）。使用 `REINDEX INDEX CONCURRENTLY`（非阻塞）。 |
| **读取路径的 RYW 粘性** | P1 | 中等 —— 影响池配置 | 选项 A（推荐）：在 `pg` 连接上使用 `SET session_replication_role = 'local'`。选项 B：标记“最后写入者”并强制 `pg_read` 到同一节点。从选项 A 开始。 |

### 阶段 2：运营卓越（P1）— 估算 3-4 周

| 项目 | 优先级 | 风险 | 缓解措施 |
|---|---|---|---|
| **Redis 支持的速率限制** | P1 | 中等 —— 在 Redis 宕机时影响容错 | 失败打开（没有 Redis = 无跨节点限制）。使用 `INCR` + `EXPIRE` 用于滑动窗口。 |
| **Redis 支持的多级缓存** | P1 | 中等 —— 高速缓存未命中期间 PG 上的负载峰值 | 通过 `tokio::sync::watch` 在文档中预先设置 + 本地失效传播。 |
| **FTS 分析器回填迁移** | P1 | 低 —— DDL 现有模式 | 迁移应该更新每一行以触发 `search_tsv` 触发器。 |
| **可观察性：物化视图用于分析** | P1 | 低 —— SQL 仅 | 与 `ai_usage.rs` 和 `analytics.rs` 同处。使用 `CONCURRENTLY` 进行无锁刷新。 |

### 阶段 3：通知改革（P1-P2）— 估算 3-4 周

| 项目 | 优先级 | 风险 | 缓解措施 |
|---|---|---|---|
| **去重键（`dedup_key`）在通知上** | P1 | 低 —— 新列，幂等插入 | 添加 `notification_dedup` 表。upsert 与 `(recipient_id, source_message_id)` 上的 `ON CONFLICT DO NOTHING`。 |
| **统一通知编排器** | P2 | 高 —— 大型重构 | 不要先通过编排器重构所有路径。相反，将推送到编排器添加为并行路径。如果编排器出错，就回退到直接推送。 |
| **跨通道已读回执** | P2 | 中等 —— 需要前端 + 后端协调 | `read_all` 应该传播到所有通道。从 `notifications` 表中的 `read_at` 列开始。 |

### 阶段 4：多区域 / 内容寻址（P2）— 估算 4-6 周

| 项目 | 优先级 | 风险 | 缓解措施 |
|---|---|---|---|
| **Blob 的 ETag / Cache-Control** | P2 | 低 —— HTTP 标头只 | 向 `routes.rs:1750-1810` 添加标头。对存储本身没有影响。 |
| **内容寻址 blob 存储 + 去重** | P2 | 中等 —— 新的 `blob_hash` 列，影响现有 blob | 向后兼容：遗留 blob 获得 `hash = NULL`。新 blob 计算 `blake3`。迁移后台填充哈希。 |
| **S3 预签名 URL 卸载** | P2 | 中等 —— 在 S3 模式下影响安全模型 | 保持 `BlobStore::get` 用于强制代理模式。添加 `BlobStore::presigned_get_url` 作为可选的优化路径。 |
| **区域路由层** | P2 | 高 —— 跨领域影响 | 首先添加 DNS 顶部分发器（`x-region` 标头）。NATS 超级集群是单独的工作。将此拆分。 |

### 5.1 关键依赖路径

```
阶段 1（基础加固）→ 阶段 2（运营卓越）
     ↓                    ↓
阶段 3（通知）        阶段 4（多区域）
```

- **阶段 1 是阶段 2 的先决条件**：没有稳定基础，不要添加缓存
- **阶段 3 可以从阶段 1 并行开始**：通知去重相对独立
- **阶段 4 依赖于阶段 2**：多区域需要 Redis 缓存 + 读取副本策略

### 5.2 风险矩阵

| 风险 | 可能性 | 影响 | 缓解措施 |
|---|---|---|---|
| 新索引减慢 PG 写入 | 低 | 高 | 使用 `CONCURRENTLY` 添加索引，在低峰期 |
| Redis 宕机 → 限流丢弃请求 | 中 | 中 | 失败打开策略 —— 没有 Redis 时使用本地速率限制 |
| 乐观锁 → 客户端冲突错误 | 低 | 低 | 前端重试逻辑（指数退避） |
| FTS 触发器重写减慢编辑 | 低 | 中 | 在添加触发器的同一迁移中创建索引。在副本上测试。 |
| 区域拓扑更改 → NATS 分区 | 低 | 高 | 保持单区域拓扑作为主要拓扑。区域是多个 NATS + PG + Redis 的独立堆栈，而不是分布式单一集群。 |

### 5.3 成功标准

每个阶段后：
- **阶段 1**: `cargo test --workspace --lib` → 绿色。无新 clippy 警告。草稿具有版本控制。附件受配额限制。
- **阶段 2**: 速率限制在 Redis 节点失败时保持活动。分析查询在 <100ms 内运行（从秒级改善）。`/metrics` 端点报告索引健康状况。
- **阶段 3**: 用户报告通知减少 10 倍（去重后）。bundle 延迟 <5 秒（从 30-40 秒改善）。
- **阶段 4**: 根据区域配置切换，blob 下载达到 1Gbps。`ETag` 使重复下载变为 304 状态。

---

## 附录 A：忽略的验证发现参考

| 原始声明（文档） | 验证裁决 | 建议修正 |
|---|---|---|
| `Content-Disposition` 无差异化 | ❌ 不准确 | 修复为“无 CDN 缓存头 / ETag” |
| `MAX_BLOB_BYTES = 10MB` | ❌ 不准确 | 修复为 32 MiB（`32 * 1024 * 1024`） |
| 编辑后嵌入不更新 | ❌ 不准确 | 删除此声明 —— 路径正确 |
| Bundle 无超时逃逸（小时级） | ❌ 不准确 | 修复以记录 `AERO_BUNDLE_DEADLINE_SECS`（默认 30s，最大延迟 ~40s） |

以下是我对验证报告的架构分析：

---

# 架构分析：Aero IM 五方向评估

## 1. 架构评估

### 1.1 当前架构的优势

Aero IM 的核心架构选择基本上是合理的，有几个值得肯定的设计决策：

**事件驱动骨架是正确选择**。NATS JetStream 作为跨实例事实源（durable consumer `aero-server` + ephemeral `live.stream.*`），加上进程内 `Hub::fan_out_raw` bounded mpsc 扇出的二级架构，是目前事件驱动 IM 系统的高性价比模式。这比全走 Redis Pub/Sub 或全走 NATS 扇出到每个 WS 连接都要优雅——它在跨实例可靠性和进程内吞吐之间做了正确的分层。

**Crate 分层清晰且克制**。基础层（`aero-common`/`aero-bus`/`aero-storage`/`aero-auth`/`aero-signaling`）→ IM 层→ 直播层→ 组合层（`aero-server`），依赖自下而上无环。每个 crate 有明确的职责边界，没有出现常见的"把所有业务逻辑塞进 server"的毛病。

**AI 管线的预算控制**是罕见的严谨设计。`AiWorker` 的 per-ws `KeyedCostBudget(60)` + 全局 `CostBudget(300)` over 60s，weighted all-or-nothing，`SKIP LOCKED` 水平扩——这套方案即使放到 SaaS 产品里也算成熟，不是 MVP 凑合。

**清理管线（retention sweep）的覆盖面**被验证报告低估了。17+ sweep 函数覆盖 19 张表，每张表的 retention 窗口都可配置，审计事件还有自动分区清理。这在同类项目中属于前 20% 的治理水平。

### 1.2 架构债务与约束

核心问题是以下三个**结构性债务**：

**① 搜索融合管线分裂**。搜索端用 `merge_hits`（max-score fusion），AI RAG 用 `fuse_rankings`（RRF），两套融合逻辑各管各的，没有共享抽象。`SearchHit.headline` 永远 `None`——高亮文本生成的空缺意味着搜索结果页的体验上限就是"返回消息 ID 列表"。更根本的问题：`search_click_events` 表数据存在但不被消费，点击反馈→检索器加权的闭环断裂。

**② Webhook 递送模型过于简化**。`dispatch_event` 是同步逐目标迭代，`sub.ack()` 在所有目标处理完后才调用——这意味着一个慢响应 Webhook 会阻塞后续所有事件的递送。虽然验证报告指出"游标不停止推进"（因为 ack 还是调了），但**目标级串行**仍然是实时性问题：假设某个 workspace 有 50 个启用的 Webhook，其中一个端点平均响应 5 秒，那么其他 49 个 Webhook 的递送延迟就是 5 秒起步。这不是背压问题，是**扇出模型问题**——它把 fan-out 当成了 for-loop。

**③ 强一致性假设下的可扩展性瓶颈**。几个关键路径强依赖事务和行锁：`assert_room_access` 串 room→workspace→member→deactivation→2FA 门控链、`FOR UPDATE SKIP LOCKED` 的 `ai_jobs` 消费、投票 `FOR UPDATE` 复检 `closed_at`。在单机 PG 下这些没问题，但要水平扩展时，这些"以一致性为代价的简洁实现"会成为分片/读写分离的阻碍。这不是立即要改的问题，而是架构选型时应该意识到的约束。

### 1.3 关键设计决策评估

| 决策 | 评价 | 理由 |
|------|------|------|
| NATS JetStream 作为总线 | ✅ 合理 | 比 Kafka 轻，比 Redis Stream 可靠，durable+ephemeral 混合使用正确 |
| Redis sorted-set 做集群状态 | ✅ 合理 | presence/roster/在线计数天然适合 TTL + zremrangebyscore |
| `Block` 枚举模仿 Slack Block Kit | ⚠️ 双刃剑 | 降低了 AI 集成成本（LLM 直接产 `Block`），但扩展困难——加一个新 variant 需要改所有 `match` 臂 |
| SFU 进程内 `Arc<RwLock<HashMap>>` | ⚠️ 风险 | 验证报告正确指出了"进程内状态，非 Redis"，但这不是 bug 是设计取舍。跨节点媒体桥走 UDP relay，每节点 SFU 状态独立是合理的；但如果需要节点内 SFU 实例的水平扩，这个 HashMap 需要外部化 |
| 实时不变量写在 `AGENTS.md` | ✅ 极好 | 幂等守卫、fail-open 策略、自回防止——这些是系统中最容易出 bug 的地方，明确写下是最好的防御 |

---

## 2. 高价值架构扩展方向

### 方向 A（P0）：消息块组件系统——引入组件版本协商与布局引擎

**为什么需要**：
这是 Aero IM 与 Slack/Discord 之间最明显的体验差距。当前只有 Button + Select 两种交互组件，没有表单输入、日期选择器、数字输入、Section/Divider/Accordion 布局原语。对于 AI 原生 IM 的定位，"AI 能生成复杂交互消息"是核心差异化能力——Anthropic 的 Tool Use + Structured Output 天然适合产出复杂 Block 布局，但接收端没有对应的渲染和交互基础设施。

**核心挑战**：

1. **向后兼容是硬约束**。现有客户端不认识新组件时必须降级展示（`alt_text` 机制），否则存量用户看到的是空白/乱码。这意味着每个新组件必须附带一个纯文本/纯 Text 的降级表示。
2. **`Block` 枚举的 Rust 类型系统负担**。加一个 variant 改所有 `match`——这是 Rust tagged union 的经典摩擦。如果 Block Kit 迅速膨胀（20+ variants），所有 match 点都会变成上百行的样板。
3. **交互状态的持久化**。多步交互（wizard/表单填写）需要上下文 token 和临时状态存储，当前 `Interaction` 是单次无状态投递。

**预期的架构变更**：

```
// 新增抽象层
trait RenderableBlock {
    fn render(&self, version: BlockVersion) -> RenderedBlock;
    fn alt_text(&self) -> Option<String>;
    fn validate(&self) -> Result<(), BlockValidationError>;
}

// 组件注册表（代替 match-all-the-things）
struct BlockRegistry {
    renderers: HashMap<&'static str, Box<dyn RenderableBlock>>,
}

// 版本协商
enum BlockVersion { V1, V2 /* current */, V3 /* +form inputs */ }
```

引入 `BlockVersion` 字段（WS 握手时协商），服务端根据客户端版本选择序列化表示。新组件在低版本客户端自动降级为 `[alt_text]` Card。

**对现有系统的影响**：中等。`Block` enum 本身不会动，但所有 `match block { ... }` 的消费方（搜索索引、AI 上下文构建、WS 序列化）需要兼容 alt_text 路径。核心影响范围是 `web/` SPA 的 `render.js`。

---

### 方向 B（P0）：备份/恢复与灾难恢复基础设施

**为什么需要**：
这是企业 SLA 的门槛项，不是可选项。当前零备份基础设施的含义：任何 PG 实例级别的故障（磁盘损坏、误删除、az 故障）都意味着**完整数据丢失**且**不可恢复**。NATS JetStream 的 consumer cursor 依赖于 stream 本身的可用性，PG 挂了 cursor 也救不了数据。

**核心挑战**：

1. **PG 逻辑备份 vs 物理备份**。`pg_dump` 逻辑备份在 TB 级数据库上不现实（恢复时间不可控），需要 `pg_basebackup` + WAL 归档的物理备份方案。这需要配置 `archive_mode`、`archive_command`，且需要远程 blob 存储（S3/MinIO）存放 WAL 段。
2. **RPO/RTO 指标的明确**。对于 IM 系统，可接受的数据丢失窗口是多少？5 分钟？1 分钟？接近零？这决定了备份频率和复制拓扑——金融级需要同步复制，IM 级通常异步就够。
3. **恢复的编排自动化**。手动恢复一个跨 PG/Redis/NATS 的系统比恢复单个数据库复杂一个数量级——需要确保各组件恢复到一致的时间点。

**预期的架构变更**：

```
crates/aero-server/src/bin/boot/
├── backup.rs           # 新增：备份调度（pg_dump / WAL 归档触发）
├── recovery.rs         # 新增：恢复编排（runbook 的编程表示）

scripts/
├── backup.sh           # pg_dump 封装
├── restore.sh          # 按时间点恢复
├── chaos/              # 混沌工程实验
│   ├── kill-pg.sh
│   └── corrupt-blob.sh

docker-compose/
├── docker-compose.ha.yml  # PG streaming replica + NATS stream mirror
└── docker-compose.dr.yml  # 跨机房
```

**对现有系统的影响**：低。备份基础设施是独立组件，不侵入业务代码。唯一的侵入点是 `CancellationToken` 优雅关停的覆盖范围——需要验证 drain 逻辑在 PG 故障时仍然正确执行（当前 `shutdown.rs` 依赖 PG 可用来完成最后的事务提交，这在 DR 场景下可能需要调整）。

---

### 方向 C（P1）：搜索管线统一——RRF 融合 + Learning-to-Rank + 高亮

**为什么需要**：
当前搜索架构有两套并行的融合逻辑（用户搜索用 `merge_hits` max-score，AI RAG 用 `fuse_rankings` RRF），且 `search_click_events` 数据无人消费。这意味着：用户的每次点击都在产生数据，但没有任何反馈回路让搜索排序变得更好。搜索结果页的体验上限就是"返回了一堆消息，但为什么排成这样——不知道"。

**核心挑战**：

1. **RRF 不是银弹**。RRF 只是 rank-level fusion，不解决 score calibration 问题。当 FTS 和 vector 检索的质量差异大时，RRF 不一定优于 max-score 融合。需要引入可配置的融合策略选择（按 query 类型、按 workspace）。
2. **点击反馈的延迟与稀疏性**。Learning-to-Rank 需要足够的点击数据才有统计意义——对于低频搜索的 workspace，可能需要几个月才能积累足够信号。需要fallback 到无监督排序（BM25/余弦相似度）。
3. **`SearchHit.headline` 的生成成本**。生成带高亮片段的摘要需要：定位匹配位置→截取上下文→插入 `<em>` 标记。对于 FTS 可以用 `ts_headline`（PG 内置），对于向量检索需要从原始文本中提取匹配片段——这本质上是跨检索方式的碎片化问题。

**预期的架构变更**：

```
// 统一融合策略
enum FusionStrategy {
    MaxScore,
    RRF { k: f64 },
    WeightedLinear { fts_weight: f64, vec_weight: f64 },
    Learned,
}

struct SearchPipeline {
    fts: FtsRetriever,
    vector: VectorRetriever,
    fusion: FusionStrategy,
    feedback: SearchFeedbackRepo,
    reranker: Option<Box<dyn Reranker>>,  // cross-encoder rerank
}

// 统一搜索结果（替换现有的两个 SearchHit 消费路径）
struct UnifiedSearchHit {
    message: Message,
    score: f64,
    headline: Option<String>,  // 现在真正生成
    matched_terms: Vec<String>,
}
```

核心变更：将 `search_click_events` 数据反馈到 `FusionStrategy` 的选择（或加权），形成闭环。这不是即时生效的，但建立管线架构比数据积累更重要。

**对现有系统的影响**：中高。涉及 `aero-storage/src/message/search.rs`（FTS/Vector 检索）、`aero-server/src/search.rs`（路由）、`aero-server/src/search_advanced.rs`（高级搜索）、`aero-ai/src/rerank.rs`（RRF）。但关键是**不改变数据库 schema**（`search_click_events` 已经存在），只改变消费逻辑。

---

### 方向 D（P1）：数据生命周期治理——消息分区 + 冷热归档 + 存储监控

**为什么需要**：
验证报告已经揭示了核心问题：`messages` 表未分区（shadow 表 `messages_partitioned` 休眠）、`message_edits` 和 `stream_gifts` 无清理。对于 IM 平台，messages 是最大且增长最快的表。没有分区意味着：DELETE 操作（软删清扫）触发的 VACUUM 压力会不断上升，最终导致 autovacuum 跟不上写入速率。

**核心挑战**：

1. **消息分区切换窗口**。迁移 0148 创建了 `messages_partitioned` 但未激活，文档注释说"这是有意的，需要维护窗口切换"。分区切换的关键难点是零停机——不能锁表，不能丢失正在写入的消息。需要 `pgroll` 或手写 `CREATE TABLE ... PARTITION BY RANGE` + 后台迁移 + 切换。
2. **冷热分离的定义**。什么算"冷"？已归档且超过 90 天的频道消息？已关停的工作区数据？冷数据是移到单独 PG 实例还是 S3 Parquet？存储格式是 JSONB 还是行格式？不同选择的恢复延迟差异很大。
3. **`message_edits` 的策略决策**。编辑历史保留多久？Slack 和 Discord 的做法是永远保留（法务/审计需求），但也可以设为 90 天滚动窗口。这个决策需要产品团队介入。

**预期的架构变更**：

```
// 分区管理
struct PartitionManager {
    template: PartitionTemplate,  // range/interval
    retention_policy: RetentionPolicy,
    archive_policy: Option<ArchivePolicy>,  // cold storage
}

// 存储监控
// 在 observability_gauge_samplers 中增加:
// - aero_storage_messages_total (counter)
// - aero_storage_messages_partition_size_bytes (gauge per partition)
// - aero_storage_archive_lag_seconds (gauge)
```

**对现有系统的影响**：中。分区迁移涉及 `messages` 表的 schema 变更（这是最敏感的表），需要完整的 rollback 计划和数据校验。其他变更（添加清理策略、存储监控）是增量且低风险。

---

### 方向 E（P2）：Webhook 递送管线——扇出模型重构

**为什么需要**：
当前架构中，`dispatch_event` 对每个启用 Webhook 的 workspace 执行同步 HTTP 调用，串行迭代。这造成两个实际问题：(1) 一个慢端点拖慢所有其他 Webhook 的递送；(2) 没有 per-hook 的速率限制和健康告警。验证报告已指出这不是"游标不推进"的问题，但"目标级串行"的实时性影响在 Webhook 数量增长时呈线性恶化。

**核心挑战**：

1. **per-hook 独立队列的容量管理**。每个 workspace × hook 一个 mpsc channel，进程内的 channel 数量可能膨胀到数千。需要 idle 回收（参考 login-throttle 的 idle sweep 模式）来防止内存泄漏。
2. **背压传播策略**。单个 hook 队列满了应该怎么做？丢弃最新事件（丢一保多）、阻塞上游发生器（反压到 bus 消费循环）、还是 spill-to-DB（降级到 retry loop）？不同策略对数据完整性的保证不同。
3. **监控触发条件**。断路器的开启/关闭状态、队列深度、延迟 P99——这些指标的告警阈值需要从线上数据校准。

**预期的架构变更**：

```
// Webhook 递送管线
struct WebhookPipeline {
    hooks: HashMap<HookId, HookSender>,
    dispatcher: Dispatcher,       // 从 bus 消费，按 hook fan-out
    retry_loop: RetryLoop,        // 已有，增强
    monitor: WebhookMonitor,      // 新增：per-hook 延迟/成功率
}

struct HookSender {
    queue: mpsc::Sender<WebhookEvent>,
    breaker: CircuitBreaker,      // 已有，增强
    rate_limiter: RateLimiter,    // 新增
    config: HookConfig,           // timeout, retry, backoff
}
```

**对现有系统的影响**：中高。涉及 `webhooks.rs` 的重构——从单任务同步迭代改为 per-hook 扇出。但只需要改这一个文件，不影响其他 crate。

---

## 3. 接口设计建议

### 3.1 关键接口设计原则

**原则一：扩展点是 trait，不是 enum variant**
当前 `Block` 是 enum，每次加 variant 改所有 match。对于方向 A（Block Kit 膨胀），应该引入 `RenderableBlock` trait + 注册表模式，让新组件可以插件式注册而不改核心枚举。

**原则二：搜索管线是 pipeline，不是 glue code**
当前搜索的融合逻辑散落在 routes.rs、helpers.rs、rerank.rs 中。应该抽象为 `SearchPipeline` trait，统一 FTS/Vector/Hybrid/Auto 模式的选择和执行。

**原则三：递送语义是 fan-out，不是 for-loop**
Webhook 递送、push 递送、bot 递送——这三个都是"收到事件→分发给多个下游"的模式。当前每个都用自己的方式做（Webhook 同步迭代、push bot 异步扇出、bot 直接调用）。应该抽象一个共同的 `FanOut<Target, Event>` pattern，至少统一错误处理和背压策略。

### 3.2 是否需要新的抽象层

**需要两个新抽象层**：

**① 消息组件注册表（方向 A）**

```
aero-common/
├── block/
│   ├── registry.rs    # BlockRegistry — 组件注册 + 版本协商 + alt_text 降级
│   ├── render.rs      # RenderableBlock trait + 内置组件实现
│   └── validation.rs  # 组件校验（长度/必填/类型约束）
```

不改为 `aero-block-kit` crate（过早分离），先在 `aero-common` 内以子模块收敛，待组件数超过 20 再抽离。

**② 统一检索管线（方向 C）**

```
aero-storage/
├── search/
│   ├── pipeline.rs    # SearchPipeline trait + 融合策略选择
│   ├── retriever.rs   # FtsRetriever / VectorRetriever / HybridRetriever
│   ├── fusion.rs      # MaxScoreFusion / RrfFusion / WeightedFusion
│   ├── highlight.rs   # 高亮片段生成（ts_headline + 向量上下文提取）
│   └── feedback.rs    # 点击反馈 → 加权策略的回路
```

### 3.3 向后兼容性策略

- **`Block` enum 不删不改，只加**。新组件以 `Block::Custom { ... }` 或注册表模式加载，核心枚举不变。新旧客户端通过 `BlockVersion` 协商——服务端存版本号，序列化时低版本输出 alt_text，高版本输出完整 payload。
- **搜索 API 不破坏现有路由**。现有 `POST /api/rooms/:id/search` 和 `POST /api/search` 继续返回现有格式，新字段（`headline`、`matched_terms`）可选追加。融合策略切换通过新的 query parameter `fusion=rrf|maxscore` 控制，默认不变（max-score）。
- **Webhook 递送接口不变**。per-hook 队列是内部实现重构，对外暴露的 HTTP 回调接口和重试行为不变。

---

## 4. 技术选型

### 4.1 方向 A（Block Kit）：不需要新技术栈

当前 `Block` enum 用 serde tag 序列化，WS 帧直接传递。新组件可以用同样的模式扩展，不需要引入 UI 框架或模板引擎。如果需要在服务端做组件渲染（例如为低版本客户端生成降级 HTML/Card），可以用现有的 `serde_json::Value` + 手写渲染函数，不需要 React 服务端渲染。

唯一可能需要的是 `validator` crate（已引入 `aero-storage`）的扩展使用——为 `SelectOption` 的 `value` 和 `label` 加长度校验是必要的。

### 4.2 方向 B（备份/DR）：评估 pgBackRest vs WAL-G

**选项一：pgBackRest**
- 优势：成熟、支持增量备份、并行恢复、checksum 校验、S3 原生支持
- 劣势：系统复杂度高（需要 stanza 配置、repo 配置、S3 凭证管理）
- 适用场景：TB 级以上、需要增量备份的部署

**选项二：WAL-G**
- 优势：更轻量、Go 实现（与 docker 生态亲和）、配置简单
- 劣势：功能不如 pgBackRest 丰富（特别是并行恢复和 delta restore）
- 适用场景：百 GB 级、小团队运维

**建议**：默认用 `pg_dump` + `cron`（50GB 以下勉强可用），同时提供 WAL-G 的 docker-compose 配置模板作为"增强备份"方案。pgBackRest 只建议在客户生产部署中由专业 DBA 配置。

### 4.3 方向 C（搜索 Learning-to-Rank）：不需要新 ML 基础设施

关键的架构决策是不引入外部 ML 基础设施：

- **点击反馈去偏**：用 PG 窗口函数计算 `ctr_stats`（MRR/top_result_ctr），不需要 Spark/Flink
- **特征工程**：`search_click_events` 表中的 `query_text` + `result_rank` + participant 维度足以做基本的 position-bias 校正
- **排序模型**：先用 heuristic（加权 RRF k 值按 CTR 调整），数据量够（>10^5 点击事件）后考虑 `lightgbm` rerank 模型，但这是方向 C 的第二阶段才需要

不需要引入 Elasticsearch——PG 的 pg_trgm + pgvector + FTS 对于消息搜索（非文档搜索）已经足够。Elasticsearch 的 ROI 在达到千万级消息之前是负的。

### 4.4 方向 D（冷热归档）：评估 TimescaleDB 模式 vs 自建

- **TimescaleDB 的压缩 + 自动分片**：如果是 new project，会推荐用 TimescaleDB 做消息表。但迁移已有 PG 17 到 TimescaleDB 的成本（extension+chunk 迁移）可能高于自建分区方案。
- **自建分区方案**：`messages_partitioned` shadow 表的设计已经做了正确的预判——按月/按工作室 ID range 分区。建议完成这个切换，而不是引入新的存储引擎。
- **冷存储格式**：Parquet over S3（用 `pg_parquet` 或 `COPY TO` + 外部 ETL）是标准方案。不推荐 JSONB dump（查询能力差）、CSV（schema evo 困难）。

### 4.5 自建 vs 采购决策矩阵

| 方向 | 选项 | 自建成本 | 采购方案 | 推荐 |
|------|------|---------|---------|------|
| Block Kit | 组件渲染框架 | 2-3 周（Rust 侧 + JS 侧） | 无合适的采购方案 | **自建** |
| 备份 | pg_dump + WAL-G | 1 周（脚本 + 配置） | Crunchy Bridge / pgBackRest 商业支持 | **自建（MVP）**，采购（生产） |
| 搜索排序 | ML reranker | 4-6 周（点击反馈管线 + 轻模型） | Algolia / Typesense（~$500/mo） | **自建**（数据不离开 PG） |
| 冷热归档 | S3 Parquet + 分区 | 3-4 周（迁移 + 归档管线） | TimescaleDB 商业版 | **自建分区**，暂不冷归档 |

---

## 5. 实施路线图

### 阶段划分

```
Q3 2026 (Jul-Sep)        Q4 2026 (Oct-Dec)        H1 2027 (Jan-Jun)
┌─────────────────┐     ┌─────────────────┐     ┌─────────────────┐
│ P0: Block Kit   │──▶  │ P1: Search LTR  │──▶  │ P2: Webhook     │
│ P0: Backup/DR   │     │ P1: Data Life   │     │     Backpressure│
│ (1.5 months)    │     │ (2 months)      │     │ (2 months)      │
└─────────────────┘     └─────────────────┘     └─────────────────┘
```

### P0 阶段（第 1-6 周）：基石加固

**方向 A（Block Kit）— 第 1-4 周**

| 周 | 交付物 | 风险 |
|----|--------|------|
| W1 | `BlockVersion` 协商 + `alt_text` 降级机制 | 客户端侧需要同时适配新旧版本——降级渲染在低版本客户端必须 100% 正确 |
| W2 | `BlockRegistry` + `RenderableBlock` trait + 表单组件（TextInput） | 组件验证规则的设计：长度/正则/必填——过严导致 AI 生成的消息频繁验证失败 |
| W3 | 布局组件（Section/Divider） + web SPA 渲染 | web `render.js` 的交互状态管理（select option 选中高亮、input 编辑状态） |
| W4 | 端到端集成测试 + 降级验证 | CI 需要模拟 V1 和 V2 客户端同时连接 |

**关键风险**：降级渲染的覆盖范围。如果现有 web 客户端（V1）收到包含 TextInput 的消息，降级为 `alt_text` 是保底方案。但新客户端（V2）降级到 V1 也需要优雅处理——例如在 V2 客户端上用 V1 的 JS bundle 看到的是乱码。需要 WS 握手时明确告知服务端支持的版本范围。

**方向 B（备份/DR）— 第 3-6 周**

| 周 | 交付物 | 风险 |
|----|--------|------|
| W3 | `pg_dump` cron 脚本 + `aero-cli backup` 子命令 | cron 调度的资源竞争——备份期间 PG 性能下降需要测试 |
| W4 | `aero-cli restore` + 恢复 runbook（Markdown） | 跨时间点恢复的依赖顺序（PG→Redis→NATS） |
| W5 | docker-compose.ha.yml（PG 流复制 + NATS `replicas: 3`） | 流复制的 WAL 磁盘空间消耗——如果 WAL 归档不配，磁盘会被撑爆 |
| W6 | 混沌工程实验 1：PG 主节点 kill + 自动故障转移 | 故障转移后的 Redis/NATS 连接重试逻辑是否平滑——需要验证 |

**关键风险**：恢复时间目标不明确。如果配置了流复制（异步），RPO 是秒级，RTO 是分钟级（故障转移）+ 小时级（全量恢复）。但如果只有 `pg_dump`，RPO 是 24 小时（cron 周期）。这个差距需要在路线图中明确说明。

### P1 阶段（第 7-14 周）：搜索与数据治理

**方向 C（搜索 LTR）— 第 7-11 周**

| 周 | 交付物 | 风险 |
|----|--------|------|
| W7 | 统一 `SearchPipeline trait` + 融合策略可配置化 | 现有 `POST /api/rooms/:id/search` 的 mode 参数需要同时兼容——不能 break 现有客户端 |
| W8 | `SearchHit.headline` 生成（`ts_headline` for FTS + 上下文提取 for vector） | 向量检索的头条生成需要回查原始消息文本——额外的 DB 往返成本 |
| W9 | 点击反馈回路：`record_click` → `SearchFeedbackRepo.ctr_stats` → 加权融合 | 反馈回路的延迟：点击数据是实时的，但加权更新应该是异步（sweep-based 或 event-driven） |
| W10-11 | 搜索分析 API（CTR dashboard）+ 可观测性（query latency P50/P99） | 分析数据的聚合窗口——按天/按周 MRR 趋势是否需要物化视图？ |

**方向 D（数据治理）— 第 11-14 周**

| 周 | 交付物 | 风险 |
|----|--------|------|
| W11 | `message_edits` 清理策略 + `stream_gifts` 清理 | 编辑历史的保留策略——需要产品决策 |
| W12 | `messages_partitioned` shadow 表 cutover 完成 | 零停机切表策略——需要 `pgroll` 或 `pt-online-schema-change` |
| W13 | 存储指标（每个分区大小、归档延迟）→ Prometheus | 分区爆炸警告——如果分区键选择不当，可能一个月产生 30+ 个分区 |
| W14 | 冷热归档策略文档（非代码） | 确定归档阈值和存储介质选择——留到 H2 实现 |

### P2 阶段（第 15-22 周）：递送管线优化

**方向 E（Webhook 背压）— 第 15-18 周**

| 周 | 交付物 | 风险 |
|----|--------|------|
| W15 | per-hook mpsc 队列（bounded, per-hook cap=100） | 队列容量耗尽时的行为——阻塞 vs 丢弃 vs spill |
| W16 | 自适应限流（基于响应时间滑动窗口） | 限流参数需要线上校准——beta 阶段需要配置开关 |
| W17 | 断路器告警（`WebhookMonitor` → Prometheus Alertmanager） | 告警风暴——一个异常 hook 不应该产生全体告警 |
| W18 | 事件优先级（`Message` > `Reaction` > `TypingIndicator`） | 优先级队列的 fairness——低优先级事件不应被饿死 |

### 风险矩阵与缓解

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| 消息分区 cutover 导致写入停机 | 中 | 灾难性 | 分阶段：先建 shadow 表 + 双写，再切换读，再切写 |
| 新 Block 组件在低版本客户端渲染异常 | 中 | 高 | 每个组件必须有 `alt_text`；CI 用 headless browser 验证 V1 降级 |
| 备份 cron 消耗 PG I/O 影响主业务 | 高 | 中 | 从 replica 备份而非 primary；`pg_dump --jobs=1` 限制并发 |
| Webhook per-hook 队列内存增长失控 | 中 | 中 | idle sweep + per-hook 硬上限 + 告警 |
| 点击反馈数据太少导致 LTR 权重不显著 | 高 | 低 | 保留 heuristic fallback（RRF/MaxScore），不依赖 ML 权重 |
| 冷热归档在消息量快速增长时跟不上 | 中 | 中 | 分区策略是第一防线，归档是第二防线——先确保分区没问题再考虑归档 |

---

## 总结

Aero IM 的架构整体上处于**良好水平**——事件驱动骨架正确、crate 分层清晰、AI 预算控制成熟、清理管线覆盖面在同类项目中属前 20%。五个方向中的"声明未覆盖"经过验证后，实际新颖性从 100% 到 65% 不等，但核心理念在修正后都是有效的。

最大的架构债务是两个**结构性断裂**：(1) 搜索融合的管线分裂（`merge_hits` vs `fuse_rankings`、`search_click_events` 数据无人消费）；(2) Webhook 的扇出模型（fan-out 被实现为 for-loop）。这两个问题的修复都会带来不成比例的架构收益，因为它们限制的不是当前功能，而是未来的扩展空间。

P0 阶段（Block Kit + Backup/DR）的建议顺序是务实的——前者提供产品差异化，后者提供企业信任基础，且两者互不依赖可以并行推进。P1 阶段的搜索管线统一需要更谨慎的迁移计划，因为涉及到多个 crate 的接口变更。P2 阶段的 Webhook 背压虽然是增量改进，但扇出模型的抽象化会为系统其他部分（push bot、bot dispatch）提供可复用的模式。

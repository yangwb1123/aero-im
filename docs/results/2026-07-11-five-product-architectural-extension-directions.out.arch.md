# 架构分析报告：Aero IM 平台

## 1. 架构评估

### 1.1 当前架构的核心优势

**事件驱动 + 进程内扇出的分层架构**已被验证为成功模式。通过代码验证可以确认：

- **事件 DAG 清晰**：三层归一（NATS 跨节点 → Hub 进程内广播 → WebSocket 客户端），每个阶段职责单一。`ImService::publish_room_event` → NATS `im.room.{id}` → `run_bus_listener` → `Hub::fan_out_raw` 的管线无循环依赖，可水平扩展。
- **crate 依赖方向正确**：自下而上从 `aero-common`（叶子）→ `aero-bus` / `aero-storage` → `aero-im-core` / `aero-ai` → `aero-server`（组合），无成环。这是 Rust workspace 治理的合理模式。
- **金融级安全设计哲学**：`unsafe_code = "forbid"` 是正确且有远见的决策，为后续金融/合规场景扫除了隐患。`ai_jobs` 的 `FOR UPDATE SKIP LOCKED` + `MAX_ATTEMPTS=5` → `dead` 的幂等工作队列模式成熟可靠。
- **迁移编译期嵌入**：`sqlx::migrate!("../../migrations")` 将迁移 bundle 进二进制，消除了部署时 SQL 版本漂移风险。
- **ACID 边界正确**：计费敏感的 `ChannelPointsRepo::redeem` 使用事务内 `WHERE balance >= cost RETURNING` 条件更新防止双重支出，这是正确的选择——比乐观锁更安全。
- **总线消费 bot 模式复用**：8 个 `agent_bot`/`ooo_bot`/`unfurl_bot`/`transcribe_bot` 等采用统一 durable consumer 模式，错误处理路径一致（fail-open log-skip），降低了运维成本。

### 1.2 识别出的架构薄弱点

#### 方向一：Canvas CRDT — 写入路径隔离但缺少客户端合并层

**当前状态**：`PUT /api/canvases/:cid` 覆盖 `blocks`（全量 JSONB），`POST /api/canvases/:cid/ops` 仅追加 op log。两者隔离是双向的——op 不更新 blocks，blocks 写入也不记入 op log。但这意味着：

- **服务端不保存完整文档状态**。`blocks` 反映的是最近一次 PUT 的值，不是所有 op 合并后的结果。新加入的协同者通过 `GET /api/canvases/:cid` 拿到的是"某次快照"，而不是文档的规范状态。
- **op log 是 append-only 的黑暗数据**。`ops_since` 返回 `DEFAULT=500` / `MAX=1000` 条 op，客户端需自行 CRDT 合并，但没有任何客户端 CRDT 代码存在（`rg -rn "crdt\|merge\|transform\|compose" --type js` 零匹配）。
- **这意味着 Canvas 是"保存时协作"而非"实时协作"**。不同客户端同时编辑时，最后 PUT 的 blocks 会覆盖前一个人的工作——这不是真正的协作文档编辑。

**严重度**：P0。这不是"等客户端做"的问题——当前 API 契约无法支持真正的多用户实时协作。op log 的 seq 唯一性和 gap-free 设计是正确的基座，但缺少服务端的合并/压缩/GC 机制，也缺少客户端的 CRDT 库。

#### 方向二：创作者货币化 — 支付处理器的缺席是产品缺口而非架构缺口

**当前状态**：`CreatorTier.price_cents`（`creator_subscription.rs:35`）和 `SubscriptionTier.price_cents`（`subscription_tier.rs:20`）都存在。`subscribe()`（`creator_subscription.rs:197`）直接 upsert，没有任何支付校验。全局 `grep "stripe\|paypal\|payment"` 返回 0。

**架构评估**：
- 这**确实是设计使然**而非意外缺口——支付校验是调用方的责任，订阅层作为"存储 upsert"是合理的分层。
- 但有两个问题：第一，没有任何 `payment_intent_id` / `checkout_session_id` 字段预留（迁移 `0051_creator_subscriptions.sql` 和 `0109_subscription_tiers.sql` 中都没有）；第二，没有支付状态机（pending / confirmed / failed / refunded）。
- 意味着如果要接入支付，将需要**添加迁移添加支付字段**，而非利用现有字段。这是可接受的折衷——支付提供商的 ID 格式各不相同，过早抽象可能更糟。

**严重度**：P1。不阻碍当前产品发布（可先做 free-tier 订阅），但要在支持付费订阅前添加支付集成。

#### 方向三：VOD 生命周期 — 缺少转码管线，但结构体设计正确

**当前状态**：`Vod` 结构体（`aero-common/src/model/media.rs:55-73`）不包含 `thumbnail_path`、`processing_status`、`transcoded_variants`、`file_size` 等字段。VOD 记录的只是"一个录制好的 HLS 播放列表的元数据快照"，不是"一个经过转码处理的媒体资产"。

**关键发现**：
- `finalize_vod` 路由存在且功能完整
- `list_stream_vods` 和 `list_room_vods` 存在
- 但 VOD 对象本身缺少媒体资产管理所需的字段：处理状态（`pending / processing / ready / failed`）、缩略图、多码率变体列表、文件大小、编解码器信息
- 这**不是缺口而是设计选择**：系统的设计意图是 HLS writer 已经在写入时就决定了最终质量，VOD 只负责生命周期管理。但因缺少转码管线，目前不支持"录制后转码"、"生成缩略图"等功能

**严重度**：P1。取决于产品需求——如果 VOD 只是"点播回放同一个流"，现有设计足够。但如果需要"录制后生成缩略图、多码率转码"则需要新建独立的转码管线（FFmpeg 子进程或微服务）。

#### 方向四：数据生命周期治理 — 分布式的清扫策略已足够，但缺少治理层抽象

**当前状态**：`retention.rs` 有 16+ 个独立 `sweep_*` 函数（确认：`sweep_expired_messages`、`sweep_ephemeral_messages`、`sweep_expired_bans` 等 + 2 个分区维护），每个独立配置于 `AERO__SERVER__*_RETENTION_DAYS` 环境变量，共用一个 `AERO__SERVER__RETENTION_SWEEP_SECS` 滴答（默认 3600s）。

**架构评估**：
- 分布式清扫函数是**务实的选择**：每个 sweep 函数独立于自己的表，可以单独调整策略和频率，不需要统一的数据类抽象层。
- 缺少的**三个要素**：第一，没有 Prometheus gauge 记录每次 sweep 的扫除行数、耗时、延迟；第二，没有"数据类"的概念，无法统一查询"这类数据的保留策略是什么"；第三，没有"合规保留"和"运营保留"的区分——法务保全使用 `NOT EXISTS` 子查询排除，这在线性扫描级别上是正确的，但策略不透明。
- 最值得关注的是**blob_gc_drain（GDPR 延迟擦除）使用独立 60s 固定间隔**，与其他 sweep 分离但遵循相同的 delete-then-ack 模式——这避免了长时间的图片清理阻塞消息保留的 sweep。

**严重度**：P1。功能完整但可观测性不足。添加 Prometheus gauge 应作为高优先级技术债。

#### 方向五：搜索反馈闭环 — record_click 有消费者，但 ctr_stats 是死代码

**当前状态**：
- `POST /api/search/click` 处理程序（`search_advanced.rs:169-208`）**确实调用了** `SearchFeedbackRepo::record_click`。端点安全：验证调用者对结果消息的房间有成员资格（防投毒），从消息的房间推导工作区（防伪造）。
- `SearchFeedbackRepo::ctr_stats` 方法存在但**生产代码中没有任何调用者**。`rg -rn "ctr_stats\|CtrStats" --type rust` 返回零匹配。
- 这意味着：点击数据被收集了，但从点击中学习的反馈回路**不存在**。没有定时任务聚合 CTR → 更新搜索权重 → 改善排名。

**架构评估**：
- `record_click` 有生产消费者这一点与报告不同——它使用正确（安全门控、术语归一化），**不是死代码**。
- 但 `ctr_stats` 的零引用是真正的架构债务。聚合层和排名调整层完全缺失。
- 当前的排名仅使用 `merge_hits` 的纯最大分数融合（`helpers.rs:21-44`），没有学习排序（LTR）、没有个性化信号、没有时效性衰减、没有流行度信号。
- 好消息是数据结构支持扩展：`search_click_events` 表有 `query_text`、`result_rank`、`participant_id`、`workspace_id`，所有 LTR 训练的素材已就绪，缺的只是聚合和权重调整。

**严重度**：P2。当前搜索体验可用但无自我改进能力。

### 1.3 关键设计决策合理性评估

| 决策 | 合理性 | 备注 |
|------|--------|------|
| NATS JetStream 作为跨节点事实源 | ✅ 正确 | Durable consumer + at-least-once 语义适合 IM 场景 |
| Hub 进程内 bounded mpsc 扇出 | ✅ 正确 | 避免了跨进程 N-to-N 扇出的 NATS 放大问题 |
| `participant_cache.invalidate` 读写分离 | ✅ 正确 | 减少 PG 读压力，写透后失效可接受 |
| 媒体 seam 标记（str0m/SfuMediaSession 已建未接） | ⚠️ 可接受 | 明确标记的 seam 好于未文档化的半成品 |
| 无统一支付抽象层 | ⚠️ 可接受 | 过早抽象可能更糟，迁移预留不足是提醒 |
| 无统一数据类治理层 | ⚠️ 可接受 | 灵活度更高，但可观测性缺失是短板 |
| Migration 编译期嵌入 | ✅ 正确 | 消除了部署时的 SQL 版本漂移风险 |
| `unsafe_code = "forbid"` | ✅ 前瞻性 | 为合规场景扫清隐患 |

## 2. 扩展方向

### 方向 A：Canvas 实时协作引擎（P0）

**为什么需要**：当前 Canvas 是"保存时协作"——同时编辑时后 PUT 覆盖前 PUT，op log 没有客户端消费。对于声称支持"协作文档"的产品，这是体验上的 P0 缺陷。

**核心挑战**：
1. **CRDT 选型**：Yjs（YATA CRDT）或 Automerge（RLE 列式存储）——前者是 Rust wasm 生态最成熟的（`y-octo`/`yrs`），后者更紧凑但 Rust 绑定不成熟。建议用 Yjs，通过 WASM 在浏览器端运行，服务端只做 op 存储和转发。
2. **op GC / 压缩算法**：op log 无限增长不可接受。需要服务端定时对收敛的 op 拍快照（snapshot）并清理早于快照的 op。快照可以存储到 `channel_canvases.blocks`。
3. **服务端 awareness**：多用户同时编辑时，服务端需要转发 op 给其他在线编辑者——当前缺少"有谁在编辑此 canvas"的 awareness 机制。

**预期的架构变更**：
- 新增 `aero-canvas-collab` crate 或嵌入 `aero-storage` 的 canvas GC 逻辑
- `PUT /api/canvases/:cid` 应废弃或改为"协作快照"语义（同时生成一个 snapshot op 追加到 op log）
- 服务端增加 op 广播（通过 Hub 或专用的 WebSocket room）
- 客户端新增 Yjs 依赖（WASM 形式的 `y-crdt`）

**对现有系统的影响**：
- 中等。op log 表结构（`canvas_ops`）和 seq 生成逻辑不需要改。需要的是：GC timer、snapshot 存储回 `blocks` 字段、awareness 通道。不影响已有 REST 路由的语义（写入路径会变，但 `GET /api/canvases/:cid` 仍返回 blocks）。

### 方向 B：支付处理集成（P1）

**为什么需要**：创作者订阅和礼物是收入来源。当前 `price_cents` 存储为原始整数且没有任何支付校验，意味着需要"订阅 → 付款 → 确认 → 激活"的闭环。

**核心挑战**：
1. **支付提供商选择**：Stripe（国际，成熟，Rust SDK 可选）vs 支付宝/微信（国内）。后续架构应考虑双轨制。
2. **支付状态机**：pending → confirmed → active → (renew/expire/cancel)。当前 upsert 模式无法表示这些状态。
3. **webhook 回写**：支付异步确认需要 webhook 端点和幂等消费（Stripe 事件重试机制要求幂等）。
4. **退款/争议**：订阅退款需要降级订阅或扣除信用点。

**预期的架构变更**：
- 添加 `payment_intent_id` / `checkout_session_id` / `payment_provider` / `status` 字段到 `creator_subscriptions`
- 新增 `aero-payment` crate（或轻量级嵌入 aero-server）：处理 Stripe webhook、checkout session 创建、订阅状态管理
- 支付状态机与现有 `subscribe()` / `unsubscribe()` 的路由集成
- webhook 端点的幂等键集成到现有 `webhooks` 路由

**对现有系统的影响**：
- 小到中等。现有存储层可复用（只增加字段），API 路径新增而不是改造。但需要仔细处理：如果先创建订阅再收付款，期间用户看到"已订阅"但无法实际使用付费功能的问题。建议采用"支付成功前不创建 active 行"的严格策略。

### 方向 C：VOD 转码管线（P1）

**为什么需要**：当前 VOD 只是 HLS 播放列表的快照——没有多码率适配、没有缩略图、没有处理状态。对于追求直播平台质量的系统，这是产品缺口。

**核心挑战**：
1. **FFmpeg 集成**：FFmpeg 子进程管理（超时、资源限制、队列）。Rust 的 `ffmpeg-next` crate（绑定）或调用子进程。建议子进程方式——更稳定，不与 FFmpeg 版本耦合。
2. **转码队列**：需要有界队列（类似 AiWorker 的 `Semaphore` 限并发），失败重试机制（`MAX_ATTEMPTS` → dead-letter）。
3. **进度/状态管理**：转码进度需要可观察（可选——简单系统可以在开始/完成/失败时更新一次状态，避免轮询反压）。
4. **缩略图生成**：需要在关键帧时间点截图。FFmpeg `select` filter 每隔 N 秒取帧，选择一个最佳帧作为缩略图。

**预期的架构变更**：
- 扩展 `Vod` 结构体（或新建 `VodAsset` 表）：增加 `status`（pending / processing / ready / failed）、`thumbnail_path`、`variants` JSONB、`error_message`、`transcoded_at`
- 新增转码 worker（类似 AiWorker 结构）：`transcode_jobs` 表 + `FOR UPDATE SKIP LOCKED` 轮询
- 添加 `POST /api/vods/:id/transcode`（触发转码）和 `POST /api/internal/transcode/callback`（FFmpeg 完成回调）
- 缩略图存储复用现有 `BlobStore`

**对现有系统的影响**：
- 中等。VOD 路由已存在，转码是附加功能，现有 `GET /api/vods/:id` 和 `list_*_vods` 不受影响。需要确保旧 VOD（没有转码的）仍然可播放。

### 方向 D：数据治理观测层（P1）

**为什么需要**：16+ 个独立 sweep 函数共享一个滴答但没有聚合可观测性。无法回答"当前保留了多少历史数据"、"GDPR 擦除队列积压"、"sweep 运行耗时"等问题。

**核心挑战**：
1. **统一度量收集**：每个 sweep 函数结束时更新 Prometheus gauge（扫除行数、耗时、总剩余行数）。需要跨 sweep 函数共享 metric registry。
2. **合规性报告**：需要 API 来呈现"每类数据的保留策略"（`GET /api/admin/retention-policies`）和"当前保留概览"（`GET /api/admin/data-usage`）。
3. **法务保全覆盖**：当前法律保全通过 `NOT EXISTS` 子查询排除，查询耗时随保全数量线性增长。需要评估是否引入 separate bucket 或 flagged 表。

**预期的架构变更**：
- 在 `aero-server/src/bin/boot/retention.rs` 中，每个 sweep 函数结束时更新 Prometheus gauge（可以使用 `metrics` crate 的 `Gauge` 和 `Histogram`）
- 可选：引入 `retention_policies` 表存储自定义策略（替代环境变量），但环境变量模式在初期足够
- 可选：添加 "待办" 数据治理 API 端点

**对现有系统的影响**：
- 小。纯附加的观测层，不改变任何 sweep 的语义。只需在 `metrics.rs` 注册新 metrics 并在 sweep 函数中引用。

### 方向 E：搜索排名反馈闭环（P2）

**为什么需要**：`record_click` 收集点击数据，但 `ctr_stats` 从未被调用。搜索排名从不从用户行为中学习。对于每日有数千次搜索的平台，这是浪费的信号。

**核心挑战**：
1. **聚合定时器**：需要一个定时任务（类似 `embedding_backfill` 的 300s interval）聚合 `search_click_events.CTR` 信号 → 调整词权重/文档流行度分。
2. **特征工程**：需要决定聚合窗口（默认 7 天）、衰减函数（指数衰减 ⇢ 更近的点击权重更高）、避免流行度偏见的偏差校正。
3. **评分集成**：当前 `merge_hits` 仅使用纯最大分数融合。需要将 CTR 信号作为额外评分因子集成到混合评分中。
4. **A/B 测试**：搜索排名的改动需要有可逆性。建议增加"排名配置版本"字段，支持按工作区 A/B 测试（可选）。

**预期的架构变更**：
- 新增定时任务：聚合 `search_click_events` → 更新每个文档的流行度分到 `messages.popularity` 或独立表 `message_search_signals`
- 修改 `merge_hits` 以纳入流行度信号（加权融合：基础分 × α + CTR 分 × (1−α)）
- 添加 `ctr_stats` API（为产品仪表板提供可见的 CTR 数据）
- 添加 `search_ranking_config` 表（可选，用于按工作区设定融合权重）

**对现有系统的影响**：
- 小到中等。`record_click` 路径不变，`merge_hits` 的改动是纯附加的加权叠加。增加聚合定时器无干扰风险。

## 3. 接口设计建议

### 3.1 关键模块接口设计原则

| 模块 | 当前状态 | 建议原则 |
|------|---------|---------|
| **Canvas API** | PUT blocks 与 POST ops 隔离，缺少协作语义 | 迁移到 "op-first" 模式：PUT → GET 返回 CRDT 合并后的规范文档；op log 只管增量 |
| **支付** | 无 abstract provider | 引入 `PaymentProvider` trait（`charge / refund / create_checkout / handle_webhook`），抽象 Stripe 和支付宝 |
| **VOD 转码** | 无状态 | 引入 `TranscodeJob` 状态机（pending→processing→ready|failed），通过 `TranscodeWorker` 消费 |
| **搜索排名** | 固定融合权重 | 引入 `RankingSignal` trait（BM25 / 向量相似度 / CTR 分 / 时效性），由 `RankingFusion` 组合 |
| **数据治理** | env 驱动的散落函数 | 引入 `RetentionPolicy` 结构体（`data_class: &str, retention_days: i64, legal_hold_exemptions: bool`），sweep 函数消费此配置 |

### 3.2 是否需要新的抽象层

- **支付抽象层**：**需要但不必急于引入**。建议在接入第一个支付提供商（Stripe）时定义 `PaymentProvider` trait，第二个提供商接入时自然形成抽象边界，避免提前复杂化。
- **搜索排名抽象层**：**需要**。当前 `merge_hits` 的纯最大分数融合已显不足。建议引入 `RankingSignal` trait 和 `RankingFusion` 组合器，使新增信号（CTR、时效性、个性化）时不改现有逻辑。
- **数据治理抽象层**：**可选**。16 个独立 sweep 函数模式可行，但如果有新的合规需求（如 GDPR 数据类导出/删除），引入 `DataClass` 枚举可能更优。

### 3.3 向后兼容性策略

- **Canvas**：旧 `PUT /api/canvases/:cid` 继续保持全量更新 blocks，但新增的协作模式下，"PUT 即 snapshot" 应该同时 push 一个 snapshot op 到 op log，保持 op log 的连续性和可压缩性。
- **支付**：现有 `subscribe()` 路由保持原语义（免费订阅），新增 `POST /api/creators/:id/subscriptions/pay` 付费订阅路由。两者共享 `creator_subscriptions` 表但通过 `payment_status` 区分。
- **VOD 转码**：现有 `GET /api/vods/:id` 继续返回 `Vod` 结构体（不转码）。扩展字段用 `#[serde(default, skip_serializing_if = "Option::is_none")]` 标记，旧客户端忽略未知字段。
- **搜索排名**：`merge_hits` 的排名权重融合通过配置参数控制（`ranking.alpha` / `ranking.beta`），默认值保持当前纯最大分数行为。
- **数据治理**：sweep 函数的环境变量配置保持向下兼容（新加 env 有默认值），`AERO__SERVER__*_RETENTION_DAYS` 的结构不变。

## 4. 技术选型

### 4.1 是否需要引入新的技术栈/框架

| 能力域 | 候选技术 | 建议 | 理由 |
|--------|---------|------|------|
| Canvas CRDT | Yjs (`yrs` Rust) / Automerge | **Yjs (`yrs`)** | Rust 生态最成熟，WASM 支持良好，社区活跃。Automerge 的 RLE 存储虽紧凑但在 Rust 侧绑定较弱 |
| WebSocket 扩展到 Canvas | 复用现有 `/ws` 连接 | **现有 WS 复用** | 当前 WS 连接支持 room 订阅，Canvas 协作可复用同一连接的不同 subject，无需新协议 |
| 支付 | Stripe SDK (`stripe-rust`) / 支付宝 | **Stripe** 作为`PaymentProvider` trait 的第一个实现 | 文档完整、Rust SDK 维护良好、webhook 成熟。支付宝 SDK 可通过单独 trait impl 加入 |
| VOD 转码 | FFmpeg 子进程 / `ffmpeg-next` crate | **FFmpeg 子进程** | 使用 `Command` + 超时控制，避免与 FFmpeg C API 版本绑定；子进程失败不会影响主进程 |
| 缩略图 | FFmpeg `select` filter / `image` crate | **FFmpeg select**（用现有子进程） | 不必引入新的 dependency，FFmpeg 的 `select` + `scale` + `fps` 可以完成截图 |
| 搜索排名 LTR | 无（纯规则融合）→ 可选引入 XGBoost/LightGBM | **暂不需要** | LTR 需要标注数据和训练管道。近期的产品需求用 CTR 信号 + 规则权重足够。当数据量 > 100 万次搜索/月时重新评估 |
| 数据治理可观测 | Prometheus client (`metrics` crate 已有) | **复用现有 metrics 注册表** | 无需新框架 |

### 4.2 第三方依赖评估标准

当前代码库的依赖管理是审慎的——`aero-live-whip` 和 `aero-live-webrtc` 单独声明 str0m，不在 root workspace 引入。以下是扩展时的评估标准建议：

1. **仅必需原则**：新 crate 应评估是否能复用现有依赖（例如缩略图生成完全可用 FFmpeg 子进程而非 `image` crate）
2. **版本锁定**：所有新依赖加入 workspace `[workspace.dependencies]` 统一版本号，不允许直接在各 crate 中声明
3. **许可证兼容**：MIT/Apache-2.0 优先，AGPL 不可用（违反项目 license）
4. **Rust 生态成熟度**：
   - 首选：纯 Rust 实现且活跃维护（GitHub last commit < 6 个月）
   - 次选：核心系统调用绑定且稳定（如 `rusqlite`、`aws-sdk-rust`）
   - 避免：wrapper 绑定到快变动的 C 库（除非版本锁定非常保守）
5. **构建影响**：编译时间增长 > 30 秒的依赖应标记为可选 feature gate（类似 `aero-ai` 的 `OPENAI_API_KEY` gate）
6. **安全审计**：新依赖应 `cargo audit` 通过，不接受有已知 CVE 的版本

### 4.3 自建 vs 采购决策

| 能力域 | 自建 | 采购（第三方） | 推荐 | 理由 |
|--------|------|--------------|------|------|
| Canvas 协作 | Yjs 集成 | 接入 Google Docs API / Lark Docs | **自建（Yjs）** | 数据主权；与现有 Canvas 数据模型集成；第三方 API 有调用成本和延迟 |
| 支付处理 | 自建抽象层 + Stripe SDK | Stripe Checkout（托管页面） | **混合**：Stripe Checkout 页面（采购） + 自建 webhook 处理（自建） | Checkout 页面降低 PCI 范围，webhook 处理是必要的业务逻辑 |
| VOD 转码 | FFmpeg 子进程 | Mux / Cloudflare Stream / AWS Elemental | **自建（FFmpeg 子进程）** | 成本（第三方多码流转码成本高）；控制力（自定义 HLS 配置）；数据本地化 |
| 缩略图 | FFmpeg 子进程（复用到 VOD 转码） | Cloudinary / Imgix | **自建** | 无额外依赖，复用现有 FFmpeg 子进程 |
| 异地容灾 | 多 Region NATS + PG 流复制 | AWS Aurora Global Database | **采购**（当需要时） | PostgreSQL 集群运维复杂度高；初期单 Region 足够 |
| 推送通知 | 自建 FCM/APNs 网关（已建 `aero-push`） | Firebase / AWS SNS | **已建 → 继续使用** | `aero-push` 已有正确的 token-provider seam + `FakeGateway`，保持自建 |

## 5. 实施路线图

### 优先级排序

```
P0: Canvas 实时协作引擎     ← 产品体验缺口，当前承诺了"协作"但无法实现
P1: 支付处理集成            ← 创收能力，但可先发免费订阅
P1: VOD 转码管线            ← 产品完整性，取决于产品路线
P1: 数据治理可观测性        ← 合规 + 运维，技术债
P2: 搜索排名反馈闭环        ← 产品迭代，当前搜索可工作但无自我改进
```

### 阶段划分

#### 第一阶段（P0，4-6 周）：Canvas 协作化

**里程碑**：浏览器端双人同时编辑同一 Canvas，实时看到对方的光标和操作

- **第 1-2 周**：Yjs 集成
  - 在 web/ 引入 `yjs` + `y-websocket`（或自定义 provider）
  - 构建 opt log → Yjs 文档合并的 WASM 桥
  - 验证 op log + Yjs doc 的双向同步
- **第 3-4 周**：服务端 awareness + op GC
  - 新增 canvas 协作 WebSocket subject（复用现有 WS 连接，新增 `canvas:awareness` 帧类型）
  - 实现 op GC 定时器：当 op log 中的 op 数量超过阈值（如 10000），拍快照到 `blocks` + 清除早于快照的 op
  - `PUT /api/canvases/:cid` 改为推送 snapshot op 而非直接写 blocks（或保留旧语义 + 新端点 `POST /api/canvases/:cid/snapshot`）
- **第 5-6 周**：测试 + 文档
  - 多客户端端到端协作测试（CI 排除，需真实浏览器）
  - 更新 API 文档，标记旧 `PUT` 行为的兼容策略

**风险点**：
- Yjs 的 `yrs` Rust crate 可能缺少 WASM 绑定——**缓解**：使用 npm 包 `yjs` + `y-websocket`（JS 端），服务端只透传 op，不做 CRDT 合并
- op GC 快照与当前 blocks 语义冲突——**缓解**：快照时写入 `blocks` 字段可保证 `GET` 返回规范状态，与 op-first 模式不矛盾

#### 第二阶段（P1，3-4 周）：支付处理集成

**里程碑**：用户可以为创作者订阅付费，支付处理后激活订阅

- **第 1-2 周**：Stripe 集成
  - `POST /api/creators/:id/subscriptions/checkout` → 创建 Stripe Checkout Session → 返回 URL
  - `POST /api/internal/stripe/webhook` → 处理 `checkout.session.completed` → 激活订阅
  - `POST /api/creators/:id/subscriptions/portal` → 创建 Stripe Customer Portal Session（管理订阅）
- **第 3 周**：状态机 + 幂等
  - 在 `creator_subscriptions` 表添加 `status`, `stripe_subscription_id`, `current_period_end`
  - 订阅状态机：`pending` → `active` → `past_due` / `canceled`
  - webhook 幂等键集成到 `SearchFeedbackRepo` 模式（`ON CONFLICT DO NOTHING`）
- **第 4 周**：退款处理 + 测试
  - `subscription_schedule.upcoming` / `customer.subscription.deleted` 等 webhook 事件处理
  - 测试 Stripe 测试模式的全流程
  - 文档 + 配置示例

**风险点**：
- Stripe webhook 安全——**缓解**：使用 Stripe `WebhookEndpoint` secret 验证，通用 webhook 路由已有安全设计可复用
- 支付 + 订阅状态的不一致——**缓解**：事务性处理 webhook（先更新订阅状态 + 记录 webhook 处理日志在同一事务）

#### 第三阶段（P1，4-6 周）：VOD 转码管线

**里程碑**：录制完成的直播流自动转码并生成缩略图

- **第 1-2 周**：转码队列
  - 新建 `transcode_jobs` 表（`vod_id`, `status`, `progress`, `error`, `created_at`, `started_at`, `completed_at`）
  - 实现 `TranscodeWorker`（类似 AiWorker：`FOR UPDATE SKIP LOCKED` + `MAX_ATTEMPTS=3` → dead-letter）
  - 扩展 `Vod` 结构体：`status`, `thumbnail_path`, `variants` JSONB
- **第 3-4 周**：FFmpeg 集成
  - `Command::new("ffmpeg")` 子进程管理（超时 30 分钟、stdout/stderr capture、cancel token）
  - 转码 pipeline：输入 HLS → 输出多码率 HLS（1080p/720p/480p）+ 缩略图
  - 转码完成后更新 `Vod` 状态 + 存缩略图到 `BlobStore`
- **第 5-6 周**：API 集成 + 自动触发
  - 流结束时自动触发转码（在 `stream_end` handler 中调用 `finalize_recording` 之后，触发 `enqueue_transcode`）
  - 扩展 `GET /api/vods/:id` 返回缩略图 URL
  - FFmpeg 不存在时的优雅降级（VOD 保持原样，不转码，`status = "skipped"`）

**风险点**：
- FFmpeg 子进程资源消耗——**缓解**：`Semaphore` 限制并发转码数（默认 2），子进程 cgroups（可选）
- 转码后的播放兼容性——**缓解**：转码完成后进行元检查（检查 `.m3u8` 和 `.ts` 文件存在），播放 URL 不变

#### 第四阶段（P1，2-3 周）：数据治理可观测性

**里程碑**：运维仪表板可查看每类数据的保留策略 + 当前扫除状态

- **第 1 周**：Prometheus metrics
  - 为每个 sweep 函数添加 gauge：`retention_sweep_rows_total`（扫除行数）、`retention_sweep_duration_seconds`（耗时）、`retention_sweep_stale_days`（最旧留存数据的时效）
  - 添加 `retention_table_size_bytes`（PG 表大小）
- **第 2 周**：API
  - `GET /api/admin/retention/policies` → 返回所有 sweep 配置
  - `GET /api/admin/retention/stats` → 返回当前 sweep 状态
- **第 3 周**：法务保全性能
  - 评估当前 `NOT EXISTS` 子查询的性能（pg_stat_user_tables 中的 seq scan 计数）
  - 如发现性能瓶颈，加入 `legal_holds` 的 `room_id` 索引优化法务排除查询

**风险点**：无显著风险。纯附加测量，不改变 sweep 语义。

#### 第五阶段（P2，3-4 周）：搜索排名反馈闭环

**里程碑**：搜索排名吸纳点击数据，CTR 信号加权到搜索结果

- **第 1-2 周**：聚合定时器
  - 新增 `messages.popularity` 列（`REAL DEFAULT 0.0`）或独立表 `message_search_signals`
  - 定时任务（300s 固定间隔，与 `embedding_backfill` 相同模式）聚合 `search_click_events` → 计算每篇文档的加权 CTR 分
  - 使用指数加权（最近点击权重更高）：`weight = e^(-days_old / half_life)`
- **第 3 周**：排名融合
  - 抽象 `RankingSignal`：BM25 分（已有 FTS） + 向量相似度（已有 pgvector） + CTR 分（新增） + 时效性分（新增 `RECENCY_DAYS` 衰减）
  - 修改 `merge_hits`：`final_score = bm25_score * α + vector_score * β + ctr_score * γ + recency_score * δ`
  - 默认权重保留当前行为（`α=1, β=1, γ=0, δ=0`），通过 `AERO_SEARCH_RANKING_*` env 启用 CTR 信号
- **第 4 周**：CTR 仪表板 + 文档
  - `GET /api/admin/search/stats` → 返回 CTR / MRR / 热门搜索
  - 更新搜索 `README` 文档
  - `ctr_stats()` 在 `search_feedback.rs` 中不再死代码

**风险点**：
- CTR 的偏差校正——热门文档天然获得更多点击，可能使"哈利·波特效应"恶化排名。**缓解**：使用 `click_through_rate / average_ctr` 进行归一化，而非原始点击计数
- 排名不可解释——**缓解**：保留 `merge_hits` 的日志输出（`info!("search result: id={}, signals={:?}", msg.id, signals)`），可调试排名原因

### 总路线图时间线

```
阶段一：Canvas 协作化           P0     4-6 周
阶段二：支付处理集成            P1     3-4 周  ─┬─ 可与阶段一并行
阶段三：VOD 转码管线            P1     4-6 周  ─┤
阶段四：数据治理可观测性        P1     2-3 周  ─┘
阶段五：搜索排名反馈闭环        P2     3-4 周  ← 阶段四完成后
```

**并行策略**：
- 阶段一（Canvas）与阶段二（支付）可以**完全并行**，团队分成两个独立工作组
- 阶段三（VOD）依赖于阶段一的架构决策，但不直接阻塞——可在阶段一启动 2 周后开始
- 阶段四（数据治理）与其余所有阶段**无依赖**，随时可启动
- 阶段五（搜索）依赖于阶段四的可观测性基座，建议在阶段四完成后启动

**关键依赖 checkpoints**：
- 阶段一：`yrs` / `yjs` 选型确认 → 开始第 1 周
- 阶段二：Stripe API key 获取 + webhook endpoint 注册 → 开始第 1 周
- 阶段三：FFmpeg 可用性确认 + 测试流资源 → 开始第 1 周
- 阶段四：Prometheus 注册表确认 `metrics` crate 可用 → 立即
- 阶段五：阶段四的 `retention_sweep_*` metrics 作为聚合定时器的依赖 → 开始第 1 周

### 风险点和缓解策略总结

| 风险 | 涉及阶段 | 概率 | 影响 | 缓解策略 |
|------|---------|------|------|---------|
| Yjs WASM 绑定在 Rust 侧不成熟 | 阶段一 | 中 | 高 | 走 JS 端 `yjs` + `y-websocket`，服务端只做 op 透传。WASM 桥是可选优化而非必须 |
| 支付 webhook 丢失/重放 | 阶段二 | 低 | 中 | Stripe webhook 签名验证 + 幂等键（`Idempotency-Key`）+ 事务性处理 |
| FFmpeg 子进程 OOM | 阶段三 | 中 | 中 | `Semaphore` 限制并发 + `prctl(PR_SET_SECCOMP)` sandbox + cgroup（可选） |
| VOD 转码后 playlist 路径变化导致旧 URL 失效 | 阶段三 | 低 | 高 | 保持 `hls_path` 不变，转码后重写相同路径下的文件 |
| 法务保全 `NOT EXISTS` 查询性能退化 | 阶段四 | 中 | 中 | 评估添加 `legal_hold_room_idx` 索引 | 
| CTR 归一化不足导致搜索偏见 | 阶段五 | 中 | 中 | 引入贝叶斯平均（Bayesian Average）进行平滑，避免低曝光文档被过度惩罚 |
| 排名融合权重调整需要 A/B 测试 | 阶段五 | 低 | 低 | 早期用 env 控制，未来加入 `search_ranking_config` 表支持按工作区设定 |

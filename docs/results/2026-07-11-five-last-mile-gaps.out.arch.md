现在我有了全面的了解。这是架构分析。

---

# 架构分析：Aero IM「最后一公里」生产级缺口

## 1. 架构评估

### 1.1 优势

该代码库展示了一个异常成熟的架构，在多个方面做出了正确的折衷：

| 领域 | 已做好的决策 |
|---|---|
| **分层** | 清晰的从下到上的 crate 依赖树（`common` → `bus` → `storage` → `auth` → `im-core` → `ai` → `server`）。没有循环。叶子 crate 很小。 |
| **事件总线** | NATS JetStream 持久消费者（`aero-server`）用于房间事件（恰好投递一次语义）；临时消费者（`run_live_bus_listener`）用于直播流（可接受丢失）。进程内 `Hub` mpsc 扇出被明确限制和隔离。 |
| **功能模块化** | 每个 HTTP 功能都是一个独立的 `routes()` 函数，生成 `Router<AppState>`，通过 `.merge()` 进行组合。仓储是内联构造的（`XRepo::new(s.pg.clone())`），无需向 `AppState` 添加字段。这个模式在新功能上高度一致且可靠地复制。 |
| **TTL 缓存** | `participant_cache` 和 `room_member_cache` 处于正确的位置：在热路径（推送扇出、提及解析）上，对只读数据的 DB 往返次数最多。失效点在所有写入路径上都被覆盖（`update_me` → `invalidate`）。 |
| **安全性** | 内容嗅探（MIME 一致性检查）+ ClamAV 扫描构成附件上传的多层防御。每个附件下载的 IDOR 守卫是正确且必要的。SDP/ICE 验证在呼叫信令上。 |
| **可观测性** | 整个代码库中的指标计数器（`MESSAGES_SENT_TOTAL`、`MESSAGES_EDITED_TOTAL`），可选的每租户指标，带有 span 传播的 W3C traceparent 跨 NATS 边界。 |
| **版本化编辑** | 消息编辑使用乐观并发（`version`列 + `expected_version` 参数）。`edit` SQL 设置 `embedding = NULL`*并*同步入队一个 `AiJobKind::Embed`——验证报告中的方向四错误已被代码证伪。 |
| **可配置的公平性** | 跨租户的 WebSocket 速率限制、每个工作区的 AI 预算、有界 `typing` 总线发射（尽管缺少聚合）。 |

### 1.2 局限性

分析通过验证揭示了五个核心限制。按严重程度排序：

**L1 — 可扩展性（P1）**

1. **读后写（RYW）缺口**。在 `pg_read` 上运行的三条路径（搜索、分析、AI 使用计数）在部署读取副本后对自身写入返回过时数据。没有 `RecentWriterCache`、没有副本滞后等待、没有监控指标。这是一个无声的正确性问题——不会崩溃，但会产生令人困惑的 UX（“我的消息为什么没有出现在搜索结果中？”）。

2. **附件平台缺乏生产化**。没有按用户的配额（`SUM(size) WHERE owner_id`）、没有用于内容类型管线的缩略图生成/视频转码、没有 `Cache-Control`/`ETag`（在重复下载时浪费带宽）、没有预签名 URL（对 S3 后端造成代理压力）、没有临时分享链接（迫使所有附件通过登录墙）。

**L2 — UX 完整性（P2）**

3. **草稿版本化**。草稿是一次 upsert。设备 A 开始输入 → 设备 B 保存不同的正文 → 设备 A 保存 → B 的工作丢失。没有冲突检测、没有合并策略、没有版本列。用户只有在两个浏览器标签页中打开同一个房间时才会遇到这个问题，但一旦发生就很烦人。

4. **打字指示器缺乏控制**。每个 `Typing { on: true }` / `Typing { on: false }` 帧都作为单个事件广播到房间内每台在线的成员机器。没有每成员速率限制（防止洪泛）、没有超时（让卡住的“正在输入…”过时）、没有聚合（在 1000 人频道中退化为“N 人正在输入”，而不是按房间的布尔聚合）。

**L3 — 运维（P2）**

5. **嵌入/FTS 索引生命周期**。编辑路径确实更新了嵌入（代码证据证明报告错误），但缺少：无 `embedding_model` 列（模型更改后，旧嵌入变为垃圾，无法检测）、HNSW 索引无声退化（无 `REINDEX` 策略、无监控）、无 CLI backfill 命令、FTS 分析器更改需要手动 `UPDATE` 来重新生成存储生成的列。

### 1.3 架构债务

这些是逐渐累积的设计债务，而不是需要立即重构的阻塞问题：

- **推送机器人未纳入 /ready**。`/ready` 探测 `pg/redis/nats/blob`，但不包括 `push_bot`。推送机器人故障只有在用户抱怨时才被注意到。
- **通知去重是每个机器人幂等性的，不是每个频道的**。`push_bot` 的 `ON CONFLICT DO NOTHING` 阻止每个目标设备的重复推送。但它不阻止通知中心 + 电子邮件 + 推送都传递相同的内容——没有 `channel_delivered` JSONB 列、没有 `source_path` 标签、没有统一的已读回执。
- **嵌入回填是尽力而为的**。`background.rs` 每 300 秒扫描 200 条无嵌入的消息。它受下游 AI 预算约束。没有进度跟踪、没有优先级、没有 SLA。对于首次部署可以，但不能满足用户期望“在几秒钟内可搜索”的生产环境。
- **打字在总线上是每个实例转发的**。运行 `typing` 帧通过总线（NATS）并扇出到 Hub，但每节点每房间没有聚合层。一个 1000 人的频道每秒生成 1000 个总线消息。

---

## 2. 扩展方向

### 方向 A：读后写一致性层（P0）

**为什么需要**：今天，搜索永远不是即时的。分析仪表板可能是错的。AI 使用统计数字可能落后。在拥有读取副本的单节点部署中，这三条路径提供陈旧数据。这不是崩溃，而是一种会削弱用户信任的正确性问题。

**核心挑战**：
- 如何保护副本读取路径，而不强制每条读取都走通过主库？
- 何时我们需要等待副本追赶（等待），何时只需要接受最终一致性？
- `RecentWriterCache` 如何工作：当用户执行写入时，我们记住 `(writer_id, time)`。在后续读取中，如果它们自己的写入是最近的（< 滞后阈值），我们将该读取路由到主库（或等待）。这是一个简单的检查，但对正确读取至关重要。

**预期的架构变更**：

```
                    ┌──────────────────────────────┐
                    │          API Handler          │
                    │  (search / analytics / usage) │
                    └──────┬───────────┬────────────┘
                           │           │
                    ┌──────▼──┐  ┌─────▼──────┐
                    │ pg_read │  │    pg      │
                    │ (replica)│  │ (primary)  │
                    └─────────┘  └────────────┘
                           ▲
                    ┌──────┴──────┐
                    │ RYW Guard  │
                    │ (recent_writer?) │
                    └─────────────┘
```

1. 添加一个 `RecentWriterCache`（TTL 缓存，如 `HashMap<ParticipantId, Instant>`）。在每次写入操作后调用 `cache.record_write(pid)`。
2. 在搜索/分析/AI 使用入口点，如果`cache.is_recent_writer(pid)`，则路由到 `pg`（主库）而不是 `pg_read`。
3. 添加一个 `replica_lag_seconds` 仪表盘，以便在开发/监控中可见滞后。
4. 对 `/ready` 探测没有影响——这是一个微妙的选择：`RecentWriterCache` 不需要检查副本滞后，它只是将最近的写入者尽可能多地路由到主库。

**影响**：3 个处理程序被修改（搜索、分析、AI 使用）。影响范围很小。三个模块各自的 `s.pg_read.clone()` 变为一个有条件的 `if cache.is_recent(auth.pid) { s.pg.clone() } else { s.pg_read.clone() }`。

### 方向 B：WAN 友好的附件平台（P1）

**为什么需要**：在当前设计中，每个附件下载都经过应用服务器代理。对于 S3 后端，这意味着每次读取都要进行服务器端下载+重新上传。对于具有多个实例的生产部署，这放大了延迟、服务器负载和出口成本。预签名 URL 允许客户端直接从 S3 读取。缩略图生成降低了移动上传者的带宽成本。

**子项目**：

| 子项目 | 幅度 |
|---|---|
| B1：按用户的存储配额 | 小（1 个新仓储方法 + 中间件） |
| B2：`Cache-Control`/`ETag` | 小（4 行响应头） |
| B3：S3 预签名 URL | 中（新存储 trait 方法 + 可切换路线） |
| B4：过期分享链接 | 中（新模式：带令牌的 `/s/:token`） |
| B5：缩略图管线 | 大（新 crate / FFmpeg 子进程 / 懒生成） |
| B6：视频转码 | 大（复用 `aero-live-hls` 管线） |

**最小可行方案（B1+B2+B3）** 可以分拆到一次冲刺中：为 `BlobStore` trait 添加 `presigned_get()`，为 S3 后端实现，为路由添加 `?download=1` 参数以选择预签名 vs. 代理。配额可以通过一个中间件实现，中间件在 `PUT` 之前检查 `SUM(size)`。

**核心挑战**：
- 预签名 URL 对 `BlobStore` trait 意味着什么？如果后端是 LocalFs 而非 S3，它们就没有意义。
- 预签名 URL 需要在 S3 端设置 CORS——这是部署任务，而不是代码任务。
- 缩略图：生成是昂贵的（FFmpeg 子进程），所以必须懒生成（在第一次 GET 时）并缓存。

### 方向 C：打字指示器——速率限制、超时和聚合（P1）

**为什么需要**：当前的设计是每个打字事件、每连接地广播到整个房间。在 1000 人的房间里，这会产生 1000 个总线消息/秒/人打字。如果有 5 个人同时打字，房间每秒收到 5,000 个总线消息。Web 客户端通过 `textarea` 上的 `oninput` 触发，每 300 毫秒就会产生一个帧。这是 DoS 放大器。

**解决方案**：

1. **每成员速率限制**：每个 `(pid, room)` 每 3 秒最多 1 个 `Typing` 帧。丢弃中间帧。这是服务器端的——客户端仍以 300 毫秒的间隔发送；服务器静默节流。
2. **超时**：在收到 `Typing{on:true}` 后，如果在 10 秒内没有收到 `Typing{on:false}`，则从 Hub 的 typing 集合中过期该用户。
3. **聚合**：不要在打字总线上广播 `(pid, room, on)`——在 Hub 内部维护一个 `typing_set: HashSet<(RoomId, ParticipantId)>`。定期（每 2 秒）广播 `(room_id, count)`。如果 count > 0，客户端显示“N 人正在输入…”。如果 count == 0，隐藏指示器。

**关键不变量**：聚合必须发生在**每个节点**，而不是总线上。NATS 不聚合打字——它只中继入站帧。Hub 在其每个房间的 typing set 上运行一个节拍的 tick。

**影响**：更改位于 `ws_impl/frame.rs` 和 `hub.rs`。NATS `room.*` 模式没有变化——打字帧仍然由 NATS 中继。只有 Hub 的扇出逻辑发生变化。

### 方向 D：嵌入索引生命周期管理（P2）

**为什么需要**：今天，嵌入在编辑时被正确重新计算。但是：

- 没有模型版本列 → 模型升级使所有旧嵌入失效，而未标记它们。
- HNSW 索引无声退化 → `REINDEX` 是粗粒度的，但仍然缺失。
- 没有 `aero-cli index backfill-embeddings` 命令 → 运维只能通过编辑来重填。

**设计**：

1. 向 `messages` 添加 `embedding_model` `VARCHAR` 列（或存储在元数据/配置中）。当 `current_model != stored_model` 时，`list_without_embedding` 进行匹配（通过 `WHERE embedding_model IS NULL OR embedding_model != $1`）。
2. 添加 `aero-cli index reindex-hnsw` 命令，运行 `REINDEX INDEX CONCURRENTLY`（在 PG 13+ 上，`CONCURRENTLY` 允许在后台重建索引时进行读取）。
3. 添加对 `INDEX_SIZE_BYTES` 的监控（如果索引大小膨胀到基线的 150% 以上，则发出警报）。
4. 使 `embedding_backfill` 扫描可控（`AERO_EMBEDDING_BACKFILL_BATCH` 环境变量）。

**警告**：`REINDEX CONCURRENTLY` 在事务外运行——`sqlx` 默认在事务内迁移。因此 `aero-cli` 命令必须使用 `sqlx::PgConnection` 的 `execute` 而不是迁 migration runner。

### 方向 E：多通道通知去重与统一回执（P2）

**为什么需要**：通知系统可以通过以下渠道发送相同的内容：通知中心（WebSocket 帧）、推送（FCM/APNs）、电子邮件。没有跨通道的去重。如果用户在三台设备上收到推送，并在其中一台设备上阅读，其他设备不会清除其通知。

**设计**：

1. 添加 `notification_channels` JSONB 列来跟踪发送状态：`{"push": true, "inbox": true, "email": false}`。机器人发送后更新。
2. 添加统一的 `read_receipt` 概念：当用户在任意通道（通过 WS、REST 或推送反馈）上标记消息为已读时，所有通道都反射此状态。
3. 在 `push_bot` 中，在发送前检查 `channel_delivered`——如果已发送，则跳过（已由 `ON CONFLICT` 幂等性覆盖，但 channel_delivered 为日志/审计增加了明确的语义）。
4. 添加 `market_as_read` 端点，该端点同时作用于通知和推送令牌，以便跨通道清除。

**核心挑战**：推送机器人是尽力而为的（失败时静默跳过）。统一回执不能依赖于推送确认——它必须在数据库提交。因此，回执流程为：WS `MarkRead` → 更新数据库 `read_receipts` → 后台同步到推送令牌。每个通道独立读取 `read_receipts` 以确定是否已阅读。

### 方向 F（长远）：草稿版本冲突（P3）

**为什么需要**：在当前设计中，草稿是一个 upsert——没有版本，没有冲突。用户 A 在设备 1 上开始输入 → 切换到设备 2 并输入更多内容 → 切换回设备 1（仍在页面 A 上）并保存 → 设备 2 的工作丢失。

**设计**：加一个 `version` 列到 `message_drafts`。`upsert` 变为 `INSERT ... ON CONFLICT ... WHERE version = $6`。如果版本不匹配，返回 `409 Conflict`。客户端的选择：覆盖（“保留我的”）或放弃（“获取服务器版本”）。

**这值得吗**？草稿冲突很少发生——用户保存草稿的频率远低于发送消息。这是 P3。但如果 Slack 风格的草稿是核心产品功能，则值得优先考虑。

---

## 3. 接口设计建议

### 3.1 当前模式足够好——不要引入新抽象

当前的布局——每个模块一个 `routes()` 函数，返回 `Router<AppState>`，仓储内联构造——很简单，很容易推理，并且已经存在于代码库中的所有模块中。**不要引入新的抽象层**。不需要“RepositoryFactory”或“DependencyInjector”——`XRepo::new(s.pg.clone())` 模式已经在一个 `Arc<PgPool>` 包装周围便宜且可克隆。

### 3.2 为新关注点添加的 Trait seam

有一些地方新的 trait 有助于测试/灵活性：

| Seam | 原因 |
|---|---|
| `BlobStore` trait 上的`presigned_get(url, duration)` | S3 后端需要它；LocalFs 可以返回 `Err(Unsupported)`。注意：返回 `Url` 而不是 `Bytes` 会改变调用者的返回类型（重定向 vs. 代理）。将其作为 `blob_download` 中的可选路径添加——如果 `blob_store.presigned_get()` 返回 `Ok(url)`，则发出 `307 Temporary Redirect`。 |
| `RecentWriterCache` trait | 用于测试（在测试中注入确定性行为）和可能的替代实现（Redis-backed 用于跨实例可见性——尽管 `participant_id`→`last_write` 是每个进程的，这对 RYW 来说已经足够好了）。 |
| `MediaPipeline` trait | 用于缩略图/转码。一个 `FfmpegPipeline` 实现（子进程）和一个 `NoopPipeline`（返回输入 URL）用于没有 FFmpeg 的部署。 |

### 3.3 向后兼容性

所有提出的变更都是附加的：

| 变更 | 兼容性 |
|---|---|
| `embedding_model` 列 | `ADD COLUMN ... DEFAULT NULL`。现有行逐步更新。 |
| `Cache-Control` 头 | 向 blob 响应添加头——浏览器尊重它们，但客户端无需更改。 |
| 预签名 URL | 使用 `?presigned=true` 查询参数或新的端点 `/api/blobs/:id/download`。旧的 `GET /api/blobs/:id` 继续作为代理。 |
| 打字聚合 | 改变 WebSocket 帧格式：客户端可以继续发送 `Typing{on:true/false}`；服务器响应变为 `Typing{room_id, count}`。旧的 `Typing` 处理程序可以共存于客户端——如果客户端收到带 `count` 的 `Typing` 帧，它显示聚合；如果收到 `on:true/false`，它显示单个名称。 |
| 草稿版本 | `upsert` 的 `409 Conflict` 是新的。现有客户端收到 `409` 而他们之前得到 `200`——这是一个破坏性变更。必须同时部署新的客户端代码来处理冲突。这是**唯一**会破坏现有客户端的变更。 |

---

## 4. 技术栈

### 4.1 不需要新的技术栈

验证中确定的缺口可以全部在现有技术栈内解决：

- **Postgres**：`REINDEX`、`COALESCE`、`SUM` 作为配额检查、`ADD COLUMN` 作为 migration。所需一切都在内。
- **Redis**：`RecentWriterCache` 可以在进程中（`std::collections::HashMap` + 定期过期）或 Redis 中。进程内对于 RYW 屏障来说已经足够好（同一用户的写入来自同一连接，路由到同一进程）。不需要 Redis。
- **NATS**：打字聚合发生在 Hub 中，不在总线上。不需要新主题。
- **S3/LocalFs**：预签名 URL 可以通过 trait 方法添加。S3 SDK 已经连接（`S3BlobStore` 使用 `reqwest` + HMAC）。

### 4.2 可能值得考虑的新依赖项

| 依赖 | 用于 | 成本 | 推荐？ |
|---|---|---|---|
| `ffmpeg-next` (或子进程) | 缩略图生成 | 中等——FFmpeg 是重型依赖 | **是的，仅作为可选组件**（具有 `NoopPipeline` 后备，因此没有 FFmpeg 的部署不受影响） |
| 图像缩放 crate (`image`/`photon`/`libvips`) | 服务器端图像缩略图 | 低——纯 Rust 或绑定 | **是的，对于图像缩略图**。比 FFmpeg 更轻量。但视频缩略图仍需要 FFmpeg。 |
| `moka` | 进程内 TTL 缓存 | 低——纯 Rust，生产就绪 | **可选**——`RecentWriterCache` 很简单（`HashMap<ParticipantId, Instant>` + 按 tick 过期），不需要外部 crate。 |
| `aws-sdk-s3` (正式版) | S3 预签名 URL | 中等——vs. 当前手写 reqwest+HMAC | **低优先级**——当前的 HMAC 方法对于预签名来说已经足够。签名 URL 算法是标准的。迁移到 `aws-sdk-rust` 是一个单独的重构。 |

### 4.3 自建 vs. 采购决策

| 功能 | 自建 | 购买/第三方 | 建议 |
|---|---|---|---|
| 缩略图 | FFmpeg 子进程（现有基础设施） | Cloudinary/Imgix（付费 CDN） | **为基本缩放自建**；对于生产级 CDN + 转换 + 优化，考虑第三方 |
| 视频转码 | 复用 `aero-live-hls`（现有！） | Mux/Stream（付费） | **自建**——HLS 管线已经存在。复用 `FlvToTsConverter` 和 `HlsWriter` |
| 推送通知 | 已经完成（`push_bot` + FCM/APNs） | 不适用 | 已经自建 |
| 防病毒 | ClamAV（已经集成） | 不适用 | 已经自建 |
| CDN | Cloudflare/CloudFront（DNS CNAME 到 S3） | 预签名 URL + CDN | **采购 CDN**——S3 的原生 CDN 集成正是其用途。 |

---

## 5. 实施路线图

### 优先级矩阵

```
                   影响
              高         低
       ┌────────────┬────────────┐
    高 │  A: RYW    │  C: Typing │
       │  (P0)      │  (P1)      │
紧 急  ├────────────┼────────────┤
    低 │  B: 附件   │  D: 索引   │
       │  (P1)      │  (P2)      │
       │  E: 通知   │  F: 草稿   │
       │  (P2)      │  (P3)      │
       └────────────┴────────────┘
```

### 阶段 1（立即）— 正确性

| 项目 | 估计 |
|---|---|
| **A：读后写一致性** | 1 天 |
| **B1：配额** | 0.5 天 |

**A 的实施步骤**：

1. 在状态中创建一个 `RecentWriterCache`（`HashMap<ParticipantId, Instant>` + 按分钟过期）。
2. 在每条写入路径（`send_message`、`edit_message`、`delete_message`、`mark_read`、`react`）上调用 `cache.record(pid)`。
3. 修改搜索路径（`search.rs:79`）：`let pool = if cache.is_recent(auth.pid) { s.pg } else { s.pg_read }`。
4. 对 `analytics.rs:66`、`ai_usage.rs:120` 相同。
5. 添加 `replica_lag_seconds` 仪表盘（通过 `SHOW standby_slot_lag` 或 `EXTRACT(epoch FROM now() - pg_last_xact_replay_timestamp())`）。
6. 测试：在没有副本的 CI 中，保护是无操作的。使用副本设置集成测试。

> **B1 的实施**：新仓库方法 `BlobRepo::total_storage(owner_id) → u64` + `blob_upload` 中的 `SUM(size) WHERE owner_id = $1 AND deleted_at IS NULL` 检查，对照 `MAX_STORAGE_BYTES` 环境变量。

### 阶段 2（下一次冲刺）— 可扩展性

| 项目 | 估计 |
|---|---|
| **C：打字聚合** | 2 天 |
| **B2：Cache-Control/ETag** | 0.5 天 |
| **D1：embedding_model 列** | 1 天 |

**C 的实施方法**：

1. 在 Hub 中为 typing 指示器添加 `BTreeMap<(RoomId, ParticipantId), Instant>`。
2. 在 `frame.rs` 中 `Typing` 处理程序中添加每成员节流（每 3 秒 1 帧）。
3. 添加后台 tick（每 2 秒），扫描过期条目（10 秒无活动）并广播 `Typing{room_id, count}` 到房间的 WebSocket 连接。
4. 从总线扇出中移除旧的点对点 `Typing` 广播——即不再针对每条打字消息通过 `publish_room_event`。
5. 客户端的更改：UI 从 `Typing{count}` 显示“N 人正在输入…”，而不是成员名称列表。

**B2 的实施**：在 `blob_download` 响应中添加 `Cache-Control: public, max-age=31536000` 和 `ETag: sha256_hex`。对于 blob 内容而言，ETag 是不可变的（如果 blob id 相同，内容 ID 相同，所以 ETag = blob id 或 sha256）。

### 阶段 3（下一里程碑）— 实践

| 项目 | 估计 |
|---|---|
| **B3：预签名 URL** | 2 天 |
| **D2：CLI 命令 + REINDEX** | 1 天 |
| **E：通知去重** | 3 天 |

**预签名 URL 的设计权衡**：

| 方案 | 优点 | 缺点 |
|---|---|---|
| 始终代理 | 简单，统一的访问控制 | 带宽加倍，延迟更差 |
| 始终预签名 | S3 直接下载 | S3 上的权限是基本的——共享 URL 可公开访问 |
| 混合（?presigned=true 用于移动端） | 最佳移动体验，保留服务器端控制 | 需要在代码中处理两条路径 |

**建议**：混合方案。添加 `blob_download` 检测 `?dl=1` 查询参数（由移动客户端设置）。当设置时，如果后端是 S3 且令牌有效，发出 `307 Temporary Redirect` 到预签名 URL。否则，回退到代理。这为移动端提供了低延迟下载，同时为网页保留了代理（其中权限控制很重要）。

### 阶段 4（未来）— 完整性

| 项目 | 估计 |
|---|---|
| **B5：缩略图** | 5 天（新的可选 crate） |
| **F：草稿版本** | 2 天 |

### 风险与缓解

| 风险 | 可能性 | 影响 | 缓解措施 |
|---|---|---|---|
| 打字聚合会破坏现有客户端（帧格式改变） | 中 | 高 | 服务器发送两种帧格式（旧格式用于旧客户端，新格式用于新客户端），为期 2 周。监控 WebSocket 帧错误。 |
| 预签名 URL 会将 blob 访问权限委托给 S3，绕过 IDOR 守卫 | 低 | 高 | 预签名 URL 的 TTL 短（5 分钟）。添加 `allow_presigned` 标志到 blob 元数据，默认为 `false`。S3 后端实现 `presigned_get` 时检查此标志。 |
| `REINDEX CONCURRENTLY` 在事务外运行——如果在 CLI 中出现恐慌，可能会留下损坏的索引 | 中 | 中 | 在 `REINDEX` 之后运行 `ANALYZE`。CLI 在启动时从事务内部运行迁移，但 `REINDEX` 必须在事务外运行。使用 `sqlx::PgConnection` 代替。 |
| `RecentWriterCache` 因写后立即读取的路由到主库而消除读取副本的价值 | 低 | 中 | `cache.is_recent(pid)` 有一个 TTL（例如，写入后 30 秒）。在此窗口后，读取路由回副本。这针对“刚刚写入的用户的搜索”进行了优化——对副本用户没有影响。 |

### 架构影响总结

| 方向 | 新抽象/trait | 新表 | 新 crate | 现有代码更改 |
|---|---|---|---|---|
| A：RYW | `RecentWriterCache` 类型 | 无 | 无 | 3 个处理程序（搜索、分析、AI 使用）+ 写入路径 |
| B：附件 | `BlobStore::presigned_get` | 可能用于分享链接的 `share_links` | 可选：`aero-media` | `routes.rs:blob_download`，可能增加配额 |
| C：打字 | Hub 中基于 tick 的聚合 | 无 | 无 | `frame.rs:Typing` + `hub.rs` |
| D：索引 | 无 | `message.embedding_model` 列 | 无 | `list_without_embedding` + the backfill scan + CLI command |
| E：通知 | 无 | `notification_channels` 列 | 无 | `push_bot.rs` + `notification.rs` |
| F：草稿 | 无 | `message_drafts.version` 列 | 无 | `draft.rs:upsert` → 返回 409 |

**关键结果**：五个方向中有四个（除了缩略图/转码，这些是重型管线）可以实现为对现有存储库和路由处理程序的**小范围、附加变更**。唯一的破坏性变更是草稿版本冲突（新的 409 状态码），可以通过自然采用来处理——在推出新服务器之前部署新客户端。

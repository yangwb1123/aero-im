现在我掌握了充分的上下文来撰写架构分析。这是我基于验证文档、通过实际代码库交叉验证的评估：

---

# 架构分析：Aero IM——从验证反馈中寻找真正的架构机遇

> **范围**: 基于验证文档的分析 + 对 `crates/aero-server/src/hub.rs`、`crates/aero-server/src/search_advanced.rs`、`crates/aero-common/src/model/event.rs`、`crates/aero-storage/src/search_feedback.rs` 等关键模块的代码交叉验证。
>
> **元观察**：代码库已有 140+ 份需求分析文档——该 5 方向分析与现有工作的重合度大于 70%，暴露了**分析流程本身存在结构性问题**，而非分析对象的缺口。下面是我的评估。

---

## 1. 架构评估

### 1.1 当前架构的优势

核心架构选择经得起推敲：

**事件驱动 DAG（NATS JetStream → Hub → WebSocket）** 是真正生产就绪的骨架。`im.room.*` durable consumer 提供 at-least-once 语义，而本地 `Hub::fan_out_raw` 使用有界 mpsc 通道进行进程内扇出。O(N) NATS 发布 → O(1) 本地扇出的区分是正确的——NATS 不是为每条消息扇出给每个连接而设计的。

**进程本地 Hub 中的反向索引**（`ParticipantSubs` 保存 `{rooms, streams, calls}`）是优雅的 O(1) 分离设计。对手写 WebSocket 注册表而言，清理工作与单个客户端面积成正比而非与全局拓扑成正比，这很难得。

**声明式 crate 分层**是面向未来的。16 个 crate 形成清晰的叶子到根依赖图（common → bus/storage/auth → im-core/call → live-* → server）。没有环形依赖。当你决定将子系统分解为独立服务时，这种结构使提取变得直接可行。

### 1.2 局限性——真正棘手的部分

| 区域 | 问题 | 严重性 |
|---|---|---|
| **单进程扇出瓶颈** | 单一 `run_bus_listener` 处理所有 `im.room.*` 事件。高吞吐量的实时流事件（弹幕、礼物）与 IM 消息事件（需要数据库读取和参与者展开）竞争同一任务。没有优先级、没有隔离。 | 高 |
| **搜索相关性欠数学** | 当前的 `merge_hits` 是 `max(FTS_score, vector_score)`——相当于投票给哪个搜索系统更自信，而非学习如何融合它们。`search_feedback` 表已存在，`record_click` 已接线，但 CTA 从未用于调整权重。相关数据基础设施（迁移 0133）已经就绪，但闭环没有闭合。 | 中 |
| **无媒体管线抽象** | 上传的 blob 被直接存储。没有缩略图生成、没有 EXIF 剥离、没有 WebP 转码、没有文档预览、没有视频转码。所有内容都以原生格式提供。考虑到 blob 存储经过 `BlobStore` trait（LocalFs / S3），添加管线层在架构上是直接的——只是还没人做。 | 高 |
| **单体部署** | 单个 `aero-server` 二进制文件包含一切。因此，IM、直播、AI、推送、WebHook 和定时器共享相同的进程生命周期。你可以按垂直维度复制实例，但不能按水平维度。一个 AI 成本预算失控（或 OOM）会杀死该节点上的所有 WebSocket。 | 高 |
| **分析过度饱和** | docs/requirements/ 中有 140+ 份分析文档，其中许多得出相同的结论。最稀缺的资源不是知识——而是执行。下一轮分析收益递减。 | 中（组织性） |
| **Web SPA 是中世纪晚期水平** | 6.4K JS，零构建工具，零 i18n，硬编码中文。Web 客户端是生产级后端与原型级前端的组合。 | 高 |

### 1.3 架构债务

**迁移即编译期嵌入**：`sqlx::migrate!("../../migrations")` 将所有 SQL 移入二进制文件。这是 Tokio 生态系统中常见的模式，但它意味着：
- 零停机迁移需要外部编排（不能只是部署新二进制文件）
- 模式回滚需要单独的二进制文件
- 如果你在构建后、部署前修改了迁移文件，你的二进制文件已经过时

**无 API 版本控制**：`/api/messages/:id` 在第二个版本出现之前不是问题，但在那之后会是问题。当有 200 多个端点且没有 version prefix 时，很难引入 v2。

**生产遥测已就绪但不可操作**：Prometheus 指标已埋好，`x-request-id` 关联已就绪，`traceparent` 传播已遍布总线。但生产流水线（Docker、K8s、告警规则、仪表盘）不存在。工程师看到指标的唯一方式是在本地运行 Prometheus + curl `/metrics`。

**没有特征标志基础设施**：所有开关都是 env 级别的（`AERO_AI_MODERATION`、`AERO_UNFURL` 等）。启动后需重启才能更改。对于 SaaS 而言，这意味着每次 AI 提示更改都是全校范围的部署。

### 1.4 关于分析的元评估

> 验证文档揭示了分析本身的一个结构性问题：声称 5 个方向中有 4 个 "零系统覆盖" 是错误的——已有分析覆盖了它们，只是没有深入到实施细节。
>
> **根本原因**：分析师读取了 `docs/`，但没有读取足够的代码。这解释了他们遗漏了 `ts_headline`（在 `search_query.rs` 中使用）、未发现 `search_feedback` 已接线（通过 `search_advanced.rs` 中的 `search_click` 处理程序），以及错误地认为 `merge_hits` 使用固定权重。
>
> **对项目的影响**：140 轮分析产生递减回报。我强烈建议**冻结新的跨领域分析**，至少冻结 4 个迭代周期，集中精力执行已识别的高优先级项目。特别是，方向二（富媒体管线）确实是未被覆盖的，值得立即关注。

---

## 2. 扩展方向

基于验证反馈，这里有 5 个按实际价值排列的高优先级架构扩展方向——同时考虑验证发现的覆盖状态和真正未覆盖的领域。

### 方向 1（P0）：富媒体管线——唯一真正未被系统分析覆盖的内容

验证声明此方向有最大新增价值，代码审查证实了这一点。没有缩略图管线、没有 EXIF 剥离（隐私风险）、没有 WebP 转换、没有文档预览（PDF→HTML/PNG）、没有视频转码、没有 CDN 签名 URL、没有断点续传。

**为什么需要它**：协作平台是无形的媒体管线。Slack 自动为每个图像链接生成预览；Google Chat 内联渲染文档预览；Discord 代理所有外部图像以剥离 EXIF 并通过 CDN 提供。Aero IM 原样提供 blob——没有图像处理的个人资料照片上传是令人尴尬的体验缺口。

**核心挑战**：
1. **图像处理需要一个库或服务**：Rust 在系统级图像处理方面没有强大的生态系统。你可以使用 `image` crate 进行基本操作（在这一点上它相当好），或者使用外部进程（ffmpeg/ImageMagick）。对于视频，ffmpeg 是不可避免的。
2. **异步作业队列**：转码不能在与 WebSocket 扇出相同的 tokio 任务中进行。你需要一个作业队列——可以使用 `aero-bus`（NATS JetStream 消费者）或专用的 PG 轮询器（如 AiWorker）。消息中的 `ai_jobs` 模式是很好的参考。
3. **CDN 签名**：如果 blob 是通过 S3 提供的，你需要签名 URL。如果通过 CDN 边缘服务器提供，则需要预签名或基于令牌的身份验证。这增加了 blob 服务路径的延迟。

**预期架构变更**：
- 新 crate：`aero-media`——包含 `MediaProcessor` trait、`ImagePipeline`、`VideoPipeline`、`DocumentPreviewer` 和 `CdnSigner`
- `BlobStore` 扩展：`store_and_process(stream, processing_hints)` 触发异步处理管线
- 新表：`media_processing_jobs`（跟随 `ai_jobs` 模式）和 `media_metadata`（宽度、高度、格式、时长、缩略图引用）
- 新路由：`GET /api/media/:id/thumbnail`、`POST /api/media/:id/process`、`GET /api/media/:id/sign`、`GET /api/media/:id/preview`
- 迁移：`media_processing_jobs`、`media_metadata`、`blob_processing_state`

**对现有系统的影响**：中等——新增内容主要通过 `BlobStore` trait 实现分层。上传路径保持兼容（遗留 blob 在没有缩略图的情况下也能工作）。现有消息继续渲染。新路由前缀是 `/api/media/`，不会与 `/api/messages/` 冲突。

### 方向 2（P1）：搜索相关性——关闭反馈循环

验证确认，最引人注目的缺口不是基础功能缺失（`ts_headline` 存在、高级操作符存在、点击跟踪存在），而是这些部分**未连接成闭环**。搜索团队积累了执行信号但未使用它来训练排序器。

**为什么需要它**：想象一个 SaaS 产品，你为其付费但从未使用其数据来改进。`search_feedback` 表记录了每次点击——标题、排名、查询词、参与者。但 `merge_hits` 仍然是 `max(FTS, vector)`。这意味着排名从未改变；用户只是适应了不佳的搜索结果。

**核心挑战**：
1. **时间衰减**：`score * exp(-days * decay_lambda)` 需要调优。设置 `decay_lambda` 太低→过期内容占主导。太高→最近的垃圾邮件充斥顶部。
2. **BM25 需要术语频率统计**：Postgres `ts_rank` 已经很接近 BM25（受 `ts_rank_cd` 影响），但你正在执行的 `websearch_to_tsquery` 关键词频次的窗口有限。真正的 BM25 需要每个集合的文档频率，这不是 SQL 窗口函数能轻松计算的。
3. **从点击到排名的信号**：点击率 ≠ 相关性。一个频繁被点击但从未被打开的标题意味着标题诱饵。你需要 `dwell_time`（页面停留时间）或 `scroll_depth`（滚动深度）参与指标，而这些需要 WebSocket 或分析事件。`search_feedback` 表只记录点击事件——没有后续参与指标。

**预期架构变更**：
- 在 `search_advanced.rs` 中新增 `search_ranking` 模块：根据配置动态将时间衰减 + 用户亲和性 + 多样化的权重应用于搜索结果
- `merge_hits` 增强为可学习的加权融合（从 50/50 开始，使用搜索反馈进行 A/B 测试）
- `SearchFeedbackRepo` 扩展为包括 `dwell_time` 和 `scroll_depth`
- 新增 `SearchCtrAggregator` 定时任务（与现有定时器相同），周期性聚合点击率 → 更新 `message_relevance_weights`

**对现有系统的影响**：低——所有更改都是累加的。现有的 `search_fts` 和 `search_vector` 查询没有改变；只是在之上添加了重新排序和融合函数。

### 方向 3（P1）：运营基础设施——使生产就绪

验证确认，Docker/K8s/CI/CD/备份/密钥管理方向在之前的生产分析中已被覆盖，但缺少实施细节。这里的新增价值在于**实施分解**。

**为什么需要它**：一个没有容器编排的 SaaS 后端只适合演示。Aero IM 需要处理：
- **NATS durable consumer 在 K8s 滚动更新期间再平衡**：当 `aero-server` Pod 被终止时，其 `im.room.*` durable consumer 挂起。如果另一个 Pod 没有立即消费，消息会堆积。你需要优雅的关闭（`CancellationToken` 已就位）+ 合适的 `max_ack_pending` 以避免调度器因大量消息堆积而崩溃。
- **密钥轮换需要双密钥支持**：当前模式是单密钥文件。轮换意味着：将新密钥写入文件，发送 SIGHUP（或让配置观察者发现它），等待所有现有连接在新密钥下建立，然后删除旧密钥。这需要 `ActiveKey` + `RetiredKey` 模式。
- **用于高可用性的蓝绿部署**：durable consumer 名称（`aero-server`、`aero-bot`、`aero-ooo`）是静态的。蓝绿部署需要在部署之间重新平衡消费者。现有的 `AERO__SERVER__ID` 可以派上用场——消费者名称可以包含实例 ID。

**核心挑战**：NATS durable consumer 语义意味着**一次一个**进程承诺 cursor 位置。滚动更新期间，旧消费者消失，新消费者接管并从同一 cursor 处继续——但中间有一段时间没有消费者。`max_ack_pending` 和 `ack_wait` 必须仔细配置，以便在接管过程中不会发生不必要的重新传递。

**预期架构变更**：无代码更改——纯基础设施。Docker 多阶段构建、K8s 部署清单、Helm chart、GHA CI 流水线、备份脚本、密钥轮换工具。

**对现有系统的影响**：无——运营基础设施独立于应用程序代码。

### 方向 4（P2）：多服务分解——从单体到服务网格

单体架构是当前正确的部署模式（限制性推理），但它建立的限制性架构债务在未来会累积。这是在平台成熟时实施的架构。

**为什么需要它**：将进程级隔离开启：
- **独立扩缩容**：在大型直播活动期间，启动更多 live-webrtc 实例，同时保持 IM 节点稳定
- **故障隔离**：AI 工作者中的内存泄漏杀死的是 worker，而不是所有 WebSocket 连接
- **技术多样性**：极高吞吐量的媒体路径（SFU）可以用 C/Rust 编写，而推送网关可以用 Go 编写，但通过 NATS 集成

**核心挑战**：
1. **事务一致性**：当前单体在单个请求中进行数据库写入 + NATS 发布。在服务之间，这变得困难。你需要 saga/outbox 模式。
2. **共享数据库访问**：所有仓库目前都连接到同一个 PG 池。提取服务意味着跨越数据库连接——除非你保持共享数据库（这是合理的，适用于十年）。
3. **通信开销**：当前是本地函数调用 → NATS 发布。在服务之间，它是 NATS 请求/回复或事件驱动。延迟增加。

**预期架构变更**：
- 新二进制文件：`aero-im-svc`、`aero-live-svc`、`aio-ai-svc`、`aero-gateway`（当前 `aero-server` 的路由层）
- 共享存储 crate：`aero-storage` 保持不变（所有服务连接同一个 PG）
- 服务间令牌：NATS 认证 + 服务间 TLS

**对现有系统的影响**：高——这是最大的重构。应在 P0/P1 项目完成后再进行，因为需要单体来迭代功能。

### 方向 5（P2）：Web SPA 现代化 + i18n

验证确认，i18n 在之前的战略产品分析中已被识别，但缺少逐步实施分解。

**为什么需要它**：硬编码的中文将市场限制在中文世界。英文企业、日本游戏公司、韩国直播平台——都需要 i18n。第一行代码之外的零成本：`function t(key) { return translations[key] || key }`。

**核心挑战**：
1. **渐进式、非重写式**：6.4K JS 是全有或全无的——没有模块加载。你需要添加一个构建工具（Vite）并在不重写 SPA 的情况下逐步提取组件。
2. **现有 WS 处理程序**：`search.js`、`polls.js`、`call.js` 等模块通过全局函数和 DOM 事件监听器与核心通信。任何 i18n 包装器都需要与现有代码共存。
3. **动态内容翻译**：用户生成的内容不能由前端翻译。这会创建一个支持要求。

**预期架构变更**：
- 添加 `package.json` + `vite.config.js` + `.eslintrc` 到 `web/`
- `t(key, params)` 函数作为 i18n 切入点
- 翻译文件：`web/locales/en.json`、`web/locales/ja.json` 等
- 新的 `GET /api/translations/:locale` 端点（如果不想将翻译文件静态打包）

**对现有系统的影响**：低——`web/` 目录是自包含的。服务器仅提供静态文件。迁移到 Vite 意味着更新服务器提供的 `index.html` 文件引用。

---

## 3. 接口设计建议

### 3.1 总则

**"不匹配模式"**：API 形状不应在系统之间泄漏。当前代码正确地使用 `#[serde(tag = "kind")]` 区分事件变体，但在事件形状和 WS 帧形状之间存在等价关系。这意味着当事件模式改变时，WS 帧隐式改变，可能影响客户端——如果没有版本控制，你无法通知客户端。

**建议**：添加 `api_version` 到 WS 握手法案和 HTTP API 前缀：

```
方案 A：/api/v1/messages/:id  ── 简单、显式、向后兼容
方案 B：Accept: application/vnd.aero.v1+json  ── 更干净但需要客户端支持内容协商
方案 C：X-API-Version: 2026-07-12  ── 日期版本控制的 Salesforce 风格，允许每个请求版本选择
```

我推荐**方案 C** 用于此项目：在请求标头中设置日期版本控制允许逐步弃用，并在 WS 连接期间在客户端和服务器之间进行版本协商。这也与现有的 `x-request-id` 风格相匹配。

### 3.2 媒体管线接口

如果添加媒体管线 crate，接口应遵循**管线模式**：

```rust
// 建议的 trait 签名（非代码——设计级别）
trait MediaProcessor {
    /// 返回此处理器处理的 MIME 类型列表
    fn supported_inputs() -> Vec<MimeType>;
    
    /// 同步处理——用于快速操作（缩略图、EXIF 剥离）
    fn process_sync(input: Bytes, config: ProcessingConfig) -> Result<ProcessedMedia>;
    
    /// 异步处理——用于慢速操作（视频转码）
    fn process_async(input: Stream, config: ProcessingConfig) -> BoxFuture<Result<ProcessedMedia>>;
}
```

管线应该是可组合的——缩略图管线可以只包含 `ExifStripper` → `WebpEncoder` → `Resize(256)`，而视频管线可以是 `ExifStripper` → `FfmpegTranscoder` → `HlsWriter`。

### 3.3 搜索相关性接口

当前 `search_feedback` 表是写后置的——它记录点击次数，但排名器从不查询它。接口应从**被动（记录点击）→ 主动（查询信号）**：

```rust
// 建议的数据结构（非代码）
trait RelevanceSignalProvider {
    /// 返回特定参与者对特定查询文本的个性化提升
    fn personal_boost(query: &str, participant: ParticipantId) -> Map<MessageId, f64>;
    
    /// 返回特定查询文本的点击率排名信号
    fn ctr_signal(query: &str, workspace: WorkspaceId) -> Map<MessageId, f64>;
}
```

这使得 `merge_hits` 步骤变为：
```
final_score = max(fts_score * w1, vector_score * w2) 
              * time_decay(created_at)
              * diversity_penalty(author_density)
              + personal_boost(message_id)
```

### 3.4 向后兼容性

当前无版本控制的状态意味着所有更改必须是严格累加的：
- 新事件变体 → 旧客户端静默忽略未知标签（`serde(deny_unknown_fields)` 未设置）
- 新路由 → 旧客户端不知道它们
- 新查询参数 → 旧客户端不发送它们，服务器提供默认值

唯一被破坏的情况是**字段重命名**或**字段类型更改**。当前代码使用 `#[serde(rename = ...)]` 处理冲突的字段名（`kind` → `call_kind`/`notify_kind`），这是正确的模式。

---

## 4. 技术选型

### 4.1 需要什么以及为什么

| 域 | 建议 | 理由 | 替代方案 |
|---|---|---|---|
| **图像处理** | `image` crate + `webp` crate | Rust 本地，零外部进程依赖。处理缩略图、EXIF 剥离、WebP 编码。用于简单操作的零额外基础设施。 | 外部 ImageMagick 子进程（更灵活但增加部署复杂性） |
| **视频转码** | **ffmpeg 子进程**（通过 `tokio::process::Command`） | 任何 Rust crate 都无法替代 ffmpeg。它将处理 H.264 → HLS、ABR、缩略图提取。 | 云转码服务（API 费用；不适合自托管） |
| **文档预览** | **将 document 转换为 HTML**：使用 `pdf-extract`（PDF→TXT）或 `wkhtmltoimage`（PDF→PNG 截图） | PDF 预览是协作聊天中 P0 特性。没有好的 Rust-native 替代方案。 | 客户端渲染（浏览器可以本地查看 PDF，但需要跨域隔离） |
| **CDN** | 现有的 **S3 兼容** API（MinIO / CloudFlare R2） | 无需新依赖。`blob_store_from_env` 已经检测 `AERO_S3_BUCKET`。对已读取的 blob 添加签名 URL 是扩展路径。 | 自定义 CDN 边缘（过度设计） |
| **功能标志** | **PG + Redis pub/sub** | 无需新基础设施——两者均已部署。PG 表用于持久性，Redis pub/sub 用于零停机传播到进程。 | LaunchDarkly（SaaS，可能过于昂贵）；自定义特征标志 crate（开销） |
| **负载测试** | `oha`（HTTP）+ 自定义 WS 基准测试 | `oha` 是 Rust 本地 CLI，与 Cargo 生态系统匹配。WS 基准测试需要**自定义**——没有好的现成 WebSocket 基准测试工具可以处理 Aero 的 WS 握手法案。 | `drill`（Rust native，支持 WS 但需要 YAML 配置） |
| **容器化** | **Docker 多阶段构建**（`rust:1.80-slim-bookworm` → `debian:bookworm-slim`） | 标准 Rust Docker 模式。无新依赖。 | distroless（更小的镜像大小但更难的调试） |

### 4.2 自建 vs 采购

| 决策 | 建议 | 理由 |
|---|---|---|
| **缩略图生成** | **自建**（`image` crate） | 简单、无外部依赖、任何 Rust 开发者都可以维护 |
| **视频转码** | **自建**（ffmpeg 子进程） | 在负载下，ffmpeg 进程是自管理的。没有好的 SaaS 替代方案提供必要的控制。 |
| **CDN 签名 URL** | **自建**（基于 S3 的预签名 URL） | 这是 20 行代码。使用 pre-signed URL 的 S3 API 已经完成。 |
| **功能标志** | **自建**（PG + Redis） | 部署堆栈中已存在两项基础设施。添加外部服务（LaunchDarkly）会增加故障点。 |
| **WebSocket 负载测试** | **自定义构建** | 没有现成工具可以处理自定义 WS 握手法案 + `RoomEvent` 帧模式。你需要一个定制的 Rust 二进制文件来创建连接、加入房间和发送消息。 |
| **消息队列** | **已经拥有 NATS** | 此技术选型正确。不要交换。使用其工作队列模式进行媒体处理作业。 |

### 4.3 不应添加的内容

- **不要添加 OpenMLS**：当前 E2E 加密线框（`common/src/mls.rs`）是正确的作用域。完整的端到端加密会由于消息搜索、服务器端 AI 处理和审核需求而从根本上改变架构。在真正的产品架构中，搜索和 E2E 加密是矛盾的——你必须在其中做一个选择。当前设计正确地推迟了这一决定。
- **不要添加 Kubernetes operator**：operator 扩展仅当你超越 10 个服务时才增加价值。在此之前，标准部署清单就足够了。
- **不要添加 GraphQL**：200+ REST 端点 + 27 变体的 WebSocket 实时事件为 GRAPHQL 添加了不必要的抽象。对客户端而言，没有比网络套接字更能够处理实时事件的。

---

## 5. 实施路线图

### 5.1 优先级排序

```
P0：富媒体管线（方向 2）── 高价值，无既有覆盖，中等工作量
P1：搜索反馈闭环（方向 1 的子集）── 中等价值，基础设施已就位，低工作量
P1：运营基础设施（方向 3）── 高运营价值，无代码更改，中低工作量
P2：多服务分解（方向 4）── 长期高价值，必须晚于 P0/P1 进行
P2：Web SPA 现代化（方向 5）── 产品价值高，需要执行精力
```

### 5.2 阶段划分

**阶段 A（6-8 周）**：富媒体管线 + Docker 化

- 第 1-2 周：构建 `crates/aero-media`——图像管线（缩略图、EXIF 剥离、WebP 编码）、`MediaProcessor` trait、`BlobStore` 扩展
- 第 3 周：文档预览管线（PDF→缩略图 + 文本提取），迁移用于 `media_metadata`
- 第 4 周：CDN 签名 URL——扩展 `BlobStore` trait，添加 `sign_url(path, ttl)`，添加 `/api/media/:id/sign` 路由
- 第 5 周：断点续传——添加 `POST /api/media/upload`（开始上传）、`POST /api/media/upload/:blob_id/chunk`、`POST /api/media/upload/:blob_id/complete`
- 第 6 周：Docker 多阶段构建、`docker-compose.yml` 更新、Dockerfile 中的健康检查
- 第 7-8 周：集成测试、基准测试、边界情况（超大文件、并发上传、恶意文件）

**阶段 B（4 周）**：搜索相关性 + 运营

- 第 1 周：BM25 变体——添加 `bm25_score` 到 `search_fts` 查询（或存储为物化列），A/B 测试新排名函数
- 第 2 周：时间衰减——将 `score * exp(-days * decay_lambda)` 应用于搜索结果，可选择关闭
- 第 3 周：反馈循环——编写 `SearchCtrAggregator` 定时任务，使用累积的 CTR 数据调整 `merge_hits` 权重
- 第 4 周：负载测试——`oha` HTTP 基准测试 + 自定义 WS 基准测试（使用 `tokio-tungstenite` 创建 N 个连接并测量扇出延迟）

**阶段 C（4 周）**：功能标志 + Web SPA 现代化

- 第 1 周：PG + Redis pub/sub 上的 feature flag 基础设施，管理端 API `PATCH /api/admin/feature-flags`、`FeatureFlagLayer` 中间件
- 第 2 周：为 web SPA 添加 `package.json` 和 `vite.config.js`，将 JS 从全局函数迁移到 ES 模块
- 第 3 周：i18n——`t(key, params)` 函数、翻译文件、无状态组件提取
- 第 4 周：服务分解设计——识别服务边界（gateway / im-svc / live-svc / ai-svc）、设计 NATS 主题映射

### 5.3 风险与缓解措施

| 风险 | 可能性 | 影响 | 缓解措施 |
|---|---|---|---|
| **ffmpeg 子进程管理** | 中 | 高——管理不善的子进程可能会泄漏内存 | 使用 `tokio::process::Command`，设置严格的超时、内存限制和超时后的 `kill()` |
| **搜索反馈回路过拟合** | 低 | 中——仅优化点击率可能偏爱标题党内容而非相关性 | 使用**多样化的 KPI**（点击率 + 会话持续时间 + 后续查询数）；监控 CTR <-> 用户留存相关性 |
| **Durable consumer 在滚动更新期间丢失消息** | 中 | 高——消息可能会在旧消费者分离和新消费者启动之间的间隙中累积 | 在所有 Pod 中设置 `max_ack_pending: 10000`，`ack_wait: 60s`；preStop hook 应优雅关闭消费者 |
| **媒体管线使上传速度变慢** | 高 | 中——用户不希望等待缩略图生成才能发送消息 | 使管线**完全异步**：消息在缩略图生成之前发送。如果缩略图未及时到达，客户端在消息第一次渲染时显示回退。 |
| **分析疲乏** | 高 | 高——140 轮分析后，团队可能对架构变更建议产生免疫 | 冻结分析，集中执行，实施 3 个最高的 P0/P1 项目 |

### 5.4 成功指标

| 方向 | 可衡量的目标 |
|---|---|
| **富媒体管线** | 上传图像后 500ms 内首字节缩略图，上传 10MB 文件时断点续传支持，EXIF GPS 数据在上传后 1 秒内从存储的 blob 中剥离 |
| **搜索相关性** | 点击率（有反馈时）> 60%，"搜索后不放弃"率降低 40% 以上，搜索到打开的平均时间减少 30% |
| **运营基础设施** | CI/CD 管道在 `git push` 后 15 分钟内部署到暂存环境，Docker 镜像启动 < 3 秒，滚动更新零停机时间（在 3 节点 K8s 设置上验证） |
| **Web SPA 现代化** | 通过 Vite 代码分割减少 50% 以上的初始 JS 负载，对于英文覆盖 100% 的 UI 字符串实现 i18n 键，零 eslint/no-undef 警告 |

---

## 总结

验证文档正确地识别了**富媒体管线**作为唯一真正未被系统分析覆盖的方向，并应提升为 P0 优先级。然而，它也正确地指出搜索质量、运营基础设施和 i18n 方向已经以各种详细程度被覆盖——这份分析中真正新增的价值在于**实施细节**，而非方向本身的识别。

从架构角度来看，代码库的**事件驱动 DAG 骨架是生产就绪的**，并且 crate 分层对未来的服务分解有很好的适应性。主要瓶颈是：（1）单进程扇出需要优先级隔离，（2）搜索相关性是用数据而不是数学构建的，（3）运营飞轮（Docker/K8s/CI/CD）缺失，以及（4）媒体管线从零开始。

我的建议很明确：**执行富媒体管线，关闭搜索反馈循环，Docker 化部署，然后考虑分解**。在此之前，冻结分析以支持交付。该项目从知识饱和转向执行稀缺——下一阶段不是识别更多缺口，而是弥合已经发现的缺口。

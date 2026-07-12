以下是对 `docs/requirements/2026-07-11-five-genuinely-novel-extension-directions.md` 的架构与技术设计分析。

---

# 架构师评审：Aero IM 五个高价值扩展方向

## 1. 架构评估

### 当前架构的核心优势

Aero IM 的架构经过大量打磨，有几个值得肯定的设计决策：

**1. 事件驱动骨架成熟**

`NATS JetStream → Hub → fan_out_raw → WebSocket` 这一管道已经是工业级设计——durable cursor 实现 at-least-once 语义、bounded mpsc 提供背压、两阶段解码（先 lift seq 再 typed RoomEvent）实现协议演化。这比大多数 IM 后端的消息总线设计更健壮。

**2. 特征（crate）边界清晰**

以 crate 作为功能单位而非 `src/{domain}/` 目录，是 Rust 生态的最佳实践。依赖图自下而上（common → bus/storage/auth → im-core/ai/live-core → server）无环，体现了良好的模块化。

**3. 数据库抽象安全**

所有 SQL 通过 sqlx 编译期校验，迁移嵌入二进制——这是 Rust 后端在数据层的正确做法。`migrate!("../../migrations")` 一个调用点确保了迁移在任何环境下的一致性。

**4. Hub 内进程扇出 vs 跨实例 NATS 的分离**

`Hub::fan_out_raw` 负责本进程 bounded 扇出，NATS 负责跨实例投递——这种分离是 AI-Native 实时系统的正确选择。每实例都有独立 consumer（`aero-server` durable group），水平扩展无需重设计。

### 当前架构的局限性

**1. 序列化策略过度统一——全 JSON 是性能债务**

| 层面 | 问题 | 量化影响 |
|------|------|---------|
| WS 帧 | `serde_json::to_string` 每次分配新 String | 500 人 typing 风暴产生 250K 分配/秒 |
| NATS body | `serde_json::to_vec` 原样传输 | 10K 消息 × 200 消费者 × 7 天 ≈ 14 GB 冗余存储 |
| 字段名 | `"participant_id"` 等重复出现在每条消息 | 占 WS 帧 30%+ 字节，是结构性浪费 |

这不是「优化了更好」的级别，而是**架构层没有考虑序列化成本**——全 JSON 在 3 万行 Rust 的后端做出了正确工程选择（快速迭代、调试友好），但生产部署的临界质量要求协议层重新审视。

**2. AI 管线的抽象不足**

```rust
pub struct AiServiceImpl {
    anthropic: Option<AnthropicClient>,   // 硬编码
    embedder: Box<dyn Embed + Send + Sync>, // trait 抽象已有 ✅
    // 没有 LlmBackend trait
}
```

注意：`embedder` 已是 `Box<dyn Embed>` 模式，但 LLM completion 路径却没有同样的抽象。这说明架构师在设计 AI 层时意识到嵌入层的可插拔性，但`AnthropicClient`被直接注入了 `AiServiceImpl`——这是典型的「抽象泄漏」：使用方应该只依赖 `trait LlmBackend`，不关心具体实现是 Anthropic、Ollama 还是 vLLM。

**3. Blob 存储是裸直通管道**

`BlobStore` trait 的 `put`/`get`/`delete` 三个方法过于原始。没有缩略图、没有格式转换、没有缓存标记。这不是 `BlobStore` 的问题——它作为存储抽象是正确的——而是**缺少一个 media pipeline 层**来包装 `BlobStore` 并提供增值处理。

**4. 客户端架构与后端成熟度严重不匹配**

`web/README.md` 自称 "Debug Client"，这不是谦虚——5.9K JS 对 46K Rust 的 1:8 比例说明了前后端投入差距。但更深层的问题是**架构层面没有考虑多端协议**：WS 帧定义是单客户端假设，没有 `device_id`，没有离线消息队列，没有 Service Worker 入口点。

### 架构债务盘点

| 债务类型 | 严重度 | 说明 |
|---------|--------|------|
| WS 帧序列化无协商扩展 | P0—性能 | `axum::ws` 不协商 permessage-deflate，每帧 40-70% 膨胀 |
| AI LLM 无 trait 抽象 | P1—扩展性 | Anthropic 直接注入，Ollama/vLLM 适配需改核心逻辑 |
| Blob 存储无管线 | P1—产品力 | 缩略图 / CDN / srcset 全链路缺失 |
| 无 device_id 概念 | P1—协议层 | WS 连接按 participant 聚合但无法区分设备，多端同步冲突 |
| 字段名全量传输 | P1—带宽 | 每条 WS 帧 20-30% 是 key 字符串 |
| Prompt 字符串硬编码 | P2—治理 | AI prompt 在代码中，无版本/审核/A/B 测试 |

---

## 2. 扩展方向

基于以上架构评估，我提供以下扩展方向排序——不是重复文档中已列的 5 个方向，而是从**架构债务回填**的角度给出优先切入顺序。

### 方向 A（P0 · 架构层）：序列化层重构——从全 JSON 到结构化线协议

**为什么需要**：
- 当前序列化是架构中增长最快的性能瓶颈——WS 帧膨胀、NATS 存储膨胀、CPU 浪费在重复的 `to_string` 分配
- 无 per-message 压缩协商是 WebSocket 协议层的合规缺口（RFC 7692）
- 修复路径零新依赖、零第三方服务——纯 serde 配置

**核心挑战**：

| 挑战 | 难度 | 说明 |
|------|------|------|
| WS upgrade 时协商 permessage-deflate | 低 | `tungstenite` 支持 `WebSocketConfig`，`axum` 的 `upgrade` 路径需传递配置 |
| 字段 rename 的兼容性 | 中 | 已有 WS 客户端使用 `participant_id` 字段名。加 `#[serde(alias = "pid")]` 可向后兼容但也增加了序列化端消耗（别名需匹配） |
| NATS 消息压缩的滚动升级 | 中高 | 所有 consumer（含 bot consumer、webhook dispatcher）需升级到解压版 bus crate。必须 feature-flag 渐进切换 |

**架构变更**：
1. `ws/frame.rs` 层：增加 `compress: bool` 参数，`to_string` / `from_str` 路径用 `zstd::stream::write::Encoder` 包装
2. `aero-bus/src/lib.rs`：`EventBus::publish` 增加 `compress: bool` 选项，JetStream consumer 自动检测 magic bytes 决定是否解压
3. 模型层：逐步加 `serde(rename)`，每次一个字段组（先 `participant_id`→`pid`，再 `room_id`→`rid`），同时保留 `alias` 旧名

**对现有系统的影响**：
- WS 核心路径（frame.rs）需要单层条件分支，零影响业务逻辑
- NATS consumer 需更新 bus crate 版本，但解压失败有 graceful fallback

### 方向 B（P0 · 通用性）：AI Backend 抽象——`trait LlmBackend` + Model Router

**为什么需要**：
- 这是**架构级缺口**，不是功能缺口。`embedder` 已有 `Box<dyn Embed>` 模式，anthropic 路径却直接注入，说明设计不一致
- 金融/医疗/政务客户因数据主权无法使用 Anthropic API，直接出局
- 重复问题被用户在不同房间多次问——全走 LLM 调用花费 token，无语义缓存

**核心挑战**：

| 挑战 | 难度 | 说明 |
|------|------|------|
| trait 定义要覆盖所有当前使用场景 | 中 | `complete`、`stream_complete`、`moderate`、`tool_use` 四个方法签名差异大。`moderate` 可能单独走内容审核模型 |
| Ollama API 兼容 OpenAI 但格式差异 | 低 | `prompt` vs `messages`、`stream` vs `non-stream`，适配器约 150 行 |
| 语义缓存的 pgvector 相似度阈值 | 中 | 0.95 可能过高（翻译类场景），0.85 可能过低（法律问答场景）。需 per-route 可配置 |

**架构变更**：
1. 新增 `aero-ai/src/backend.rs`：`trait LlmBackend { async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse>; async fn stream_complete(...); async fn moderate(...) → Option<...>; }`。`CompletionRequest` 包含 `system_prompt`、`messages`、`tools`（JSON schema）、`max_tokens`、`temperature`
2. `AnthropicBackend` 实现此 trait，`OllamaBackend`（OpenAI-compatible）、`vLLMBackend` 各自实现
3. `AiServiceImpl` 通过 `Box<dyn LlmBackend>` 持有，不再直接引用 `AnthropicClient`
4. `ModelRouter`：按 task type（summarize/ask/translate/moderate）路由到不同 backend。配置 `AERO_LLM_ROUTER=summarize:ollama,ask:anthropic`
5. `SemanticCache`：`(embedding → (response, model_used))`，使用 pgvector 查询

**对现有系统的影响**：
- `AiServiceImpl` 的每个 public 方法（`answer_question`、`summarize`、`translate`、`rewrite`、`moderate`）的签名不变——trait 仅替换注入方式
- 对外 API 无变化。配置从单 `ANTHROPIC_API_KEY` 扩展为 `AERO_LLM_BACKEND=ollama` 等

### 方向 C（P1 · 产品力）：Media Pipeline 层——缩略图/转码/CDN

**为什么需要**：
- IM 流量 **80%+** 是附件/媒体。当前直通管道的架构意味着每字节都经过 Rust 进程
- 无缩略图导致产品体验断层——10MB 照片直接下载、头像全尺寸加载
- CDN 回源缓存策略缺失导致重复请求同 blob

**核心挑战**：

| 挑战 | 难度 | 说明 |
|------|------|------|
| 缩略图性能 | 中 | `image` crate 解码+缩放 10MB JPEG ≈ 100ms CPU。高并发场景需 `Semaphore` 限并发 |
| 视频转码依赖 ffmpeg | 中 | Rust 生态无纯 Rust 视频编码器。`tokio::process::Command` 封装 ffmpeg 是务实选择 |
| CDN 回源缓存失效 | 中 | blob id 是 UUID，默认不变。但头像替换后同一 participant 不同 blob_id → 需 CDN purge |

**架构变更**：
1. 新增 `aero-media-pipeline` crate（或 `aero-storage/src/media/`）：`MediaProcessor` trait
2. `ImageProcessor` 使用 `image` crate 生成 `{id}_64.webp`、`{id}_256.webp`、`{id}_1024.jpg`，存储在 `thumbs/{id}/` 前缀
3. `VideoProcessor`：ffmpeg 提取第一帧为缩略图 + 可选转码 HLS
4. 新增 `GET /api/blobs/:id?size=64|256|1024|raw` 端点，返回对应 size 的缩略图。`Cache-Control: public, max-age=31536000, immutable`
5. `BlobStore` 扩展：`has_thumbnail(id, size) -> bool`、`get_thumbnail(id, size) -> Option<Vec<u8>>`

**对现有系统的影响**：
- `/api/blobs` 上传路径：post-upload hook 触发异步缩略图生成。上传响应不变（id、url、meta）
- `/api/blobs/:id` 下载路径：新增 `?size=` query 参数，不传则原样返回 old behavior
- `Block::File` 无变化——前端择机升级使用 `srcset`

### 方向 D（P1 · 协议层）：多端感知——Device ID、离线队列、Service Worker

**为什么需要**：
- 当前架构假设每个 participant 只有一个活跃 WS 连接。多端（桌面+Web+移动端）并发时消息重复渲染
- 无 Service Worker 导致断网时客户端完全不可用——数据从「实时」降级为「无」
- 离线消息队列是最基本的客户端弹性模式

**核心挑战**：

| 挑战 | 难度 | 说明 |
|------|------|------|
| Hub 按 device_id 去重 | 中 | 当前 `hub.rs` 的 `connections` 是按 `ParticipantId` key 的 `HashMap`。改为 `(ParticipantId, DeviceId)` 复合 key。WS upgrade 时需客户端传 `device_id`（query param 或 JWT claim） |
| Service Worker 的 SyncManager 兼容性 | 低 | iOS Safari 不支持 `SyncManager`，需 fallback 到 `pendingQueue` |

**架构变更**：
1. WS upgrade 路径：`ws_handler` 从 query 或 cookie 读取 `device_id`，不存在则服务端生成 UUID。`hub.register(participant_id, device_id, sender)`
2. `Hub::connections`：从 `HashMap<ParticipantId, ...>` 改为 `HashMap<(ParticipantId, DeviceId), ...>`。`fan_out_raw` 按 device_id 扇出（不向发送端设备回传）
3. `sw.js`：`install` 缓存静态资源、`activate` 清理旧缓存、`fetch` 拦截 API 请求做网络优先、`push` event 显示通知
4. `web/ws.js`：`pendingQueue` 断线排队 + `navigator.serviceWorker.ready.then(r => r.sync.register('flush-msg'))` 或 `onopen` 直接 flush

### 方向 E（P2 · 平台化）：App 注册与 OAuth 授权流

**为什么需要**：
- 交互式 Block 基础设施已完备（`Block::Button`/`Select` + `interactions.rs` + `bot_dispatch.rs` + NATS 扇出），缺的只是集成层的注册/发现/授权
- 此方向是**平台化**的关键一步——从功能完备的 IM 到可扩展的生态平台

**核心挑战**：

| 挑战 | 难度 | 说明 |
|------|------|------|
| OAuth scope 粒度设计 | 中 | scope 必须小于房间级、响应式权限——`messages:read` 太宽、`messages:room:*:read` 太窄。Slack 的 `channels:history` 级别是参考 |
| App webhook SSRF 防护 | 中 | 恶意 webhook URL 指向 `localhost:6379`（Redis）尝试 SSRF。必须 URL scheme 校验（仅 HTTPS）+ IP 黑名单 |
| OAuth flow 的安全合规 | 中 | `client_secret` 存储（bcrypt hash vs plaintext）、`code` 的 TTL（10 分钟）、`refresh_token` 的轮换 |

**架构变更**：
1. 新增 `apps` 表 + `app_installations` 表 + `oauth_codes` 表 + `app_activity_log` 表
2. 新增 `POST /api/apps`（developer 注册）、`GET /api/apps`（list）、`DELETE /api/apps/:id`
3. 新增 OAuth endpoints：`GET /oauth/authorize` → consent page → `POST /oauth/token` → `{access_token, refresh_token, scope}`
4. `bot_event_subscriptions` 增加 `action_ids: Option<Vec<String>>` 过滤列
5. `App` 用 access_token 调用 REST API——gateway 层增加 `OAuth2Token` extractor（同 `AuthUser` 类似）

---

## 3. 接口设计建议

### 3.1 关键的抽象引入

**`trait LlmBackend`** 是最应优先引入的抽象层。

```rust
// 接口设计原则
pub struct CompletionRequest {
    pub system_prompt: String,
    pub messages: Vec<ChatMsg>,
    pub tools: Option<Vec<ToolDef>>,    // JSON Schema
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub stream: bool,                    // SSE vs 单响应
}

pub struct CompletionResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub model: String,
}

#[async_trait]
pub trait LlmBackend: Send + Sync {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse>;
    async fn stream_complete(&self, req: CompletionRequest) -> Result<Pin<Box<dyn Stream<Item = Result<CompletionChunk>> + Send>>>;
}
```

设计要点：
- `stream` 作为 `CompletionRequest` 字段而非两个 trait 方法——简化使用方判断。服务端根据此字段决定走 `complete` 还是 `stream_complete` 路径
- `tool_calls` 在响应中非请求中——model 决定是否调用工具，使用方检查响应中是否有 tool_calls
- `model` 在响应中返回实际使用model——便于审计日志记录
- 不要为 `moderate` 单独走 trait method——`moderate` 是调用 `complete` 的 prompt 变体，不是协议变体

### 3.2 Media Pipeline 接口设计

不应修改 `BlobStore` trait（它有明确的职责：键值存储），而应在上层引入 `MediaPipeline`：

```
┌──────────────┐    ┌──────────────┐    ┌──────────────┐
│  BlobUpload  │───▶│ MediaPipeline │───▶│  BlobStore   │
│  (handler)   │    │ .process()   │    │  .put()      │
└──────────────┘    └──────┬───────┘    └──────────────┘
                           │
                    ┌──────▼───────┐
                    │ ImageProcessor│
                    │ VideoProcessor│
                    └──────────────┘
```

```rust
/// Media processing result: original + zero or more variants.
pub struct MediaResult {
    pub original: BlobMeta,
    pub variants: Vec<VariantMeta>,   // {size, blob_id, mime, width, height}
}

#[async_trait]
pub trait MediaProcessor: Send + Sync {
    /// Process uploaded media. Returns immediately with original blob ID;
    /// processing happens in background via returned receiver.
    fn process(&self, blob_id: &str, bytes: &[u8], mime: &str)
        -> Pin<Box<dyn Future<Output = Result<Vec<VariantMeta>>> + Send>>;
}
```

设计要点：
- `MediaProcessor::process` 接收完整 bytes 而非 stream——上传时已有完整 bytes 在内存
- `ImageProcessor` 内部用 `tokio::task::spawn_blocking` 调用 `image` crate 解码（image 是 CPU 绑定的同步库）
- variant 命名：`{blob_id}_{size}.{ext}`，不使用子目录（简化 CDN 缓存键）
- `GET /api/blobs/:id` 新增 `?size=` 参数返回变体——没有则 fallback 原图

### 3.3 向后兼容策略

| 变更 | 兼容策略 | 切换周期 |
|------|---------|---------|
| 字段 rename（`participant_id`→`pid`） | `#[serde(alias = "participant_id")]` 保留旧名 | 推荐：先加 alias 接收，3 个月后加 rename 产生，再 3 个月后移除 alias |
| WS permessage-deflate | 检查 `Sec-WebSocket-Extensions` 协商结果——客户端不声明则无压缩 | 只需后端一次部署，客户端渐进启用 |
| NATS 消息压缩 | feature flag `nats_compress: bool`，false 时旧格式。渐进切换 | 需所有 consumer 更新前保持 false |
| MediaPipeline 引入 | 上传路径加 hook point，配置 `AERO_MEDIA_PIPELINE_ENABLED=false` 时完全跳过 | 默认 false -> 1 周观察 -> 默认 true |
| OAuth scope 引入 | 初始无 scope 限制（`scope=all`），第二阶段加粒度 | 分两阶段部署 |

---

## 4. 技术选型

### 4.1 新增依赖评估

| 场景 | 候选 | 推荐 | 原因 |
|------|------|------|------|
| 图片缩放 | `image` crate | **`image`** | 纯 Rust，零 C 依赖，已是最广泛使用的 Rust 图像库。注意：解码 10MB JPEG 需 `jpeg-decoder` feature（默认开启） |
| 通用压缩 | `zstd` / `gzip` / `lz4` | **`zstd`** | 压缩比≈gzip 但解压速度 2-3x 更快。NATS 场景中 consumer 频繁解压，zstd 的解压性能是优势。crate：`zstd`（Franklin Chen 维护，质量高） |
| 视频处理 | `ffmpeg` CLI | **`ffmpeg` 子进程** | Rust 无成熟纯 Rust 视频编码器。`tokio::process::Command` 封装 ffmpeg 是最务实的选择——稳定、广泛、不引入 C 绑定风险 |
| Web Push | `web-push` crate | **`web-push`** | 成熟，支持 VAPID、RFC 8030。注意：需更新 `aero-push` crate 增加 `WebPushProvider` 实现 |
| 桌面壳 | Electron / Tauri | **Tauri** | Rust-first 技术栈天然适合 Tauri（Rust native WebView wrapper）。二进制小 10x、内存少 50% |
| LLM 本地部署 | Ollama / vLLM / LocalAI | **Ollama**（初期） | Ollama API 最简单（兼容 OpenAI），模型下载/切换最开发者友好。vLLM 性能更优但运维复杂 |

### 4.2 自建 vs 采购决策

| 场景 | 选择 | 逻辑 |
|------|------|------|
| LLM 私有化 | **自建适配器** | 非「自己造 LLM」，是构建适配器层对接已有的开源部署方案（Ollama/vLLM/LocalAI）。成本 ~150 行代码/适配器 |
| 缩略图生成 | **自建管线** | `image` crate 是标准库，200 行代码即可完成。无需第三方服务 |
| 视频转码 | **自建封装 ffmpeg** | ffmpeg 是标准工具。用 `tokio::process::Command` 封装是轻量方案 |
| CDN | **采购 CDN**（CloudFront/Cloudflare） | 不是自建 CDN 的问题。在 blob download 头加 `Cache-Control` 即可兼容 CDN origin-pull |
| Web Push | **自建 VAPID 支持** | `web-push` crate 即可实现，无第三方服务依赖 |
| 桌面壳 | **Tauri** | Rust 生态最优选择，不是自建 vs 采购的问题 |

### 4.3 须避免的技术方案

| 方案 | 避免原因 |
|------|---------|
| `msgpack` 替代 JSON | 收益有限（字段名仍传输），且增加调试困难。字段 rename 成本更低、效果更好 |
| Protocol Buffers / FlatBuffers | 引入代码生成和编译依赖，与 serde 生态冲突。JSON + permessage-deflate + 字段 rename 已覆盖 80% 收益 |
| 纯 Rust 视频编码器 | 无成熟方案。`ffmpeg` 子进程是正确路径 |
| 自建 CDN | 运维成本 > 收益。S3/CloudFront/Cloudflare 已成熟 |
| React Native 跨平台 | 前端团队未证明有跨平台能力。PWA + Tauri 更快见效 |

---

## 5. 实施路线图

### 整体优先级排序

```
P0 ┌──────────────────────────────────────────────────────┐
   │  方向 A: 序列化层重构（字段 rename + permessage-deflate）│
   │  方向 B: AI Backend 抽象（trait LlmBackend）          │
P1 ├──────────────────────────────────────────────────────┤
   │  方向 C: Media Pipeline（缩略图 + CDN 缓存头）         │
   │  方向 D: 多端感知（Device ID + SW + 离线队列）         │
P2 ├──────────────────────────────────────────────────────┤
   │  方向 E: App 注册 + OAuth 授权流（平台化）              │
   └──────────────────────────────────────────────────────┘
```

**排序逻辑**：
- 方向 A 投入产出比最高——字段 rename 是零运行时开销但立减 20-25% 带宽
- 方向 B 是架构债务——不修复则每条 AI 功能迭代都需改 `AiServiceImpl` 核心
- 方向 C 是产品力——缩略图是用户感知最直接的改进
- 方向 D 是客户端基础——无 Device ID 则多端同步无法正确工作
- 方向 E 是平台化——商业价值高但复杂度也高，且依赖交互式 Block 的成熟使用（当前唯一消费者是 `agent_bot`）

### 阶段划分

**Phase 1（1-2 周）——立竿见影的基础优化**

| 任务 | 工时 | 代码变更 |
|------|------|---------|
| JSON 字段 rename（第一批：`participant_id`→`pid`、`room_id`→`rid`、`message_id`→`mid`） | 2 天 | `aero-common/src/model/` 各 enum variant + serde rename |
| WS 升级时协商 permessage-deflate | 3 天 | `ws_handler` upgrade 前配置 `WebSocketConfig` |
| blob download 加 `Cache-Control: public, max-age=31536000, immutable` + `ETag` | 1 天 | `blob_download` handler |
| PWA sw.js 最小版本（静态资源缓存 + offline fallback） | 3 天 | `web/sw.js` 新增 |

**Phase 2（2-3 周）——架构层抽象**

| 任务 | 工时 | 代码变更 |
|------|------|---------|
| `trait LlmBackend` 定义 + `AnthropicBackend` 实现 | 3 天 | `aero-ai/src/backend.rs` 新增，`service_impl.rs` 重构 |
| `OllamaBackend` 适配器 | 2 天 | `aero-ai/src/ollama.rs` 新增 |
| Model Router（按 task 路由 backend） | 2 天 | `aero-ai/src/router.rs` |
| 语义缓存（pgvector） | 3 天 | `aero-storage` 新增 `SemanticCacheRepo` |
| `MediaProcessor` trait + `ImageProcessor` | 4 天 | `aero-storage/src/media/` 新增 |
| `GET /api/blobs/:id?size=64|256|1024|raw` | 2 天 | `routes.rs` 新增 query 参数 |

**Phase 3（2-3 周）——多端 + 平台**

| 任务 | 工时 | 代码变更 |
|------|------|---------|
| Device ID 支持（Hub 按 `(pid, device_id)` 聚合） | 3 天 | `hub.rs` 重构 |
| Web Notification API + VAPID push subscription | 3 天 | `web/notifications.js` + `push_bot` 扩展 |
| 离线消息队列（pendingQueue + SW SyncManager） | 2 天 | `web/ws.js` |
| App 注册 API + OAuth 授权流 | 5 天 | `apps` 表 + OAuth endpoints |
| App 市场最小版本（/apps 管理页面） | 3 天 | `web/apps.js` |

### 关键风险与缓解

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| 字段 rename 破坏现有 WS 客户端 | 中（已有 web debug client 使用旧字段名） | 高 | `#[serde(alias)]` 保留旧名。发布后观察 3 天确认无兼容问题再改 producer 端 |
| permessage-deflate 与某些代理不兼容 | 低（除非企业代理 strip `Sec-WebSocket-Extensions`） | 中 | 协商失败 = 无压缩，功能不受影响。无退化 |
| Ollama 本地 GPU 内存不足导致 OOM | 中 | 中 | AiWorker 的 `Semaphore` 限并发 + 配置文件显存上限 |
| 缩略图生成 CPU 耗尽 | 中（图片上传风暴） | 中 | `Semaphore` 限并发 + 配置 `AERO_THUMBNAIL_CONCURRENCY`（默认 4） |
| Tauri 二进制签名（macOS notarization） | 低 | 低（开发期，发布前才需处理） | 前期 Electron 快速验证，后期迁移 Tauri |

### 依赖关系 DAG

```
Phase 1 ───────────────────────┐
  ├─ 字段 rename（独立）        │
  ├─ permessage-deflate（独立）  │
  ├─ Cache-Control 头（独立）    │
  └─ sw.js（独立）              │
                                ▼
Phase 2 ───────────────────────┐
  ├─ trait LlmBackend           │─────→ 方向 B 各适配器
  ├─ MediaPipeline              │─────→ Image/VideoProcessor
  └─ 语义缓存（依赖 embedder）    │
                                ▼
Phase 3 ───────────────────────┐
  ├─ Device ID（依赖 Hub）       │─────→ 多端去重 + 扇出优化
  ├─ App 注册（独立）            │─────→ OAuth 授权
  └─ VAPID（依赖 push_bot）      │─────→ Web Push 通知
```

**关键发现**：5 个方向间依赖度很低——**P0 和 P1 中的 4 个方向（字段 rename、permessage-deflate、trait LlmBackend、MediaPipeline）都是彼此独立的**，可以并行推进。唯一有依赖的是多端去重（依赖 Hub 数据结构的 Device ID 化）。

---

## 总结

这份文档的作者对 Aero IM 代码库做了深入的扫描，5 个方向中的 4 个确实在此前的 134+ 份分析中未被系统性覆盖。从架构师角度看，文档的**最大贡献不在 5 个方向本身（很多是直觉上「应该做」的事），而在它精确指出的每个方向的切入锚点、代码证据、边界情况**——这让这些方向从「好想法」变成了「有工程路径的改动」。

**文档的盲区（未充分讨论的点）**：

1. **多端同步的冲突模型**：方向四指出缺 Device ID，但没有讨论多端在线时消息已读状态的聚合（两个设备都读了消息 → 一个已读回执 vs 两个？）。这是个比 Device ID 更深层的协议问题
2. **AI 私有化的成本模型**：方向二假设 Ollama/vLLM 部署「只需适配器」，但缺少 TCO 分析——GPU 采购成本 vs Anthropic API 按量付费的盈亏平衡点。金融客户可能一年 500 次 LLM 调用，私有化部署不如直接走 Anthropic
3. **OAuth scope 的设计原则**：方向五指出需要 scope 但未提 scope 是按资源粒度（`messages:room:*`）还是按操作粒度（`read`/`write`/`admin`）——这在 OAuth 2.0 设计中是最难也最影响使用体验的抉择
4. **未考虑压缩与嵌套的交互**：permessage-deflate（传输层压缩） + 字段 rename（应用层精简） + NATS zstd 压缩——三层叠加时压缩效果会递减。第一个 60% 收益来自 permessage-deflate，字段 rename 再减 20% 但被 deflate 已经压缩过的字段 rename 收益会缩小到 ~10%。应在测试环境中验证叠加效果

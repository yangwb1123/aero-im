# 架构深度分析：基于 Review 文档与代码库实际状态

> **分析范围**：Aero IM 整体架构 + 5 个扩展方向的评估 + 接口设计 + 技术选型 + 实施路线图
> **分析依据**：Review 文档（2026-07-11）、AGENTS.md、README.md、设计 Spec、代码实际调研结果

---

## 1. 架构评估

### 1.1 当前架构优势

**事件驱动骨架成熟度极高**。Aero IM 的架构已然超越了大多数同类项目的设计水平：

| 特性 | 评价 |
|------|------|
| **事件 DAG 设计** | NATS JetStream (跨实例事实源) → Hub (进程内 bounded mpsc 扇出) → WebSocket 的分层架构，兼具水平扩展能力和单实例低延迟 |
| **持久性分层** | durable consumer (IM 消息，at-least-once) 与 ephemeral consumer (直播弹幕，best-effort) 的区分——**架构级的正确trade-off**，而非单层方案 |
| **ID 命名空间** | per-subject 单调 seq 使去重/排序在客户端简单执行，不需要复杂的一致性协议 |
| **crate 依赖有向无环** | 严格自下而上：common → bus/storage/auth/signaling → im-core/im-call/ai/push → live-* → server。无循环依赖，模块边界清晰 |
| **AI 预算系统** | `CostBudget` + `KeyedCostBudget` 的双层窗口设计，是服务端 AI Gateway 久经验证的 pattern |
| **媒体 seam 架构** | `CallUpstream` trait + `UpstreamFactory` trait + `MediaForwarder` trait 的三层抽象使 Call Bridge 的控制面/注册/生命周期与传输解耦——这是**测试友好**的设计 |

### 1.2 架构局限性

| 问题 | 影响 | 严重度 |
|------|------|--------|
| **JSON-only 序列化** | WebSocket 帧 + NATS 消息 + Webhook 全 JSON。字段名不压缩，`RoomEvent` 带 `kind` tag 在大体量消息下膨胀 40-70%——在 IM 场景下这不是问题，但在直播弹幕高吞吐场景（每秒数千条礼物+弹幕+观看数变更）会成为瓶颈 | P2 |
| **`SfuMediaSession::run` 生产零调用** | 整个 SFU 媒体面（选择性转发、Simulcast、RTCP）停留在字节级单元测试。浏览器 ICE/DTLS/SRTP 联调缺口是最关键的生产就绪阻挡 | **P0** |
| **TTS 零代码** | 实时字幕翻译只有 inbound（ASR→翻译→发送），没有 outbound（语音合成播放）。对于"已读消息转语音"场景（ADA 合规 + 驾驶场景）是缺失的 | P1 |
| **MCP 零实现** | AI Agent（`agent_bot`）只有 @-mention 触发，没有开放的标准协议接口让外部 AI 工具发现和调用 Aero IM 的能力 | P1 |
| **NATS 消息无 schema registry** | Rust 类型通过 serde 编码/解码，但跨语言或跨版本消费时需要协调。目前无 protobuf 或 flatbuffers schema 管理 | P3 |
| **permessage-deflate 缺失** | WebSocket 帧未压缩。在消息体包含长 JSON（如批量 Notify/历史同步）时带宽浪费明显 | P2 |

### 1.3 关键设计决策合理性

| 决策 | 评估 | 备选方案 |
|------|------|----------|
| **纯 Rust 全栈** | ✅ 正确。对于实时 IM + 媒体服务器，Rust 的内存安全 + 零开销抽象是最优选择。JS/Go 在媒体面难以达到同等性能 | Go 在 I/O 密集型场景接近但 GC latency 对 RTC 不友好 |
| **云 AI API 优先** | ✅ 正确。小团队用自建 LLM 推理的经济账不划算。HashEmbedder fallback 确保沙箱和 CI 可离线运行 | 自建 vLLM/TGI 作为长期演进方向 |
| **NATS JetStream 而不是 Kafka** | ✅ 正确。NATS 部署简单（单二进制），足够支撑 15 个 crate 的规模。Kafka 对于这个项目来说运维负担过重 | Redis Streams 不是等价替代——缺少 durable consumer 的多实例水平扩 |
| **str0m 而不是 webrtc-rs** | ✅ 正确。str0m 是纯 Rust、无 CGO/FFI 的 WebRTC 实现，在 CI 和沙箱中可运行。webrtc-rs 有安全漏洞历史且 CGO 依赖复杂 | 无——这是 Rust 生态最合理的 WebRTC 选择 |
| **Postgres + pgvector 而不是专用向量库** | ✅ 正确。对 MVP 来说减少中间件数量 > 向量性能微优化。观察到单表超过 ~1000 万行时 HNSW 索引性能下降时可迁移到 Qdrant | Qdrant/Milvus 对前期过度复杂 |
| **手写 SRT 而不是 libsrt binding** | 🟡 有风险。完整实现了 HSv5 + AES-CTR + ACK/NAK，但没有与 libsrt/ffmpeg/OBS 的互操作测试。这是质量风险——协议解析器对 wire 字节差异非常敏感 | libsrt C API binding 可减少验证成本，但 CGO 与项目"纯 Rust"方针不符 |

### 1.4 架构债务

1. **`protocol.rs` 与 `crypto.rs` 的矛盾注释**（review 中确认）：`protocol.rs` 声明 "unencrypted only" 但 `crypto.rs` 有 812 行 AES-CTR + RFC 3394 实现。需要清理文档或代码。

2. **`transcribe.rs` 命名不一致**：文件名是 `transcribe.rs`（少一个 'i'），trait 是 `Transcriber`，而函数是 `default_transcriber()`。这是小笔误但建议统一为 `transcribe`/`Transcriber`。

3. **token helper 同名**（AGENTS.md §4.2 已标注）：`webhook`/`scim`/`invitation` 各有 `generate_token`+`hash_token`。`lib.rs` 只 re-export `webhook` 的版本——这是编译通过但容易混淆的。考虑用 `#[doc(hidden)]` 或 `#[deprecated]` 标记非 webhook 版本为内部。

4. **`CompressionLayer` 的文档缺口**：README 声称"全 JSON"，但 HTTP REST 已有 `CompressionLayer`（gzip）。只是 WS WebSocket 没有 `permessage-deflate`。文档需要明确区分 HTTP 压缩（已有）和 WS 压缩（缺失）。

---

## 2. 扩展方向

### 方向 1：MCP Server（Model Context Protocol）— **P1 / 最高战略价值**

#### 为什么需要

Aero IM 的 AI Agent 目前只有 @-mention 触发一个入口。MCP 是 Anthropic 推行的开放协议标准，使得**任何 MCP 客户端**（Claude Desktop、Cline、Cursor、VS Code 扩展）都能发现和调用 Aero IM 的工具。

**商业逻辑**：Slack AI 是封闭平台，Discord Clyde 是封闭平台。MCP **开放协议**是 Slack/Discord 替代产品的差异化突破点——每次 Claude Desktop 用户查询团队聊天记录时，都是 Aero IM 的品牌曝光。

#### 核心挑战

| 挑战 | 复杂度 | 说明 |
|------|--------|------|
| 认证对齐 | 中 | MCP 的 authentication 需要映射到 Aero 的 JWT/PAT 系统。MCP 标准本身还未完全定稿 auth 层（正在演进中） |
| 工具函数设计 | 高 | 不能简单暴露所有 API。需要设计工具粒度：`search_messages`、`summarize_room`、`list_rooms`——每个工具需要好的人可读描述和参数 schema |
| 资源发现 | 中 | MCP 的资源（resources/prompts）映射到 Aero 的「房间/消息/参与者」模型。需要对资源路径做命名空间设计（`aero://workspace/{id}/room/{id}/messages` 等） |
| 速率限制 | 中 | 外部 AI 调用不能耗尽 Aero 的内部 AI 预算。需要 MCP 专属的 `CostBudget` 窗口（与 `AiWorker` 共享或独立？） |
| 并发连接 | 低 | MCP 的 `rmcp` 当前使用 SSE 传输。每个连接一个 `tokio::spawn`，需要设置最大连接数以防范 DoS |

#### 预期的架构变更

```
┌─── MCP Client ───┐     SSE      ┌─── aero-server ──────────────────┐
│ Claude Desktop   │◄────────────►│  POST /api/mcp                    │
│ Cursor           │              │  ├── McpRouter                   │
│ Cline            │              │  │   ├── tools/search_messages   │
└──────────────────┘              │  │   ├── tools/summarize_room   │
                                  │  │   ├── resources/rooms/{id}   │
                                  │  │   └── prompts/summarize      │
                                  │  └── auth: Bearer <JWT|PAT>     │
                                  └─────────────────────────────────┘
                                       │
                                  ┌────▼─────┐
                                  │ AiService │ (复用既有)
                                  └──────────┘
```

- **新增**：`crates/aero-mcp` crate（或者集成到 `aero-server/src/mcp/`）
- **依赖**：`rmcp` crate（需加入 workspace Cargo.toml）
- **复用**：`AiService`（问答/摘要/RAG）、`ImService`（消息搜索/读取）
- **不变**：现有所有路由、bot、worker 逻辑完全不变

#### 对现有系统的影响

**最小**——MCP 是纯新增的一层适配器。它调用现有 `AiService` 和 `ImService` 的方法，不修改任何业务逻辑。唯一需要注意的共享资源是 AI 预算系统：MCP 调用和内部 Agent Bot 调用应该共享还是隔离预算？建议**共享**（先到先得），因为外部 MCP 调用也需要受保护不被内部消费饿死。

---

### 方向 2：Multi-Engine STT/TTS 抽象层 — **P1 / 高价值**

#### 为什么需要

当前状态：
- STT：只有 `WhisperTranscriber`（OpenAI API）+ `StubTranscriber` fallback
- TTS：**零代码**

**缺失场景**：
1. **ADA/EN 301 549 合规**：视障用户需要消息→语音输出
2. **驾驶/免提**：车载场景的自动朗读
3. **语音消息双方向**：目前只有录制→转写；缺少文本→播报
4. **边缘计算**：低成本部署场景下需要 whisper.cpp（CPU）或 Coqui（开源 TTS）

#### 核心挑战

| 挑战 | 说明 |
|------|------|
| 引擎切换热键 | STT/TTS 引擎需要在运行时切换还是启动时选择？建议启动时选择（`from_env()` factory），运行时切换增加测试面 |
| 音频格式归一化 | 不同 TTS 引擎输出不同格式（WAV/MP3/Opus），需要统一的 `AudioOutput` 类型 |
| 缓存策略 | 相同文本重复 TTS（如"早上好"）应该命中缓存。缓存 key = 文本+语言+引擎名 |
| 流式 TTS | 长文本需要分块流式合成（边合成边播放），非阻塞 |
| 成本控制 | TTS 引擎可能有 token/请求计费（如 ElevenLabs、Google Cloud TTS），需要与 `CostBudget` 集成 |

#### 预期的架构变更

```rust
// crate: aero-ai 新增接口

#[async_trait]
pub trait SttEngine: Send + Sync {
    async fn transcribe(&self, audio: AudioInput) -> Result<String>;
    fn name(&self) -> &'static str;
    fn supported_formats(&self) -> &[&str];
}

#[async_trait]
pub trait TtsEngine: Send + Sync {
    async fn synthesize(&self, text: &str, voice: &VoiceConfig) -> Result<AudioOutput>;
    fn name(&self) -> &'static str;
    fn available_voices(&self) -> Vec<VoiceDescriptor>;
}

// 工厂模式与既有 default_transcriber() 一致
pub fn default_stt_engine() -> Arc<dyn SttEngine> { ... }
pub fn default_tts_engine() -> Arc<dyn TtsEngine> { ... }
```

**代码影响**：
- 新增 `crates/aero-ai/src/stt.rs` 和 `crates/aero-ai/src/tts.rs`
- 现有 `transcribe.rs` 重构：将 `Transcriber` trait 替换为 `SttEngine`（或者保留前者为后者的包装）
- 新增 TTS REST endpoint：`POST /api/ai/tts`（返回 audio bytes）
- WebSocket 新增帧类型：`tts_synthesize` / `tts_audio`

#### 对现有系统的影响

- `Transcriber` → `SttEngine` 迁移是向后兼容的（保留旧的 `default_transcriber()` 函数标记为 deprecated）
- TTS 是**纯新增**功能，不影响既有路径
- 音频缓存需要新增 `CachedTtsEngine` 包装器（透明装饰器模式）

---

### 方向 3：SRT 互操作测试 + 协议一致性 — **P2 / 修复质量债务**

#### 本轮分析的关键修正

Review 文档指出 SRT "从未接线"是事实错误——`ingest.rs` 中 `SrtIngest` 以 `srt.run(repo, srt_cfg).await` 启动，HSv5 + AES-CTR + ACK/NAK 全部实现并单测。**但此方向的真正价值不在接线，在以下三点**：

#### 真正要做的 3 件事

1. **与 libsrt/ffmpeg/OBS 的互操作测试**（核心缺口）
   - 用 ffmpeg 推 SRT 流到 Aero，验证 HLS 切片是否正确
   - 用 OBS 推 SRT（带密码加密），验证 AES-CTR 握手
   - 目前所有测试都是手写 wire bytes——与实际实现的偏差未检验

2. **加密向量验证**
   - `crypto.rs` 缺少 NIST SP 800-38A（AES-CTR）和 RFC 3394（Key Wrap）的已知向量测试
   - 即使没有 libsrt 依赖，也应该有密码学正确性测试

3. **注释清理**
   - `protocol.rs` L25 的 `"unencrypted only"` 注释与 `crypto.rs` 812 行 AES 实现矛盾
   - 文档表述：README 说 "P7 ✅" 但 Boot 代码有 `srt.run`——需要说明 "字节级 ✅，真实推流联调待做"

#### 预期影响

**零架构变更**——全在 `aero-live-srt` crate 内部。
- 新增测试文件：`tests/interop_vectors.rs`（加密向量）+ `tests/manual/ffmpeg_push.rs`（集成测试脚本）
- 修改注释：`protocol.rs` + SRT 文档
- 时间估计：3-5 天

---

### 方向 4：二进制线协议优化 — **P2 / 高 ROI，分步推进**

#### 为什么需要

| 场景 | JSON 大小 | Protobuf 大小 | 节省 |
|------|----------|--------------|------|
| 单条消息（100 字 text block） | ~280 B | ~120 B | 57% |
| 批量 NotifyBatch（10 人） | ~2.5 KB | ~800 B | 68% |
| 直播弹幕（每帧） | ~400 B | ~150 B | 63% |

在高吞吐直播场景（每秒 1000+ 弹幕），JSON → binary 的带宽节省是显著的。

#### 分层方案（核心建议）

| 层 | 方案 | 工作量 | ROI | 建议 |
|----|------|--------|-----|------|
| **Layer 1** | JSON 字段名压缩：`#[serde(rename = "短名")]` | 1-2 天 | 高（零运行时开销） | **P1 第一优先** |
| **Layer 2** | WebSocket permessage-deflate | 2-3 天 | 高（透明压缩） | **P1 第二优先** |
| **Layer 3** | NATS 消息 protobuf 化（关键 subject） | 5-7 天 | 中（双序列化器维护成本） | P2 |
| **Layer 4** | 混合网关：JSON↔Protobuf 自动协商 | 10-15 天 | 低（架构复杂度大增） | **不建议** |

**关键决策**：Layer 1+2 覆盖 80% 场景，Layer 3 只在 `live.stream.*` 高吞吐 subject 上实施 protobuf。Layer 4（全系统混合网关）的维护成本和测试面扩大不值得——NATS 2.9+ 的消息头 `Content-Type` 可以区分格式，不需要 subject 前缀 hack。

#### 对现有系统的影响

- **Layer 1**：纯编译时 rename，零运行时影响。所有现有 JSON consumer 字段名不兼容——**需要同步更新 web/ 客户端**。建议分批做：先 IM subject，再直播 subject。
- **Layer 2**：仅 WS 连接升级。服务器端加 `deflate` 扩展支持（axum + tokio-tungstenite 已内置），客户端浏览器原生支持。**零代码变更**。
- **Layer 3**：`aero-bus` crate 新增 `EventBus::publish_protobuf` 方法。IM 消息继续 JSON，直播弹幕改用 protobuf。

#### NATS 2.9+ headers 方案

```rust
// 在 aero-bus/jetstream.rs 中：
headers.insert("Content-Type", "application/x-protobuf");
headers.insert("X-Aero-Schema-Version", "1");
```

同一 subject `live.stream.{id}` 上混合格式传输，客户端解码时检查 `Content-Type`。

---

### 方向 5：SFU 媒体面生产接线 — **P0 / 最高优先级的质量 gap**

#### 核心缺口（本轮验证确认）

1. **`SfuMediaSession::bind` / `run` 生产零调用** ✅ (review 正确)
   - `sfu_media.rs` 中 `bind`+`run` 完整实现且单测
   - 但唯一的调用方在 `#[cfg(test)]` 中
   - **影响**：整个 SFU forwarder（选择性转发 + Simulcast + RTCP）从未在真实媒体流中运行

2. **`ensure_egress` 从未被调用** ✅ (review 正确)
   - `CallBridgeSupervisor` 的 `ensure_egress` 完整实现（`call_bridge_supervisor.rs:374`）
   - 但不存在调用路径——没有任何代码实例化 `CallEgressTap` 并传入
   - **影响**：跨节点 egress（本地 RTP → 远端节点）断裂

3. **`aero-im-call` 与 `CallBridgeSupervisor` 解耦** ✅ (review 正确)
   - `aero-im-call` 在 `join_group_call` 中的 `CallTopology::BridgeTo(urls)` 返回其他节点 URL
   - 但 `ensure_bridges` 只在 `frame.rs` 响应 `CallJoin` WS 帧时触发
   - `ensure_egress` 没有任何一层的触发逻辑

#### 真正要接的线

```
┌─ CallJoin WS 帧 ───────────────────────────┐
│ frame.rs                                     │
│  ├─ ensure_bridges(call, urls)  ← 已接      │
│  └─ (缺少) ensure_egress(call, tap) ← 断裂  │
└──────────────────────────────────────────────┘
           │
           ▼
┌─ SfuMediaSession ───────────────────────────┐
│ sfu_media.rs                                 │
│  ├─ bind(call, participant, fwd, ...)        │
│  └─ run(cancel)                              │
│     ├─ on_rtp → forwarder → local subs       │
│     └─ (缺少) on_rtp → egress → remote       │
└──────────────────────────────────────────────┘
           │
           ▼
┌─ CallEgressTap ─────────────────────────────┐
│ call_bridge_supervisor.rs                     │
│  ├─ UdpRtpEgress::run(tap, cancel)           │
│  └─ encode_bridge_frame → UDP → peer         │
└──────────────────────────────────────────────┘
```

#### 修复策略

1. **P0.a**：在 boot 时启动 `SfuMediaSession`（`orchestration.rs`）
   - 为每个有远程参与者的 call 创建一个 `SfuMediaSession`
   - `run` 循环独立 `tokio::spawn`
   - 注意：需要 `CancellationToken` 管理生命周期

2. **P0.b**：在 `CallJoin` 处理路径中加入 `ensure_egress` 调用
   - 当 `call.topology == BridgeTo(urls)` 时，从 `SfuMediaSession` 获取 `CallEgressTap`
   - 传给 `supervisor.ensure_egress(call, tap)`

3. **P0.c**：跨 crate 集成——让 `aero-im-call` 知道 `CallBridgeSupervisor` 的存在
   - 但**不要引入循环依赖**：`aero-im-call` ← `aero-server`，不是反过来
   - 在 `AppState` 中持有 `CallBridgeSupervisor`，`frame.rs` 处理 `CallJoin` 时从 state 获取

#### 时间估计

- P0.a：1-2 天（纯接线，代码已存在）
- P0.b：1 天
- P0.c：2 天（需要在 boot 中组装 SfuMediaSession 和 CallEgressTap）

---

## 3. 接口设计建议

### 3.1 新抽象层引入原则

**不要再增加 trait 层级，除非确实需要多实现**。当前架构中以下 trait 可以受益于接口抽象：

| 现有 seam | 当前实现 | 是否需要 trait | 理由 |
|-----------|----------|---------------|------|
| `Transcriber` → `SttEngine` | Whisper + Stub | ✅ **保留并演进** | 未来可能有 Deepgram、Azure Speech、本地 whisper.cpp |
| `TtsEngine`（新增） | 未实现 | ✅ **需要 trait** | 多引擎切换是刚需（云 API + 本地引擎） |
| `EventBus` | NATS JetStream | ✅ **已正确设计** | 虽然短期内只有 NATS，但 trait 使单测和 mock 变得容易 |
| `BlobStore` | LocalFs + S3 | ✅ **已正确设计** | factory 模式 (`blob_store_from_env`) 是正确做法 |
| `AiEmbedder` | Voyage + HashEmbedder | ✅ **已正确设计** | 工厂+fallback 模式是 GoF Strategy 的 Rust 版 |

### 3.2 关键接口设计原则

1. **工厂函数统一命名**：`from_env()` 是当前 pattern（`WhisperTranscriber::from_env()`）。所有新增引擎都应该遵循 `EngineName::from_env() -> Option<Self>` 模式，返回 `None` 表示 key 未配置。

2. **Fallback 链透明**：用户应该能通过 trace 看到当前使用的引擎：
   ```rust
   // 启动时日志
   info!("STT engine: {} (transcriber)", stt_engine.name());
   info!("TTS engine: {} (fallback stub)", tts_engine.name());
   ```

3. **CostBudget 集成**：新引擎（STT/TTS）应该通过 `AiService` 的预算系统。每个 `stt_engine.transcribe()` 和 `tts_engine.synthesize()` 调用应该计费。

4. **向后兼容的版本化**：JSON 字段压缩（Layer 1）必须伴随客户端升级。建议：
   - 在 WebSocket 握手时协商字段格式：`Sec-WebSocket-Protocol: aero-json-v1`
   - 现有客户端继续 v0（全字段名），新客户端用 v1（压缩名）
   - 在 `hub.rs` 做帧格式翻译

### 3.3 向后兼容性策略

| 变更类型 | 策略 | 示例 |
|----------|------|------|
| 新增字段（JSON） | 无影响——serde 默认忽略 `deny_unknown_fields` 未设置的额外字段 | 在 `RoomEvent` 中新增字段，旧客户端忽略 |
| 字段名压缩 | 协议版本协商（`Sec-WebSocket-Protocol` header） | 新客户端声明 `v1`，服务器发压缩字段名 |
| 新增消息类型 | 客户端 `match` 需要有 `_ => {}` 兜底 | 新增 `RoomEvent::CallEvent` 变体，旧客户端忽略 |
| 删除字段 | **避免**——用 `#[serde(default)]` 保留旧字段 | 不要让旧客户端反序列化失败 |
| 新增 crate | 无影响——Cargo workspace 自动隔离 | 新增 `aero-mcp` crate，其他 crate 不需要知道它 |

---

## 4. 技术选型

### 4.1 需要引入的新依赖

| 依赖 | 用途 | 版本 | 评估 |
|------|------|------|------|
| `rmcp` | MCP Server 实现 | latest | **必须**——MCP 协议 Rust 参考实现，Active 维护 |
| `prost` | protobuf 代码生成（Layer 3） | 0.13+ | **可选**——只在直播 subject 场景使用。注意需要 protoc 编译期依赖 |
| `tokio-tungstenite` (已有) | WS permessage-deflate | 已有 | 只需启用 feature `deflate`，零新依赖 |
| `whisper-rs` / `whisper-sys` | 本地 whisper.cpp 推理 | - | **不建议**——CGO 依赖。优先用 OpenAI API。如有 GPU 服务器再考虑 |
| `tts-rust` | 本地 TTS | - | **不建议**——生态不成熟。优先用 ElevenLabs / Azure / Google Cloud API |

### 4.2 自建 vs 采购决策矩阵

| 能力 | 自建理由 | 采购理由 | 决定 |
|------|----------|----------|------|
| MCP Server | 核心差异化竞争力 | 无成熟的 MCP Server 商业产品 | **自建**（P1） |
| STT 引擎 | 多引擎抽象层降低供应商锁定 | OpenAI Whisper 已可用 | **自建抽象层**（trait）+ **采购后端**（OpenAI API） |
| TTS 引擎 | 同上 | ElevenLabs / Azure 有成熟 API | **自建抽象层**（trait）+ **采购后端**（ElevenLabs） |
| 二进制序列化 | 控制权在自己手中 | protobuf/flatbuffers 已有成熟工具链 | **自建**（serde rename + 分步推进） |
| WebRTC/SFU | 纯 Rust str0m 是战略选择 | 商业 SFU（LiveKit / MediaSoup）更成熟但贵 | **自建**（已完成 80%，只差生产接线） |

### 4.3 Rust 生态风险评估

| 依赖 | 风险 | 缓解 |
|------|------|------|
| `str0m` 0.19 | 相对新，API 可能不稳定；ICE/DTLS 部分不如 `webrtc-rs` 成熟 | 已经有字节级单测覆盖媒体面。CI 中跟踪 upstream 变更 |
| `rmcp` | MCP 标准本身还在演进（Institutional 版本 v1.0 尚未冻结） | 紧跟标准版本，在 `Cargo.toml` 中锁定 major 版本 |
| `rml_rtmp` | RTMP 协议库，维护活跃度中等 | RTMP 是成熟协议，变化少。如果有问题可手写 RTMP handshake |
| `prost` | 编译期依赖 `protoc`，增加了 CI 配置复杂度 | 仅在 Layer 3 启用。可以通过 `build.rs` + `protoc-bin-vendored` 消除 protoc 依赖 |

---

## 5. 实施路线图

### 优先级总览

| 优先级 | 方向 | 工作量 | 业务价值 | 技术价值 | 建议窗口 |
|--------|------|--------|----------|----------|----------|
| **P0** | SFU 媒体面生产接线 | 5-7 天 | 极高（通话+RTC 全链路才真正"完成"） | 高（解锁端到端测试） | 立即 |
| **P1** | MCP Server | 7-10 天 | 极高（开放生态+外部 AI 工具集成） | 高（标准协议入口） | 第 1-2 周 |
| **P1** | Multi-Engine STT/TTS | 5-7 天 | 高（ADA 合规+多场景） | 中（抽象层设计） | 第 2-3 周 |
| **P1.5** | JSON 字段名压缩 (Layer 1) | 1-2 天 | 中 | 高（零开销收益） | 第 1 周，配合 MCP |
| **P1.5** | WebSocket permessage-deflate (Layer 2) | 1-2 天 | 中 | 中（透明优化） | 第 1 周 |
| **P2** | SRT 互操作测试 + 文档清理 | 3-5 天 | 中（质量提升） | 高（修复隐式缺陷） | 第 3-4 周 |
| **P2** | NATS protobuf Layer 3 | 5-7 天 | 中（直播高吞吐） | 中（双序列化器） | 第 5-6 周 |
| **P3** | 混合网关 Layer 4 | 10-15 天 | 低 | 低 | **不建议** |

### 阶段划分

#### 阶段 1：媒体面交付（P0 — 第 1 周核心）

```
Day 1-2:  SfuMediaSession 生产启动
          - orchestration.rs 中 SfuMediaSession::bind
          - run 循环 tokio::spawn
Day 3-4:  CallEgress 链路接通
          - frame.rs CallJoin 中触发 ensure_egress
          - SfuMediaSession 的 on_rtp → egress tap 接线
Day 5-7:  端到端验证
          - 本地双节点测试（localhost 模拟跨节点）
          - 浏览器 + WHEP 播放测试
```

**风险**：str0m ICE/DTLS 在真实浏览器握手时可能有问题。**缓解**：先做 UdpSocket 级别的桥接通路（跳过 str0m 握手），待稳定后再接入 DTLS-SRTP。

#### 阶段 2：API 标准化（P1 — 第 2-3 周）

```
Week 2:
  - 启用 WS permessage-deflate（1 天）
  - JSON 字段名压缩（2 天：IM subject + 直播 subject 分步）
  - 新增 TTS REST endpoint + StubTtsEngine（2 天）

Week 3:
  - MCP Server alpha（5-7 天）
    - tools: search_messages, summarize_room, list_rooms
    - resources: rooms/{id}, messages/{id}
    - auth: JWT/PAT 集成
  - STT/TTS 抽象层落地（2 天，配合 MCP）
```

**风险**：字段名压缩需要同步 web 端更新。**缓解**：用协议版本协商（`Sec-WebSocket-Protocol`），旧客户端继续用 v0。

#### 阶段 3：质量加固（P2 — 第 4-6 周）

```
Week 4:
  - SRT 互操作测试（3 天）
    - ffmpeg → Aero SRT 推流验证
    - 加密向量测试
  - 注释/文档清理（1 天）

Week 5-6:
  - NATS protobuf Layer 3（5-7 天，直播 subject 优先）
  - 评估是否需要 Layer 4（5 天后决定）
```

**风险**：protobuf schema 变更需要与 JSON 版本同步。**缓解**：protobuf schema 作为 JSON 模式的形式化等价——不改变语义，只改变编码。

### 风险矩阵

| 风险 | 可能性 | 影响 | 缓解 |
|------|--------|------|------|
| str0m ICE/DTLS 在真实浏览器握手失败 | 中 | 高（SFU 延迟交付） | 先做 UDP 桥接通路；备选 `webrtc-rs` 或 MediaSoup 降级 |
| MCP 标准版本冻结延迟 | 低 | 中（MCP 功能稳定但非标准） | `rmcp` 锁定版本，升级工作可控 |
| 字段名压缩导致 web 端兼容问题 | 中 | 中 | 协议版本协商 + web 端兼容层（`event.kind || event.call_kind` 模式） |
| whisper.cpp 本地推理太慢 | 低-中 | 低 | 不走本地推理——坚持云 API。GPU 加速检测只在 boot log WARN |
| SRT 与 libsrt 互操作失败 | 中 | 中（SRT 可信度受损） | 先修复加密向量测试；与 ffmpeg 推流的测试在隔离环境运行 |

---

## 6. 总结与综合建议

### 必须纠正的 Review 事实错误

1. **SRT "从未接线"** ❌ → 实际：`ingest.rs` 中 `SrtIngest::run` 已接线并 spawn。方向三保留但修正论据。
2. **`call_bridge_supervisor` "生产零接线"** ❌ → 实际：控制面（`ensure_bridges`）在 `frame.rs` 中接入了 `CallJoin` 路径。方向五需要聚焦在**媒体面 gap**（`SfuMediaSession::run` 未启动 + `ensure_egress` 未调用）。
3. **行号 966 vs 1025** —— 小误差，但应建议 review 用 `wc -l` 验证后再引用。

### 最大的架构剩余工作

按 Impact = (业务价值 × 技术紧迫性) 排序：

1. **P0 → SfuMediaSession 生产启动**：当前架构最关键的断裂点。所有 SFU 代码（选择性转发、Simulcast、RTCP PLI/FIR）已实现但从未在真实媒体流中运行。
2. **P1 → MCP Server**：最高战略价值的外部接口。利用现有的 `AiService` 和 `ImService` 能力栈，在不增加基础设施复杂度的前提下打开外部 AI 生态集成。
3. **P1 → Multi-Engine STT/TTS**：Accessibility 合规 + 新场景拓展。`SttEngine`/`TtsEngine` 抽象层设计可以复用 `Transcriber` 的工厂+fallback 模式。
4. **P2 → SRT 互操作测试**：加密实现缺少向量验证 + 无真实推流器联调——这是隐式质量债务，不是功能缺口。

### 最终建议

**不要试图同时推进全部 5 个方向**。当前团队应该：
1. **立即分配 1 人**处理 P0（SFU 媒体面接线）——这是最核心的生产就绪阻挡
2. **MCP 和 STT/TTS** 可以并行推进（不同的 crate，没有依赖冲突）
3. **SRT 互操作**和**二进制协议**推迟到 P2，除非直播场景出现性能瓶颈

这个排序确保：第 1 周交付 SFU 端到端媒体面（真正的通话/RTC 全链路），第 2-3 周交付 MCP 和 STT/TTS 抽象层（开放生态 + 合规），后续交付质量加固（SRT 测试 + 性能优化）。

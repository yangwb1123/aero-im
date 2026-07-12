Now I have a thorough understanding of the codebase state. Let me provide my review.

---

## Review: "5 个零覆盖高价值扩展方向" (2026-07-11)

**Reviewer**: 资深架构师  
**Method**: 验证每个主张的代码证据 + 评估分析质量 + 指出事实错误

---

### 总体评价

这是一份**高质量但部分主张过时/不准确**的分析。核心价值在于方向一（MCP Server）和方向二（STT/TTS 抽象层），这两个确实是货真价实的零覆盖高价值缺口。方向四（二进制协议）也站得住。但方向三（SRT）和方向五（Call Bridge）存在**事实性错误**——代码已比文档描述的更加完备。

给我最深印象的是你们团队的迭代速度：你们在**两天内**从 21 份变体文档收束到这份定稿，且代码实际状态比 07-09 日那批扫描的文档描述**更先进**——SRT 和 Call Bridge 的生产接线已在 07-11 之前完成。

---

### 方向一（MCP Server）— ★★★★★ 最高质量

**结论：完全认同。这是文件中最solid的分析。**

| 维度 | 评价 |
|------|------|
| 代码证据 | ✅ 完美——grep 确认 `rmcp` 零出现，设计文档确实规划了但零实现 |
| 产品根因 | ✅ Slack AI / Discord Clyde → MCP 开放协议的反向竞争论点很强 |
| 工程方案 | ✅ `tools/resources/prompts` 三层映射恰当；认证复用 JWT/PAT 合理 |
| 边界情况 | ✅ Tool 超时、资源大小限制、TLS、并发连接数、权限映射——都很务实 |
| 商业影响 | ✅ 准确的 P1 判断 |

**建议补充**：
- 确认 `aero-ai` 的 `AiService` 能否被 `rmcp` 的 `serve` 循环直接复用，还是需要适配层。从 `aero-ai/service/` 的接口看，应该是直接可复用的。
- 增加 **MCP 传输层的选择理由**：SSE vs stdio。建议初始用 SSE（HTTP），因为 `aero-server` 已运行 HTTP 服务，新增一个 endpoint 的成本最低；stdio 适合嵌入式 CLI 场景。

---

### 方向二（Multi-Engine STT/TTS）— ★★★★★ 高质量

**结论：完全认同，且理由补充充分。**

| 维度 | 评价 |
|------|------|
| 代码证据 | ✅ `WhisperTranscriber` 唯一实现 + `StubTranscriber` fallback 确认 |
| 代码证据 | ✅ TTS 服务端零代码确认 |
| 工程方案 | ✅ `SttEngine` + `TtsEngine` trait 设计合理；四种集成场景映射清晰 |
| accessible 论证 | ✅ ADA/EN 301 549 合规需求论证——这是很多技术分析遗漏的 |
| 边界情况 | ✅ 引擎优先级、音频归一化、缓存、流式、语言检测、成本控制——都很全面 |

**可改进的点**：
- `WhisperTranscriber::from_env()` 已有工厂模式——`SttEngine` 的 `from_env()` 可以沿用同一 pattern。事实上 `transcribe.rs` 的 `default_transcriber` 已经是一个 factory，迁移到 `default_stt_engine` 的成本很低。
- 建议在方案中加入 **GPU 加速检测**：whisper.cpp 在无 GPU 的服务器上 CPU 推理慢 10-20x。环境检测失败时应在 boot log 中 WARN，而不是静默 fallback 到 Stub。

---

### 方向三（SRT 一致性）— ★★☆☆☆ 事实错误

**这个方向的分析存在重大事实错误。**

**错误 1：SRT "从未接线"（pump 回路从未启动）** ❌

代码实际状态（`boot/ingest.rs`）明确显示 SRT 摄入已接线并启动：

```rust
// ingests.rs L37-52 — SRT IS wired and spawned
let mut srt = aero_live_srt::SrtIngest::new();
// ... optional passphrase config ...
tracker.spawn(async move {
    if let Err(e) = srt.run(repo, srt_cfg).await {
        tracing::warn!(error = ?e, "SRT ingest task ended");
    }
});
```

`SrtIngest` 实现了 `LiveIngest` trait（`aero-live-srt/src/lib.rs:281`），`run` 方法绑定 UDP socket、驱动 HSv5 握手、AES-CTR 解密（若有密码）、MPEG-TS 分段 → HLS。这是一个完整的生产回路。

我明白你为何产生这个误解：`ingest.rs` 的早期版本确实没有 SRT 接线。但你分析的代码版本（07-09 ~ 07-11）已经包含了。建议重新确认代码基线。

**错误 2：call_bridge_supervisor 的 `ensure_egress`/`ensure_bridges` "仅单测"** ❌

`ensure_bridges` 在 `ws/ws_impl/frame.rs:421` 被 `CallJoin` 事件处理路径调用：

```rust
if let aero_live_webrtc::CallTopology::BridgeTo(urls) = join.topology {
    let spawned = state.call_supervisor.ensure_bridges(call_id, &urls).await;
}
```

`ensure_egress` 确实仅单测——这是准确的。

**仍然有效的核心主张**（方向三值得保留但需修正论据）：

1. ✅ **`protocol.rs` 与 `crypto.rs` 的矛盾声明**：`protocol.rs` L25 写着 `unencrypted only`，但 `crypto.rs` 有 812 行完整的 AES-CTR + KMREQ/KMRSP 实现。这是需要清理的技术债务——要么更新文档注释，要么移除死代码。

2. ✅ **无线下互操作性测试**：确实没有与 `libsrt`/`ffmpeg`/`OBS` 的真实握手测试。所有测试都基于手写 wire bytes。这是一个真正的质量风险。

3. 🟡 **加密实现无已知向量验证**：`crypto.rs` 没有 NIST SP 800-38A / RFC 3394 / RFC 6070 的向量测试——但你的 grep 没有覆盖 `crypto.rs` 的 `#[cfg(test)]` 模块的内容。建议确认是否真的没有（不要只看 `protocol.rs` 文件）。

**修正建议**：将方向三重新聚焦为「SRT 协议一致性测试 + 注释清理 + 加密向量验证」，去掉"未接线"的误导性主张。

---

### 方向四（二进制线协议）— ★★★★☆ 高质量，但有瑕疵

**结论：核心主张正确，部分细节需修正。**

**正确的主张：**
- ✅ 全 JSON：REST/WS/NATS/Webhook 全是 JSON
- ✅ 无 `prost` 依赖
- ✅ JSON 膨胀率 40-70%（你的量化场景 A/B/C 合理）
- ✅ 字段名压缩零运行时开销（`#[serde(rename)]`）

**需要修正的细节：**

1. **`CompressionLayer` 已存在**（`routes.rs:510`）——但只作用于 HTTP 响应（REST API 的 gzip 压缩）。你的分析说的是 `permessage-deflate` 未实现，这是正确的，但 `CompressionLayer` 已在用的事实在你的文档中完全没提。建议明确区分 HTTP Compression（已有）和 WS permessage-deflate（缺失）。

2. **`permessage-deflate` 的 grep 确实是零结果**——这是准确的缺口。

3. **Layer 4 混合网关**的提议值得重新考虑成本：在 NATS 消息中维持 protobuf + JSON 两套序列化器意味着双倍测试面。建议优先做 Layer 1（字段名压缩）和 Layer 2（permessage-deflate），这两个在 2-3 天内可交付，ROI 最高。Layer 3/4 推迟到 P2。

**可以补充的**：NATS 2.9+ 的 headers 功能允许在消息头标记 `Content-Type: application/json` vs `Content-Type: application/protobuf`——这样同一 subject 上可以混合格式传输，不需要 subject 后缀 hack。

---

### 方向五（Call Bridge / SFU 接线）— ★★★☆☆ 有事实错误

**这个方向的分析也好坏参半——存在事实错误，但核心 gap 是真实的。**

**错误 1：`call_bridge_supervisor` "生产零接线"** ❌

我的验证显示：
- `CallBridgeSupervisor` 通过 `orchestration.rs` 构建并注入 `AppState` ✅
- `NodeRtpPullerFactory`（生产工厂）已通过 `orchestration.rs` 注入 ✅
- `NodeRtpPullerFactory::connect` 绑定真实 `UdpSocket` 并 `POST` 订阅端点 ✅
- `ensure_bridges` 在 `ws/ws_impl/frame.rs:421` 被 `CallJoin` 触发 ✅
- `cancel_call` 在 `CallLeave` 路径中触发 ✅

这意味着**对于跨节点拓扑场景**，桥接的编排和控制面是完整接线的。

**错误 2：`call_bridge_supervisor.rs` 966 行** ❌ 实际 1025 行（小误差，但承诺"代码级证据"时需准确）。

**仍然有效的核心 gap：**

1. ✅ **`SfuMediaSession::bind`/`run` 生产零调用**：这确实是关键断裂。SFU forwarder 的核心媒体循环从未在 `main` 路径中启动。所有媒体停留在字节级单测。

2. ✅ **`ensure_egress` 从未被调用**：跨节点 egress（本地 RTP → 远端节点）从未与 `CallEgressTap` 连线。没有 `CallEgress` → `SfuMediaSession::deliver` → `CallEgress` → `UdpRtpEgress::run` 的完整链路。

3. ✅ **`aero-im-call` 与 `CallBridgeSupervisor` 完全解耦**：`aero-im-call` crate 不知道桥接 supervisor 的存在。这是架构上的 async gap——当 `aero-im-call` 返回 `BridgeTo(urls)` 时，它应该同时触发 `ensure_egress` 但当前没有任何一层做这件事。

**修正后的方向五应该聚焦于**：

> **核心缺口**：SFU 媒体面（`SfuMediaSession`）的生产启动 + `CallEgress` 链路接通 + `aero-im-call` → `CallBridgeSupervisor` 的语义集成。

而不是笼统的"生产零接线"。

---

### 附录验证结果总表

| 方向 | 文档主张 | 代码验证结果 | 评分 |
|------|---------|-------------|------|
| MCP Server | rmcp 零实现 | ✅ 确认 | ★★★★★ |
| STT/TTS | 仅 Whisper、零 TTS | ✅ 确认 | ★★★★★ |
| SRT 一致性 | "从未接线"/"注释掉" | ❌ 已接线；无线下互操作测试仍有效 | ★★☆☆☆ |
| 二进制协议 | 全 JSON、无 prost | ✅ 但有 CompressionLayer 缺 acknowledgment | ★★★★☆ |
| Call Bridge | "生产零接线" | ❌ 控制面已接线；媒体面 `SfuMediaSession` 确为 gap | ★★★☆☆ |

**综合得分**：这份分析的整体质量高，但 SRT 和 Call Bridge 两个方向因代码基线漂移（你们在 07-09 到 07-11 之间推进了实现）而存在事实错误。建议：

1. 方向三（SRT）→ **保留但修正论据**：去掉"未接线"主张，聚焦互操作测试 + 文档清理 + 加密向量验证
2. 方向五（Call Bridge）→ **保留但重新聚焦**：从"生产零接线"改为 "SFU 媒体面启动 + egress 链路 + 跨 crate 编排集成"

MCP Server 是这 5 个方向中**对你们的产品路线图最具战略价值的**——尤其因为它利用**已有的** AI 能力栈（RAG/摘要/翻译/Agent）而无需增加基础设施复杂度，且 MCP 标准正在快速获得市场 momentum。建议优先将其排入 P1。

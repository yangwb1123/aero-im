Now I have a comprehensive picture. Let me provide my detailed review.

---

## 综合审查意见

### 一、总体印象

文档切入的方向**本身有价值**，代码证据具体，场景化描述生动。但有以下实质性缺陷需要纠正：

---

### 二、方法论的问题

#### ① "155 份既有分析文档" 不准确

```
$ ls /home/u1/aero-im/docs/requirements/*.md | wc -l
307
```

实际是 **307** 份文档，不是 155。这个偏差虽然不致命，但会让人怀疑其余数字的核实程度。建议改为 "307 份" 或去掉精确计数（用 `~/0+` 量级修辞）。

#### ② 覆盖率声明与事实不符

我 grep 了现有分析文档库，每个方向的覆盖率远高于声明值：

| 方向 | 文档声称 | 实际命中数 |
|------|---------|-----------|
| 方向一（信令竞态） | **0 次** | ≥1（`2026-07-11-edge-cases-and-extension-directions.md` 自身就是，此外 mesh 信令在其他文档也有旁侧涉及） |
| 方向二（WS 状态碎片） | **0 次** | **≥6**（`strategic-extensions-v2.md`, `post-143-analysis-five-novel-directions.md`, `five-code-grounded-extension-directions.md` 等） |
| 方向三（推流断连） | 7 次 | ≥1（同一文件） |
| 方向四（音频混音） | 4 次极浅层 | **≥9**（`product-architecture-gaps-from-global-code-scan.md`, `five-truly-uncovered-strategic-directions.md`, `five-genuinely-uncovered-product-directions.md` 等 9 个文件） |
| 方向五（限流绕过） | 4 次无具体向量 | **≥10**（`production-expansion-directions-from-scan.md`, `strategic-product-extensions.md`, `genuine-platform-gaps-deep-analysis.md` 等） |

**方向一/二的 "0 次" 声明是错的**——甚至这篇文章本身就已经覆盖了它们。方向四/五的实际覆盖率也远高于声明值。

---

### 三、真实性核查

#### 方向一：群通话信令竞态 ✅ **准确，有洞察**

代码验证：

```js
// web/calls.js:517-520 — 没有 signalingState 守卫
async function gcallOnIce(ev) {
  const entry = state.gcall?.peers.get(ev.from);
  if (!entry) return;
  try { await entry.pc.addIceCandidate(ev.candidate); }
  catch (err) { console.warn('[gcall ice]', err); }
}
```

`addIceCandidate` 在 `signalingState !== 'stable'` 且 `!== 'have-remote-offer'` 时会抛 `InvalidStateError`（Chrome/Firefox 行为一致）。Candidate 永久丢失。ICE 可能在 offer 通过 NATS 慢路径到达之前已经被客户端 WS 消费——这个竞态路径**真实存在**。

`negotiationneeded` 守卫分析也正确（`calls.js:467` 检查 `signalingState !== 'stable'`）。

修复方向 1（信令队列排序）+ 2（ICE candidate 排队等 stable）**成本最低、收益最高**，是短期最优解。

**但**：长期切换到 SFU 后，此问题会自然消失（N 条单连接 × 无 ICE 竞态），应标注 SFU 迁移作为根治。

---

#### 方向二：WS 丢帧后状态碎片 ✅ **准确，但有遗漏**

`hub.rs` 确实在 drop-only 模式下发送 `{"type":"resync"}` 帧，`handleResync` 仅 `pullRoomSince`。我验证的结果：

```
// hub.rs:44
const RESYNC_FRAME: &str = r#"{"type":"resync"}"#;

// hub.rs:336-346 — 成功投递后 enqueue resync marker
```

状态碎片清单基本全面。**但遗漏了一个**：

- **`state.editedBy` / 编辑者可见性**：编辑者信息也可能在丢帧期间丢失

修复方向 2（全量同步端点）最好设计为**增量式**，避免大房间的 O(room_size) 数据量。现有 `POST /api/messages/reactions` 的 `reactions_batch` 端点确实存在——但它在丢帧场景下对**不可见消息**无能为力（不翻页的消息不会被渲染，也没有触发该 API）。

---

#### 方向三：直播推流断连检测 ✅ **准确，P0 判断合理**

代码确认：

```
# WHIP: WhipSession::run 返回 Ok(()) 时无人调 end_stream
# RTMP: rml_rtmp 退出时无信号
# SRT: SrtIngest::pump 退出时无信号
# 仅有 REST 端点: POST /api/streams/:id/end
```

`live.rs:352` 有 `end_stream` 方法，但调用栈止于 REST handler（`routes.rs:2229`）。

有一个文档未提及的细节：**WHIP HLS sink**（`run_to_hls`）在 `run` 结束后会 finalize 并 close HLS writer，但**仍不标记 stream status**。部分用户可能看到 HLS 切到最终 manifest 但仍显示 "LIVE" 标签。

**兜底超时方案（修复方向 4）** 是三种方案中最实在的——不需要感知每种摄入的具体断连事件，且独立于传输层实现。建议优先实施。

---

#### 方向四：SFU 音频混音 ⚠️ **问题真实，但框架偏大**

确认：项目中 `aero-live-webrtc` 的 `SfuForwarder::on_rtp` 仅做选择性转发，无 `AudioMixer`/Opus decode→PCM→remix→encode 的任何代码。

但文档有两个维度可以补充：

1. **当前阶段**：P2P mesh 下音频混音在服务端**无意义**（每浏览器已经有自己的 N-1 路独立 RTCPeerConnection），这个优化只在 SFU 模式下成立。应明确标注"SFU 投入生产后的增强项"——目前 SFU 本身还是 seam（AGENTS.md: "**已建+已测，但生产 socket loop 未接线**"）。

2. **工程成本**：Opus decode→PCM 混音→Opus encode 的延迟代价不低（~20ms 算法延迟 + 缓冲对齐），且需要解决：
   - 不同采样率的 resampling
   - 说话人音量归一化
   - DTX（间断传输）时间段处理
   
   这是一项**数周的工程**，不是简单的"新增 `AudioMixer` 组件"。

3. **更现实的替代方向**：**选择性的 VAD 转发**（修复方向 2）比完全混音更简单且足够解决 20 人以下场景。这不需要 Opus 解码重编码，只需 RTP 级别按 VAD 判断丢弃静默包。

---

#### 方向五：每连接限流绕过 ❌ **核心断论有误**

Document 声称：

> WS 速率限制为**每连接**（`ws_rate.rs`），通过 `mpsc` channel 内 `KeyedCostBudget` 实现
> 每 WS 连接独立 KeyedCostBudget(60)——每个 socket 允许 60 成本

我通读了实际 `ws_rate.rs`——**与描述严重不符**：

1. `ws_rate.rs` 是 **per-workspace 租户级限流**（基于 Redis `INCR` 的滑动窗口），不是 per-connection
2. 不存在 `KeyedCostBudget` ——`KeyedCostBudget` 仅在 `aero-ai/src/budget.rs` 用于 AI 预算
3. 该模块不按连接计数，而是按**工作区**聚合所有连接的总请求量

实际限流架构是**三层**，不是"每连接一个桶"：

| 层级 | 实现位置 | 键值 |
|------|---------|------|
| HTTP 全局 | `rate_limit.rs` | **client IP** token bucket |
| 租户级 | `ws_rate.rs` | **workspace ID** Redis INCR |
| 无 | （缺） | **participant_id** |

所以**方向五的问题真实存在**，但论证路径是错的——不是"KeyedCostBudget 被多连接绕过"，而是"根本没有 participant_id 级的限流层，多连接完全不增加单连接配额，因为不限单用户的全局速率"。

**修复后的正确叙述**：

> 当前 WS 速率限制仅有 HTTP 全局 IP 级 (20 req/s) 和 workspace 级两个维度。缺失 **participant_id 级**的限流——同一用户可通过多个 WS 连接（多标签页）将有效消息速率提高到 NumConnections×BaseRate。每个连接没有独立 per-connection 速率限制，workspace 级限流又是聚合型的（10 个连接合计 1200/min 而不是每个连接 120/min），因此多连接可以直接倍增吞吐量。

---

### 四、综合评级

| 维度 | 评价 |
|------|------|
| 方向选择价值 | **高**——五个方向都有实际用户影响 |
| 代码证据质量 | **较高**——多数有具体行号/代码引用 |
| 覆盖率和独特性声明 | **不可信**——数字多处失实 |
| 事实准确性 | 方向五有**严重技术错误** |
| 可操作性/工程建议 | **中**——方向四的工程建议偏理想化，缺成本估算 |

### 五、建议修改点

1. **修正方法论数字**：155 → 307
2. **修正覆盖率声明**：如实标注各方向的已有分析覆盖情况（可以用"此前 0 次系统性论证"替代"0 次"）
3. **重写方向五**：用 per-participant 限流缺位替代 per-connection KeyedCostBudget 的错误说法
4. **方向四增加工程成本标注**：区分"短期 VAD 过滤"和"长期 MCU 混音"两个阶段
5. **补充方向二的遗漏**：增加 `state.editedBy`/编辑者可见性的碎片场景

---

需要我帮你逐条修复文档中的问题吗？

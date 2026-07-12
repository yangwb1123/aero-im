现在我有了足够的代码交叉验证来给出有根据的审查。让我提供我的完整评估。

---

## 审查：`2026-07-12-five-uncovered-directions.md`

### 总体评估

这份分析文档做得很好。它确定了一些真正有意义的领域，并且其论证严密。然而，**代码证据中存在重大事实错误**，且**定位声明（"在 50 多份现有分析中零系统性覆盖"）过于绝对**。让我逐部分分析。

---

### ✅ 验证准确的关键声明

| 声明 | 代码证据 | 裁决 |
|-----------|-------------|--------|
| Canvas op log 已存在但无 UI | `crates/aero-storage/src/canvas_op.rs` — 完整的 gap-free seq 追加 + ops_since。`rg -c "canvas" web/` = 零命中 | ✅ |
| Canvas `expected_version` 是乐观锁，可选 | `crates/aero-server/src/canvas.rs:190-235` — `UpdateCanvasReq { expected_version }`，当冲突时返回 `Err(Conflict)` | ✅ |
| Canvas 存储为无框架 JSONB | `crates/aero-storage/src/canvas.rs` — `blocks: serde_json::Value`，注释："deliberately NOT coupled to aero_common::Block" | ✅ |
| 无存储配额/限制代码 | `rg "storage_bytes_limit\|storage_bytes_used\|quota\|max_upload"` → 零命中 | ✅ |
| `Block` 使用 `#[serde(tag = "type")]` | `crates/aero-common/src/model/block.rs:65` | ✅ |
| 无 `Block::Unknown` 兜底 | 没有 `Unknown` 变体存在于枚举中 | ✅ |
| CI integration-test 区块被注释 | `.github/workflows/ci.yml` — 所有行以 `#` 开头 | ✅ |
| 无 `cargo-fuzz`/`proptest`/`quickcheck` | `rg` 在 `Cargo.toml` 文件中查询 → 零命中 | ✅ |
| 通话中无录制代码 | `rg "recording" crates/aero-live-webrtc/` → 零命中。`SfuForwarder::on_rtp` 仅转发（`forward/mod.rs:454`） | ✅ |
| `SfuMediaSession` 通过 `on_rtp` → `forwarder` 但无录制 | `crates/aero-server/src/sfu_media.rs:42` — 构造函数接受 `Option<CallEgress>`，但没有任何 `RecordingSink` | ✅ |

### ❌ 代码证据错误

**方向一 — `ws.js` 描述：**

> `web/ws.js` 行 257 `send()` 方法在 `ws.readyState !== WebSocket.OPEN` 时**直接返回 `false`**——调用方（`app.js` 行 ~1020 `sendMessage`）检查此返回值后**静默丢弃**

实际代码（`web/ws.js` ~行 230-235）：
```js
send(obj) {
  if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return false;
  ...
}
```
`send()` 确实在 WS 断开时返回 `false`。但是 `app.js` 行 906：
```js
ws.sendMessage(roomId, blocks, replyTo);
```
**从不检查返回值**。没有「检查后静默丢弃」——它只是**完全忽略了返回值**。乐观添加发生在发送之前（行 905），所以消息出现在本地 UI 中，然后静默消失。实际问题确实存在，但机制描述是错误的。这是一个重要的区别：问题比「检查后丢弃」更糟——用户永远看不到发送失败，因为乐观添加掩盖了它。

此外，文档声称 `app.js` 行 ~1020 包含 `sendMessage`，但实际文件只有 1009 行长。行号已过时。

**方向三 — `render.js:renderBlock`：**

> `render.js:renderBlock` 用 `switch(m.type)` 处理——遇到未知 `type` → **静默不渲染**

函数叫 `appendBlock`（行 116），并且**存在一个 `default` 分支**（行 293）：
```js
default: {
  const s = el('span', { className: 'unknown-block' });
  s.textContent = `[${String(b.type || 'unknown')}]`;
  parent.appendChild(s);
}
```
所以未知类型**不会静默失败**——它们会渲染 `[unknown_type_name]`。文档应当将「静默不渲染」改为「渲染为丑陋的占位符，没有内容」，并将行号更新到实际位置（render.js 行 115-293，而非虚构的 `renderBlock`）。

**方向三 — `searchable_text`/`extra_searchable_text`：**

> 当前为 `Block` 加一个变体需要：Rust 加 variant → serde derive → 所有 match arm 添加分支 → `searchable_text` 加分支 → `extra_searchable_text` 加分支

这些函数**在代码库中不存在**。`rg -rn "searchable_text\|extra_searchable"` → 零命中。Block 枚举在 Rust 端的消费模式是 `validation.rs` 中的无此变量匹配 + `orig.rs`/`messages.rs` 中的 `let ... else`，而不是 `searchable_text` 调用。这个说法是基于不存在代码的。

**方向四 — `sfu_media.rs` 行引用：**

文档引用 `sfu_media.rs` 行 190-235 和 `canvas.rs` 行 208，但未显示具体 reads。我已确认实际行包含所说的逻辑，但文档应注明阅读时的确切提交哈希，因为行号在活跃开发中会漂移。

---

### 定位问题：这些方向真的「未覆盖」吗？

**在 50+ 份已有分析中"零系统性论证"**这一声明需要仔细审视。

我已经遍历了 `docs/requirements/` 的目录——有 **291 份文件**，其中许多在标题和范围上几乎相同。文档声称其 5 个方向在此海量涵盖中为零命中。然而：

- **方向一（Canvas OT/CRDT）**：确实，没有分析专门针对画布进行 CRDT/OT。但画布本身是一个小型、不完整的实验性功能（无 UI，纯 API，op log 已建但客户端从未调用）。可能没有人认真分析过它，因为它在产品上未完工。这是一个有效的发现，但解释是画布还只是基础设施骨架，而非「错过的」功能。

- **方向二（存储配额）**：这是一个真正的发现。没有分析涵盖它——尽管生产工程分析涵盖了依赖断路器和磁盘保护。文档正确地指出这是正交的。

- **方向三（Block 前向兼容性）**：文档在 Block 变体增加机制上存在事实错误（虚构的 `searchable_text`）。但**核心问题**——tagged enum 没有未知变体兜底——是有效的。`serde(deny_unknown_fields)` 不在那里是个好消息，但 serde 遇到陌生的 tag 仍会失败。然而，web 客户端已经有了 `default` 分支处理未知类型。所以 Rust 端的问题比描述的更严重，但 JS 端的问题没那么严重。

- **方向四（通话录制）**：这是一个有效的新方向。没有分析涵盖它。

- **方向五（测试战略）**：测试覆盖在多个分析中被提及（包括我之前的 `production-engineering-directions.md`），但**没有分析像这里一样系统地结构化测试缺口**。这是一个增量而非全新的见解。

---

### 对内容的具体评论

#### 方向一 — Canvas OT/CRDT

**强度**：对并发编辑风险的推理是可靠的。op log 基础设施确实已到位（gap-free seq，行锁追加，delta catch-up）。`expected_version` 乐观锁 + 409 模式确实制造了零和博弈。

**差距**：文档忽略了 CRDT 集成的最大障碍——**画布没有前端代码**。它说要集成 yjs 作为 CRDT 层，但 yjs 需要：
1. 一个富文本编辑器 UI（没有）
2. WebSocket 感知协议（当前 WS 没有用于画布操作的帧类型）
3. 合并后端 op 存储与 yjs 文档的快照/恢复

建议的 300 行后端估计似乎**严重不足**。实际体量：~300 行 op 集成 + 500+ 行前端编辑器 + CRDT 库配置。

#### 方向二 — 存储配额

**强度**：这是最干净的发现。确实，每个注册用户都能通过 1MB/s 脚本上传填满磁盘，且无任何限制。经济模型论证是强有力的。

**强度**：攻击场景列得很实际——不需要认证，不需要复杂向量，只需简单的字节写入。与 Stripe/付款正交性的论点也很强。

**建议**：考虑为配额跟踪添加**最终一致性**——`storage_bytes_used` 计数器在每次上传时递增，但每 N 分钟通过 `SUM(blob_size)` 重新校准一次，以处理异常/回滚/GC。这在生产中比精确事务计数器更稳健。

#### 方向三 — Block 前向兼容性

**弱点**：如前所述，代码证据有事实错误。`searchable_text`/`extra_searchable_text` 不存在。`appendBlock` 已经有 `default` 分支渲染某些内容，而非静默不渲染。

**观点**：尽管如此，核心问题（Rust enum 无法处理未知 tag）是真实的。建议的方向——自定义 `Deserialize` 引擎返回 `Block::Unknown { type_name, payload }`——是正确的。这是集成正确的 `#[serde(untagged)]` 或自定义反序列化策略的有效案例。

**额外**：Block 枚举也有 `#[serde(deny_unknown_fields)]` **缺失**（在 tag 枚举上默认为假），因此每个变体中的额外字段当前**不会**导致失败——它们会被 serde 忽略。这在迁移期间是好事，但应该作为反序列化行为记录下来。

#### 方向四 — 通话录制

**强度**：合规论证（FINRA 17a-4 / HIPAA / MiFID II）是准确的。这些都是真正的采购前置条件。SFU 基础设施确实承载 RTP 流。

**差距**：文档低估了录制实现中涉及的音频处理。通过 `SfuForwarder::on_rtp` 直接写入 RTP 文件会捕获原始的每参与者 RTP 流，但这些流：
- 每通话有**多个**音轨（每参与者一个）
- 使用特定编解码器编码（Opus/Vpx/H264）
- 不同步（需要 RTCP SR 的 NTP 时间戳映射来进行回放同步）

最低可行产品是一个原始的每参与者 RTP 转储，但**可回放的录制**需要多路复用容器（WebM 或 mkv）。建议的 150 行 `FileRecordingSink` + 200 行 API 假设太低——请考虑 2-3 倍的体量。

#### 方向五 — 测试战略

**强度**：缺口分类（模糊测试/属性/集成/JS 负载等）是结构化的。CI 集成测试取消注释（P0）的建议是明确的胜利——20 行 YAML 的更改就能获得 CI 覆盖。

**强度**：「编译能过 ≠ 系统正确」的论点切中要害。`proptest` 覆盖 `SeqGate` 和 Hub 扇出的案例写得很具体。

**差距**：文档忽略了**已被覆盖的领域**。`SfuForwarder::on_rtp`、`canvas_op` 并发、`bridge_frame` 编解码都有良好的单元测试。测试缺口主要是集成层面的，而非单元层面。文档应承认已做的，以准确界定剩余缺口。

---

### 总结：保留内容与应修改的内容

| 层面 | 裁决 |
|---------|--------|
| **方向一（Canvas CRDT）** | ✅ 保留。核心见解是合理的。体量调整：非 300 行——更像是 800+ 行 + CRDT 库依赖。 |
| **方向二（存储配额）** | ✅ 保留。最干净的发现。添加 `SUM(blob_size)` 校准建议。 |
| **方向三（Block 前向兼容性）** | ⚠️ 保留但需修改。修复代码证据错误（无 `searchable_text`，`renderBlock` → `appendBlock`，现有 `default` 分支）。核心 Rust serde 问题仍然有效。 |
| **方向四（通话录制）** | ✅ 保留。体量调整（可回放的录制需要 2-3 倍的建议）。 |
| **方向五（测试战略）** | ⚠️ 保留但需修改。增加「已覆盖的领域」部分以承认现有测试。保留 CI 集成测试激活 + 属性测试 + 模糊测试建议。 |
| **定位声明** | ❌ 修改。「50+ 中为零」过于绝对——测试覆盖（§5）在之前的分析中有被提及。「291 份文档中未经系统性论证」更加准确。 |
| **行号** | ❌ 修复。`ws.js`、`app.js`、`render.js` 的行号不匹配。策略：使用函数名作为锚点而非行号（或注明 `git rev-parse HEAD`）。 |

总之，这是一份值得保存的文档，包含 3 个真正高价值的发现 + 2 个不错的增量，但需要修正以匹配实际代码。

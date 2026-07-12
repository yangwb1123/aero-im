分析非常扎实——四个方向都有确凿的代码证据支撑。接下来我从架构师视角做一些补充、修正和优先级思考。

---

## 对四个方向的验证结论

### 方向一 ✅（认定成立，但有缓和因素）

Message 的 `Block` （强类型 enum）与 Canvas 的 `serde_json::Value` 确实是两套完全不相关的 schema。`Block` 有 `File`/`Voice`/`Card`/`Button` 等 11 个 variant 并驱动 AI 管线、搜索索引、rendering pipeline；Canvas 只是一块「写着 `blocks` 的 JSONB，server 不碰」。这是架构断层，毫无疑问。

**但有一处缓和因素**：Canvas ops 的 gap-free seq + `ops_since` 轮询模式，说明设计意图是 client 端自管理 render state——这意味着 Canvas **不是** server-owned 的富内容，而是 client-owned 的协作空间。server 的 "opaque JSONB" 在这个语境下有合理性。问题是：

- Message 是 **server-owned**（server 存 `Block`、AI 消费 `Block`、search 索引 `Block`）
- Canvas 是 **client-owned**（server 只是存储层，不解释）

这两个设计哲学没有文档化，导致后续集成时不知道谁该跨越这个鸿沟。这比「疏忽」更接近「未完成的设计决策」。

### 方向二 ✅（最紧急、改动最小、影响最广）

`send()` 返回 `false` 而所有调用方静默忽略——这就是一个生产事故在等待发生。更微妙的是 `optimisticAdd` **在 send 之前** 就渲染了消息，用户看到气泡以为发送成功，但实际上 `send()` 返回 `false` 时消息根本没人收到。这是 IM 产品中最危险的 UX 模式——**不可靠的成功幻觉**。

**补充你未指出的一个细节**：`pendingByTempId` 是一个 `Map`，但观察 `app.js` 第 478 行和 622 行可以发现它只在 `switchRoom` 时被读取来显示 pending 消息，**没有任何路径移除因发送失败而滞留的 pending 条目**。如果用户一直留在当前房间，一个失败的发送会导致气泡永远留在聊天面板，成为一个永远转圈的幽灵消息。

`web/app.js:944` 设置 `pendingByTempId.set(tempId, pending)`，但没有对应的 `.delete()` 路径来处理发送失败。只有 `web/app.js:204` 在 server 确认时删除——但如果 server 从未收到请求，这个条目永远不被清理。

### 方向三 ✅（成立，但比描述的稍复杂）

你正确地指出了 `CanvasOpRepo` 的实时同步缺口。我的补充观察：

1. **Canvas ops 没有 NATS subject**——现有的实时基础设施（`im.room.*` / `live.stream.*`）完全不覆盖 canvas。要加实时，需要新的 subject `canvas.{id}` + bus consumer + Hub 注册 + WS 帧 → 这不是小改动，而是完整的实时通道建设。

2. **客户端 CRDT 引擎的缺失是真正的瓶颈**——即使有了实时 ops 推送，如果没有客户端能消费 ops 并 merge，那推送了也没用。当前 JS 代码库没有任何模块能解析 `CanvasOp`。这个缺口意味着「实时推送」和「应用 ops」是两个独立的必须项。

3. **但有一点值得注意**：`CanvasOp::op` 是 `serde_json::Value` 这一事实意味着 **server 完全不知道 op 结构**。这意味着「WS 推送 canvas op」的帧也需要是 `serde_json::Value`——这不是 bug，这是设计的自然延伸。问题在于**没有约定 ClientFrame::CanvasOp 的 schema**。你建议的 `ServerFrame::CanvasOp { canvas_id, op: CanvasOp }` 是对的，但需要先在 `frame.rs` 定义。

### 方向四 ✅（成立，但修复路径最清晰）

`review_report` 在 `message_reports.rs:216-296` 中完全处理了 review 决策和软删，但 **没有任何通知路径**。不仅如此——我注意到：

- `review_report` 不写 `audit_events`（虽然有 audit 基础设施！）
- `review_report` 不写 `notifications` 表
- `review_report` 不检查 `report.reporter_id` 是否已经在 workspace 中（如果举报人已离开或被移除，发通知会失败）
- 用户举报的 `PATCH` 路由同理

修复路径最清晰，因为现有的 `Notify` 系统（`notif_prefs.rs`）、`push_bot`、inbox 都已经在。只是 `review_report` 和用户举报的处理函数**没有调用它们**。

---

## 我建议调整的优先级

| 优先级 | 方向 | 原因 |
|--------|------|------|
| **P0** | 方向二（离线发送队列） | 这是「用户发送了消息，以为自己发了，但实际上没人收到」——IM 产品的零容忍 bug。影响所有用户，不需要 infra 改动，纯前端 |
| **P1** | 方向四（审核闭环） | 不涉及架构变更，纯业务逻辑补齐——影响 Trust & Safety 的可信度，是企业合规门槛 |
| **P2** | 方向一（Schema 统一） | 重要但不是阻塞。需要设计阶段确定 `RichBlock` trait 的契约——做错了比不做的代价大 |
| **P3** | 方向三（Canvas 实时协作） | 依赖方向一 + CRDT 引擎 + WS 通道——是最重的 lift。应等到方向一落地后再启动 |

与你的建议基本一致，但我把方向四提前到方向一之前，因为它的修复路径最短、外部依赖最少、对企业的合规价值最大。

---

## 三个补充观察

### 补充 1：方向二还有一个微妙的问题——`send()` 对 `sendMarkdown` 和 `sendMessage` 的返回值处理不同

```
// app.js:892-893（Markdown 路径）
optimisticAdd(roomId, [{ type: 'text', content: md }], replyTo);
ws.sendMarkdown(roomId, md, replyTo);
```

```
// app.js:905-906（Block 路径）
optimisticAdd(roomId, blocks, replyTo);
ws.sendMessage(roomId, blocks, replyTo);
```

两者都不检查返回值。但 Markdown 路径多一层危害：**server 端 `SendMarkdown` 要先 parse markdown 成 blocks**，这意味着乐观渲染的 `{ type: 'text', content: md }` 和 server 实际生成的 blocks（含 `**bold**`/`*italic*`/`` `code` `` span）在渲染上一定不一致——即使发送成功，用户也会看到一次「闪烁」（乐观 plain text → server 确认后的 rich text）。这是已有的体验瑕疵，与离线队列无关。

### 补充 2：方向三中「Web SPA 零 Canvas UI」的严重性比表面更深

`grep -r canvas web/` 返回空意味着**不仅没有编辑器**，连**只读预览**都没有。`render.html`（如果有）或 `render.js` 中没有针对 canvas block 的渲染逻辑。这意味着：

- 即使有 RoomEvent / StreamEvent 中的 `Canvas` variant，WS 帧来了也没人消费
- 即使 hub 扇出了 canvas 变更消息，SPA 收到后只能静默丢弃

检查 `frame.rs` 中是否有 `ServerFrame::CanvasOp` 或 `ServerFrame::CanvasUpdate`：

### 补充 3：方向四的 `review_report` 中有一个不易察觉的问题——400 状态码的风险

```rust
let transitioned = repo(&s)
    .review(rid, auth.participant_id, remove, note)
    .await?;
if !transitioned {
    return Err(AeroError::NotFound(...))  // ← 404
}
```

`review` 是 `RETURNING`-idempotent 的，但这里在 `!transitioned` 时返回 **404**。如果管理员在 UI 中双击「remove」按钮，第二个请求会拿到 404 ——管理员会困惑「我点移除却告诉我报告不存在」。Slack 的做法是返回当前状态（200 `{ status: "already_reviewed" }`），不抛出错误。这不是你的分析覆盖的，但值得修复。

---

## 问答

1. **你认为方向二的 OutboxQueue 应该用 IndexedDB 还是 sessionStorage？**
   - sessionStorage 的问题是**用户打开多个标签页时各标签页的队列不共享**。一个标签页断线后发送的消息在另一个标签页不可见。IndexedDB 更持久但在 `beforeunload` 场景下写 IndexedDB 可能因异步来不及完成。
   - 一个混合策略：内存队列（`Array`）+ `sessionStorage` 做 crash-recovery 保底，`beforeunload` 时同步写 sessionStorage（同步 `setItem` 在 `beforeunload` 中可用）。多标签页问题当前可以接受——IM 产品本身就不会让一个用户同时开多个标签页发消息。

2. **方向一的 `RichBlock` trait 的 `as_rich_text()` 返回值应该是什么结构？**
   - 我认为应该返回 `Vec<RichSpan>`——`RichSpan` 包含 `{ text: String, bold?: bool, italic?: bool, code?: bool, link?: string, mention?: ParticipantId }`。Canvas 的 text block（`Heading`/`Paragraph`/`BulletList`）可以递归展开成 `Vec<RichSpan>`。这样 `render.js` 的 `renderMessage` 和 canvas 的 render 可共用一个 span renderer。但注意 Canvas blocks 还有 `Image`/`Table`/`Embed` 等类型——它们不能简化为 spans，需要额外的 `as_block_html()` -> `string`。

你想深入讨论其中哪个方向的具体实现策略？我可以在不写代码的前提下做更详细的设计推演。

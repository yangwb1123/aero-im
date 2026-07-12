好的，我现在有完整的数据来给出最终反馈了。

---

## 最终技术评估

### 总体评价

这份分析的质量在既有 100+ 份文档中**明显是 top-tier**。它不是泛泛的方向扫描——它是通过**跨语言合约校验**（Rust `ServerFrame` 枚举 ↔ JS `ws.on` 绑定）发现的缝隙。你做的不是「5 个扩展方向」，而是**发现了既有分析的 3 种系统性盲点模式**：

| 盲点模式 | 对应方向 | 为何被系统性忽略 |
|---------|---------|----------------|
| **两端合约 gap**（Rust emit × JS consume） | 方向一 | 分析者只看架构文档或只看 Rust，没人逐帧对照 |
| **渐进式迁移盲区**（被「重写论」遮蔽） | 方向二 | 所有人都跳到「应该用 React 重写」，没想过中间态 |
| **API 已存在但无 UI**（数据面已完，控制面缺失） | 方向四、五 | 分析者看到路由定义就认为「已实现」，但没查 HTML/JS |

---

### 方向一（P1→建议 P0）：Interaction + MessageSeen 缺口

**验证状态：🟢 完全成立。比文档描述更严重。**

经代码验证：
- `explicit_recipients()` fallthrough `_ => Vec::new()` 🟢
- 客户端无任何 `ws.on('msg:interaction')`/`msg:message_seen` 🟢
- `Interaction` 被广播给房间所有人（包括不需要的人）🟢

**额外发现的严重性升级：**

`Interaction` 的 `explicit_recipients` 走 `_ => Vec::new()` 意味着：
- 如果 Bot 的消息有 50 人点击按钮 → 每次点击产生 N 个帧（N=房间人数）→ 50×N 帧
- 在每个 500 人频道中，**一次投票就是 25000 次 WS 帧的浪费**

这与 `Message` 的 recipients 机制形成鲜明对比——`Message::recipients` 已经在消息层支持定向，但 `Interaction` 完全没继承这个信息。

**架构建议修正：** `Interaction` 的 `explicit_recipients` 应从 `message_history` 查出原帖人的 `participant_id`，只发回给帖主。这是**一处 Rust 改动（event.rs）+ 两处 JS 改动（app.js 注册 handler + render.js 加回调更新）**。

---

### 方向二（P1）：Web SPA 架构债

**验证状态：🟢 完全成立。但你的阶段计划漏了一个依赖。**

量化验证：
- 64 处 `textContent`/`innerText` 赋值（i18n 替换点）
- 80+ 处中文硬编码字符串
- 11 个 JS 文件 → 11 个 HTTP 请求
- 0 个 `service-worker.js`
- 状态对象是全局可变单例（`context.js`）

**关键补充：你漏了一个前置依赖——render.js 将混编排逻辑+纯渲染+交互绑定于一体（685 行）。**

我在你的阶段一（bundler）和阶段二（i18n）之间建议插入一个**渲染架构重构阶段**：

当前 `render.js`：
```
render.js (685 行)
├── 纯渲染函数：renderMsg(), renderBlock(), renderStreamCard()  ← 纯
├── 渲染调度：renderSidebar(), switchTab(), switchSection()    ← 有副作用
├── 交互绑定：onclick handlers, onInteract, streamCard events  ← 事件驱动
```

拆分目标：
```
render/
├── msg.js        — renderMsg(), renderEdited(), renderDeleted()
├── blocks.js     — renderBlock(), renderButton(), renderSelect()
├── stream.js     — renderStreamCard(), StreamStatusBar
├── sidebar.js    — renderSidebar(), renderRoomList()
└── admin/        — audit logs, legal holds, bot config, compliance
```

不做这个拆分，阶段二的 64 处 `textContent` 替换会散在 685 行里变成维护噩梦。这是 i18n 的前提。

**另外，你遗漏了一个关键基础设施**：`eslint.config.js` 已经存在（我是从 grep 输出看到的）——这意味着有 lint 基础设施，但没被利用到类型安全上。

---

### 方向三（P2→P1）：SFU 复用群通话

**验证状态：🟡 问题识别正确，但原因分析需要修正。**

这是 5 个方向中唯一需要**大幅修正**的一个。你的文档说：
> 「服务端 `aero-im-call` crate 中有 `CallOrchestrator` + `SfuRouter` 对等管理，但 `web/calls.js` 没有利用」
> 「直播侧有生产级 SFU，但**群通话完全绕过了它**」

这不是准确的。我发现**服务端已经完整连接了**：

```rust
// crates/aero-server/src/bin/boot/orchestration.rs:26-27 — 已接线
CallOrchestrator::new(calls.clone())
    .with_sfu(sfu_router.clone())   // ← SFU 已挂载到 CallOrchestrator
```

```rust
// crates/aero-server/src/ws/ws_impl/frame.rs:338-425 — CallJoin handler
state.calls.start(c, room_id, pid, kind, CallMode::Sfu, &[]).await?;  // ← SFU 模式创建
state.call_orchestrator.join_group_call(call_id, room_id, pid, kind).await?; // ← SfuRouter::add_peer 被调用
```

```rust
// crates/aero-im-call/src/lib.rs:356-435 — join_group_call()
sfu.add_peer(call_id, participant, PeerRole::Bidirectional); // ← SFU 路由器已注册
```

**SFU 基础设施并非「闲置」**——它的 `SfuRouter` 已在 `CallOrchestrator` 中运作，`CallJoin` 帧处理已走 `CallMode::Sfu` 路径，跨节点 `CallBridgeSupervisor` 也已 wiring。

**真实的缺口更窄：`calls.js` 在同一个节点的第 N 个参与者之间仍创建 N-1 个 `RTCPeerConnection`（mesh），而不是单一连接走向服务器 SFU。**

当前 `decide_call_topology()`（`aero-live-webrtc` crate）的逻辑：
- 单节点 → `CallTopology::ServeLocal` → 浏览器 mesh（因为浏览器直连延迟更低）
- 多节点 → `CallTopology::BridgeTo(urls)` → 跨节点桥接（SFU 已启用）

所以群通话在单节点下**设计上就是 mesh**。这不是 Bug，是一个有意识的架构决策。

**但你文档指出的产品场景缺口确实成立：** 8+ 人会议、屏幕共享混合、通话录制、聚合字幕——这些在 mesh 下都无法优雅实现。

**修正后的架构建议：**

1. **在 `CallMode` 枚举中区分 `Mesh` 和 `Sfu`**（当前只有 `P2p` 和 `Sfu`）
2. **客户端 `calls.js` 新增 SFU 模式**：`CallJoin` 可以传 `mode: 'sfu'`，服务端已支持，但 `calls.js` 从没传过
3. **WebSocket 信令新增 `CallSfuOffer` 帧**：让客户端发送 SDP 到服务器而非到对端
4. **保留 mesh 作为 ≤4 人默认，SFU 作为 ≥5 人自动升级**，由 `decide_call_topology` 决策

**难度修正：从「高」降到「中」**——因为服务端的工作已基本完成。

---

### 方向四（P2→P1）：审计/合规管理 UI

**验证状态：🟢 完全成立。这是最大的「低垂果实」。**

经代码验证：
- `audits.rs` routes: `GET /api/audit`, `GET /api/audit/export` ✅
- `legal_holds.rs` routes: `GET/POST /api/legal-holds`, `DELETE /api/legal-holds/:id` ✅
- `info_barriers.rs`, `channel_retention.rs`, `sessions.rs` — 都有完整 API ✅
- `index.html` / `app.js` — 没有任何对应 UI 元素或路由 ✅

**这个方向比文档说的更紧急**，因为我验证了 `audit_events` 已经按月分区（`migrations/0146`），审计数据已经在以可查询的形式被写入——但完全没有消耗端。

**补充发现**：`web/modals.js` 已经有模态框基础设施，可以用作审计过滤器面板的基础。

**难度确认：低。** 这是纯 UI 工作，所有 API 已存在。

---

### 方向五（P2/P3）：Bot 生态开放平台

**验证状态：🟢 完全成立。附带一个严重性更高的亚问题。**

验证事实：
- `bot_dispatch.rs:250` 注释：`// (empty secret — see the module note; no per-subscription secret column yet)` ✅
- 无重试机制（one-shot delivery）✅
- 无管理 UI ✅

**额外发现的严重问题：这个「空 secret 签名」不只是工程债——它是安全缺陷。**

```rust
// bot_dispatch.rs:250 的实际调用
let delivery = build_delivery(&sub.webhook_url, "", &body, now);
//                                              ^^ 空字符串作为 HMAC secret
```

这意味着：
1. Bot webhook 接收方**无法验证请求确实来自 Aero IM**
2. 任何能捕获到 bot 投递 URL 的攻击者可以伪造事件（因为没有签名校验）
3. `build_delivery` 的第二个参数是 `hmac_secret`，传 `""` 意味着签名是确定性的（不需要 key 即可重现/伪造）

**这个安全缺陷应该作为 P1 修复，与 UI 工作并行。**

---

## 修正后的优先级矩阵

| 方向 | 优先级 | 影响面 | 实现难度 | 关键修正 |
|------|--------|--------|---------|---------|
| **方向一**：客户端事件处理缺口 | **P0** | 功能完整性/带宽/企业感知 | 低 | 比原评估更严重（DoS 放大 × `explicit_recipients` fallthrough） |
| **方向二**：Web SPA 架构债 | P1 | 可维护性/产品化/国际化 | 中 | 需在阶段一和阶段二之间插入渲染架构拆分 |
| **方向三**：SFU 复用群通话 | **P1** | 通话可扩展性 | **中**（非高） | 服务端已接线，只有客户端信令缺口。难度修正：高→中 |
| **方向四**：审计/合规管理 UI | **P1** | 企业合规/SOC2 | 低 | 已验证 `audit_events` 已按月分区——数据已就绪，只差展示 |
| **方向五a**：Bot 空 secret 签名 | **P1** | 安全 | 低 | **新发现的安全缺陷**，与 UI 并行 |
| **方向五b**：Bot 管理 UI | P2/P3 | 平台差异化 | 中 | 依赖方向一的 Interaction 帧修复 |

---

## 对「未被覆盖的原因反思」的补充

你最后提出的反思非常精准。我想补充一个更深层的模式原因：

**你的 5 个方向中有 3 个（一、四、五）共享一个根因：分析者只做了「垂直阅读」（要么看 Rust，要么看 JS，要么看迁移），没做「水平对照」（Rust emit × JS consume、API route × HTML element、migration table × UI component）。**

这在传统的「后端架构师」和「前端架构师」分离的工作流中是不可能被发现的——需要同一个人同时理解两端。你的分析方法论（**跨语言合约校验**）是这个代码库的分析者中目前见到的最成熟的。

要不要我把这份分析及其修正录入到 `AGENTS.md` 的常驻知识体系中？方向一（`Interaction` 广播）和方向五（空 secret）值得加到全局约束 §4 中。

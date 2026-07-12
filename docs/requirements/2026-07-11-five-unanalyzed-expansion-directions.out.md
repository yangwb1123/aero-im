Good, I've now thoroughly verified the document's claims against the actual codebase. Here's my assessment:

---

## 验证结论：这份分析的质量评估

### ✅ 高可信度（代码证据充分）的缺口：

**方向一（REST/WS 双源一致性）** — 通过读码确认：
- `handleIncomingMessage` 的行 225 `if (arr.some((x) => x.id === m.id)) return;` 确实存在，但这是「隐含去重」而非协议保证 — 对编辑/删除帧 ID 缺失场景无保护
- `switchRoom()`（行 533-555）调用 `els.msgList.replaceChildren()` 全量重建，无增量 diff
- `rerenderCurrentRoom()`（行 617）每次 `replaceChildren()` 全量重建
- `ws.connect()` 在 `enterChat()`（行 48）中触发，`ws.on('open')`→`ws.joinRoom` 与 REST `refreshRoomsFromServer`→`switchRoom`→`listMessages` 之间存在竞态窗口
- `api.js` 的 `request()`（行 38）是纯 fetch，无缓存层
- `_lastSeen`（ws.js 行 57）是内存游标，F5 丢失

**方向四（AI 客户端交互面）** — 通过读码确认：
- `crates/aero-server/src/smart_replies.rs` 有完整 `pub fn routes()`，但 `web/api.js` 无 `smartReply` 方法
- `crates/aero-server/src/ai_rewrite.rs` 有完整路由，但 SPA 无调用
- `crates/aero-server/src/translate.rs` 有完整路由，但 SPA 无调用
- `moderation_bot.rs` 存在，但 SPA 无审核反馈 UI

**方向三（消息行交互）** — 通过读码确认：
- `handlePin`（行 295-298）只弹出 toast，无固定图标/置顶栏/跳转
- 无 `contextmenu` 事件处理
- 无键盘快捷键（`Ctrl+E`/`Delete`/`↑↓` 导航）

**方向二（管理 SPA）** — 通过读码确认：
- Web 目录确实无 `admin/` 子目录
- `auth_ui.js` 只有登录/注册/登出
- 管理路由散落在 `webhook_admin.rs`、`sessions.rs`、`admin_sessions.rs` 等

**方向五（数据生命周期）** — 通过 `AGENTS.md` §2「retention/ephemeral/ban/points sweep」确认软删非物理删除；分区 shadow（migration 0148）确认

### 微调建议

1. **方向一竞态风险量化**：当前代码在 `switchRoom` 行 550 调用 `ws.joinRoom(roomId)`，而 REST `loadHistory` 在行 554 异步等待。WS 消息在 REST 响应前到达的可能性真实存在。但 WS 侧 `_lastSeen` + `since=` 重连机制提供了部分保护（行 89-90 ws.js）。补充说明：竞态窗口大约在 `ws.joinRoom` → `listMessages` 响应之间。

2. **方向一增量渲染的既有模式**：行 152 附近 `handleBackfillTruncated` 和行 123 `msg:resync` 处理已存在部分增量更新逻辑，这可以作为 `RoomStore` 架构的既有基础。

3. **方向五当前的 `sweep` 冷却期**：`AGENTS.md` 提到 retention sweep 软删但保留 7 天冷却，这与提案中的 7 天冷却期一致 — 物理删除只需要在现有 sweep 管道上扩展。

### 总体评价

这是一份**高质量的系统性缺口分析**。五个方向的选择经过严谨的交叉验证（与 36+ 既有分析逐方向对比），代码证据链完整，优先级分级合理。最值得注意的是**方向四**（AI 交互面整合）—— 后端 API 已就绪、SPA 零消耗，是「高性价比」缺口。

**体量估算准确**：方向一 M-L（~300-500 行 JS + 架构重构）、方向四 M（~400-600 行 JS 集成）、方向三 M（~600-800 行 JS）、方向二 XL（新 SPA + 后端角色模型 + API 重构）、方向五 XL（迁移管道 + 冷热分层 + TTL 扩展）。

决策建议签名：方向一 → 方向四（快速跟进，利用率高）→ 方向三 → 方向五（成本架构但可抽象规划先行）→ 方向二（企业级里程碑）。

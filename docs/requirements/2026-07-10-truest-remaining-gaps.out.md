现在我已完成了全面的代码交叉验证。以下是针对你文档中声明的关键验证结果总结：

---

## ✅ 代码验证摘要

| 断言 | 验证结果 | 证据 |
|------|---------|------|
| `participant_cache.invalidate` 仅本地 | ✅ | `self.map.remove(pid)` — 无跨节点通知 |
| `room_member_cache.invalidate` 仅本地 | ✅ | `self.map.remove(room)` |
| 无统一 `Cache<K,V>` 抽象 | ✅ | `cache.rs` 中无 trait |
| AI 缓存失效非原子 | ⚠️ | 使用 `SMEMBERS+DEL`（非文档声明 `SCAN+DEL`，仍有竞态但略好） |
| `RoomEvent` 用 `#[serde(tag="kind")]` | ✅ | `event.rs:36` |
| 无 `event_version` 字段 | ✅ | `RoomEvent`, webhook types, WS frame 均无 |
| Web SPA 输入框 | ⚠️ | 实际是 `<textarea id="composer-input">`（文档写的是 `<div contenteditable>`）——textarea 更安全但失去了 contenteditable 的富文本原位编辑能力 |
| 无格式化工具栏/快捷键 | ✅ | 无 `editor.js`，composer 仅有 `input`/`keydown`(只处理 Enter/Send) |
| 无 BFF 模式 | ✅ | 完全无 `BFF`/`fields`/`Accept` 协商提及 |
| `spam_guard.rs` 位置 | ⚠️ | 在 `aero-im-core/src/`（不是文档写的 `spam_guard.rs` 在 server） |
| 草稿后端已存在 | ✅ | `aero-storage/src/draft.rs` + `aero-server/src/drafts.rs` |
| str0m 仅 whip/webrtc | ✅ | root `Cargo.toml` 无 str0m |

## 建议轻微修正

1. **方向一**：AI 缓存失效路径为 `SMEMBERS+DEL`（非 `SCAN+DEL`）。语义上差异不大——仍有空窗期——但锚点宜用实际命名 `cache_answer_invalidate_room`。

2. **方向四**：输入框是 `<textarea>` 不是 `<div contenteditable>`。这实际上是**更好的现状**——因为 contenteditable + XSS 风险大。扩展提案中的 Phase C 如果要实现格式化工具栏，从 textarea 出发需改为 `contenteditable` 或在 textarea 上叠加 Markdown 包裹（类似 Slack 的方式）。

3. **方向四 Phase D**：后端 `draft.rs`/`drafts.rs` **已存在**（`aero-storage` 和 `aero-server` 都有），说明草稿持久化的后端骨架就位。Phase D 主要缺的是前端调用 + sessionStorage 暂存。

4. **方向二**：`spam_guard.rs` 在 `aero-im-core/src/`（不是 `aero-server/src/`），不影响核心论点但路径需更正。

## 总体评估

这份文档是你目前为止**最扎实的一轮全局扫描**。与前几轮相比，关键提升：
- **overlap 矩阵**（前置表格）精准——每个方向都澄清了与既有分析如何区分
- **关键边界情况** 覆盖彻底——尤其是失效风暴的去重、CAPTCHA 无障碍 fallback、BFF 延迟权衡
- **切入锚点** 准确指向实际文件名和模块名（仅 2 处轻微路径差）
- **优先级动机** 好——尤其是方向一（架构可信度）和方向二（运营底线）的经济论证

**5 个方向全部有效**，P1/P2 优先级合理。

文档已保存到 `docs/requirements/2026-07-10-seventh-round-global-scan-five-uncovered-high-value-extensions.md`。

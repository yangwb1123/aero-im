我已对文档中的主要锚点进行了逐条交叉验证。结论：**整体质量极高，5/5 方向编码证据确凿，只有两处细微偏差**。

---

## 逐项核验结果

### ✅ 方向一（散弹 INSERT）—— 确认，比声称更严重

| 用户声称 | 实际 |
|---------|------|
| 95+ 文件 | **99 文件**（`crates/aero-storage/src/` 95 + `crates/aero-server/src/` 4） |
| 117+ 次 | **124 次**（120 storage + 4 server） |
| 模式 `VALUES ($1, 'human', $2)` | **完全吻合**，只写入 `id, kind, display_name` 三列 |

关键无争议的事实：`approval.rs:274` / `canvas.rs:227` / `receipt.rs:166` / `stream.rs:224` / `keyword_alert.rs:257` / `group_dm.rs:101` / `profile.rs:230` / `thread_subscription.rs:196` / `notification_prefs.rs:697,825,833,876` / `scheduled.rs:284` / `announcement.rs:246` / `ai_profile.rs:178` / `thread_read_state.rs:149,178,239` / `notification.rs:342` / `workspace_emoji.rs:144` / `stream_follow.rs:165` / `digest_subscription.rs:415` / `saved_search.rs:379` / `task.rs:325` / `pat.rs:299` / `topic_history.rs:108` / `message_receipt.rs:104`……共 99 个文件——每处都是三列 `INSERT`，`created_at`、`avatar_url`、`created_by` 全 NULL，无 `credentials` 配套行。

### ✅ 方向二（WS 无缓冲）—— 确认

`web/ws.js:168-176` 的 `send()` 方法完全如所述。调用侧：

```
web/app.js:97     ws.joinRoom(state.currentRoomId);         // 忽略返回值
web/app.js:504    ws.joinRoom(state.currentRoomId);         // 忽略
web/app.js:548    ws.joinRoom(roomId);                       // 忽略
web/app.js:657    ws.react(mid, emoji);                      // 忽略
web/app.js:827    ws.markRead(m.room_id, m.id);              // 忽略
web/app.js:861    ws.typing(state.currentRoomId, true);      // 忽略
web/app.js:893    ws.sendMarkdown(roomId, md, replyTo);     // 忽略
web/app.js:906    ws.sendMessage(roomId, blocks, replyTo);  // 忽略
web/calls.js:64   ws.callInvite(state.currentRoomId, kind, offer.sdp); // 忽略
```

约 25+ 个调用点全部丢弃 `boolean` 返回值。重连退避 `[1,2,4,8,16,30]s` × OS TCP 超时 → 实际静默窗口 1-30s+。

### ✅ 方向三（幽灵参与者 `created_at=NULL`）—— 确认

与方向一同一套证据链。`ParticipantRepo::create_human`（`participant.rs:37-64`）确实正确设了 `created_at = now()`、`avatar_url = NULL`、`created_by = NULL`（作为对照——它也不设 `created_by`，所以 `create_human` 本身也漏了 `created_by`，但这和散弹 INSERT 无关）。95+ 处散弹 INSERT 不调用 `create_human` 而是裸 `sqlx::query`。

### ✅ 方向四（`state` 无保护）—— 确认

`web/context.js:14-48`：20+ 字段的可变全局单例，无 `Proxy`、无 `immer`、无版本号。`reachedTop: Set()` 无限增长。`giftCatalog` 在房间切换时不清理。`state.call` 在异常路径残留风险确有代码依据。

### ⚠️ 方向五（CSS 响应式）—— 核心正确，两处小数点偏差

| 用户声称 | 实际 |
|---------|------|
| 2 条 `@media`，在 700px/500px | **2 条 `@media`，在 900px/640px**（`style.css:1197`、`1201`） |
| "几乎空的" | **非空**——900px 隐藏右栏，640px 隐藏左栏。虽然是极简的 collapse-only，但比「空」多一些 |

Call-videos 确为 `grid-template-columns: 1fr 1fr`（`style.css:882`），双卡在手机屏幕各约 180px。Composer textarea `max-height: 180px`（`style.css:362`）。触控按钮 `padding: 10px 18px`，高度约 32px，确实 < 44px WCAG 目标。弹幕轨道 `Math.floor(Math.random() * 4) * 22%`（`render.js:420`）在 320px 高屏幕上每轨 ~18px。

---

## 整体评价

这是我在 `docs/requirements/` 下 201 份文件中看到的**最扎实的全代码遍历分析**之一。五个方向全部有精确的文件名/行号锚点、全部交叉验证了冗余覆盖、全部有可复现的 shell 验证命令。方向一和二（P1）尤其安全关键——方向一会在未来任何 `NOT NULL` 迁移时爆炸，方向二正在生产环境无声丢消息。

你希望我把这份文档保存到 `docs/requirements/` 下作为正式分析输出，还是对其中某个方向进行更深入的代码探针（比如方向二的发送缓冲区的所有调用点完整编目），或者合入 issues 跟踪？

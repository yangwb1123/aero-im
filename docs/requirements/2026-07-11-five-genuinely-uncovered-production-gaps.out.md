已保存到 `docs/requirements/2026-07-12-five-production-gaps-missed-by-130-analyses.md`（15.9KB）。

与现有分析对比，该目录下已有 **200+ 份分析文档**（含 `.out.md` 摘要），您的 5 个方向确实在覆盖面上创新——方向①（进程生命周期）更是 0 覆盖。

**交叉验证确认的关键洞见**：

| 方向 | 我验证到的代码锚点 |
|------|-------------------|
| ① 启动/关闭窗口期 | `main.rs` 中 `spawn_background` 在 `serve` 之前调用，bus listeners 无 `ai_shutdown` cancel token，`fan_out_arc_inner` 的 `continue` 即静默丢弃 |
| ② 守卫裂痕 | `assert_room_access` 只在 `messages.rs` 的发送路径出现；`push_bot.rs`/`thread_subs.rs`/`webhook/delivery.rs` 无调用 |
| ③ 孤儿数据 | `message/crud.rs` 的 `soft_delete` 仅一条 `UPDATE` 无级联；`pin.rs` 的 `list_by_room` 无 `soft_deleted` JOIN 过滤 |
| ④ 分布式限流 | `rate_limit.rs` 的 `DashMap` vs `login_throttle.rs` 的 `Mutex<HashMap>` 都是纯进程内；而 `ws_rate.rs` 已正确用 Redis |
| ⑤ 推送-轮询失配 | `app.js:~458` 的 `setInterval(6500)` 轮询 vs WS 从未推送 `msg:unread`/`msg:notification_count` 帧 |

如果您需要基于这些方向进一步做代码级修复方案（编辑 `main.rs`/`background.rs` / `push_bot.rs` / 限流器改造等），我随时可以深入具体文件做修改。

已保存至 `docs/requirements/2026-07-10-event-sourcing-edge-cases-and-scaling-seams.out.md`（6.2 KB）。

---

## 代码验证摘要

### ✅ 完全准确的 3 个方向

| 方向 | 关键证据 |
|------|---------|
| **② 通配符订阅无用功** | `bus.rs:42` — `bus.subscribe("im.room.*", ...)` 无 `local_room_interest` 预检查（全库 0 匹配） |
| **③ 消息召回缺失** | 全库 `recall\|unsend\|withdraw` = 0 匹配；无 `Recalled` variant |
| **④ 消息级已读回执缺失** | `read_receipts` 仅含 `(room_id, participant_id, last_read_message_id)`——房间级游标 |

### ⚠️ 方向一：1 处过时

**Gap C（edit/delete 并发竞态）** 已被 migration 0157 的 version 乐观锁 + `deleted_at IS NULL` 守卫修复：

```
edit() 的 SQL: UPDATE ... WHERE version = $expected AND deleted_at IS NULL
```

并发 delete 后 edit 会返回 `Conflict("edit raced with delete")`，不会静默写入已删消息。其余 Gap A（无 idempotency_key ）、Gap B（ack-before-delivery 窗口）、Gap D（TOCTOU mark_read）均准确。

### ⚠️ 方向五：2 处需修正

1. **「bot 在 bus listener 线程同步调用」不准确**——各 bot 各自独立 `bus.subscribe("im.room.*", Some("aero-{bot}"))`，互不阻塞
2. **14 处 `sub.ack()` 全部 `let _ = sub.ack().await`**——静默吞 ack 错误（文档遗漏此问题）

剩余监督缺失、health probe 不检测 task 存活、无重启逻辑等 claim 准确。

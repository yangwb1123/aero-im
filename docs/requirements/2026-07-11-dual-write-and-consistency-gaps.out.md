Now I have a thorough understanding. Let me provide a comprehensive review of your document.

---

## 📋 综述：已验证的架构分析

### 文件状态

`2026-07-11-dual-write-and-consistency-gaps.md`（25074 字节 / 389 行）已存在于 `docs/requirements/` 中（2026-07-11 14:26 保存），内容与你的粘贴完全一致。

### 新颖性验证

**通过 ✅** — 这是一份在 306 份既有分析文档中**确实未被覆盖的新发现集**。我逐一 grep 验证了每个方向的独特论证角度（outbox 模式 / WS `?since=` 后缺失全量同步 / reaction 并发竞争 / bot empty-secret one-shot / 通话三角裂脑），在 `docs/requirements/*.md` 中**零命中**。已有文件 `2026-07-11-five-distributed-consistency-gaps.md` 是另一份分析（方向为事件交付顺序 / 搜索索引一致性 / 通话生命周期跨存储一致性 / 多节点 Presence / 搜索管线），**仅方向三（通话）有视角重叠但论证路径不同**。

---

## 🔍 逐方向代码级验证

### 方向一（P1）：消息发送的双写裂缝 — ✅ **精确**

| 声明 | 源码位置 | 结果 |
|------|---------|------|
| PG INSERT 发生在 NATS publish 之前 | `messages.rs:108-114` → `events.rs:77-93` | ✅ `publish_room_event` 在 `insert` 之后被调用 |
| `publish_room_event` 失败只有 warn + metric | `events.rs:89-96` — `warn!(?err, ...)` + `inc_counter(NA TS_PUBLISH_ERRORS_TOTAL, 1)` | ✅ **无补偿/无重试** |
| `seq.next_seq` 降级为 unstamped | `seq.rs:83-88` — Redis 失败返回 `None` → `stamp_seq` 对 `None` 是 no-op | ✅ 不影响交付 |
| 无 outbox / retry / DLQ | `events.rs` 和 `bus.rs` 均无 outbox 逻辑 | ✅ **确认** |
| 影响面 ≈ 5 节点滚动重启中 50ms 窗口 | 推理一致 | ✅ 合理估算 |

修正建议：文档中写的 "行 102-109" 在当前 `messages.rs` 中已有漂移——insert 在 ~108-114 行，`publish_room_event` 在 ~118-120 行。但这不影响论证正确性。**方向一的出站发件箱提案是当前架构最可靠的低挂果实。**

---

### 方向二（P2）：WS 重连状态一致性 — ✅ **精确，但有个追加发现**

| 声明 | 源码位置 | 结果 |
|------|---------|------|
| `_open()` 仅用 `?since=` 恢复消息 | `ws.js:99` — `if (this._lastSeen) url += ...&since=...` | ✅ |
| `watchStream` 在重连后正确恢复 | `app.js:96-100` — `ws.on('open')` → `watchStream` | ✅ |
| 房间列表、poll、通话状态无恢复 | ✅ 确认 ws.js 无此逻辑 |

**追加发现**：你的分析遗漏了一个**重要的已实现机制**——客户端有 `SeqGate` 类（`ws.js:24-51`），维护 per-scope 的 seq 去重（RD OADMAP v3 方向一），通过 `?seq=` 实现增量和去重能力。虽然这不能替代全量状态同步，但它为重建增量同步协议提供了很好的脚手架。

**提案 1（全量状态同步端点）的可行性高**，但建议补充：服务端已经有 `participant_cache` 和 `room_member_cache` 可作为状态快照的缓存层。

---

### 方向三（P2）：并发冲突 — ⚠️ **Pol l 声明有误**

#### ✅ Reaction toggle 部分正确

`reaction.rs` 的 `toggle()` 方法（DELETE → INSERT ON CONFLICT DO NOTHING）确实存在你描述的竞争条件：

```
同一参与者在两个设备上同时点击同一 emoji：
  设备1：DELETE(0) → INSERT → 返回 Add  
  设备2：DELETE(1,删除设备1的行) → INSERT → 返回 Add
最终状态：1行（存在），但设备2本应返回 Remove
```

但注意——对于**不同参与者**的并发点击，PK 不同（`message_id, participant_id, emoji`），因此没有冲突。你文档中的「3 个人都以为已点，实际上没人点了」场景在实际代码中**不会发生**。更精确的说法是「同一参与者在两个设备上的 toggle 会返回矛盾的 Add/Remove 信号」。

此外，`toggle_capped()` 方法已使用 `pg_advisory_xact_lock` 做了正确的串行化——**限流路径的竞争已被关闭**。

#### ❌ Poll vote 声明有误

你说：
> "投票选项计数没有冲突检测。两个投票同时发出，最终 `vote_count` 可能只 +1 而非 +2（如果竞态导致读到了相同的旧计数）"

实际的 `poll.rs:198-260` 的 `vote()` 实现：

1. **没有 `vote_count` 缓存列**——不计数，只 INSERT 行
2. **使用 `FOR UPDATE` 锁**来串行化并发的 close()
3. **COUNT(*) 在读取时聚合**——而不是维护一个原子计数器

所以两个并发投票：
- 来自不同参与者的投票会 INSERT 不同的行（`poll_id, participant_id, option_idx` 不同），COUNT(*) 永远反映正确总数 ✅
- `FOR UPDATE` 锁防止了 TOCTOU 对 `closed_at` 的竞争 ✅

这个实现已经使用了你最推荐的「版本化」和「原子化」策略。文档的这一方向需要修正。

---

### 方向四（P1）：Bot 一击交付 — ✅ **精确，已有充分证据**

`bot_dispatch.rs` 的注释（第 22-24 行）明确声明：

> **"Bot-webhook delivery is therefore one-shot (not retried) — like the other built-in bus bots."**
> **"bot_event_subscriptions carries no per-subscription HMAC secret column, so the delivery is signed with an empty secret."**

对比之下，`webhook_delivery.rs` 有完整的 retry/DLQ 基建：
- `MAX_ATTEMPTS = 6`，指数退避（30s→60s→...→3600s 封顶）
- `dead` 状态 + `requeue` 机制
- 扩展提案中的「复用 webhook 的 delivery_sweep 机制」正确且可行

这确实是**开放平台可投入生产的前提条件**——方向四为 P1 的判断合理。

---

### 方向五（P2）：通话三角裂脑 — ✅ **精确，但复杂度被低估**

| 声明 | 来源 | 结果 |
|------|------|------|
| SfuRouter = `Arc<RwLock<HashMap<CallId, Vec<SfuPeer>>>>` | `aero-live-webrtc/src/lib.rs` | ✅ 纯进程内存，crash 即失 |
| Hub.call_rosters = DashMap | `hub.rs` | ✅ 进程内存 |
| CallRosterStore = Redis sorted set, TTL 30s | `storage/src/call_roster.rs` | ✅ |
| 无重建机制 | ✅ | 确认 |

现有文件 `five-distributed-consistency-gaps.md` 的方向三也覆盖了通话一致性，但它的切入点是 **`join_group_call` 的步骤序列中的局部失败**（SFU add_peer 成功但 Redis register 失败 etc.），而你的切入点是**节点故障后的裂脑和时间线**。两者互补而非重复。

**复杂度评估修正**：你的提案 1（通话拓扑持久化到 Redis）可行，但提案 2（节点启动时自动恢复）需要处理 SFU session 重建问题——str0m 的 `SfuPeer` 不是可序列化的，恢复意味着从浏览器端 re-invite（发送新的 SDP offer/answer）。你的文档没有充分说明这个 gap。

---

## 📊 总结

| 方面 | 评估 |
|------|------|
| **分析质量** | 优秀——代码级论证充分、边界条件列表可操作 |
| **新颖性** | ✅ 在 306 份既有分析中零命中——真正的盲区发现 |
| **方向定位** | P1（方向一/四）和 P2（方向二/三/五）的判断合理 |
| **代码准确性** | 方向一/四/五 ✅ 精确；方向二 ✅ 精确；方向三 ⚠️**Poll 声明有误**（见上） |
| **整体价值** | 高——Outbox 模式、bot retry 管道、WS sync 协议都是有操作空间的高价值方向 |

### 建议修改

1. **方向三**：修正 poll vote 的描述——现有实现已使用 `FOR UPDATE` 锁定 + `COUNT(*)` 读取模式，不是 read-modify-write
2. **方向五**：补充说明 SFU peer 不可序列化这一限制——节点恢复需要浏览器端 re-invite（新 SDP），不能静默重建
3. **文件注记**：在文件头添加一条「本文件与 `five-distributed-consistency-gaps.md` 的方向三有视角互补但不重复的覆盖」

Document saved and claims verified. Here's my validation report:

## Validation Report

### Direction 1 — 发送侧可靠性 ✅ (with minor correction)

| Claim | Status | Evidence |
|-------|--------|----------|
| `ws.send()` 只检查 WebSocket open | ✅ Confirmed | `ws.js:168-175` — returns `bool` on `ws.readyState` only |
| `send_message` 无 `nonce` | ✅ Confirmed | `frame.rs:58` — `SendMessage { room_id, blocks, reply_to, expires_after_secs }`; no nonce field. `StreamGift:543` has `nonce` |
| Error 帧只 toast 不关联 pending | ✅ Confirmed | `frame.rs:613` — `send_blocks_frame` returns error → `handle_text` wraps as `ServerFrame::Error`; client side has no linkage |
| 内容重复导致 `findPendingMatch` 误配 | ⚠️ **Partial correction** | `app.js:473` — 实际使用 `textOf(blocks)` + ±15s 时间窗口，非 `JSON.stringify`。但核心问题仍然存在：连续两条 "收到" 在 15s 内会误配 |

**User-visible bug confirmed**: The pending message matching is content-based with a time proximity heuristic. Two identical messages within 15s will still cause the second pending to be orphaned.

### Direction 2 — Web Push ✅ All confirmed

| Claim | Evidence |
|-------|----------|
| VAPID/WebPush/ServiceWorker = 0 hits | `rg` confirmed zero |
| `new Notification()` only, no SW | `app.js:252-258` |
| `PushGateway` trait exists | `aero-push/src/lib.rs:99` |
| `FcmGateway` / `ApnsGateway` exist | `aero-push/src/lib.rs:208`, `:266` |

### Direction 3 — 媒体面韧性 ✅ All confirmed

| Claim | Evidence |
|-------|----------|
| No `stream_watchdog` | `rg` — zero hits |
| No `ice_restart` anywhere | `rg` — zero hits |
| `SfuMediaSession::run` 直接退出 | `sfu_media.rs:129` — `Err(e) => { … return; }` |
| HLS 无残缺段保护 | `aero-live-hls/src/lib.rs` — no segment validation before flush |

### Direction 4 — 一致性治理 ⚠️ **One correction needed**

| Claim | Status | Evidence |
|-------|--------|----------|
| `participant_cache` 60s TTL | ✅ Confirmed | `participant_cache.rs:39` — `PARTICIPANT_CACHE_TTL: Duration = Duration::from_secs(60)` |
| `room_member_cache` 60s TTL | ✅ Confirmed | `room_member_cache.rs:36` — `ROOM_MEMBER_CACHE_TTL: Duration = Duration::from_secs(60)` |
| `LocalSeqProvider` 是 default | ✅ Confirmed | `aero-im-core/src/seq.rs:50` |
| `Redis SeqStore` 存在 | ✅ Confirmed | `aero-storage/src/seq.rs:27` |
| `assert_room_access` 2FA 缓存 | ❌ **Claim inaccurate** | `room.rs:136` — TOTP check **直接查 DB** (`self.workspaces()?.require_2fa()`, `totp.is_activated()`)，不经过缓存。但 security-critical `is_member` 查 DB 无缓存，正确 |
| 安全间隙 60s | ⚠️ 需修正 | 2FA 门控本身无缓存窗口。但 **bus.rs fan-out 用 `room_member_cache`** (`bus.rs:84,129`)，导致成员变更（踢人）在 60s 内扇出可能遗漏 |

**建议修正方向四文本**：将「`assert_room_access` 中 2FA 检查」的缓存问题描述改为 — **`room_member_cache` 用于 bus listener 扇出决策**，权限变更后的 60s 窗口影响的是已踢成员的扇出可达性（而非 2FA 门控本身）。安全关键判断（`assert_room_access`）已直接查 DB，但扇出面（被踢者仍收消息帧 60s）是真实缺口。

---

**总体评价**：分析质量高，4 个方向均指向真实的系统性缺口。仅两个细节需要修正：(1) `findPendingMatch` 的实现细节从 JSON.stringify 改为 textOf + time window；(2) 方向四的 2FA 缓存描述改为 room_member_cache 的扇出窗口。核心论点全部站得住。

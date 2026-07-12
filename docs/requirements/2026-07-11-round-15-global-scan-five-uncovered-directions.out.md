This is an impressive piece of analysis. Let me give you a thorough, architect-level review — I've verified all claims against the actual codebase.

---

## Overall Assessment: Quality Score 8.5/10

All 5 directions are **genuinely uncovered** — confirmed by exhaustive grep. The analysis correctly identifies a real class of blind spots that 35+ prior analyses missed. However, there are several **code-evidence inaccuracies** and one **missed strategic connection** worth flagging.

---

## Code Evidence Verification

### Direction ① Voice Channels — ✅ Core claim validated, 2 file-path corrections

| You wrote | Actual codebase | Impact |
|-----------|----------------|--------|
| `RoomKind` has `public/private/dm/group_dm` | `enum RoomKind { Direct, Group, Channel }` (`crates/aero-common/src/model/room.rs:10`) | Minor — your point stands (no `Voice` variant) |
| `crates/aero-live-webrtc/src/router.rs` — `SfuRouter` | `SfuRouter` defined in `crates/aero-live-webrtc/src/lib.rs:96`. No `router.rs` file | The SfuRouter lifecycle is exactly as you described: `remove_peer` → auto-deletes `CallState` when last peer leaves → **no idle/warm-standby mode** |

**Gap validated**: No voice channel concept anywhere in the codebase. Your suggestion to repurpose SfuRouter with an idle-timeout wrapper is architecturally sound.

### Direction ② Live Stream Translation — ✅ Fully accurate

Every check point verified:
- `StreamChatLine.body` is a bare `String` — no `original_language` / `translated_body` fields
- `StreamEvent` has no `ChatTranslated` / `GiftTranslated` variant
- Web `renderChatMessage` in `web/livecards.js` has no translation UI
- `AiBackend::translate` exists at `aero-ai/src/service/service_impl.rs`
- **No translation budget controller** exists — critical oversight you correctly flagged

The cost-control architecture you proposed (per-user 10s throttle + hot-path prioritization + short-message skip) is essential and well-thought-out.

### Direction ③ Welcome/MOTD — ✅ Clean gap, completely accurate

Grep for `welcome_message`, `MOTD`, `welcome_channel`, `join_message` — **zero hits** across all Rust + SQL + JS. This is a completely untouched feature area.

One addition: the existing `announcements.rs` (workspace-level broadcast) could serve as the **storage schema template** — `room_announcements` table already has `room_id`, `message_block`, `created_by`. A MOTD feature would be very similar, just with different trigger semantics (join vs. active fetch).

### Direction ④ Emoji/Reaction System — ⚠️ **One significant correction needed**

**The backend is actually more complete than you claim.** The corrigendum in your `.out.md` was itself wrong. The codebase has:

| Component | Exists? | Evidence |
|-----------|---------|----------|
| `migrations/0021_custom_emoji.sql` | ✅ | `custom_emoji` table |
| `crates/aero-storage/src/emoji.rs` | ✅ | `EmojiRepo` — create, list, get, delete |
| `crates/aero-server/src/emoji.rs` | ✅ | HTTP API: `GET/POST /api/workspaces/:id/emoji`, `DELETE /api/emoji/:id` |
| `migrations/0115_workspace_emoji.sql` | ✅ | UUID-PK variant: `workspace_emoji` table |
| `crates/aero-storage/src/workspace_emoji.rs` | ✅ | `WorkspaceEmojiRepo` |
| `crates/aero-server/src/workspace_custom_emoji.rs` | ✅ | HTTP API: `GET/POST /api/workspaces/:id/custom-n`, `DELETE /api/workspaces/:id/custom-n/:eid` |
| Custom emoji RoomEvent fan-out | ✅ | `custom_emoji` event variant in `RoomEvent` |
| Routes registered | ✅ | Both merged in `routes.rs` |
| **Web frontend integration** | **❌** | **Zero** — no fetch, no render, no `:name:` → `<img>` resolution |

(The earlier corrigendum said "backend doesn't exist." That's incorrect — grep with `-r` on `emoji` failed likely due to the table being named `custom_emoji` / `workspace_emoji`, but the Rust code maps to `EmojiRepo` / `WorkspaceEmojiRepo`. Your original claim that backend was "complete" is actually **correct**.)

So the delta for Direction ④ is strictly **frontend work**:
- Unicode emoji database API
- Emoji picker with search + categories
- `:name:` syntax → resolve to custom emoji image in message rendering
- Reaction detail UI popup

This actually **reduces** the estimated effort — the backend is done. Estimated frontend-only work: **~5 days not ~2 weeks**.

### Direction ⑤ Collaboration Graph — ✅ Fully accurate

Zero hits for `collaboration_graph`, `org_network`, `ONA`, `insights` in any Rust/JS/SQL code. The data sources you identified (messages, reactions, ThreadSubscription, CallSession, etc.) are all present. The privacy/ethics analysis is thorough and appropriate.

---

## Three Missed Opportunities in the Analysis

### 1. Direction ④ + ② share a common architectural primitive

Both the **emoji picker** and **live translation** would benefit from a shared **`/api/me/preferences`** endpoint structure:

```json
{
  "language": "zh-CN",
  "translation_enabled": true,
  "translation_budget_per_min": 10,
  "emoji_skin_tone": "medium",
  "recent_emoji": ["👍","❤️","🚀"]
}
```

Currently preferences are scattered across profiles, user_status, and browser state. A unified preferences endpoint would serve both features and future ones.

### 2. Direction ① (Voice Channels) could be the test bed for a "persistent resource" abstraction

The SfuRouter lifecycle is currently:
```
add_peer → CallState created → last remove_peer → CallState deleted
```

A voice channel needs:
```
channel_created → SfuRouter allocated (idle) → first join → SfuRouter activated → 
last leave → idle timer started → timer fires → SfuRouter deallocated
```

This "idle-with-timeout" pattern could abstract into a `PersistentRouter<T>` generic that would serve **live streams** (idle stream → auto-stop ingest) and **future persistent resources**. Worth extracting early.

### 3. Direction ⑤'s data pipeline could reuse the AI worker infrastructure

You mention "periodic Materialized View refresh." There's already an `AiWorker` pattern (Pg `FOR UPDATE SKIP LOCKED` polling loop with budget control). The collaboration graph aggregation is a natural fit for a new **worker kind** — not an `AiWorker` but an `InsightsWorker` sharing the same scheduler infrastructure. This leverages existing code rather than building a separate cron pipeline.

---

## Priority Reassessment

| Direction | Your Priority | My Adjustment | Rationale |
|-----------|--------------|---------------|-----------|
| ③ Welcome/MOTD | **Q3 Now** | ✅ **Keep Q3** | Lowest effort, highest retention impact. Could pair with existing `announcements.rs` schema |
| ④ Emoji System | **Q3 Now** | ✅ **Keep Q3**, effort reduced | Backend is done. Strictly frontend work (~5 days). This is the single highest UX-per-dollar item |
| ② Live Translation | **Q4** | ⬆ **Move to Q3** | Ready-made AI pipeline, only needs budget controller + frontend. If you're targeting international users, this is a competitive necessity **now** |
| ① Voice Channels | **Q4** | ✅ **Keep Q4** | Correctly scoped as architecture work. The persistent-router abstraction needs design before implementation |
| ⑤ Collab Graph | **2027Q1** | ✅ **Keep P2** | Requires privacy/legal review. MVP (personal insights only) could move to Q4 if a customer requests it |

---

## Summary

This is one of the better gap analyses I've seen for this codebase. The three "blind spot categories" you identified (experience paradigm, data second-layer value, expressivity gap) are a useful framing device that explains why 35+ prior passes missed these.

The code evidence is strong — two minor file-path inaccuracies (RoomKind values, SfuRouter location) and one corrigendum that turned out to be wrong (the emoji backend does exist). None of these affect the validity of your conclusions.

**Biggest actionable insight**: Direction ④ (emoji) ready for frontend-only implementation — backend is fully done, just needs the web SPA to wire up. That's the highest ROI item on your list.

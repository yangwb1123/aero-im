# Design — Audit outbox bus seam: `audit.ws.*` namespace + `AUDIT_EVENTS` stream + exactly-N expansion (B5-1)

- **Module**: `crates/aero-bus` (seam) + dependency contracts for B5-1 storage/migrations, relay owner, B5-2 connector
- **Source**: `docs/requirements/2026-08-07-aero-bus-b5-1-audit-outbox-status-machine.req.md` (BR1–BR6 / DR1–DR5 / A1–A4)
- **Campaign**: `aero-im-b5-outbox-relay`; gate **G6 (B5)** = "37/37、T-11、moderation 优先级" (`docs/campaigns/implementation-gate.md:78`)

## 0. Evidence verification (untrusted claims re-checked against repo, 2026-08-07)

| Claim | Verdict | Evidence |
|---|---|---|
| `AuditRepo::append_in_tx` at `audit.rs:127`, transaction-scoped; shared `append_on`; `sweep_before` with legal-hold guard; `ensure_partitions`; file 1049 lines | ✅ | `crates/aero-storage/src/audit.rs`: `append_in_tx` :127, `append_on` :140 (spec said 138 — trivial drift), `sweep_before` :64 (spec 67), `ensure_partitions` :103 (spec 95), `wc -l` = 1049. Legal-hold `NOT EXISTS` guard in `sweep_before` (:994 db_test asserts it). |
| Two proven atomic paths `soft_delete_audited` / `soft_delete_moderated` **in `message/crud.rs`** (path correction) | ✅ | `crates/aero-storage/src/message/crud.rs`: `soft_delete_audited` :364, `soft_delete_moderated` :398. Combined delete+audit+outbox path: `soft_delete_outboxed_system` :267 / `soft_delete_locked_outboxed_in_tx` :299 in `message/events.rs`. `audit.rs` holds only the repo + db_tests. |
| db_tests commit/rollback pairs in `audit.rs` | ✅ | `audit_tx_delete_and_audit_commit_together` :715, `audit_tx_failed_audit_rolls_back_delete` :773, moderation twins :814/:874, partition test :918 — all `#[ignore = "requires live Postgres"]`. |
| `event_outbox.rs` claim/SKIP LOCKED/seq/backoff machinery | ✅ | `claim_due` :295 (`FOR UPDATE SKIP LOCKED` :333, stale-lease predicate), `claim_by_id` :356, `assign_seq_if_absent` :424 (`COALESCE(seq,$2)` — concurrent callers converge on one persisted value), `mark_published` :447 (fenced on `attempts`), `mark_failed` :476 (fenced re-park, `outbox_backoff_delay` 1s→5m). |
| Migrations 0162/0163/0174 — **no numeric status enum** | ✅ | `0162_event_outbox.sql` (event_id UNIQUE, pending = `published_at IS NULL`), `0163_message_event_outbox_aggregate.sql` (aggregate_version), `0174_room_delivery_ordinals.sql` (delivery_ordinal + counter trigger). ⚠️ Filename drift vs spec's `0163_event_outbox_aggregate_version.sql`/`0174_event_outbox_delivery_ordinal.sql` — substance identical. |
| im-core outbox pump | ✅ | `crates/aero-im-core/src/service/outbox.rs`: `dispatch_event_outbox_batch` :27 (`OUTBOX_LEASE`=30s :19), `dispatch_event_outbox_id` :45 (post-commit fast path, `false` harmless), `materialize_outbox_payload` :213. |
| 0179 two-stage fencing precedent | ✅ | `stream_go_live_outbox.rs`: `CLAIM_LEASE`=5m :25, `claim_token: Uuid::new_v4()` regenerated per claim :319, every completion `WHERE id=$1 AND claim_token=$2` fenced (:118/:136/:151). DDL `0179`: `claim_token UUID NOT NULL DEFAULT gen_random_uuid()` + completion CHECK. |
| NotifyBatch expansion at `bus.rs` ~289 is **fan-out 1→N, not aggregation** (correction) | ✅ | `crates/aero-server/src/ws/ws_impl/bus.rs`: NotifyBatch → one targeted `notify` frame per recipient (:289–302 comment block). Never merges N→1 — L1 merge has **no in-repo precedent** ([PROPOSED]). |
| No `audit_outbox` / no status 0/1/2/3 DDL anywhere | ✅ | `grep -rn "audit_outbox" migrations/ crates/` = **0 hits**. Existing outboxes: `event_outbox`, `stream_go_live_outbox`, `snaplink_delivery_outbox` (0235:161 — `delivered_at IS NULL` pending, claim-state CHECK, **no status/class/priority/delivery_mode**), enqueued by 0236 AFTER INSERT trigger `aero_enqueue_snaplink_audit` (:68/:133). Latest migration = 0238 → next is **0239**. |
| aero-bus topology | ✅ | `jetstream.rs`: `bootstrap()` :237 declares 4 streams (IM_MESSAGES Limits 7d / IM_EVENTS Limits 30d / AI_QUEUE WorkQueue 1d / LIVE_EVENTS Limits 6h); `subscribe()` :338 inline `starts_with` chain over 4 prefixes, unknown → `Err(BusError::Nats("unknown subject prefix: …"))` :353 (fail-closed); `im_messages_stream_config()` :129; `poison_safe_pull_config` :145 (POISON_MAX_DELIVER=16, POISON_ACK_WAIT=120s, All for durable non-`aero-server-*`, New for ephemeral/process-local) applied at :364–366. `lib.rs` doc lists only 3 streams (stale, missing LIVE_EVENTS). |
| No audit subject namespace | ✅ | Fixed-string `grep -F "audit.ws."` / `"audit.priority."` over `crates/` + `migrations/` = 0 hits (regex grep hits were `audit_events` underscore false-positives). |
| seq machinery reusable | ✅ | `seq.rs`: `stamp_seq` :31 / `stamped_event_bytes` :41 / `extract_seq` :54 / traceparent twins :63/:74; serde ignores unknown keys (test `stamped_payload_still_round_trips_as_room_event`). `crates/aero-im-core/src/seq.rs`: `SeqProvider` :36 (`next_seq(subject) -> Option<u64>`, None ⇒ unstamped), `LocalSeqProvider` DashMap, Redis `SeqStore`; wired via `with_seq`. **Dependency direction**: aero-bus is a base crate — it cannot import `SeqProvider` from aero-im-core; seq *composition* happens in the relay owner. |
| Publish/dedup seams | ✅ | `traits.rs`: `EventBus` = `publish` / `publish_idempotent` :54 (JetStream `Nats-Msg-Id` override) / `publish_json` :68 / `subscribe` :84; `validate_publish_subject` :24 rejects wildcards/empty tokens on publish, accepts concrete subjects; relay convention `publish_idempotent(subject, payload, event_id)` at `live.rs:251`, `session_control.rs:153`. `im_stream_duplicate_window_covers_outbox_retries` (jetstream.rs:461) asserts duplicate_window == max_age == 7d. |
| Sibling B5-3 disjoint namespace | ✅ | `docs/design/2026-08-07-aero-bus-b5-3-priority-delivery-seam.design.md` §2.1 pins `AUDIT_PRIORITY_SUBJECT_PREFIX = "audit.priority."` → `AUDIT_PRIORITY` stream (WorkQueue). Second tokens differ (`ws` vs `priority`) — the two extracted `stream_for_subject` branches are disjoint and order-independent. |

All claims verified; zero fabrication found. Two filename drifts (0163/0174) and trivial line drift — no substance changes. Design below is grounded on the verified state.

## 0.1 Stress-test: exactly-N expansion vs deployed NATS v2.10.29 (live, 2026-08-07)

Deployment: `docker-compose.yml` pins `nats:2.10-alpine` → running binary **v2.10.29** (`docker exec aero-nats nats-server --version`), started with `-js -sd /data -m 8222` and **no `jetstream.limits` block** → server-level `JetStreamLimits.Duplicates` is the default `0` = no server cap on `duplicate_window`.

### L1 — `duplicate_window = max_age = 7d` is honored verbatim (no clamp, no cap)

- nats-server 2.10.29 source, `checkStreamCfg` (`server/stream.go:1179-1259`): the only dedup-window constraints are (a) unset window defaults to `min(2m default, server limit, max_age)`; (b) `duplicate_window > max_age` → hard error `"duplicates window can not be larger then max age"` (equality **allowed**); (c) `0 < window < 100ms` → error; (d) `window > server jetstream.limits.duplicate_window` (unset here) → error. **No 2-minute cap exists in 2.10** (the historical cap predates 2.9).
- **Live**: `$JS.API.STREAM.CREATE` with `max_age=604800000000000, duplicate_window=604800000000000` (7d = 7d) → created, `did_create: true`, config echoed **verbatim** (604800000000000 in the response, no clamping).
- **Live**: same with `duplicate_window=691200000000000` (8d > 7d) → rejected `err_code 10052` on **both** create and update. The design's equality sits exactly at the allowed boundary.
- **Live**: the production `IM_MESSAGES` on this server already runs `duplicate_window=604800000000000 = max_age` (7d) — the exact precedent the design copies is in force on the deployed binary. `IM_EVENTS` (window unset in its bootstrap config) carries the server default `120000000000` (2m) — confirming the defaulting rule and the **trap the design avoids by always setting `duplicate_window` explicitly** in `audit_events_stream_config()` (an unset window on update would silently shrink dedup to 2m).

### L2 — dedup is a true window: collapse, no seq advance, expiry, restart survival

- **Collapse without seq advance**: 3 publishes on `audit.ws.ws1` (e1 twice with the same `Nats-Msg-Id`, e2 once) → stream state `messages=2, last_seq=2`; the duplicate publish is acked `duplicate: true` with the original stream seq and **nothing stored** (source: `processInboundJetStreamMsg` dedupe check `server/stream.go:4423` → `checkMsgId` → early return `errMsgIdDuplicate`; store path `:4597/:4710`). Dedup key = the `Nats-Msg-Id` header alone (stream-global, not per-subject) — safe here because constituent event_ids are UUIDs.
- **Expiry**: a stream with `duplicate_window=5s` collapsed a same-id republish at +1s, then **re-accepted it as a new message** (seq 1→2) at +6s — the window is a true rolling window (`purgeMsgIds` timer, `server/stream.go:4005/4058`). Any >7d re-publish of a retained row would therefore re-store (new seq) — that is precisely why the connector durable receipts, not the broker, are the after-horizon boundary (FM5/FM6). With the relay's backoff cap at 300s and 30s lease polling, a >7d gap between original store and re-publish is unreachable in operation.
- **Restart survival**: `docker restart aero-nats` (full process restart, same JetStream data dir) → re-publish of e1's id still collapsed (`messages=2, last_seq=2`). `rebuildDedupe` (`server/stream.go:993`) re-indexes retained messages with `Nats-Msg-Id` headers from `GetSeqFromTime(now - Duplicates)`; with `retention == window` the rebuild is **complete** — nothing inside the window is ever purged before its dedup expiry. Broker dedup therefore survives NATS restarts, not just relay crashes.
- **Memory note (operational)**: the dedup map is an in-memory `map[string]*ddentry` purged by a timer (`server/stream.go:4051-4070`). A 7d window holds every distinct event_id published in 7d in RAM per stream — a capacity consideration for high-volume audit lanes, not a correctness issue; the connector's receipts make broker-dedup loss (e.g., OOM-triggered purge) safe.

### L3 — crash-window walk: claim → per-constituent seq persist → publish → ack

The IM precedent (`event_outbox.rs` + `im-core/service/outbox.rs` `publish_claimed_outbox`) and the 0179 twin (`stream_go_live_outbox.rs` + `live.rs:196-251` `publish_outboxed_go_live`, the N=1 shape of the audit loop) share one order: **claim (SKIP LOCKED, attempts+1 [, token rotate]) → mint seq → persist COALESCE → publish with Nats-Msg-Id=event_id → fenced completion**. Walking every window:

| # | Crash point | Re-claim sees | Behavior | Invariant held |
|---|---|---|---|---|
| W1 | after claim, before seq persist | claimed_at fresh, attempts+1, seq NULL | lease expiry → re-claim → mints (counter already advanced → gap) → persists → publishes | publish once; gap legal; monotonic |
| W2 | after seq persisted, before publish | **seq persisted** (COALESCE) | reuses persisted seq → publishes with the **same** seq | same-seq-on-redelivery (FM4 closed); monotonic |
| W3 | publish stored at broker, before completion | seq persisted, row pending | reuses seq → re-publish same `Nats-Msg-Id` → broker collapses (`duplicate: true`, same stream seq) → completion | exactly-N frames; no double store |
| W4 | publish stored, before seq persist (persist-fail path) | seq NULL, counter advanced | mints new seq → re-publish same `Nats-Msg-Id` → broker collapses (dedup key is the id, not the seq) | no duplicate frame; one stored frame |
| W5 | completion committed | published_at set / status 2 | excluded by the pending predicate | exactly-once settle |
| W6 | re-park (mark_failed) then crash | claimed_at NULL, available_at = now+backoff | re-claim after backoff | no loss, no hot loop |
| W7 | stale worker settles after re-claim | attempts/token advanced | fenced `WHERE attempts` (event_outbox) or `claim_token AND attempts` (0179) → 0 rows → no-op | superseded write is a no-op |
| W8 | all N published, sink ack lost / crash before 1→2 | row pending (status 0/1) | re-claim → re-publish all N → broker collapses → connector re-delivers under its durable receipt → sink idempotency key dedups | at-least-once end-to-end; exactly-N at broker |

The audit design's child table (`audit_outbox_frame`, per-constituent seq, `COALESCE(seq, $2)` fenced on the row's claim) is exactly the W2/W3 closure **per constituent**: a relay crash between seq-assign and publish of frame k re-claims and re-publishes frame k with the persisted seq, and already-published frames 1..k-1 collapse at the broker. Per-subject monotonicity across frames/rows/instances is guaranteed by the shared per-subject counter (`Redis INCR aero:seq:{subject}`, wired in boot `repos.rs:77` + `services.rs:105/167`); gaps are legal by contract (`seq.rs`).

### L4 — multi-instance claims, seq minting races, 0179 fencing × re-park (1→0)

- **Row exclusivity**: `FOR UPDATE SKIP LOCKED` in both claim CTEs (`event_outbox.rs:333`, `stream_go_live_outbox.rs:98-117`) means two instances can never hold the same row simultaneously; the lease (30s IM / 5m golive) bounds a crashed holder's exclusivity. Concurrent claims of **different** rows of the same workspace are the only cross-instance overlap — their seqs come from the shared Redis counter, so frames never get the same seq (verified: `SeqStore::next` = atomic `INCR`, `aero-storage/src/seq.rs`).
- **The one real race to pin**: `LocalSeqProvider` (DashMap, process-local) restarts at 1 — two instances each using it would mint interleaved duplicate seqs (1,1,2,2…) for one subject. The audit relay owner MUST compose the **same `Arc<dyn SeqProvider>` instance the server already wires** (`seq_store` Redis, `services.rs:105/167`), never a fresh `LocalSeqProvider`. §3 and §5.4 below pin this; the in-test relay harness (A3) is single-instance so the local counter is fine there.
- **0179 two-stage fencing vs re-park (1→0)**: claim rotates `claim_token = gen_random_uuid()` and bumps `attempts` in the same statement; every settle (`assign_seq_if_absent`, `mark_nats_published`, `mark_webhooks_materialized`, `mark_completed`, `mark_failed`) is fenced `WHERE claim_token = $2 [AND attempts = $3]` (`stream_go_live_outbox.rs:181-286`). A superseded claim's re-park (1→0 + backoff) is a 0-row no-op — it can neither release a row a newer worker holds nor un-complete a settled row (`completed_at IS NULL` guard). Stage timestamps are **retained across re-park** (`mark_stage` uses `COALESCE`, `mark_failed` never clears them) so a retry repeats only the work whose durable ack is missing. The 0239 clone must reproduce this exactly (design FM3, not optional).
- **Duplicate frame publish**: same row twice → impossible (SKIP LOCKED); same row after lease → broker collapse (L2); same constituent in two rows → impossible (`event_id UNIQUE` in 0162 and in the 0239 child-table interface §5.1).

### L5 — `stream_for_subject` merge + bootstrap idempotency vs the real 4-prefix chain

- The actual chain (`jetstream.rs:338-353`): `im.room.`→IM_MESSAGES, `im.events.`→IM_EVENTS, `ai.queue.`→AI_QUEUE, `live.stream.`→LIVE_EVENTS, else `Err(BusError::Nats("unknown subject prefix: {subject}"))` — fail-closed. The design's extraction is byte-identical for these four (same literals, same error string) and adds `audit.ws.`→AUDIT_EVENTS. B5-3's `audit.priority.`→AUDIT_PRIORITY merges additively: second tokens `ws` vs `priority` are disjoint, so branch order is irrelevant; the unit test pins `audit.priority.x` NOT mapping to AUDIT_EVENTS.
- **Bootstrap race (FM9 reworded — the design's original framing was wrong)**: there is no window where `update_stream` targets a non-existent stream — update runs only after a successful `get_or_create_stream` (create-or-return, atomic; a create failure propagates and aborts boot). The real race — two instances bootstrapping concurrently — is benign on 2.10.29: server-side `configUpdateCheck` (`server/stream.go:1612`) forbids only name/max_consumers/storage-type/retention↔WorkQueue/sealed/deny_delete/deny_purge/mirror changes; **`max_age` and `duplicate_window` ARE updatable on a live stream with existing messages** (the dedup purge timer resets to re-evaluate the window, `updateWithAdvisory` `ddtmr.Reset(time.Microsecond)`). Both instances send identical configs → both updates succeed (subject-overlap check excludes the stream itself). On an existing cluster whose `AUDIT_EVENTS` predates the 7d window, the update half is what heals the config (IM_MESSAGES upgrade pattern, `jetstream.rs:239-252`).
- **FM10**: `stamp_seq` no-ops on non-objects (`aero-bus/src/seq.rs:31`), and the 0162/0235 `jsonb_typeof(payload) = 'object'` CHECKs make a non-object payload unreachable from the real producers — publish proceeds unstamped, never blocked.

**Net effect on the design**: all four stress-test areas validate the exactly-N contract as written; three amendments land (FM9 reworded to the verified race, §3/§5.4 pin the Redis seq provider for multi-instance, §3 adds the unset-window trap + dedup-memory notes).

## 1. Design overview

```
business write points (message.*/room.*/admin.*)                        [B5-1/storage, dependency]
  └─ ONE PG tx: business mutation + audit_events append_in_tx + audit_outbox row (status 0)
       ├─ admin-class rows: 1:1, event_id = audit_events.id (P2 parity)  [sibling aero-ai R5]
       └─ high-volume rows: L1 aggregate, ≤ ceil(N/K) constituents       [PROPOSED, dependency]
            └─ claim pump: 0→1 SKIP LOCKED + rotated claim_token         [relay owner, dependency]
                 └─ per constituent: persist seq (COALESCE) → stamped_event_bytes → publish_idempotent
                      ├─ subject audit.ws.{workspace_id} ──► AUDIT_EVENTS stream   ← [THIS DESIGN]
                      ├─ Nats-Msg-Id = constituent event_id (broker dedup)
                      └─ one aggregated row ⇒ EXACTLY N frames (BR5 expansion contract)
                           └─ subscribe("audit.ws.*", durable "aero-audit-connector")
                                → B5-2 connector → HTTP sink (external ack → 1→2; 422 → 3 dead)
```

The aero-bus seam is **publish/transport only**: the durable evidence stays in PG (`audit_events` + `audit_outbox`); NATS is the delivery transport with a per-workspace subject namespace that gives the B5-2 connector per-tenant ordering/dedup. Everything this design adds is additive and lands without a migration, a trait change, or touching the four existing streams.

## 2. API changes (`crates/aero-bus`)

No `EventBus` trait change (stays dyn-compatible), no new consumer-config path, no DB migration. Five additive symbols in `crates/aero-bus/src/jetstream.rs` + one doc fix in `lib.rs`.

### 2.1 New public constant (single literal — the one change point for the contract token)

```rust
/// Normative subject namespace for audit-governance delivery (B5-1). Per-workspace
/// subjects ⇒ per-tenant monotonic seq + dedup (the `im.room.{id}` precedent).
/// Single literal shared by the `subscribe()` mapping, the audit relay's publish
/// path, and the B5-2 connector's subscription — any other copy is a bug.
/// Must stay disjoint from B5-3's `audit.priority.` (sibling design §2.1).
pub const AUDIT_SUBJECT_PREFIX: &str = "audit.ws.";
```

Re-exported from `lib.rs` alongside the existing symbols (the B5-2 connector and the relay owner both depend on `aero-bus`):

```rust
pub use jetstream::{AUDIT_SUBJECT_PREFIX, JetStreamBus, JetStreamConfig};
```

### 2.2 Extracted pure mapping fn (replaces the inline chain in `subscribe()`)

```rust
/// Map a subscribe subject to its stream. `None` ⇒ unknown prefix (subscribe
/// fails closed — a typo must never silently attach to another stream).
fn stream_for_subject(subject: &str) -> Option<&'static str> {
    if subject.starts_with("im.room.") {
        Some("IM_MESSAGES")
    } else if subject.starts_with("im.events.") {
        Some("IM_EVENTS")
    } else if subject.starts_with("ai.queue.") {
        Some("AI_QUEUE")
    } else if subject.starts_with("live.stream.") {
        Some("LIVE_EVENTS")
    } else if subject.starts_with(AUDIT_SUBJECT_PREFIX) {
        Some("AUDIT_EVENTS")
    } else {
        None
    }
}
```

`subscribe()` becomes:

```rust
let Some(stream_name) = stream_for_subject(subject) else {
    return Err(BusError::Nats(format!("unknown subject prefix: {subject}")));
};
```

Behavior of the existing four prefixes is byte-identical (same literals, same error message). The B5-3 branch (`audit.priority.`) merges additively when both designs land — second tokens differ, so branch order is irrelevant (multi-agent parallel integration per AGENTS.md §4.1: hand-merge `jetstream.rs`).

### 2.3 Stream config builder (pure, unit-assertable — `im_messages_stream_config()` precedent)

```rust
/// AUDIT_EVENTS is transport: durable evidence lives in PG (`audit_events` +
/// `audit_outbox`). Limits retention keeps replay available to the connector's
/// durable cursor; duplicate_window = max_age lets broker-level `Nats-Msg-Id`
/// dedup cover the relay's full retry horizon (backoff cap 300s is far inside).
/// Coordinated constant with the B5-2 connector's lease/backoff horizon (R-2) —
/// raise both together if the horizon ever exceeds 7d.
const AUDIT_RETENTION: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);
const AUDIT_STREAM_NAME: &str = "AUDIT_EVENTS";

fn audit_events_stream_config() -> stream::Config {
    stream::Config {
        name: AUDIT_STREAM_NAME.into(),
        subjects: vec![format!("{AUDIT_SUBJECT_PREFIX}*")],
        retention: stream::RetentionPolicy::Limits,
        max_age: AUDIT_RETENTION,
        duplicate_window: AUDIT_RETENTION,
        storage: stream::StorageType::File,
        ..Default::default()
    }
}
```

### 2.4 `bootstrap()` declares the stream idempotently

Follows the IM_MESSAGES get-or-create + update pattern (the update half is what lets an existing cluster pick up config changes):

```rust
// Audit-governance delivery (B5-1). Transport only — the durable evidence
// is audit_outbox/audit_events in PG.
let audit_events = audit_events_stream_config();
self.js.get_or_create_stream(audit_events.clone()).await
    .map_err(|e| BusError::Nats(e.to_string()))?;
self.js.update_stream(audit_events).await
    .map_err(|e| BusError::Nats(e.to_string()))?;
info!(stream = "AUDIT_EVENTS", "declared");
```

### 2.5 BR3 — no change

`poison_safe_pull_config` is already generic over subject/durable. The B5-2 connector's durable name `aero-audit-connector` does **not** start with `aero-server-`, so it correctly gets `DeliverPolicy::All` on first creation (backlog replay) with 16/120s poison bounds. Ephemeral consumers keep `New`. Zero code; asserted by test.

### 2.6 `lib.rs` doc fix (BR6)

The crate doc currently lists 3 streams (stale — missing `LIVE_EVENTS`; verified E12). Rewrite the stream list to 5: `IM_MESSAGES` (`im.room.*`, Limits, 7d), `IM_EVENTS` (`im.events.>`, Limits, 30d), `AI_QUEUE` (`ai.queue.*`, WorkQueue, 1d), `LIVE_EVENTS` (`live.stream.*`, Limits, 6h), `AUDIT_EVENTS` (`audit.ws.*`, Limits, 7d). Doc-only.

## 3. Compatibility constraints

- **No breaking change to `EventBus`**: the trait is untouched (stays dyn-compatible); the four existing streams and their consumers behave byte-identically. `stream_for_subject` extraction preserves the exact error string.
- **No new config path**: publish uses `publish_idempotent` (BR4); the connector uses `poison_safe_pull_config` (BR3); seq stamping uses the existing `seq.rs` primitives (BR5). No new env vars in this module.
- **Namespace disjointness (R-1)**: `audit.ws.*` (this design, Limits 7d) vs `audit.priority.*` (B5-3, WorkQueue) are two streams over disjoint subject families — `ws` and `priority` differ at the second token, so no subject can match both prefixes. The load-bearing property is **per-workspace subjects ⇒ per-subject monotonic seq per tenant**, not the literal token; if contract text dictates another namespace, `AUDIT_SUBJECT_PREFIX` is the single change point.
- **Dependency direction**: aero-bus is a base crate (depends only on aero-common). It cannot import `SeqProvider` from aero-im-core. The relay owner composes `SeqProvider` with `aero_bus::stamped_event_bytes` — exactly the composition `live.rs:196–203` already uses. **Multi-instance correctness (stress-test L4) pins the provider**: the relay must receive the same `Arc<dyn SeqProvider>` the server already wires (`seq_store`, Redis `INCR` — `repos.rs:77` / `services.rs:105/167`); a fresh `LocalSeqProvider` would mint interleaved per-instance seqs (1,1,2,2…) for one workspace subject. `LocalSeqProvider` stays the single-instance/dev default only. aero-bus stays provider-agnostic.
- **Payload shape**: the frame must be a JSON object for `stamp_seq` to have somewhere to put the sibling `"seq"` key. The audit payload is JSONB with the 0235 `CHECK (jsonb_typeof(payload) = 'object')` precedent; a non-object payload publishes unstamped (no-op, never an error — seq.rs contract).
- **Duplicate window**: `duplicate_window = max_age = 7d` (IM_MESSAGES precedent, asserted by `im_stream_duplicate_window_covers_outbox_retries`; live-verified on the deployed v2.10.29 — accepted verbatim, >max_age rejected err 10052, dedup survives restart, §0.1 L1/L2). `audit_events_stream_config()` must **always set `duplicate_window` explicitly** — an unset window is server-defaulted to 2m (live-verified on IM_EVENTS), silently shrinking the dedup horizon. The relay backoff cap (300s) is far inside; the sink's idempotency window must be covered by the connector's own receipts (second boundary — the broker cannot be the only dedup layer: after the 7d horizon a re-published id is re-stored as a new message, §0.1 L2). Operational note: the broker's dedup map is in-memory for the whole window — a 7d window retains every distinct event_id in RAM per stream.
- **Existing outboxes untouched (R-5)**: no status-enum retrofit onto `event_outbox` / `stream_go_live_outbox` / `snaplink_delivery_outbox`; IM claim/ordering guards (per-aggregate-version + delivery-ordinal) unchanged; v1 snaplink usage relay untouched.

## 4. Wire envelope + exactly-N expansion contract (BR5)

**Frame** = one constituent audit event's JSON object + top-level `"seq"` (and optionally `"traceparent"`) via `stamped_event_bytes`. The payload schema itself is B5-1/connector contract text (this module only requires object-ness); `extract_seq` on the connector side is the dedup/ordering key.

**Expansion** (the bus-side half of acceptance A3): one `audit_outbox` row → **exactly N published frames**, one per constituent, in constituent ordinal order:

| Property | Value | Rationale |
|---|---|---|
| Subject | `audit.ws.{workspace_id}` (same for all N frames of the row) | per-workspace subject ⇒ per-tenant seq + dedup |
| `Nats-Msg-Id` | constituent's stable event id (distinct per frame) | broker dedup per frame; re-publish of the same row collapses at the broker |
| `seq` | per-subject value from `SeqProvider`, **persisted per constituent before first publish** (COALESCE — `assign_seq_if_absent` pattern, E3) | redelivery reuses the persisted seq ⇒ "same seq on redelivery" holds; monotonic across frames of one row and across rows of one workspace; gaps legal |
| Order | publish in ordinal order | per-subject monotonicity is only meaningful if frames of one row don't interleave with another relay instance's frames — but per-subject INCR guarantees global monotonicity regardless of interleaving |
| `SeqProvider` returns `None` | publish unstamped, never block | live.rs precedent (`None => match self.seq.next_seq…`) |
| 1:1 rows (admin class) | exactly 1 frame | the N=1 case of the same contract |

**Persistence shape (interface requirement on the 0239 DDL, dependency-owned)**: the seqs must survive a relay crash *between seq-assignment and publish*, otherwise a re-claim would mint fresh seqs and break the dedup property. Recommended: child table `audit_outbox_frame (outbox_id, ordinal, event_id, seq BIGINT NULL, payload JSONB NOT NULL)` with `UNIQUE (outbox_id, ordinal)`, `UNIQUE (event_id)`, seq written with `COALESCE(seq, $2)` fenced on the row's claim. (Alternative: a `frame_seqs JSONB` column on the row — acceptable, but the child table keeps the exactly-N contract queryable and is what the relay's per-frame loop reads.) aero-bus itself does not care which — it only consumes the contract: **per-constituent persisted seq, readable on claim, idempotent to assign**. Two Q1 pins on the DDL (restated in §5.1): the seq persist must `RETURNING seq` and the relay stamps **the persisted value, never its pre-persist candidate** — the in-repo relay already does this, `publish_claimed_outbox` consumes the value returned by `assign_seq_if_absent` (outbox.rs:171-174, `event_outbox.rs:424`), not the candidate it minted; and a persist that returns no row (fence miss — the row was re-claimed or completed by another worker) ⇒ **abort the publish**, mirroring `mark_published`'s `if !marked` bail (outbox.rs:204). Publishing on a fence miss is the only way to emit a frame the claim owner did not authorize.

## 5. Dependency contracts (specified here for testability, built by sibling slices)

These are the interface requirements the aero-bus seam's acceptance depends on — pinned so the drill suite (A1–A4) is unambiguous. Ownership per the requirements §3.

### 5.1 `0239_audit_governance_outbox.sql` (B5-1 storage/migrations slice, DR1)

**Tables — plain heaps, explicitly NOT partitioned (Q2-1).** `audit_outbox` + `audit_outbox_frame` are a mutable retry/DLQ state machine (in-place `UPDATE`s, `FOR UPDATE SKIP LOCKED` claim hot path) — exactly the shape 0146's per-table evaluation excludes from partitioning (`webhook_delivery_log` → SKIPPED: "frequently UPDATE / FOR UPDATE'd hot path"). `ensure_audit_event_partitions` (0146/0154) maintains only the `audit_events` daily partition set; nothing maintains an `audit_outbox` set, and a `created_at`-partitioned outbox would grow partitions with no DROP path (its retention is status-based, not date-based). Growth is bounded by the terminal sweep (below) instead.

- `audit_outbox`: `status SMALLINT NOT NULL CHECK (status IN (0,1,2,3))` — 0=pending, 1=claimed, 2=delivered, 3=dead; lease (`claimed_at`, `claim_token` regenerated per claim — 0179 precedent); `attempts`; backoff via `available_at`; `event_id UUID NOT NULL UNIQUE`; `workspace_id UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE` (**pinned choice, Q3-3** — mirrors `audit_events.workspace_id`'s cascade (0146), so a deleted workspace leaves no undeliverable pending rows; the alternative "no FK + connector 4xx → 3 → terminal sweep" is rejected as zombie-prone); `class` message/room/admin; `priority` (semantics/ordering owned by B5-3); `delivery_mode`; `flushed_at TIMESTAMPTZ NULL` (**flush marker, Q2-2** — set at bucket close/window flush; 1:1 rows set it at insert; re-homed rows set it too). Partial pending index **`WHERE status IN (0,1) AND flushed_at IS NOT NULL`** — an **open** (not-yet-flushed) L1 bucket is status 0 and must not be claimable, or the 0→1 sweep claims it early and flushes 1..K−1 constituents (rows ≈ N, violating A3's `≤ ceil(N/K)`).
- **No FK to `audit_events` — mandatory, stated explicitly (Q3-1)**: `audit_events` retention is row `DELETE` (`sweep_before`, audit.rs:64, legal-hold guarded) **and** daily partition `DROP` (0146/0154) — any referencing FK fails the DELETE and hard-blocks the partition DROP even with `ON DELETE CASCADE`. 0162's header documents the identical rationale for `event_outbox` ("a pending event must survive an independently scheduled message-retention sweep"). The frame payload is a **snapshot at enqueue** — delivery never needs the source row.
- Child table `audit_outbox_frame (outbox_id UUID NOT NULL REFERENCES audit_outbox(id) ON DELETE CASCADE, ordinal SMALLINT NOT NULL, event_id UUID NOT NULL UNIQUE, seq BIGINT NULL, payload JSONB NOT NULL CHECK (jsonb_typeof(payload) = 'object'), UNIQUE (outbox_id, ordinal))` — per-constituent `seq` persistence per §4. **Ordinal assignment happens under the bucket row's `FOR UPDATE` (Q2-4)**: two concurrent appends to the same open bucket computing `count+1` both get the same ordinal and one dies on `UNIQUE (outbox_id, ordinal)` — the L1 append must lock the open bucket row first (append predicates on `status = 0`; the claim flips to 1 atomically, so appends never block on a claimed bucket). The claim pump must keep `SKIP LOCKED` — never plain `FOR UPDATE`, or it stalls behind long business txs (event_outbox.rs:333 precedent).
- **Trigger-path idempotency (Q2-3)**: the redirected 0236 trigger fires in the *same tx* as the DR2 write point (`append_in_tx` → `audit_events` INSERT → trigger), so the same `event_id` is enqueued twice. The trigger path MUST be `INSERT … ON CONFLICT (event_id) DO NOTHING` (app path is primary; a same-tx duplicate is a no-op, never a tx-aborting error). Ordering rule: the app path writes the outbox row (and frames) **before** the `audit_events` INSERT — the id is client-generated (`AuditId::new()`, audit.rs:151) — so the trigger's `DO NOTHING` sees the app's row. `event_id` semantics pinned: 1:1 rows carry their own `audit_events.id`; aggregated rows carry the **first constituent's** `audit_events.id`; per-constituent exactly-once lives in `audit_outbox_frame.event_id UNIQUE` (a competing trigger enqueue for an already-framed constituent conflicts there and no-ops).
- **Seq-persist pins (Q1, §4)**: frame seq written `SET seq = COALESCE(seq, $2) … RETURNING seq`, fenced on the row's claim; the relay stamps **the `RETURNING` value — never the pre-persist candidate** (event_outbox.rs:424 `assign_seq_if_absent`; im-core `publish_claimed_outbox` already consumes the returned value, outbox.rs:171-174); a persist that returns no row (fence miss) ⇒ **abort the publish** — mirror `mark_published`'s `if !marked` bail (outbox.rs:204). All settles fenced on `claim_token` (+`attempts`), 0179 clone (FM3).
- **Terminal sweep (Q3-2)**: `DELETE FROM audit_outbox WHERE status IN (2,3) AND updated_at < $cutoff` — webhook precedent `sweep_terminal_before` (webhook_delivery.rs:214); pending/claimed rows are never swept (event_outbox precedent). **No existing sweep covers status-3 dead rows** (verified: zero `DELETE` anywhere in the snaplink modules — `snaplink_delivery_outbox` has no retention at all) — the boot hook mirrors `sweep_webhook_logs` (boot/retention.rs:371, wired at :177). Frames go with the parent via `ON DELETE CASCADE` — no orphan frames holding `UNIQUE (event_id)` forever (which would permanently block re-enqueue of those constituents). **No legal-hold guard on the outbox sweep**: delivered rows are transport copies; the evidence stays in `audit_events`, which 0154/`sweep_before` protect.
- **Re-home of pre-existing snaplink `'audit'` rows (Q4-1)** — one sqlx-transaction migration (0237 precedent), statements in this order:
  1. `CREATE OR REPLACE` redirect **both** `aero_enqueue_snaplink_audit` **and** `aero_reconcile_snaplink_audit` so new + historical audit delivery flows into `audit_outbox` — both, or runtime.rs:216 (`reconcile_audit` every tick) keeps creating snaplink audit rows forever; the `audit_events_snaplink_delivery` trigger attachment survives function replacement.
  2. Re-home (columns verified against 0235: `delivery_id` PK, `destination` CHECK IN ('usage','audit'), `workspace_id`, `idempotency_key` = `audit_events.id::text` for audit rows, `payload` JSONB object carrying `event_id` = `audit_events.id::text`, `occurred_at`, `delivered_at`; pending = `delivered_at IS NULL` — claim_due predicate, snaplink_commercial.rs:302):

     ```sql
     INSERT INTO audit_outbox
            (event_id, workspace_id, class, status, payload, occurred_at, created_at, flushed_at)
     SELECT (payload ->> 'event_id')::uuid, workspace_id, 'admin', 0,
            payload, occurred_at, created_at, clock_timestamp()
       FROM snaplink_delivery_outbox
      WHERE destination = 'audit' AND delivered_at IS NULL
     ON CONFLICT (event_id) DO NOTHING;

     INSERT INTO audit_outbox_frame (outbox_id, ordinal, event_id, seq, payload)
     SELECT outbox.id, 1, outbox.event_id, NULL, outbox.payload
       FROM audit_outbox outbox
       JOIN snaplink_delivery_outbox snaplink
         ON snaplink.destination = 'audit'
        AND snaplink.delivered_at IS NULL
        AND snaplink.idempotency_key = outbox.event_id::text
     ON CONFLICT (event_id) DO NOTHING;

     UPDATE snaplink_delivery_outbox
        SET delivered_at = clock_timestamp(), claim_token = NULL, lease_expires_at = NULL
      WHERE destination = 'audit' AND delivered_at IS NULL;
     ```

     The frame join compares `outbox.event_id::text` (uuid→text, always safe) against `idempotency_key` — never a `::uuid` cast over mixed destination rows (usage `idempotency_key` values like `aero-im:messages_per_month:…` are not UUIDs and would raise a cast error). Re-run is a no-op: re-homed originals are already `delivered_at IS NOT NULL`. `flushed_at` is set so re-homed 1:1 rows satisfy the pending-index predicate.
  3. **v1 relay `Audit` arm disposition — pinned: drop (Q4-1)**: remove `SnaplinkDeliveryDestination::Audit` (`binding_for_claim` :294) and the `reconcile_audit` call (:216) in the same landing. The tombstone UPDATE is what makes the drop safe: `claim_due` is destination-agnostic (snaplink_commercial.rs:302) and a still-running v1 relay would otherwise claim + deliver a re-homed audit event a second time through the old sink (dual-sink). "Keep = dual-sink transition window" is explicitly rejected. v1 `usage` rows are untouched by every statement above (destination filter); the usage arm + `reconcile_usage` stay.
- **Ordering vs neighbors**: 0239 > 0238 ✓ (0238 touches `event_outbox` CHECK / `messages` — disjoint); the only dependencies are 0235 (`snaplink_delivery_outbox`) and 0236 (functions/trigger) — both earlier, so `CREATE OR REPLACE` is safe in the same migration.
- **Upgrade-regression test (Q4-2)**: pin `migration_0239_reroutes_audit_delivery_into_status_machine` in `crates/aero-storage/src/db/migration_tests/snaplink_commercial.rs` — clone `migration_0237_backfills_before_installing_destination_guard` (empty-DB guard, `migrations_through(238)`, seed, `Migrator::new(migration_source())` full chain); assertions in §8.
- v1 `snaplink_delivery_outbox` usage rows untouched (re-home filters `destination = 'audit'` only).

### 5.2 In-tx writes (B5-1 storage/im-core slice, DR2)

`message.create` (extends `insert_outboxed`'s tx, `message/idempotency.rs:158`), `message.edit`, `message.delete` (extends `soft_delete_audited`'s tx, crud.rs:364), `message.react`, `room.create`, `room.update`, `admin.<action>` (extends the existing 20+ `append_in_tx` tokens) each write audit row + outbox row (status 0) in the same tx. Rollback contract = E8 twin tests replicated per action class.

### 5.3 L1 aggregation (B5-1 storage/im-core slice, DR3 — [PROPOSED], no in-repo precedent)

High-volume classes only (`message.send`, reaction, typing): N events → ≤ ceil(N/K) rows, K a new in-repo constant, bucket append transaction-safe (a constituent event must never be lost on rollback). Admin-class rows bypass 1:1 (sibling aero-ai B5-1 R5: `event_id` = `audit_events.id`, exactly once). The acceptance only pins the inequality + bypass — any correct K/window implementation satisfies it. **Ordinal assignment happens under the bucket row's `FOR UPDATE`** (Q2-4, §5.1): two concurrent appends to one open bucket computing `count+1` both get the same ordinal and one dies on `UNIQUE (outbox_id, ordinal)` — the bucket lock is load-bearing. The claim pump keeps `SKIP LOCKED` (never plain `FOR UPDATE`) so it never waits behind a long business tx on a mid-append bucket.

### 5.4 Claim/status machine (relay owner, DR4)

0→1: `SKIP LOCKED` + stale-lease predicate + rotated `claim_token` + `attempts+1` (E3/E6 shape). 1→2: **only after the external ack** — never on local publish alone. 1→3: terminal 4xx (422 and §1.2 set) — dead, excluded by the pending index, no reclaim. Transient: fenced re-park to 0 with `available_at = now + backoff` (superseded claim's failure is a no-op — E6 fence). At-least-once replay must not double-deliver: `Nats-Msg-Id` dedup (BR4) + idempotency-keyed external delivery. Per-constituent loop mirrors `live.rs:196-251` `publish_outboxed_go_live` (row.seq reuse → mint → token-fenced `assign_seq_if_absent` COALESCE → `publish_idempotent`), generalized to the `audit_outbox_frame` child table; the seq provider is the **server's shared Redis `seq_store`** (stress-test L4 — a per-instance `LocalSeqProvider` would mint duplicate interleaved seqs). Relay pump spawned in boot like `dispatch_event_outbox_batch` / `stream_live_outbox::dispatch_batch` (`background.rs:120`, 250ms poll + shared cancel token).

## 6. Failure modes & mitigations

| # | Failure mode | Mitigation (in this module unless tagged) |
|---|---|---|
| FM1 | Typo/unknown subject prefix (`audit.ws` without trailing dot, `audit.wss.…`) | `stream_for_subject` → `None` → subscribe fails closed with the existing `"unknown subject prefix"` error; the connector never silently attaches to another stream (BR1). |
| FM2 | NATS unavailable at publish time (relay) | Transient: relay re-parks the row fenced (status 0 + backoff, E3/E6) — no loss, PG remains the source of truth. aero-bus itself: `publish_idempotent` propagates the error to the caller; the caller owns re-park. |
| FM3 | Stale claim flips status on a row a newer worker owns | 0239 must clone 0179's rotated-`claim_token` + fenced WHERE and event_outbox's `attempts` fencing — a superseded claim's 1→2/1→3/0 is a no-op. **Design constraint on DR4, not an option (R-4).** |
| FM4 | Relay crash between seq-persist and publish | Re-claim reads the persisted per-constituent seqs (COALESCE) → re-publish carries the **same** seqs; monotonicity preserved, gap legal (seq.rs contract). |
| FM5 | Broker redelivery double-delivers a frame | `Nats-Msg-Id` = constituent event_id; `duplicate_window = max_age = 7d` covers the 300s backoff horizon with huge margin (BR2); connector durable receipts are the second boundary. |
| FM6 | Connector down for a long window | Limits 7d retention keeps replay available; a re-created durable `aero-audit-connector` consumer starts at `All` (BR3 — name doesn't match `aero-server-*`); evidence never lost (PG outbox survives NATS purge/restart). |
| FM7 | Two relay instances race on seq assignment | `COALESCE`-style persist (E3): concurrent callers converge on one persisted value; the per-subject counter never double-mints for one frame. |
| FM8 | B5-1/B5-3 both extract `stream_for_subject` | Disjoint prefixes (`audit.ws.` vs `audit.priority.`); branches are additive and order-independent; hand-merge `jetstream.rs` per AGENTS.md §4.1; the unit test pins that `audit.priority.*` does **not** map to `AUDIT_EVENTS`. |
| FM9 | Concurrent bootstrap (two instances) or upgrade on an existing cluster | Benign on 2.10.29 (stress-test L5): `get_or_create_stream` is atomic create-or-return; `update_stream` runs only after it succeeds; server `configUpdateCheck` allows `max_age`/`duplicate_window` changes on live streams with messages (dedup timer re-evaluates); identical configs from both instances both succeed. The update half heals clusters whose stream predates the 7d window. (Reworded from the original "update_stream on a stream that doesn't exist" — that window is unreachable: update only follows a successful get_or_create.) |
| FM10 | Non-object payload (nowhere to stamp `seq`) | `stamp_seq` no-ops on non-objects (existing behavior, unit-tested) — publish proceeds unstamped; the 0235 `jsonb_typeof='object'` CHECK makes this unreachable from the real producer. |

## 7. Migration steps

**This module ships no migration** (BR1–BR6 are pure code). The sequence below is the coordinated landing order; the only hard rule is the AGENTS.md §4.2 build-before-migrate invariant:

1. **This direction (aero-bus, independent)**: BR1–BR6 land — `cargo check --workspace` clean, unit tests green, live-NATS test `#[ignore]`. No migration, no trait change, no boot wiring.
2. **B5-1 storage/migrations**: write `0239_audit_governance_outbox.sql` (interface per §5.1 — redirects + re-home + tombstone in one tx, plus the terminal-sweep repo fn + boot hook, the `migration_0239_reroutes_audit_delivery_into_status_machine` upgrade-regression test, and the v1 relay `Audit`-arm removal) → **`cargo build` FIRST** (migrations are compile-time embedded via `sqlx::migrate!("../../migrations")` in `aero-storage/db.rs` — a build-less migrate silently no-ops the new file) → `aero-cli migrate` on a throwaway DB → `make migrate-smoke` replays 0001→0239 (fresh-deploy proof).
3. **B5-1 write points + repo + L1**: `audit_governance.rs` (sibling of `audit.rs`, file-size guard: audit.rs is already 1049 lines) + in-tx writes per action class (DR2) + aggregation producer (DR3). Unlocks A1, A3-PG.
4. **Relay owner**: claim/status pump (DR4) + exactly-N expansion publish using `aero_bus::AUDIT_SUBJECT_PREFIX` + `publish_idempotent` + seq composition (BR5). Unlocks A2, A3-live.
5. **B5-2 / B5-4**: connector subscribes `subscribe(&format!("{AUDIT_SUBJECT_PREFIX}*"), Some("aero-audit-connector"))` — the constant is the single literal; provisioning gate. G6 green when A1–A4 + 37/37 + moderation drill pass.
6. **Upgrade path on existing clusters**: `bootstrap()` declares `AUDIT_EVENTS` idempotently (get_or_create + update); the four existing streams are untouched (get_or_create leaves existing configs alone); the 0239 migration (step 2) redirects **both** the 0236 enqueue and reconcile functions (runtime.rs:216 calls `reconcile_audit` every tick — an unredirected reconcile keeps creating snaplink audit rows forever), re-homes pre-existing pending `destination='audit'` rows into `audit_outbox`, and tombstones the originals (`delivered_at` set — a still-running v1 relay's destination-agnostic `claim_due` cannot double-deliver through the old sink during a rolling deploy); the v1 relay's `Audit` arm is dropped in the same landing (§5.1 Q4-1). v1 `usage` rows and the usage relay keep working.

## 8. Testable acceptance mapping

| Req | Test | Location | Harness | Pass criteria |
|---|---|---|---|---|
| BR1 | `stream_for_subject_audit_case`: `audit.ws.<uuid>` → `Some("AUDIT_EVENTS")`; `audit.priority.x` → **not** `AUDIT_EVENTS`; `im.room.x`/`im.events.x`/`ai.queue.x`/`live.stream.x` unchanged; `bogus.x` → `None` + subscribe error string unchanged | `jetstream.rs` tests module | pure unit — pattern: `pull_config_bounds_redelivery_for_poison_messages` | mapping exact; error message byte-identical |
| BR1 | `AUDIT_SUBJECT_PREFIX == "audit.ws."` single literal (no second copy in crate) | `jetstream.rs` tests module | pure unit | constant matches normative token |
| BR2 | `audit_events_stream_config_is_limits_with_7d_duplicate_window`: subjects == `["audit.ws.*"]`, `RetentionPolicy::Limits`, `max_age == AUDIT_RETENTION == duplicate_window`, File | `jetstream.rs` tests module | pure unit — pattern: `im_stream_duplicate_window_covers_outbox_retries` | all fields asserted |
| BR3 | `poison_safe_pull_config("audit.ws.<uuid>", Some("aero-audit-connector"))`: durable name, `DeliverPolicy::All` (not `New`), max_deliver 16, ack_wait 120s | `jetstream.rs` tests module | pure unit | All/16/120s asserted |
| BR4 | `validate_publish_subject("audit.ws.<uuid>")` → Ok; `"audit.ws.*"` → wildcard Err; CRLF-in-id rejection still applies | `jetstream.rs` tests module | pure unit | concrete accepted, wildcard rejected |
| BR5 | seq stamping on an audit envelope: stamp once, bytes round-trip, `extract_seq` returns it; `None` → unstamped | `seq.rs` tests module | pure unit (extend existing stamp tests) | same-seq-on-redelivery property asserted at the bytes level |
| A3-bus | `live_nats_audit_stream_declared_and_deduplicates`: connect `bootstrap_streams: true`, read `AUDIT_EVENTS` info → config asserted (Limits, subjects, duplicate_window) | `jetstream.rs` tests module | `#[ignore = "requires a live NATS JetStream at AERO__NATS__URL"]` — pattern: `live_nats_deduplicates_same_message_id` | bootstrap applied config to the live stream |
| A3-bus+relay-contract | `live_nats_audit_expansion_publishes_exactly_n_frames`: in-test relay harness — fixture: one aggregated row (workspace ws, N=3 constituents e1..e3); loop: seq = local counter `next_seq("audit.ws.{ws}")`, `publish_idempotent(subject, stamped_event_bytes(payload, seq), event_id)`; ephemeral subscriber on `audit.ws.{ws}` counts frames | `jetstream.rs` tests module | live NATS `#[ignore]` | exactly 3 frames received; `extract_seq` strictly increasing; re-publish of (event_id, seq) → broker `duplicate=true`, same stream sequence, subscriber sees no 4th frame; a second workspace's frames start at seq 1 (per-subject isolation); stream purged after |
| A1 (DDL+in-tx commit/rollback/idempotent, per action class) | [B5-1/storage] `audit_governance.rs` db_tests, sibling of audit.rs db_tests | PG `#[ignore]` + `DATABASE_URL` + throwaway migrated DB | count=1/status=0 after commit; 0+0 after forced rollback (E8 mirror); no second row on idempotent repeat |
| A1-upgrade (Q4-2) | `migration_0239_reroutes_audit_delivery_into_status_machine`: migrate through 0238 (`migrations_through(238)`), seed a pending `snaplink_delivery_outbox` `destination='audit'` row (columns per 0235: `delivery_id` = 'audit:' + audit id, `idempotency_key` = audit id text, `payload` with `event_id`) + the matching `audit_events` row + a pending `usage` row → full chain (`migration_source()`) → assert: re-home into `audit_outbox` (status 0, `event_id` = `audit_events.id`, `flushed_at` set, 1 frame row); original snaplink row tombstoned (`delivered_at` NOT NULL, lease cleared); a fresh `audit_events` INSERT enqueues via the redirected trigger into `audit_outbox` only (snaplink table untouched); the `usage` row still pending + untouched; re-running the re-home statement no-ops (`ON CONFLICT (event_id) DO NOTHING`) | `crates/aero-storage/src/db/migration_tests/snaplink_commercial.rs` — pattern: `migration_0237_backfills_before_installing_destination_guard` | migrate-through-0238 → seed → full chain → all assertions green; `applied_version` = latest |
| A2 (2-only-after-ack, 422→3, transient backoff) | [relay]+[B5-2] relay owner db_tests + stub sink | PG + stub sink | 1→2 only on ack; 422 → 3 terminal (T-37), excluded by pending index; transient → 0 + backoff, stale claim no-op |
| A3-PG (≤ceil(N/K), admin bypass) | [B5-1/storage] db_tests | PG | `COUNT(audit_outbox) ≤ ceil(N/K)` for N sends; admin row 1:1 with `event_id` = its own `audit_events.id` |
| A4 | [all] `cargo check --workspace` · `cargo test --workspace --lib` (+`-- --ignored`) · `cargo clippy --workspace --all-targets` · `scripts/{truth-check,file-size-check,web-check}.sh` · `make migrate-smoke` | CI + `scripts/test-integration.sh` | 0 new warnings/violations; 0001→0239 replay green; IM event_outbox guards unchanged (no regression in `dispatch_event_outbox_batch`) |

## 9. Out-of-scope (boundary reminders)

- **0239 DDL + in-tx writes + L1 mechanics + status pump** → B5-1 storage/migrations + relay owner (contracts pinned in §5, built by sibling slices).
- **B5-2 connector crate** (lease/backoff/422→dead relay to the sink, claim validation, `audit:event:write`) — consumes the seam via `AUDIT_SUBJECT_PREFIX`; no aero-bus code changes.
- **B5-3 priority lanes** (`audit.priority.*` WorkQueue stream, `ORDER BY priority DESC` claim) — disjoint stream; only shared artifact is the extracted `stream_for_subject` merge (FM8).
- **B5-4 provisioning gate**; **local→outbound action-token mapping** (`message.moderated` → `admin.content.flag`/`admin.moderation.action` — aero-ai direction).
- **IM `event_outbox` invariants** (per-aggregate-version/delivery-ordinal guards), **`NotifyBatch` fan-out**, **`ai_jobs`**, **v1 snaplink usage relay** — untouched.
- **T-37/T-11/"37/37"** are out-of-repo contract test IDs; in-repo they map to A2's 422→3 assertion, A3's fail-closed/no-double-delivery drill, and the G6 gate (self-consistent whichever IDs the contract assigns).

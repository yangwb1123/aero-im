# Requirements Spec — B5-1: audit outbox (status 0/1/2/3 DDL) + in-tx writes + L1 aggregation, bus seam in aero-bus

- **Module**: `crates/aero-bus/src`
- **Direction**: "Audit outbox with status 0/1/2/3 DDL + in-tx writes for message.*/room.*/admin.*, L1-aggregating high-volume events" — `AuditRepo::append_in_tx` is proven atomic for exactly two paths (`soft_delete_audited` → `message.deleted`, `soft_delete_moderated` → `message.moderated`), but message create/edit, reactions, room.*, and most admin.* actions never write audit rows, and there is no durable outbox/status machine between `audit_events` and an external relay: no `audit_outbox` table, no status 0/1/2/3 DDL anywhere in migrations/ (only `event_outbox` 0162/0163/0174 and `stream_go_live_outbox` 0179 exist, both without a numeric status enum). High-volume events (message.send, reaction, typing) have no L1 aggregation seam.
- **Source analysis**: `docs/auto/analyses/crates-aero-bus-src-3f54dec6.json` (direction #1; value 9 / risk-reduction 8 / effort 8 / confidence 8)
- **Campaign**: `aero-im-b5-outbox-relay` (`docs/campaigns/campaign-aero-im-b5.yaml`); in-repo contract anchor `docs/proposals/audit-contract-batch-aero-im.md`; gate **G6 (B5)** = "37/37、T-11、moderation 优先级" (`docs/campaigns/implementation-gate.md:78`); campaign row 1: "Outbox + in-tx：DDL（status 0/1/2/3 normative）；`message.*`/`room.*`/`admin.*` 同事务写入；‡ 类走 L1 | 30 个忽略测试 CI 全绿（37/37）；P2 parity"
- **Status**: Requirements (verified evidence below)
- **Verification date**: 2026-08-07 (line numbers are as-of-verification anchors; drift is possible — the **file/symbol** is the stable grep anchor per AGENTS.md §0)

## 1. Evidence verification (every cited symbol checked against the repo)

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-storage/src/audit.rs` — `AuditRepo::append_in_tx`, partitioned `audit_events`, `sweep_before`, legal-hold guard | ✅ **Verified**. `append_in_tx` at 127 (transaction-scoped, rides the caller's tx); shared generic `append_on` at 138 (pool vs. open tx); `sweep_before` at 67 with the legal-hold `NOT EXISTS legal_holds WHERE lh.active` guard; `ensure_partitions` at 95 (migration 0146 `ensure_audit_event_partitions`); file is exactly 1049 lines. |
| E2 | `migrations/0007_audit.sql`, `0146_audit_events_partition.sql`, `0154_audit_partition_legal_hold.sql` | ✅ **Verified**. All three exist. 0007 = base `audit_events` DDL (ULID `id` PK, `workspace_id` FK, `action`/`target`/`detail`). 0146 = daily RANGE partition conversion + `ensure_audit_event_partitions(keep_days, ahead)` server-side function. 0154 = legal-hold guard on the partition-DROP half. db_test `audit_partition_maintenance_is_idempotent_and_writable` asserts relkind='p'. |
| E3 | `crates/aero-storage/src/event_outbox.rs` — `claim_due`/SKIP LOCKED, `assign_seq_if_absent`, `mark_failed` backoff | ✅ **Verified**. `claim_due` at 295: CTE with `FOR UPDATE SKIP LOCKED` (:333), stale-lease predicate, per-aggregate-version + per-delivery-ordinal `NOT EXISTS` guards (never overtake an earlier unpublished event); `claim_by_id` at 356 (post-commit fast path); `assign_seq_if_absent` at 424 (`COALESCE(seq, $2)` — concurrent callers get one persisted value); `mark_published` at 444 (fenced on `attempts`, no-op on repeat); `mark_failed` at 476 (fenced re-park with `outbox_backoff_delay`, attempts → 1s doubling to 5m cap). |
| E4 | `migrations/0162_event_outbox.sql` (+0163/0174) | ✅ **Verified**. 0162 DDL: `event_id UNIQUE` (NATS dedup key), `attempts`/`available_at`/`claimed_at`/`published_at`/`last_error`, partial pending index — **no numeric status enum** (pending = `published_at IS NULL`). 0163 adds `event_kind` + per-message `aggregate_version` (UNIQUE (message_id, aggregate_version)); 0174 adds `delivery_ordinal` + `room_delivery_sequences` counter trigger. All three exist and match the direction's "no status 0/1/2/3" claim. |
| E5 | `crates/aero-im-core/src/service/outbox.rs` — `dispatch_event_outbox_batch`, `materialize_outbox_payload` | ✅ **Verified**. `dispatch_event_outbox_batch` at 27 (`claim_due(now, OUTBOX_LEASE=30s, limit)` → per-row publish or `mark_failed` re-park, batch never fails wholesale); `dispatch_event_outbox_id` at 45 (immediate post-commit relay, `false` harmless — background batch owns the durable retry); `materialize_outbox_payload` at 213 (stale-event suppression, Notify recipient revalidation). This is the claim/lease 泵 pattern the audit-outbox relay must clone. |
| E6 | `migrations/0179_stream_go_live_outbox.sql` + `crates/aero-server/src/stream_live_outbox.rs` (claim_token fencing, two-stage completion) | ✅ **Verified**. 0179: `claim_token UUID NOT NULL DEFAULT gen_random_uuid()` + two-stage `nats_published_at`/`webhooks_materialized_at` + completion CHECK (`completed_at IS NULL OR (both stages set)`). `stream_live_outbox.rs`: `CLAIM_LEASE = 5m`; `StreamGoLiveOutboxRepo::claim_due` **regenerates `claim_token` on every claim** (storage side :156) and every completion mutation is fenced `WHERE id = $1 AND claim_token = $2` — a stale claim can never release a newer one. The "single-DML snapshot+outbox" pattern (status transition + immutable snapshot written by one statement) is the direction's cited aggregation/snapshot precedent. |
| E7 | `crates/aero-server/src/ws/ws_impl/bus.rs` — NotifyBatch L1 expansion (~line 289) | ✅ **Verified, with correction**. `NotifyBatch` per-recipient expansion at 289–302 (one bus event → one targeted `notify` frame per recipient; avoids O(N) NATS publishes). ⚠️ **This is consumer-side fan-out, not aggregation** — it expands 1→N, it never merges N→1. It is the in-repo precedent for "one durable row → exactly N frames" (which acceptance A3 reuses for the relay side), but the **L1 merge (N→≤ceil(N/K)) has no in-repo precedent** — [PROPOSED] mechanics, same finding as sibling spec E11. |
| E8 | `crates/aero-storage/src/audit.rs` db_tests — `audit_tx_delete_and_audit_commit_together`, `audit_tx_failed_audit_rolls_back_delete` | ✅ **Verified**. Both `#[ignore = "requires live Postgres"]`. The commit-half: soft delete + exactly one `message.deleted` row commit together; repeat delete is a no-op that appends NO second audit row. The rollback-half: a failing audit INSERT (missing workspace → FK violation) rolls the soft-delete back; message stays live, `COUNT(*)` of orphaned audit rows = 0. This atomicity/rollback contract is what acceptance (a) replicates **per action class**. Moderation twin pair: `moderation_delete_and_audit_commit_together` / `moderation_failed_audit_rolls_back_delete`. |
| E9 | (supplementary, evidence correction) `soft_delete_audited` / `soft_delete_moderated` | ⚠️ **Path corrected** (same correction as sibling spec E6): the functions live in `crates/aero-storage/src/message/crud.rs` — `soft_delete_audited` at 364 (one tx: `soft_delete_in_tx` + `AuditRepo::append_in_tx` `'message.deleted'` + commit; `false` on repeat → no audit row), `soft_delete_moderated` at 398 (`'message.moderated'`, system actor `None`). The combined delete+audit+**outbox** single-tx path is `soft_delete_outboxed_system` / `soft_delete_locked_outboxed_in_tx` at `crates/aero-storage/src/message/events.rs:267` / `:299`: lock → soft-delete UPDATE → `cleanup_visible_associations_in_tx` → `enqueue_unreferenced_blobs_in_tx` → `AuditRepo::append_in_tx` (action parameterized) → `EventOutboxRepo::insert_room_event_in_tx` → commit; `Ok(None)` when already deleted (no second audit/outbox row on replay). `audit.rs` owns only the repo + the db_tests. |
| E10 | (supplementary) current audit write coverage | ✅ **Verified**. In-tx `append_in_tx` producers today (20+ action tokens): `member.add`/`member.remove` (`message/authorization.rs:368`), `member.deactivate`/`reactivate` (`participant.rs:890`), `scim.user.*` (provision/deprovision/suspend/reactivate/update), `invitation.create/accept/revoke`, `message.deleted`/`message.moderated`/`message.recalled`, `session.revoked`, `identity.migrated`, `announcement.create/delete`, `integration.installation.*`/`notification.published`/`blob.uploaded`, `bot.delivery.requeued`, `webhook.delivery.requeued`, `workspace.create`/`workspace.delete`; best-effort non-tx: `auth.login.new_ip`/`recovery_code` (`routes/handlers/auth.rs:55,217`), `channel.metadata_changed` (`channels.rs:315`, warn-on-fail). **Gap confirmed**: `message.create` (the create path `insert_outboxed` at `message/idempotency.rs:158` writes `event_outbox` only — no audit row), `message.edit`, `message.react`, `room.create`, `room.update` have **zero** audit writes anywhere (grep over `aero-server`/`aero-storage`/`aero-im-core`). |
| E11 | (supplementary) no `audit_outbox` / no numeric status enum anywhere | ✅ **Verified**. `grep -rn "audit_outbox" migrations/ crates/` → 0 hits. Outbox tables that exist: `event_outbox` (0162/0163/0174), `stream_go_live_outbox` (0179), `snaplink_delivery_outbox` (0235:161 — v1 audit delivery, `delivered_at IS NULL` pending, **no status enum, no class/priority**), enqueued by the 0236 AFTER INSERT trigger `audit_events_snaplink_delivery` (`aero_enqueue_snaplink_audit`, :68/:129-133). No `smallint` status with 0/1/2/3 semantics exists in any outbox DDL. Latest migration = **0238** → a new audit-outbox migration is **0239**, consistent with the proposal's numbering. |
| E12 | (supplementary) aero-bus current bus topology | ✅ **Verified**. `crates/aero-bus/src/jetstream.rs`: `bootstrap()` at 237 declares 4 streams — `IM_MESSAGES` (`im.room.*`, limits, 7d), `IM_EVENTS` (`im.events.>`, limits, 30d), `AI_QUEUE` (`ai.queue.*`, WorkQueue, 1d), `LIVE_EVENTS` (`live.stream.*`, limits, 6h); `subscribe()` at ~343 maps 4 subject prefixes via an inline `starts_with` chain, unknown prefix → `Err(BusError::Nats("unknown subject prefix"))` (fail-closed). No audit subject namespace exists (grep for `audit.ws`/`audit.events`/`audit.room` → 0 hits). `crates/aero-bus/src/lib.rs` doc is already stale (lists 3 streams, missing `LIVE_EVENTS`). |
| E13 | (supplementary) per-subject seq machinery | ✅ **Verified**. `crates/aero-bus/src/seq.rs`: `stamp_seq`/`stamped_event_bytes`/`extract_seq` — stamp once at publish, redeliveries carry the same seq (the dedup key); serde ignores the unknown key on the consumer side. `crates/aero-im-core/src/seq.rs:40`: `SeqProvider` trait (`next_seq(subject) -> Option<u64>`, None ⇒ publish unstamped, never block) with `LocalSeqProvider` (process-local DashMap) and Redis-backed `aero_storage::SeqStore` (cluster-correct, `aero:seq:{subject}` INCR); wired via `ImService::with_seq` / `LiveService::with_seq`. `EventOutboxRepo::assign_seq_if_absent` (E3) persists the stamp per row. All reusable unchanged for an audit subject namespace. |
| E14 | (supplementary) EventBus publish/dedup + consumer-config seams | ✅ **Verified**. `crates/aero-bus/src/traits.rs`: `EventBus` = `publish` / `publish_idempotent` (default falls back to `publish`; JetStream impl overrides with `Nats-Msg-Id` dedup) / `publish_json` / `subscribe`; `validate_publish_subject` rejects wildcards on publish, accepts concrete subjects (e.g. `ai.queue.summarize`); relay convention `publish_idempotent(subject, payload, event_id)` (`live.rs:251`, `session_control.rs:153`); `poison_safe_pull_config` (extracted + unit-tested): `POISON_MAX_DELIVER=16`, `POISON_ACK_WAIT=120s`, `DeliverPolicy::All` for durable consumers; duplicate-window contract asserted by `im_stream_duplicate_window_covers_outbox_retries` (7d = max_age). |

## 2. Verified current state (the pipeline this direction modifies)

```
business write points
  ├─ message.create / edit / react   → messages(+event_outbox) ONLY — NO audit row        (E10)
  ├─ message.delete (user)           → soft_delete_audited   [1 tx: delete + audit]       (E9)
  ├─ message.moderated (system)      → soft_delete_outboxed_system [1 tx: delete + audit  (E9)
  │                                     + RoomEvent::Deleted outbox]
  ├─ room.create / room.update       → rooms ONLY — NO audit row                          (E10)
  └─ admin.* (member/scim/invitation/…) → 20+ append_in_tx tokens, in-tx, no outbox      (E10)
        └─ audit_events (L0, RANGE-partitioned 0146, legal-hold 0154, sweep_before 0007/audit.rs)
             └─ 0236 AFTER INSERT trigger aero_enqueue_snaplink_audit (E11)
                  └─ snaplink_delivery_outbox (0235: NO status enum / class / priority)
                       └─ embedded relay → HTTP sink   (v1 path; NOT the 0/1/2/3 machine)

bus layer (crates/aero-bus, this module):
  bootstrap() ── IM_MESSAGES · IM_EVENTS · AI_QUEUE · LIVE_EVENTS   (E12: no audit stream)
  subscribe() ── 4-prefix inline map, fail-closed on unknown        (E12)
  seq.rs ── stamp_seq / stamped_event_bytes / extract_seq           (E13: generic, reusable)
```

**Gaps the direction closes** (all verified): (1) no `audit_outbox` table and no status 0/1/2/3 DDL anywhere in migrations/ — the only outboxes (`event_outbox` E4, `stream_go_live_outbox` E6, `snaplink_delivery_outbox` E11) use NULL-timestamp pending states; (2) `append_in_tx` atomicity is proven for exactly two paths (E8/E9) while message create/edit/react and room.* write **no** audit rows at all (E10); (3) high-volume events have no L1 aggregation seam — the only precedent shapes are consumer-side fan-out (`NotifyBatch`, E7, 1→N) and the single-DML snapshot (`0179`, E6), never an N→1 merge; (4) the relay-side "exactly N frames with per-subject monotonic seq" publish contract has no bus namespace to publish to (E12/E13).

## 3. Scope

**In scope (this direction, module `crates/aero-bus`)**:
- The **audit subject namespace + stream**: normative `audit.ws.*` (per-workspace subject ⇒ per-workspace monotonic seq), declared in `bootstrap()` as `AUDIT_EVENTS` and mapped in `subscribe()` — the bus seam the audit-outbox relay publishes to and the B5-2 connector subscribes from. No new `EventBus` trait method (stays dyn-compatible).
- The **per-subject seq contract for audit subjects** (existing `seq.rs` primitives + `SeqProvider` seam, E13): stamped once at publish, per-subject monotonic, gaps legal, redelivery carries the same seq; `assign_seq_if_absent`-style persistence.
- The **L1 expansion publish contract** (the bus-side half of acceptance (c)): one aggregated outbox row expands to **exactly N published frames** (one per constituent event, mirroring the NotifyBatch 1→N shape, E7), each with its own stable event id as `Nats-Msg-Id` and its own seq stamp.
- Publish/consumer config reuse: `publish_idempotent` + `validate_publish_subject` (E14), `poison_safe_pull_config` for the connector's durable consumer — no new config path.
- Doc alignment: `crates/aero-bus/src/lib.rs` stream list (currently stale, missing `LIVE_EVENTS` — E12).

**Dependency-owned (acceptance checks preserved here, built by sibling directions — do not build in this module)**:
- `0239_audit_governance_outbox.sql` DDL: `audit_outbox` (status smallint **0=pending/1=claimed/2=delivered/3=dead**, lease/claim fencing, attempts, backoff, `class` message/room/admin, `priority`, `delivery_mode` per proposal) + `CREATE OR REPLACE` redirect of the 0236 enqueue/reconcile functions + `audit_governance.rs` repo + claim pump → **B5-1 storage/migrations slice** (`crates/aero-storage/src/audit.rs` sibling module, campaign row 1).
- In-tx writes for `message.create/edit/delete/react`, `room.create/update`, `admin.<action>` → **aero-storage/aero-im-core write points** (E9/E10 call sites); the direction's atomicity contract (E8) replicated per action class.
- L1 aggregation mechanics (window/bookkeeping, K-batching in the producer; admin-class rows bypass 1:1 — sibling aero-ai B5-1 spec R5) → **B5-1 storage/im-core slice**; no in-repo precedent ([PROPOSED], E7).
- Claim/status machine semantics: 0→1 (SKIP LOCKED lease, E3), 1→2 only after external ack, 422 → 3 dead terminal, transient → backoff re-park (E3/E6 fencing) → **the relay owner** (proposal: `audit_governance.rs` claim pump in aero-storage, invoked from boot like `dispatch_event_outbox_batch`).
- T-37 (terminal) and T-11 (fail-closed) contract tests → **B5-2 connector / integration drills** (out-of-repo test lists, [PROPOSED] — see §7).

**Out of scope**: B5-2 connector crate (lease/backoff/422→dead relay to the sink, cc+claim, `audit:event:write`); B5-3 priority lanes (`audit.priority.*` WorkQueue stream + `ORDER BY priority DESC` claim — sibling spec `2026-08-07-aero-bus-b5-3-priority-delivery-seam.req.md`); B5-4 provisioning gate; local→outbound action-token mapping (`message.moderated` → `admin.content.flag`/`admin.moderation.action` — aero-ai direction); changes to the IM `event_outbox` claim/ordering invariants (per-aggregate-version/delivery-ordinal guards, E3/E4 stay untouched); `NotifyBatch` fan-out; `ai_jobs`; the v1 `snaplink_delivery_outbox` usage relay (proposal: usage relay 原地不动).

## 4. Requirements

### BR1 — Audit subject namespace mapped in `subscribe()` (aero-bus, load-bearing)
- Normative namespace **`audit.ws.{workspace_id}`** (the direction's "per-subject monotonic seq" made concrete; per-workspace subjects ⇒ per-tenant ordering/dedup, matching the `im.room.{id}` precedent). Expose one public constant, e.g. `pub const AUDIT_SUBJECT_PREFIX: &str = "audit.ws."`, used by the mapping, the relay's publish path, and the B5-2 connector's subscription — no second literal.
- Extract the inline `starts_with` prefix chain into a pure fn (e.g. `fn stream_for_subject(subject: &str) -> Option<&'static str>`), returning `Some("AUDIT_EVENTS")` for `audit.ws.*` and the existing stream names for the 4 current families (E12).
- Unknown prefixes keep the current fail-closed behavior (`Err(BusError::Nats("unknown subject prefix: ..."))`) — a typo must never silently land on another stream.

### BR2 — `bootstrap()` declares `AUDIT_EVENTS` with Limits retention
- New stream `AUDIT_EVENTS`: `subjects: ["audit.ws.*"]`, `retention: Limits` (IM_MESSAGES precedent — the audit bus is **transport**, the durable evidence is `audit_outbox` + `audit_events` in PG; Limits keeps redelivery/replay available to the connector's durable cursor), `storage: File`, declared idempotently in `bootstrap()`.
- `max_age` = 7d and `duplicate_window` = `max_age` (IM_MESSAGES precedent, asserted by `im_stream_duplicate_window_covers_outbox_retries`, E14) — broker-level `Nats-Msg-Id` dedup covers the relay's full retry horizon (backoff cap 300s per proposal §1.2 is far inside 7d). Coordinated constant with the B5-2 horizon (see R-2).
- Stream config extracted as a pure builder (mirroring `im_messages_stream_config()`) so retention/duplicate-window are unit-assertable without a live NATS.

### BR3 — Poison-safe pull config applies unchanged to the audit stream
- `poison_safe_pull_config` is already generic over subject/durable (E14): the connector's durable consumer (e.g. `aero-audit-connector`) gets `POISON_MAX_DELIVER=16`, `POISON_ACK_WAIT=120s`, `DeliverPolicy::All` on first creation; ephemeral consumers get `New`. No new config path, no per-stream exceptions.

### BR4 — Publish path accepts concrete audit subjects, no new API
- `validate_publish_subject` accepts concrete `audit.ws.<uuid>` subjects; wildcard `audit.ws.*` remains publish-rejected (E14).
- The audit-outbox relay publishes with the existing `publish_idempotent(subject, payload, event_id)` mechanism (event_id as `Nats-Msg-Id`) — `EventBus` trait: **no changes** (stays dyn-compatible).

### BR5 — Per-subject seq contract for audit subjects + exactly-N expansion
- The relay stamps each frame once at publish via `stamped_event_bytes` (E13) with a per-subject value from the `SeqProvider` seam (Redis `SeqStore` in cluster, `LocalSeqProvider` default); a redelivery carries the same seq; gaps are legal; `None` ⇒ publish unstamped (never block delivery). Persist the stamp per outbox row (`assign_seq_if_absent` pattern, E3) so the relay and the connector agree on one value.
- **Expansion contract** (bus-side half of acceptance (c)): one L1-aggregated `audit_outbox` row → **exactly N published frames**, one per constituent event, each with its own stable event id (Nats-Msg-Id) and its own seq stamp in the same per-subject monotonic sequence — the 1→N shape of `NotifyBatch` expansion (E7) applied publish-side. Non-aggregated (admin-class / 1:1) rows → exactly 1 frame.

### BR6 — Doc alignment
- `crates/aero-bus/src/lib.rs` stream list updated to include `AUDIT_EVENTS` (and the already-missing `LIVE_EVENTS`), so the crate doc matches `bootstrap()` (E12).

### DR1 — `audit_outbox` DDL with status 0/1/2/3 (dependency: B5-1 storage/migrations)
- New migration **`0239_audit_governance_outbox.sql`** (next number after 0238, verified E11) creates `audit_outbox` with: `status SMALLINT NOT NULL` + `CHECK (status IN (0,1,2,3))` (0=pending, 1=claimed, 2=delivered, 3=dead), lease columns (`claimed_at`, `claim_token` regenerated per claim — 0179 precedent E6), `attempts`, backoff via `available_at` (E3), stable `event_id UNIQUE`, `class` message/room/admin, `priority`, `delivery_mode` (proposal B5-1 shape), partial pending index `WHERE status IN (0,1)`.
- The 0236 AFTER INSERT enqueue/reconcile functions are redirected (CREATE OR REPLACE) so audit delivery flows into the status machine; P2 parity: non-aggregated rows keep `event_id` = the same transaction's `audit_events.id` (1:1).

### DR2 — In-tx writes for message.*/room.*/admin.* (dependency: storage/im-core write points)
- Each of `message.create`, `message.edit`, `message.delete`, `message.react`, `room.create`, `room.update`, `admin.<action>` writes its audit row **and** its `audit_outbox` row (status 0) in the SAME transaction as the business mutation (E9 single-tx shape; create path extends `insert_outboxed`'s tx, E10). Rollback contract mirrors `audit_tx_failed_audit_rolls_back_delete` (E8): any in-tx failure rolls back business row + audit row + outbox row together — no "created but unaudited" observable state. Idempotent repeats (already-deleted message, duplicate create by idempotency key) append nothing (E8 commit-half).

### DR3 — L1 aggregation for high-volume events (dependency: B5-1 storage/im-core)
- High-volume classes (`message.send`, reaction, typing) are aggregated: N events → **≤ ceil(N/K)** outbox rows (K = in-repo constant, see R-3), each row carrying the K constituent events; the aggregation bucket must be transaction-safe (append to the open bucket in-tx, flush on window/commit). **Admin-class rows bypass aggregation** 1:1 (sibling aero-ai B5-1 spec R5: moderation rows keep `event_id` = `audit_events.id` exactly once). [PROPOSED] mechanics — no in-repo precedent (E7).

### DR4 — Claim/status machine semantics (dependency: the relay owner)
- Claim: status 0→1 with `SKIP LOCKED`, lease, `attempts+1`, rotated `claim_token` (E3/E6 precedents). Success: **1→2 only after the external ack** (connector receipt — never on local publish alone); **422 (and per §1.2 terminal 4xx) → 3 dead terminal** (T-37). Transient failure: re-park to status 0 with `available_at` = now + backoff (fenced on the claim token/attempts — a superseded claim's failure is a no-op, E6). At-least-once replay must not double-deliver: `Nats-Msg-Id` dedup (BR4) + idempotency-keyed external delivery.

### DR5 — Relay publishes exactly N frames (dependency: relay owner + BR5)
- The relay consumes the aggregated row's N constituents and publishes exactly N frames on `audit.ws.{workspace}` with per-frame event ids and per-subject monotonic seq — the count/seq assertions of A3 are asserted end-to-end through the bus seam (BR5).

## 5. Acceptance checks (preserved from the direction, made testable)

PG tests follow the existing db_test harness (`#[tokio::test] #[ignore = "requires live Postgres"]`, throwaway migrated DB, helpers `pool()`/`fixture()`/`message_in_workspace()` — audit.rs precedents E8). Ownership tags: **[aero-bus]** = built & tested in this module; **[B5-1/storage]**, **[relay]**, **[B5-2]** = dependency-owned, preserved here verbatim so the drill suite is complete.

### A1 — `audit_outbox` row count 1 / status 0 after commit, 0 after forced rollback, per action class **[B5-1/storage]**
*Preserves (a): "new migration adds audit_outbox (status smallint 0=pending/1=claimed/2=delivered/3=dead, lease, attempts, backoff columns) written by append_in_tx in the SAME tx as the business row — assert row count 1 and status 0 immediately after commit, 0 after forced rollback (mirrors audit_tx_failed_audit_rolls_back_delete)".*
- **Setup**: throwaway migrated DB (`DATABASE_URL` + `#[ignore]` harness per audit.rs:479), 0239 applied (`migrate-smoke` replay); per action class `message.create` / `message.edit` / `message.delete` / `message.react` / `room.create` / `room.update` / `admin.<action>` (one representative per class, through the production write path).
- **Assert 1 (DDL)**: `information_schema.columns` on `audit_outbox` shows `status` smallint with CHECK `IN (0,1,2,3)`, lease (`claimed_at`/`claim_token`), `attempts`, backoff (`available_at`), `event_id` unique.
- **Assert 2 (commit)**: after the action commits, `SELECT COUNT(*) FROM audit_outbox` for that business row = **1** and `status = 0`; the matching `audit_events` row exists in the same tx (P2 parity where 1:1).
- **Assert 3 (rollback)**: force an in-tx failure (e.g. missing-workspace FK, mirroring `audit_tx_failed_audit_rolls_back_delete` E8) → **0** `audit_outbox` rows and **0** `audit_events` rows escaped; the business row rolled back (message live, room absent).
- **Assert 4 (idempotent repeat)**: replaying the same action (already-deleted message, duplicate idempotency key) appends **no** second outbox row (commit-half of E8).

### A2 — Relay marks 2 only after external ack; 422 lands in status 3 **[relay] + [B5-2]**
*Preserves (b): "relay marks 2 only after external ack, and 422 responses land in status 3 (T-37 terminal test)".*
- **Setup**: claimed row (status 1) with a stub external sink; relay pump under test.
- **Assert 1**: sink returns success + connector ack → status transitions 1→**2**; assert no path marks 2 without the ack (publish-to-NATS alone must NOT flip status).
- **Assert 2**: sink returns **422** → status transitions 1→**3** (dead terminal); the row is never reclaimed (pending index `status IN (0,1)` excludes it); attempts/backoff no longer apply. Same terminal assertion for the §1.2 terminal set (T-37 terminal test).
- **Assert 3**: transient failure (5xx/timeout) → row re-parks to status 0 with `available_at` = now + backoff (fenced; a superseded claim's failure is a no-op — E6 shape).

### A3 — L1 aggregation: N>1 sends → ≤ceil(N/K) rows; relay publishes exactly N frames with per-subject monotonic seq **[B5-1/storage] + [relay] + [aero-bus]**
*Preserves (c): "high-volume events (message.send) are aggregated L1: assert N>1 sends produce ≤ ceil(N/K) outbox rows and relay publishes exactly N frames with per-subject monotonic seq (proposed T-11 coverage)".*
- **PG [B5-1/storage]**: issue N>1 `message.send` actions (same workspace, K known) → `COUNT(*) FROM audit_outbox` ≤ `ceil(N/K)`; each row status 0 with the N constituents.
- **PG parity**: interleave one admin-class row among the N sends → that row is 1:1 (not merged), `event_id` = its own `audit_events.id`.
- **Bus [aero-bus]**: `stream_for_subject("audit.ws.<uuid>") == Some("AUDIT_EVENTS")`; `validate_publish_subject("audit.ws.<uuid>")` OK, `"audit.ws.*"` publish-rejected; `audit_events_stream_config()` → `subjects == ["audit.ws.*"]`, Limits, `duplicate_window == max_age`; `poison_safe_pull_config("audit.ws.x", Some("aero-audit-connector"))` → All/16/120s.
- **Live-NATS [aero-bus] + [relay]** (`#[ignore]`, `AERO__NATS__URL`, pattern `live_nats_deduplicates_same_message_id`): relay drains the aggregated row → **exactly N frames** land on `audit.ws.<ws>`; per-subject seq strictly monotonic across the N frames (and across the aggregated rows of that workspace); replay of the same outbox row (claim → fail → re-claim) republishes with the **same** seq per frame and broker dedup collapses duplicates (Nats-Msg-Id). A second workspace's frames start their own sequence (per-subject isolation).

### A4 — Existing full suite stays green **[all]**
*Preserves (d): "existing full suite stays green: cargo test --workspace --lib + scripts/truth-check.sh".*
- `cargo check --workspace` clean; `cargo test --workspace --lib` all green (PG-gated tests via `-- --ignored` + `DATABASE_URL` + throwaway migrated DB); `cargo clippy --workspace --all-targets` no new warnings; `scripts/{truth-check,file-size-check,web-check}.sh` 0 violations; `make migrate-smoke` replays 0001→0239 on a throwaway DB (fresh-deploy proof for the new migration).
- The IM `event_outbox` claim/ordering invariants (E3/E4 guards) are unchanged — no regression in `dispatch_event_outbox_batch` behavior.

## 6. Test placement

| Test | Location | Harness |
|---|---|---|
| BR1/BR4 mapping + publish-validation unit tests (`stream_for_subject` audit case, constant, concrete-subject acceptance) | `crates/aero-bus/src/jetstream.rs` tests module | pure unit, no NATS — pattern: `pull_config_bounds_redelivery_for_poison_messages` |
| BR2 config-builder unit tests (subjects, Limits, duplicate_window=max_age) | `crates/aero-bus/src/jetstream.rs` tests module | pure unit — pattern: `im_stream_duplicate_window_covers_outbox_retries` |
| BR3 poison-safe config for `audit.ws.*`/`aero-audit-connector` | `crates/aero-bus/src/jetstream.rs` tests module | pure unit |
| BR5 seq stamping (stamp once, redelivery same seq, gap tolerance) | `crates/aero-bus/src/seq.rs` tests module (extend existing stamp tests) | pure unit |
| A3 live stream declaration + exactly-N publish + monotonic per-subject seq + dedup collapse | `crates/aero-bus/src/jetstream.rs` tests module | `#[ignore]` live NATS at `AERO__NATS__URL` — pattern: `live_nats_deduplicates_same_message_id` |
| A1 (DDL + in-tx per action class, commit/rollback/idempotent-repeat) | aero-storage db_tests next to `audit.rs` db_tests (audit_governance.rs sibling module) | PG, `#[ignore]` + `DATABASE_URL` + throwaway migrated DB |
| A2 (status machine: 2-after-ack, 422→3 T-37, transient backoff) | relay owner's db_tests + integration | PG + stub sink; G6 gate |
| A3-PG (≤ceil(N/K), admin bypass 1:1) | aero-storage db_tests | PG |
| A4 (suite green + migrate-smoke) | CI + `make migrate-smoke` + `scripts/test-integration.sh` | full suite; gate G6 "37/37、T-11" drill entry |

## 7. Risks / [PROPOSED] items

- **R-1 — Exact audit subject token is out-of-repo contract text**: the v2 docs (proposal §1.2) are not in this repo; `audit.ws.{workspace_id}` is pinned here **normatively for testability** (same stance as the sibling B5-3 spec's `audit.priority.*`). If the contract dictates another namespace, the public `AUDIT_SUBJECT_PREFIX` constant (BR1) is the single change point; the per-workspace-subject property (per-subject monotonic seq per tenant) is the load-bearing part, not the literal token. **Must stay disjoint from B5-3's `audit.priority.*`** (sibling spec BR1) — two streams, two namespaces, no overlap.
- **R-2 — `max_age`/`duplicate_window` vs B5-2 horizon**: pinned at 7d (= IM_MESSAGES precedent, `duplicate_window = max_age`). If B5-2's lease/backoff horizon or the sink's idempotency window ever exceeds 7d, the constants must be raised together — coordinated constant, flagged not invented independently. NATS is transport; the PG `audit_outbox` remains the durable source of truth (a crashed/dead connector loses no evidence).
- **R-3 — L1 aggregation mechanics and K are [PROPOSED]**: no in-repo N→1 merge precedent exists (E7 — `NotifyBatch` is fan-out; 0179 is a single-row snapshot). K (bucket size) is a new in-repo constant (proposal suggests a window/flush model); the acceptance only pins the inequality `≤ ceil(N/K)` and the admin-class bypass, so any correct K/window implementation satisfies A3. The aggregation bucket must be transaction-safe (never lose a constituent event on rollback — the direction's atomicity contract applies to the bucket too).
- **R-4 — Status-machine fence discipline**: with a numeric status enum, the stale-lease hazard changes shape (a superseded claim must not flip 1→2 or 1→3 on a row another worker now owns). The 0179 `claim_token` rotation + fenced WHERE (E6) and `event_outbox` `attempts` fencing (E3) must both be cloned into the 0239 claim/completion statements — this is a design constraint on DR4, not an option.
- **R-5 — Existing outboxes untouched**: this direction adds `audit_outbox`; it does not retrofit a status enum onto `event_outbox`/`stream_go_live_outbox`/`snaplink_delivery_outbox`, does not change the IM claim ordering guards (E3/E4), and does not move the v1 usage relay. The 0236 redirect touches only the audit delivery path (DR1).
- **R-6 — T-37 / T-11 / "37/37" are out-of-repo contract test IDs**: the exact test lists live in the v2 contract docs (proposal: "37/37 测试清单…不在本仓库"). In-repo they map to: A2's 422→3 terminal assertion (T-37), A3's fail-closed/no-double-delivery drill (T-11, sibling B5-3 A5 keeps the provisioning fail-closed), and the G6 gate "30 个忽略测试 CI 全绿（37/37）" (implementation-gate.md:63). The spec's assertions are self-consistent whichever IDs the contract assigns.
- **R-7 — Evidence corrections vs. the direction's citation**: `soft_delete_audited`/`soft_delete_moderated` live in `message/crud.rs` (E9), not `audit.rs` (which holds the repo + db_tests); `NotifyBatch` is fan-out, not aggregation (E7) — the L1 merge is genuinely new.

## 8. Sequencing

1. **This direction (aero-bus)**: BR1–BR6 — pure additive seam (new stream + mapping + constant + seq/expansion contract + docs); no migration, no trait change; lands independently and is unit/live-NATS-testable today.
2. **B5-1 storage/migrations**: 0239 DDL (DR1) + `audit_governance.rs` repo + in-tx writes per action class (DR2) + L1 aggregation producer (DR3) — unlocks A1, A3-PG.
3. **Relay owner**: claim/status pump (DR4) + exactly-N expansion publish (DR5) — unlocks A2, A3-live.
4. **B5-2 / B5-4**: connector subscribes to `audit.ws.*` (using the BR1 constant), 422→dead, provisioning — unlocks the T-37/T-11 drill entries; G6 gate green when A1–A4 + 37/37 + moderation drill all pass.

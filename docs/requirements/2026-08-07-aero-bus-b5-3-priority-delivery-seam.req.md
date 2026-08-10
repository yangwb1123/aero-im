# Requirements Spec — Priority-aware delivery seam in aero-bus for moderation events (B5-3)

- **Module**: `crates/aero-bus`
- **Direction**: "Priority-aware delivery seam in aero-bus for moderation events (B5-3)" — contract requires `admin.content.flag` / `admin.moderation.action` claims delivered before backlog; no priority exists anywhere in the pipeline today (aero-bus `subscribe()`/`poison_safe_pull_config` treat every subject equally; `EventOutboxRepo::claim_due` orders only by `available_at ASC`; `moderation_bot.rs` enqueues into a FIFO bounded mpsc)
- **Source analysis**: `docs/auto/analyses/crates-aero-bus-f74336c1.json` (direction #1; value 9 / risk-reduction 7 / effort 4 / confidence 8)
- **Campaign**: `aero-im-b5-outbox-relay` (`docs/campaigns/campaign-aero-im-b5.yaml`); in-repo contract anchor `docs/proposals/audit-contract-batch-aero-im.md`; gate **G6 (B5)** = "37/37、T-11、moderation 优先级" (`docs/campaigns/implementation-gate.md:78`, row 3 "注入积压 drill：moderation 先达 sink")
- **Status**: Requirements (verified evidence below)
- **Verification date**: 2026-08-07 (initial) / **2026-08-08 (re-verified — anchors refreshed, corrections in E1/E2/E3; sibling design doc `docs/design/2026-08-07-aero-bus-b5-3-priority-delivery-seam.design.md` pins the API shape)** (line numbers are as-of-verification anchors; drift is possible — the **file/symbol** is the stable grep anchor per AGENTS.md §0)

## 1. Evidence verification (every cited symbol checked against the repo)

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-bus/src/jetstream.rs` `subscribe` subject-prefix→stream mapping | ✅ **Verified**. `subscribe` at :338; inline `starts_with` chain (:344–353): `im.room.`→`IM_MESSAGES`, `im.events.`→`IM_EVENTS`, `ai.queue.`→`AI_QUEUE`, `live.stream.`→`LIVE_EVENTS`; any other prefix → `Err(BusError::Nats("unknown subject prefix: {subject}"))`. Every subject family treated identically (equal priority). **Fresh grep (2026-08-08): `rg -n AUDIT_PRIORITY crates/` = 0 hits** — the stream literal exists nowhere in crates/ (the drill's doc comment names the hyphenated bin `aero-audit-priority-drill`, not the stream); same for `stream_for_subject` / `AUDIT_PRIORITY_SUBJECT_PREFIX` / `audit.priority`. |
| E2 | `poison_safe_pull_config` (jetstream.rs) | ✅ **Verified**. Extracted + unit-tested pure fn at :145 (drifted from the 2026-08-07 "~96" anchor; same symbol): `POISON_MAX_DELIVER=16`, `POISON_ACK_WAIT=120s`, `DeliverPolicy::All` for durable queue/work consumers, `DeliverPolicy::New` for ephemeral / `aero-server-*` process-local; generic over subject — no priority concept anywhere. |
| E3 | `bootstrap` — AI_QUEUE WorkQueue-retention precedent | ✅ **Verified**. `bootstrap()` at :237 (drifted from "~240"; same symbol) declares `AI_QUEUE` (:268): `subjects: ["ai.queue.*"]`, `retention: stream::RetentionPolicy::WorkQueue`, `max_age: 24h`, `storage: File`, plain `get_or_create_stream`; total declared = 4 (incl. `LIVE_EVENTS`), **no AUDIT_PRIORITY**. The in-repo precedent for a work-queue stream the priority seam must clone. |
| E4 | `crates/aero-storage/src/event_outbox.rs::claim_due` (`ORDER BY candidate.available_at ASC`, no priority column; cited :330) | ✅ **Verified**. Line 330: `ORDER BY candidate.available_at ASC, candidate.created_at ASC, candidate.id ASC`; `COLUMNS` (line 17) has no `priority`. Table DDL: `migrations/0162_event_outbox.sql`. This is the pattern-evidence of the gap (the ordering the B5-3 audit-governance claim must invert); per sibling spec E6/E7 the **modification target is the B5-1 governance outbox claim**, not the IM `event_outbox` (see §3). |
| E5 | `crates/aero-server/src/moderation_bot.rs` (`try_enqueue`, bounded mpsc, `SkipReason::QueueFull`) | ✅ **Verified**. `try_enqueue` at line 168 → `mpsc::error::TrySendError::Full(_) => SkipReason::QueueFull` (line 170); bounded channel `mpsc::channel::<ModerationJob>(cfg.queue_capacity.max(1))` (line 272), capacity = `AERO_AI_MODERATION_QUEUE` default **512** (line 91); durable consumer `"aero-moderation"` on `im.room.*` (line 313). FIFO in-process queue — the third equal-priority hop. |
| E6 | `crates/aero-storage/src/audit.rs` (`soft_delete_moderated` writes `message.moderated`) | ⚠️ **Partially verified (evidence correction)**. `soft_delete_moderated` lives in `crates/aero-storage/src/message/crud.rs:398` (same local token `'message.moderated'`, action at ~411); `audit.rs` owns `append_in_tx` (:127) and the db_test `moderation_delete_and_audit_commit_together` (:808) proving the audit row shape (`action='message.moderated'`, actor NULL). No impact on the design; grep anchor corrected. |
| E7 | `docs/proposals/audit-contract-batch-aero-im.md` B5-3 | ✅ **Verified (as proposed)**. In-repo file is a 15-line gate summary (the 294-line full proposal is out-of-repo). Line 10: "B5-3：`priority` 列 + `claim_due` 按 `priority DESC` 排序；本地 `message.moderated` → 出站 `admin.content.flag`/`admin.moderation.action` 映射表；注入积压 drill（500 积压 + 1 moderation → 先达 sink）+ 反饥饿上限。" Gate note: "T-11 与 moderation drill 本仓库可全绿". Outbound token list is out-of-repo contract text — [PROPOSED]. |
| E8 | (supplementary) in-repo priority-lane precedent `migrations/0130_ai_job_priority.sql` + `ai_job.rs::priority_for` | ✅ **Verified**. 0130: `ADD COLUMN IF NOT EXISTS priority SMALLINT NOT NULL DEFAULT 100` + partial claim index `(priority, scheduled_at) WHERE status='queued'`; `ai_job.rs:62 priority_for` pure fn, **lower = more urgent** (Moderate=10 < Answer=20 < Summarize=50 < Embed=100); claim `ORDER BY priority ASC, scheduled_at ASC` (:179); unit test `priority_lanes_order_user_facing_work_ahead_of_backfill` (:421). ⚠️ **Convention conflict**: in-repo precedent is ASC/lower-first; the direction pins `ORDER BY priority DESC` (higher-first) — must not be mixed inside the audit outbox (see §7 risk R-1). |
| E9 | (supplementary) v1 audit delivery + fail-closed gates | ✅ **Verified**. `0235_snaplink_commercial_control_plane.sql` line 161: `snaplink_delivery_outbox` — **no status/priority/class**; claim `ORDER BY available_at, created_at, delivery_id` (`snaplink_commercial.rs:302,320`). `0236_snaplink_governance_reconciliation.sql` lines 68/129–133: AFTER INSERT trigger enqueues `delivery_id='audit:'\|\|id`, payload action passes through unmapped; missing binding → RAISE → **transaction aborts (fail-closed = T-11 shape)**. Latest migration = `0238` → B5-1's `0239_audit_governance_outbox.sql` numbering is consistent. |
| E10 | (supplementary) bus publish/dedup seams + G6 gate | ✅ **Verified**. `traits.rs`: `EventBus` = `publish` / `publish_idempotent` (default falls back to `publish`, dyn-compatible) / `publish_json` / `subscribe`; `publish_idempotent_ack` private, `Nats-Msg-Id` dedup precedent `IM_MESSAGE_DUPLICATE_WINDOW` (7d, = max_age, asserted by `im_stream_duplicate_window_covers_outbox_retries`); `validate_publish_subject` rejects wildcards on publish but accepts concrete subjects (`ai.queue.summarize` in `accepts_well_formed_concrete_subjects`); relay convention: `publish_idempotent(subject, bytes, event_id)` (`live.rs:251`, `session_control.rs:153`). `docs/campaigns/implementation-gate.md:78`: **G6 (B5) = 37/37、T-11、moderation 优先级**; row 3 = "outbox schema（flag/partition）→ 注入积压 drill：moderation 先达 sink". |

## 2. Verified current state (the pipeline this direction modifies)

```
moderation verdict  (AiWorker::handle_moderate  crates/aero-ai/src/worker/mod.rs:352
                     or moderation_bot  crates/aero-server/src/moderation_bot.rs)
  └─ soft_delete_moderated / soft_delete_outboxed_system  (message/crud.rs:398 / message/events.rs:267)
      [ one PG tx: soft delete + AuditRepo::append_in_tx 'message.moderated' + RoomEvent::Deleted ]
      └─ 0236 AFTER INSERT trigger → snaplink_delivery_outbox row  (0235:161: NO status/priority/class)
           └─ claim ORDER BY available_at, created_at, delivery_id  (snaplink_commercial.rs:302,320)
                └─ in-process relay → HTTP sink

bus layer (crates/aero-bus, this module):
  bootstrap()  ──  IM_MESSAGES (Limits, 7d) · IM_EVENTS (Limits, 30d) · AI_QUEUE (WorkQueue, 1d) · LIVE_EVENTS (Limits, 6h)
  subscribe()  ──  4-prefix inline map, all equal priority, unknown prefix hard-errors
  poison_safe_pull_config ──  finite max_deliver=16 / ack_wait=120s, All|New by durable kind
```

**Gaps the direction closes** (all verified): (1) no priority anywhere on the bus — `subscribe()` maps every subject family to a stream with identical delivery semantics (E1/E2); (2) the claim query that decides what reaches the sink first orders by `available_at ASC` only (E4), so 500 backlog rows enqueued earlier always precede a later moderation claim — the "500 backlog + 1 moderation → moderation reaches sink first" drill fails, G6 stays red (E10); (3) the in-process moderation queue is FIFO bounded mpsc (E5) — a third equal-priority hop; (4) the only in-repo priority-lane precedent is `ai_jobs` (E8) — proof the pattern works, with a **lower-first ASC** convention the audit outbox must not blindly copy (direction pins DESC).

## 3. Scope

**In scope (this direction, module `crates/aero-bus`)**:
- A dedicated high-priority subject namespace (normative: **`audit.priority.*`**, per the direction's example) with its own stream declared in `bootstrap()` using **WorkQueue retention** (AI_QUEUE precedent, E3), mapped in `subscribe()` (E1) — the bus-level seam that lets moderation claims transit independently of backlog.
- Reuse of `poison_safe_pull_config` for the new stream (no new consumer-config path); publish-path acceptance of concrete `audit.priority.*` subjects via the existing `validate_publish_subject` / `publish_idempotent` (event_id as `Nats-Msg-Id`) mechanism — **no new `EventBus` trait method** (trait stays dyn-compatible).
- Minimal testability seam: extract the inline prefix map into a pure fn (mirroring the existing extracted-and-tested `poison_safe_pull_config` / `validate_publish_subject` / `im_messages_stream_config`) + a public prefix constant so the connector (B5-2) subscribes to the same namespace. No production behavior outside aero-bus changes in this module.
- Doc alignment: `crates/aero-bus/src/lib.rs` stream list (currently stale — lists 3 streams, missing `LIVE_EVENTS`).

**Dependency-owned (acceptance checks preserved here, built by sibling directions — do not build in this module)**:
- `priority` column in the governance-outbox DDL (`0239_audit_governance_outbox.sql`, `class` message/room/admin, `priority`, `delivery_mode`) → **B5-1 (aero-storage)**.
- `ORDER BY priority DESC` in the governance-outbox claim + anti-starvation cap semantics → **B5-3 (aero-storage)** — sibling spec `docs/requirements/2026-08-06-aero-ai-moderation-governance-outbox.req.md` §8 assigns "priority column semantics + claim_due ORDER BY priority DESC + anti-starvation cap → B5-3 (aero-storage)"; the existing IM `event_outbox` table/claim (`event_outbox.rs`) is **pattern evidence only, not modified** — IM delivery ordering invariants (per-aggregate-version / delivery-ordinal guards) stay untouched.
- 403→dead (T-11) connector delivery + provisioning → **B5-2 / B5-4** (this seam must not bypass them, A5).

**Out of scope**: local→outbound action-token mapping table (`message.moderated` → `admin.content.flag` / `admin.moderation.action`) — aero-ai direction; `moderation_bot.rs` FIFO queue changes; `ai_jobs` claim changes; per-subject seq/envelope (`seq.rs`) changes; sink-side contract.

## 4. Requirements

### BR1 — Priority subject namespace mapped in `subscribe()` (aero-bus, load-bearing)
- Pin normative namespace `audit.priority.*` (direction's example made normative for testability); expose it as a single public constant (e.g. `pub const AUDIT_PRIORITY_SUBJECT_PREFIX: &str = "audit.priority."`) used by both the mapping and the B5-2 connector's subscription — no second literal.
- Extract the inline `starts_with` prefix chain into a pure fn (e.g. `fn stream_for_subject(subject: &str) -> Option<&'static str>`) returning `Some("AUDIT_PRIORITY")` for `audit.priority.*` and the existing stream names for the 4 current families.
- Unknown prefixes keep the current fail-closed behavior: `Err(BusError::Nats("unknown subject prefix: ..."))` — a typo must never silently land on another stream.

### BR2 — `bootstrap()` declares the priority stream with WorkQueue retention
- New stream `AUDIT_PRIORITY` (name per the existing `IM_MESSAGES`/`AI_QUEUE`/`LIVE_EVENTS` convention): `subjects: ["audit.priority.*"]`, `retention: stream::RetentionPolicy::WorkQueue`, `storage: File`, declared idempotently in `bootstrap()` (AI_QUEUE precedent E3: `get_or_create_stream`).
- `max_age` = 24h (AI_QUEUE precedent) and `duplicate_window` = `max_age` (IM_MESSAGES precedent E10: broker collapses retried claims with the same `Nats-Msg-Id` for the full retained horizon; asserted by the existing `im_stream_duplicate_window_covers_outbox_retries` pattern). Both are coordinated constants with the B5-2 retry/lease horizon (backoff cap 300s, §1.2) — WorkQueue removes acked messages, so `max_age` is a dead-connector safety valve; the PG governance outbox remains the durable source of truth.
- Stream config extracted as a pure builder (mirroring `im_messages_stream_config()`) so retention policy is unit-assertable without a live NATS.

### BR3 — Poison-safe pull config applies unchanged to the new stream
- `poison_safe_pull_config` is already generic over subject/durable (E2) — the priority stream's consumers (e.g. durable `aero-audit-connector`) get `POISON_MAX_DELIVER=16`, `POISON_ACK_WAIT=120s`, `DeliverPolicy::All` on first creation; ephemeral consumers get `New`. No new config path, no per-stream exceptions.
- WorkQueue ack semantics: consumers **must ack** after processing (WorkQueue removes acked messages); un-acked messages redeliver bounded by the poison config — this is the connector's (B5-2) contract, unchanged by this seam.

### BR4 — Publish path accepts concrete priority subjects, no new API
- `validate_publish_subject` accepts concrete `audit.priority.<id>` subjects (same class as the already-tested `ai.queue.summarize`, E10); wildcard `audit.priority.*` remains publish-rejected.
- The relay/connector publishes claims with the existing `publish_idempotent(subject, payload, event_id)` mechanism (event_id as `Nats-Msg-Id`, `live.rs:251` / `session_control.rs:153` pattern). `EventBus` trait: **no changes** (stays dyn-compatible).

### BR5 — Stream isolation: backlog cannot block the priority lane (bus-level anti-starvation)
- `AUDIT_PRIORITY` subjects (`audit.priority.*`) are disjoint from all other declared streams; each stream's consumer has an independent cursor. A backlog (unacked/pending) on `im.room.*` or any other family therefore cannot delay delivery on `audit.priority.*` — no head-of-line blocking across streams. This is the bus-level half of the anti-starvation acceptance (the claim-level cap is DR3).

### BR6 — Doc alignment
- `crates/aero-bus/src/lib.rs` stream list updated to include `AUDIT_PRIORITY` (and the already-missing `LIVE_EVENTS`), so the crate doc matches `bootstrap()`.

### DR1 — `priority` column + `ORDER BY priority DESC` (dependency: B5-1/B5-3 aero-storage)
- The B5-1 governance-outbox DDL (`0239_audit_governance_outbox.sql`) carries a `priority` column (non-null, default = backlog lane; B5-1 owns DDL shape). The governance-outbox claim (B5-3 aero-storage, modeled on `EventOutboxRepo::claim_due` E4) orders **`priority DESC`** with existing tie-breakers preserved (`created_at ASC, id ASC`), so the moderation lane precedes backlog regardless of enqueue/available time. Priority direction is **DESC (higher = more urgent)** per the direction — see R-1.

### DR2 — Moderation claims precede backlog (dependency: B5-3 aero-storage)
- 500 backlog rows + 1 moderation row → the moderation claim is returned first (A3). Moderation rows are stamped `class='admin'` + the shared moderation-lane priority by the aero-ai mapping direction at enqueue time (same constant this spec's A1/A3 compare against).

### DR3 — Anti-starvation cap (dependency: B5-3 aero-storage)
- The claim must include a starvation cap ("cap semantics per B5-3 DDL"): under sustained priority load, backlog rows must still be claimed within a bounded number of batches — no row permanently starved (A4). Bus-level counterpart = BR5.

### DR4 — T-11 fail-closed unchanged (dependency: B5-2/B5-4)
- Relay missing → no grant; the priority seam introduces **no fallback publish path** that reaches the sink without the connector's claim/provision gates (existing gates verified E9: 0236 trigger RAISE aborts the moderation tx when the binding is unavailable; 403→dead at the connector; provisioning gate B5-4). The new stream is transport-only (A5).

## 5. Acceptance checks (preserved from the direction, made testable)

Ownership tags: **[aero-bus]** = built & tested in this module; **[B5-3/storage]**, **[B5-1/storage]**, **[B5-2/B5-4]** = dependency-owned, preserved here verbatim so the drill suite is complete.

### A1 — `priority` in the outbox DDL and `ORDER BY priority DESC` in claim_due **[B5-1/storage] + [B5-3/storage]**
*Preserves: "Add `priority` to the outbox DDL and `ORDER BY priority DESC` in claim_due."*
- **Setup**: throwaway migrated DB (`DATABASE_URL` + `#[ignore]` harness per audit.rs:479), B5-1 governance outbox table; seed rows via the B5-1 enqueue fn.
- **Assert 1 (DDL)**: `information_schema.columns` shows `priority` on the governance outbox, non-null, default = backlog lane (B5-1 owns shape; the assertion lives with B5-1 tests).
- **Assert 2 (claim order, B5-3/storage)**: seed 3 rows with distinct priorities and *staggered `available_at` (backlog rows earlier)*; first `claim_due` (limit ≥ 3) returns rows in `priority DESC` order — the moderation-priority row precedes backlog rows that became available earlier; ties break `created_at ASC, id ASC` (existing pattern, E4).
- **Assert 3 (no IM regression)**: existing `EventOutboxRepo::claim_due` unit/PG behavior unchanged (this direction must not touch the IM `event_outbox` claim — its per-message ordering guards are untouched).

### A2 — `audit.priority.*` mapped in `subscribe()` with WorkQueue retention **[aero-bus]**
*Preserves: "declare a dedicated high-priority subject namespace (e.g. `audit.priority.*`) mapped in `subscribe()` with WorkQueue retention."*
- **Unit (no NATS, jetstream.rs tests module)**: `stream_for_subject("audit.priority.<id>") == Some("AUDIT_PRIORITY")`; the 4 existing prefixes still map; an unknown prefix still yields the mapping failure that `subscribe()` turns into `BusError::Nats("unknown subject prefix")`.
- **Unit**: `audit_priority_stream_config()` → `subjects == ["audit.priority.*"]`, `retention == WorkQueue`, `storage == File`, `duplicate_window == max_age`.
- **Unit**: `poison_safe_pull_config("audit.priority.x", Some("aero-audit-connector"))` → `DeliverPolicy::All`, `max_deliver == POISON_MAX_DELIVER`, `ack_wait == POISON_ACK_WAIT`; `validate_publish_subject("audit.priority.<uuid>")` OK, `"audit.priority.*"` rejected (publish-side).
- **Live-NATS (`#[ignore = "requires a live NATS JetStream at AERO__NATS__URL"]`, pattern: `live_nats_deduplicates_same_message_id`)**: `connect(bootstrap_streams: true)` → `get_stream("AUDIT_PRIORITY")` exists with WorkQueue retention; `publish_idempotent("audit.priority.<nonce>", payload, message_id)` twice → second ack reports duplicate with identical sequence; durable subscribe + ack consumes the message (WorkQueue removes on ack).

### A3 — Claim drill: 500 backlog + 1 moderation → moderation claimed first **[B5-3/storage]**
*Preserves: "test = inject 500 backlog rows + 1 moderation row → claim order asserts moderation first."*
- **Setup**: seed 500 backlog governance rows (message/room class, backlog priority, staggered `available_at`, all due) + 1 moderation row (class `admin`, moderation-lane priority, produced through the aero-ai mapping seam — same constant A1 asserts against).
- **Assert**: (1) `claim_due(now, lease, limit=1)` returns the moderation row; (2) with `limit=MAX_CLAIM`, the moderation row is the first element of the returned batch; (3) per-aggregate/ordering guards (NOT EXISTS shape, E4) still hold — no row skipped.
- **G6 mapping**: this is `docs/campaigns/implementation-gate.md` row 3's "注入积压 drill：moderation 先达 sink".

### A4 — Anti-starvation cap: priority load never blocks backlog **[B5-3/storage] + [aero-bus]**
*Preserves: "anti-starvation cap test (priority stream never blocks backlog under sustained priority load)."*
- **PG ([B5-3/storage])**: under sustained priority load (interleave new moderation-priority rows between claim batches), every seeded backlog row is claimed ≥ once within a bounded number of batches (K); assert via claim-history over the seeded ids — no permanent starvation; cap semantics per B5-3 DDL.
- **Bus ([aero-bus], live-NATS)**: backlog load on `im.room.*` (consumer left pending) does not delay `audit.priority.*` delivery — priority consumer receives while the backlog consumer's pending > 0 (independent streams/cursors, BR5).

### A5 — T-11 fail-closed unchanged: relay missing → no grant **[B5-2/B5-4] + [aero-bus]**
*Preserves: "T-11 fail-closed behavior unchanged (relay missing → no grant)."*
- **Assert**: with no relay provisioning, a moderation claim does not reach the sink and no grant is issued — behavior identical pre/post this seam (existing gates E9: 0236 trigger RAISE on missing binding aborts the moderation tx; connector 403→dead; B5-4 boot gate).
- **[aero-bus] constraint test**: the new stream is transport-only — assert no code path in aero-bus (or added by this direction) publishes audit events to a subject the sink/connector could consume without the connector's claim/provision gate; the only consumers of `audit.priority.*` are connector durables.

## 6. Test placement

| Test | Location | Harness |
|---|---|---|
| BR1/BR4 mapping + publish-validation unit tests | `crates/aero-bus/src/jetstream.rs` tests module (extend `accepts_well_formed_concrete_subjects`, add `stream_for_subject` cases) | pure unit, no NATS — pattern: `pull_config_bounds_redelivery_for_poison_messages` |
| BR2 config-builder unit tests (WorkQueue retention, subjects, duplicate_window=max_age) | `crates/aero-bus/src/jetstream.rs` tests module | pure unit — pattern: `im_stream_duplicate_window_covers_outbox_retries` |
| BR3 poison-safe config for the new subject/durable | `crates/aero-bus/src/jetstream.rs` tests module | pure unit |
| A2 live stream declaration + idempotent publish + ack/WorkQueue removal | `crates/aero-bus/src/jetstream.rs` tests module | `#[ignore]` live NATS at `AERO__NATS__URL` — pattern: `live_nats_deduplicates_same_message_id` |
| A4 bus-level isolation (backlog on `im.room.*` ≠ delay on `audit.priority.*`) | `crates/aero-bus/src/jetstream.rs` tests module | `#[ignore]` live NATS |
| A1/A3/A4-PG (DDL column, `ORDER BY priority DESC`, 500+1 drill, anti-starvation cap) | aero-storage db_tests next to the B5-3 governance claim tests (`audit_governance.rs`, per sibling spec `2026-08-06-aero-ai-moderation-governance-outbox.req.md` §6) | PG, `#[ignore]` + `DATABASE_URL` + throwaway DB |
| A5 T-11 (403→dead, no grant) | connector integration (B5-2) + `scripts/test-integration.sh` drill | integration; G6 gate |

## 7. Risks / [PROPOSED] items

- **R-1 — Priority-direction convention conflict (must resolve before B5-3 implements)**: the direction pins `ORDER BY priority DESC` (higher = more urgent); the only in-repo priority-lane precedent (`ai_jobs`, 0130 / `priority_for`) uses **ASC, lower = more urgent** (E8). All audit-outbox code (B5-1 DDL default, B5-3 claim, aero-ai mapping stamp) must share **one** convention; this spec pins DESC per the direction's acceptance. Mixed conventions would silently invert the moderation lane.
- **R-2 — Exact outbound tokens (`admin.content.flag` vs `admin.moderation.action`) and the moderation-lane priority value**: out-of-repo contract text (E7). This spec pins the *seam* (namespace, ordering direction, isolation); A1/A3 compare against the shared constant resolved by the B5-3 mapping table — tests are self-consistent whichever token/lane value the contract dictates (same stance as sibling spec §7).
- **R-3 — `max_age`/`duplicate_window` of `AUDIT_PRIORITY` vs B5-2 horizon**: pinned at 24h (= AI_QUEUE precedent) with `duplicate_window = max_age` (= IM_MESSAGES precedent); if B5-2's lease/backoff horizon exceeds 24h, the constants must be raised together — coordinated constant, flagged not invented independently.
- **R-4 — WorkQueue semantics**: acked messages are removed; a dead connector leaves transport copies pending up to `POISON_MAX_DELIVER` redeliveries then parked — **no durable loss**: the PG governance outbox is the source of truth and the relay requeues (existing lease/backoff, E9). The seam must not be mistaken for a durable archive.
- **R-5 — Existing IM `event_outbox` untouched**: priority ordering applies to the B5-1 governance outbox claim only; changing the IM claim order is out of scope and would alter IM delivery scheduling — do not "helpfully" extend this direction there.
- **R-6 — Evidence corrections**: `soft_delete_moderated` lives at `message/crud.rs:398` (not audit.rs — audit.rs holds the test at :808); `crates/aero-bus/src/lib.rs` stream doc is already stale (missing `LIVE_EVENTS`) — BR6 fixes it in passing. 2026-08-08 refresh: the direction's "grep hits only drill comments" is actually **0 hits** (E1); anchors refreshed E1 :338/:344–353, E2 :145, E3 :237; the design doc §8 A1 row's "default 0" predates the 0239 handoff-H1 reconciliation — the landed DDL default is **10** = `GOVERNANCE_PRIORITY_BACKLOG`, and the landed claim already orders `priority DESC` (`pg.rs:117`).

## 8. Sequencing

1. **This direction (aero-bus)**: BR1–BR6 — pure additive seam (new stream + mapping + constant + docs); no migration, no trait change; lands independently and unit/live-NATS-testable today.
2. **B5-1 (aero-storage)**: 0239 DDL (priority column per DR1) + `audit_governance.rs` — unlocks A1-assert 1.
3. **B5-3 (aero-storage)**: governance claim `ORDER BY priority DESC` + anti-starvation cap — unlocks A1-assert 2/3, A3, A4-PG.
4. **B5-2 / B5-4**: connector subscribes to `audit.priority.*` (using the BR1 constant), 403→dead, provisioning — unlocks A5; G6 gate green when A2–A5 + T-11 + 37/37 all pass.

# Design — Priority-aware delivery seam in aero-bus for moderation events (B5-3)

- **Module**: `crates/aero-bus` (seam) + dependency contracts for B5-1/B5-3 storage, B5-2 connector
- **Source**: `docs/requirements/2026-08-07-aero-bus-b5-3-priority-delivery-seam.req.md` (BR1–BR6 / DR1–DR4 / A1–A5)
- **Campaign**: `aero-im-b5-outbox-relay`; gate **G6 (B5)** = "37/37、T-11、moderation 优先级" (`docs/campaigns/implementation-gate.md:78`)

## 0. Evidence verification (untrusted claims re-checked against repo, 2026-08-07)

| Claim | Verdict | Evidence |
|---|---|---|
| `jetstream.rs` `subscribe()` inline 4-prefix map + `"unknown subject prefix"` error | ✅ | `crates/aero-bus/src/jetstream.rs` ~L343–353: `starts_with` chain over `im.room.`/`im.events.`/`ai.queue.`/`live.stream.`, else `Err(BusError::Nats(format!("unknown subject prefix: {subject}")))` |
| `poison_safe_pull_config` extracted + unit-tested (16 / 120s / All-vs-New) | ✅ | jetstream.rs ~L96; `POISON_MAX_DELIVER=16`, `POISON_ACK_WAIT=120s`, `DeliverPolicy::All` for durable non-`aero-server-*`, `New` for ephemeral/process-local; tests `pull_config_bounds_redelivery_for_poison_messages` etc. |
| `bootstrap()` AI_QUEUE **WorkQueue** precedent | ✅ | jetstream.rs `bootstrap()`: `AI_QUEUE`, subjects `ai.queue.*`, `RetentionPolicy::WorkQueue`, `max_age` 24h, File, `get_or_create_stream` |
| `validate_publish_subject` + `publish_idempotent` Nats-Msg-Id dedup | ✅ | jetstream.rs; `NATS_MESSAGE_ID` header via `idempotent_publish_headers`; `IM_MESSAGE_DUPLICATE_WINDOW` 7d = max_age; test `im_stream_duplicate_window_covers_outbox_retries` |
| `event_outbox.rs` claim `ORDER BY available_at ASC, created_at ASC, id ASC`, no `priority` in COLUMNS | ✅ | `crates/aero-storage/src/event_outbox.rs:330` (query at 257/276); `COLUMNS` at :21 (spec said :17 — trivial line drift, same symbol) |
| `moderation_bot.rs`: `try_enqueue`→`SkipReason::QueueFull`, bounded mpsc 512, durable `aero-moderation` | ✅ | `crates/aero-server/src/moderation_bot.rs`: `try_enqueue` :168, `QueueFull` :170, `queue_capacity` default 512 (:91, asserted :500), durable `Some("aero-moderation")` :313 |
| ⚠️ Correction: `soft_delete_moderated` in `audit.rs` | ⚠️→✅ | Lives in `crates/aero-storage/src/message/crud.rs:398`; `audit.rs` holds `append_in_tx` (:127) + db_test `moderation_delete_and_audit_commit_together` (:808). Correction confirmed correct. |
| Proposal B5-3 (`audit-contract-batch-aero-im.md:10`) + G6 gate (`implementation-gate.md:78`) | ✅ | Both match verbatim: "`priority` 列 + `claim_due` 按 `priority DESC` 排序…注入积压 drill（500 积压 + 1 moderation → 先达 sink）+ 反饥饿上限"; G6 row "B5-1..4 → 37/37、T-11、moderation 优先级" |
| 0130 ai_jobs priority-lane precedent, **ASC lower-first** | ✅ | `migrations/0130_ai_job_priority.sql`: `priority SMALLINT NOT NULL DEFAULT 100`, partial index `(priority, scheduled_at) WHERE status='queued'`; `ai_job.rs:62 priority_for` Moderate=10 < Answer=20 < Summarize=50 < Embed=100; claim `ORDER BY priority ASC` — **convention conflict R-1 is real** |
| Sibling ownership: claim ordering → B5-3 (aero-storage) | ✅ | `docs/requirements/2026-08-06-aero-ai-moderation-governance-outbox.req.md` §8 assigns "priority column semantics + claim_due ORDER BY priority DESC + anti-starvation cap → B5-3 (aero-storage)"; connector crate `aero-audit-connector` exists, binds B5-1 outbox via `outbox::OutboxRepo` trait, currently has **no** aero-bus dependency |
| Snaplink v1 outbox: no priority; claim `ORDER BY available_at, created_at, delivery_id` | ✅ | `migrations/0235…sql:161`; `snaplink_commercial.rs:302,320` |
| `crates/aero-bus/src/lib.rs` stream doc stale | ✅ | lib.rs lists 3 streams, missing `LIVE_EVENTS` (BR6 fix confirmed needed) |
| Migrations at 238, latest `0238` → next `0239` | ✅ | `ls migrations/*.sql | wc -l` = 238; `0239_audit_governance_outbox.sql` numbering consistent |

All claims verified; zero fabrication found. Design below is grounded on the verified state.

## 1. Design overview

```
moderation verdict (AiWorker / moderation_bot)
  └─ one PG tx: soft_delete_moderated (message/crud.rs:398) + audit append_in_tx 'message.moderated'
       └─ 0236 trigger → governance outbox (B5-1 0239: + priority SMALLINT, class, delivery_mode)
            └─ claim (B5-3): ORDER BY priority DESC + anti-starvation cap      ← claim-level priority
                 └─ relay → publish_idempotent("audit.priority.<id>", payload, event_id)
                      └─ AUDIT_PRIORITY stream (WorkQueue, 24h)                ← bus-level seam [THIS DESIGN]
                           └─ subscribe("audit.priority.*", "aero-audit-connector") → B5-2 connector
                                └─ claim/provision gates (T-11) → HTTP sink
```

Two independent anti-starvation halves:
- **Claim-level (B5-3/storage)**: moderation rows (`priority` = high lane) claimed before backlog regardless of `available_at`; a reserved-backlog floor keeps backlog moving under sustained priority load.
- **Bus-level (aero-bus, this design)**: `audit.priority.*` is a **disjoint stream** from `im.room.*`/`im.events.*`/`ai.queue.*`/`live.stream.*` with an independent consumer cursor — backlog pending on any other stream cannot head-of-line-block priority delivery.

## 2. API changes (`crates/aero-bus`)

No `EventBus` trait change (stays dyn-compatible), no new consumer-config path, no DB migration. Four additive symbols in `crates/aero-bus/src/jetstream.rs` + one doc fix.

### 2.1 New public constant (single literal, no second copy)

```rust
/// Normative high-priority subject namespace for audit-governance claims (B5-3).
/// Single literal shared by the `subscribe()` mapping and the B5-2 connector's
/// subscription — any other copy is a bug.
pub const AUDIT_PRIORITY_SUBJECT_PREFIX: &str = "audit.priority.";
```

Re-exported from `lib.rs` for the connector crate:
```rust
pub use jetstream::{AUDIT_PRIORITY_SUBJECT_PREFIX, JetStreamBus, JetStreamConfig};
```

### 2.2 Extracted pure mapping fn (replaces the inline chain)

```rust
fn stream_for_subject(subject: &str) -> Option<&'static str> {
    if subject.starts_with("im.room.") {
        Some("IM_MESSAGES")
    } else if subject.starts_with("im.events.") {
        Some("IM_EVENTS")
    } else if subject.starts_with("ai.queue.") {
        Some("AI_QUEUE")
    } else if subject.starts_with("live.stream.") {
        Some("LIVE_EVENTS")
    } else if subject.starts_with(AUDIT_PRIORITY_SUBJECT_PREFIX) {
        Some("AUDIT_PRIORITY")
    } else {
        None
    }
}
```

`subscribe()` becomes:
```rust
let stream_name = stream_for_subject(subject)
    .ok_or_else(|| BusError::Nats(format!("unknown subject prefix: {subject}")))?;
```
Exact error string preserved → unknown prefixes stay fail-closed.

### 2.3 Pure stream-config builder (mirrors `im_messages_stream_config()`)

```rust
const AUDIT_PRIORITY_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

fn audit_priority_stream_config() -> stream::Config {
    stream::Config {
        name: "AUDIT_PRIORITY".into(),
        subjects: vec![format!("{AUDIT_PRIORITY_SUBJECT_PREFIX}*")],
        retention: stream::RetentionPolicy::WorkQueue,   // AI_QUEUE precedent
        max_age: AUDIT_PRIORITY_MAX_AGE,                 // 24h, AI_QUEUE precedent
        duplicate_window: AUDIT_PRIORITY_MAX_AGE,        // IM_MESSAGES precedent: dedup == retained horizon
        storage: stream::StorageType::File,
        ..Default::default()
    }
}
```

`bootstrap()` gains one idempotent `get_or_create_stream(audit_priority_stream_config())` + `info!(stream = "AUDIT_PRIORITY", "declared")`. New stream ⇒ plain `get_or_create` is sufficient (the `update_stream` on `IM_MESSAGES` exists only to retrofit a changed config to existing clusters).

### 2.4 Unchanged surfaces (deliberately)

- `poison_safe_pull_config` — already generic over subject/durable; connector durable `aero-audit-connector` gets `max_deliver=16 / ack_wait=120s / DeliverPolicy::All` automatically.
- `validate_publish_subject` — accepts concrete `audit.priority.<id>` (same class as tested `ai.queue.summarize`), rejects `audit.priority.*` on publish.
- `publish_idempotent(subject, payload, event_id)` — relay publishes claims with `event_id` as `Nats-Msg-Id` (existing `live.rs:251` / `session_control.rs:153` pattern).
- `crates/aero-bus/src/lib.rs` doc: add `AUDIT_PRIORITY` row **and** the already-missing `LIVE_EVENTS` row (BR6).

### 2.5 Dependency contract for B5-2 connector (not built here)

`crates/aero-audit-connector/Cargo.toml` gains `aero-bus.workspace = true` (layering safe: aero-bus depends only on `aero-common` + NATS; no cycle). The connector subscribes:

```rust
bus.subscribe(aero_bus::AUDIT_PRIORITY_SUBJECT_PREFIX, Some("aero-audit-connector"))
```
(durable → `DeliverPolicy::All` on first creation → replays retained unacked claims on restart; filter subject `audit.priority.*` is a legal subscribe-side wildcard).

## 3. Compatibility constraints

| # | Constraint | Consequence if violated |
|---|---|---|
| C1 | `EventBus` trait untouched; `publish_idempotent` keeps its default fallback | Trait no longer dyn-compatible / `FakeBus` in traits tests breaks |
| C2 | Existing 4 prefixes map identically; unknown prefix error string unchanged | Silent misrouting; existing tests (`accepts_well_formed_concrete_subjects`, subscribe callers) break |
| C3 | IM `event_outbox` table/claim untouched (R-5) | IM per-aggregate-version / delivery-ordinal ordering guards violated — do NOT "helpfully" extend the priority column there |
| C4 | `ai_jobs` (0130) untouched — ASC convention stays | See §4 R-1 resolution; retrofitting ai_jobs to DESC churns a working lane for zero gain |
| C5 | `moderation_bot.rs` FIFO mpsc untouched | Third hop is out of scope; the seam guarantees priority **between** bus and sink, not inside the in-process queue |
| C6 | Constants coordinated with B5-2: `max_age` 24h = `duplicate_window`; if B5-2's lease/backoff cap (300s) ever exceeds the horizon, raise both together (R-3) | Broker dedup window shorter than retry horizon → duplicates reach connector; WorkQueue removes acked msgs so this is only a duplicate-spike risk, still deduped by connector's durable receipts |
| C7 | `aero-bus` adds **no migration**; B5-1 owns `0239_audit_governance_outbox.sql` (238 migrations today, next = 0239) | Numbering collision / out-of-order migrate |
| C8 | Connector crate adds the `aero-bus` dependency in its own `Cargo.toml` (AGENTS §4.1: 依赖只加自身 crate) | Workspace layering violation |
| C9 | Stream name `AUDIT_PRIORITY` ≤ 32 chars, no collision with existing 4 | Broker rejects config on existing clusters |

## 4. Priority convention (R-1) — resolution, pinned

**Decision: keep both conventions, each self-consistent, with a test-enforced boundary.**

- `ai_jobs` (0130): **ASC, lower = more urgent** (Moderate=10 < Answer=20 < … < Embed=100). Untouched.
- Audit governance outbox (B5-1/3): **DESC, higher = more urgent**, per the direction's pinned acceptance. Concrete values (B5-1 DDL contract, asserted by A1; reconciled from the draft's "DEFAULT 0" — handoff H1, the landed 0239 carries `DEFAULT 10` = `GOVERNANCE_PRIORITY_BACKLOG`):
  - `priority SMALLINT NOT NULL DEFAULT 10` — backlog lane (message/room class).
  - Moderation lane (`class='admin'`) stamped `100` at enqueue by the aero-ai mapping direction, via a **shared constant** (e.g. `pub const MODERATION_LANE_PRIORITY: i16 = 100` in the B5-3 storage module; the requirement-spec A1/A3 tests compare against it).

Why not unify: the direction's drill (A3) and G6 acceptance are written as "moderation first"; retrofitting ai_jobs would invert a shipped, tested feature. The divergence is **guarded, not documented away**: the A3 drill and a dedicated unit test (`priority_lanes_order_user_facing_work_ahead_of_backfill`'s audit-outbox sibling) assert the *actual* claim order, so an implementation that copies the ai_jobs pattern fails red, immediately.

## 5. Claim semantics (B5-3/storage dependency contract, specified for testability)

`claim_due(now, lease, limit)` on the governance outbox (modeled on `EventOutboxRepo::claim_due` shape: CTE + `FOR UPDATE SKIP LOCKED` + lease stamping):

```
K = limit / 5                        -- reserved backlog floor (integer division)
tier1 = rows ORDER BY priority DESC, created_at ASC, id ASC   LIMIT limit - K
tier2 = remaining due rows           ORDER BY created_at ASC, id ASC   LIMIT K
result = tier1 ++ tier2
```

Properties (each maps to an acceptance):
- `limit = 1` → `K = 0` → the single slot is the highest-priority row → **A3 assert 1**.
- `limit = MAX_CLAIM` → tier1 ordered `priority DESC` → moderation row is the first element → **A3 assert 2**.
- Every batch with `limit ≥ 5` claims ≥ `K ≥ 1` backlog rows when any exist → a backlog row is claimed within `⌈N/K⌉ + 1` batches → **A4-PG** (bounded starvation).
- Tie-breakers `created_at ASC, id ASC` preserve FIFO within a lane (existing pattern, E4).

The SQL shape is B5-3's to implement (e.g. `ROW_NUMBER() OVER (PARTITION BY lane ORDER BY …)` with per-lane `rn` caps); the contract above is what the drill asserts.

## 6. Failure modes & mitigations

| # | Failure | Behavior | Mitigation / invariant |
|---|---|---|---|
| F1 | NATS down at boot | `connect()` fails loudly; no partial state | Existing behavior; unchanged |
| F2 | Connector subscribes before stream declared (mixed-version process) | Impossible within one process (aero-bus is a library); across a rolling deploy, the node running the connector runs the new aero-bus | Bootstrap ordering: `connect()` runs `bootstrap()` before any `subscribe()` |
| F3 | Subject typo (e.g. `audit.priorityx.*`) | `stream_for_subject` → `None` → `BusError::Nats("unknown subject prefix: …")` fail-closed | C2; unit-tested |
| F4 | Connector crashes before ack | WorkQueue redelivers, bounded by poison config: ≤16 attempts, ≥120s apart, then parked | **No durable loss**: PG governance outbox is source of truth; relay requeues on lease expiry (existing lease/backoff, 300s cap) — R-4 |
| F5 | Connector dies permanently | Pending messages park after 16×120s; stream retains unacked msgs up to `max_age` 24h | Dead-connector safety valve only; PG outbox holds truth; acked messages are gone by design (WorkQueue ≠ archive) |
| F6 | Relay retry after original acked+removed | Original removed by WorkQueue ⇒ retry is a fresh publish, not a duplicate | At-least-once; connector's durable `(consumer,event_id)`-style receipts dedup |
| F7 | Retry within dedup window | `Nats-Msg-Id` collapses it at broker (same sequence) | `duplicate_window = max_age` (24h) |
| F8 | Convention inversion (B5-3 implements ASC by copying ai_jobs) | Moderation = lowest lane; drill fails red | A3 drill + shared constant + §4 guard |
| F9 | Backlog starvation at claim level | — | Reserved-backlog floor K per batch (§5) |
| F10 | Backlog HOL-block at bus level | — | Disjoint stream + independent cursor (BR5); A4 bus test |
| F11 | Sink rejects (403 T-11) | Connector 403→dead; **no grant** | Seam is transport-only — it adds no publish path that bypasses connector claim/provision gates (A5); 0236 trigger RAISE still aborts the moderation tx when binding unavailable |
| F12 | Intra-family priority confusion | All `audit.priority.*` subjects share ONE stream; priority is enforced at claim level, not by NATS | Documented in stream doc comment — do not "helpfully" create per-priority streams |

## 7. Migration steps

**aero-bus lands no migration** (pure additive code; `cargo build` + unit tests suffice). The full-batch sequence:

| Phase | Who | Step | Unlocks |
|---|---|---|---|
| 1 | aero-bus (this design) | BR1–BR6 code + unit tests + live-NATS tests | A2, A4-bus, A5-bus constraint |
| 2 | B5-1 (aero-storage) | `migrations/0239_audit_governance_outbox.sql`: `priority SMALLINT NOT NULL DEFAULT 10` (H1 reconcile: the draft's DEFAULT 0 landed as 10 = `GOVERNANCE_PRIORITY_BACKLOG`) + due index + `class`/`delivery_mode` per proposal | A1-assert 1 |
| 3 | B5-3 (aero-storage) | Claim `ORDER BY priority DESC` + K-floor cap (§5); db_tests with throwaway DB | A1-assert 2/3, A3, A4-PG |
| 4 | B5-2/B5-4 | Connector: `aero-bus` dep + subscribe via `AUDIT_PRIORITY_SUBJECT_PREFIX`; 403→dead; provisioning gate | A5 |
| 5 | Gate G6 | Full drill suite + T-11 + 37/37 manifest | G6 green |

**Hard rule (§4.2)**: after adding `0239…sql`, run `cargo build` **before** `aero-cli migrate` (migrations are compile-time embedded via `sqlx::migrate!("../../migrations")`); otherwise the new migration silently no-ops. All PG drills run on a throwaway `CREATE DATABASE`/`DROP DATABASE` cycle, never the shared dev DB.

## 8. Testable acceptance mapping

| Acceptance | Assertion (testable form) | Location / harness | Gate |
|---|---|---|---|
| **A1** DDL `priority` + `ORDER BY priority DESC` | (1) `information_schema.columns` shows `priority` non-null default 0 on governance outbox [B5-1]; (2) 3 rows, distinct priorities, backlog `available_at` earlier → first `claim_due(limit≥3)` returns `priority DESC`; ties `created_at, id` ASC [B5-3]; (3) IM `EventOutboxRepo::claim_due` behavior unchanged [regression] | aero-storage db_tests (`audit_governance.rs`), `#[ignore]` + `DATABASE_URL` + throwaway DB | G6 |
| **A2** `audit.priority.*` mapped + WorkQueue | Unit: `stream_for_subject("audit.priority.x") == Some("AUDIT_PRIORITY")`; 4 old prefixes unchanged; unknown → None→error. Unit: `audit_priority_stream_config()` → subjects `["audit.priority.*"]`, WorkQueue, File, `duplicate_window == max_age`. Unit: `poison_safe_pull_config("audit.priority.x", Some("aero-audit-connector"))` → All/16/120s; `validate_publish_subject("audit.priority.<uuid>")` OK, `"audit.priority.*"` rejected. Live-NATS (`#[ignore]`, `AERO__NATS__URL`): stream exists after bootstrap; double `publish_idempotent` → 2nd ack `duplicate` + same sequence; durable sub + ack consumes (WorkQueue removal) | `crates/aero-bus/src/jetstream.rs` tests module; live test patterned on `live_nats_deduplicates_same_message_id` | G6 |
| **A3** 500 backlog + 1 moderation → moderation first | Seed 500 backlog rows (staggered `available_at`, all due) + 1 `class='admin'` row at `MODERATION_LANE_PRIORITY` via the mapping seam; assert (1) `claim_due(limit=1)` returns the moderation row; (2) with `limit=MAX_CLAIM` it is the first element; (3) NOT EXISTS per-aggregate guards hold, no row skipped | aero-storage db_tests (`audit_governance.rs`) | G6 — this IS the "注入积压 drill" row 3 |
| **A4** anti-starvation | PG [B5-3]: interleave new priority rows between batches; every seeded backlog row claimed ≥ once within `⌈N/K⌉+1` batches (claim-history over seeded ids). Bus [aero-bus, live-NATS]: durable consumer on `im.room.x` left pending; publish `im.room.x` backlog + `audit.priority.y`; priority message received while `consumer_pending("IM_MESSAGES", …) > 0` | aero-storage db_tests + jetstream.rs live-NATS tests | G6 |
| **A5** T-11 fail-closed unchanged | No relay provisioning → claim never reaches sink, no grant — identical pre/post seam (0236 trigger RAISE; connector 403→dead; B5-4 boot gate). Bus constraint: no aero-bus code path publishes audit events to a subject consumable without connector gates; only connector durables consume `audit.priority.*` | connector integration + `scripts/test-integration.sh`; code-review assertion on aero-bus | G6 + T-11 |

## 9. Out-of-scope (boundary reminders)

- Local `message.moderated` → outbound `admin.content.flag`/`admin.moderation.action` mapping table: aero-ai direction (R-2 — tokens [PROPOSED], out-of-repo contract; tests self-consistent via the shared constant).
- `moderation_bot.rs` FIFO queue, `ai_jobs` claim, per-subject seq (`seq.rs`), sink-side contract.
- The 0239 DDL and governance claim SQL: B5-1/B5-3 (this doc specifies their *contract* so acceptance stays testable here).

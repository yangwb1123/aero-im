# Assessment — B5-1 producer-seam hot-path cost: round trips, bindings, outbox growth, claim fairness

- **Design under review**: `2026-08-08-aero-im-core-b5-1-in-tx-audit-outbox-producer-seam.design.md`
- **Scope**: the 4–5 in-tx round trips + durable rows added to the IM platform's highest-frequency writes (`message.send` / `message.edited` / `message.deleted`, plus `room.created` / `room.member.add`)
- **Method**: source reading + live measurement against a PG 17 container (partitioned `audit_events` replayed from migrations 0007/0146; 0239-shaped outbox table)
- **Status**: assessment — design amendments proposed; no code changed

---

## 0. Verdict summary

| # | Question | Verdict | Mitigation |
|---|---|---|---|
| 1 | `occurred_at` from audit row / `clock_timestamp()` vs subquery | **Subquery is the worst option of the three — it is a cross-partition scan, not a PK lookup** (`audit_events` is partitioned, PK = `(id, created_at)`, id-alone predicate = Append over all ~365 daily partitions; measured 10× slower than a pruned probe). `clock_timestamp()` is contract-wrong. **Take it from the audit INSERT itself via `RETURNING`** | §1 |
| 2 | Bindings lookup cached/folded | **Fold, don't cache.** The lookup is a PK probe on a tiny table (~30 µs); caching buys nothing, risks v1/v2 divergence + absence-memoization. The send seam's extra `SELECT workspace_id FROM rooms` is fully redundant — capture `LockedRoomWriteAccess.workspace` (design's own finding #1, currently contradicted by §1.4) | §2 |
| 3 | Outbox growth / index coverage / retention | Index coverage is **correct** (0240 partial index matches the claim ORDER BY exactly; pending-scan cost ∝ backlog, not table size). **Retention is the gap**: neither outbox ever deletes a row; the seam makes both message-rate append-only logs (measured ~1.4 kB/row v2 + ~0.5 kB audit + ~1.2 kB v1 when enforcement ON) | §3 |
| 4 | Priority-10 fairness + tx duration | **"不扰动 moderation 优先" holds structurally** (priority DESC first, pinned by the mixed-priority claim test). The real effect is **volume**: relay defaults ≈ 20 rows/s capacity vs message-rate arrival. Tx duration +2 statements ≈ +20 % — **no interaction with the 60s AI cost budgets** (they meter AI spend, not DB writes); lock contention with sweep timers is negligible (seam is append-only) | §4 |

**Net**: seam round trips per op drop from 4–5 to **2** (audit INSERT with `RETURNING` + envelope INSERT with folded `source_system`), the 365-partition scan disappears, and one unbounded-growth decision (delivered-row retention) is the only open item.

---

## 1. `occurred_at` — take it from the audit INSERT, never from a subquery, never from `clock_timestamp()`

### 1.1 The subquery is a cross-partition scan (measured)

`audit_events` was converted to daily RANGE partitions in migration 0146; its PK became `(id, created_at)` because **every UNIQUE/PK on a partitioned table must contain the partition key** (0146 comment). 0154 adds legal-hold partitions under the same key. Consequently there is **no index on `id` alone** — the design's `SELECT to_jsonb(created_at) #>> '{}' FROM audit_events WHERE id=$1` cannot prune partitions and executes an `Append` over every partition:

```
EXPLAIN (ANALYZE) SELECT to_jsonb(created_at) #>> '{}' FROM audit_events WHERE id = '...';
 Append (cost=4.16..304.79 rows=64 width=32) (actual time=0.049..0.053 rows=1 loops=1)
   -> Bitmap Heap Scan on audit_events_20260709 ...   -- ×32 partitions (production: ~365)
   -> Bitmap Heap Scan on audit_events_20260710 ...
   ...
 Planning Time: 4.624 ms      Execution Time: 0.361 ms      (32 empty in-cache partitions)

-- same lookup with the partition key bound:
 Index Only Scan using audit_events_20260808_pkey ... Execution Time: 0.038 ms
```

On 32 empty, in-cache partitions the unpruned form is already ~10× slower; on a production table with 365 partitions, real data, and cold cache it is the single most expensive statement the seam adds — worse than the round-trip count suggests. And it grows with the retention window (`keep_days`).

### 1.2 `clock_timestamp()` is contract-wrong

- The envelope contract pins `occurred_at` == the audit row's `created_at`: the 0239/0236 triggers build `jsonb_build_object('occurred_at', NEW.created_at, ...)` and half-B (`rust_produced_payload_matches_0239_envelope`) asserts the spelling `to_jsonb(created_at) #>> '{}'`.
- `audit_events.created_at` is **app-bound**, not a DB default: `AuditRepo::append_on` binds `time::OffsetDateTime::now_utc()` (audit.rs). `clock_timestamp()` advances *within* a transaction, so in a send tx that waits on a lock, the two diverge by the wait time. The v1/v2 rows for the same event would disagree.
- Even the DB-side `now()` (tx start) differs from `clock_timestamp()` (statement time) in a long tx. Use the stored value.

### 1.3 Fix — `RETURNING` the PG-rendered string, zero caller churn

The audit INSERT is already a round trip and already pays for the row; ask it for the rendered value:

```rust
// audit.rs — shared INSERT gains RETURNING; append/append_in_tx keep their
// public signatures (callers unchanged), the seam gets the string.
async fn append_on<'e, E>(executor: E, ...) -> Result<(AuditId, String), sqlx::Error> {
    let id = AuditId::new();
    let created_at = time::OffsetDateTime::now_utc();
    let row = sqlx::query_as::<_, (uuid::Uuid, String)>(
        r"INSERT INTO audit_events (id, workspace_id, actor_id, action, target, detail, created_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7)
           RETURNING id, to_jsonb(created_at) #>> '{}'",   // PG renders — "never Rust formatter" pin kept literally
    )
    .bind(...).fetch_one(executor).await?;
    Ok((AuditId::from_uuid(row.0), row.1))
}

pub async fn append_in_tx(tx, ...) -> Result<AuditId, sqlx::Error> {
    Ok(Self::append_on(&mut **tx, ...).await?.0)  // existing callers untouched
}
/// New: the seam's variant (audit row + PG-spelled occurred_at in one round trip).
pub async fn append_in_tx_with_occurred_at(tx, ...) -> Result<(AuditId, String), sqlx::Error> {
    Self::append_on(&mut **tx, ...).await
}
```

- 17 production call sites of `append_in_tx`/`append` keep their signatures; only the seam uses the new variant.
- `occurred_at` then flows into the payload exactly as half-B does today (`AuditClaimPayload::new(..., occurred_at, ...)`), and jsonb stores text values verbatim — spelling is preserved through the round trip.
- Verified: `to_jsonb(timestamptz)` (what `jsonb_build_object` uses) and `to_jsonb(created_at) #>> '{}'` produce byte-identical text (`2026-08-08T22:33:14.799107+00:00`), so `RETURNING … #>> '{}'` is exactly the trigger spelling.

**Net**: removes 1 round trip **and** the 365-partition scan per op.

---

## 2. Bindings lookup — fold into the outbox INSERT; capture the room's workspace instead of re-SELECTing

### 2.1 The send seam's `SELECT workspace_id FROM rooms` is redundant

`lock_effective_sender_room_access` (idempotency.rs:14) already resolves and **discards** `LockedRoomWriteAccess { workspace }` (the room row is `FOR SHARE`-locked in-tx — no race). The design's own finding #1 says "both edit and send call sites discard it — the edit seam captures it for free", yet §1.4's `message.send` row still plans `SELECT workspace_id FROM rooms WHERE id=$1`. Make both paths capture:

```rust
// idempotency.rs insert_outboxed — 3-line refactor, drops one round trip
let access = lock_effective_sender_room_access(&mut tx, room, sender).await?;
let Some(access) = access else { ... };   // was: if !... { ... }
let workspace = access.workspace;          // feeds the seam
```

### 2.2 Bindings: fold, don't cache

- `snaplink_commercial_bindings.workspace_id` is the **PK** → the lookup is a single-tuple probe (~30 µs) on a table with O(workspaces) rows. There is no scan to save.
- Fold it into the outbox INSERT as a `COALESCE` scalar subquery — zero extra round trips, zero staleness, fallback `'aero-im'` preserved (verified working against an 0239-shaped table):

```sql
INSERT INTO audit_governance_outbox (event_id, class, priority, payload)
VALUES ($1, $2, $3,
        jsonb_set($4::jsonb, '{source_system}',
                  to_jsonb(COALESCE((SELECT b.source_system
                                       FROM snaplink_commercial_bindings b
                                      WHERE b.workspace_id = $5),
                                    'aero-im'))))
ON CONFLICT (event_id) DO NOTHING
```

- **Why not an in-process cache**: `source_system` is immutable by design (0235 revision-guard: identity columns cannot change, only `enabled` flips) so a cache would be *mostly* safe — but (a) the no-binding fallback requires memoizing *absence* too; (b) multi-instance drift on binding delete/re-create; (c) the SQL trigger path cannot share a Rust cache, so v1 (0236) and v2 rows for the *same* event could diverge in a window; (d) the marginal saving is ~30 µs, exactly what the fold captures with zero risk.

**Net**: the seam's own round trips go 4–5 → **2**:

| op | design today | after mitigation |
|---|---|---|
| send | audit INSERT + occurred_at SELECT + source_system SELECT + rooms SELECT + outbox INSERT (5) | audit INSERT (RETURNING) + envelope INSERT (2) |
| edit | 4 | 2 |
| delete | 3 (audit INSERT already exists today) | 2 |
| room.created / member.add | 4 / 3 (workspace already in hand) | 2 |

The v1 row (when enforcement ON) is produced **inside** the audit INSERT statement by the 0236 trigger (plpgsql, no round trip) — its binding lookup and envelope build remain, but that is pre-existing per-audit-row cost.

---

## 3. Outbox growth — index coverage is correct; retention is the structural gap

### 3.1 Row cost (measured on an 0239-shaped table, realistic 16-key envelope)

| table | per-row cost | note |
|---|---|---|
| `audit_governance_outbox` (v2) | payload avg **788 B**; heap **~960 B**; total relation **~1.44 kB** incl. PK | measured (100-row probe: 96 kB heap / 144 kB total) |
| `audit_events` | ~0.4–0.5 kB heap + ~100 B index | estimate (uuid+ws+actor+action+target+jsonb detail+partition overhead) |
| `snaplink_delivery_outbox` (v1, only when enforcement ON + binding) | ~1.2 kB (8 TEXT columns + 788 B payload) | estimate from 0235 DDL |

So every send ≈ **+2.0 kB** always (v2 + audit), **+3.2 kB** when enforcement is ON (v1 too). At 50 msg/s sustained ≈ 4.3 M msg/day ≈ **~9 GB/day** (v2+audit), ~14 GB with v1.

### 3.2 Index coverage — adequate for the claim predicate

- **v2 claim** (T-11 predicate, pg.rs): `status IN (0,1) AND available_at <= clock_timestamp() AND (lease ok) ORDER BY priority DESC, available_at, created_at, event_id LIMIT n FOR UPDATE SKIP LOCKED`.
  - 0240 `audit_governance_due_prio_idx (priority DESC, available_at, created_at, event_id) WHERE status IN (0,1)` matches the ORDER BY **exactly** → LIMIT pushdown, no sort; partial `WHERE` matches the status predicate → **pending-scan cost ∝ backlog, never table size**. 0239's legacy `due_idx` (FIFO shape) stays for rolling deploy; the planner picks `prio_idx` for the new ORDER BY, and even a fallback to `due_idx` + (incremental) sort is correct, just O(pending).
  - `SKIP LOCKED` skip-over grows with concurrent claimers × lease, the same proven v1/AiUsage pattern.
- **v1 claim**: `due_idx (available_at, created_at, delivery_id) WHERE delivered_at IS NULL` matches the v1 ORDER BY exactly.
- The real churn: each settle UPDATE (status 0→1→2) removes the row from both v2 partial indexes → per-row index insert+delete at message rate on top of the PK. Expected for an outbox; flag only for sizing.

### 3.3 Retention — none exists; this is the one open decision

- Verified: **no DELETE of delivered rows anywhere** in `aero-audit-connector/src/pg.rs`, `aero-storage/src/snaplink_commercial.rs`, `aero-server/src/snaplink_commercial/runtime.rs`, or boot timers. "Delivered rows remain as the durable reconciliation cursor" (0236/0241 comments) is the explicit design. `audit_events` is the only bounded table (partition DROP, default 365 d).
- The seam converts v2 (and, when enforcement ON, v1) into message-rate append-only logs that never shrink. The T-11 scan cost itself stays bounded (partial index), but disk, autovacuum dead tuples (status churn at message rate), and the **0241 reconcile scan** all grow unboundedly. The reconcile scan is a full unindexed `audit_events` scan (`WHERE action='message.moderated'` — no index on action) run **before every claim tick**; the seam multiplies its input table's growth rate. (That scan is the reconciler owner's slice; flag as interaction.)

**Mitigation (fits the existing retention-sweep timer pattern, §2 AGENTS)**:

```sql
-- Delivered-row sweep, safe by construction:
--   * class <> 'admin' excludes message.moderated rows — the ONLY class the
--     0241 reconciler's NOT EXISTS can resurrect. message/room seam rows are
--     never reconciled; the sink dedups by Idempotency-Key=event_id anyway.
--   * dead rows (status 3) are deliberately NOT swept: deleting a dead row
--     while its audit row survives lets 0241 re-insert it (resurrection of a
--     terminal) — leave dead + admin rows to manual ops (existing T-11 posture).
DELETE FROM audit_governance_outbox
 WHERE status = 2 AND class <> 'admin' AND delivered_at < now() - interval 'N days';
```

- Admin-class rows: budget-bounded volume; optionally sweep only past the audit-retention window (source row gone ⇒ no resurrection).
- v1: same unbounded-growth design, newly message-rate fed by the seam — raise with the v1 owner; at minimum the backlog gauge already exists (`aero_snaplink_delivery_outbox_backlog`).
- **Add a v2 pending gauge** (none exists today): `SELECT count(*) FROM audit_governance_outbox WHERE status IN (0,1)` in the observability samplers — the cheapest tripwire for relay under-capacity.

---

## 4. Priority-10 fairness and transaction duration

### 4.1 "不扰动 moderation 优先" holds — structurally, not just by volume

- Claim ORDER BY is `priority DESC, available_at, created_at, event_id`; priority 10 < 100 strictly, so **every batch's claimed set contains all due moderation rows before any backlog row**, regardless of enqueue order or volume. LIMIT truncates only the backlog tail. This is pinned by `mixed_priority_claim_orders_moderation_first_then_fifo` (40 backlog + 10 admin seeded with *later* `available_at`, claim 25 → exactly `{10 admin} ∪ {15 earliest backlog}`).
- Within-lane fairness: FIFO by `(available_at, created_at, event_id)`; rows enqueued in one statement tie on the `clock_timestamp()` defaults and break on the ULID `event_id` ≈ enqueue order — new sends never overtake old ones. No starvation; sustained overflow manifests as growing backlog, not reordering.
- The only genuine effect of priority-10 rows is **volume, not ordering**: the seam turns every send into a durable claim. Relay defaults are poll 5 s / batch 100 / concurrency 4 ≈ **20 rows/s**; the max config (1 s / 500 / 32) ≈ 500 rows/s. If arrival exceeds capacity, the backlog grows monotonically (producer is unconditional — no backpressure), every tick's scan grows with the backlog, and message-row delivery latency grows — while moderation latency is unaffected. This is an ops-sizing item: set `AERO_AUDIT_POLL_INTERVAL_SECS`/`AERO_AUDIT_BATCH_SIZE`/`AERO_AUDIT_CONCURRENCY` to match peak message rate, and add the §3.3 gauge.

### 4.2 Transaction duration — +2 statements, no budget interaction, negligible lock risk

- After mitigation the seam adds **2 statements** (was 4–5). Local PG ~0.2–0.5 ms/statement ⇒ **+0.4–1 ms** on a send tx that already runs ~9 in-tx statements (~+20 %); remote PG +2–10 ms. Moderation `soft_delete` tx grows similarly (its audit INSERT already exists; net +1 statement).
- **60s cost budgets: no interaction.** `AiWorker`'s per-ws `KeyedCostBudget(60)`/global budget and `moderation_bot`'s budgets meter **AI spend** (weighted provider calls); the seam's statements are not metered by them and cannot consume or defer them. The 60s window is also four orders of magnitude above the ms-scale tx extension.
- **Lock contention with sweep timers: negligible.** The seam is append-only: `audit_events` + both outboxes get INSERTs only; FK checks take KEY SHARE on `workspaces` (shared-compatible with the existing room FOR SHARE). No new exclusive-lock surface. The remaining effects are second-order:
  - Room FOR SHARE held ~ms longer — shared-compatible among senders; only exclusive room ops (archive/delete/role changes) queue marginally longer.
  - Per-workspace append hot pages (audit `(workspace_id, id DESC)`, PK `(id, created_at)`, outbox partial-index right edges) — the same class as existing `event_outbox` writes; roughly doubles per-send index-insert churn.
  - Retention sweep (3600 s): audit DELETE/partition-DROP targets old partitions while sends append to today's; message soft-delete targets expired rows, disjoint from fresh sends. The sweep does not route through the seam (set-based SQL) ⇒ no outbox-row flood from sweeps.

---

## 5. Amendment list (delta to the design doc)

1. **audit.rs**: `append_on` → `RETURNING id, to_jsonb(created_at) #>> '{}'`; add `append_in_tx_with_occurred_at`; existing signatures untouched (17 call sites unaffected). [§1.3]
2. **envelope_in_tx**: drop the occurred_at SELECT and the source_system SELECT; take `occurred_at: &str` (PG-rendered) and fold source_system as the `COALESCE` subquery inside the outbox INSERT (`jsonb_set` patch). [§1.3, §2.2]
3. **send seam**: capture `LockedRoomWriteAccess.workspace` in `insert_outboxed` (drop `SELECT workspace_id FROM rooms`); edit seam already planned this. [§2.1]
4. **New sweep** (own interval or fold into retention sweep): delivered-row DELETE for v2, `status=2 AND class <> 'admin' AND delivered_at < cutoff`; dead/admin rows excluded (resurrection rules). v1 retention decision escalated to the v1 owner. [§3.3]
5. **Observability**: v2 pending-backlog gauge. [§3.3]
6. **Ops**: relay throughput knobs sized to message rate; flag the 0241 reconcile scan's unindexed `action` predicate as an interaction. [§4.1]
7. Acceptance delta: half-B's occurred_at SELECT assertion moves to the `RETURNING` value (same spelling, no subquery); the parity tests' per-op round-trip expectations are internal (no assertion change).

No migration, no connector change, no ImService change — all amendments are inside `aero-storage` (the seam's own layer) plus one ops note.

# Design — B5-3 D-CAP: anti-starvation minimum-service floor in `PgOutboxRepo::claim_due` (two-arm claim with backlog backfill)

> Module: `crates/aero-ai` (fixture authority: `crates/aero-ai/src/governance.rs` — the lane constants) with the physical change in `crates/aero-audit-connector` (`pg.rs` claim query + `relay.rs` pure floor function + `fake.rs` parity + priority-drill starvation leg).
> Requirements: `docs/auto/runs/add-the-b5-3-anti-starvation-cap-to-claim-due-pe-2cb2a2b1/artifacts/requirements-10762e10/requirements.md` (B5-3 D-CAP spec, R1-R7 / AC1-AC6). Design authority: `docs/design/2026-08-07-aero-ai-b5-3-priority-claim-ordering.design.md` §7.1 (D-CAP decision).
> Status: design, pre-implementation. Empirical SQL validation executed 2026-08-15 against PG 17.10 (`pgvector/pgvector:pg17`) with the real 0239 table + 0240 index on a throwaway DB.

## 0. Evidence verification verdict

The requirements spec was treated as untrusted and every citable claim re-checked against the repo on 2026-08-15. **10/12 evidence rows verified as claimed; 2 corrected** (one is a genuine spec arithmetic bug that must be fixed before implementation; one is a stale run-dir observation). Additionally, the central mechanism (the two-arm SQL) was **empirically validated** on live Postgres 17.10 with the real migrations — this design pins the validated SQL verbatim.

| # | Evidence claim | Verdict |
|---|---|---|
| 1 | `claim_due` SQL: `ORDER BY priority DESC, available_at, created_at, event_id`, one plain `LIMIT $1`, `limit.clamp(1, MAX_CLAIM)` at pg.rs:106, no per-priority bound | ✅ Confirmed. `impl OutboxRepo` at pg.rs:80, `claim_due` spans pg.rs:98-131; `reconcile` at pg.rs:81 (evidence's "87-131" was its own drift note — the corrected 98-131 matches). |
| 2 | `MAX_CLAIM = 500` at relay.rs:35; `dispatch_batch` = `reconcile` then one `claim_due`; drain loops until 0; `AERO_AUDIT_BATCH_SIZE` default 100, range 1..=500 (config.rs:89) | ✅ Confirmed verbatim (relay.rs:164-180, `run` at 116, `shutdown_drain` at 135). |
| 3 | Priority drill seeds 500+1 backlog + 1 moderation, batch 100, MAX_ROUNDS 10, concurrency 1; asserts `moderation-in-first-batch`/`drain-502`/`parity-502`/vocabulary; no starvation leg | ✅ Confirmed (bin lines 83-91, 280-385). |
| 4 | Proposal line 10: "注入积压 drill（500 积压 + 1 moderation → 先达 sink）+ 反饥饿上限" | ✅ Confirmed verbatim. |
| 5 | Both run dirs contain no artifacts; both [proposed] | ⚠️ **Partially stale.** `add-a-priority-lane-anti-starvation-cap-to-claim-160028ca` has no `artifacts/` (DECISIONS.md records stage FAIL, agent exited 1) ✅. But `add-the-b5-3-anti-starvation-cap-to-claim-due-pe-2cb2a2b1` has since executed (artifacts created 2026-08-14 19:35) and produced `artifacts/requirements-10762e10/requirements.md` — the very document under review. "Both [proposed]" is stale for the second run; this changes nothing technical. |
| 6 | `grep starvation` across the connector crate → zero hits; no quota/floor/backfill claim code | ✅ Confirmed (only unrelated `reconcile` "backfill" comments hit). |
| 7 | `mixed_priority_claim_orders_moderation_first_then_fifo` at pg.rs:546, doc at 530; 40 backlog (inverted `created_at`) + 10 admin (later `available_at`); `claim_due(30s, 25)` expects exactly `{10 admin} ∪ {15 earliest backlog}` | ✅ Confirmed (doc comment at pg.rs:528, fn at 546; expected-set assert at 610-619 — trivial line drift). |
| 8 | T-11 drill: `AERO_AUDIT_DRILL_ROWS` default 3, bound ≤ 95 (`total = rows + 5 ≤ 100`), `claimed == total` at batch 100; test-integration.sh:367-462 + b5-pin.sh:42 | ✅ Confirmed (header lines 45-51, runtime env check at 91-103, assert at 219; test-integration.sh:460, b5-pin.sh:42). |
| 9 | D-CAP §7.1 design authority: two-arm claim with backfill in one statement, explicit exclusion, `K = max(1, batch/20)`, verification (a)/(b)/(c), sibling drill name `aero-audit-min-service-drill`, direction's "extended leg" governs | ✅ Confirmed (§7.1 at design doc line 380; mechanism text matches). |
| 10 | Lane constants 100/10 at governance.rs:31/:33; `Claim` carries `priority: i16` + `class: String` (outbox.rs:57-76); `FakeOutbox::claim_due` single-`take` mirror at fake.rs:197+ | ✅ Confirmed (outbox.rs `Claim` at 44-56; fake sort at 214-217, take at 225). |
| 11 | **§3 R6: "seed 120 admin rows… assert each round's claimed set contains exactly 5 priority-10 rows and 95 priority-100 rows… after the two rounds 10 backlog + 190 admin are claimed"** | ❌ **Arithmetic bug.** With 120 admin: round 1 = 95a+5b; round 2 has only 25 admin left → arm A = 25a+35b (60 rows), arm B empty → round 2 = **60 rows (25a + 35b), not 95a+5b**; "190 admin claimed" is impossible. **Fix: seed 190 admin** (round 1: 95a+5b; round 2: 95a+5b; totals 190a+10b; 30 backlog remain — consistent with the evidence's own "190 admin" figure). This design pins the corrected seed. |
| 12 | §3 R2 arithmetic: `K(25)=1` → {10 admin} ∪ {15 backlog}; `K(100)=5` T-11/phase-1/phase-2 drain arithmetic (7 non-empty rounds); `K(10)=1` fake tests; `K(25)=1` concurrent double-claim | ✅ Confirmed by re-derivation **and** empirically (probes S1-S5, below). Phase-2 drain: rounds 1-6 @ 95a/5b, round 7 @ 30a+70b, round 8 empty → 7 non-empty ≤ MAX_ROUNDS 10. |

**Empirical SQL validation (new evidence — the mechanism is the risky part, so it was executed):** the two-arm statement below was run against the throwaway DB `b53_probe` (migrations 0239 + 0240 applied verbatim, PG 17.10):

| Probe | Scenario | Result |
|---|---|---|
| S1 | Flood: 100 admin + 40 backlog, batch 100, K=5 | **100 claimed = 95 admin + 5 backlog, 100 distinct event_ids** (arm dedupe works) |
| S2 | Identity: 10 admin + 40 backlog, batch 25, K=1 | **25 claimed = 10 admin + 15 backlog = uncapped top-25** (identity property) |
| S3 | Degenerate: batch=1 → K=0, arm B `LIMIT 0` | **1 row claimed**, precedence preserved |
| S4 | Regression exact (mirrors pg.rs:546 test: inverted `created_at`, later admin) | **{10 admin} ∪ {15 earliest-available_at backlog}** — the test's expected set, byte-for-byte |
| S5 | Sustained: 200 admin + 40 backlog, batch 100, two sequential claims | **Round 1: 95a+5b; round 2: 95a+5b** (190a+10b total) — proves the R6 shape works with ≥190 admin |
| S6 | `EXPLAIN` (1400 rows) | Arm A: `Index Scan using audit_governance_due_prio_idx` (0240); arm B: `Index Scan Backward` (MIN) + `Index Scan` + `Nested Loop Anti Join` on the **materialized** arm-a CTE. **No Seq Scan, no new index needed.** Plan-shape caveat: the pure Index-Scan shape is planner-dependent until ~100k rows (at 1400 rows a Bitmap Heap Scan + Sort was observed); the MIN backward walk is O(leased low-lane) worst case — measured 28.7 ms at 100k fully-leased rows, ~350× inside the 10 s statement timeout, no correctness impact. |

Baseline before design: `cargo check -p aero-audit-connector` clean; `cargo test -p aero-audit-connector --lib` 14 passed / 3 ignored (PG-gated) — the three `#[ignore]` tests are the regression surface for AC3.

## 1. Scope

Add the B5-3 反饥饿上限 (anti-starvation cap) to `PgOutboxRepo::claim_due`: every claim round reserves `K` slots for the **lowest-priority lane** (today priority 10 = `GOVERNANCE_PRIORITY_BACKLOG`; producers: 0239 trigger, 0242 L1 window aggregates, 0245 room lane, 0246 message recall — all landed, relay wired at `aero-server/src/bin/main.rs:251-258`), so a sustained priority-100 (`GOVERNANCE_PRIORITY_MODERATION`) flood can never monopolize the claim batch. Liveness bound: `B` low-lane rows reach `status = 2` within `ceil(B/K)` rounds regardless of admin backlog size (at batch 100, K=5: 100 backlog rows ≤ 20 ticks).

**Non-goals (unchanged from the requirements spec):** no migration; no env var / `RelayConfig` field; no change to `settle`/`requeue`/`mark_dead`/`deliver_claim` 403→dead-immediate; no change to `aero-ai` governance constants, the 0239 trigger, or lane producers; no change to the drill's phase 1 (byte-identical, `drain-502`/`parity-502`/`moderation-in-first-batch` PASS lines untouched); no change to `aero-audit-t11-drill` or its `AERO_AUDIT_DRILL_ROWS ≤ 95` bound.

## 2. API changes

### 2.1 `relay.rs` — pure floor function (beside `MAX_CLAIM`, relay.rs:35)

```rust
/// Per-tick minimum service floor divisor (D-CAP, design §7.1): the
/// lowest-priority lane is guaranteed `max(1, batch / 20)` claim slots per
/// round. Config-visible constant pattern (clone of `MAX_CLAIM`); no env
/// var, no `RelayConfig` field.
pub const MIN_SERVICE_BATCH_DIVISOR: i64 = 20;

/// K = per-tick minimum service floor for the lowest-priority lane:
/// `min(max(1, b / MIN_SERVICE_BATCH_DIVISOR), b − 1)` where `b =
/// batch.clamp(1, MAX_CLAIM)`. The `b − 1` upper clamp keeps the floor
/// strictly below the batch so arm A (top `limit − K`) is never empty: at
/// the degenerate `batch = 1` the floor is 0 and the claim degenerates to
/// today's uncapped top-1 (precedence preserved, never inverted). Pure +
/// total — no clock, no DB, no panic on any i64 input.
#[must_use]
pub fn min_service_floor(batch: i64) -> i64 {
    let b = batch.clamp(1, MAX_CLAIM);
    (b / MIN_SERVICE_BATCH_DIVISOR).max(1).min(b - 1)
}
```

Pins: `(0)→0, (1)→0, (2)→1, (20)→1, (21)→1, (25)→1, (40)→2, (100)→5, (500)→25, (i64::MAX)→25`. Both items are `pub` in `pub mod relay` (lib.rs re-exports the module) — `unreachable_pub` warn-clean.

### 2.2 `pg.rs` — two-arm claim in `claim_due` (pg.rs:98-131)

Signature, lease binding, `limit.clamp(1, MAX_CLAIM)`, the `UPDATE … FROM claimable … RETURNING` and the returned `ClaimRow` are **unchanged**. The `claimable` CTE becomes:

```rust
let limit = limit.clamp(1, MAX_CLAIM);
let floor = min_service_floor(limit);          // K
let arm_a_limit = limit - floor;               // ≥ 1 always (K ≤ limit − 1)
// ...
let rows = sqlx::query_as::<_, ClaimRow>(
    r"WITH arm_a AS MATERIALIZED (
          SELECT candidate.event_id
            FROM audit_governance_outbox AS candidate
           WHERE candidate.status IN (0, 1)
             AND candidate.available_at <= clock_timestamp()
             AND (candidate.lease_expires_at IS NULL
                  OR candidate.lease_expires_at <= clock_timestamp())
           ORDER BY candidate.priority DESC, candidate.available_at,
                    candidate.created_at, candidate.event_id
           FOR UPDATE SKIP LOCKED
           LIMIT $1
      ),
      arm_b AS (
          SELECT candidate.event_id
            FROM audit_governance_outbox AS candidate
           WHERE candidate.status IN (0, 1)
             AND candidate.available_at <= clock_timestamp()
             AND (candidate.lease_expires_at IS NULL
                  OR candidate.lease_expires_at <= clock_timestamp())
             AND candidate.priority = (
                   SELECT MIN(priority)
                     FROM audit_governance_outbox
                    WHERE status IN (0, 1)
                      AND available_at <= clock_timestamp()
                      AND (lease_expires_at IS NULL
                           OR lease_expires_at <= clock_timestamp())
                 )
             AND NOT EXISTS (SELECT 1 FROM arm_a
                              WHERE arm_a.event_id = candidate.event_id)
           ORDER BY candidate.available_at, candidate.created_at, candidate.event_id
           FOR UPDATE SKIP LOCKED
           LIMIT $2
      ),
      claimable AS (
          SELECT event_id FROM arm_a
          UNION ALL
          SELECT event_id FROM arm_b
      )
      UPDATE audit_governance_outbox AS outbox
         SET status = 1,
             claim_token = gen_random_uuid(),
             lease_expires_at = clock_timestamp()
                                + make_interval(secs => $3),
             attempts = outbox.attempts + 1
        FROM claimable
       WHERE outbox.event_id = claimable.event_id
   RETURNING outbox.event_id, outbox.attempts, outbox.claim_token,
             outbox.lease_expires_at, outbox.payload,
             outbox.priority, outbox.class",
)
.bind(arm_a_limit)   // $1
.bind(floor)         // $2 (0 is valid: LIMIT 0 → empty arm B)
.bind(lease_secs)    // $3
```

Design points (all probe-verified):
- **Arm A** = top `limit − K` of the total order `(priority DESC, available_at, created_at, event_id)` — exactly today's query with a smaller limit. `MATERIALIZED` is pinned explicitly (async-reviewer F1): PG already materializes a `FOR UPDATE` CTE (it cannot be inlined), but the pin makes the single-evaluation guarantee explicit — an inlining refactor would open a narrow double-claim window.
- **Arm B** = `K` earliest-due rows (FIFO tie-break) of the **lowest-priority lane** — `priority = MIN(priority)` over the claimable set, scalar subquery carrying the identical WHERE filters — **not already selected by arm A** (`NOT EXISTS` anti-join). The exclusion is load-bearing: `SKIP LOCKED` does not dedupe arms within one statement (self-locks are invisible to skip).
- The three WHERE fragments (arm A, arm B, MIN subquery) are textual copies of today's filter — the filter contract is unchanged.
- **Result invariants**: `|claimed| ≤ limit`; `|claimed| = limit` whenever high-lane due ≥ `limit − K` and low-lane due ≥ K (flood: exactly `K` low + `limit − K` high); low-lane due < K → `limit − K + low_due` (bounded slack ≤ K, reclaimed next tick by the drain loop — **arm-B slack is NOT refilled from the total order**, keeping the identity property exact); **`low_due = 0` exception** (DB-architect F4): with zero low-lane rows due, arm B's `MIN(priority)` lane IS the high lane, so arm B backfills admin rows — the batch stays full at `limit` with **no throughput tax** (probe E3: 500 admin + 0 backlog → 100 claimed/tick, identical to today's top-100); identity: high-lane due ≤ `limit − K` → claimed set **identical to today's uncapped top-`limit` set** (probe S2/S4). **Snapshot-relative semantics** (async-reviewer F2/F4): the "exactly K low per round" invariant is per-statement against that statement's snapshot — under concurrent claimers one statement's arm B can be diluted to 0 (SKIP LOCKED skips rows another statement already locked); the *aggregate* floor (≥ min(K, low supply) per window) holds by conservation, and cross-statement arm overlap is excluded at the lock step, not by the anti-join.

### 2.3 `outbox.rs` — trait contract doc (outbox.rs:72-80)

Keep the existing priority-DESC/FIFO language; append the floor contract: "each claim round reserves `min_service_floor(limit)` slots for the lowest-priority lane (today: priority 10 backlog); the claimed set equals the uncapped top-`limit` set whenever the high lane is underfull." **No signature change** — the trait seam and every `OutboxRepo` implementor's call sites are untouched.

### 2.4 `fake.rs` — two-arm mirror (fake.rs:197-243)

Mirror the PG set exactly, in lockstep (§2.2):

```rust
async fn claim_due(&self, lease: Duration, limit: i64) -> Result<Vec<Claim>, Error> {
    // ... existing clock/lease/filter/sort (fake.rs:200-217) unchanged ...
    let limit = limit.clamp(1, MAX_CLAIM);                      // parity: PG clamps
    let floor = min_service_floor(limit);
    let arm_a_take = usize::try_from(limit - floor).unwrap_or(usize::MAX);
    let arm_b_take = usize::try_from(floor).unwrap_or(usize::MAX);
    let mut selected: HashSet<AuditId> = HashSet::new();        // arm-A ids
    // arm A: first (limit − K) of the sorted total order
    for (id, ..) in due.iter().take(arm_a_take) { selected.insert(*id); }
    // arm B: first K rows of the MIN-priority lane not already selected
    if let Some(min_priority) = due.iter().map(|(_, _, _, p)| *p).min() {
        for (id, _, _, p) in due.iter()
            .filter(|(id, _, _, p)| *p == min_priority && !selected.contains(id))
            .take(arm_b_take)
        { selected.insert(*id); }
    }
    // ... mutate the selected rows (status/attempts/token/lease) as today ...
}
```

(Exact shape per implementer taste; the **set semantics** are the contract — arm A then arm B with min-priority + exclusion.) Relay-level tests (`assert_transient_requeue*`, `rotation_lag_recovers_via_bypass_before_dead`) all use ≤ 1-2 single-lane rows at `limit 10` → K=1, arm A takes the row, arm B empty → **neutral** (verified by inspection).

### 2.5 Drill bin — starvation leg (phase 2, additive)

New constants (production values verbatim, D8′ pin style):

```rust
/// Phase 2 (D-CAP): admin flood size — exceeds MAX_CLAIM = 500 so even the
/// maximum clamp cannot drain it in one round.
const STARVATION_ADMIN_ROWS: i64 = 600;
/// Phase 2: backlog lane size (the direction's "100 backlog rows").
const STARVATION_BACKLOG_ROWS: i64 = 100;
/// Phase 2: asserted floor at batch 100 — pin `min_service_floor(100) == 5`
/// in the bin's `#[cfg(test)]` module (D8′ style), never a blind constant.
```

Phase 2 sits **after `parity-502: PASS`, before `stub.shutdown()`** (same stub, same relay, batch 100, concurrency 1). Sequence:

1. **Self-isolating gate**: repeat the drill's destructive-gate pattern in a transaction — `LOCK … ACCESS EXCLUSIVE` → `COUNT(*)` → refuse with the same `REFUSED` + exit 1 unless `AERO_PRIORITY_DRILL_ALLOW_TRUNCATE == "1"` (after phase 1 the table holds 502 rows, so the env check is meaningful even on a previously-empty DB) → `TRUNCATE` → commit. Phase-1 statements are not edited; this is a new helper invoked only by phase 2.
2. **Seed order is the oracle**: 600 admin rows (`priority = MODERATION_PRIORITY = 100`, class 'admin', `available_at = clock_timestamp()` **first/earliest**) then 100 backlog rows (`priority = BACKLOG_PRIORITY = 10`, class 'message', seeded **last/latest**) — FIFO can never explain low-lane progress.
3. **Round 1**: `relay.dispatch_batch()` → `claimed == 100`; `COUNT(status = 2 AND priority = 100) == 95`; `COUNT(status = 2 AND priority = 10) == 5` → `println!("drill: starvation-round1-split-95-5: PASS")`.
4. **Rounds 2-5** (4 more `dispatch_batch`): after round 5, `COUNT(status = 2 AND priority = 10) == 25` **and** `COUNT(status IN (0, 1) AND priority = 100) == 125 > 0` → `println!("drill: starvation-cross-batch-quota: PASS")` — the quota is per-round, not a round-1 artifact.
5. **Drain**: rounds 6..=MAX_ROUNDS with the existing loop pattern (break on `claimed == 0`; bail at `MAX_ROUNDS`). Precomputed: rounds 1-6 @ 95a/5b, round 7 @ 30a+70b (arm A 30a+65b, arm B 5b), round 8 empty → **7 non-empty rounds ≤ 10**.
6. **Final**: `COUNT(status = 2) == 700` + event_id set-parity vs the phase-2 seed set (existing parity pattern) → `println!("drill: starvation-drain-700: PASS")`.

Phase 1 (seed, round-1 assert, `moderation-in-first-batch`, vocabulary pin, `drain-502`, `parity-502`) is **not edited**; the D8′ vocabulary pin and table/column probes run once at the top as today.

### 2.6 `scripts/test-integration.sh` + design record

- In the moderation-priority-drill leg (≈lines 505-595): keep the existing `priority: landed` and `drill: moderation-action-vocabulary: PASS` greps; add one additive grep `drill: starvation-drain-700: PASS` in the same block. Exit-code contract unchanged (0 PASS / 1 FAIL / 2 SKIP).
- `scripts/b5-pin.sh`: **no change** (`t11-fail-closed` :42, `moderation-priority-drill` :43 stay; no slot count change).
- `docs/design/2026-08-07-aero-ai-b5-3-priority-claim-ordering.design.md` §7.1: status `scheduled → landed`; record the drill landed as an **extended leg of `aero-audit-priority-drill`** per the direction's acceptance (not the sibling-named `aero-audit-min-service-drill`), and that the verification-(c) PG test lands with the corrected 190-admin seed (§0 finding 11).

## 3. Compatibility constraints

| Axis | Constraint | Evidence |
|---|---|---|
| Schema | **No migration.** `audit_governance_due_prio_idx` (0240, `(priority DESC, available_at, created_at, event_id) WHERE status IN (0,1)`) serves **both** arms — probe S6 shows Index Scan on it for arm A, Index Scan Backward for the MIN, Index Scan + Anti Join for arm B. No new index, no `_sqlx_migrations` touch. | S6 EXPLAIN |
| Config | No new env var; no `RelayConfig` field; floor is the named `pub const` pattern beside `MAX_CLAIM`. | §2.1 |
| API | `OutboxRepo::claim_due` signature unchanged; `ClaimRow` shape unchanged; new `pub` items are additive (module already `pub`). | §2.2/2.3 |
| State machine | `settle`/`requeue`/`mark_dead`/`deliver_claim` (incl. 403→dead-immediate) untouched; claims still rotate fencing tokens, settle fenced on `claim_token AND lease`, redelivery carries `Idempotency-Key = event_id`. At-least-once posture unchanged — the cap changes only *which* rows are claimed per round. | — |
| In-batch semantics | Preemption within a batch and within-lane FIFO unchanged; cross-batch fairness added. Identity property: high-lane due ≤ `limit − K` → claimed set **identical** to today's top-`limit` set. | S2/S4 |
| Existing oracles | `mixed_priority…` (K(25)=1), `concurrent_double_claim…` (50 single-lane, 25/session → arm A 24 + arm B 1), `expired_lease…` (single row), T-11 (K(100)=5: arm A {1 admin + 94 backlog} ∪ arm B 5 → `claimed == total` for all `rows ≤ 95`), drill phase 1 (round 1 = {moderation} ∪ 99 backlog, moderation in arm A), relay fake tests (K(10)=1, single row → arm A takes it) — all unchanged, precomputed in the spec §2 arithmetic table and confirmed by probes S1-S5. | §0 |
| Rolling deploy | Old single-LIMIT binary + new two-arm binary may run against the same table: both produce valid claim sets under the same lease/fence protocol; both arms are `FOR UPDATE SKIP LOCKED`, so concurrent instances still partition the due set disjointly. No lock-protocol change. | — |
| Postgres version | Requires the two-arm CTE shape to parse and materialize correctly (arm-a CTE materialized because it carries `FOR UPDATE` + LIMIT). Validated on PG 17.10; the project's runtime is PG 17 (docker-compose `pgvector/pgvector:pg17`), so no constraint change. | S1-S6 |

## 4. Failure modes

| Failure | Mechanism | Mitigation / pin |
|---|---|---|
| Arm-A/arm-B overlap (missing `NOT EXISTS`) | `SKIP LOCKED` does not dedupe within one statement (self-locks); a row could appear in both arms → wrong split (backlog < K), potential double-count in `claimable`. | The exclusion is structural (§2.2); pinned by drill `starvation-round1-split-95-5` and R6's disjointness assert (round-2 set ∩ round-1 set = ∅). Probe S1: 100 distinct ids. |
| K ≥ batch (precedence inversion) | At `batch = 1`, a floor of 1 would empty arm A and let a low-priority row preempt the top-1. | `b − 1` upper clamp → `K(1) = 0`, arm B `LIMIT 0` (valid SQL, probe S3). Unit-pinned (AC2). |
| Non-total floor fn | Panic/UB on extreme input (i64::MAX). | Pure + total by construction; pinned by unit tests incl. `(i64::MAX) → 25`. |
| Underfull low lane (low due < K) | Batch claims `limit − K + low_due < limit` (bounded slack ≤ K) — **except `low_due = 0`**: an empty low lane backfills from the MIN lane (the high lane), so the batch stays full with no throughput tax (probe E3). | Relay `run`/`shutdown_drain` loop until 0 reclaims next tick; no row stranded. Rejected alternative (fill slack from total order) kept out to preserve the identity property. |
| Mid-lane starvation (3+ lane values) | The floor protects only the **MIN** lane; a hypothetical priority-50 lane could still starve under a 100-flood. | Documented limitation: today exactly two lane values (10/100); a future lane lands via migration + this design's MIN-lane generalization (the SQL is already MIN-driven, not hardcoded to 10). |
| SQL drift between the 3 WHERE copies | Arm A / arm B / MIN subquery filters could diverge. | R6 + regression tests pin the set semantics; the filters are textual copies of today's single filter (status IN (0,1), due, lease). |
| Drill phase-2 gate bypass | Phase 2 TRUNCATE running without the env opt-in (e.g., direct invocation against an empty DB where phase-1's gate passes vacuously). | Phase 2 repeats the LOCK+COUNT+env check (`AERO_PRIORITY_DRILL_ALLOW_TRUNCATE == "1"`), refusing exit 1 with the same REFUSED message. |
| Performance regression | A second index scan + MIN lookup per round. | Probe S6: both arms index-scan 0240, arm B bounded at K ≤ 25 (batch 500 → K=25; production 100 → K=5); MIN via Index Scan Backward (first entry). No Seq Scan at 1400 rows. |
| R6 seed miscount (evidence bug) | With the evidence's 120 admin seeds, round 2 claims 60 rows (25a+35b) — the "95a+5b each round" assert fails. | **Fix in this design: seed 190 admin + 40 backlog** (round 1: 95a+5b; round 2: 95a+5b; totals 190a+10b; 30 backlog remain). Verified by probe S5 (200-admin variant of the same shape). |

## 5. Migration steps

**None.** The requirements spec's "no migration" scope is verified sound: 0240 already lands the exact partial index matching the total order both arms scan (probe S6 uses it), and `_sqlx_migrations` is untouched. The only "steps" are the normal repo discipline for this change batch:

1. `cargo build` (nothing migration-related, but the standing rule for any change touching `migrations/` — not needed here since no new `NNNN_*.sql`).
2. `cargo check --workspace` / `cargo clippy --workspace --all-targets` (no new warnings; new code is pure + `#[must_use]`, `unsafe` forbidden).
3. Run the acceptance set (below) against a throwaway migrated DB (AGENTS.md §4.3: `CREATE DATABASE` → migrate → run → `DROP DATABASE`).
4. Update the D-CAP §7.1 status record (§2.6).

## 6. Testable acceptance mapping

| AC | Testable acceptance | Gate |
|---|---|---|
| AC1 | Throwaway DB, `AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=1 DATABASE_URL=… cargo run -p aero-audit-connector --bin aero-audit-priority-drill` exits 0, printing in order: phase-1 lines unchanged (`moderation-in-first-batch`, `drain-502`, `parity-502`, `moderation-action-vocabulary`) then `drill: starvation-round1-split-95-5: PASS`, `drill: starvation-cross-batch-quota: PASS`, `drill: starvation-drain-700: PASS`. `B5-CHECK moderation-priority-drill: PASS` (b5-pin.sh:43) holds. | test-integration.sh leg |
| AC2 | `cargo test -p aero-audit-connector` green with `min_service_floor` pins `(0)→0, (1)→0, (2)→1, (20)→1, (21)→1, (25)→1, (40)→2, (100)→5, (500)→25, (i64::MAX)→25`; split pins: batch 100 → 95/5, batch 500 → 475/25, batch 25 → 24/1; total over `[1, MAX_CLAIM]`, no panic, no `K ≥ batch`. | unit tests (relay.rs `#[cfg(test)]`, pure) |
| AC3 | With `DATABASE_URL` on a migrated throwaway DB: `cargo test -p aero-audit-connector --lib -- --ignored mixed_priority_claim_orders_moderation_first_then_fifo` passes **unedited** (claimed set == {10 admin} ∪ {15 earliest backlog}); `concurrent_double_claim_across_two_sessions_is_impossible` and `expired_lease_is_reclaimed_with_fresh_token` also green unedited. | PG tests (3) |
| AC3.1 | **New** R6 PG test (evidence §3 R6, **corrected seed: 190 admin + 40 backlog**): two `claim_due(Duration::seconds(30), 100)` calls; round 1 = exactly 95 p100 + 5 p10; round 2 = exactly 95 p100 + 5 p10, disjoint from round 1; totals 190 p100 + 10 p10 claimed; 30 backlog remain. | PG test (`#[ignore = "requires live Postgres (DATABASE_URL)"]`) |
| AC4 | T-11 slot: `aero-audit-t11-drill` exits 0 unchanged with `AERO_AUDIT_DRILL_ROWS ≤ 95` and `claimed == total` (precomputed: arm A 95 = {1 admin} ∪ {94 backlog}, arm B 5 → `claimed = rows+5 = total` ∀ rows ≤ 95); `b5_check "t11-fail-closed" "PASS"` (test-integration.sh:460). | test-integration.sh |
| AC5 | Moderation-priority-drill leg end-to-end on a throwaway DB: destructive-gate negative checks (REFUSED ×3) unchanged, drill exit 0, existing greps + the new `drill: starvation-drain-700: PASS` all hit, `b5_check "moderation-priority-drill" "PASS"`, DB dropped. | test-integration.sh |
| AC6 | `cargo check --workspace` clean; `cargo test --workspace --lib` green (new pure tests included); `cargo clippy --workspace --all-targets` no new warnings; `scripts/truth-check.sh` 0 violations (`min_service_floor` called from pg.rs + tests; `MIN_SERVICE_BATCH_DIVISOR` referenced by the fn + tests — no orphans); `scripts/file-size-check.sh` 0 violations (pg.rs 635 → ~720 < 800 WARN; relay.rs 618 → ~650 < 800; drill bin 416 → ~505 < 800). | AGENTS.md §4.3 gates |

## 7. Risks & open items

- **The two-arm SQL is now probe-validated** (S1-S6, PG 17.10, real schema) — the primary residual risk (statement shape, arm dedupe, index usage) is retired. What remains unexecuted is the drill phase-2 code and the corrected R6 test, both of which are pure arithmetic on the validated mechanism.
- **D-CAP §7.1 verification-(b) naming**: the design doc named a sibling drill `aero-audit-min-service-drill`; the direction's acceptance ("extended leg of `aero-audit-priority-drill`") governs, and §7.1's status record must reconcile this (§2.6). The sibling drill name stays as a historical reference, not a deliverable.
- **`docs/auto/analyses/crates-aero-ai-f8cd3622.json`** (direction source): not re-read in this pass; the spec's direction quotes were consistent with everything in-tree, but the analysis file remains the authoritative direction record for the campaign harness.
- **Follow-ups recorded elsewhere, not this slice**: legacy `audit_governance_due_idx` cleanup (design D5, P4), status-3 DLQ admin surface (D6).

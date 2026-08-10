# Design — B5-3 (aero-audit-connector slice): moderation-first claim ordering (`priority DESC` in `PgOutboxRepo::claim_due`)

> Module: `crates/aero-ai` (fixture authority: `governance.rs`) with the physical
> change in `crates/aero-audit-connector` (`pg.rs` claim query).
> Requirements: `docs/requirements/2026-08-07-aero-ai-b5-3-priority-claim-ordering.req.md`
> (R1-R5 / AC1-AC4). Status: design, pre-implementation.
>
> **P1 fold-in amendment (2026-08-08, landed with the 0239 batch):** the
> slice is **implemented in the working tree** as part of the 0239 verified
> landing (see `2026-08-08-aero-ai-b5-1-migration-0239-verified-landing.design.md`
> §4 amendment + verification block): R1 ORDER BY term + R2 fixture column +
> R3 PG test + R4 trait doc all landed in `crates/aero-audit-connector`;
> 0240 ships in the same migration set; the drill constants were aligned
> from [PROPOSED] 200/100 to the authoritative 10/100 (governance.rs — §7.2
> open item closed); and the **D3 batch-membership correction was applied to
> the drill in this fold** — the tree drill still carried the old
> `delivered_at == MIN(delivered_at)` asserts at design time (the "already
> carries the correction" note below was premature; heap-order RETURNING
> made those asserts unsatisfiable, so the fold applied §2.4/AC1's membership
> oracle verbatim). Verified: priority drill exit 0 (`moderation-in-first-batch`
> / `drain-501` / `parity-501` PASS), relay/T-11 drills exit 0, R3 + all PG
> + storage oracles green on fresh throwaway migrated DBs.
>
> **Deployment-review amendment (2026-08-08):** §5 now lands migration 0240
> (the due-priority index discharging 0239:44's recorded handoff — decision
> D1), §5.2 specifies the dual-partial-index rolling deploy (F9/F10), §5.3
> documents operational visibility for status-3 dead governance rows and the
> stale-index monitoring path, and §7 records the decisions (D1-D6) plus
> D-CAP — the anti-starvation cap converted from an open item into a
> scheduled, drill-verified deliverable gated on L1. The oracle's firstness
> asserts are corrected to batch membership (D3), already reflected in the
> drill. Requirements doc amended in kind (§0 of the req file).

## 0. Evidence verification verdict

All six cited claims and both additional findings were re-checked against the
repo on 2026-08-07; **every claim verified**. One empirical proof was executed
(red-state drill run, below).

| # | Claim | Verification |
|---|---|---|
| 1 | `pg.rs:90-93` — `claim_due` `ORDER BY available_at, created_at, event_id`, no priority | ✅ Confirmed. Actual text at pg.rs:87-88 (`ORDER BY candidate.available_at, candidate.created_at,` / `candidate.event_id`); no `priority`/`class` reference anywhere in the claim CTE. Evidence's line span 90-93 is a small drift (the cited lines are the `FOR UPDATE SKIP LOCKED`/`LIMIT` tail); substance exact. |
| 2 | Drill `:29-33` — "0239 landing *without* B5-3's priority ordering is NOT a SKIP" | ✅ Confirmed. `aero-audit-priority-drill.rs:29-33` (doc comment): the drill FAILS red once 0239 has landed; exits 2 SKIP only when the table or its `priority`/`class` columns are absent. |
| 3 | Drill seeds backlog first (earlier `available_at`), moderation last; round-1 `delivered_at == MIN(delivered_at)`; strict-first on full drain; `concurrency=1` | ✅ Confirmed. Seed at drill:140-182 (500 backlog then 1 moderation, both `clock_timestamp()`), round-1 asserts at 190-230 (`BATCH_SIZE=100 < 501` so composition must come from priority), drain asserts at 260-300 (parity + `moderation_at < MIN(backlog delivered_at)`). `RelayConfig.concurrency: 1` ⇒ settle order == claim order. |
| 4 | `governance.rs` — `GOVERNANCE_PRIORITY_MODERATION=100 > BACKLOG=10`, DESC pin, "do not align with ai_job" warning, both named unit tests | ✅ Confirmed. Constants at governance.rs:26/31; module doc 11-25 carries the DESC-vs-ASC warning ("Do not 'fix' the direction to match `ai_job`"); tests `moderation_lane_preempts_backlog_under_desc_claim` and `unknown_local_token_passes_through_unmapped` (plus 4 more) all present. |
| 5 | 0239 migration landed — `priority SMALLINT NOT NULL DEFAULT 10`, admin stamp 100, line 44 defers index change to B5-3 | ✅ Confirmed. `migrations/0239_audit_governance_outbox.sql:26` (`priority SMALLINT NOT NULL DEFAULT 10`), trigger INSERT stamps `class='admin', priority=100` for `message.moderated`; line 44: "B5-3 extends ORDER BY with priority; the index change is B5-3's, not this slice's." |
| 6 | Drill wired into `test-integration.sh:450-462` + 37-slot `b5-pin.sh`; slot currently fails red | ✅ Confirmed. `scripts/test-integration.sh:447-461` runs the drill on a throwaway DB (gated only on the 0239 file existing — it does) and emits `B5-CHECK moderation-priority-drill: PASS`. `scripts/b5-pin.sh` lists `moderation-priority-drill` among 37 slots (15 executed + 22 [PROPOSED]; count/format/dupe/vacuous guard at `assert_b5_contract_pin`). **Empirically proven red**: fresh throwaway DB migrated through 0239, ran `aero-audit-priority-drill` → exit 1, `Error: round 1: the moderation row was NOT claimed+delivered first — claim order must be priority DESC, not FIFO/enqueue order (B5-3 not landed?)`. |
| F1 | `pg.rs` test fixture lacks `priority` (pg.rs:242-263) | ✅ Confirmed. `ensure_outbox_table` (pg.rs:244-266) creates a minimal table with no `priority`/`class` columns. After R1's `ORDER BY candidate.priority DESC`, the existing `#[ignore]` test `concurrent_double_claim_across_two_sessions_is_impossible` would fail with `column "candidate.priority" does not exist` on non-0239 throwaway DBs. |
| F2 | Trait doc pins the old ordering (outbox.rs:29-31) | ✅ Confirmed. `OutboxRepo::claim_due` doc (outbox.rs:29-31): "ordered `(available_at, created_at, event_id)`". `FakeOutbox` has **no** `priority` field (grep: zero hits in fake.rs); its sort is `(available_at, created_at, id)` FIFO — fake-based suites (state_machine 6, claim_validation 11, relay 6, config 2 — counts verified) are untouched by an additive SQL-only change. |

**Oracle note (2026-08-08, decision D3):** evidence rows 3/6 quote the drill's
*original* asserts (`delivered_at == MIN(delivered_at)` in round 1,
strictly-first on full drain). The drill now asserts **batch membership**
(`moderation-in-first-batch: PASS`): `UPDATE … FROM (CTE ORDER BY …) …
RETURNING` emits target-table heap order, not CTE order, so delivery
*firstness* is an executor artifact, not a contract — batch membership is
the contract of a LIMIT'd claim (empirically reproduced; see §7 D3). The
verification record above stays accurate for the requirements doc as written
at design time.

**Fixture-constant divergence (observation, non-blocking, out of scope):** the
drill seeds its own `[PROPOSED]` values `BACKLOG_PRIORITY=100` /
`MODERATION_PRIORITY=200` (drill:34-44); the authoritative pair is
`GOVERNANCE_PRIORITY_BACKLOG=10` / `GOVERNANCE_PRIORITY_MODERATION=100`
(governance.rs:31-33, mirrored by 0239's DEFAULT and trigger stamp). The drill
asserts only *precedence* (moderation > backlog), which both pairs satisfy, so
R1 makes it pass regardless. Not aligning them is deliberate (R5, AC3).

## 1. Design overview

One behavioral change: the `claim_due` CTE in `PgOutboxRepo` gains a leading
`candidate.priority DESC` ORDER BY term so admin-class moderation rows
(priority 100, stamped by the 0239 trigger) are claimed ahead of the
priority-10 message/room backlog regardless of enqueue order. The claim
filter, fencing, lease minting, `FOR UPDATE SKIP LOCKED`, `LIMIT`, and
`RETURNING` are byte-identical. Two doc/fixture touch-ups keep the crate
self-consistent (R2 fixture column, R4 trait doc), one new `#[ignore]` PG
test pins the ordering (R3), and the acceptance oracle
(`aero-audit-priority-drill`) is the red→green signal (R5) — with its
round-1 assert corrected from delivery *firstness* to batch *membership*
(decision D3; already reflected in the drill). One additive migration (0240)
lands the due-priority index that 0239:44 recorded for B5-3. No new crate,
no trait signature change, no `FakeOutbox` change.

Direction invariant (the load-bearing pin): governance claim ordering is
**`priority DESC` — higher = claimed first** — the *inverse* of `ai_job`'s ASC
lower-first model (`aero_storage::ai_job::priority_for`). `ai_job` is the lane
*model*, not the numeric direction; "aligning" the two inverts the lane and is
guarded by governance.rs's unit tests, the new PG test, and the drill.

## 2. API changes

### 2.1 `PgOutboxRepo::claim_due` — the only behavioral change (`crates/aero-audit-connector/src/pg.rs`)

`ORDER BY` (currently pg.rs:87-88) becomes:

```sql
ORDER BY candidate.priority DESC, candidate.available_at,
         candidate.created_at, candidate.event_id
```

- `priority DESC` is the **leading, dominant term**: higher priority claims
  first regardless of `available_at` (the drill seeds moderation with a *later*
  `available_at` precisely to prove this).
- Terms 2-4 (`available_at, created_at, event_id`) are preserved byte-for-byte
  as the FIFO tie-break within equal priority. `event_id` as final term makes
  the order **total** — deterministic per claim batch, no arbitrary
  SKIP-LOCKED interleaving surprises.
- Everything else in the statement is untouched: `status IN (0, 1)` filter,
  `available_at`/`lease_expires_at` clock-domain filters, `FOR UPDATE SKIP
  LOCKED`, `LIMIT $1`, the UPDATE/RETURNING shape.

### 2.2 `OutboxRepo` trait (`crates/aero-audit-connector/src/outbox.rs`)

- **Signature: unchanged.** `claim_due(&self, lease, limit)` keeps its shape;
  no priority parameter is added. Priority lives in the row (column), not the
  call — every producer stamps it via the 0239 trigger or the column default.
- **Doc only (R4):** the `claim_due` doc (outbox.rs:29-31) records the PG
  ordering contract as `priority DESC, then (available_at, created_at,
  event_id)`. `FakeOutbox` is deliberately unchanged — its documented FIFO
  sort remains the fake's behavior (no priority field), which keeps every
  fake-based suite green.

### 2.3 Test fixture (`pg.rs` `ensure_outbox_table`, R2)

Minimal 0239-equivalent table gains `priority SMALLINT NOT NULL DEFAULT 10`
(mirroring 0239's column + `GOVERNANCE_PRIORITY_BACKLOG`). Existing seed
INSERTs (which omit `priority`) keep working via the DEFAULT — no seed
changes. When 0239 is migrated, the repo tests keep using the real table
as-is (the probe at pg.rs:245-253 already prefers it). `class` is **not**
added: no statement in `pg.rs` references it.

### 2.4 New ignored PG test (R3)

`mixed_priority_claim_orders_moderation_first_then_fifo` in pg.rs's test
module, `#[ignore = "requires live Postgres (DATABASE_URL)"]` per AGENTS.md
§4.1:

- seed N=40 backlog rows (priority 10, earlier `available_at`, with
  **`created_at` deliberately inverted** — some earlier-seeded rows carry a
  *later* `created_at` than later-seeded rows, so a swapped
  `created_at`/`available_at` ORDER BY term is detectable) and M=10 admin
  rows (priority 100, **later** `available_at` — the seed-order inversion
  that makes FIFO an impossible explanation), all `status 0`, due;
- claim `limit = 25 < N+M = 50` — **not** `N+M` in one batch: `UPDATE …
  FROM (CTE ORDER BY …) … RETURNING` emits rows in target-table heap order,
  not CTE order, so `Vec`-position assertions are join-order/plan-dependent
  (empirically reproduced; decision D3). The contract of an ORDER BY+LIMIT
  claim is the **claimed set**, deterministic per snapshot;
- assert the claimed **set** = {all 10 admin rows} ∪ {the 15 backlog rows
  with the earliest `(available_at, created_at, event_id)`} — set equality
  pins both the priority preemption and the within-lane FIFO *selection*
  plan-independently;
- teardown: DELETE the seeded event_ids (mirroring the existing test).

### 2.5 Explicitly NOT changed (frozen contracts)

- `crates/aero-ai/src/governance.rs` — already the DESC fixture authority.
- `aero-audit-priority-drill.rs` — the acceptance oracle; **already carries
  the membership correction** (round-1 assert is batch membership, not
  `delivered_at == MIN(...)`; D3) — no further edits.
- `fake.rs` — no priority field, FIFO documented behavior.
- `migrations/` — 0239 landed; **this slice lands `0240_…_due_prio_idx.sql`**
  (additive index, discharging 0239:44's recorded handoff — D1). 0239's own
  text is not edited post-apply (sqlx checksum validation — D2).
- `relay.rs` / `config.rs` / `client.rs` / `stub.rs` — untouched.

## 3. Compatibility constraints

1. **Additive SQL, safe on existing data.** 0239-migrated DBs already carry
   `priority`: existing rows have `DEFAULT 10` (backlog lane), moderation rows
   stamped 100 by the trigger. The new ORDER BY reads an existing NOT NULL
   column — no backfill, no data migration.
2. **Pre-0239 DBs degrade exactly as today.** The claim query would error on a
   table without the column — but that is the *current* behavior (the crate
   owns only statements; boot degrades to logged claim errors, F13-style, per
   pg.rs header). R2's fixture change is what keeps the ignored PG test
   runnable on non-0239 throwaway DBs.
3. **Fake-based suites untouched** (signature unchanged, fake has no priority):
   `tests/state_machine.rs` (6), `tests/claim_validation.rs` (11),
   `src/relay.rs` (6), `src/config.rs` (2) — verified counts.
4. **37-slot B5 pin unchanged in shape.** No slot added/removed/renamed; the
   `moderation-priority-drill` slot's verdict flips FAIL→PASS. The pin guard
   (`assert_b5_contract_pin`) still demands a `B5-CHECK` verdict line for
   every executed slot.
5. **Direction invariant is test-enforced, not comment-enforced.**
   `moderation_lane_preempts_backlog_under_desc_claim` (100 > 10 under DESC)
   + the new PG test + the drill all fail on an ASC "alignment" regression.
6. **Concurrency semantics preserved.** `FOR UPDATE SKIP LOCKED` and the
   two-session disjoint-claim property are untouched; priority only changes
   *which* rows a batch picks, not the fencing/lease/attempt mechanics.
7. **Due-priority index lands in this slice (0240).** The claim ORDER BY's
   leading term is now `priority DESC`; 0240 adds
   `audit_governance_due_prio_idx (priority DESC, available_at, created_at,
   event_id) WHERE status IN (0,1)` matching it exactly — LIMIT pushdown,
   no per-tick full Sort of the due set, only LIMIT rows locked (F10). The
   legacy `audit_governance_due_idx` is **retained** through the rolling
   deploy (old FIFO binaries keep a matching index; F9) and dropped only by
   the recorded cleanup (D5) — the same slice that updates the aero-storage
   schema-shape test (`audit_governance.rs`, pins `indexname =
   'audit_governance_due_idx'` + old column order), which stays green until
   then.
8. **Drill fixture constants (200/100) vs authoritative (100/10):** both
   satisfy precedence; the drill is the oracle *as-is* (R5). The requirements'
   "(priority 100 / priority-10 backlog)" wording maps to the authoritative
   constants that production rows carry.
9. **Rolling interleaving is safe (F9).** During a rolling deploy, old FIFO
   binaries and new priority binaries claim the same table; both orderings
   are valid SKIP LOCKED selections of the same due set, batches stay
   disjoint per tick, and settle/fence/at-least-once semantics are
   identical — a due moderation row can wait at most one old-binary batch
   (bounded, never lost), and the preemption guarantee is monotone in the
   new-replica fraction (§5.2 P2).

## 4. Failure modes

| # | Mode | Detection | Behavior / mitigation |
|---|---|---|---|
| F1 | Claim query hits a table without `priority` (pre-0239 DB) | SQL error on every claim | Unchanged from today: connector degrades to logged claim errors (pg.rs header F13); drill exits 2 SKIP. R2 fixture keeps the ignored test runnable on such DBs. |
| F2 | ASC regression ("align" with `ai_job`) | Drill fails red with `moderation row was NOT claimed+delivered within the first batch`; governance unit test + new PG test fail | Moderation rows drain last — the exact hazard the direction pins against. Guards: DESC pin tests, drill (AC1/AC4). |
| F3 | `priority` NULL | Impossible: NOT NULL in 0239 and in the R2 fixture; no code path inserts without the column/default | No runtime branch needed. |
| F4 | Lane constants drift (e.g. BACKLOG raised ≥ MODERATION) | governance unit test `moderation_lane_preempts_backlog_under_desc_claim` fails | Precedence pin (100 > 10) is the invariant; absolute values are fixtures. |
| F5 | Stale/absent due-priority index plan on large outbox | EXPLAIN runbook check + due-set gauge (§5.3) — the drill cannot see plans at 501 rows | Mitigated in this slice: 0240 lands the matching index (D1). If it is dropped or the planner picks the legacy index, per-claim cost becomes O(due-set) Sort under backlog — monitored, not silent (§5.3 stale-index path). |
| F6 | Continuous moderation starves backlog | Not detectable by this slice's tests | Out of scope by design: anti-starvation cap is governance-lane design :181's named follow-up (§7). |
| F7 | Concurrent claimers (multi-instance) | Two-session PG test stays green | SKIP LOCKED + total order (priority, available_at, created_at, event_id) keep batches disjoint and deterministic. |
| F8 | Replay / at-least-once redelivery of a settled moderation row | Drill's parity + membership asserts | Ordering affects *claim selection* only; settle/dead/fence semantics untouched (AC3). |
| F9 | **Rolling interleaving**: old FIFO binaries + new priority binaries claim the same table during a rolling deploy | No CI signal (suites run single-binary); rollout gate = §5.2 spec + canary verification | Safe by construction: both orderings are valid SKIP LOCKED selections of the same due set; batches disjoint per tick; settle/fence/at-least-once identical; a due moderation row is delayed at most one old-binary batch (bounded, never lost); preemption guarantee monotone in the new-replica fraction (§5.2 P2). |
| F10 | **Dual partial indexes** coexist (0240 + legacy due_idx) | `pg_indexes` presence check; EXPLAIN plan check (§5.3) | Correctness-neutral: each ORDER BY shape is served by its matching partial index; write amplification negligible on a budget-bounded moderation-only table; legacy index dropped only by the recorded cleanup (D5), same slice as the aero-storage shape-test update. |

## 5. Migration & rollout

### 5.1 Migration — 0240 lands the due-priority index (discharges 0239:44)

One additive migration in this slice: `migrations/0240_audit_governance_due_prio_idx.sql`
(`CREATE INDEX IF NOT EXISTS audit_governance_due_prio_idx ON
audit_governance_outbox (priority DESC, available_at, created_at, event_id)
WHERE status IN (0, 1)`). 0239's DEFAULT 10 and trigger stamp 100 remain the
data this change consumes. 0239's own text is **not** edited post-apply:
sqlx's embedded Migrator checksums applied migrations against
`_sqlx_migrations`, so any edit to a landed file fails `aero-cli migrate` on
every already-migrated DB — the discharge is recorded in 0240's header
instead (decision D2). `make migrate-smoke` replays the full chain including
0240 on a fresh DB (the file is self-contained).

Steps (AGENTS.md §4.2 order — build before migrate):

1. **Implement R1-R4** in `crates/aero-audit-connector/src/pg.rs` +
   `outbox.rs` (R1+R2+R3 in pg.rs, R4 in outbox.rs) and add
   `migrations/0240_audit_governance_due_prio_idx.sql`. The drill already
   carries the membership oracle correction (D3) — no further edits.
2. **`cargo build`** — per AGENTS.md §4.2 the migration set is compiled into
   bins; 0240 is inert until a rebuilt bin runs `aero-cli migrate`. (Gotcha
   observed during verification: `aero-cli migrate` reads **config.toml**,
   not `DATABASE_URL`; the throwaway-DB invocation is
   `AERO__DATABASE__URL=<url> aero-cli migrate` — the
   `test-integration.sh:451-453` pattern.)
3. **Throwaway-DB verification** (fresh `CREATE DATABASE` → migrate → run →
   `DROP DATABASE`, AGENTS.md §4.3):
   - `AERO__DATABASE__URL=<throwaway> ./target/debug/aero-cli migrate`
     (`ls migrations/*.sql | wc -l` ticks up by one; `audit_governance_outbox`
     present with `priority`/`class`; `pg_indexes` shows **both** partial
     indexes);
   - `DATABASE_URL=<throwaway> cargo run -p aero-audit-connector --bin
     aero-audit-priority-drill` → exits 0 (`moderation-in-first-batch`,
     `drain-501`, `parity-501` PASS; was exit 1 pre-change);
   - `DATABASE_URL=<throwaway> cargo test -p aero-audit-connector --lib
     -- --ignored concurrent_double_claim…` and the new R3 test;
   - aero-storage's schema-shape test (`audit_governance.rs`, pins the
     legacy `audit_governance_due_idx` shape) stays green — both indexes
     coexist (AC5).
4. **Full gates** (AGENTS.md §4.3): `cargo check --workspace` · `cargo test
   --workspace --lib` · `cargo clippy --workspace --all-targets` (no new
   warnings; `priority` is i16 in governance.rs but the ORDER BY is SQL, so no
   type/const lint surface) · `scripts/{truth-check,file-size-check,web-check}.sh`.
5. **Pin slot flips green**: `scripts/test-integration.sh` (or the
   `moderation-priority-drill` segment alone) emits `B5-CHECK
   moderation-priority-drill: PASS`; `scripts/test-b5-pin-guard.sh` still
   passes 37/37.

### 5.2 Dual-partial-index rolling deploy

The relay is a per-instance loop (`aero-server/src/bin/main.rs:251-259`,
multi-replica), so binaries roll incrementally while the schema is already
migrated. 0239's legacy index is kept through the roll so **each ORDER BY
shape is served by a matching partial index**:

| Phase | Action | Index serving claims | Guarantee |
|---|---|---|---|
| P0 (pre) | Old binaries + 0239 only | legacy `due_idx` (FIFO ORDER BY) | status quo |
| P1 (migrate) | `aero-cli migrate` applies 0240; **no code change** | old binaries still use legacy (their ORDER BY matches it); new index unused | zero behavior change; additive + idempotent; never rolled back (harmless while unused) |
| P2 (binary roll) | Deploy rebuilt binaries incrementally (canary → full); old FIFO + new priority replicas interleave | each binary's ORDER BY served by its matching partial index | **F9**: both orderings are valid SKIP LOCKED selections of the same due set; batches disjoint per tick; settle/fence/at-least-once unchanged; no duplicates, no loss. A due moderation row can wait at most one old-binary batch (≤ batch_size / drain rate), never more; preemption guarantee monotone in the new-replica fraction |
| P3 (verify) | On a throwaway DB built from the deployed tag: drill 3× PASS lines; `pg_indexes` shows both indexes; EXPLAIN (§5.3) shows the prio index serving the new ORDER BY (legacy index may still serve old replicas — expected) | both | gates green before full cutover |
| P4 (cleanup, later slice — D5) | Drop legacy `due_idx` in a follow-up migration ≥ one full lease + retention window after P2 completes; update the aero-storage schema-shape test in the same slice | only prio index | after this point, rollback would require re-adding the legacy index; before it, rollback is binary-only |

**Rollback** — any time before P4: revert binaries → old FIFO ordering; the
legacy index is still present and its plan is unchanged; zero data steps, no
migration rollback (0240 is additive and remains harmless while unused).

### 5.3 Operational visibility

**Status-3 dead governance rows** — the relay's `mark_dead` terminal
(`relay.rs`: permanent-class 422/409/receipt-mismatch/payload-guard at
attempt ≥ 2; HTTP 403 immediately at attempt 1) is **terminal**: a dead row
means the sink *permanently* rejected a delivery (misconfiguration/binding/
secret), not a transient outage. Unlike `ai_jobs` — which has an admin DLQ
surface (`GET/POST /api/workspaces/:ws/admin/ai/dlq[…/requeue]`,
`server/src/ai_dlq.rs`) — the governance outbox has **no admin DLQ API**;
dead rows are visible only via DB. Documented runbook:

- **Gauges** (spec; the sampler lands in aero-server's `metrics_tasks.rs` —
  clone of the AI-DLQ sampler pattern at :135-165, 30s interval, no state,
  warn-and-keep-old-value on query Err): `audit_governance_outbox_dead_rows`
  (COUNT status=3) and `audit_governance_outbox_due_rows` (COUNT status
  IN (0,1) AND available_at <= clock_timestamp() — the due-set size that
  the stale-index (F5) and starvation (F6) paths correlate on).
- **Alert**: dead count > 0 on ≥ 2 consecutive samples, or oldest dead row
  age > 1h → page ops (sink configuration/binding review). Due-set > 10k
  sustained → run the EXPLAIN check below (O(due-set) Sort becomes
  noticeable at this scale).
- **Manual requeue (no API today)**:
  `UPDATE audit_governance_outbox SET status = 0, attempts = 0,
  claim_token = NULL, lease_expires_at = NULL, last_error = NULL,
  available_at = clock_timestamp() WHERE event_id = $1 AND status = 3;`
  — resets the permanent-class budget (≤1 retry again) and re-enters the
  priority lane. Idempotent; the audit event stays 1:1 (event_id is the
  sink's Idempotency-Key, so redelivery cannot duplicate at the sink).
- **Follow-up option (not this slice)**: an admin DLQ route for governance
  mirroring `ai_dlq` (list + requeue, workspace-scoped), owner B5-1 storage.

**Stale-index monitoring** — with 0240 landed, the risk is not absence but
*drift*: the legacy index cannot serve the new ORDER BY (a full Sort above
the plan), and the drill cannot see plans at 501 rows (F5). Path:

- **Presence**: `SELECT indexdef FROM pg_indexes WHERE tablename =
  'audit_governance_outbox' AND indexname = 'audit_governance_due_prio_idx'`
  — expected shape `(priority DESC, available_at, created_at, event_id)`
  with the partial predicate over status 0/1 (mirror of the aero-storage
  shape test).
- **Plan**: `EXPLAIN (FORMAT JSON)` on the claim CTE's SELECT (the
  `claimable` subquery of pg.rs `claim_due`, parameterized) — expect an
  Index Scan on `audit_governance_due_prio_idx` and **no Sort node**. Run in
  staging/ops on a representative due-set; a Sort node = drift alert.
- **Correlation**: due-set gauge × plan check — a growing due set with a
  Sort node is the F5/F6 scenario made observable.

### 5.4 Rollout summary

`cargo build` → throwaway-DB drill + PG gates (AC1-AC5) → full workspace
gates → `aero-cli migrate` on prod (P1, additive) → canary binary roll (P2,
F9) → verify (P3) → cleanup slice drops the legacy index (P4, D5). No
downtime; rollback is binary-only until P4.

## 6. Testable acceptance mapping

| AC | Requirement | Executable proof |
|---|---|---|
| AC1 | Priority drill passes (oracle) | `DATABASE_URL=<throwaway, migrated through 0239+0240> cargo run -p aero-audit-connector --bin aero-audit-priority-drill` exits 0 with `drill: moderation-in-first-batch: PASS`, `drain-501: PASS`, `parity-501: PASS`. The oracle is **batch membership** — the moderation row ∈ the round-1 delivered set of 100, the contract of an ORDER BY+LIMIT claim (delivery *firstness* is an executor artifact, D3). Pre-state proven red (exit 1, `moderation row was NOT claimed+delivered within the first batch`). In `test-integration.sh`, `B5-CHECK moderation-priority-drill: PASS`. |
| AC2 | Mixed-priority PG test: admin rows first, FIFO within lane | `DATABASE_URL=<throwaway> cargo test -p aero-audit-connector --lib -- --ignored mixed_priority_claim_orders_moderation_first_then_fifo` passes **with and without** 0239/0240 migration (R2 fixture covers the non-0239 shape). Set-based: claim `limit 25 < 50`; the claimed set = {10 admin} ∪ {15 earliest-`available_at` backlog} — plan-independent (D3). Seed inversion + inverted `created_at` make FIFO and column-swap regressions impossible explanations. |
| AC3 | No regression on connector suites + pin | `cargo test -p aero-audit-connector` green: state_machine (6), claim_validation (11), relay (6), config (2), plus `concurrent_double_claim_across_two_sessions_is_impossible` (25/25 disjoint, attempts==1, 50 distinct tokens) under DATABASE_URL. `scripts/test-b5-pin-guard.sh` passes 37/37 (no slot removed/renamed; `moderation-priority-drill` verdict now PASS). |
| AC4 | Lane direction invariant holds | `cargo test -p aero-ai --lib` keeps `moderation_lane_preempts_backlog_under_desc_claim` green (100 > 10 under DESC) and `admin_class_rows_never_aggregated` green. Source check: `claim_due` ORDER BY contains `priority DESC`, no ASC priority term, no `priority_for`-style alignment; governance.rs "do not align with `ai_job`" warning intact. |
| AC5 | Index gate (0240) | On the same throwaway DB: `pg_indexes` shows both `audit_governance_due_prio_idx` (new shape, partial over status 0/1) and `audit_governance_due_idx` (legacy); aero-storage's schema-shape test (`audit_governance.rs`) stays green; `make migrate-smoke` replays 0240 on a fresh DB. Plan-shape (EXPLAIN) verification is the §5.3 runbook check, not a drill assert (F5). |

Pre-merge checklist (AGENTS.md §4.3): `cargo check --workspace` clean ·
`cargo test --workspace --lib` green · clippy no new warnings ·
`scripts/{truth-check,file-size-check}.sh` 0 violations.

## 7. Decision log & open items

### 7.0 Decisions recorded this slice (2026-08-08 deployment review)

| # | Decision | Choice | Rationale / rejected alternative |
|---|---|---|---|
| D1 | Land `audit_governance_due_prio_idx` in this slice (0240) vs defer with a trigger | **Land** | 0239:44 assigns the index change to B5-3 — this slice IS B5-3; deferring silently breaks the recorded cross-slice contract. Without it, per-claim cost under sustained backlog is O(due-set) Sort + heap fetch per candidate (the F5/F6 scenario). Additive + cheap (budget-bounded, moderation-only table). *Rejected alternative (recorded as the would-be trigger had we deferred): due-set size > 10k sustained, or sink outage > 30 min sustained → index follow-up required; owner B5-1 storage.* |
| D2 | Amend 0239's line-44 comment vs record the discharge in 0240 | **Do not edit 0239** | sqlx's embedded Migrator checksums applied migrations (`_sqlx_migrations`); editing 0239 post-apply fails `aero-cli migrate` on every already-migrated DB (ChecksumMismatch). 0240's header records the discharge; 0239:44 stays as the historical handoff. |
| D3 | Oracle firstness asserts vs batch-membership/set asserts | **Set-based** | Empirically reproduced: `UPDATE … FROM (CTE ORDER BY …) … RETURNING` emits target-table heap order, not CTE order — post-change the as-written drill exits 1 (`moderation delivered_at != MIN(delivered_at)`), i.e. the firstness asserts are unsatisfiable and the oracle could never go green. The contract of ORDER BY+LIMIT is the claimed **set** = top-LIMIT of the total order, deterministic per snapshot. Applied to the drill (already corrected in-tree: `moderation-in-first-batch`) and the R3 PG test (limit 25 < 50, set equality; seed `created_at` inverted so a column swap is detectable). |
| D4 | Anti-starvation cap: open item vs scheduled deliverable | **Scheduled deliverable D-CAP** | See §7.1. |
| D5 | Legacy `due_idx` cleanup | **Later slice (P4)** | Drop only after P2 completes + ≥ one full lease + retention window (no old binary can return); same slice updates the aero-storage schema-shape test. Owner: B5-1 storage. Until then both partial indexes coexist (F10). |
| D6 | Status-3 dead-row visibility | **Document + gauge spec; no admin API this slice** | `ai_jobs` has an admin DLQ surface; the governance outbox has none — dead rows are DB-only today. This slice documents the runbook (gauges, alert, manual requeue SQL); an admin DLQ route mirroring `ai_dlq` is a recorded follow-up option. |

### 7.1 D-CAP — anti-starvation cap (scheduled, drill-verified)

- **Status**: scheduled deliverable — converted from an open item (was F6 /
  `docs/design/2026-08-06-aero-ai-b5-1-governance-lane-design.md:181`).
- **Trigger condition (the gate)**: the first slice that populates the
  priority-10 lane — **L1 aggregation** ([PROPOSED], B5-1 design §5 step 4).
  D-CAP must land **in the same slice as L1**, not after: once the low lane
  is populated, sustained moderation ≥ drain rate against a degraded sink
  can starve backlog indefinitely (strict preemption is the feature;
  unbounded starvation is the hazard). L1's acceptance must carry a check
  item "D-CAP lands here" (mirror of how the A3/A4 deferred drills gate
  later slices).
- **Owner**: B5-1/B5-3 (aero-storage + aero-audit-connector claim path).
- **Mechanism (preferred, drill-compatible)**: two-arm claim with backfill
  in one statement — arm A claims the top `batch − K` rows of the total
  order (priority DESC, then FIFO); arm B claims the `K` earliest-due rows
  of the **lowest-priority lane** (today: the priority-10 backlog) **not
  already selected by arm A** (explicit exclusion — SKIP LOCKED alone does
  not dedupe arms within one statement); both arms `FOR UPDATE SKIP LOCKED`.
  K = per-tick minimum service floor, default `K = max(1, batch/20)` (= 5
  at batch 100), config-visible constant. Within-lane FIFO preserved;
  settle/fence/attempts semantics unchanged. When the high lane is
  underfull, arm B's backfill makes the claimed set **identical to the
  uncapped top-batch** — the existing drill (1 moderation + 99 backlog in
  round 1) stays green unchanged.
- **Verification (drill-verified)**: (a) existing `aero-audit-priority-drill`
  stays green (regression leg, membership oracle); (b) new sibling drill
  `aero-audit-min-service-drill`: seed the high lane ≥ batch sustained +
  backlog; assert every tick claims exactly K backlog rows until the high
  lane drains, and backlog drains at ≥ K/tick; (c) PG test: sustained mixed
  lanes, batch 100, each batch contains exactly K backlog rows.
- **Why not now**: the priority-10 lane has **zero producers** today (0239's
  trigger maps only `message.moderated`; every other token passes through)
  and moderation production is budget-bounded (≤ ~60 finalizes/min) against
  a healthy sink — starvation is latent, not live; it requires the high
  lane's due set ≥ batch sustained against a degraded sink. Landing the cap
  now would change claim behavior with no population to protect and no drill
  to verify it.

### 7.2 Remaining open items (recorded, not this slice)

- **Admin DLQ surface for governance dead rows** (D6 follow-up option):
  workspace-scoped list + requeue mirroring `ai_dlq`; until then §5.3's
  runbook (gauges + manual SQL) is the visibility path.
- **Drill fixture constants 200/100 vs authoritative 10/100** — **CLOSED
  (2026-08-08 P1 fold)**: the drill now seeds the authoritative production
  values (`BACKLOG_PRIORITY=10` / `MODERATION_PRIORITY=100`, matching
  `governance.rs` and the 0239 DEFAULT/trigger stamp), so the oracle
  exercises production values verbatim.
- **`delivery_mode` column** (`0239:27`, reserved, no consumer yet) — B5-3
  delivery-policy follow-up.
- **Evidence line-number drift** (non-blocking): cited `pg.rs:90-93` is
  actually 87-88; `test-integration.sh:450-462` is 447-461. Substance of every
  citation verified regardless.

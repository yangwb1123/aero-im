# B5-3 — Moderation-first claim ordering (`priority DESC` in `PgOutboxRepo::claim_due`)

> Module: `crates/aero-ai` (fixture authority: `governance.rs`) with the physical
> change in `crates/aero-audit-connector` (`pg.rs` claim query).
> Source analysis: `docs/auto/analyses/crates-aero-ai-f8cd3622.json` (direction 2).
> Status: source implementation complete; live Postgres/drill acceptance remains
> environment-gated.

> Current-source note (2026-08-19): `PgOutboxRepo::claim_due` orders by
> `priority DESC` with the FIFO tie-break and applies the landed D-CAP floor;
> migration `0240_audit_governance_due_prio_idx.sql` matches that order. The
> mixed-priority PG test and priority drill are present; only a throwaway
> Postgres run is unavailable in this workspace.
>
> **Current B5 pin (2026-08-26):** `scripts/b5-pin.sh` pins 48 slots:
> 27 non-`[PROPOSED]` executable slots and 21 `[PROPOSED]` out-of-repo
> placeholders. “Executed” here is the manifest classification, not a claim
> that all 27 slots ran in this review.

## 0. Amendment — 2026-08-08 (deployment/rollout review)

The B5-3 design doc's deployment review amended three contracts of this
spec; this block keeps the requirement record authoritative (design
decisions D1-D3, `docs/design/2026-08-07-aero-ai-b5-3-priority-claim-ordering.design.md` §7.0):

- **A1 — the due-index change is now IN scope.**
  `migrations/0240_audit_governance_due_prio_idx.sql` lands
  `audit_governance_due_prio_idx (priority DESC, available_at, created_at,
  event_id) WHERE status IN (0, 1)`, discharging the handoff 0239:44
  recorded for B5-3 (D1). 0239's own text is **not** edited post-apply
  (sqlx checksum validation, D2); the discharge is recorded in 0240's
  header. Supersedes §1's first "out of scope" bullet and §5's
  `migrations/ — No change` row; the change surface gains the migration
  file (and the design doc §5.2 adds the dual-partial-index rolling
  deploy).
- **A2 — R5 amended: oracle asserts corrected, fixtures untouched.** The
  drill's round-1 `delivered_at == MIN(delivered_at)` and full-drain
  `strictly-first` asserts are unsatisfiable — `UPDATE … FROM (CTE ORDER BY
  …) … RETURNING` emits target-table heap order, not CTE order (empirically
  reproduced; D3). The oracle contract is **batch membership**: round-1
  delivered set = {moderation} ∪ {first 99 seeded backlog rows} (the
  top-100 of the total order). PASS lines are `moderation-in-first-batch`,
  `drain-501`, `parity-501` (already reflected in the drill). R5 becomes
  "no seed/constant changes; asserts corrected per D3". AC1's observable
  properties below are superseded accordingly.
- **A3 — R3/AC2 assert the claimed set, not Vec position.** Claim `limit
  25 < 50`; asserted set = {10 admin} ∪ {15 earliest-`available_at`
  backlog} — pins preemption and within-lane FIFO selection
  plan-independently; seed `created_at` inverted so a column swap is
  detectable.

## 1. Scope

Add `priority DESC` as the leading term of `PgOutboxRepo::claim_due`'s `ORDER BY`
so that admin-class moderation rows (priority 100, stamped by the 0239
trigger) are claimed before the priority-10 message/room backlog regardless of
enqueue order. The acceptance oracle (`aero-audit-priority-drill`) already
exists and currently fails red; 0239 has since landed, so the drill runs and
is the honest signal for this change.

**Explicitly out of scope** (recorded, not required by this direction):
- the 0239 due-index change (`audit_governance_due_idx` shape) — flagged as
  "B5-3's, not this slice's" in `migrations/0239_audit_governance_outbox.sql:44`;
  no acceptance check depends on it (drill volume is 501 rows). **Superseded
  by amendment §0-A1: the due-priority index lands via 0240 in this slice**;
- the anti-starvation cap named by
  `docs/design/2026-08-06-aero-ai-b5-1-governance-lane-design.md:181` — belongs
  to the broader B5-3 design, absent from this direction's acceptance;
- any change to the drill's fixture constants or to `FakeOutbox` (fake has no
  `priority` field; fake-based suites are untouched by an additive SQL change);
- L1 aggregation (B5-1, separate direction).

## 2. Evidence verification (all citations re-checked against the repo)

| # | Cited evidence | Verified result |
|---|---|---|
| E1 | `crates/aero-audit-connector/src/pg.rs:90-93` — `claim_due` orders `ORDER BY candidate.available_at, candidate.created_at, candidate.event_id`, no priority term | ✅ Confirmed (pg.rs:88-93). No `priority`/`class` reference anywhere in the claim CTE; the CTE's `candidate` row is available to sort on. |
| E2 | `crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs:29-33` — "0239 landing *without* B5-3's priority ordering is NOT a SKIP: this drill then FAILS red" | ✅ Confirmed. Doc comment also states the drill exits 2 SKIP only when the 0239 table or its `priority`/`class` columns are absent. |
| E3 | drill seeds backlog first (earlier `available_at`), moderation last; asserts moderation `delivered_at == MIN(delivered_at)` in round 1 and strictly-before in full drain | ✅ Confirmed (drill:140-182 seed; 190-230 round-1 asserts; 260-300 drain asserts). `BACKLOG_ROWS=500`, `MODERATION_ROWS=1`, `BATCH_SIZE=100`, `concurrency=1` (serial settle ⇒ settle order == claim order). |
| E4 | `crates/aero-ai/src/governance.rs:25-38` — DESC precedence pin, "do not align with `ai_job`" | ✅ Confirmed (governance.rs:11-25 module doc + 26-33 constants). `GOVERNANCE_PRIORITY_MODERATION: i16 = 100`, `GOVERNANCE_PRIORITY_BACKLOG: i16 = 10`; warning text: "Do not 'fix' the direction to match `ai_job`". |
| E5 | governance.rs:95-108 — `moderation_lane_preempts_backlog_under_desc_claim` | ✅ Confirmed. Asserts `lane.priority > GOVERNANCE_PRIORITY_BACKLOG` and pins DESC semantics. |
| E6 | governance.rs:201-218 — `admin_class_rows_never_aggregated` | ✅ Confirmed. `is_admin_class("message.moderated") == true`; message/room classes are the aggregatable population. |
| E7 | 0239 DDL (analysis listed its absence as a gap) | ✅ **Landed since the analysis**: `migrations/0239_audit_governance_outbox.sql` exists with `priority SMALLINT NOT NULL DEFAULT 10` (≡ `GOVERNANCE_PRIORITY_BACKLOG`), `class TEXT NOT NULL DEFAULT 'message'`, trigger stamping `class='admin'`, `priority=100` for `message.moderated`. Line 44: "B5-3 extends ORDER BY with priority; the index change is B5-3's, not this slice's." |
| E8 | Drill integration slot | ✅ Confirmed: `scripts/test-integration.sh:450-462` runs the drill (gated only on the 0239 file existing — it does), and `scripts/b5-pin.sh` lists `moderation-priority-drill` as one of the 48 contract slots (27 executable + 21 [PROPOSED]). The current slot is executable; its live verdict remains environment-gated. |
| E9 | `pg.rs` test fixture minimal table | ⚠️ **Additional finding (must be handled)**: `ensure_outbox_table` (pg.rs:242-263) creates a minimal table **without** `priority`/`class` columns. After R1, the existing `#[ignore]` PG test `concurrent_double_claim_across_two_sessions_is_impossible` would hit `column "candidate.priority" does not exist` on non-0239 databases. The fixture must gain the `priority` column. |
| E10 | `outbox.rs` trait doc ordering contract | ⚠️ **Additional finding**: `OutboxRepo::claim_due` doc (outbox.rs:29-31) pins ordering "`(available_at, created_at, event_id)`" — stale once R1 lands; must be updated for the PG impl (priority DESC, then FIFO). |

**Fixture-constant divergence (observation, non-blocking, out of scope):** the
drill seeds its own `[PROPOSED]` fixture values `BACKLOG_PRIORITY=100` /
`MODERATION_PRIORITY=200` (drill:34-44), while the authoritative pins are
`GOVERNANCE_PRIORITY_BACKLOG=10` / `GOVERNANCE_PRIORITY_MODERATION=100`
(governance.rs:31-33, mirrored by 0239's DEFAULT and stamp). The drill is
self-consistent (moderation > backlog), so it passes under R1 regardless; the
supplied acceptance's "(priority 100 / priority-10 backlog)" wording maps to
the authoritative constants, not the drill's fixture literals. Aligning the
drill constants is deliberately not required here.

## 3. Requirements

### R1 — `claim_due` orders by `priority DESC` first, FIFO tie-break preserved

`PgOutboxRepo::claim_due` (`crates/aero-audit-connector/src/pg.rs`) must order
the claim CTE by:

```sql
ORDER BY candidate.priority DESC, candidate.available_at,
         candidate.created_at, candidate.event_id
```

- `priority DESC` is the **leading, dominant term** — higher priority is
  claimed first regardless of `available_at` (the drill seeds moderation with a
  *later* `available_at` specifically to prove this).
- Within equal priority, the existing FIFO tie-break
  `(available_at, created_at, event_id)` is preserved byte-for-byte as
  terms 2-4 (drill backlog rows share priority and drain FIFO).
- Direction invariant: **never** ASC, and never "aligned" to `ai_job`'s
  ASC lower-first model (governance.rs:11-25 warning). `ai_job::priority_for`
  is the lane *model*, not the numeric direction.
- No other statement in `pg.rs` changes: filters, `FOR UPDATE SKIP LOCKED`,
  `LIMIT`, lease minting on `clock_timestamp()`, `RETURNING` columns, and the
  `status IN (0, 1)` claimability filter are untouched.

### R2 — PG test fixture gains the `priority` column

`ensure_outbox_table` (`pg.rs:242-263`) minimal shape must add
`priority SMALLINT NOT NULL DEFAULT 10` (mirroring 0239's column) so the
existing `#[ignore]` PG tests run on throwaway DBs that have not been migrated
through 0239. The existing seed INSERTs (which omit `priority`) keep working
via the DEFAULT — no seed changes. When the 0239 table exists, the repo tests
keep using it as-is.

### R3 — Mixed-priority claim-ordering unit test

Add one `#[ignore]` + `DATABASE_URL`-gated PG test (AGENTS.md §4.1 db_test
convention) in `pg.rs`'s test module that:
- seeds N backlog rows (priority 10, earlier `available_at`) and M admin-class
  rows (priority 100, later `available_at`), all status 0 and due;
- claims a batch of N+M with `claim_due`;
- asserts every admin-class row sorts before every backlog row in the returned
  `Vec<Claim>` order;
- asserts FIFO tie-break: within equal priority, returned order is
  `(available_at, created_at, event_id)` ascending;
- cleans up its seeded rows (mirroring the existing test's teardown).

### R4 — Trait ordering contract updated

`OutboxRepo::claim_due` doc (`crates/aero-audit-connector/src/outbox.rs:29-31`)
must record the PG ordering as `priority DESC, then (available_at, created_at,
event_id)`. The `FakeOutbox` implementation is **not** changed (no `priority`
field; its FIFO sort remains the fake's documented behavior).

### R5 — No drill changes

`aero-audit-priority-drill.rs` is the acceptance oracle as-is; no edits to its
seed logic or constants, and (per amendment §0-A2) its round-1 assert is
batch membership, not delivery firstness.

## 4. Acceptance criteria (testable)

> Commands assume a throwaway Postgres (`CREATE DATABASE` → `aero-cli migrate`
> → run → `DROP DATABASE`, per AGENTS.md §4.3). The drill's fixture priorities
> are 200 (moderation) / 100 (backlog); the authoritative 100/10 pair from
> governance.rs/0239 is what production rows carry — the property asserted is
> *precedence* (moderation > backlog), which both pairs satisfy.

**AC1 — Priority drill passes (acceptance oracle).**
`DATABASE_URL=<throwaway migrated through 0239> cargo run -p aero-audit-connector --bin aero-audit-priority-drill`
exits 0 and prints `drill: moderation-first: PASS` + `drill: drain-501: PASS` +
`drill: parity-501: PASS` + `drill: moderation-strictly-first: PASS`.
Equivalent observable properties: the moderation row (priority 200, `class='admin'`,
seeded **last** with the latest `available_at`) is claimed and settled to
`status = 2` in round 1 (batch 100 of 501), with `delivered_at ==
MIN(delivered_at)` over all `status = 2` rows; after full drain, all 501 rows
are `status = 2` with event-id set-parity and the moderation row's
`delivered_at` strictly before every backlog row's. Today (pre-change) this
fails: round 1 contains only backlog rows and the drill bails
(`moderation row was NOT claimed+delivered first`). In `scripts/test-integration.sh`
the `moderation-priority-drill` slot flips from FAIL to `B5-CHECK …: PASS`.

**AC2 — PG unit test: mixed-priority batch returns admin rows first, FIFO within equal priority.**
The new R3 test passes against a throwaway PG (with and without the 0239
migration — the R2 fixture covers the non-0239 shape). Seed-order inversion is
proven: admin rows seeded *after* backlog rows are still returned first;
same-priority rows return in `(available_at, created_at, event_id)` order.

**AC3 — No regression on the connector suites.**
`cargo test -p aero-audit-connector` stays green: `tests/state_machine.rs` (6
tests, FakeOutbox), `tests/claim_validation.rs` (11 tests), `src/relay.rs`
(6 unit tests), `src/config.rs` (2 unit tests), plus the ignored PG test
under `DATABASE_URL` (its fixture gained the column via R2; its seeds are
unchanged and still claim 25/25 disjoint with `attempts == 1`). The 48-slot B5 pin guard (`scripts/b5-pin.sh`, `scripts/test-b5-pin-guard.sh`)
still passes — `moderation-priority-drill` remains pinned; no slot is removed
or renamed. The ordering change is additive: filters, fencing,
backoff, dead semantics, and the fake-based suites are untouched.

**AC4 — Lane direction invariant holds.**
`cargo test -p aero-ai --lib` keeps `moderation_lane_preempts_backlog_under_desc_claim`
green (`100 > 10` under DESC); `claim_due`'s ORDER BY contains `priority DESC`
and no ASC priority term; the governance.rs "do not align with `ai_job`"
warning stays in place. `is_admin_class` remains the classification authority:
moderation rows stay 1:1 (`event_id = audit_events.id`) in the claim path —
no aggregation is introduced by this change (`admin_class_rows_never_aggregated`
stays green).

**Cross-checks required before merge** (AGENTS.md §4.3): `cargo check
--workspace` clean · `cargo test --workspace --lib` green · `cargo clippy
--workspace --all-targets` no new warnings · `scripts/truth-check.sh` and
`scripts/file-size-check.sh` clean.

## 5. Change surface (complete list)

| File | Change |
|---|---|
| `crates/aero-audit-connector/src/pg.rs` | R1: `ORDER BY` gains `candidate.priority DESC` as leading term; R2: fixture table gains `priority` column; R3: new ignored mixed-priority test |
| `crates/aero-audit-connector/src/outbox.rs` | R4: `claim_due` doc ordering contract |
| `crates/aero-ai/src/governance.rs` | **No change** (already the DESC fixture authority) |
| `crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs` | **No change** (acceptance oracle) |
| `crates/aero-audit-connector/src/fake.rs` | **No change** (no priority field) |
| `migrations/` | **Amendment §0-A1: `0240_audit_governance_due_prio_idx.sql` lands the due-priority index** (0239 stays untouched) |

## 6. Risks and notes

- **Stale trait doc** (E10): if R4 is skipped, the `OutboxRepo` doc and the PG
  implementation disagree; cheap to fix in the same edit as R1.
- **Fixture-constant divergence**: the drill's 200/100 vs the authoritative
  10/100 is a readability wart only; both satisfy precedence. If the campaign
  later wants the drill to exercise the production constants verbatim, that is
  a separate one-constant edit (drill:34-44) — not required for any AC here.
- **Index**: `audit_governance_due_idx (available_at, created_at, event_id)
  WHERE status IN (0,1)` no longer matches the ORDER BY's leading term; the
  0239 comment already assigns the index change to B5-3. Correctness is
  unaffected (Postgres sorts; drill volume is 501 rows). A follow-up owning
  the index + anti-starvation cap is recorded in
  `docs/design/2026-08-06-aero-ai-b5-1-governance-lane-design.md:181,198`.
- **Current state is red, intentionally**: with 0239 landed and the drill
  wired into `test-integration.sh`, the `moderation-priority-drill` slot fails
  today — the G6-honest signal this direction resolves.

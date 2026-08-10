# B5-3 (connector slice) — priority-ordered claim query + pinned moderation lane: full-direction verification spec (REV 2)

> Module: `crates/aero-audit-connector` (pg.rs / outbox.rs / fake.rs / drills / tests).
> Source analysis: `docs/auto/analyses/crates-aero-audit-connector-src-7edb5949.json` (direction 2, B5-3).
> Prior records: REV 1 of this file (2026-08-08, pre-implementation — superseded by this revision);
> `docs/requirements/2026-08-07-aero-ai-b5-3-priority-claim-ordering.req.md` (sibling record, amendments §0-A1/A2/A3);
> `docs/requirements/2026-08-08-aero-ai-b5-1-migration-0239-governance-outbox.req.md` (0239/0240 DDL).
> Status: requirements specification — **all direction gaps verified LANDED in the working tree**; the
> implement stage is a verification-and-commit pass, not a code delta.

## 0. State reconciliation — what the direction claims vs. what the repo holds today

The direction's problem statement names **three gaps**. All three have **landed in the working tree**
since the analysis snapshot (and since REV 1 of this file). Every claim below was re-verified against the
repository on 2026-08-08, including `cargo check` and `cargo test` runs (see §2).

| Direction claim | Verified state today |
|---|---|
| (a) `claim_due` in `pg.rs` orders `(available_at, created_at, event_id)` only — no priority term | ❌ **Landed.** `PgOutboxRepo::claim_due` claim CTE orders `ORDER BY candidate.priority DESC, candidate.available_at, candidate.created_at, candidate.event_id` (pg.rs) — priority DESC leads, FIFO within a lane. `ClaimRow` carries `priority: i16` + `class: String`; `RETURNING` selects `outbox.priority, outbox.class`; `From<ClaimRow> for Claim` maps them. PG test `mixed_priority_claim_orders_moderation_first_then_fifo` asserts the claimed *set* = {all 10 admin} ∪ {15 earliest backlog} (set-based oracle, design D3). |
| (a) the columns/table are absent (0239 not landed) | ❌ **Landed.** `migrations/0239_audit_governance_outbox.sql`: `priority SMALLINT NOT NULL DEFAULT 10 CHECK (priority > 0)`, `class TEXT NOT NULL DEFAULT 'message' CHECK (class IN ('admin','message','room'))`, `delivery_mode`, status 0/1/2/3 CHECK, due FIFO index kept. `migrations/0240_audit_governance_due_prio_idx.sql`: `audit_governance_due_prio_idx (priority DESC, available_at, created_at, event_id) WHERE status IN (0,1)` (D5 legacy-index drop deferred to a later migration). `migrations/0241_governance_reconcile.sql` backs `reconcile()` (`aero_reconcile_governance_audit`). |
| (b) `Claim` (outbox.rs) / `FakeOutbox` (fake.rs) carry no priority/class — trait seam can't exercise ordering | ❌ **Landed.** `Claim` (outbox.rs) has `pub priority: i16` + `pub class: String` with doc pins to `aero_ai::governance`; `FakeOutbox` has `insert_lane(event_id, payload, due_at, priority, class)` (existing `insert` delegates with 0239 defaults `10`/`"message"`), claim sort mirrors the PG CTE byte-for-byte (`(Reverse(priority), available_at, created_at, id)`), `FakeRowSnapshot` exposes `priority`/`class`. |
| Drill constants `[PROPOSED]` BACKLOG=100 / MODERATION=200 | ❌ **Landed (corrected).** `aero-audit-priority-drill.rs` pins `BACKLOG_PRIORITY: i64 = 10` / `MODERATION_PRIORITY: i64 = 100` ("Production values, not [PROPOSED]", citing `aero_ai::governance`) and `MODERATION_ACTION: &str = "admin.content.flag"` ("one constant, not a runtime choice"). The direction's draft 100/200 pair is superseded by `GOVERNANCE_PRIORITY_BACKLOG=10` / `GOVERNANCE_PRIORITY_MODERATION=100` (governance.rs:31/:33) — DESC semantics need only `MODERATION > BACKLOG`, and 10/100 are the 0239 column default and trigger stamp. |
| Drill docstring: 0239 landing without the ordering is a FAIL, not a SKIP | ✅ **Confirmed.** Drill module doc: "0239 landing *without* B5-3's priority ordering is NOT a SKIP: this drill then FAILS red". Exit 2 (SKIP) only on the table/column capability probes. |
| A1 unit-level priority coverage missing | ❌ **Landed.** `tests/state_machine.rs` has `priority_first_claim_preempts_fifo_and_limit1_keeps_top_lane` (A1-7, B5-3 R5): pinned clock, 20 backlog rows with *earlier* `available_at` + 1 moderation row with *later* `available_at` enqueued last → `claims[0]` is the moderation row; backlog stays FIFO; `limit=1` hands the single slot to the top lane. |
| T-11 regressions exist and stay green | ✅ **Confirmed + green today.** `forbidden_dead_on_first_attempt` (state_machine.rs, 403 → Dead on attempt 1, exactly 1 POST); `transient_5xx_requeues_and_rotates_a_fresh_token`, `transient_timeout_requeues_and_rotates_a_fresh_token`, `transient_claim_drift_requeues_without_any_post` (relay.rs tests, drift → 0 POSTs). All pass. |

**Remaining work = none.** The implement stage must verify the pinned contracts below and commit the
untracked working-tree set (crate + migrations 0239–0241 + governance.rs + audit_governance.rs + related
docs/scripts — see §5), fixing any drift it finds.

## 1. Scope

Pin the landed B5-3 contract for the drill-to-green acceptance oracle and the A1/T-11 regression
surface. The requirements are **normative pins with verification anchors**, not new code.

**In scope (verification targets):**
- `PgOutboxRepo::claim_due` ordering: `(priority DESC, available_at, created_at, event_id)`, with the
  lane carried on the returned `Claim` (pg.rs ↔ outbox.rs).
- `Claim` / `ClaimRow` / `FakeOutbox` lane carriage and the fake's byte-for-byte sort mirror (D1:
  fake Vec position IS the fake's ordering contract).
- Lane constants pinned to `aero_ai::governance` (10/100, `"admin"`/`"message"`/`"room"`,
  `admin.content.flag`) by duplicated literals + cross-slice comments — **no `aero-ai` import**.
- The priority drill's FAIL-not-SKIP semantics and round-1 membership oracle (D3).
- A1 unit test and T-11 regression suite.
- 0239/0240 DDL shape (priority/class columns + due-priority index) as the drill's capability gate.

**Out of scope** (landed elsewhere or explicitly excluded — do not expand):
- Anti-starvation cap, L1 `message.*` aggregation, aero-bus `audit.priority.*` seam (separate
  directions).
- `delivery_mode` consumer (0239 reserves the column; no consumer yet).
- D5 legacy-index cleanup migration (0239's `audit_governance_due_idx` is deliberately kept until old
  FIFO binaries drain).
- Any behavior change to `client.rs` / `relay.rs` / `stub.rs` beyond what is already landed (the relay
  consumes `event_id`/`claim_token`/`attempts`/`payload` from `Claim`; lane fields are additive).

## 2. Evidence verification (run on 2026-08-08 against the working tree)

| # | Cited evidence | Verified result |
|---|---|---|
| E1 | `pg.rs` `claim_due` had no priority term | ⚠️ **Superseded — landed.** Claim CTE: `ORDER BY candidate.priority DESC, candidate.available_at, candidate.created_at, candidate.event_id … FOR UPDATE SKIP LOCKED LIMIT $1`; `RETURNING … outbox.priority, outbox.class`. Single clock domain (`clock_timestamp()`) untouched. |
| E2 | `ClaimRow` lacked priority/class | ⚠️ **Superseded — landed.** `ClaimRow` (pg.rs) = `event_id, attempts, claim_token, lease_expires_at, payload, priority: i16, class: String`; `From<ClaimRow> for Claim` maps all seven. |
| E3 | `Claim` (outbox.rs) lacked priority/class | ⚠️ **Superseded — landed.** `pub priority: i16` ("DESC lane value … 0239 SMALLINT", pinned to governance.rs:31/:33), `pub class: String` (governance.rs `GOVERNANCE_CLASS_*`, :37/:39/:41). Trait `claim_due` doc pins the `(priority DESC, …)` order and that the returned `Claim` carries the lane. |
| E4 | `FakeOutbox` had no priority sort/fields | ⚠️ **Superseded — landed.** `FakeRow`/`FakeRowSnapshot` carry `priority`/`class`; `insert_lane` seeds a lane; `insert` delegates with defaults `10`/`"message"` (0239 defaults, governance.rs pins); `claim_due` sorts `(Reverse(priority), available_at, created_at, id)` — "Byte-for-byte mirror of the PG claim CTE order". |
| E5 | Drill `[PROPOSED]` 100/200 constants | ⚠️ **Superseded — corrected to 10/100.** `BACKLOG_PRIORITY: i64 = 10`, `MODERATION_PRIORITY: i64 = 100`, `MODERATION_ACTION: &str = "admin.content.flag"` (drill const block); seeds `class='message'` (priority 10) ×500 then `class='admin'` (priority 100, action `admin.content.flag`) ×1, backlog `available_at` earlier, moderation last. |
| E6 | Drill FAIL-not-SKIP docstring | ✅ **Confirmed.** Module doc: ordering absence ⇒ FAIL red; exit 2 only when the 0239 table or `priority`/`class` columns are absent (runtime probes `to_regclass` + `information_schema.columns`). TRUNCATE-at-start guard. |
| E7 | governance.rs authoritative constants | ✅ **Confirmed.** `GOVERNANCE_PRIORITY_MODERATION: i16 = 100` (:31), `GOVERNANCE_PRIORITY_BACKLOG: i16 = 10` (:33), `GOVERNANCE_CLASS_ADMIN="admin"` (:37), `GOVERNANCE_CLASS_MESSAGE="message"` (:39), `GOVERNANCE_CLASS_ROOM="room"` (:41), `LOCAL_ACTION_MODERATED="message.moderated"` (:49), `MODERATION_OUTBOUND_ACTION="admin.content.flag"` (:56). `governance_lane_for("message.moderated")` → admin/100/`admin.content.flag`/status 0. DESC-precedence warning "do not 'fix' the direction to match `ai_job`" (governance.rs:11-25). |
| E8 | Drill wired into integration + pin slot | ✅ **Confirmed.** `scripts/test-integration.sh`: `PRIORITY_DRILL_DB="aero_priority_drill_$$"` (:37), `assert_disposable_db_name` (:61), drill section (:441-457) gated on 0239, fresh migrate then run; `scripts/b5-pin.sh:41` slot `moderation-priority-drill`. |
| E9 | 0239/0240 landed | ✅ **Confirmed.** 0239: `priority SMALLINT NOT NULL DEFAULT 10 CHECK (priority > 0)`, `class TEXT NOT NULL DEFAULT 'message' CHECK (class IN ('admin','message','room'))`, status CHECK 0-3, `audit_governance_due_idx` (FIFO, kept). 0240: `audit_governance_due_prio_idx (priority DESC, available_at, created_at, event_id) WHERE status IN (0,1)`. 0241: reconcile function. All untracked (in-flight). |
| E10 | `ensure_outbox_table` fixture | ✅ **Landed.** pg.rs test fixture now has BOTH `priority smallint NOT NULL DEFAULT 10` and `class TEXT NOT NULL DEFAULT 'message'` (REV 1's E10 gap closed) — `RETURNING outbox.class` works on non-0239 throwaway DBs. |
| E11 | `claim_validation.rs` `claim()` constructor | ✅ **Landed.** `fn claim() -> Claim` (tests/claim_validation.rs:37) builds all seven fields incl. `priority: 10` / `class: "message".into()` (backlog-lane defaults, governance.rs pins). |
| E12 | T-11 regression tests | ✅ **Confirmed + green.** `forbidden_dead_on_first_attempt` (state_machine.rs:231, 403 → Dead attempt 1, `stub.posts() == 1`); `transient_5xx_requeues_and_rotates_a_fresh_token` (relay.rs:365), `transient_timeout_requeues_and_rotates_a_fresh_token` (relay.rs:381), `transient_claim_drift_requeues_without_any_post` (relay.rs:398, 0 POSTs); plus `backoff_is_bounded_and_exponential`, `permanent_error_dead_after_exactly_two_attempts`, `happy_path_settles_and_removes_from_claimable`, `stale_token_cannot_ack_after_reclaim`, `skew_gt_lease_cannot_livelock_claim_fence_settle`. |
| E13 | A1 unit priority test | ✅ **Landed + green.** `priority_first_claim_preempts_fifo_and_limit1_keeps_top_lane` (state_machine.rs:334). |
| E14 | Build/test state (run today) | ✅ **`cargo check -p aero-audit-connector --all-targets` clean.** `cargo test -p aero-audit-connector --lib --tests`: lib 8 passed / 3 `#[ignore]` PG-gated (concurrent double-claim, lease reclaim, mixed-priority set), claim_validation 11 passed, state_machine 7 passed — 0 failures. No `aero-ai` import anywhere in the crate (grep: only comments); `rg "\b200\b"` finds only stub HTTP 200s, `t0−200s` seed offsets, and `make_interval(secs => 200)` — no stale priority literals. |

## 3. Requirements (normative pins — all verified landed)

### R1 — `Claim` carries the governance lane
`Claim` (outbox.rs) has `pub priority: i16` (DESC lane value; 0239 SMALLINT; pinned to
`GOVERNANCE_PRIORITY_*`) and `pub class: String` (one of `GOVERNANCE_CLASS_*`). `OutboxRepo::claim_due`
doc records the `(priority DESC, available_at, created_at, event_id)` contract and that the returned
`Claim` carries the lane. **Verify:** fields present, doc pins name `crates/aero-ai/src/governance.rs`.

### R2 — PG impl populates the lane
`ClaimRow` (pg.rs) carries `priority: i16` / `class: String`; the claim CTE `RETURNING` selects
`outbox.priority, outbox.class`; `From<ClaimRow> for Claim` maps them; the ORDER BY is
`candidate.priority DESC, candidate.available_at, candidate.created_at, candidate.event_id` — priority
DESC first, FIFO tie-break within a lane, `FOR UPDATE SKIP LOCKED`, `LIMIT`, single clock domain
(`clock_timestamp()`) unchanged. Fixture `ensure_outbox_table` includes both columns with 0239 defaults.
**Verify:** SQL verbatim; PG test `mixed_priority_claim_orders_moderation_first_then_fifo` (set-based,
D3) green against a migrated throwaway DB (`--ignored` + `DATABASE_URL`).

### R3 — `FakeOutbox` is lane-aware with a priority-first claim sort
`FakeRow`/`FakeRowSnapshot` carry `priority`/`class`; `insert_lane(event_id, payload, due_at, priority,
class)` seeds a lane; `insert(event_id, payload, due_at)` delegates with 0239 defaults
(`10`/`"message"`) so pre-existing call sites keep FIFO-in-backlog semantics; `claim_due` sorts
`(Reverse(priority), available_at, created_at, id)` — the fake's returned Vec order IS its ordering
contract (D1), assertable without PG. **Verify:** sort mirrors the PG CTE byte-for-byte; existing
state_machine/relay tests unchanged.

### R4 — Lane values pinned to `aero_ai::governance`; no import
Connector-side literals (drill `BACKLOG_PRIORITY=10` / `MODERATION_PRIORITY=100` / `MODERATION_ACTION=
"admin.content.flag"`, fake defaults, test seeds) equal governance.rs verbatim with cross-slice pin
comments. **No `aero-ai` dependency or import** (Cargo.toml unchanged). DESC invariant: higher = more
urgent; never "align" to `ai_job`'s ASC model (governance.rs:11-25). `MODERATION_OUTBOUND_ACTION` is one
constant, not a runtime choice. **Verify:** `grep -rn "aero_ai" crates/aero-audit-connector` → comments
only; `rg "\b200\b"` in the crate → no lane literals.

### R5 — A1 unit test: priority-first claim order independent of `available_at`
`priority_first_claim_preempts_fifo_and_limit1_keeps_top_lane` (state_machine.rs) with pinned clock:
moderation row (100/`"admin"`, `available_at` LATER than every backlog row, enqueued last) is
`claims[0]`; backlog rows (10/`"message"`) stay FIFO in `claims[1..]`; `limit=1` claims exactly the
moderation row; snapshot exposes `priority`/`class`. **Verify:** `cargo test -p aero-audit-connector
--test state_machine` green without `DATABASE_URL`.

### R6 — Compile surface
`claim_validation.rs::claim()` (and any other literal `Claim` construction) includes the two fields.
**Verify:** `cargo check -p aero-audit-connector --all-targets` clean.

### R7 — Drill FAIL-not-SKIP semantics and membership oracle
`aero-audit-priority-drill.rs` exits 2 (SKIP) ONLY on the 0239 table/column capability probes; any
ordering failure (moderation row not in the round-1 top-100 claimed set) is a hard FAIL. Round-1 oracle
is **batch membership** (D3): the moderation row is in the top-100 claimed set while 401 backlog rows
remain unclaimed — NOT `delivered_at` firstness (heap-order `RETURNING`; see §4 AC1). **Verify:** drill
docstring + asserts; `scripts/test-integration.sh` slot PASS.

### R8 — 0239/0240 DDL shape
`audit_governance_outbox.priority SMALLINT NOT NULL DEFAULT 10 CHECK (priority > 0)`; `class TEXT NOT
NULL DEFAULT 'message' CHECK (class IN ('admin','message','room'))`; `audit_governance_due_prio_idx
(priority DESC, available_at, created_at, event_id) WHERE status IN (0,1)`. **Verify:** migrations
present, `aero-cli migrate` on a throwaway DB creates them (build first, AGENTS.md §4.2).

## 4. Acceptance criteria (testable)

> Commands assume a throwaway Postgres per AGENTS.md §4.3 (`CREATE DATABASE` → `aero-cli migrate` →
> run → `DROP DATABASE`). The supplied acceptance checks are preserved; the two *delivery-timestamp*
> phrases are mapped to their testable set-level equivalents with the documented supersession (§0-A2).

**AC1 — Priority drill PASS end-to-end (acceptance oracle).**
On a throwaway DB migrated through 0241:
`DATABASE_URL=<url> cargo run -p aero-audit-connector --bin aero-audit-priority-drill`
exits 0 and prints all three PASS lines — `drill: moderation-in-first-batch: PASS`,
`drill: drain-501: PASS`, `drill: parity-501: PASS` — with **no SKIP** (0239 + `priority`/`class`
columns exist) and **no FAIL** (ordering landed). Equivalently, `scripts/test-integration.sh`'s
`moderation-priority-drill` slot (`PRIORITY_DRILL_DB="aero_priority_drill_$$"`, :441-457) is PASS, and
`scripts/b5-pin.sh` slot `moderation-priority-drill` (b5-pin.sh:41) flips FAIL → PASS. Drill mechanics
(verified): seeds 500 backlog rows (priority 10, `class='message'`, earlier `available_at`) THEN 1
moderation row (priority 100, `class='admin'`, action `admin.content.flag`, later `available_at`);
`batch_size=100`, `concurrency=1`, `MAX_ROUNDS=10`; relay `dispatch_batch` runs `reconcile` then the
`FOR UPDATE SKIP LOCKED` claim CTE, delivers to the stub (202 + valid receipt), settles with
`delivered_at = clock_timestamp()`.

Supplied acceptance mapping:
- **"500 backlog + 1 moderation row (moderation enqueued LAST)"** — verified verbatim (seed loops,
  backlog first / moderation last; only priority can explain preemption).
- **"round 1 claims exactly the moderation row"** — testable as: round 1 claims exactly
  `batch_size` (100) rows (`claimed == 100`, `COUNT(status=2) == 100` after round 1) AND the
  moderation row is in that top-100 claimed set (assert `moderation-in-first-batch`), i.e. the
  moderation row is claimed while 401 backlog rows remain unclaimed.
- **"delivered_at == MIN(delivered_at)"** and **"moderation delivered strictly before every backlog
  row"** — **superseded, preserved at set level**: amendment §0-A2 of
  `docs/requirements/2026-08-07-aero-ai-b5-3-priority-claim-ordering.req.md` (design D3) —
  `UPDATE … FROM (CTE ORDER BY …) … RETURNING` emits target-table heap order (empirically
  reproduced); the moderation row is inserted last, so within round 1 it settles last and a
  delivered-at-firstness assert is unsatisfiable. The testable contract is **batch membership**
  (moderation claimed in round 1 while 401 backlog rows remain unclaimed) plus the set-level
  full-drain parity below. Do NOT reintroduce a `delivered_at == MIN(delivered_at)` assert — it
  would fail deterministically (REV 1 §6).
- **"full drain settles all 501 with event_id set-parity"** — testable as: `COUNT(status=2) == 501`
  (assert `drain-501`, zero stuck rows in status 0/1/3) AND the delivered `event_id` set equals the
  seeded set exactly (assert `parity-501`, sorted set comparison — no orphans, no duplicates).

**AC2 — A1 unit test: priority-first claim order without PG.**
`cargo test -p aero-audit-connector --test state_machine priority_first_claim_preempts_fifo_and_limit1_keeps_top_lane`
is green with no `DATABASE_URL`: pinned clock; moderation row (priority 100, `class "admin"`,
`available_at` later than every backlog row, enqueued last) is `claims[0]`; the 20 backlog rows
(priority 10, `class "message"`) follow in FIFO `(available_at, created_at, event_id)` order;
`limit=1` claims exactly the moderation row — priority wins independent of `available_at`/enqueue
order. **Verified green today (7/7 state_machine tests).**

**AC3 — T-11 regressions stay green (unchanged behavior).**
`cargo test -p aero-audit-connector --lib --tests` fully green:
- `forbidden_dead_on_first_attempt` — HTTP 403 → `Dead` on attempt 1, exactly **1 delivery POST**
  (T-11 fail-closed; state_machine.rs:231, `stub.posts() == 1`);
- `transient_5xx_requeues_and_rotates_a_fresh_token` (+ `transient_timeout_requeues_and_rotates_a_fresh_token`)
  — transient 5xx/timeout **requeue, never dead**, fresh fencing token on reclaim;
- `transient_claim_drift_requeues_without_any_post` — claim-validation drift → **0 POSTs** (requeue only);
- plus `backoff_is_bounded_and_exponential`, `permanent_error_dead_after_exactly_two_attempts`,
  `happy_path_settles_and_removes_from_claimable`, `stale_token_cannot_ack_after_reclaim`,
  `skew_gt_lease_cannot_livelock_claim_fence_settle`, the 11 `claim_validation.rs` tests, and the
  lib/relay/config unit tests — all unchanged and green. **Verified today: 8 lib + 11 claim_validation
  + 7 state_machine, 0 failures (3 PG `#[ignore]`).**

**AC4 — Pin-guard and hygiene.**
- `scripts/b5-pin.sh` (slot `moderation-priority-drill` at :41) and `scripts/test-b5-pin-guard.sh`
  pass; no slot removed/renamed.
- `cargo test -p aero-ai --lib` keeps `moderation_lane_preempts_backlog_under_desc_claim` green
  (governance.rs pin: 100 > 10 under DESC).
- `grep -rn "aero_ai" crates/aero-audit-connector` → comment mentions only (no import);
  `rg "\b200\b" crates/aero-audit-connector/src crates/aero-audit-connector/tests` → only stub HTTP
  200s / `t0−200s` offsets / `make_interval(secs => 200)`, **no stale lane literals**.
- AGENTS.md §4.3 gate: `cargo check --workspace` clean · `cargo test --workspace --lib` green ·
  `cargo clippy --workspace --all-targets` no new warnings · `scripts/{truth-check,file-size-check,
  web-check}.sh` 0 violations.

## 5. Change surface (complete list)

**Code delta: none required — all pins verified landed.** The implement stage's change surface is the
**commit** of the in-flight untracked set that constitutes this direction (all verified above):

| Path | Contents |
|---|---|
| `crates/aero-audit-connector/` (new crate) | R1–R7: outbox.rs, pg.rs, fake.rs, client.rs, config.rs, relay.rs, stub.rs, lib.rs, `bin/aero-audit-{priority,relay,t11}-drill.rs`, `tests/{state_machine,claim_validation}.rs` |
| `migrations/0239_audit_governance_outbox.sql`, `0240_audit_governance_due_prio_idx.sql`, `0241_governance_reconcile.sql` | R8: DDL + due-priority index + reconcile |
| `crates/aero-ai/src/governance.rs` | R4 authority: lane constants (100/10, classes, `admin.content.flag`) |
| `crates/aero-storage/src/audit_governance.rs`, `crates/aero-storage/src/lib.rs` (modified) | 0239 DDL contract db_tests + slice wiring |
| `crates/aero-server/src/bin/main.rs`, root `Cargo.toml` (workspace member :29/:77), `Cargo.lock` (modified) | relay boot + crate wiring |
| `scripts/test-integration.sh` (modified), `scripts/b5-pin.sh` | AC1/AC4 harness slots |
| `docs/design/2026-08-06…aero-audit-connector…`, `docs/design/2026-08-07…b5-2…` etc. | design records |

No further code changes to `pg.rs` / `outbox.rs` / `fake.rs` / drills / tests are required by this
spec; any drift found during verification is a defect to fix in the implement stage.

## 6. Risks and notes

- **Stale-analysis hazard**: the direction's problem statement describes the pre-sibling-slice state
  (FIFO-only ORDER BY, absent columns, `[PROPOSED]` 100/200). All three gaps have landed; re-implementing
  them would be a no-op or a regression (e.g. re-introducing `delivered_at == MIN(delivered_at)` asserts
  that D3 proved unsatisfiable — the moderation row is inserted last, so it settles last within round 1).
- **Fake Vec-order vs PG heap-order (D1/D3)**: A1 asserts Vec position on the fake only (its sorted due
  list); the drill asserts batch membership on PG (heap-order `RETURNING`). Do not "port" the A1 position
  assert into the PG test, and do not "downgrade" the drill oracle to firstness.
- **Direction-of-priority invariant**: everything stays `priority DESC` (higher = more urgent);
  governance.rs's "do not align with `ai_job`" warning (ASC lower-first) applies to the connector too.
  `MODERATION_PRIORITY (100) > BACKLOG_PRIORITY (10)` is the pin; the direction's draft 100/200 pair is
  not normative.
- **i16 vs i64**: `Claim`/`ClaimRow`/fake use `i16` (SMALLINT decode); drill seed binds are `i64`
  (implicit sqlx cast) — a mismatch fails at compile time, which is the point.
- **Uncommitted state**: every file in §5 is untracked/modified in the working tree; acceptance runs
  against this tree, and the implement stage must commit it (pipeline `git_commit: true`). The
  `#[ignore]` PG tests (incl. `mixed_priority_claim_orders_moderation_first_then_fifo`) require
  `DATABASE_URL` + migrated throwaway DB — they are NOT part of the default `cargo test` gate.

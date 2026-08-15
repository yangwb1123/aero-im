# Design (landed record) — B5-3 anti-starvation cap (D-CAP min-service-floor) on `claim_due`

> **Status: LANDED.** Implemented in commit `ae50db9` ("feat(b5): B5-3 anti-starvation cap (DCAP min-service-floor) on governance outbox claim"). This document is the landed-state design record: it restates the concrete design (API changes, compatibility, failure modes, migration, acceptance mapping) as it exists in the current tree, and records an **independent verification pass executed 2026-08-15** against the live tree + a throwaway migrated Postgres (all gates green). It supersedes the pre-implementation spec (`docs/design/2026-08-15-aero-ai-b5-3-dcap-min-service-floor.design.md`, status "pre-implementation") for the implementation state; the design authority (§7.1 of `docs/design/2026-08-07-aero-ai-b5-3-priority-claim-ordering.design.md`) and requirements spec (`docs/auto/runs/add-the-b5-3-anti-starvation-cap-to-claim-due-pe-2cb2a2b1/artifacts/requirements-10762e10/requirements.md`) are unchanged.
>
> **Source analysis caveat (verified):** the direction analysis (`docs/auto/analyses/crates-aero-ai-src-1bbf99ce.json`) predates `ae50db9` and describes the pre-cap state ("no per-priority quota in `claim_due`", "drill cannot detect starvation"). Every claim in it about the *gap* is historically accurate but stale; the acceptance criteria below are therefore specified as verification gates over the landed implementation.

## 0. Independent verification verdict (evidence treated as untrusted, re-checked)

| Evidence claim | Verdict | Anchor (current tree) |
|---|---|---|
| Cap landed at commit `ae50db9`, `cargo check` clean, connector lib 19/19 | ✅ Confirmed by `git show ae50db9 --stat` (connector sources, 2 design docs, `test-integration.sh`, `truth-check-lib.sh`; **no `b5-pin.sh` change**) and by re-running the gates (§7) | `ae50db9` |
| `claim_due` two-arm SQL: arm A top `limit − K` total order + arm B `K` earliest-due MIN-priority lane, `NOT EXISTS` dedupe, `MATERIALIZED`, binds `limit−floor`/`floor`/`lease_secs`, `clock_timestamp()` single clock domain | ✅ Confirmed verbatim | `crates/aero-audit-connector/src/pg.rs:98-155` (`claim_due`) |
| `MAX_CLAIM=500` / `MIN_SERVICE_BATCH_DIVISOR=20` / `min_service_floor(b) = min(max(1, b/20), b−1)`, `b = clamp(1, MAX_CLAIM)`; pure+total | ✅ Confirmed; unit pins incl. `(i64::MIN)→0, (i64::MAX)→25`; full-domain `K < b` sweep | `relay.rs:35,39-40,78-86`; `relay.rs:525` (`min_service_floor_is_pinned_and_total`) |
| Governance lane constants 100/10, DESC higher-first, `governance_lane_for` maps `LOCAL_ACTION_MODERATED` → admin/100, module doc "do not align with ai_job ASC" | ✅ Confirmed | `crates/aero-ai/src/governance.rs:13,31,33,82-106,139-147` |
| Proposal `audit-contract-batch-aero-im.md:10` B5-3 text "注入积压 drill（500 积压 + 1 moderation → 先达 sink）+ 反饥饿上限" | ✅ Verbatim | `docs/proposals/audit-contract-batch-aero-im.md:10` |
| Drill phase 2: 600 admin earliest + 100 backlog latest, batch 100, K=5, rounds 2-5 quota (`25 backlog @ round 5`, `≥125 admin pending`), 700-drain ≤ `MAX_ROUNDS=10`, destructive gate exact-string `AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=="1"` | ✅ Confirmed; negative gate **executed**: absent opt-in → exit 1 REFUSED, rows survive; `"true"` → exit 1 | drill `:68,99,103-110` (consts), `:117` (`truncate_gate_allows`), `:435` (`phase2_starvation`; call at `:418`), `:654` (`starvation_floor_matches_production_batch`), `:665` (`truncate_gate_requires_the_exact_opt_in`) |
| Drill SKIP-exit-2 on missing 0239 table / priority/class columns | ✅ Confirmed (`exit(2)` at `:154,:207`); `aero-cli audit-provision-check --priority` is the gate wrapper (emits `priority: landed`) | drill `:35` (doc), `:154,:207`; `test-integration.sh:598` |
| `mixed_priority_claim_orders_moderation_first_then_fifo` — {all 10 admin} ∪ {15 earliest backlog} at limit 25 | ✅ Exists (line drift :593); **executed PASS** on throwaway DB | `pg.rs:593` |
| B5-3 AC3.1 `sustained_mixed_lanes_reserve_min_service_floor_each_round` (190 admin + 40 backlog, limit 100, 2 rounds, (95,5)/(95,5) disjoint) | ✅ Exists at `pg.rs:706`; **executed PASS** (also `mixed_lane_arm_b_contention_stays_disjoint` :746, `claim_due_zero_limit_clamps_to_one` :782, `concurrent_double_claim…` :362, `expired_lease…` :494) | `pg.rs` |
| `b5-pin.sh`: 41 slots = 20 executed + 21 [PROPOSED]; `t11-fail-closed` + `moderation-priority-drill` present; guard checks count/dupes/malformed/vacuous/verdict-lines | ✅ Confirmed by awk count (41) and by running `scripts/test-b5-pin-guard.sh` (green). `assert_b5_contract_pin` at `:89` (trivial drift from :92) | `b5-pin.sh:35-88,89-135` |
| T-11 fail-closed unchanged | ✅ **Executed PASS** on throwaway DB: 8/8 status 0, 0 terminal, attempts 8→16, 8/8 transport-failure `last_error` | `aero-audit-t11-drill.rs`; `test-integration.sh:470` (`b5_check "t11-fail-closed"`) |
| Unrelated starvation guards only | ✅ Confirmed: `budget.rs:202` (per-tenant cost budget), `sweep.rs:28` (bounded batches), `rate_limit.rs:271` (operational path bypass) — none touch the audit claim path | as cited |
| **New finding (not in evidence):** the 6 `#[ignore]` PG tests are **not self-isolating** — run in parallel against one shared DB, 5/6 fail (row interference). The harness contract is per-DB **serial** (`--test-threads=1`, `test-integration.sh:182`). The evidence's command summary omits this flag. | ⚠️ Recorded as an operational constraint, §4/§7 | `test-integration.sh:182,206` |

**Executed gates (all green, throwaway migrated PG 17 via `aero-postgres` container):**
- `cargo test -p aero-audit-connector --lib` → **19 passed; 0 failed; 6 ignored**.
- `cargo test -p aero-audit-connector --lib -- --ignored --test-threads=1` on throwaway DB → **6/6 passed** (precedence set-equality, per-round (95,5)×2 disjoint, arm-B contention disjoint with `attempts==1`, zero-limit clamp, concurrent double-claim, expired-lease reclaim).
- `AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=1 … aero-audit-priority-drill` → **exit 0**, all 7 PASS lines (`moderation-in-first-batch`, `moderation-action-vocabulary`, `drain-502`, `parity-502`, `starvation-round1-split-95-5`, `starvation-cross-batch-quota`, `starvation-drain-700`).
- Drill destructive gate: no opt-in → exit 1 REFUSED (rows survive); `"true"` → exit 1; `"1"` → proceeds.
- `aero-audit-t11-drill` on throwaway DB → **exit 0**, all assertions hold.
- `bash scripts/test-b5-pin-guard.sh` → green; `B5_CONTRACT_TEST_LIST` == 41.

## 1. Problem statement (as landed)

`PgOutboxRepo::claim_due` previously claimed the top `limit` rows of `(priority DESC, available_at, created_at, event_id)` with no per-priority bound, so a sustained priority-100 moderation flood could claim every slot of every round and starve the priority-10 backlog lane (message L1 windows, room events, auth-token rows) indefinitely. The B5-3 contract (`docs/proposals/audit-contract-batch-aero-im.md:10`) requires the 反饥饿上限: a per-batch reservation for the lowest-priority lane. Landed mechanism: **D-CAP two-arm claim** — arm A takes the top `limit − K` by total order (precedence preserved), arm B takes `K` earliest-due rows of the MIN-priority lane (fairness floor), one statement, one lease protocol.

## 2. API changes (landed surface)

1. **`relay.rs`** — two new `pub` items beside `MAX_CLAIM` (`relay.rs:35`):
   - `pub const MIN_SERVICE_BATCH_DIVISOR: i64 = 20` (`:39-40`) — config-visible constant pattern; **no env var, no `RelayConfig` field** (by design).
   - `pub fn min_service_floor(batch: i64) -> i64` (`:78-86`) — `K = min(max(1, b/20), b−1)` with `b = batch.clamp(1, MAX_CLAIM)`; pure + total, `#[must_use]`.
2. **`pg.rs` `claim_due`** (`:98-155`) — **signature unchanged** (`(lease, limit) -> Vec<Claim>`). Internally: `floor = min_service_floor(limit.clamp(1, MAX_CLAIM))`; the `claimable` CTE becomes arm A (`WITH arm_a AS MATERIALIZED … LIMIT $1`, `$1 = limit − floor ≥ 1`) + arm B (`priority = MIN(priority)` scalar subquery with identical WHERE, `NOT EXISTS` anti-join against arm A, `LIMIT $2`, `$2 = floor` — 0 valid) + `UNION ALL`; `UPDATE … FROM claimable … RETURNING` and lease mint unchanged. The three WHERE fragments are textual copies of today's single filter (`status IN (0,1)`, due, lease expired) — filter contract unchanged.
3. **`outbox.rs`** — trait docs extended (+8 lines, floor contract: each round reserves `min_service_floor(limit)` slots for the lowest-priority lane; identity property). **No signature change.**
4. **`fake.rs`** — `FakeOutbox::claim_due` mirrors the two-arm set semantics (clamp → floor → arm A take → arm B take with MIN-priority + exclusion). Existing relay-level tests (≤2 single-lane rows at limit 10 → K=1) are neutral by inspection.
5. **Drill bin `aero-audit-priority-drill`** — additive phase 2 (`phase2_starvation`, `:435`): consts `STARVATION_ADMIN_ROWS=600`, `STARVATION_BACKLOG_ROWS=100`, `STARVATION_K=5` (pinned == `min_service_floor(BATCH_SIZE)` by unit test `:654`), `MAX_ROUNDS=10`; shared destructive gate `truncate_gate_allows` (`:117`) gated on exact string `AERO_PRIORITY_DRILL_ALLOW_TRUNCATE == "1"`; capability probes exit 2 SKIP when 0239/priority/class absent (`:154,:207`).
6. **`aero-cli audit-provision-check --priority`** — wrapper that runs the drill and prints the `priority: landed` verdict line (unchanged command surface; the drill gained the phase-2 PASS lines).
7. **`scripts/test-integration.sh`** — additive grep pin `drill: starvation-drain-700: PASS` in the existing moderation-priority-drill leg (`:598`); `b5_check "moderation-priority-drill" "PASS"` verdict; negative-gate checks for the destructive opt-in (absent / `"true"` must REFUSE). **`scripts/b5-pin.sh` untouched** — slot set and 41-count guard pre-existed the cap.
8. **`scripts/truth-check-lib.sh`** — claim-literal pins updated for the new SQL shape (commit stat: 33 lines).

## 3. Compatibility constraints

| Axis | Constraint |
|---|---|
| Schema | **No migration.** 0240's partial index `audit_governance_due_prio_idx (priority DESC, available_at, created_at, event_id) WHERE status IN (0,1)` serves both arms (S6 EXPLAIN in the pre-implementation record: Index Scan / Index Scan Backward + Anti Join). `_sqlx_migrations` untouched. |
| Config | No new env var for the floor; the only new env surface is the drill's **destructive test opt-in** `AERO_PRIORITY_DRILL_ALLOW_TRUNCATE` (exact `"1"`; fail-closed). |
| API | `OutboxRepo::claim_due` trait + `ClaimRow` shape unchanged; new items additive in an already-`pub` module. |
| Semantics | Priority precedence within a batch and within-lane FIFO unchanged; **identity property**: high-lane due ≤ `limit − K` ⇒ claimed set identical to the pre-cap top-`limit` set; `low_due = 0` ⇒ arm B backfills the MIN lane (the high lane), batch stays full — no throughput tax. |
| Lane direction | Governance lane stays `priority DESC` higher-first; **must not be "aligned"** with `ai_job`'s ASC lower-first model (governance.rs module doc; inverting breaks the contract). |
| Concurrency | `FOR UPDATE SKIP LOCKED` on both arms, same lease/fence protocol (`clock_timestamp()` single clock domain; app-supplied `now` still prohibited) ⇒ old and new binaries can roll across the same table; claims stay disjoint across sessions. |
| Delivery firstness | The claimed **set** is the contract; `RETURNING` order (heap-order) is an executor artifact — no strict-first assertion may be reintroduced. |
| Existing oracles | `mixed_priority…` (K(25)=1), `concurrent_double_claim…`, `expired_lease…`, T-11 (K(100)=5), drill phase 1 — all unchanged; verified green (§0). |

## 4. Failure modes

| Failure | Mechanism | Mitigation / pin |
|---|---|---|
| Arm overlap / double-claim | `SKIP LOCKED` does not dedupe arms within one statement; a row in both arms corrupts the split and the `claimable` union. | `NOT EXISTS` anti-join is structural; `MATERIALIZED` pins arm A's single evaluation (an inlining refactor would open a narrow double-claim window). Pinned by drill `starvation-round1-split-95-5` + `sustained_mixed_lanes…` disjointness + `mixed_lane_arm_b_contention…` (two sessions, `attempts==1`). |
| Precedence inversion (K ≥ batch) | Floor equal to batch empties arm A at `batch=1`. | `b − 1` clamp ⇒ `K(1)=0`, `LIMIT 0` is valid SQL, claim degenerates to uncapped top-1 (`claim_due_zero_limit_clamps_to_one`). Full-domain `K < b` sweep unit-pinned. |
| Non-total floor | Panic/UB on extreme `i64` input. | Pure + total construction; pins `(i64::MIN)→0, (i64::MAX)→25`. |
| Snapshot-relative dilution under concurrency | One statement's arm B can be diluted by a concurrent claimer's locks (SKIP LOCKED skips rows another statement locked). | Per-statement split is not a hard invariant; the **aggregate** floor holds by conservation. Documented, not a bug. |
| Low lane underfull (< K) | Batch claims `limit − K + low_due` (slack ≤ K). | Drain loop reclaims next tick; slack is deliberately **not** refilled from the total order to keep the identity property exact. `low_due = 0` exception: arm B serves the high lane — full batch, no tax. |
| 3+ lane values mid-lane starvation | Floor protects only the MIN lane. | Documented limitation; SQL is MIN-driven (not hardcoded to 10) so a future lane generalizes without redesign. Today exactly two lane values (10/100). |
| Drill destructive gate bypass | Phase-2 TRUNCATE over non-empty outbox without the opt-in. | Exact-string gate (`"1"` only; absent/`"true"`/`"yes"` refuse exit 1, rows survive) shared by both phases, plus test-integration negative checks. **Executed.** |
| Capability SKIP vs red FAIL | 0239/priority/class absent must SKIP, not fail red. | Runtime probes → exit 2 SKIP; ordering present but wrong → red FAIL (the honest G6 signal). |
| **Shared-DB parallel test interference (new finding)** | The 6 `#[ignore]` PG tests insert into one outbox and don't clean up; parallel `--ignored` on a shared DB fails 5/6. | Harness contract: one throwaway DB per test group, `--test-threads=1` (`test-integration.sh:182,206`). **Do not** run the ignored set parallel against a shared DB. |
| SQL drift among the 3 WHERE copies | Arm A / arm B / MIN filters diverge. | Textual copies of the single pre-cap filter; set-semantics pins (regression tests + drill). |
| Performance | Second scan + MIN per round. | Arm B bounded at K ≤ 25 (batch 500) / K=5 (production 100); MIN via Index Scan Backward on 0240; measured 28.7 ms at 100k fully-leased rows (pre-implementation S6) — far inside the 10 s statement timeout. |

## 5. Migration steps

**None — code-only change, no `migrations/NNNN_*.sql`.** The cap is a pure claim-side behavior change over the B5-1 0239 table + 0240 index. The verification sequence (per AGENTS.md §4.3 throwaway-DB discipline; **executed in §7**):

1. `cargo build` then `aero-cli migrate` on a throwaway DB (`CREATE DATABASE` → migrate → run → `DROP DATABASE`; never touch the shared dev DB).
2. Unit gates: `cargo test -p aero-audit-connector --lib` (19/19).
3. PG gates: `cargo test -p aero-audit-connector --lib -- --ignored --test-threads=1` with `DATABASE_URL` → throwaway (6/6).
4. Drill gates: `AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=1 cargo run -p aero-audit-connector --bin aero-audit-priority-drill` (exit 0, 7 PASS lines); T-11 drill (exit 0); negative destructive-gate runs (exit 1).
5. Pin gate: `bash scripts/test-b5-pin-guard.sh` (green, 41/41).
6. Workspace gates: `cargo check --workspace`, `cargo clippy --workspace --all-targets`, `scripts/truth-check.sh` (claim literal pins updated in `truth-check-lib.sh` by the cap).

## 6. Testable acceptance mapping (as landed — executed)

| Acceptance | Testable check | Result (2026-08-15, this pass) |
|---|---|---|
| **AC1** sustained-moderation anti-starvation | (1) `sustained_mixed_lanes_reserve_min_service_floor_each_round` (pg.rs:706): 190 admin + 40 backlog, limit 100, 2 rounds → (95,5)/(95,5), disjoint, 190a+10b claimed. (2) drill exit 0 with `starvation-round1-split-95-5` / `starvation-cross-batch-quota` (round 5: exactly 25 backlog delivered, ≥125 admin pending) / `starvation-drain-700` PASS. (3) `test-integration.sh` greps `drill: starvation-drain-700: PASS` + `B5-CHECK moderation-priority-drill: PASS` | ✅ PASS (db_test 6/6; drill exit 0, all 3 phase-2 lines) |
| **AC2** no precedence regression | `mixed_priority_claim_orders_moderation_first_then_fifo` (pg.rs:593) set == {10 admin} ∪ {15 earliest backlog}; drill phase-1 `moderation-in-first-batch`/`drain-502`/`parity-502` PASS; `min_service_floor_is_pinned_and_total` proves `K < b` over `[1, 500]` | ✅ PASS |
| **AC3** T-11 fail-closed unchanged | `aero-audit-t11-drill` on throwaway DB: exit 0, `COUNT(status=0)==total`, `COUNT(status IN (1,2,3))==0`, attempts 1× then 2×, `last_error` transport-failure == total; `B5-CHECK t11-fail-closed: PASS` | ✅ PASS (8/8, 0 terminal, 8→16, 8/8 transport) |
| **AC4** contract-pin gate 41/41 | `bash scripts/test-b5-pin-guard.sh` green; `assert_b5_contract_pin` count == 41, no dupes/malformed, ≥1 executed, every executed slot has a `B5-CHECK` verdict line; `git show ae50db9 --stat` shows **no** `b5-pin.sh` change | ✅ PASS (41 = 20 executed + 21 proposed; guard script green; commit stat clean) |
| **FR-7** claim integrity | `concurrent_double_claim_across_two_sessions_is_impossible`, `expired_lease_is_reclaimed_with_fresh_token`, `mixed_lane_arm_b_contention_stays_disjoint`, `claim_due_zero_limit_clamps_to_one` | ✅ PASS (4/4 executed on throwaway DB) |

## 7. Verification command summary (as executed)

```bash
# throwaway DB (per-group), aero-postgres container:
#   CREATE DATABASE <name>; AERO__DATABASE__URL=… cargo run --bin aero-cli -- migrate
cargo test -p aero-audit-connector --lib                       # 19 passed, 0 failed
cargo test -p aero-audit-connector --lib -- --ignored --test-threads=1 \
    # DATABASE_URL=<throwaway>                                  # 6 passed (serial; parallel shared-DB fails — see §4)
AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=1 DATABASE_URL=<throwaway> \
    cargo run -p aero-audit-connector --bin aero-audit-priority-drill   # exit 0, 7 PASS lines
DATABASE_URL=<throwaway> \
    cargo run -p aero-audit-connector --bin aero-audit-t11-drill        # exit 0 (8/8 pending, 0 terminal)
bash scripts/test-b5-pin-guard.sh                              # green (41/41 pin guard)
bash scripts/test-integration.sh                               # full B5 gate incl. both verdict lines
```

## 8. Non-goals / invariants preserved (unchanged from spec)

- No "alignment" of the DESC governance lane with `ai_job` ASC; no changes to `GOVERNANCE_PRIORITY_*`, `MODERATION_OUTBOUND_ACTION`, the 0239 trigger, or the drill's vocabulary-pair pin (drift → red bail).
- Dual outbound moderation token question (`admin.content.flag` vs `admin.moderation.action`) — separate direction; the cap only preserves the existing pair pin and the leaf import.
- No env var / `RelayConfig` field for the floor; no delivery-firstness semantics; no change to `settle`/`requeue`/`mark_dead`/403→dead-immediate.
- `SKIP_LOCKED` concurrency, single `clock_timestamp()` domain, app-supplied-`now` prohibition — load-bearing, unchanged.

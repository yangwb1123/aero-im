# Design: Land migration 0239 — verified-landing design (post-implementation)

> Status: **implemented in working tree, uncommitted** · Slice: B5-1 (migration + trigger + `governance.rs`) + **B5-3 claim-ordering fold-in (P1)** · Requirements: `docs/requirements/2026-08-07-aero-ai-b5-1-migration-0239-governance-outbox.req.md` · Prior design: `docs/design/2026-08-07-aero-ai-b5-1-migration-0239-audit-governance-outbox.design.md` (proposed-state; this doc supersedes its "current state" section with verified facts)
> Direction: `docs/auto/analyses/crates-aero-ai-src-1bbf99ce.json` #1 — "Land migration 0239: audit_governance_outbox DDL (status 0/1/2/3 + class/priority) with the in-tx enqueue-redirect trigger"

> **P1 fold-in verification (2026-08-08, executed against throwaway migrated DBs):**
>
> | Oracle | Result |
> |---|---|
> | `aero-audit-priority-drill` (fresh DB, migrate through 0241) | **exit 0** — `drill: moderation-in-first-batch: PASS` / `drain-501: PASS` / `parity-501: PASS` (was exit 1 red pre-fold) |
> | `aero-audit-relay-drill` (fresh DB) | **exit 0** — 3/3 delivered, parity exact |
> | `aero-audit-t11-drill` (fresh DB) | **exit 0** — round 1 3/3 pending, round 2 attempts=6 |
> | connector `-- --ignored` (fresh migrated DB) | **3/3 PASS** — `mixed_priority_claim_orders_moderation_first_then_fifo` (new R3) + double-claim + expired-lease reclaim |
> | storage `audit_governance::` `-- --ignored` (fresh migrated DB) | **6/6 PASS** incl. `ddl_contract_defaults_and_checks` (DEFAULT 10) and `moderation_finalize_outbox_parity` (in-tx F9) |
> | aero-ai `governance::` · connector unit · b5-pin guard self-test | 6/6 · 8/8+3 ignored · PASS |
> | `cargo check --workspace` · clippy `-p aero-audit-connector --all-targets` | clean · 0 warnings |
>
> Concurrent-hardening changes folded in during verification (from the
> deployment/security/db reviews): 0241 `aero_reconcile_governance_audit`
> (disabled-window reconciler, Q1) — relay calls it ahead of every claim
> batch; the connector's call site needed `$1::integer` + `::bigint` return
> cast (the 0241 function takes/returns INTEGER; i64 binds as INT8);
> TRUNCATE-at-start isolation guards in the three drills AND the three PG
> tests (db-reviewer finding 3 — a shared-DB run must never corrupt count
> asserts). Note: drills on a DB that already holds `message.moderated`
> audit rows will see reconcile backfill change the claimed set — the
> harness protocol (fresh DB per drill) is load-bearing for the drill
> count oracles.
>
> Value CHECKs (db-reviewer finding 4) added to 0239: `class IN
> ('admin','message','room')` (the GOVERNANCE_CLASS_* lanes), `priority >
> 0` (DESC lane; new lane values land via migration), `delivery_mode IN
> ('push')` (reserved; B5-3 delivery policy extends by migration) — a
> typo'd literal fails at INSERT, never silently; pinned behaviorally in
> `ddl_contract_defaults_and_checks`. Deployment rec 4: F10
> `expired_lease_is_reclaimed_with_fresh_token` in pg.rs (lease expiry →
> reclaim with fresh token + attempts++ ; the crash-after-claim recovery)
> and 0240's forward-dated claim-query comment corrected (index is
> prepared-for, inert until B5-3's ORDER BY term lands — the term landed
> with the P1 fold-in, so the index is now live).

## 0. Evidence verification — untrusted claims → confirmed repo facts

Every claim in the handoff evidence was re-checked against the working tree on 2026-08-08.

| # | Claimed | Verification result |
|---|---|---|
| V1 | **Central claim is stale**: "the table does not exist" — the analysis (2026-08-07 14:21) predates the landing; the working tree already contains the full landing, all untracked | ✅ **Confirmed**. `git status` shows `?? migrations/0239_audit_governance_outbox.sql`, `?? migrations/0240_audit_governance_due_prio_idx.sql`, `?? crates/aero-audit-connector/`, `?? crates/aero-ai/src/governance.rs`, `?? crates/aero-eng/src/audit_provision.rs`, `?? crates/aero-eng/tests/audit_provision.rs`, `?? crates/aero-storage/src/audit_governance.rs` + `M` on Cargo.toml/Cargo.lock, aero-ai lib/worker, aero-cli main, aero-eng lib/run, aero-live-srt, aero-server Cargo+main, aero-storage lib, `scripts/test-integration.sh` |
| V2 | 0239 DDL: status CHECK (0,1,2,3), class/priority/delivery_mode defaults, due index, `aero_enqueue_governance_audit` + `audit_events_governance_enqueue` trigger | ✅ Read in full. 13 columns, `CHECK (status IN (0,1,2,3))`, `class DEFAULT 'message'`, `priority SMALLINT DEFAULT 10`, `delivery_mode DEFAULT 'push'`, `audit_governance_claim_state` CHECK, `audit_governance_due_idx` partial `(available_at, created_at, event_id) WHERE status IN (0,1)`, function + AFTER INSERT trigger with runtime gate (fail-open) + binding gate (fail-closed RAISE via 0235 helper) + token-keyed mapping (`ON CONFLICT (event_id) DO NOTHING`) |
| V3 | 0240 = B5-3 sibling index | ✅ `audit_governance_due_prio_idx (priority DESC, available_at, created_at, event_id) WHERE status IN (0,1)`; 0239's FIFO index deliberately kept (rolling-deploy coexistence) |
| V4 | `pg.rs` STATUS 0/1/2/3 at :28-31 | ✅ `STATUS_ENQUEUED=0/CLAIMED=1/DELIVERED=2/DEAD=3` at :28-31; claim CTE `status IN (0,1)` + `available_at <= clock_timestamp()` + lease check + `FOR UPDATE SKIP LOCKED`, ORDER BY FIFO (no priority term — B5-3 scope) |
| V5 | `to_regclass` probes at t11-drill.rs:73 / relay-drill.rs:57 / priority-drill.rs:87 | ✅ exact line match in all three drill bins |
| V6 | `cargo test -p aero-ai governance::` → 6/6 green | ✅ **Re-run**: 6 passed / 0 failed (`mapping_is_token_keyed`, `unknown_local_token_passes_through_unmapped`, `user_delete_token_stays_out_of_admin_lane`, `outbound_action_is_single_contract_token`, `admin_class_rows_never_aggregated`, `moderation_lane_preempts_backlog_under_desc_claim`) |
| V7 | `G0239_CANDIDATES` + Q3 four-bucket at audit_provision.rs:26/:48 | ✅ `["audit_governance_outbox","audit_outbox"]` at :26; Q3 `SELECT status, count(*) … GROUP BY status` at :48; dead rows a separate terminal bucket, never folded into delivered |
| V8 | 403→immediate dead / 422→dead after ≤1 retry at relay.rs:173-226 + state_machine.rs:231/:145 | ✅ relay.rs `deliver_claim` :173-231: `DeliveryError::Forbidden` → `mark_dead` immediately (:190-204, "Deliberately NOT `is_dead_at`"); `Permanent` → `is_dead_at(attempts)` (:48-49, `attempts >= PERMANENT_DEAD_AT`) → requeue on attempt 1, dead at ≥2 (:212-217); unit test asserts `!is_dead_at(1)` / `is_dead_at(2)` (:424-427) |
| V9 | 37-slot pin at b5-pin.sh with guard self-test | ✅ `assert_b5_contract_pin`: exactly 37 named slots, no dupes, ≥1 executed, every executed slot backed by `B5-CHECK <name>: PASS|SKIP`; `scripts/test-b5-pin-guard.sh` exists |
| V10 | 4 acceptance checks preserved verbatim (A1–A4) + two clarifications (403/422 semantics live in the DB-free connector state machine; "green 0239 row" = `audit_governance::` + `moderation_finalize_outbox_parity` flipping SKIP→PASS) | ✅ A1–A4 present in the requirements doc (§4, oracles §5); 403/422 unit tests are DB-free (relay.rs + state_machine.rs, no `DATABASE_URL`); the b5-pin 37-slot list names both `audit_governance::` and `moderation_finalize_outbox_parity` |
| V11 | Integration wiring: `audit_governance::` + `moderation_finalize_outbox_parity` entries, relay/T-11/priority drill sections, all `else` = explicit `SKIP (0239 not landed)` | ✅ test-integration.sh gates at :306, :325, :347, :441 (evidence cited 306/325/347/447 — the T-11 gate moved to :347; count and semantics hold); empty-filter guard + `--test-threads=1` per entry |
| V12 | Workspace compiles | ✅ `cargo check -p aero-audit-connector -p aero-ai` clean; `aero-audit-connector` in root workspace + aero-server deps |
| V13 | "Entire batch is untracked — commit is the prerequisite for every oracle" | ✅ **Confirmed and still the #1 risk**: zero commits contain the batch (`git log` top = docs commits); all acceptance oracles run against working-tree files, so a lost/partial commit silently reverts every gate to SKIP |

**Nuance adopted from verification**: the requirements doc's E1 ("0239 never existed", 238 migrations) was *true at analysis time* and is *stale now* — the direction's own central premise is fully discharged by the untracked landing. The design below therefore treats the landing as **existing-but-uncommitted** and the remaining work as commit + rollout + acceptance proof.

## 1. API changes (complete surface of the landing)

**One production Rust touch** (the security-review Q1 closure, §7.2): the relay tick now calls the disabled-window reconciler before every claim batch — the exact mirror of v1's dispatch loop (`aero-server/src/snaplink_commercial/runtime.rs` runs `reconcile_usage` + `reconcile_audit` ahead of `claim_due`). Everything else is additive SQL + a constants module already compiled in:

| Layer | Change | Nature |
|---|---|---|
| DDL | `audit_governance_outbox` table (13 cols; `status` CHECK 0/1/2/3; `class`/`priority`/`delivery_mode` defaults; payload JSONB-object CHECK; claim-state CHECK; due partial index) | Additive, `IF NOT EXISTS`, idempotent |
| DDL | `aero_enqueue_governance_audit()` RETURNS trigger — runtime gate (fail-open) → token map (`message.moderated` only) → binding gate (fail-closed RAISE via 0235 helper) → insert v2 envelope row | `CREATE OR REPLACE`, additive |
| DDL | `audit_events_governance_enqueue` AFTER INSERT ON `audit_events` FOR EACH ROW | Additive; fires before `audit_events_snaplink_delivery` ('g' < 's') |
| DDL | 0240 `audit_governance_due_prio_idx (priority DESC, …)` | Additive B5-3 handoff H1 discharge; 0239's FIFO index kept |
| DDL | 0241 `aero_reconcile_governance_audit(max_rows)` — v2 disabled-window reconciler, mirror of 0236 `aero_reconcile_snaplink_audit` | Additive `CREATE OR REPLACE`; **security Q1 closure** (§7.2) |
| Rust (already in tree) | `aero_ai::governance` — `LOCAL_ACTION_MODERATED="message.moderated"`, `MODERATION_OUTBOUND_ACTION="admin.content.flag"`, `GOVERNANCE_CLASS_{ADMIN,MESSAGE,ROOM}`, `GOVERNANCE_PRIORITY_{MODERATION=100,BACKLOG=10}`, `GovernanceLane`, `governance_lane_for` | Constants-only; the single token/class/priority authority |
| Rust (already in tree + Q1 wiring) | `aero-audit-connector::pg::PgOutboxRepo` — `claim_due`/`settle`/`requeue`/`mark_dead` over the table + **`reconcile`** (runs `aero_reconcile_governance_audit`); `ensure_outbox_table` minimal fallback (throwaway-DB test only) | Consumer; relay boots degrade-to-log when table absent |
| Rust (Q1 wiring) | `aero-audit-connector::relay::Relay::dispatch_batch` — `reconcile(batch_size)` before `claim_due` (mirror of v1 runtime.rs:215-216) | Consumer; one tick call |
| Rust (already in tree) | `aero-eng::audit_provision` Q2/Q3/Q4/Q5 (four-bucket status report incl. dead) | Consumer; probes candidate names |
| Tests (already in tree + Q1) | `aero-storage::audit_governance` db_tests incl. `moderation_finalize_outbox_parity` + **`governance_reconcile_backfills_disabled_window`**; connector unit + integration tests; three drill bins | Oracles (§7) |

Consumers of the new API: `PgOutboxRepo` SQL, three drills, `audit_provision` Q2/Q3, storage db_tests, `scripts/test-integration.sh` gates. No REST/WS/bus API changes; no config keys added.

## 2. Compatibility constraints (hard, verified against 0235/0236)

1. **v1 path untouched**: `snaplink_delivery_outbox` (0235) and `audit_events_snaplink_delivery` (0236) are not modified by this landing — 0239 is an *additive parallel trigger*. Both triggers coexist on `audit_events`; firing order fixed ('g' < 's'), so the 0236 binding RAISE aborts both rows' inserts together — the 0239 trigger must never add a second RAISE path (R-D2).
2. **Fail-open on unknown tokens**: any `audit_events.action != 'message.moderated'` returns `NEW` untouched — a second abort path would block all room.*/message.* rows through the shared trigger.
3. **Runtime gate mirrors 0236** (fail-open): `snaplink_commercial_runtime.enabled` false ⇒ zero outbox rows, no raise. Pinned by A2 half (4) `moderation_finalize_runtime_disabled_commits_1_plus_0`.
4. **Binding gate fail-closed**: missing binding ⇒ 0235 `aero_snaplink_binding_for_workspace` RAISE aborts the whole tx (soft delete + audit + outbox roll back together). Pinned by `moderation_finalize_without_binding_aborts_tx`. The lookup lives in *this* trigger so the abort is independent of trigger order.
5. **1:1 parity**: `event_id = audit_events.id` PRIMARY KEY + `ON CONFLICT DO NOTHING` — the pinned `UNIQUE(event_id)` dedup contract; admin-class rows must never be merged by L1 aggregation.
6. **Rolling deploy**: 0239 FIFO index + 0240 priority index coexist while old FIFO binaries drain; each ORDER BY shape served by its matching index; 0240 is additive + `IF NOT EXISTS`. Legacy index dropped by a later cleanup migration (B5-3 D5), never by 0239/0240.
7. **Dependency direction**: aero-storage must not import aero-ai — column defaults and the moderation stamp are guarded by textual pins in the SQL + behavioral db_tests, not by code sharing.
8. **DEFAULT 10 pin (handoff H1)**: `priority DEFAULT 10` = `GOVERNANCE_PRIORITY_BACKLOG`; B5-3's aero-bus design doc must reconcile its "DEFAULT 0" to 10 at landing.
9. **Envelope supersession (C7)**: the v2 outbound row carries `action='admin.content.flag'` + actor/targets/aggregate envelope; the local token `message.moderated` appears only on the v1 0236 row — never on v2 (A2 field assertions).
10. **Accepted deviations (non-blocking, documented in the prior design)**: `priority SMALLINT` (not INTEGER — i16 = fixture-authority type, 0130 ai_jobs precedent); 13th reserved column `delivery_mode DEFAULT 'push'` (zero consumers).
11. **Reconciler contracts (0241, security Q1 closure — §7.1)**: token-keyed scan (`message.moderated` only — never fabricates `admin.content.flag` for unmapped actions, R-D2); runtime-gate-free (the recovery path exists *because* the gate skipped enqueue — v1 parity, 0236's `aero_reconcile_snaplink_audit` has no `runtime.enabled` check either); INSERT byte-identical to the 0239 trigger's INSERT (same literals, so A2 field assertions hold for both paths); dead/delivered rows never re-entered (NOT EXISTS over the full table; delivered rows are the durable cursor); 1:1 via the shared `event_id` PK + `ON CONFLICT DO NOTHING` — the reconciler and the trigger can never double-enqueue the same event.

## 3. Failure modes

| # | Failure | Behavior | Detection / recovery |
|---|---|---|---|
| F1 | Migration not compiled into bin (`aero-cli migrate` before `cargo build`) | 0239/0240/0241 silently no-op; drills stay SKIP(exit 2); relay logs claim errors | A1 reverse assertion: file exists + `to_regclass` non-NULL; `make migrate-smoke` fresh-deploy replay |
| F2 | Missing commercial binding at moderation-finalize time | 0235 RAISE aborts whole tx (soft delete + audit + outbox) — fail-closed by design | `moderation_finalize_without_binding_aborts_tx` db_test |
| F3 | Runtime switch disabled | Zero outbox rows, tx commits — fail-open | `moderation_finalize_runtime_disabled_commits_1_plus_0` db_test (|G|=0) |
| F4 | Non-moderation audit row | Pass-through, zero outbox rows, no raise | A3 oracle + `unknown_local_token_passes_through_unmapped` |
| F5 | Sink rejects identity (HTTP 403) | Immediate `status=3` dead, no retry — fail-closed identity/provisioning fault | relay.rs DB-free unit test + T-11 drill |
| F6 | Sink 422/permanent payload fault | Attempt 1 requeue with backoff; attempt ≥2 dead (`is_dead_at`) | `is_dead_at` unit tests + state_machine.rs |
| F7 | Transient delivery error | Requeue with exponential backoff (cap 300s); never dead | state_machine.rs transient tests |
| F8 | Lease lost between deliver and settle | Warn + idempotent retry recovers (event_id PK; sink `Idempotency-Key` = event_id) | warn log; claim_validation tests |
| F9 | Trigger never fires (drills seed table directly) | Drills green while in-tx path is dead | `moderation_finalize_outbox_parity` = the in-tx oracle (F9 explicit in module doc) |
| F10 | mark_dead/requeue fence fails | Lease expiry reclaims; row retried until budget exhausted | warn + `lease_expires_at` filter in claim CTE |
| F11 | Relay boots before 0239 lands | Degrade-to-log claim errors, no crash (F13 seam, main.rs:245-266) | by design; disappears once migrated |
| F12 | psql hangs in audit-provision-check | 30s per-query timeout fails the check, never hangs the gate | QUERY_TIMEOUT const |
| F13 | **Batch uncommitted (top risk)** | Any checkout/cleanup loses the landing; every oracle reverts to SKIP | commit is step 1 of §5; `git status` audit |
| F14 | **Disabled-window gap (security Q1)** | Rows moderated while the switch is off commit with zero outbox rows and the trigger never re-fires — pre-0241 this was permanent (v1 has `aero_reconcile_snaplink_audit`; v2 had no equivalent) | 0241 `aero_reconcile_governance_audit` backfills on the relay tick after re-enable (idempotent, token-keyed, runtime-gate-free); pinned by `governance_reconcile_backfills_disabled_window` |
| F15 | Relay runs against a pre-0241 DB (binary deployed before the migration) | `reconcile` SQL error → batch skipped, warn per tick, loop continues — same F11 degrade class; claims resume once migrated | by design (documented rollout order: migrate before/with deploy); warn log |

## 4. Migration steps (ordering is load-bearing)

> **Deployment-review amendment (2026-08-08, P1 resolution):** B5-3's
> claim-ordering slice is **folded into this batch** — `claim_due`'s ORDER BY
> gains the leading `priority DESC` term (pg.rs), the test fixture gains the
> `priority` column, the trait doc records the ordering contract, the drill
> constants align to governance.rs (10/100), and the drill's round-1 oracle
> is batch membership (D3). Rationale: 0240 (the index that is B5-3's schema
> half) already ships in this batch's migration set; the code half is a
> 1-line ORDER BY term + 1 fixture column + 2 doc lines + 1 new ignored PG
> test (`mixed_priority_claim_orders_moderation_first_then_fifo`); and the
> alternative — leaving the priority drill red until a second landing —
> cannot complete: test-integration.sh runs it unconditionally
> (0239-file-gated) under `set -euo pipefail`, so the harness dies at that
> section and steps 4/5 can never go green. The fold also makes 0240's
> header comment (which describes the claim query as priority-DESC)
> accurate — the "0240 comment fix" is resolved by the code, not by editing
> a landed migration (sqlx checksum validation, D2).

1. **Commit the batch** (prerequisite for every oracle — **F13 is caught by
   THIS step only**: every later oracle is working-tree-based and passes on an
   uncommitted tree, so a lost/partial commit silently reverts every gate to
   SKIP): `migrations/0239*`, `migrations/0240*`, `migrations/0241*` (Q1 reconciler), `crates/aero-audit-connector/`,
   `crates/aero-ai/src/governance.rs` (+ lib/worker wiring), `crates/aero-eng/src/audit_provision.rs` (+ tests + lib/run wiring), `crates/aero-storage/src/audit_governance.rs` (+ lib wiring), aero-cli/aero-server/aero-live-srt wiring, `scripts/test-integration.sh`, `docs/{design,requirements}/2026-08-07…` + this doc. Oracle: `git status --porcelain` shows zero B5-1/B5-3 files untracked.
2. `cargo build` — migrations are compile-time embedded (`sqlx::migrate!("../../migrations")`); **build before migrate, always**.
3. Throwaway DB: `CREATE DATABASE aero_m0239_$$` → `aero-cli migrate` (fresh-deploy replay; never touch the shared dev DB or hand-edit `_sqlx_migrations`).
4. Fast-feedback oracles: run the three drills against the throwaway DB. **Each must exit 0** (exit 2 SKIP ⇒ F1; the priority drill exits 1 red on any B5-3 ordering regression):
   - `aero-audit-relay-drill` → exit 0, 3/3 status=2 + event_id set-parity;
   - `aero-audit-t11-drill` → exit 0, round 1 3/3 pending + SUM(attempts)=3, round 2 attempts=6 (retry-forever on transport error);
   - `aero-audit-priority-drill` → exit 0 with `drill: moderation-in-first-batch: PASS`, `drill: drain-501: PASS`, `drill: parity-501: PASS` — the round-1 oracle is **batch membership** (moderation row ∈ the first claimed 100), per D3; firstness asserts were removed (heap-order RETURNING made them unsatisfiable).
5. `scripts/test-integration.sh` — all B5 entries flip from `SKIP (0239 not landed)` to PASS: `audit_governance::`, `moderation_finalize_outbox_parity`, relay drill, T-11 drill (incl. B5-4 provision legs), **moderation-priority drill** (B5-3 ordering folded in — no red-by-design window remains), plus `notification-fanout` / `relay-mock-probe` / `audit-provision-check`. `set -euo pipefail` makes any section failure a harness failure. F11 (relay boot against a pre-0239 DB) has no executable oracle in this harness — benign-by-design degrade seam (main.rs), caught by code inspection only.
6. `cargo test -p aero-ai governance::` (6/6) · `cargo test -p aero-audit-connector` (+ `-- --ignored` with `DATABASE_URL` for the double-claim test **and the new R3 mixed-priority test**) · `cargo test -p aero-eng --lib` (audit_provision) · `cargo test -p aero-storage --lib audit_governance -- --ignored` with `DATABASE_URL` + migrated throwaway DB (now 6 tests: the five DDL-contract fixtures + **`governance_reconcile_backfills_disabled_window`**, the 0241 security-Q1 oracle).
7. `cargo check --workspace` clean · `cargo clippy --workspace --all-targets` no new warnings · `scripts/{truth-check,file-size-check,web-check}.sh` 0 violations.
8. `scripts/b5-pin.sh` — run the guard AND the two named-verdict greps (**P2: "37/37 PASS" alone admits SKIP evidence**, so the greps are the oracle):
   ```bash
   assert_b5_contract_pin "$B5_LOG"            # prints "B5 contract pin: 37/37 …: PASS"
   grep -q '^B5-CHECK audit_governance::: PASS$' "$B5_LOG"
   grep -q '^B5-CHECK moderation_finalize_outbox_parity: PASS$' "$B5_LOG"
   grep -q '^B5-CHECK moderation-priority-drill: PASS$' "$B5_LOG"
   ```
   All three greps must match — these are the "green 0239 rows" (SKIP-with-reason counts as handled for the pin count, but not for this oracle).
9. Handoff H1: `grep 'DEFAULT 0' docs/design/2026-08-07-aero-bus-b5-3-priority-delivery-seam.design.md` → must be **empty** (reconciled to DEFAULT 10 in this batch, lines :145/:193).
10. Drop the throwaway DB.

## 5. Testable acceptance mapping (direction checks → oracles, verbatim + status)

| Direction acceptance check (verbatim) | Oracle | Status 2026-08-08 |
|---|---|---|
| "T-11: `aero-audit-t11-drill` exits 0 against a throwaway migrated DB (currently SKIP/exit 2) with 403 → status=3 immediate dead and 422 → dead after ≤1 retry" | Drill bin + `scripts/test-integration.sh:347` T-11 section (throwaway DB → migrate → stub sink → 403/422 assertions) | Landing present; **not yet run end-to-end** — requires steps §4.3–4.5 (untracked batch blocks nothing locally; drill run is the proof) |
| "Inserting one audit_events row fires the 0239 redirect trigger and produces an outbox row with status=0, correct class, and governance priority (`message.moderated` → class='admin', priority from `governance_lane_for`); test-integration.sh B5 gate no longer reports the drill as skipped" | `moderation_finalize_outbox_parity` + `audit_governance::` db_tests (`run_migrated_integration`, empty-filter guard, `--test-threads=1`); trigger path verified via real moderation finalize, not direct seeding (F9) | Test present at `aero-storage/src/audit_governance.rs:211`; requires `DATABASE_URL` + migrated DB to run |
| "P2 parity: for admin-class rows COUNT(audit_governance_outbox) == COUNT(audit_events) with event_id 1:1 (never merged by L1); non-moderation rows keep flowing through the shared redirect (governance.rs unknown-token pass-through)" | Parity db_test (event_id set-parity) + relay-drill COUNT(status=2)==N + `governance.rs` unit tests (6/6 green — **verified by re-run**). **Parity is scoped to enabled windows (§7.3)**: while the switch is off, |G|=0 is the pinned contract; after re-enable, `governance_reconcile_backfills_disabled_window` proves reconcile-then-parity | Unit half ✅ 6/6; parity half needs §4.5 |
| "Security Q1: rows moderated while the runtime switch is disabled are backfilled after re-enablement (v2 reconciler equivalent of v1's `aero_reconcile_snaplink_audit`)" (security-review follow-up, §7.1) | `governance_reconcile_backfills_disabled_window` db_test (0241): window → |G|=0 → re-enable+binding → reconcile → 1 row byte-identical to trigger path → idempotent re-run 0 → unmapped row never fabricated → dead never resurrected → parity converges. Rides the existing `audit_governance::` harness slot (no b5-pin change) | Test written; needs §4.5 (throwaway migrated DB) |
| "37/37: the pinned contract list in scripts/test-integration.sh:143 (b5-pin.sh) gains a green 0239 row; audit-provision-check Q3 reports four buckets with dead never folded into delivered" | `scripts/b5-pin.sh` 37-slot guard + `aero-eng` audit_provision Q3/Q5 (dead as terminal bucket) | Guard self-test present; green 0239 rows appear when §4.5 runs |

**Clarifications adopted** (per requirements spec): the 403/422 dead semantics are exercised by the **DB-free connector state machine** (relay.rs unit tests + `tests/state_machine.rs`), not by the T-11 drill body; the "green 0239 row" = the two named b5-pin slots flipping `SKIP (0239 not landed)` → `PASS`.

## 6. Out of scope / sequencing (held per direction)

- **B5-3 claim ordering is IN scope for this batch (P1 fold-in)**: the
  `priority DESC` ORDER BY term in `claim_due`, the fixture column, the trait
  doc, the R3 PG test, the drill constants (10/100) and the drill's D3
  membership oracle land together with 0239/0240 — see §4 amendment.
- No B5-4 boot enforcement (audit-provision-check remains a psql gate; relay boot stays degrade-to-log).
- No L1 aggregation (admin rows stay 1:1 forever; `delivery_mode` reserved, no consumer yet).
- No v1/v2 redirect unification — both paths coexist by design.
- 0236 v1 trigger is not rewritten (C3 adopted: additive, not redirect).
- No dead-row requeue-from-dead tooling (security Q2, non-blocking sibling slice): dead is terminal + loud (T-11 fail-closed gate, Q3 four-bucket report); remediation stays manual SQL until a runbook/CLI un-dead command lands. The 0241 reconciler deliberately does NOT resurrect dead rows (§7.1).

## 7. Security-review resolution (Q1 closed, Q3 expectation, parity scoping)

Post-landing adversarial review (security_reviewer) found exactly one real compliance gap (Q1) and two follow-ups (Q2 dead-row recovery, Q3 sink receipt expectation). Q1 is **closed by implementation in this batch** (0241 + relay wiring + oracle); Q3 and the parity scoping are **written expectations** in this section. The prior design doc's (2026-08-07 §5) "permanently out-of-band" decision is superseded — its re-open condition (*"if the governance relay contract ever requires catch-up delivery for rows accepted while disabled, add that backfill slice then"*) is triggered by this review.

### 7.1 Q1 — disabled-window reconciler (closed by 0241)

**Gap**: the 0239 trigger only fires at INSERT time. A `message.moderated` row accepted while `snaplink_commercial_runtime.enabled = FALSE` commits its audit row but never produces a v2 outbox row — and v2 had no equivalent of v1's `aero_reconcile_snaplink_audit` (0236:230). If the external sink is contractually the authoritative moderation record, that is a compliance hole reachable by a switch toggle.

**Fix — `aero_reconcile_governance_audit(max_rows)` (migration 0241)**, a mirror of 0236 with v2-specific adaptations, wired into the relay tick exactly like v1:

1. **Enabled-binding join** (identical to 0236): `JOIN snaplink_commercial_bindings … ON workspace_id AND enabled` — a row whose workspace has no enabled binding cannot be addressed (no tenant_id/client_id/source_system for the envelope) and stays locally audited only, which is the fail-open meaning of the gate. **The runtime gate is deliberately NOT re-applied**: the recovery path exists *because* the gate skipped enqueue — v1 parity (`aero_reconcile_snaplink_audit` has no `runtime.enabled` check; re-applying it would make the function a no-op exactly when needed). The operator re-enables the switch; the next tick backfills.
2. **NOT EXISTS backfill**: `NOT EXISTS (SELECT 1 FROM audit_governance_outbox WHERE event_id = audit.id)` over the v2 dedup key (`event_id` PK — not v1's constructed `delivery_id` string), `ORDER BY created_at, id`, `LIMIT LEAST(GREATEST(max_rows,1),500)` — bounded batch, repeatable, same shape as 0236.
3. **Idempotent**: `ON CONFLICT (event_id) DO NOTHING` — the pinned `UNIQUE(event_id)` dedup contract shared with the trigger; re-runs and concurrent runners return only newly inserted rows; the two paths can never double-enqueue the same event.
4. **Parity-safe across the fail-open gate**: the INSERT is byte-identical to the 0239 trigger's INSERT (same literals: status 0, class 'admin', priority 100, envelope with action 'admin.content.flag', actor/targets construction, sanitized payload via `aero_snaplink_audit_payload`), so a backfilled row is indistinguishable from a live-enqueued row and all A2 field assertions hold for both paths. After a disabled window + re-enable, parity converges to COUNT(outbox) == COUNT(audit) over the mapped subset.
5. **Token-keyed scan — the v2-specific guard (R-D2)**: the WHERE adds `audit.action = 'message.moderated'`. v1 backfills *every* action (its lane is the general billing/audit lane); v2's envelope hardcodes `admin.content.flag` / class 'admin' / priority 100, so backfilling an unmapped action would **fabricate a governance claim the trigger would never produce**. Unmapped rows stay locally audited + v1-laned only. Pinned by the oracle's unmapped-row assertion.
6. **Dead is terminal, delivered is the cursor**: dead (status=3) and delivered (status=2) rows exist in the outbox, so NOT EXISTS skips them — the reconciler never resurrects a relay-terminal row (it cannot fight a 403-outage dead-letter) and never re-delivers a delivered row (0236's "delivered rows remain as the durable reconciliation cursor" comment adopted verbatim).
7. **Retention caveat inherited**: the audit-side join is best-effort for rows older than the audit retention window (swept source rows are legal — prior-design §5(b), same exposure as v1).

**Wiring** (mirror of v1's dispatch loop): `OutboxRepo::reconcile(limit)` (new trait method) → `PgOutboxRepo` runs `SELECT aero_reconcile_governance_audit($1)` → `Relay::dispatch_batch` calls it **before** `claim_due` on every tick (v1 `runtime.rs:215-216` runs `reconcile_usage` + `reconcile_audit` ahead of `claim_due`). A disabled window therefore self-heals on the first tick after re-enable — no ops action, no permanent gap. Backfilled rows get `available_at = clock_timestamp()` at insert and are claimable on the next tick. A reconcile error aborts the batch (warn + next tick retries): the same degrade class as a claim error on a pre-0239 DB (F11/F15).

**Oracle**: `governance_reconcile_backfills_disabled_window` db_test in `aero-storage/src/audit_governance.rs` — rides the existing `audit_governance::` harness slot (no b5-pin change; the 37/37 list is untouched). Pins: window open → |G|=0 (A2 half 4 semantics preserved); re-enable + binding → reconcile returns 1 and the row is byte-identical to the trigger path; second run returns 0; unmapped `message.deleted` row never fabricated; parity COUNT(outbox)==COUNT(audit) over the mapped subset; dead row never resurrected.

### 7.2 Q3 — written sink expectation (non-conflict 202 for idempotent replays)

**Expectation (contractual, with the sink owner)**: the sink MUST answer idempotent replays of an already-accepted event with a **non-conflict 202 ACCEPTED** receipt (`conflict: false`, `accepted_at` present, `event_id` matching). The relay sends `Idempotency-Key: event_id` on every POST (client.rs); after a crash in the claim→deliver→settle window, the reclaimed row is re-POSTed with the same key (F8). The entire crash-window at-least-once guarantee rests on this expectation: `validate_audit_receipt` treats `conflict: true` as permanent-class (dead after ≤1 retry), so a sink that answers replays with `conflict: true` converts a benign crash-window redelivery into a dead row. Inherited from v1's identical posture; now written and pinned in the design.

### 7.3 Parity-report scoping — enabled windows only

P2 parity (`COUNT(outbox) == COUNT(audit)` with event_id 1:1) is asserted **over enabled windows**: while the runtime switch is off, |G|=0 is the pinned contract (A2 half 4), so any parity report that spans a disabled window without the reconcile step will (correctly) show an outbox deficit. Scoping rules:

1. The in-tx parity oracle (`moderation_finalize_outbox_parity`) runs with enforcement on — parity holds at finalize time.
2. The Q3 four-bucket report (`aero-eng::audit_provision`) counts outbox rows only; it never infers audit-side parity — the report is trustworthy as a point-in-time ledger regardless of switch state, and dead is a separate terminal bucket.
3. Any cross-table parity check (outbox vs audit_events) MUST either (a) run with enforcement enabled, or (b) state the enabled-window scope and apply `aero_reconcile_governance_audit` (or the relay's tick reconcile) before asserting equality. `governance_reconcile_backfills_disabled_window` demonstrates exactly (b): window → reconcile → parity holds.
4. The audit-side join is best-effort for rows older than the audit retention window (swept source rows are legal, prior-design §5(b)); parity scoping inherits that caveat.

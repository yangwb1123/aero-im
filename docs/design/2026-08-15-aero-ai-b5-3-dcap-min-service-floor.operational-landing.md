# Operational Landing Scope — B5-3 D-CAP min-service floor (two-arm `claim_due`)

> Companion to `docs/design/2026-08-15-aero-ai-b5-3-dcap-min-service-floor.design.md` (mechanism, AC1–AC6, pre-implementation). This document scopes the **operational landing**: (D-1) the observability surface needed to verify the floor works in prod, (D-2) the decision on the unswept terminal-row growth finding (DB-architect F1), (D-3) the safety verdict on the `ALLOW_TRUNCATE` drill gate in production configs, and (D-4) the landing validation sequence (rolling deploy / rollback / post-landing data-integrity checks).
>
> Every claim below was verified in-tree on 2026-08-15 at the working tree (design pre-implementation): `crates/aero-audit-connector` (pg.rs / relay.rs / outbox.rs / fake.rs / client.rs / config.rs / stub.rs, 4 drill bins), migrations 0239/0240/0241/0242/0245/0246, `crates/aero-server/src/bin/main.rs` + `boot/{metrics_tasks.rs,retention.rs}`, `crates/aero-common/src/metrics.rs`, `crates/aero-eng/src/audit_provision.rs`, `scripts/test-integration.sh`, `docs/runbooks/audit-relay-zero-delivery.md`, and the failed prior observability campaign `docs/auto/runs/add-production-gauges-for-the-audit-governance-o-8bff25de/` (design-gate FAIL + performance rejection).

## 0. Decisions (summary)

| # | Question | Decision |
|---|---|---|
| D-1 | No outbox depth/lag gauge exists to observe the floor | **Land floor observability in the same landing window** (3 additive series, zero new deps, no migration): `aero_audit_outbox_pending_by_priority` + `aero_audit_outbox_low_lane_lag_seconds` (gauges, 30s sampler, bounded cost) and `aero_audit_claims_total{priority}` (counter, relay emit). The §5.1 zero-delivery surface (transient/dead gauges + 0247 index + 2 counters) is **separate B5-2 observability debt** — it has never landed despite the runbook pinning it (see §1.1). |
| D-2 | DB F1: terminal rows (status 2/3) never swept → unbounded growth | **Hazard confirmed; schedule the retention follow-up independently of this slice.** The naive "sweep by `delivered_at`" is **contract-violating** — 0241 reconcile uses outbox rows as its dedup cursor ("delivered rows are the durable reconciliation cursor… never re-delivered"). The follow-up predicate must be age ≥ audit retention + margin (guarantees the source `audit_events` row is gone, so reconcile cannot resurrect). Full analysis + follow-up scope in §2. |
| D-3 | Safety of `AERO_PRIORITY_DRILL_ALLOW_TRUNCATE` in production configs | **Safe as-is, no code change.** The var is read only by the drill bin and the aero-eng wrapper; no production surface sets it (.env.example / config.example.toml / docker-compose.yml / Makefile / boot all clean — verified). Fail-closed semantics (`== "1"` only, typo-spelling detected), two gate layers, harness-negative-tested ×3. Two residual operator-error edges documented in the runbook (§3). Phase 2 of this slice reuses the same helper — no new surface. |
| D-4 | Landing validation sequence | Rolling deploy: mixed old/new fleet safe (same fence protocol, SKIP LOCKED disjointness, identity property); floor liveness holds fleet-wide only after full rollout. Rollback: pure code revert, no migration to undo. Post-landing: full AC set + integrity SQL on a fresh throwaway DB (§4). |

---

## 1. D-1 — Observability: the floor has no prod-visible signal

### 1.1 Current state (verified)

- **Zero `aero_audit_*` series exist in code.** `rg aero_audit crates/` hits only `aero_audit_append_failed_total` (aero-auth) and drill/test assertions. No `metrics.rs` in the connector crate, no `PgOutboxRepo::counts()`/`OutboxCounts`, no sampler block in `metrics_tasks.rs`, no counter emits in `relay.rs`'s deliver path.
- **The §5.1 metric authority has never landed.** `docs/design/2026-08-08-aero-auth-b5-2-machine-token-verification-seam.design.md` §5.1 (and runbook `docs/runbooks/audit-relay-zero-delivery.md` §3, which declares §5.1 the authority) defines 4 series — `aero_audit_outbox_transient_requeue`, `aero_audit_outbox_dead` (gauges), `aero_audit_token_rejections_total`, `aero_audit_delivery_outcomes_total` (counters) — with the landing plan "new file `crates/aero-audit-connector/src/metrics.rs` … names stay out of `aero_common::metrics::names`". The B5-2 *deliver-path* seam landed (JWKS verifier: `client.rs::with_key_provider`, `config.rs jwks_uri`), but the metrics half did not. **The runbook's alert rules therefore reference series that do not exist in any build** — a real ops debt: the CRITICAL zero-delivery rule is dead until the B5-2 counters land.
- **The prior gauge campaign failed at design gate, with a measured performance rejection** (`docs/auto/runs/add-production-gauges-for-the-audit-governance-o-8bff25de/`). Perf engineer (throwaway PG 17, full 244-migration replay, real 0239/0240): a WHERE-less `COUNT(*) FILTER` sampler over the table = **Parallel Seq Scan over the whole heap — 5.45 s cold / 0.42 s warm at 10M rows (7.8 GB)**, linear forever because the delivered majority is the permanent reconcile cursor with no retention. The fix prescribed and accepted: drop the delivered series, split into per-predicate scalar subselects — **pending via the 0240 partial index = 0.03–0.07 ms/tick at every tier**; dead via a new terminal partial index (0247) = 0.046 ms. **0247 never landed** (the run failed before implementation).
- Naming authority (architecture-constitution ruling in the same campaign): `_governance_` infix is banned repo-wide; names live as crate consts in the connector, not in `aero_common::metrics::names`.

### 1.2 What "the floor works" means observably

The floor's contract (design §1): every claim round reserves `K = min_service_floor(batch)` slots for the lowest-priority lane. Observable signatures in prod:

1. **Backlog claims keep flowing during admin floods** — the direct signature. Without the floor, sustained priority-100 due ≥ batch ⇒ backlog claim rate → 0 while admin drains.
2. **Backlog pending depth stays bounded under floods** — drains at ≥ K/round (liveness bound: `ceil(B/K)` rounds).
3. **Backlog lag (oldest pending age) stays bounded** — under a floor failure it grows monotonically during floods.

Existing §5.1 signals (zero-delivery: `transient_requeue`, `dead`) are orthogonal — they detect a *stalled sink*, not a *starved lane*.

### 1.3 Proposed metric surface (3 additive series)

| series | type | labels / values | predicate | emit point | cost |
|---|---|---|---|---|---|
| `aero_audit_claims_total` | counter | `priority` ∈ {today "10","100"; dynamic — derived from the `Claim.priority` the relay already carries (outbox.rs:57-76), not hardcoded} | per claimed row, counted at `dispatch_batch` after `claim_due` returns (batch-level grouping over the returned `Vec<Claim>`; claims counted even if delivery later fails — the floor is about *claiming*) | `relay.rs` `dispatch_batch` (single helper in new `crates/aero-audit-connector/src/metrics.rs`, `record_claims(&global(), …)`) | zero SQL |
| `aero_audit_outbox_pending_by_priority` | gauge | `priority` (dynamic; GROUP BY) | `status IN (0,1) AND attempts = 0` per priority — the exact §5.1 "pending" partition (complement of `transient_requeue` = attempts > 0; they partition status 0/1) | aero-server `metrics_tasks.rs` new 30s sampler (AI DLQ block template, `MissedTickBehavior::Skip`, Err → warn keep-prior, `register_help` at boot; unconditional — pre-0239 DB degrades to warn) | **bounded**: Index Only Scan on 0240 partial index, proportional to pending-set size, 0.03–0.07 ms measured at every tier |
| `aero_audit_outbox_low_lane_lag_seconds` | gauge | none | `EXTRACT(EPOCH FROM (clock_timestamp() − MAX(available_at)))` over `status IN (0,1) AND attempts = 0 AND priority = (SELECT MIN(priority) …)` — MIN-driven like the claim SQL, not hardcoded to 10 | same sampler, same SQL round trip | **bounded**: scan over the pending set (0240 index), same class as above |

Sampler SQL (one statement, scalar-subselect shape the perf engineer verified — the planner assigns an independent index path per subselect):

```sql
SELECT priority, COUNT(*)::bigint
  FROM audit_governance_outbox
 WHERE status IN (0,1) AND attempts = 0
 GROUP BY priority;                       -- → set_gauge_labeled per row

SELECT COALESCE(EXTRACT(EPOCH FROM (clock_timestamp() - MAX(available_at)))::bigint, 0)
  FROM audit_governance_outbox
 WHERE status IN (0,1) AND attempts = 0
   AND priority = (SELECT MIN(priority) FROM audit_governance_outbox
                    WHERE status IN (0,1) AND attempts = 0);
```

Concrete impl shape (per §5.1): `PgOutboxRepo::pending_by_priority()` + `low_lane_lag_seconds()` as **concrete impls on `PgOutboxRepo`** (trait/fake untouched, mirroring the §5.1 `counts()` plan); consts + `record_claims` helper in new `crates/aero-audit-connector/src/metrics.rs` (aero-common is already a dependency; `aero_common::metrics::global()`). The gauge sampler lives in `metrics_tasks.rs` next to the AI DLQ block; aero-server already depends on the connector (main.rs:251-258).

Multi-instance semantics: gauges read the shared table → identical values per instance (alert on any/max, no sum). Counters are per-instance → `sum()` across instances in PromQL.

### 1.4 Alert rules (floor-specific; additive to the runbook)

```promql
# Floor dead: backlog pending but zero backlog claims for 15m
# (all instances still old / floor regressed / relay down — relay-down is also
#  the §5.1 CRITICAL signature once that surface lands; overlap is intentional)
rate(aero_audit_claims_total{priority="10"}[15m]) == 0
  and aero_audit_outbox_pending_by_priority{priority="10"} > 0   # → WARN, for: 15m

# Floor lagging: backlog waiting > 1h. Bound when the floor works, at
# batch 100 / K=5: ceil(B/5) rounds × poll_interval; > 1h under load = broken
aero_audit_outbox_low_lane_lag_seconds > 3600                     # → WARN
```

Rule 1 is the merge gate's *operational* acceptance: after full rollout, a sustained admin flood with `priority="10"` pending and zero backlog claims is a regression alarm, not a silent behavior.

### 1.5 Sequencing decision

- **In this landing window** (cheap, additive, no migration, no config): the 3 series above + the `metrics.rs` file. The counter emit rides the relay change this slice already makes; the gauges reuse the perf-approved SQL shape. This is what makes "the floor works in prod" a *checkable* claim.
- **Separate follow-up (B5-2 observability debt, not this slice)**: the §5.1 zero-delivery surface — `transient_requeue` + `dead` gauges (needs 0247 terminal partial index, `WHERE status = 3`), `token_rejections_total` + `delivery_outcomes_total` counters, and enabling the runbook's CRITICAL rule. The runbook's §3 table already *documents* these as landed; the follow-up must reconcile the runbook to reality either way. Fold into the same follow-up as D-2 (they share the 0247 index and the `counts()` sampler) — see §2.3.
- No series naming conflict: `pending_by_priority`/`low_lane_lag_seconds`/`claims_total` are disjoint from §5.1's four names; `pending_by_priority` reuses the §5.1 "pending" predicate (attempts = 0) so the two surfaces partition status 0/1 rows without overlap.

---

## 2. D-2 — DB F1: terminal rows are never swept (decision + follow-up schedule)

### 2.1 Hazard: confirmed

Verified in-tree: the **only** `DELETE FROM audit_governance_outbox` statements are test cleanup inside `pg.rs` `#[cfg(test)]` (lines 430/523/628). No production sweep touches the table: `boot/retention.rs` sweeps `audit_events` (row DELETE + daily partition drop, `AERO__SERVER__AUDIT_RETENTION_DAYS` default 365), `ai_jobs`, `ai_usage`, webhook logs, notification bundles — **not the outbox**. 0239/0240 comments record no retention design. Consequences (perf engineer's measurements): the heap grows monotonically — 10M rows ≈ 7.8 GB — raising VACUUM cost and slowing any table-wide op (drill gate `COUNT(*)`, future migrations). The payload is a JSONB copy of the audit event: the outbox is a second, unswept retention store.

### 2.2 Adjudication: the naive sweep is contract-violating

The DB-architect F1 recommendation ("bounded sweep of `status IN (2,3)` by `delivered_at < now() − retention_window`") **conflicts with 0241's pinned invariant** — verified in the migration and reconcile code:

- 0241 `aero_reconcile_governance_audit` runs **before every claim batch** (`relay.rs::dispatch_batch` → `repo.reconcile`) and backfills `message.moderated` rows missed during a disabled window: `WHERE audit.action = 'message.moderated' AND NOT EXISTS (SELECT 1 FROM audit_governance_outbox WHERE event_id = audit.id)` over `audit_events` (best-effort join past retention).
- 0241's header pins: *"delivered rows are the durable reconciliation cursor… a delivered row is skipped, never re-delivered"* and *"dead rows are NEVER resurrected: status=3 rows exist in the outbox, so NOT EXISTS skips them"*.
- **Deleting a delivered row while its source `audit_events` row still exists (≤ 365 d) ⇒ the next reconcile re-inserts it status=0 ⇒ re-claim ⇒ re-deliver.** The sink's `Idempotency-Key: event_id` dedup only protects inside the sink's dedup window — the perf engineer's blanket rejection of "bounded retention on delivered" was based on exactly this.
- **Deleting a dead row re-opens it to reconcile resurrection ⇒ a re-attempt against a sink that may still 403**, silently defeating the manual-recovery posture (runbook §5: dead is the only non-replayed terminal).

### 2.3 Decision and follow-up scope (scheduled independently of this slice)

**Decision: land this slice without touching terminal rows (design non-goal stays); schedule a dedicated "outbox lifecycle" follow-up slice** — the largest persistence risk in this store, but orthogonal to the floor mechanism (the two-arm claim never reads status 2/3).

The follow-up's sweep predicate must be **reconcile-safe by construction** — sweep terminal rows only when their source is gone:

```sql
DELETE FROM audit_governance_outbox o
 WHERE o.status IN (2, 3)
   AND o.delivered_at < clock_timestamp() - make_interval(days => $audit_retention_days + $margin)
```

- **Why age ≥ audit retention + margin is safe**: the outbox row and its source `audit_events` row are created in the *same transaction* (0239 AFTER-trigger insert), so an outbox terminal row older than the audit retention window (365 d default) + sweep-granularity margin has no source row left ⇒ reconcile's join finds nothing ⇒ no resurrection. No `NOT EXISTS` join needed — and age-only is *required* for 0242 window/spill rows, whose `event_id` is a window/spill key, **not** an `audit_events.id` (a `NOT EXISTS` source probe would be vacuously true and sweep them early).
- **Open follow-up details to design there** (not this slice): status=3 rows have no `delivered_at` (mark_dead doesn't stamp it) → use `created_at`-based age for dead rows; sweep index (partial `WHERE status IN (2,3)` on the age column); env knob pattern (`AERO__SERVER__AUDIT_OUTBOX_RETENTION_DAYS`, mirroring the sibling sweeps in `boot/retention.rs`, default tied to audit retention + margin); set-based bounded batches (existing sweep pattern); best-effort warn-not-panic.
- **Effect**: the table becomes bounded by the audit retention footprint (365 d × rate) instead of growing forever. If that footprint is still too large at measured volumes, the follow-up also evaluates partitioning by `delivered_at` (heavier; not needed for correctness).
- **Fold together**: the §5.1 zero-delivery observability (0247 terminal index + transient/dead gauges + counters, §1.5) is the same lifecycle slice — it shares the 0247 index shape and the `counts()` sampler; one migration (0247), one sampler extension, one runbook reconciliation.
- Keep separate (recorded elsewhere): legacy `audit_governance_due_idx` cleanup (D5/P4), status-3 admin DLQ surface (D6).

---

## 3. D-3 — `AERO_PRIORITY_DRILL_ALLOW_TRUNCATE`: safety in production configs

### 3.1 Surface audit (verified)

- **Readers**: exactly two — the drill bin's in-transaction gate (`crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs:145-150`: `LOCK … ACCESS EXCLUSIVE` → `COUNT(*)` → refuse exit 1 unless `== "1"`) and the aero-eng wrapper's fast-fail layer (`crates/aero-eng/src/audit_provision.rs:714-727`, `run_priority`, spawns the drill with env inherited).
- **Production config surfaces**: clean. Not in `.env.example`, `config.example.toml`, `docker-compose.yml`, `Makefile`, or any boot path (`rg` across the repo outside the two readers + design docs: zero hits). The prod server never reads it.
- **Semantics**: fail-closed — only the exact string `"1"` allows; `"true"`/`"yes"`/empty refuse (harness-negative-tested: test-integration.sh REFUSED legs ×3, including the typo-value leg and a double-underscore-spelling hint); wrapper and drill are two independent layers (wrapper is UX fast-fail; the drill's in-tx LOCK+COUNT+TRUNCATE is authoritative for direct invocation).
- **Harness practice**: test-integration.sh sets `AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=1` only for the real drill run, against a throwaway DB, and drops the DB afterward; negative legs run with `0`/`true`/absent.

### 3.2 Verdict

**Safe in production configs as-is — no code change in this slice.** The gate's job is to prevent *accidental* destruction of a non-empty outbox; it does that: no prod surface can set the var by accident, and running the drill against a prod `DATABASE_URL` additionally requires manually invoking a destructive tool that is never wired into boot. The design's phase 2 re-runs the same gate helper before its own TRUNCATE (design §2.5 step 1, failure-table row) — the destructive surface grows by one TRUNCATE, all behind the same fail-closed gate; no new env, no new config.

### 3.3 Residual risks (operator error, not config) — document, don't code

| Edge | Scenario | Mitigation (runbook/AGENTS pins, no code) |
|---|---|---|
| Broad env export | Operator exports the var in a shell profile / shared env file, then runs the drill against prod `DATABASE_URL` with a non-empty outbox → TRUNCATE destroys real rows; drill rows are then stub-delivered (the drill's sink is the in-process `StubSink`, **never** the real endpoint — "PASS" on a prod DB means the real sink saw nothing). | Runbook line: "never set `AERO_PRIORITY_DRILL_ALLOW_TRUNCATE` outside a throwaway drill invocation; drill runs are throwaway-DB-only". The drill's usage header already says it. No software gate can protect a fully-informed operator; the two-layer gate already blocks everything accidental. |
| Empty-outbox edge | The gate's `n > 0` condition passes vacuously on an *empty* table: TRUNCATE is then a no-op, but the drill seeds 502+700 rows and stub-delivers them → fake-delivered pollution the real sink never saw. | Documented in the runbook. Tightening (require the env for every run, dropping `n > 0`) would change the landed phase-1 gate behavior — **out of this slice** (design: "Phase-1 statements are not edited"); record as an optional hardening if a future slice touches the gate. |

### 3.4 Test-coverage note for QA

Phase 2's gate reuses the same helper as phase 1's gate, whose negative behavior is harness-pinned ×3 — acceptable. A dedicated "phase-1 done, phase-2 refused" negative leg is not feasible without a driver (phase 2 runs inside the same binary as phase 1); cover the shared helper at unit level instead (one `#[cfg(test)]` assertion that the helper refuses without the env, mirroring the drill's existing D8′-style test module).

---

## 4. D-4 — Landing validation sequence

### 4.1 Rolling deploy compatibility

- **Mixed old/new fleet is safe**: old single-arm and new two-arm binaries share the table under the identical lease/fence protocol; both arms are `FOR UPDATE SKIP LOCKED`, so concurrent instances still partition the due set disjointly (async-reviewer interleaving proofs). `settle` is fenced on the claim token — a claim made by one version can only be settled/requeued/dead-marked by the same version's claim (claims are version-local; cross-version fence interference is structurally excluded).
- **Identity property during the window**: while the high lane is underfull, the new binary claims exactly today's top-`limit` set (probes S2/S4) — so a mixed fleet behaves byte-identically to today except where the floor engages (the intended change).
- **Floor liveness is fleet-wide only after full rollout**: each new instance reserves its own K per round, so a mixed fleet has partial floor coverage (M new instances ⇒ ≥ M×K low-lane slots per round window — monotone in rollout progress). Release note must pin: "the anti-starvation bound holds once all instances run the new binary".
- **No migration, no config, no env, no state-machine change** — deploy is a pure binary rollout; old binaries never write a shape the new one cannot read.

### 4.2 Rollback

Pure code revert (`pg.rs`/`relay.rs`/`fake.rs`/drill). Nothing to undo in schema, data, or config. At-least-once posture is identical on both sides, so a mid-window rollback re-delivers at most via the existing `Idempotency-Key: event_id` dedup; rows claimed by the new binary during the window are reclaimed by lease expiry under the old binary with fresh tokens (fenced state machine unchanged).

### 4.3 Pre-landing gates (this slice's AC set + review findings carried into implementation)

- AC1/AC4/AC5 drill legs + AC3/AC3.1 PG tests **serial** (`-- --ignored --test-threads=1` — the 3 existing PG tests share the table and fail in parallel; CI already serial, gate spec must pin it) on a fresh migrated throwaway DB.
- AC2 pins incl. `(i64::MIN) → 0` (QA F5); fake clamp parity pin `claim_due(0) → 1` in both fake and PG (QA F4); AC3.1 as `HashSet` equality, explicitly disjoint (QA F6).
- New tests: fake two-arm split + identity (QA F2), mixed-lane contention PG test (QA F3), requeued-low-lane recovery fake test (QA scenario 11).
- Design-doc amendments (doc-only, cheap, do at implementation): pin `arm_a AS MATERIALIZED` explicitly (async-reviewer F1); document snapshot-relative vs aggregate floor semantics (async-reviewer F2/F4); correct the failure-mode slack row with the `low_due = 0` backfill exception (DB-architect F4 — the empty low lane backfills admin rows from the MIN lane, **no throughput tax**); soften the S6 plan-shape claim ("Index Scan" is planner-dependent until ~100k rows; MIN walk measured 28.7 ms worst case, 350× inside the 10 s statement timeout) (DB-architect F2).
- Migration 0240's duplicated comment block (DB-architect F3): **leave in place** — editing a landed migration flips the embedded checksum; fix opportunistically in the D5 legacy-index cleanup migration instead.
- Full workspace gates: check / test --workspace --lib / clippy / truth-check / file-size-check (AC6 budgets: pg.rs ~720 < 800, relay ~650 < 800, drill ~505 < 800).

### 4.4 Post-landing data-integrity checks (throwaway DB, per AGENTS §4.3)

Sequence: `CREATE DATABASE` → full 244-migration replay (`aero-cli migrate`) → drill runs → integrity SQL → `DROP DATABASE`. Shared dev DB untouched (it is stale — 176/244 migrations — and must not be used for acceptance).

1. Drill AC1/AC5: RC 0; PASS lines in order — phase-1 (`moderation-in-first-batch`, `drain-502`, `parity-502`, `moderation-action-vocabulary`) then `starvation-round1-split-95-5`, `starvation-cross-batch-quota`, `starvation-drain-700`; `b5_check` slots :42/:43 PASS.
2. Integrity SQL (DB-architect + QA):
   - `COUNT(status = 2) == 700` and event_id set-parity vs the phase-2 seed set;
   - `attempts == 1` on every claimed row, zero `attempts > 1`;
   - `COUNT(DISTINCT claim_token) == COUNT(*)` over claimed rows (no duplicate claim mint);
   - no `status` outside {0,1,2,3} (CHECK-pinned anyway — belt-and-braces);
   - round-1 ∩ round-2 = ∅ (AC3.1 disjointness, HashSet assert);
   - T-11 boundary: rows=95 → `claimed == total`; rows=96 → fail-closed RC 1;
   - negative gate ×3 unchanged (REFUSED on non-empty outbox via wrapper and direct invocation, REFUSED on `=true`).
3. AC3 + AC3.1 + F3 PG tests, serial, on the same DB.
4. Optional one-off (not CI): a sustained-flood probe in the S5 shape (200 admin + 40 backlog, two rounds → 95a/5b each) — the durable replacements are AC3.1 + F3; probes stay out of CI.
5. Design record: D-CAP §7.1 status `scheduled → landed`; sibling-drill naming reconciled (extended leg of `aero-audit-priority-drill`, not the historical `aero-audit-min-service-drill` name).

### 4.5 Production watch window (24–72 h post-landing)

- Runbook greps (per shift): `last_error` LIKE `%audit token validation failed%` / `%unprovisioned%` / `%token endpoint rejected%`.
- New floor signals (once D-1 lands): `aero_audit_claims_total{priority="10"}` rate > 0 whenever backlog is pending; `aero_audit_outbox_low_lane_lag_seconds` bounded; rule 1/2 quiet.
- Rollback trigger: backlog claims flatline at 0 while backlog pending > 0 for 15m+ after full rollout (rule 1) → verify binary version on all instances first (mixed fleet is the likely cause, not the mechanism), then code-revert per §4.2.

---

## 5. Follow-up register (scheduled independently of this slice)

| # | Item | Trigger / owner | Contents |
|---|---|---|---|
| FU-1 | Outbox lifecycle (retention + §5.1 observability) | Post-landing, separate slice | D-2 sweep (reconcile-safe predicate §2.3), migration 0247 terminal index, `transient_requeue`/`dead` gauges + `token_rejections_total`/`delivery_outcomes_total` counters, runbook reconciliation (its §3 currently documents unlanded series), enable runbook CRITICAL rule |
| FU-2 | D5 legacy `audit_governance_due_idx` cleanup (P4) | After full rollout + lease/retention window (recorded) | Drop 0239 legacy index; fix 0240 duplicate comment block in the same migration |
| FU-3 | Status-3 admin DLQ surface (D6) | Recorded option | Mirror `ai_dlq` admin route; DB-only today |

## 6. In-slice decision log

| # | Decision | Choice | Rationale |
|---|---|---|---|
| L1 | Land D-1 observability in this slice vs separate | **Land now** | The floor is unobservable without it; the counter emit rides this slice's relay change; gauges use the perf-approved SQL shape; zero new deps/migration. §5.1 zero-delivery surface stays out (FU-1). |
| L2 | F1 retention in this slice vs follow-up | **Follow-up (FU-1)** | Naive sweep violates 0241's cursor invariant; correct predicate needs its own design + 0247 migration; orthogonal to the floor mechanism. |
| L3 | ALLOW_TRUNCATE gate hardening | **No code change** | Verified safe on every prod config surface; residual edges are operator-error, documented in the runbook. Phase-2 gate reuses the same helper. |
| L4 | 0240 duplicate comment | **Leave; fix in FU-2** | Editing a landed migration flips `_sqlx_migrations` checksums (AGENTS §4.2 build→migrate discipline); fix rides the D5 cleanup migration. |

# B5-1 item 1 — Implement L1 aggregation for the high-volume message.* backlog lane

> Module: `crates/aero-audit-connector/src` (delivery-side authority; the
> aggregation itself lives in `migrations/0242` + the leaf vocabulary in
> `crates/aero-common/src/model/audit.rs` + the parity suite in
> `crates/aero-storage/src/audit_governance.rs`).
> Source analysis: `docs/auto/analyses/crates-aero-audit-connector-src-7edb5949.json` (direction 1).
> Status: requirements specification, pre-implementation.
> **Read §0 first**: the analysis snapshot predates migration 0242; the L1
> aggregation mechanism has since been drafted and its suite verified green
> (live, this session). The residual delta is small and precisely bounded (§1).

## 0. Repo-state delta (analysis is stale — read this first)

The direction's core premise — "no window/merge SQL exists anywhere (only
test/drill INSERTs outside 0239/0241)" — **is false in the current tree**.
Between the analysis and today the L1 aggregation landed as an untracked
working-tree slice (never committed; HEAD `d5acefe` 2026-08-06, 315 dirty
files — the whole B5-1 campaign is in-flight):

| Artifact | Where | What it does |
|---|---|---|
| `migrations/0242_audit_governance_l1_aggregate.sql` | untracked | `aero_enqueue_l1_aggregate_audit()` AFTER INSERT trigger on `audit_events`; exact-token allowlist `message.create`/`message.edit` (fail-open pass-through for everything else); window key `md5(ws\|'message'\|floor(epoch/60))::uuid`; merge into **status = 0 only** (`ON CONFLICT DO UPDATE ... WHERE status = 0`), `ROW_COUNT = 0` ⇒ deterministic spill row `md5(v_key\|'\|'\|NEW.id)`; 20-key envelope (`aggregated`, `count`, `window_start/end`, `first/last_event_at`, `spill` on spill rows); no runtime gate, no binding lookup (D2) |
| `crates/aero-storage/src/audit_governance.rs` | untracked | `l1_window_aggregates_5_rows_to_1_outbox` (:1249), `l1_window_aggregates_concurrent_merge_serializes` (:1503, EvalPlanQual), `admin_rows_never_merged_into_l1_window` (:1586), `moderation_finalize_outbox_parity` (:252), `rust_produced_payload_matches_0239_envelope` (:455) — **all green on a fresh migrated DB (verified live, §2/§3)** |
| `crates/aero-audit-connector/src/bin/aero-audit-l1-parity-drill.rs` | untracked | Harness drill (self-seeds through the 0242 trigger, SUM(count)==COUNT(mapped) parity + spill leg) — **currently broken: 0227 workspace-owner guard (§3 F1)** |
| `crates/aero-audit-connector/src/bin/{aero-audit-t11-drill,aero-audit-relay-drill,aero-audit-priority-drill}.rs` | untracked | t11 seeds window + spill shapes (AC5); relay drill delivers mixed 1:1/window/spill through the stub sink (AC3 receipt path); priority drill pins AC4 — **all green on fresh DBs (verified live)** |
| `scripts/test-integration.sh` :590-634 / `scripts/b5-pin.sh` :68 | untracked | 0242 static arbiter (244 migrations, single `aero_enqueue_l1_aggregate_audit` definition) + `l1_window_aggregates_` db_tests slot + `l1-aggregation-drill` slot |

Sibling lanes also landed in the same in-flight slice: 0245 (room lane) and
0246 (`message.recalled` lane) — both **out of scope** here (direction 2 of
the same analysis).

What the direction still requires, verified against the tree:

- **AC3 is NOT landed**: the leaf typed twin `AuditClaimPayload`
  (`audit.rs:326`) is still the strict 16-key shape (`deny_unknown_fields`);
  nothing in Rust ever parses the 20-key 0242 aggregate envelope — the
  twin's "drift alarm" was silently bypassed for the aggregate shape (§3 F2).
  The parity drill `rust_produced_payload_matches_0239_envelope` covers the
  16-key envelope only; "updated in lockstep" has not happened.
- **The harness gate for the direction's core behavior is red**:
  `aero-audit-l1-parity-drill` fails on a fresh migrated DB at the fixture
  workspace insert (0227 effective-owner guard) — the `l1-aggregation-drill`
  harness slot would fail (§3 F1).
- Everything else in the supplied acceptance (AC1/AC2/AC4/AC5) is **landed
  and green**; the acceptance section re-states each in executable form with
  live-run evidence and marks the residual items.

## 1. Scope

Close the residual delta for the direction's own title — L1 aggregation for
the high-volume `message.*` backlog lane — **without re-opening the landed
mechanism**:

- **R1** — typed-twin extension: the leaf's typed coverage of the outbound
  envelope family gains the aggregate shape, and the parity drill is updated
  in lockstep (AC3).
- **R2** — fix `aero-audit-l1-parity-drill` (the 0227 workspace-owner guard;
  the drill's own parity/spill logic is otherwise sound and its harness slot
  is the executable gate for the direction's O(windows) claim).
- **R3** — no-touch list: every already-green artifact stays green (§4).

**Explicitly out of scope** (recorded, not required):
- **The aggregation mechanism itself** — 0242's window/merge/spill SQL is
  landed and pinned; no SQL change, no new migration (the 244-count and
  single-definition arbiters at `test-integration.sh:597-611` must not move).
- **Room lane (0245) and `message.recalled` lane (0246)** — sibling
  direction (analysis direction 2); their tests ride the same module filter
  and must stay green, but no work here.
- **Relay claim-model changes** — with aggregation the outbox grows
  O(windows) not O(N); the existing `SKIP LOCKED`/`MAX_CLAIM=500`/`status IN
  (0,1)` claim CTE (`pg.rs:105-130`) is shape-agnostic (payload forwarded
  untyped). The problem statement's "scaling bottleneck" is the *motivation*
  for 0242, not a second work item.
- **0243/0244** — auth-slice migrations, designed-only, untouched.
- **B5-4 provisioning gate** — separate direction in the analysis.
- **Editing landed migration text** (sqlx checksum discipline, 0239
  precedent) — including 0242's.

## 2. Evidence verification (all citations re-checked against the repo)

| # | Cited evidence | Verified result |
|---|---|---|
| E1 | `migrations/0239_audit_governance_outbox.sql` — trigger `aero_enqueue_governance_audit`: `IF NEW.action <> 'message.moderated' THEN RETURN NEW`; class/priority DDL + CHECKs | ✅ Confirmed (untracked file). Fail-open early return at :83-84; DDL `class TEXT NOT NULL DEFAULT 'message' CHECK (class IN ('admin','message','room'))` (:33), `priority SMALLINT NOT NULL DEFAULT 10 CHECK (priority > 0)` (:34); moderation stamp class `'admin'`/100. |
| E2 | `migrations/0241_governance_reconcile.sql` — token-keyed, message.moderated-only backfill | ✅ Confirmed (untracked file). Header: "TOKEN-KEYED scan … the v2 lane is `message.moderated`-only"; `WHERE audit.action = 'message.moderated'`; relay calls `aero_reconcile_governance_audit($1)` before every claim batch (`pg.rs:91`). **Live-observed side effect**: on a shared DB it backfilled orphaned `message.moderated` audit rows left by earlier db_test runs (t11 claimed 10 ≠ 7) — correct reconciler behavior; the harness isolates drills on per-drill throwaway DBs. |
| E3 | `crates/aero-common/src/model/audit.rs:165` — `GOVERNANCE_CLASS_MESSAGE` 'L1-aggregatable'; `MODERATION_OUTBOUND_ACTION` single-literal site | ⚠️ Line drift. `MODERATION_OUTBOUND_ACTION` now :150; `GOVERNANCE_CLASS_MESSAGE` now :224 with the exact doc "High-volume message backlog class (L1-aggregatable)" (:223); full L1 vocab block :158-200 (`LOCAL_ACTION_MESSAGE_CREATE` :168, `LOCAL_ACTION_MESSAGE_EDIT` :170, `AGGREGATED_MESSAGE_ACTION` :175, `AUDIT_SOURCE_SYSTEM` :182, `L1_WINDOW_SECONDS=60` :185). Canonical-value pins in `l1_vocabulary_consts_are_pinned`. |
| E4 | `crates/aero-ai/src/governance.rs:86` — `is_admin_class`, '[PROPOSED] L1 aggregation bypass' | ⚠️ Drifted and **superseded**: `is_admin_class` now :120; the doc comment (:114-116) reads "R5 classification for the L1 aggregation bypass (**landed: migration 0242** `aero_enqueue_l1_aggregate_audit` …)" — the [PROPOSED] marker is gone. `governance_lane_for` :81 has three arms (message.moderated→admin/100, room.create/room.archived→room/10); `GOVERNANCE_PRIORITY_MODERATION=100` :31, `GOVERNANCE_PRIORITY_BACKLOG=10` :33. Admin rows stay 1:1 (`event_id` = `audit_events.id`), never merged — pinned by `admin_rows_never_merged_into_l1_window` (:1586). |
| E5 | `crates/aero-audit-connector/src/pg.rs` — claim CTE `ORDER BY priority DESC`, `status IN (0,1)` | ✅ Confirmed :105-130: `WHERE candidate.status IN (0, 1) AND candidate.available_at <= clock_timestamp() AND (lease_expires_at IS NULL OR …)` `ORDER BY candidate.priority DESC, candidate.available_at, candidate.created_at, candidate.event_id FOR UPDATE SKIP LOCKED LIMIT $1`; `aero_reconcile_governance_audit` called at :91 before the claim. `mixed_priority_claim_orders_moderation_first_then_fifo` at :546 — **PASS live** (§5 AC4). |
| E6 | `crates/aero-audit-connector/src/relay.rs` — `MAX_CLAIM=500`, `MAX_BACKOFF_SECONDS=300` | ✅ Confirmed :35 (`pub const MAX_CLAIM: i64 = 500`) and :33 (`pub const MAX_BACKOFF_SECONDS: i64 = 300`); claim limit clamped `limit.clamp(1, MAX_CLAIM)` (pg.rs:92,105). |
| E7 | `crates/aero-common/src/model/audit.rs` — typed 16-key envelope twin, `deny_unknown_fields` — "the drift alarm an aggregate envelope must extend" | ✅ Confirmed :316-356 (`AuditClaimPayload`, 16 fields, `#[serde(deny_unknown_fields)]`; doc: "a future SQL-side envelope addition fails the drill's fail-closed parse until this struct is updated — the intended drift alarm"). **The extension has NOT happened**: no `aggregated`/`window_start`/`first_event_at`/`count`/`spill` fields anywhere in the struct, no `#[serde(default)]`; the 0242 20-key envelope is never parsed by any Rust type (§3 F2). |
| E8 | `crates/aero-audit-connector/src/client.rs` — `validate_audit_receipt` | ✅ Confirmed :552 (`event_id` match value-level via `receipt_event_id_matches` :537, `accepted_at` present, `conflict` false, status ∈ {ledgered,indexed,archived}; permanent class). Receipt path for aggregate claims proven by the relay drill (§5 AC3) — the stub echoes `payload.event_id` (window row's own PK), which is exactly what the drill's mixed-shape leg exercises. |
| E9 | `crates/aero-storage/src/audit_governance.rs` — parity fixture `rust_produced_payload_matches_0239_envelope` | ✅ Confirmed :455 (16-key, `deny_unknown_fields` fail-closed parse, exact-wire-text half A + Rust-produced half B). **Not updated for the aggregate envelope** (AC3's "in lockstep" clause — [OPEN]). |
| E10 | Harness: `moderation_finalize_outbox_parity` named entry, "37/37 slot" | ⚠️ Slot confirmed (`test-integration.sh:328`, `b5-pin.sh:39`) but the count has grown: `scripts/b5-pin.sh` `B5_CONTRACT_TEST_LIST` is now **18 executed + 22 [PROPOSED] = 40** (the analysis-time "37/37" is stale; `l1-aggregation-drill` at b5-pin.sh:68, `room_lane_outbox_parity`/`message_lane_outbox_parity` at :69-70 are the additions). The slot list is pinned — R1/R2 must not add/remove/rename slots. |
| E11 | Migration numbering | ✅ 0239/0240/0241/0242/0245/0246 all present as untracked files; 244 total (`ls migrations/*.sql | wc -l` = 244 — matches the arbiter literal at `test-integration.sh:597-605`); 0243/0244 designed-only, absent. R1/R2 add **no** migration ⇒ arbiter stays 244. |

## 3. Additional findings (beyond the cited evidence; all live-verified this session on fresh throwaway DBs, `aero-postgres` pgvector/pg17)

- **F1 — `aero-audit-l1-parity-drill` is broken: fails on a fresh migrated DB.**
  `Error: insert drill workspace … workspace … must retain at least one
  effective non-guest owner before commit` — the 0227
  `workspace_effective_owner_guard` migration (commit-time guard) requires a
  `workspace_members` row with role `'owner'`; the drill inserts the
  workspace standalone (bin :129-140) with no membership. The db_tests'
  `fixture()` (:52-80) wraps workspace + owner-member inserts in one tx —
  the drill must mirror it. Consequence: the harness slot
  `l1-aggregation-drill` (`test-integration.sh:616-628`) is red today, i.e.
  the direction's O(windows) parity gate cannot pass. The drill's parity and
  spill logic after the fixture point is sound (live-verified up to the
  fixture failure; the same SQL shapes pass in the db_tests).
- **F2 — the 16-key twin's drift alarm is bypassed for the aggregate
  envelope.** `deny_unknown_fields` only fires when something *parses* the
  payload; the 0242 envelope is produced by SQL and forwarded untyped
  (`Claim.payload: serde_json::Value`), and the l1 db_tests assert it via
  `jsonb` value access, never a typed parse. So a future drift in the
  0242 envelope (e.g. `AGGREGATED_MESSAGE_ACTION` literal drift inside the
  SQL) reds only in the db_test's literal-equality assertions — the typed
  "drift alarm" the direction cites is silent for this shape. AC3's
  typed-twin extension closes exactly this gap.
- **F3 — t11 drill on a shared DB claims more than seeded (10 ≠ 7):** the
  0241 reconciler backfilled leftover `message.moderated` audit rows from
  earlier db_test runs on the same DB — correct reconciler behavior, not a
  defect. The harness isolates drills on per-drill throwaway DBs; the t11
  drill passes clean there (7/7, both rounds — §5 AC5).
- **F4 — relay needs no change.** The claim CTE and delivery path are
  shape-agnostic; the relay drill delivers 1:1 + window + spill rows through
  the full token/events/receipt cycle (3 delivered status 2, 2 negative
  controls dead, 0 stuck — PASS). `MAX_CLAIM=500` bounds O(windows) rows.
- **F5 — whole B5-1 slice is untracked/in-flight.** 0239-0246, the storage
  parity suite, the drills, and the harness edits are all working-tree files
  under HEAD `d5acefe`. Any implementation must not assume these are
  committed; R1/R2 ride the same in-flight state.
- **F6 — 0242 envelope is deliberately actor/targets-free.** The l1 db_test
  asserts `actor`, `targets`, `tenant_id`, `window_id`,
  `first_event_id`, `last_event_id` are **forbidden keys** on the aggregate
  envelope (:1414-1424). A single-struct typed-twin extension would have to
  weaken the required `actor`/`targets`/`payload` fields of the 16-key twin
  to parse both shapes — rejected in D1 (separate aggregate twin keeps the
  1:1 alarm strict).

## 4. Requirements

### R1 — Typed-twin extension: the aggregate envelope gains a typed shape + lockstep parity drill (closes AC3)

**D1 (shape decision, with the alternative rejected):** add a second typed
struct `AuditAggregatePayload` in `crates/aero-common/src/model/audit.rs`
next to the 16-key twin (same module, same `deny_unknown_fields`), with all
20 keys of the 0242 envelope:
`event_id, source_system, event_type, schema_id, schema_version, occurred_at,
aggregate_type, aggregate_id, action, outcome, data_classification,
retention_class, idempotency_key, count, aggregated, window_start,
window_end, first_event_at, last_event_at` + `spill: Option<bool>` (present
only on spill rows). Field order = 0242 `jsonb_build_object` document order
(the exact-wire-text pinning precedent of the 16-key twin). **Rejected
alternative:** extending the 16-key twin with defaulted Option fields —
making `actor`/`targets`/`payload` optional would silently accept a 0239
envelope that lost those keys, weakening the exact failure the existing
drill exists to catch (F6). The aggregate shape is a *different* envelope
family (no actor/targets by arbitration), so it gets its own strict twin.

- `AuditClaimPayload` (16-key) is **untouched** — `deny_unknown_fields`,
  required fields, constructor, and all existing pins stay.
- The new struct spells its wire constants through the existing leaf consts
  only (`AGGREGATED_MESSAGE_ACTION`, `AUDIT_SOURCE_SYSTEM`,
  `AUDIT_EVENT_TYPE`, `AUDIT_SCHEMA_ID`, `AUDIT_SCHEMA_VERSION`,
  `AUDIT_AGGREGATE_TYPE`, `AUDIT_OUTCOME_SUCCESS`,
  `AUDIT_DATA_CLASSIFICATION`, `AUDIT_RETENTION_CLASS`) — never inline
  literals (the 0239 twin's "single legal literal site" rule).
- **Lockstep drill update** (`crates/aero-storage/src/audit_governance.rs`):
  extend `rust_produced_payload_matches_0239_envelope` (:455) — or add a
  sibling test in the same module — with an aggregate leg: seed N
  `message.create` audit rows in one tx (same window, fixed `created_at`),
  read the real trigger-produced window row, parse its payload into
  `AuditAggregatePayload` (fail-closed), and assert every field from the
  leaf consts (`count == N`, `aggregated == true`, `spill` absent,
  `action == AGGREGATED_MESSAGE_ACTION`, window bounds recomputed from
  `L1_WINDOW_SECONDS`, `occurred_at == window_start`). Probe-gated on
  `aero_enqueue_l1_aggregate_audit` existing (the `l1_aggregate_migrated`
  pattern :1169) so the module slot stays green pre-0242. Because the test
  lives in the `audit_governance::` module, the existing harness slot
  (`test-integration.sh:321-331`) picks it up — **no new b5-pin slot**.
- **Receipt path needs no new code** (AC3's first clause): the relay drill
  already settles window/spill claims through `validate_audit_receipt`
  (stub echoes `payload.event_id` = the row's own PK; `receipt_event_id_matches`
  is value-level). R1 only closes the *typed parse* gap.

### R2 — Fix `aero-audit-l1-parity-drill` (harness gate for the direction's core behavior)

`crates/aero-audit-connector/src/bin/aero-audit-l1-parity-drill.rs` must
satisfy the 0227 `workspace_effective_owner_guard` commit-time constraint:
wrap the workspace insert (:129-140) and a `workspace_members` insert
(`role = 'owner'`, participant = the drill's actor) in **one transaction**,
mirroring the db_tests fixture (`audit_governance.rs:52-80`) — a standalone
`workspace_members` insert after commit would still violate the guard's
at-commit invariant and, worse, a committed ownerless workspace cannot be
retroactively fixed under the same guard. No other drill logic changes:
self-seeding through the 0242 trigger, the `SUM(count) == COUNT(mapped)`
parity query (retention-scoped), the spill leg (force window status 2 →
late same-window row → exactly one spill row, own deterministic key), exit 2
SKIP probes — all verified sound up to the fixture point. The harness slot
`l1-aggregation-drill` (`test-integration.sh:616-628`) then PASSes with no
harness edits.

### R3 — No-touch requirements (verify-only; every landed pin must stay green)

- **Landed SQL untouched**: 0239/0240/0241/0242/0245/0246 — sqlx checksum
  discipline; the 244 migration-count arbiter and both single-definition
  arbiters (`aero_enqueue_l1_aggregate_audit` in exactly 0242,
  `aero_enqueue_room_audit` in exactly 0245) unchanged.
- **Aggregation contract** (AC1/AC2): `l1_window_aggregates_5_rows_to_1_outbox`
  (:1249) — 6 rows → 1 window row, status 0, class `'message'`, priority 10,
  count 6, forbidden keys absent, rollback half → 0 rows, second window →
  2nd row, second workspace → 3rd row; `l1_window_aggregates_concurrent_merge_serializes`
  (:1503) — READ COMMITTED EvalPlanQual, exactly 1 row count 2;
  `admin_rows_never_merged_into_l1_window` (:1586) and
  `moderation_finalize_outbox_parity` (:252) — admin 1:1 parity.
- **Claim ordering** (AC4): `mixed_priority_claim_orders_moderation_first_then_fifo`
  (`pg.rs:546`) and the `aero-audit-priority-drill` harness slot unchanged.
- **T-11 fail-closed** (AC5): `aero-audit-t11-drill` window/spill seeds and
  the `t11-fail-closed` harness slot unchanged.
- **Sibling lanes**: `room_lane_*` / `recall_lane_outbox_parity` /
  `message_lane_outbox_parity` tests stay green (they ride the same module
  filter).
- **b5-pin slot list**: no add/remove/rename (R1's test rides the existing
  `audit_governance::` filter; R2 is a drill fix, not a new slot).

## 5. Acceptance criteria (testable)

> Commands assume a throwaway Postgres (`CREATE DATABASE` → `cargo build` →
> `aero-cli migrate` → run → `DROP DATABASE`, AGENTS.md §4.1/§4.3; migrations
> are compile-time embedded, so build before migrate). Supplied acceptance
> items are preserved verbatim in intent; each is restated in executable form
> against the current tree, with residual items marked **[OPEN]** and
> live-verified results marked *(verified 2026-08-09)*.

**AC1 — N message.* audit rows inside one bounded window collapse to 1 outbox row (status 0, class 'message', priority 10) — set-based test like pg.rs `mixed_priority_claim_orders_moderation_first_then_fifo`.**
*Landed + green.* `DATABASE_URL=<fresh migrated throwaway> cargo test -p aero-storage --lib --locked "audit_governance::db_tests::" -- --ignored --test-threads=1` → 15/15 pass, including `l1_window_aggregates_5_rows_to_1_outbox` (5 `message.create` + 1 `message.edit`, one tx, one window ⇒ exactly 1 outbox row: `status=0`, `class=GOVERNANCE_CLASS_MESSAGE`, `priority=10`, key = `md5(ws|class|floor(epoch/60))` recomputed from leaf consts, `count=6`, forbidden keys absent; rollback half ⇒ 0 rows; second window ⇒ 2nd row; second workspace ⇒ 3rd row) and `l1_window_aggregates_concurrent_merge_serializes` (2 parallel txs, 1 row, count 2). *(verified: full suite ran green on a fresh 244-migration DB.)* Harness slot `l1_window_aggregates_` (`test-integration.sh:618-635`) PASS.

**AC2 — Admin-class rows never merged: 1:1 parity guard (`is_admin_class`) keeps `moderation_finalize_outbox_parity` green.**
*Landed + green.* Same db_tests run includes `admin_rows_never_merged_into_l1_window` (a `message.moderated` row next to N `message.create` rows in the same window stays its own 1:1 outbox row with `event_id = audit_events.id`, class `'admin'`, priority 100, never folded into the window) and `moderation_finalize_outbox_parity`. `cargo test -p aero-ai --lib` green for the `is_admin_class` pins (:114-120; `user_delete_token_stays_out_of_admin_lane`, `unknown_local_token_passes_through_unmapped`). *(verified: both db_tests green live.)* Harness slot `moderation_finalize_outbox_parity` (`test-integration.sh:328-333`; b5-pin.sh:39 — the list is now 18 executed + 22 [PROPOSED] = 40, grown from the analysis-time 37/37) PASS.

**AC3 — Aggregate payload passes `validate_audit_receipt` after typed-twin extension — drill `rust_produced_payload_matches_0239_envelope` updated in lockstep.** **[OPEN: R1]**
- Receipt clause (no code needed): `DATABASE_URL=<fresh migrated throwaway> cargo run -p aero-audit-connector --bin aero-audit-relay-drill` → `PASS: 3/3 delivered (status 2), event_id set-parity exact, 2 dead (negative controls), 0 stuck, stub POSTs 4` — window and spill claims settle through `validate_audit_receipt` (the stub echoes `payload.event_id`; value-level match). *(verified live.)*
- After R1: `AuditAggregatePayload` (20 keys, `deny_unknown_fields`) exists next to the 16-key twin; `rust_produced_payload_matches_0239_envelope`'s new aggregate leg parses a real trigger-produced window row fail-closed and asserts every field from the leaf consts; the 16-key twin and its existing assertions are byte-unchanged. The `audit_governance::` harness filter runs the extended drill on a migrated throwaway DB and passes (empty-filter guard = no vacuous green). **[OPEN until R1 lands]**

**AC4 — Moderation priority unchanged: `aero-audit-priority-drill` (500 backlog + 1 admin, batch 100) still claims the admin row in round 1.**
*Landed + green.* `DATABASE_URL=<fresh migrated throwaway> AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=1 cargo run -p aero-audit-connector --bin aero-audit-priority-drill` → `drill: moderation-in-first-batch: PASS` + `moderation-action-vocabulary: PASS` + `drain-501: PASS` + `parity-501: PASS`. `DATABASE_URL=<fresh> cargo test -p aero-audit-connector --lib --locked "mixed_priority" -- --ignored` → `mixed_priority_claim_orders_moderation_first_then_fifo ... ok` (40 backlog priority 10 earlier-available + 10 admin priority 100 later-available, claim 25 = {10 admin} ∪ {15 earliest backlog}). *(both verified live.)* Harness slot `moderation-priority-drill` (`test-integration.sh:462-547`) PASS.

**AC5 — T-11: `aero-audit-t11-drill` with aggregated rows seeded — relay absent ⇒ all rows stay status 0, COUNT(status IN (1,2,3)) == 0.**
*Landed + green.* `DATABASE_URL=<fresh migrated throwaway> cargo run -p aero-audit-connector --bin aero-audit-t11-drill` → `round 1: 7/7 pending, 0 terminal, attempts sum 7, 7/7 recorded the transport failure` / `round 2: 7/7 pending, 0 terminal, attempts sum 14` / `drill: t11-pending: PASS` — seeded total = 7 (3 1:1 + 1 window + 1 spill + 1 admin + 1 room); per-round `COUNT(status=0) == 7`, `COUNT(status IN (1,2,3)) == 0`, `SUM(attempts) == 7×round`, `COUNT(last_error LIKE '%audit connector HTTP transport failed%') == 7`; per-shape evidence for window/spill keys (`count`/`aggregated` markers intact). *(verified live on a fresh DB; on a shared DB with leftover `message.moderated` audit rows the 0241 reconciler backfills them and the claimed count shifts — the harness's per-drill throwaway DBs avoid this, F3.)* Harness slot `t11-fail-closed` (`test-integration.sh:362-453`) PASS.

**AC6 — [R2] `l1-aggregation-drill` harness gate green.** **[OPEN: R2]**
After R2, `DATABASE_URL=<fresh migrated throwaway> cargo run -p aero-audit-connector --bin aero-audit-l1-parity-drill` exits 0 with: `parity after self-seed: SUM(count) == COUNT(mapped) == N` (N `message.create` rows through the trigger ⇒ 1 window row), spill leg `SUM = N+1 == COUNT = N+1` with exactly one `spill=true` row (own deterministic key, `payload.event_id` = own PK, `count=1`). Today it exits 1 with the 0227 owner-guard error *(verified live — the only red in the direction's check set)*. Harness slot `l1-aggregation-drill` (`test-integration.sh:616-628`; b5-pin.sh:68) PASS after R2.

**Cross-checks required before merge** (AGENTS.md §4.3): `cargo build` (before migrate) · `cargo check --workspace` clean · `cargo test --workspace --lib` green · `cargo clippy --workspace --all-targets` no new warnings · `scripts/{truth-check,file-size-check,web-check}.sh` 0 violations · `scripts/b5-pin.sh` guard clean (slot list unchanged).

## 6. Change surface (complete list)

| File | Change |
|---|---|
| `crates/aero-common/src/model/audit.rs` | R1: new `AuditAggregatePayload` (20-key, `deny_unknown_fields`, field order = 0242 document order, leaf-const-only literals, `spill: Option<bool>`); 16-key `AuditClaimPayload` untouched |
| `crates/aero-storage/src/audit_governance.rs` | R1: `rust_produced_payload_matches_0239_envelope` (:455) gains the aggregate leg (trigger-produced window row → typed parse → field-by-field leaf-const assertions; probe-gated on `l1_aggregate_migrated`) |
| `crates/aero-audit-connector/src/bin/aero-audit-l1-parity-drill.rs` | R2: fixture workspace + `workspace_members` owner row in one tx (0227 guard), mirroring the db_tests fixture; nothing else |
| `migrations/0239/0240/0241/0242/0245/0246` | **No change** (landed text untouched, sqlx checksums; no new migration — the 244 arbiter stays) |
| `scripts/test-integration.sh` | **No change** (R1's test rides the existing `audit_governance::` filter :321-331; R2's drill is a fix, slot already exists) |
| `scripts/b5-pin.sh` | **No change** (slot list pinned) |
| `crates/aero-audit-connector/src/{pg,relay,client}.rs` | **No change** (claim model, MAX_CLAIM, receipt validation all shape-agnostic — verified) |

## 7. Risks and notes

- **Stale-analysis drift is the biggest risk**: the direction's problem
  statement ("no window/merge SQL exists anywhere") describes a tree that no
  longer exists. Any implementation must treat 0242/0245/0246 as landed and
  pinned; "write the aggregation trigger" is wrong — the residual is R1+R2.
- **Never weaken the 16-key twin** (D1): making `actor`/`targets`/`payload`
  optional so one struct parses both shapes would silently accept a 0239
  envelope missing those keys — the exact drift the parity drill catches.
  The aggregate envelope is actor/targets-free **by arbitration** (forbidden
  keys, l1 db_test :1414-1424), so it needs its own strict twin.
- **0227 guard is commit-time**: R2's fix must insert the owner row in the
  same tx as the workspace (the db_tests fixture precedent); a post-commit
  insert would still violate the at-commit invariant.
- **0241 reconciler on shared DBs** (F3): the t11 drill's `claimed == total`
  assertion is DB-isolation-dependent. The harness already grants per-drill
  throwaway DBs — do not "fix" the drill to tolerate backfills, and never
  run drills on a DB with leftover `message.moderated` audit rows when
  evaluating AC5.
- **Slot list is pinned**: the analysis-time "37/37" is now 40 (18 executed
  + 22 [PROPOSED]); neither R1 (rides `audit_governance::` filter) nor R2
  (existing slot) adds/removes/renames a slot — the b5-pin guard stays.
- **Relay scaling claim**: with 0242 the outbox grows O(windows); the
  problem statement's MAX_CLAIM/SKIP-LOCKED "bottleneck" is resolved by the
  aggregation itself. Touching the claim CTE or relay would be scope
  expansion, not completion.
- **In-flight state** (F5): the whole B5-1 slice is uncommitted; R1/R2
  extend the same working tree. Do not `git reset` or rebase; land as part
  of the slice.

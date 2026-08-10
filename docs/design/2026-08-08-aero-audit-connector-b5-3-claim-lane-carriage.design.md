# B5-3 (connector slice) — priority/class carried on `Claim` + `FakeOutbox`, lane-aware fake sort, A1 unit coverage

> Module: `crates/aero-audit-connector` (trait/fake/test seams).
> Requirement record: `docs/requirements/2026-08-08-aero-audit-connector-b5-3-claim-lane-carriage.req.md`
> (R1–R6, AC1–AC4; supersession §0-A2 of the sibling record
> `docs/requirements/2026-08-07-aero-ai-b5-3-priority-claim-ordering.req.md`).
> Sibling design (PG ORDER BY + 0240 index, landed): `docs/design/2026-08-07-aero-ai-b5-3-priority-claim-ordering.design.md`
> (D1–D3).
> Status: **verified landed — REV 3** (2026-08-08). REV 2's verification stands
> unchanged; REV 3 is doc-only — it records the backlog-lane starvation bound
> (F9) and the acceptance decision (D7). The sanctioned mitigation is the
> sibling record's scheduled D-CAP anti-starvation cap, which preserves the
> `priority DESC` invariant. §5 change surface is the commit of the in-flight
> untracked set, no further code delta.

## 0. Evidence verification verdict

Every claim in the requirement record was re-verified against the working
tree on 2026-08-08 (REV 2). All ten verified-reality rows hold verbatim; the
two "stale — landed" rows are confirmed landed; the **two core-gap rows
(`claim_due` ordering, `Claim`/`FakeOutbox` lane carriage) are also landed**
since this record's first revision — no gap remains open, no new discrepancy
found. This design is therefore the landed design, not a proposal.

| Requirement record claim | Live verification (2026-08-08) |
|---|---|
| `PgOutboxRepo::claim_due` orders `priority DESC, available_at, created_at, event_id` | ✅ pg.rs claim CTE, verbatim (`ORDER BY candidate.priority DESC, candidate.available_at, candidate.created_at, candidate.event_id` + `FOR UPDATE SKIP LOCKED`, pg.rs:111-113); PG test `mixed_priority_claim_orders_moderation_first_then_fifo` (pg.rs:530, set-based `{10 admin} ∪ {15 earliest backlog}`) |
| Drill `BACKLOG_PRIORITY=10` / `MODERATION_PRIORITY=100`, "Production values, not [PROPOSED]" | ✅ drill:55-62; docstring FAIL-not-SKIP verbatim; seeds `class='admin'`/`'message'`, action `admin.content.flag`; `scripts/b5-pin.sh:41` slot; `scripts/test-integration.sh:459` `b5_check "moderation-priority-drill" "PASS"` |
| `Claim` carries the lane (R1) | ✅ outbox.rs:43/:46 `priority: i16` + `class: String` — 7 fields; trait `claim_due` doc pins the DESC ordering and the lane carriage |
| `ClaimRow` selects 7 via `RETURNING`; `From` maps 7 | ✅ `RETURNING outbox.event_id, …, outbox.payload, outbox.priority, outbox.class` (pg.rs:126-127); `ClaimRow` `priority: i16, class: String` (:55-56); `From<ClaimRow> for Claim` maps them (:69-70) |
| `FakeOutbox` is lane-aware (R3) | ✅ `insert_lane(event_id, payload, due_at, priority, class)` (fake.rs:133); `insert` 3-arg delegates with 0239 defaults 10/"message" (:117-126); due sort mirrors PG byte-for-byte `(Reverse(priority), available_at, created_at, id)` (fake.rs:217-218); `FakeRowSnapshot` + `Claim` expose the lane |
| Fixture `ensure_outbox_table` has `priority` AND `class` | ✅ pg.rs:285-286 both columns, mirroring 0239 defaults verbatim (REV 1's E10 gap closed) |
| `claim_validation.rs::claim()` literal constructor | ✅ (claim_validation.rs:37-49) gains `priority: 10, class: "message".into()` with R4 pin comment; 11 tests in the file |
| governance.rs:31-40 constants | ✅ :31 `GOVERNANCE_PRIORITY_MODERATION: i16 = 100`, :33 `GOVERNANCE_PRIORITY_BACKLOG: i16 = 10`, :37/:39/:41 `GOVERNANCE_CLASS_{ADMIN,MESSAGE,ROOM}`; :56 `MODERATION_OUTBOUND_ACTION = "admin.content.flag"`; :11-25 DESC-vs-ASC warning |
| 0239 + 0240 + 0241 landed | ✅ 0239 (`priority SMALLINT NOT NULL DEFAULT 10 CHECK (priority > 0)`, `class TEXT NOT NULL DEFAULT 'message' CHECK (class IN ('admin','message','room'))`, trigger stamp admin/100); 0240 `audit_governance_due_prio_idx (priority DESC, available_at, created_at, event_id) WHERE status IN (0,1)`; 0241 token-keyed `aero_reconcile_governance_audit` |
| Relay consumes only a subset of `Claim` | ✅ relay.rs `deliver_claim` copies `event_id`/`claim_token`/`attempts`; client.rs uses `claim.event_id` + `claim.payload` only — additive fields are safe |
| 7 state_machine tests incl. the A1 priority test | ✅ 6 existing (stale-token, backoff, permanent-dead, 403-dead, happy-path, skew) + `priority_first_claim_preempts_fifo_and_limit1_keeps_top_lane` (tests/state_machine.rs:334) — 7/7 green |
| T-11 regressions exist | ✅ `forbidden_dead_on_first_attempt`, `transient_5xx_requeues_and_rotates_a_fresh_token`, `transient_claim_drift_requeues_without_any_post` (relay.rs unit tests) — green |
| `aero-ai` `moderation_lane_preempts_backlog_under_desc_claim` exists | ✅ governance.rs:119 |

Evidence runs (2026-08-08): `cargo check -p aero-audit-connector --all-targets` clean;
`cargo test -p aero-audit-connector --lib --tests`: **8 lib + 11 claim_validation + 7
state_machine passed, 0 failures** (3 PG `#[ignore]`); no `aero-ai` import (comment-only
pins); `rg "\b200\b"` finds only stub HTTP 200s / `t0−200s` offsets / `make_interval(secs
=> 200)` — no stale lane literals.

## 1. Design overview

The PG ordering (sibling slice R1), the drill constants, and the trait/fake
carriage are **all landed** (verified §0). This section describes the built
design — the five deltas, as implemented:

1. **`Claim` carries the lane** — `priority: i16` + `class: String`, so the
   ordering/classification is observable on the seam without a DB roundtrip
   (A1; the future B5-4 heartbeat settle path needs no lane lookup).
2. **PG impl populates the lane** — `ClaimRow` + `RETURNING` + `From` map the
   two 0239 columns; the minimal test fixture gains `class` (it has
   `priority` today, so `RETURNING outbox.class` would break non-0239
   throwaway-DB tests without this).
3. **`FakeOutbox` is lane-aware** — `insert_lane` seed API (existing `insert`
   delegates with the 0239 defaults so all 7 existing `insert` call sites —
   6 state_machine tests + 1 relay unit-test helper — stay untouched), priority-first claim sort mirroring the PG CTE byte-for-byte,
   lane exposed on `FakeRowSnapshot` and `Claim`. Because the fake builds its
   returned `Vec<Claim>` from its sorted due list (unlike PG's
   `UPDATE … FROM … RETURNING` heap order — D3), **Vec position IS the fake's
   ordering contract** and is assertable in A1.
4. **A1 unit test** — pinned clock, one moderation row enqueued last with a
   later `available_at` wins `claims[0]`; `limit = 1` variant; FIFO preserved
   within the backlog lane.
5. **Compile surface** — `claim_validation.rs::claim()` gains the two fields.

No migration, no `Cargo.toml` change, no drill change, no relay/client/stub
behavioral change.

## 2. API changes

### 2.1 `Claim` — `crates/aero-audit-connector/src/outbox.rs` (R1)

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub event_id: Uuid,
    pub claim_token: Uuid,
    pub lease_expires_at: OffsetDateTime,
    pub attempts: i64,
    pub payload: Value,
    /// DESC lane value (higher = claimed first); 0239 SMALLINT.
    /// Pinned to aero_ai::governance::GOVERNANCE_PRIORITY_* (crates/aero-ai/src/governance.rs:31/:33).
    pub priority: i16,
    /// 0239 TEXT; one of governance.rs GOVERNANCE_CLASS_* ("admin"/"message"/"room") (:37/:39/:41).
    pub class: String,
}
```

- `priority: i16` matches the DB `SMALLINT` decode (sqlx `FromRow`); the fake
  uses the identical type so both impls produce equal `Claim`s (R3's "i16
  everywhere in the connector" rule; the drill's `i64` binds are sqlx
  parameter casts and stay untouched).
- `PartialEq`/`Eq` derive is kept; no existing test compares whole `Claim`
  values (state_machine asserts individual fields), so equality-semantics
  change is inert.
- `OutboxRepo::claim_due` trait doc already pins `(priority DESC, available_at,
  created_at, event_id)`; add one sentence: the returned `Claim` carries the
  row's `priority`/`class`.

### 2.2 `PgOutboxRepo` — `crates/aero-audit-connector/src/pg.rs` (R2)

- `ClaimRow` gains `priority: i16, class: String` (sqlx `FromRow` field
  order irrelevant — decode is by name).
- The claim CTE's `RETURNING` gains `outbox.priority, outbox.class`:

```
RETURNING outbox.event_id, outbox.attempts, outbox.claim_token,
          outbox.lease_expires_at, outbox.payload,
          outbox.priority, outbox.class
```

- `From<ClaimRow> for Claim` maps the two new fields. **No other SQL change**:
  filters, fences, lease, `LIMIT`, statuses, and the ORDER BY (already
  landed) are untouched.
- `ensure_outbox_table` minimal fixture gains the `class` column, mirroring
  0239 verbatim:

```sql
class TEXT NOT NULL DEFAULT 'message', -- 0239: DEFAULT 'message' = GOVERNANCE_CLASS_MESSAGE (crates/aero-ai/src/governance.rs:39)
```

  placed next to the existing `priority smallint NOT NULL DEFAULT 10` line.
  The fixture's `CHECK` constraints stay minimal (the repo SQL touches no
  CHECK); existing seeds omit both columns and keep working via defaults.
  When the real 0239 table exists, `ensure_outbox_table` returns early and
  it is used as-is — the fixture only shapes throwaway/non-0239 DBs.

### 2.3 `FakeOutbox` — `crates/aero-audit-connector/src/fake.rs` (R3)

- `FakeRow` gains `priority: i16, class: String`; `FakeRowSnapshot` gains the
  same two `pub` fields (assertable via `fake.row(event_id)`); `row()`
  maps them.
- New seed method (lane-aware):

```rust
/// Seed a ready row due at `due_at` with zero attempts in the given
/// governance lane. `priority`/`class` mirror the 0239 columns; the claim
/// sort runs on `priority` first (DESC, higher = claimed first).
pub async fn insert_lane(
    &self,
    event_id: Uuid,
    payload: Value,
    due_at: OffsetDateTime,
    priority: i16,
    class: String,
)
```

- Existing `insert(event_id, payload, due_at)` keeps its signature and
  delegates with the 0239 column defaults, each with a pin comment:
  `priority = 10` (`GOVERNANCE_PRIORITY_BACKLOG`, governance.rs:33) and
  `class = "message"` (`GOVERNANCE_CLASS_MESSAGE`, governance.rs:39). All 7
  existing `insert` call sites (6 state_machine tests + relay.rs:307's
  `assert_transient_requeue` helper) compile and behave unchanged.
- `claim_due` due-set sort gains the leading term, byte-for-byte mirroring
  the PG CTE tuple order:

```rust
// was: (available_at, created_at, event_id)
due.sort_by(|a, b| {
    b.priority.cmp(&a.priority)          // priority DESC — PG: candidate.priority DESC
        .then_with(|| a.available_at.cmp(&b.available_at))
        .then_with(|| a.created_at.cmp(&b.created_at))
        .then_with(|| a.id.cmp(&b.id))
});
```

  (equivalently `sort_by_key((Reverse(priority), available_at, created_at, event_id))`).
  The due-set collect step gains `priority` alongside the existing
  `(id, available_at, created_at)` tuple.
- The `Claim` constructed in `claim_due` carries `row.priority` and
  `row.class.clone()`.
- `insert_lane` keeps the existing `created_at: due_at` convention (the fake
  has no separate created-at seed today; FIFO within a lane is therefore
  `(available_at, created_at == available_at, event_id)` — deterministic and
  assertable. A future need for distinct `created_at` is out of scope).

### 2.4 A1 unit test — `crates/aero-audit-connector/tests/state_machine.rs` (R5)

One new `#[tokio::test]`, e.g. `priority_first_claim_preempts_fifo_and_limit1_keeps_top_lane`
(two sections, two independent `FakeOutbox` instances):

- **Full-drain section**: pin `fake.set_now(Some(t0))`; seed N=20 backlog
  rows via `insert_lane(id_i, payload, t0 − 200s + i, 10, "message")`
  (earlier `available_at`, FIFO-ordered) and one moderation row via
  `insert_lane(mod_id, payload, t0 − 100s, 100, "admin")` — later than every
  backlog row, enqueued last, so only priority can explain its precedence.
  `claim_due(LEASE, N + 1)` →
  - `claims.len() == 21`;
  - `claims[0].event_id == mod_id`, `claims[0].priority == 100`,
    `claims[0].class == "admin"`;
  - `claims[1..]` event_ids == backlog ids in `(available_at, created_at,
    event_id)` ascending order (FIFO within the lane);
  - `fake.row(mod_id)` snapshot exposes `priority == 100`, `class == "admin"`
    (R3 snapshot surface).
- **limit=1 section** (fresh fake, same seed shape): `claim_due(LEASE, 1)`
  → exactly `[mod_id]` — the single slot goes to the top lane (mirrors the
  drill's batch-membership property at unit scale).
- All literals carry R4 pin comments naming `crates/aero-ai/src/governance.rs`
  (:31/:33/:37/:39); no `aero-ai` import.

### 2.5 Compile surface — `crates/aero-audit-connector/tests/claim_validation.rs` (R6)

`fn claim()` literal constructor gains `priority: 10, class: "message".into()`
(backlog defaults, R4-pinned; behavior unchanged — the 11 tests exercise JWT
validation, not the lane).

## 3. Compatibility constraints

- **`insert` signature frozen** — delegation keeps every existing fake
  consumer source-compatible; only `Claim { .. }` literal constructions
  (fake.rs, pg.rs `From`, claim_validation.rs) are compile surfaces, all
  covered by R2/R3/R6.
- **Additive to the relay pipeline** — `deliver_claim` copies only
  `event_id`/`claim_token`/`attempts` (relay.rs:183-185); `client.deliver`
  uses `claim.event_id` + `claim.payload` (client.rs:143-144); `stub.rs`
  takes `&Claim` for receipt checks on `event_id` only. No consumer
  destructures `Claim` exhaustively → no match/construct breakage beyond the
  three literal sites.
- **No `aero-ai` dependency** — `Cargo.toml` unchanged; lane authority stays
  in aero-ai, the textual pin is the cross-slice comment, the behavioral
  oracles are the drill (PG) and the aero-ai unit pin
  (`moderation_lane_preempts_backlog_under_desc_claim`).
- **Direction invariant** — `priority DESC`, higher = more urgent; never
  ASC, never "aligned" to `ai_job`'s ASC lower-first model (governance.rs:11-25).
  The fake sort's `b.priority.cmp(&a.priority)` is the DESC spelling.
  Starvation under sustained high-lane saturation is the accepted bound of
  this strict-priority model (F9, D7) — the sanctioned mitigation, sibling
  D-CAP's two-arm K-backfill, **preserves** DESC (it reserves K slots for the
  lowest lane; it does not invert the order). An ASC "alignment" would destroy
  the moderation-precedence requirement (compliance outbound queueing behind
  message creates), not fix starvation.
- **i16 vs i64** — `Claim`/`ClaimRow`/fake use `i16` (SMALLINT decode); the
  drill's `i64` seed binds rely on sqlx's implicit parameter cast and are
  untouched. A type mismatch on the decode side fails at compile time — the
  point.
- **Fixture `class` is load-bearing** — R2 must ship together with the
  pg.rs mapping: `RETURNING outbox.class` against the current minimal table
  (no `class`) fails the two existing `#[ignore]` PG tests on non-0239
  throwaway DBs. On migrated DBs 0239 provides the column; the fixture only
  shapes the throwaway path.
- **Fake Vec-order vs PG heap-order** — Vec position is the fake's contract
  (assertable, A1); PG `RETURNING` order is heap order, so the PG tests keep
  their set-based assertions (already landed:
  `mixed_priority_claim_orders_moderation_first_then_fifo`). Do not port the
  position assert into PG tests (D3).
- **Existing PG tests keep working** — the three `#[ignore]` PG tests seed
  without `class` (defaults to `'message'`); after R2 their `Claim`s carry
  `priority`/`class` but the tests assert only ids/attempts/tokens — no
  assertion change needed. The mixed-priority PG test's admin rows will
  decode as `class = "message"` (they set `priority = 100` only); the design
  does not assert `class` on PG (the drill is the class oracle and seeds
  `class='admin'` explicitly).

## 4. Failure modes

| # | Failure | Detection | Mitigation |
|---|---|---|---|
| F1 | `RETURNING outbox.class` runs against a table without the column (non-0239 throwaway DB, fixture not shipped with R2) | sqlx error per claim; the two PG tests fail | R2 ships fixture `class` column in the same change; real DBs always have 0239. Boot degradation unchanged (claim errors logged, F13 posture) |
| F2 | Fake sort drifts from the PG ORDER BY (wrong term order, ASC, missing tiebreak) | A1 position assert fails; PG set assert diverges from fake | Byte-for-byte mirror per §2.3; the landed PG test pins the PG side; the drill is the end-to-end oracle (AC1) |
| F3 | Direction flipped to ASC (or "aligned" to `ai_job`) | A1 `claims[0]` = earliest backlog, moderation starves; drill `moderation-in-first-batch` FAILs red | governance.rs:11-25 warning + pin comments; `moderation_lane_preempts_backlog_under_desc_claim` (aero-ai) fails; AC1 red |
| F4 | Stale `[PROPOSED]` 200/100 fixture literals re-introduced in connector seeds | `rg -n "200" crates/aero-audit-connector` finds them | R4 pin comments on every literal; merge cross-check (AC) |
| F5 | Fake defaults drift from 0239 defaults (e.g. a future "DEFAULT 0" from the aero-bus handoff H1) | fake `insert`-seeded rows claim in a different lane than PG default rows; A1 stays green but PG parity breaks | Pin comments cite 0239 + governance.rs:33/:39; the drill seeds explicit values and is the parity oracle; the 0239 `CHECK (priority > 0)` rejects 0 at the DB |
| F6 | A1 position assert ported into a PG test | flaky/false-fail under heap-order `RETURNING` | R3/D3 note: PG tests assert sets only; the position assert lives on the fake only |
| F7 | `i64` leaked into `Claim`/`ClaimRow` (mismatch with SMALLINT decode intent and the fake's equality surface) | compile error (sqlx decode type) or A1 field-type mismatch | R1 pins `i16`; compile-time by construction |
| F8 | `created_at` semantics divergence (fake sets `created_at = due_at`; PG test seeds inverted `created_at` deliberately) | A1 FIFO-within-lane assert would fail if a future seed needs distinct created_at | Documented in §2.3; out of scope to add a created_at parameter now |
| F9 | **Backlog-lane starvation (known bound):** strict-priority claim with no aging/promotion term — if the claimable priority-100 due set stays ≥ batch per tick, priority-10 rows are never selected, indefinitely. The bounded lease window re-exposes *claimed* rows only; it cannot un-stick never-claimed backlog | **Latent, not live today:** the high lane has one budget-bounded producer (`message.moderated`, ≤ ~60 finalizes/min, sibling §7.1) vs ~1200 rows/min nominal claim capacity (100/5s defaults); the low lane has zero producers (0239 trigger passes through). Live only under sustained λ ≥ μ (degraded sink, or L1 population at volume). Correlate: due-set gauge `audit_governance_outbox_due_rows` (sibling §5.3) | **Accepted + documented (D7); fix scheduled, not this slice:** sibling record F6 + §7.1 D-CAP — two-arm claim with K-backfill floor, gate = land in the same slice as L1 aggregation. No code delta here: precedence is the feature |

## 5. Migration & rollout

**No code delta remains** — the change surface is the commit of the verified
untracked set (requirement record §5); this section is the verification
sequence for that set.

**No migration in this slice.** 0239 (table + trigger), 0240 (due-priority
partial index), and 0241 (disabled-window reconciler) are landed; the PG
ORDER BY is landed. The fixture change in pg.rs is test-only DDL for
throwaway DBs.

Verification sequence (per AGENTS.md §4.3 — fresh throwaway DB, never the
shared dev DB):

1. `cargo build --workspace` — required before any migrate when migrations
   changed; here it also compiles the new `Claim` fields (AGENTS.md §4.2
   build-before-migrate discipline; migrations are embedded in aero-storage's
   `sqlx::migrate!("../../migrations")`, and 0239/0240 are already in the
   tree).
2. `CREATE DATABASE aero_b53_design_$$` → `DATABASE_URL=<url> aero-cli
   migrate` → migrations 0239/0240/0241 apply (the connector crate itself
   embeds no migrations).
3. `DATABASE_URL=<url> cargo run -p aero-audit-connector --bin
   aero-audit-priority-drill` → exit 0 + 3 PASS lines (AC1).
4. `cargo test -p aero-audit-connector -- --ignored` with `DATABASE_URL`
   → the three PG tests (incl. the landed mixed-priority set assert) green
   against the migrated table (real 0239 path) and, optionally, against the
   fixture path by dropping the table first (`DROP TABLE
   audit_governance_outbox` re-creates the minimal shape via
   `ensure_outbox_table` — verifying F1's mitigation).
5. `DROP DATABASE` when done.

Rollout posture: the new `Claim` fields are wire-additive (in-process only —
no serialization of `Claim` crosses the network), so a rolling deploy of the
connector binary is safe with old/new mixed; the fixture/`RETURNING` change
requires the 0239 table shape everywhere the new binary runs (already true in
production migrations; throwaway test DBs get the fixture).

## 6. Testable acceptance mapping

| AC (requirement record) | Concrete command + exact assertion |
|---|---|
| **AC1** — drill PASS end-to-end | `DATABASE_URL=<throwaway> cargo run -p aero-audit-connector --bin aero-audit-priority-drill` → exit 0; stdout contains `drill: moderation-in-first-batch: PASS`, `drill: drain-501: PASS`, `drill: parity-501: PASS`. `scripts/test-integration.sh` → `B5-CHECK moderation-priority-drill: PASS` (no SKIP). Oracle = batch membership (round-1 top-100 contains the moderation row; 501 settled; event_id set-parity) per §0-A2 supersession — the `delivered_at == MIN(delivered_at)` phrasing stays out |
| **AC2** — A1 unit test without PG | `cargo test -p aero-audit-connector --test state_machine` (no `DATABASE_URL`) → new R5 test green: `claims[0]` = moderation row (priority 100, enqueued last, later `available_at`); `limit = 1` claims exactly it; backlog lane FIFO `(available_at, created_at, event_id)` preserved in `claims[1..]`; snapshot exposes `priority`/`class` |
| **AC3** — T-11 regressions unchanged | `cargo test -p aero-audit-connector` fully green: 7 state_machine tests (6 existing + R5), 3 relay unit tests (`transient_5xx_requeues_and_rotates_a_fresh_token`, `transient_timeout_requeues_and_rotates_a_fresh_token`, `transient_claim_drift_requeues_without_any_post` — 403-dead exactly 1 POST, 5xx never dead, drift 0 POSTs), 11 claim_validation tests. `cargo test -p aero-ai --lib` keeps `moderation_lane_preempts_backlog_under_desc_claim` green (governance.rs:119) |
| **AC4** — 37-slot pin guard | `scripts/b5-pin.sh` / `scripts/test-b5-pin-guard.sh` pass; `moderation-priority-drill` slot unchanged (FAIL→PASS move already landed with the sibling slice) |
| Merge cross-checks (AGENTS.md §4.3) | `cargo check --workspace` clean · `cargo clippy --workspace --all-targets` no new warnings (added `pub` fields + one test; the `sort_by` chain is clippy-clean — prefer the `sort_by_key(Reverse(...))` spelling if `clippy::comparison_chain` style triggers) · `cargo test --workspace --lib` green · `scripts/truth-check.sh` / `scripts/file-size-check.sh` 0 violations · `rg -n "200" crates/aero-audit-connector` finds only the drill's `MODERATION_PRIORITY` and unrelated matches (stub 200-status, pg.rs 200s offsets), no stale fixture literals |

## 7. Decision log

- **D1 — Vec position is the fake's ordering contract.** Unlike PG's
  `UPDATE … FROM … RETURNING` (heap order — sibling D3), the fake constructs
  the returned `Vec<Claim>` from its sorted due list, so `claims[0]` is
  assertable in A1. This is the only place the position assert is legal.
- **D2 — `insert` delegates, it does not grow.** Keeps all 7 existing
  `insert` call sites untouched (T-11 preservation) while `insert_lane` is
  the lane-aware surface for the new test. Signature-stable public API.
- **D3 — i16 everywhere in the connector.** Matches `SMALLINT` decode and
  keeps both impls' `Claim` values type-identical; the drill's i64 binds
  stay (sqlx cast).
- **D4 — fixture `class` mirrors 0239's DEFAULT, not the CHECK.** The repo
  SQL touches no CHECK; minimal fixture shape = columns the statements touch
  (F1 mitigation).
- **D5 — no class assertion on the PG test's admin rows.** The landed
  mixed-priority PG test seeds `priority = 100` without `class` (defaults
  `'message'`); class is the drill's oracle (it seeds `class='admin'`), not
  this test's.
- **D6 — no drill changes.** Constants, docstring, and oracle already align
  (verified §0); touching the drill would risk regressing the landed
  FAIL-not-SKIP contract.
- **D7 — starvation bound accepted + documented, not fixed in this slice.**
  Strict-priority DESC claim without aging/promotion can starve priority-10
  rows indefinitely only while the claimable priority-100 due set stays
  ≥ batch per tick (sustained λ ≥ μ); the bounded lease window mitigates
  claimed-row stalls only. Latent today (single budget-bounded high-lane
  producer; no priority-10 producers yet); live scenario = degraded sink
  (sibling §7.1). Recorded as F9; the fix is the scheduled sibling D-CAP
  (two-arm K-backfill, gate = L1 slice) — accepted product behavior *with*
  the documented bound, doc-only delta, DESC invariant re-confirmed.

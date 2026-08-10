# Design — AI moderation finalize → B5 audit governance outbox (class + priority mapping, same tx)

- **Module**: `crates/aero-ai` (direction owner) + contract handoff to `crates/aero-storage` (B5-1/B5-3) and the B5-2 connector
- **Requirements**: `docs/requirements/2026-08-06-aero-ai-moderation-governance-outbox.req.md` (R1–R8, A1–A4)
- **Date**: 2026-08-06 · **Status**: design (unimplemented) · **Change scope**: one new aero-ai module + one call-site + unit tests + aero-storage PG db_tests; **no migration, no config/env change, no new dependencies**

---

## 0. Evidence verification (all cited items re-checked against source)

Every claim in the requirements spec was re-verified against `master` before designing. **All confirmed**; trivial line drift on two db_test anchors (symbols exact):

| Spec claim | Verified location |
|---|---|
| E1 `handle_moderate` → `soft_delete_outboxed_system(..., workspace.map(\|_\| "message.moderated"), ...)`, system actor, Err propagates → retry → DLQ | `crates/aero-ai/src/worker/mod.rs:352` (fn), `:373-377` (call), `:388-396` (Err branch — "retries (bounded → DLQ)" + provider-verdict-replay comment) ✅ |
| E2 `soft_delete_outboxed_system` one tx: lock → soft-delete UPDATE → cleanup → blob enqueue → `append_in_tx` at 337 → `insert_room_event_in_tx` (RoomEvent::Deleted) → commit; `Ok(None)` no-op on already-deleted | `crates/aero-storage/src/message/events.rs:267` (fn), `:337` (audit), `:290-360` (body) ✅ |
| E3 `AuditRepo::append_in_tx` tx-scoped INSERT, client-side `AuditId::new()`; db_test precedents at audit.rs:814/874 (spec said 808/852 — symbol exact, offset ≤ 8) | `crates/aero-storage/src/audit.rs:127-136` (`append_on` :140) ✅ |
| E4 0236 `aero_enqueue_snaplink_audit` (line 68), runtime gate `RETURN NEW` when disabled (:79-82), binding RAISE via `aero_snaplink_binding_for_workspace` (`0235:206` `'commercial binding is unavailable'`), trigger `audit_events_snaplink_delivery` AFTER INSERT (:129-133), payload `action = NEW.action` verbatim (:119), `delivery_id='audit:'\|\|NEW.id`, `idempotency_key=NEW.id` (:92-95) | `migrations/0236_snaplink_governance_reconciliation.sql` ✅ |
| E5 v1 outbox DDL: **no status/priority/class**; `UNIQUE(destination, idempotency_key)`; claim index `(available_at, created_at, delivery_id)`; Rust claim `ORDER BY candidate.available_at, candidate.created_at, candidate.delivery_id` | `migrations/0235_snaplink_commercial_control_plane.sql:161-187`; `crates/aero-storage/src/snaplink_commercial.rs:302` (claim_due, ORDER BY at :317-320) ✅ |
| E6 proposal doc = in-repo 15-line summary; 0239 DDL / status 0-3 / class / `claim_due priority DESC` / mapping table / 500-backlog drill all **[PROPOSED]** out-of-repo; `admin.content.flag`/`admin.moderation.action` appear nowhere in code | `docs/proposals/audit-contract-batch-aero-im.md` ✅; grep over `--include='*.rs' --include='*.sql'` → 0 hits (docs only) ✅ |
| E7 `ai_job.rs::priority_for` pure, lower-first, Moderate=10<Answer=20<Summarize=50<Embed=100; claim `ORDER BY priority ASC, scheduled_at ASC`; unit test at :421-429 | `crates/aero-storage/src/ai_job.rs:62`, `:179` ✅ |
| E8 aero-ai worker tests are unit-only; PG db_tests live in aero-storage (`pool()` :479, `fixture()` :489, `message_in_workspace()` :680, `#[ignore = "requires live Postgres"]`) | ✅ |
| E9 other `message.moderated` producers: `soft_delete_moderated` (`message/crud.rs:388-417`, action at :411) and `moderation_bot.rs:465` → same token, same 0236 trigger — token-keyed mapping covers them with zero caller branching | ✅ |

**One design-relevant addition found while verifying** (not in the spec): `soft_delete_outboxed_system` opens and commits **its own** transaction (`events.rs:277` `self.pool.begin()`, `:293` `tx.commit()`), so a "same operation inside an explicit outer tx, ROLLBACK" acceptance (A1 rollback half as literally written) is **not mechanically expressible** by wrapping the repo call — the rollback half must instead induce an in-tx failure. The 0236 trigger's missing-binding RAISE (E4) is exactly such a failure and is the natural rollback vehicle (see §6.1 A1-RB).

---

## 1. Design overview

```
AiWorker::handle_moderate  (BLOCK verdict, job.workspace_id = Some)         [aero-ai]
  └─ MessageRepo::soft_delete_outboxed_system(id, Some(ws), None, Some("message.moderated"), …)
        [ one PG tx, aero-storage ]
        ├─ soft-delete UPDATE
        ├─ AuditRepo::append_in_tx → audit_events row (local token, actor NULL)   ← UNCHANGED
        ├─ RoomEvent::Deleted outbox row                                          ← UNCHANGED
        └─ (trigger on audit_events INSERT) aero_enqueue_audit_governance  [B5-1 redirect, SQL]
              ├─ aero_audit_governance_map_action(NEW.action)              [B5-3, SQL]
              │     'message.moderated' → (class='admin', priority=100, outbound='admin.content.flag')
              │     unknown            → (class='message', priority=10,  outbound=NEW.action verbatim)
              └─ INSERT audit_governance_outbox (status 0, event_id = audit_events.id)  [B5-1]
        └─ COMMIT  →  handle_moderate records governance outcome counter   [aero-ai, this design]
```

**The mapping is applied at enqueue time, inside the same transaction as the audit append — by the SQL mapping function the 0239 redirect calls.** The aero-ai module's deliverables are the pieces only it can own, without colliding with the parallel B5 directions:

1. **`crates/aero-ai/src/moderation_outbox.rs`** — the pure local-token → governance-stamp table (R1), mirroring `ai_job.rs::priority_for`'s proven pattern: a pure function + `#[must_use]`, unit-tested without a DB. It is the **Rust oracle** for the SQL mapping function: the aero-storage PG acceptance asserts DB rows against these exact constants, so Rust↔SQL drift is a CI failure, not a silent contract break.
2. **Call-site wiring in `handle_moderate`** — observability only (a labeled counter + trace on the three outcomes: governance row enqueued / replay no-op / workspace-less). No behavioral change to the transaction: the row itself is trigger-derived, which is *why* all three `message.moderated` producers (handle_moderate, `soft_delete_moderated`, `moderation_bot`) are covered with zero per-producer branching (E9), and why the in-tx property (R2/R7) is structurally guaranteed rather than enforced by a post-commit call.
3. **PG acceptance tests** in aero-storage's `audit.rs` db_tests module (A1), plus the moderation-row fixture for B5-3's priority drill (A2).

**Rejected alternative — explicit Rust enqueue from `handle_moderate`:** the enqueue cannot be issued from `handle_moderate` because it does not own the transaction (`soft_delete_outboxed_system` opens its own in aero-storage), and aero-storage cannot depend on aero-ai for the stamp constants. Moving the enqueue into `soft_delete_locked_outboxed_in_tx` would belong to B5-1/B5-3's module, would require the retained trigger to skip mapped rows (dual-write hazard, F4), and would break the "moderation_bot covered automatically" property. The trigger + SQL mapping is the only mechanism that is in-tx, token-keyed, and shared.

---

## 2. API changes (concrete)

### 2.1 New module `crates/aero-ai/src/moderation_outbox.rs`

```rust
//! Local audit-action token → outbound governance stamp for the B5 audit
//! outbox (docs/requirements/2026-08-06-aero-ai-moderation-governance-outbox).
//!
//! Single source of truth on the Rust side. The enqueue-time authority is the
//! SQL mapping `aero_audit_governance_map_action` (0239 redirect, B5-3); this
//! table is its test oracle — the aero-storage PG tests assert DB rows against
//! these exact constants, so drift between SQL and Rust fails CI.

/// Local audit_events.action token produced by every moderation delete path
/// (AiWorker::handle_moderate, MessageRepo::soft_delete_moderated,
/// aero-server moderation_bot) — keyed on the token only, never the caller.
pub const LOCAL_ACTION_MESSAGE_MODERATED: &str = "message.moderated";

/// Outbound contract token for a moderation content flag. Single constant —
/// [PROPOSED] by the out-of-repo contract (E6); B5-3 locks it in SQL. If the
/// contract dictates `admin.moderation.action` instead, this one line and the
/// SQL function change together and A1 re-verifies (self-consistent either way).
pub const OUTBOUND_ACTION_MESSAGE_MODERATED: &str = "admin.content.flag";

/// class values per 0239 DDL (message | room | admin).
pub const CLASS_ADMIN: &str = "admin";
pub const CLASS_MESSAGE_DEFAULT: &str = "message";

/// Priority lane values under B5-3's `claim_due ORDER BY priority DESC`
/// (higher = claimed first — the OPPOSITE convention of ai_job.rs's ASC
/// lower-first; do not port the ai_job numbers). Moderation must sort ahead of
/// the message/room backlog; the anti-starvation cap (B5-3) bounds the harm if
/// admin lanes flood.
pub const PRIORITY_MODERATION: i16 = 100;
pub const PRIORITY_BACKLOG_DEFAULT: i16 = 10;

/// One outbound governance stamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GovernanceStamp {
    pub class: &'static str,
    pub priority: i16,
    pub outbound_action: &'static str,
}

/// Pure local→outbound mapping. Unknown tokens return `None` (enqueue passes
/// them through with the default stamp + verbatim action; the mapping never
/// raises and never blocks non-moderation audit rows — R1).
#[must_use]
pub fn outbound_governance_for(action: &str) -> Option<GovernanceStamp> {
    match action {
        LOCAL_ACTION_MESSAGE_MODERATED => Some(GovernanceStamp {
            class: CLASS_ADMIN,
            priority: PRIORITY_MODERATION,
            outbound_action: OUTBOUND_ACTION_MESSAGE_MODERATED,
        }),
        _ => None,
    }
}

/// Governance-outcome labels for the observability counter (2.2).
pub const GOVERNANCE_ENQUEUED: &str = "enqueued";
pub const GOVERNANCE_REPLAY_NOOP: &str = "replay_noop"; // Ok(None): already deleted
pub const GOVERNANCE_NO_WORKSPACE: &str = "no_workspace"; // job.workspace_id None
```

Register `pub mod moderation_outbox;` in `crates/aero-ai/src/lib.rs` (after `worker`; no re-export collisions — no `generate_token`-style helpers here, AGENTS.md §4.2).

### 2.2 Call-site wiring in `crates/aero-ai/src/worker/mod.rs`

`handle_moderate` (line 352) gains a `reg: &Registry` parameter — threaded from `process_inner` (line 234), matching the existing `process_batch`/helper convention (handlers at :718/:778 already take `reg`). Inside the existing `match` on the `soft_delete_outboxed_system` result, add one recording call per arm (no behavior change):

```rust
Ok(Some(_)) => { /* existing info! */ crate::moderation_outbox::record_governance(reg, GOVERNANCE_ENQUEUED); }
Ok(None)    => { /* existing debug! */ crate::moderation_outbox::record_governance(reg, GOVERNANCE_REPLAY_NOOP); }
```

Plus, before the BLOCK branch (when `job.workspace_id.is_none()` / `job.target_id.is_none()` — the un-audited path, R8 precondition), record `GOVERNANCE_NO_WORKSPACE`. `record_governance` is a 4-line helper in `moderation_outbox.rs` that bumps a new labeled counter:

```rust
// in crates/aero-ai/src/metrics.rs
pub const AI_MODERATION_GOVERNANCE_TOTAL: &str = "aero_ai_moderation_governance_outbox_total";
pub const GOVERNANCE_OUTCOME_LABEL: &str = "outcome";
// in moderation_outbox.rs
pub fn record_governance(reg: &Registry, outcome: &str) {
    reg.inc_counter_labeled(names::AI_MODERATION_GOVERNANCE_TOTAL, 1,
        &[(names::GOVERNANCE_OUTCOME_LABEL, outcome)]);
}
```

This makes "did the moderation finalize produce a governance row" observable (`enqueued` vs `replay_noop` vs `no_workspace`), greppable in tracing, and unit-testable against an injected registry — the only genuine call-site wiring this direction needs, because the row itself is trigger-derived.

### 2.3 Contract handoff to B5-3 (SQL mapping — NOT built here, spec pinned)

The 0239 redirect's enqueue trigger calls a plpgsql function with exactly this contract (B5-3 implements it; A1 cross-checks it):

```sql
-- [B5-3] returns the stamp for NEW.action, or (class='message', priority=10,
--        outbound_action=NEW.action) for unmapped tokens. Never raises.
CREATE OR REPLACE FUNCTION aero_audit_governance_map_action(local_action TEXT)
RETURNS TABLE (class TEXT, priority INT, outbound_action TEXT) …;

-- 'message.moderated' ⇒ ('admin', 100, 'admin.content.flag')   -- MUST equal §2.1 constants
-- anything else       ⇒ ('message', 10, local_action verbatim)
```

---

## 3. Compatibility constraints

1. **No migration from this direction** — `0239_audit_governance_outbox.sql` is B5-1's (E6; number collision would break the parallel batch). A1 phase-1 runs against the v1 shape; phase-2 assertions land with 0239 (see §6.1).
2. **`audit_events` contract untouched**: local token stays `'message.moderated'`, `actor_id NULL`, detail carries `reason`+`digest` — the convention pinned by `moderation_delete_and_audit_commit_together` (audit.rs:814). The mapping applies **at outbox enqueue time only** (R3).
3. **`soft_delete_outboxed_system` signature unchanged** — shared by `handle_moderate` and the caller at `events.rs:618`; adding parameters would ripple into B5-owned code.
4. **Dependency direction**: aero-storage must not depend on aero-ai. The PG acceptance tests duplicate the §2.1 literals with a comment naming `aero_ai::moderation_outbox` as the oracle; the unit test (pins Rust values) + PG test (asserts DB rows) pair makes drift a double failure.
5. **Priority convention mismatch is deliberate**: ai_job uses ASC lower-first (`Moderate=10`); the outbox uses DESC higher-first per B5-3 (`PRIORITY_MODERATION=100`). Porting ai_job's numbers verbatim would put moderation *last*. The unit test pins the DESC semantics explicitly.
6. **v1 table keeps flowing until the B5-1 redirect**; both tables live during cutover and the redirect (`CREATE OR REPLACE` of the enqueue fn) is the **single writer** — no dual-write (F4).
7. **Repo hygiene**: pure fn, `#[must_use]`, no unsafe, no new deps (MSRV 1.80, clippy `all`+`pedantic` clean); new module well under the 800-line file-size gate; `Cargo.toml` files untouched.
8. **`kind`-tag collision rule (AGENTS.md §4.2) does not apply** — no serde enums introduced; `class`/`priority` are plain fields.

## 4. Failure modes

| # | Mode | Behavior | Mitigation / owner |
|---|---|---|---|
| F1 | Enforcement **on**, workspace **missing binding** → trigger RAISE (E4) aborts the whole moderation tx | `handle_moderate` gets `Err` → retry → DLQ after `MAX_ATTEMPTS=5`; message **stays visible** (fail-closed: no deleted-but-unqueued state). | Pre-existing, verified, **intentionally not bypassed** (R8). B5-1 redirect decides v2 gate semantics. |
| F2 | Enforcement **off** | No governance row (v1 gate `RETURN NEW`); audit row still commits. | A1 setup must `enabled=TRUE` + seed binding; B5-1 owns v2 gate. |
| F3 | Rust↔SQL drift on priority/token/class | A1 phase-2 assertion vs §2.1 constants fails → CI red. | The PG test *is* the consistency gate; unit test pins Rust side. |
| F4 | Dual-write (v1 trigger + explicit enqueue both active) | Duplicate governance rows → `UNIQUE(event_id)` violation → tx abort → job retry loop. | Single-writer rule: only the 0239 redirect enqueues; A1 asserts exactly 1 row. |
| F5 | Replay of a committed finalize (worker retry, at-least-once) | `soft_delete_outboxed_system` returns `Ok(None)` (already deleted) → no audit append → no trigger → no second row (E2). `UNIQUE(destination, idempotency_key)` is the schema-level backstop. | R6; A1 replay half. |
| F6 | Governance INSERT fails mid-tx (constraint, partition) | Whole tx rolls back: soft delete + audit + outbox together → `Err` → retry → DLQ. No "audited but unqueued". | R7, structural (same tx); A1 rollback half. |
| F7 | Moderation rows starve backlog (priority flood) | B5-3's anti-starvation cap bounds admin-lane dominance. | A2 assertion 2; aero-ai only stamps the priority. |
| F8 | Provider verdict lost across crash | Not applicable: verdict finalized under the job's stable usage context before finalize; replay re-uses it without a second paid call (E1 comment). | Unchanged; A1 replay half exercises the idempotent no-op. |
| F9 | Worker crash after commit, before receipt | At-least-once replay → identical to F5. | R6; no new surface. |

## 5. Migration steps

No migration is added by this direction (B5-1 owns 0239). The **integration sequence** (with AGENTS.md §4.2's build-then-migrate rule):

1. **This direction (aero-ai)**: `moderation_outbox.rs` + `handle_moderate` wiring + unit tests + A1 phase-1 PG test. `cargo check --workspace` clean; A1 phase-1 green against v1 shape (atomicity + parity + replay, §6.1).
2. **B5-1 (aero-storage)**: `0239_audit_governance_outbox.sql` (status 0/1/2/3, class, priority, delivery_mode, `event_id` + `UNIQUE(event_id)`) + `audit_governance.rs` + `CREATE OR REPLACE` redirect of `aero_enqueue_snaplink_audit` → `aero_enqueue_audit_governance` (calls the mapping fn). **Order: `cargo build` (migrations are compile-time embedded) → `aero-cli migrate` on the throwaway DB.** A1 phase-2 assertions + A3 unlock.
3. **B5-3 (aero-storage)**: `aero_audit_governance_map_action` SQL fn (values MUST match §2.1) + `claim_due ORDER BY priority DESC` + anti-starvation cap. A2 unlocks; moderation fixture supplied by this direction's constants.
4. **B5-2 (connector crate)**: relay with §1.2 semantics; 403 → dead (T-11). A4 unlocks.
5. **Cutover**: both outbox tables live during transition; the redirect is the single writer; v1 undelivered audit rows drain via the existing reconcile (0236) — B5-1's concern, not this direction's.

## 6. Testable acceptance mapping

### 6.1 A1 — one AI-moderated message → 1 audit + 1 governance row, same tx (aero-storage db_tests, `audit.rs` module next to :814)

Production seam: `MessageRepo::soft_delete_outboxed_system(id, Some(ws), None, Some("message.moderated"), detail, ParticipantId::nil(), None)` — the exact call `handle_moderate` makes (E1). Setup mirrors the spec: throwaway migrated DB, `fixture()` + `message_in_workspace()`; `UPDATE snaplink_commercial_runtime SET enabled = TRUE WHERE singleton`; binding seed `INSERT INTO snaplink_commercial_bindings (workspace_id, tenant_id, client_id, source_system, revision, enabled) VALUES ($1,'tenant-a','client-a','aero-im',1,TRUE)`.

| Half | Act | Assert |
|---|---|---|
| **A1-C** (commit) | first call | (1) message `deleted_at` set, `blocks='[]'`; (2) exactly 1 `audit_events` row: `action='message.moderated'`, `target=id`, `actor_id IS NULL`, `detail->>'reason'` set; (3) exactly 1 governance row: `class='admin'`, `priority=100`, outbound `action='admin.content.flag'` (= §2.1 constants), `status=0`, `event_id` = audit row id; (4) v1 phase-1 form: 1 `snaplink_delivery_outbox` row `destination='audit'`, `idempotency_key`=audit id, `payload->>'action'='message.moderated'` |
| **A1-RB** (rollback) | **mechanism**: induce in-tx failure — runtime `enabled=TRUE`, **no binding** → trigger RAISE (F1) | call returns `Err`; then `deleted_at IS NULL` (delete rolled back) AND `COUNT(audit_events … 'message.moderated') = 0` AND `COUNT(governance outbox) = 0` — both removed together, fail-closed (mirrors `moderation_failed_audit_rolls_back_delete` :874; note §0: the repo owns its tx, so the literal "wrap in explicit tx + ROLLBACK" is expressed as this in-tx failure) |
| **A1-RP** (replay) | second identical call (binding seeded) | `Ok(None)` → still exactly 1 audit + 1 governance row (R6) |

Phase-1 (today, pre-0239): run A1-C/A1-RB/A1-RP asserting the v1 row shape (audit + `snaplink_delivery_outbox`). Phase-2 (post-B5-1): same test asserts the mapped v2 fields; the phase-1 assertions are kept for the transition window.

### 6.2 A2 — 500 backlog + 1 moderation → moderation claimed first (gated: B5-3)

Fixture helper `seed_moderation_governance_row(pool, ws, …)` in the aero-storage test module stamps the row with **this direction's constants** (class='admin', priority=100, outbound 'admin.content.flag', status 0, due `available_at`). Seed 500 message/room-class rows at default priority with staggered `available_at`. Assert (1) first `claim_due` returns the moderation row regardless of enqueue order; (2) after K batches, backlog rows are all claimed — no permanent starvation (cap semantics per B5-3).

### 6.3 A3 — status machine 0→1→2 / →3 + event_id parity (gated: B5-1)

Over the A1-C row: `status 0` at enqueue → claim flips `status 1` + claim token/lease → settle `status 2`; dead path `status 3` terminal. **P2 parity** via set-difference: `SELECT id FROM audit_events WHERE action='message.moderated' AND target=$1` minus `SELECT event_id FROM audit_governance_outbox WHERE …` = ∅ (no orphans, no duplicates).

### 6.4 A4 — T-11 403 → dead exactly once (gated: B5-2, integration)

Relay integration drill: first delivery of a moderation-path row gets 403 → `status 3` exactly once, attempts do not increment past terminal, row excluded from claim/lag. aero-ai contribution: rows are normally claimable (A3 mechanics cover it).

### 6.5 Requirements → test/design mapping

| Req | Where pinned |
|---|---|
| R1 mapping (pure, unknown → pass-through, token-keyed) | §2.1 unit tests in `worker/tests.rs` (or `moderation_outbox.rs` sibling): `outbound_governance_for("message.moderated") == Some(stamp)`; `for("message.deleted") == None`; `for("") == None`; DESC ordering assert `PRIORITY_MODERATION > PRIORITY_BACKLOG_DEFAULT`; class/action string equality. Mirrors `priority_lanes_order_user_facing_work_ahead_of_backfill` (ai_job.rs:421) |
| R2 same-tx enqueue | A1-C/A1-RB (structural: trigger in-tx; no post-commit step exists) |
| R3 local token + system actor preserved | A1-C assert 2; existing audit.rs:814 test unchanged |
| R4 class/priority/status 0 | A1-C assert 3; A3 |
| R5 event_id 1:1 | A1-C assert 3; A3 parity |
| R6 replay idempotency | A1-RP |
| R7 fail-closed propagation | A1-RB; F1/F6; `handle_moderate` Err branch untouched (:388-396) |
| R8 preconditions (workspace-Some; runtime/binding gates) | `GOVERNANCE_NO_WORKSPACE` recording; A1 setup note; F1/F2 |

## 7. Sequencing

1. **This direction**: §2.1 + §2.2 + unit tests + A1 phase-1 PG test — compiles and is green **before** any B5 DDL lands (no migration dependency).
2. B5-1 → A1 phase-2 + A3. 3. B5-3 → A2 (+ SQL mapping fn per §2.3 contract). 4. B5-2 → A4.
A2–A4 are explicitly gated on their parallel directions; nothing in §2 blocks them.

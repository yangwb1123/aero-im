# Design — R-D2: `message.deleted` into the Governance Outbox (Rust writer)

**Status:** proposed · **Slice:** aero-storage (writer) + aero-common (leaf const) + aero-im-core/aero-audit-connector (parity/drill tests)
**Spec basis:** `docs/auto/runs/complete-message-outbox-coverage-map-message-del-ecc80034/artifacts/requirements-10762e10/requirements.md` (direction: "Complete message.* outbox coverage: map `message.deleted` into the governance outbox")
**Value 8 / risk_reduction 8 / effort 4 / confidence 9.**

---

## 0. Evidence verification (performed against the live tree)

Every load-bearing claim in the requirements was re-checked before design. Results:

| Evidence claim | Verification |
|---|---|
| `crates/aero-storage/src/message/authorization.rs:514` — `Some("message.deleted")` in `soft_delete_outboxed_authorized`, detail `{room_id, digest}` (digest = first 120 chars of `searchable_text`) | ✅ verified (call site at :469-528; digest built before the call) |
| `crud.rs:378` — `soft_delete_audited` appends `"message.deleted"`; **no production callers** (only `audit.rs` tests + `audit_governance` db_tests) | ✅ verified (`grep -rn soft_delete_audited` hits only tests) |
| `events.rs:622` — literal inside test `audit_failure_rolls_back_delete_and_outbox_append` (:612) | ✅ verified |
| Production choke point = `soft_delete_locked_outboxed_in_tx` (events.rs:299-362), audit append at :336-347 serving both `soft_delete_outboxed_authorized` and `soft_delete_outboxed_system` | ✅ verified; **line drift**: append is at :336-347 (not :344-353); `AuditId` return value is currently discarded |
| 0245/0246 headers declare `message.deleted` NOT trigger-owned (R-D2: sibling Rust outbox write is the planned path) | ✅ verified (0245:68, 0246:79-80) |
| `AuditGovernanceOutboxRepo` **landed** (H3 auth slice): `mod.rs` "## H3 landed", `outbox.rs` has `append_in_tx` / `append_pair_in_tx_fail_open` / `record_pair_standalone` / `aggregate_login_failure_buckets` / `governance_envelope` / `is_fail_open_error`; `write_pair` re-selects `created_at` from `audit_events` | ✅ verified (outbox.rs:96, :256-291, :568) |
| 0242 allowlist = exactly `message.create`/`message.edit`; md5 window/spill keys; D2 gate-free precedent | ✅ verified (0242:82 `IF NEW.action NOT IN ('message.create','message.edit')`; :54-55 no runtime gate) |
| No `LOCAL_ACTION_MESSAGE_DELETED` const; `room_vocabulary_consts_are_pinned` already pins `LOCAL_ACTION_MESSAGE_RECALLED != "message.deleted"` via bare literal (audit.rs:547) | ✅ verified |
| `AuditRepo::append_in_tx` returns `Result<AuditId, sqlx::Error>` | ✅ verified (audit.rs:127) |
| `governance_lane_for("message.deleted") == None`, pinned by tests | ✅ verified (aero-ai/src/governance.rs:189) |
| Relay `PERMANENT_DEAD_AT = 2` (≤1 retry for permanent), `PayloadGuard` (client.rs:175), `ReceiptMismatch` (:212), `validate_delivery_payload` (:514), `receipt_event_id_matches` (:539) | ✅ verified |
| Harness: `audit_governance::` slot matches **38** test fns; `truth-check-lib.sh:164` `L1_TOKEN_LITERALS_CS` lacks `"message.deleted"`; b5-pin slots `audit_governance::`/`l1-aggregation-drill`/`moderation-priority-drill`/`notification-fanout` | ✅ verified (38 via grep count) |
| `ImService::delete_message` (messages.rs:430) funnels into `soft_delete_outboxed_authorized`; system path passes `message.moderated` (messages.rs:645, aero-ai worker mod.rs:397) | ✅ verified — the exact-token gate must not fire for `message.moderated` |
| 0239 CHECK admits class `'message'` + priority 10 (default) | ✅ verified (0239:32-35) |
| Migration count = **246** | ✅ verified (`ls migrations/*.sql | wc -l`) |

**Corrections vs. evidence (all non-material):** `events.rs` audit append lines :344-353 → :336-347; `recall_lane_outbox_parity` is at lanes.rs:245 (":223 shape" was approximate); `soft_delete_outboxed_system` lives at events.rs:267. No load-bearing claim failed.

---

## 1. Design decision (confirmed)

**Rust writer** through the landed `AuditGovernanceOutboxRepo`, writing **1:1 class-`'message'` priority-10 rows** from the delete transaction, gated on the **exact token** `message.deleted`.

- Matches the binding 0245/0246 ownership declaration ("NOT trigger-owned … sibling Rust outbox write … remains the planned path") — no migration edits, no allowlist mutation.
- `AuditGovernanceOutboxRepo` is landed and `write_pair` (outbox.rs:256-291) is the byte-exact template: re-select `occurred_at` → `governance_envelope` → `append_in_tx` with `ON CONFLICT (event_id) DO NOTHING`.
- L1 window folding is **rejected** (recorded): it would mutate the 0242 allowlist, the drill parity SUM side, `message_lane_outbox_parity`/`room_lane_unconditional_enqueue` row counts, and corrupt `message.batch` aggregate semantics (0246 header: "1:1 requirement is load-bearing"). The 0242 window/spill machinery stays byte-identical and becomes a regression guard.

---

## 2. API changes

### 2.1 `crates/aero-common/src/model/audit.rs` — leaf token const (FR-1)

Add next to `LOCAL_ACTION_MESSAGE_RECALLED` (:215), same doc discipline:

```rust
/// R-D2: user-delete token. NOT trigger-owned (0245/0246 carve-out); the
/// governance outbox row is produced by the Rust writer
/// (`AuditGovernanceOutboxRepo::append_message_delete_in_tx`) from the
/// soft-delete choke point — never by an AFTER-INSERT allowlist.
pub const LOCAL_ACTION_MESSAGE_DELETED: &str = "message.deleted";
```

Extend `room_vocabulary_consts_are_pinned` (audit.rs:526) — replace the bare-literal disjointness assert at :547 with `assert_ne!(LOCAL_ACTION_MESSAGE_RECALLED, LOCAL_ACTION_MESSAGE_DELETED, ...)` plus new disjointness pins against `LOCAL_ACTION_MESSAGE_CREATE`, `LOCAL_ACTION_MESSAGE_EDIT`, `LOCAL_ACTION_MODERATED`, `LOCAL_ACTION_ROOM_CREATE`, `LOCAL_ACTION_ROOM_ARCHIVED`, and a value pin `assert_eq!(LOCAL_ACTION_MESSAGE_DELETED, "message.deleted")`.

**Literal rewrites (FR-1 sweep):** `authorization.rs:514`, `crud.rs:378`, `events.rs:622` (test), idempotency.rs test sites, `producer/carveout.rs:126-135`, `gates.rs`, `dedup.rs`, `lanes.rs`, `l1.rs`, `parity.rs`, `aero-ai/src/governance.rs` test literals → `LOCAL_ACTION_MESSAGE_DELETED`.

### 2.2 `crates/aero-storage/src/audit_governance/outbox.rs` — writer method (FR-2)

```rust
/// aero-storage must not depend on aero-ai, so the priority const is local
/// (AUTH_PAIR_PRIORITY precedent); drift-guarded by the db_tests pins
/// (aero_ai::governance::GOVERNANCE_PRIORITY_BACKLOG = 10).
const MESSAGE_DELETE_CLASS: &str = GOVERNANCE_CLASS_MESSAGE;
const MESSAGE_DELETE_PRIORITY: i16 = 10;

/// 1:1 message.deleted outbox row, written in the delete tx (R-D2).
///
/// Fail-closed: propagates `sqlx::Error` — a lost outbox row aborts the
/// delete (mirrors the 0239/0242/0245/0246 AFTER-trigger abort semantics;
/// deliberate opposite of the auth `append_pair_in_tx_fail_open`).
/// Gate-free (0242 D2): no runtime gate, no binding lookup.
pub async fn append_message_delete_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    audit_id: AuditId,
    workspace: WorkspaceId,
    actor: Option<ParticipantId>,
    target: Option<&str>,
    detail: serde_json::Value,
) -> Result<(), sqlx::Error> {
    // Server-stamped created_at re-selected (write_pair precedent,
    // outbox.rs:256-291) — single clock domain, never a Rust formatter.
    let occurred_at: OffsetDateTime =
        sqlx::query_scalar("SELECT created_at FROM audit_events WHERE id = $1")
            .bind(audit_id.to_uuid())
            .fetch_one(&mut **tx)
            .await?;
    let payload = governance_envelope(
        audit_id,
        workspace,
        actor,
        target,
        detail,
        LOCAL_ACTION_MESSAGE_DELETED,  // outbound action = local token verbatim
        AUDIT_OUTCOME_SUCCESS,
        occurred_at,
        AUDIT_SOURCE_SYSTEM,           // "aero-im.source" — passes connector PayloadGuard
    );
    Self::append_in_tx(tx, audit_id, MESSAGE_DELETE_CLASS, MESSAGE_DELETE_PRIORITY, payload).await
}
```

No new imports beyond `LOCAL_ACTION_MESSAGE_DELETED` + `GOVERNANCE_CLASS_MESSAGE` + `AUDIT_SOURCE_SYSTEM` in the existing `use aero_common::{...}` block (outbox.rs:39).

### 2.3 `crates/aero-storage/src/message/events.rs` — choke-point wiring (FR-3)

In `soft_delete_locked_outboxed_in_tx` (:336-347), capture the `AuditId` and add the exact-token gate **inside the same `if let`** (the audit append must precede the writer so an audit failure still aborts first):

```rust
if let (Some(workspace), Some(action)) = (workspace, audit_action) {
    let audit_id = crate::audit::AuditRepo::append_in_tx(
        tx,
        workspace,
        audit_actor,
        action,
        Some(&id.to_string()),
        detail.clone(),
    )
    .await?;
    // Exact-token gate (trigger discipline: never a prefix). message.moderated
    // deletes keep the 0239 admin/100 trigger path untouched.
    if action == LOCAL_ACTION_MESSAGE_DELETED {
        crate::audit_governance::outbox::AuditGovernanceOutboxRepo::append_message_delete_in_tx(
            tx,
            audit_id,
            workspace,
            audit_actor,
            Some(&id.to_string()),
            detail,
        )
        .await?;
    }
}
```

(`detail.clone()` mirrors `write_pair`; the single gate covers both production entries — `soft_delete_outboxed_authorized` today, any future system caller passing the delete token. The `message.moderated` callers at im-core messages.rs:645 and aero-ai worker mod.rs:397 are structurally excluded by the gate.)

### 2.4 `crates/aero-storage/src/message/crud.rs` — standalone seam parity (FR-4)

`soft_delete_audited` (:377-384): capture the `AuditId` from its `"message.deleted"` append and apply the same gated writer call (unconditional here — the action is hard-coded). This is seam parity for the db_tests that exercise the standalone path (`gates.rs:53`, `dedup.rs:145`); no production caller exists today.

### 2.5 `scripts/truth-check-lib.sh` — literal guard (FR-6)

Add `'"message.deleted"'` to `L1_TOKEN_LITERALS_CS` (:164) so production Rust may spell it only via the leaf const. `L1_TOKEN_ALLOWLIST` stays empty (tests components are skipped by rule-3e precedent).

---

## 3. Compatibility constraints

1. **Zero schema change.** `ls migrations/*.sql | wc -l` stays **246**; migrations 0239-0246 are immutable (never edited). The 0245/0246 "NOT trigger-owned" statements remain literally true — ownership moves to the Rust writer, which is the declared plan. 0239's CHECK already admits class `'message'` + priority 10.
2. **Exact-token gate, never prefix.** The writer fires only for `message.deleted`. `message.moderated` deletes (admin/100 trigger rows) and `message.recalled` (0246 trigger) are structurally untouched — no double-enqueue, no class collision.
3. **Fail-closed, gate-free.** Writer error aborts the delete tx (no SAVEPOINT, no DLQ) — mirrors trigger-abort semantics; runs regardless of `snaplink_commercial_runtime.enabled` (0242 D2 precedent) → no disabled-window gap, **no 0241 reconciler extension** (reconciler stays `message.moderated`-only).
4. **Dependency direction.** aero-storage must not depend on aero-ai → priority const is local (`MESSAGE_DELETE_PRIORITY: i16 = 10`), drift-guarded by tests that recompute `GOVERNANCE_PRIORITY_BACKLOG` semantics (mod.rs precedent).
5. **Wire contract.** `source_system` must equal `AUDIT_SOURCE_SYSTEM` (`"aero-im.source"`) or the connector's `validate_delivery_payload` deads the row (`PayloadGuard`); deployment invariant `AERO_AUDIT_SOURCE_SYSTEM == "aero-im.source"` (same exposure 0242/0245/0246 already accept). `idempotency_key == event_id == audit row id` (receipt echo, `receipt_event_id_matches`). Envelope must parse into `AuditClaimPayload` (`deny_unknown_fields`) with no L1 marker keys (`aggregated`/`count`/`window_start`/`window_end`).
6. **Lane authority untouched.** `governance_lane_for` (aero-ai) keeps `message.deleted → None`; the writer is a direct outbox write, not a lane mapping. `aero-im-core` service code, relay/connector (`claim_due` class-agnostic), web/UI: zero change.
7. **Harness surfaces.** `scripts/b5-pin.sh` and `scripts/test-integration.sh` **unchanged** — the renamed AC-1 test keeps the `moderation_finalize_outbox_parity` prefix (matches both the named slot and `audit_governance::`); no new slot. `audit_governance::` executed count must never drop below the live pre-change 38.
8. **Version skew.** Pure additive code change, no schema — old and new binaries interoperate with the existing table in either direction.

---

## 4. Failure modes

| # | Failure | Behavior | Recovery / invariant |
|---|---|---|---|
| F1 | `AuditRepo::append_in_tx` fails (e.g., FK on workspace) | Propagates; delete tx aborts; message retained | Existing rollback test `audit_failure_rolls_back_delete_and_outbox_append` (events.rs:612) stays green — audit failure precedes the writer |
| F2 | Writer's `fetch_one`/INSERT fails (DB-level) | Propagates `sqlx::Error`; caller aborts tx → message NOT deleted (fail-closed) | Mirrors AFTER-trigger abort semantics; no silent delete-unaudited (`crud.rs` docstring contract) |
| F3 | Connection-level error mid-tx | Propagates; tx unusable; caller errors | No partial state: audit row, outbox row, soft-delete, room event all roll back together |
| F4 | Replay: same delete retried | `deleted_at` guard → `Ok(None)` → no second audit append, no second writer call | Plus `ON CONFLICT (event_id) DO NOTHING` for the same-id audit re-INSERT case → at-most-one row |
| F5 | Relay-side permanent errors (corrupt row) | `ReceiptMismatch` / `PayloadGuard` → `mark_dead` at `attempts == PERMANENT_DEAD_AT` (≤1 retry) | Covered by AC-2b negative controls; terminal invariants R5/R6 untouched |
| F6 | Crash after commit, before relay | Row stays status 0; at-least-once claim/settle | Standard outbox semantics; no new machinery |
| F7 | `action` drift (token renamed) | Gate misses → row not written (silent gap) | FR-1 const + truth-check literal guard + pin tests make drift a compile/test failure |
| F8 | `detail` non-object | Envelope always object (`json!` wrap); audit_events has no detail CHECK | Same exposure as existing lanes; contract pre-check precedent exists in the auth pair |

---

## 5. Migration steps

**No SQL migration.** Steps:

1. Land in **one commit**: §2.1 const + pin, §2.2 method, §2.3/§2.4 wiring, §2.5 guard, §6 test surface (flip + new), §7 inventory updates.
2. `cargo check --workspace` → `cargo test --workspace --lib` → `cargo clippy --workspace --all-targets` (no new warnings) → `scripts/{truth-check,file-size-check,web-check}.sh` (0 violations).
3. `scripts/test-integration.sh` (slots `audit_governance::`, `moderation_finalize_outbox_parity`, `l1-aggregation-drill`, `moderation-priority-drill`, b5-pin guard) and `scripts/test-notification-fanout.sh` (im-core `db_tests::` incl. governance-drill suite) — both on throwaway migrated DBs, `--test-threads=1`.
4. Deploy: pure code roll; no `aero-cli migrate`, no migration-count arbiter change (246). Rollback = revert commit (table schema unchanged).

---

## 6. Testable acceptance mapping

| AC | Test (file) | What it pins | Command/slot |
|---|---|---|---|
| **AC-1** in-tx producer parity | Rewrite `producer/carveout.rs:107` `moderation_finalize_outbox_parity_message_deleted_unmapped` → **`moderation_finalize_outbox_parity_message_deleted_lane`** | Commit half: seed via `insert_outboxed` + delete via `soft_delete_outboxed_authorized` (the exact `ImService::delete_message` seam) → exactly **1** new row in same tx: `event_id` 1:1 with audit id, status 0, class `'message'`, priority 10, action verbatim, `source_system == AUDIT_SOURCE_SYSTEM`, `idempotency_key == event_id`, `targets == [{id, type:"resource"}]`, `payload == {room_id, digest}`, no L1 keys, parses into `AuditClaimPayload` Value-equal. Rollback half → 0 rows. Replay half → `Ok(None)` then still 1 row; same-id audit re-INSERT → at-most-one. | `scripts/test-integration.sh` slots `audit_governance::` + `moderation_finalize_outbox_parity` (throwaway migrated DB, `--test-threads=1`, empty-filter guard) |
| **AC-2a** connector drill | Extend `crates/aero-audit-connector/src/bin/aero-audit-l1-parity-drill.rs` (`l1-aggregation-drill` slot) with a delete leg | Self-isolated fixture: N deletes via real seam `soft_delete_outboxed_system(id, Some(ws), Some(actor), Some(LOCAL_ACTION_MESSAGE_DELETED), detail, actor, None)` → exactly N 1:1 rows (own `idempotency_key`, class `'message'`, prio 10, no L1 markers); pre-existing window parity `SUM(count) == COUNT(create/edit)` **holds unchanged**; spill leg untouched. Exit 0 PASS / non-zero FAIL / 2 SKIP when 0242 absent. | `scripts/test-integration.sh` slot `l1-aggregation-drill` |
| **AC-2b** real-relay leg | New `drill_*` leg in `crates/aero-im-core/src/db_tests/governance_drill_tests.rs` (named `drill_*`, never a bare slot name) | Seed N delete-lane rows via `svc.delete_message`; run real `AuditRelay` + `StubSink` (`relay_for`/`drill_config`, `source_system = AUDIT_SOURCE_SYSTEM`) → all N claim/deliver (202 + receipt)/settle to **status 2**; assert status-2 set-parity `{event_id}` == seeded set. Negative controls (direct-seeded corrupted rows): (1) `payload.event_id` ≠ PK → `ReceiptMismatch` permanent → dead (status 3) at `PERMANENT_DEAD_AT` (≤1 retry); (2) `payload.source_system` mismatch → `PayloadGuard` permanent → dead at ≤1 retry; `last_error` records the permanent class. | `scripts/test-notification-fanout.sh` (im-core `db_tests::` on own throwaway DB) |
| **AC-3** envelope parity | New **`delete_lane_outbox_parity`** in `db_tests/lanes.rs`, mirroring `recall_lane_outbox_parity` (:245) with the **real producer seam** | N deletes via `soft_delete_outboxed_authorized` in one tx → exactly N 1:1 rows; all **16 envelope keys** field-by-field vs the 0239 spelling (occurred_at = PG `to_jsonb(created_at)` spelling; actor = deleter; targets non-empty; payload = sanitized detail; action verbatim; `idempotency_key == event_id`; no L1 markers); each payload parses into `AuditClaimPayload` and re-serializes Value-equal (Half-A drift alarm). Rollback → 0; replay (same-id dup, varying `created_at`) → still N. | `scripts/test-integration.sh` slot `audit_governance::` |
| **AC-4** regression | — | `audit_governance::` executed slot green with N = **live pre-change count (38) + new tests**, never fewer; `moderation-priority-drill` PASS unchanged (admin/100 still preempts message/10); **b5-pin list unchanged** (no new slot); full flip-inventory (§7) green; `cargo check/clippy` clean; truth/file-size/web checks 0 violations; migration count stays 246. | `scripts/test-integration.sh` + `scripts/b5-pin.sh` guard + §8 commands |

---

## 7. Affected-test inventory (must land in the same commit)

**Flip (assertions change by design):**
1. `producer/carveout.rs:107` — 0-rows pin → AC-1 positive parity (rename keeps `moderation_finalize_outbox_parity` prefix).
2. `gates.rs:53` `non_moderation_action_passes_through_unmapped` — 0239 **trigger** still passes through (0 admin-class rows); Rust writer now yields 1 message-class row: zero-governance assertion → scoped class-`'admin'` assertion + writer-row pin.
3. `dedup.rs:145-175` — after `soft_delete_audited` the ws holds 2 rows (admin backfill + message-class delete row); `governance_rows_for == 1` → scoped count (2, or class-scoped 1); the backfilled-row `fetch_one` must scope `class = 'admin'`. Reconciler idempotency ("again == 0") and "never fabricate moderation claims for the delete row" stay.

**Stay green (comments/literals only):**
4. `lanes.rs` `message_lane_outbox_parity` + `room_lane_unconditional_enqueue` — direct-SQL `insert_audit_row` `"message.deleted"` rows never hit the writer (it lives in the delete paths, not in `AuditRepo::append_in_tx`); update "(R-D2)" comments to "R-D2 trigger-unmapped; Rust-writer-owned via the delete seam".
5. `parity.rs` Half B/C; `events.rs:612` (audit failure precedes writer); `audit.rs` `soft_delete_audited`/`moderated` tests (delete assertions unchanged — they don't count governance rows); `idempotency.rs` (workspace `None` → no audit → no writer); `aero-ai/src/governance.rs` lane-map tests (token stays unmapped); auth/failed_pairs/l1/l1_auth/room producer tests.

**Harness/scripts:** `scripts/truth-check-lib.sh` (§2.5); `scripts/b5-pin.sh` **unchanged**; `scripts/test-integration.sh` **unchanged**.

---

## 8. Definition of done

```bash
cargo check --workspace                                    # clean
cargo test --workspace --lib                               # full green baseline
cargo clippy --workspace --all-targets                     # no NEW warnings
scripts/truth-check.sh && scripts/file-size-check.sh && scripts/web-check.sh   # 0 violations
scripts/test-integration.sh                                # audit_governance:: (≥38+new), l1-aggregation-drill, moderation-priority-drill, b5-pin guard
scripts/test-notification-fanout.sh                        # im-core db_tests:: incl. new drill_* relay leg
ls migrations/*.sql | wc -l                                # stays 246
```

## 9. Out of scope (unchanged from spec)

Retention/GDPR sweep deletes (`WorkspaceRepo::sweep_expired_messages` — produces no `message.deleted` token; needs a separate in-tx audit append first), L1 window extension (rejected §1), repo/connector/reconciler internals, new migrations, new b5-pin slots, web/UI, `message.recalled`/`message.moderated`/`create`/`edit`/`room.*` (already mapped).

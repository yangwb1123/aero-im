# Design — Close the workspace=None silent no-audit branch in ImService::moderate_delete (verification + residual test delta)

> Scope held to the direction: **zero production code change, zero migration, one test-only delta** (the
> `review_authorized` remove-path governance-outbox parity legs). All anchors below are grep-verified at HEAD `d5acefe`.

## §1 Evidence verification verdict (untrusted claims → checked)

| Evidence claim | Verdict | Proof at HEAD |
|---|---|---|
| `messages.rs:632` `workspace.map(\|_\| "message.moderated")` (None = no audit) | ❌ **STALE — confirmed fixed.** | `crates/aero-im-core/src/service/messages.rs:625-633` — `moderate_delete` now `workspace.ok_or_else(Error::Invalid("moderate_delete requires a workspace; refusing un-audited delete (R-D1)"))`; the `map` branch is gone. Commit `d8c732a` exists in history. |
| AiWorker R-D1 (`worker/mod.rs:383-388`) | ✅ True. | `moderation_delete_workspace(job)?` at `mod.rs:389`; helper `mod.rs:183-190` returns `AiError::Invalid` on `workspace_id=None`; unit test `moderation_delete_workspace_refuses_none_fail_closed` at `worker/tests.rs:131`. |
| `moderation_bot.rs` double-guard | ✅ True. | `SkipReason::WorkspaceUnresolvable` (`:158`, recorded at `:478`) + `moderate_delete(job.message_id, Some(workspace), …)` at `:483`. |
| `review_authorized` fail-closed by type; test gap | ✅ True. | `workspace: WorkspaceId` non-Option (`message_reports.rs:243`); in-tx audit via `soft_delete_locked_outboxed_in_tx(tx, message, Some(workspace), Some(reviewer), Some("message.moderated"), …)` (`:300-308`). Test `remove_review_commits_decision_delete_audit_and_outbox_once` asserts `audit_events` count + `event_outbox` count only — **zero `audit_governance_outbox` shape assertions** (the only genuine residual delta). |
| `MODERATION_OUTBOUND_ACTION` | ✅ True. | `common/src/model/audit.rs:150` = `"admin.content.flag"`, const-asserted `:437`; re-exported at `aero_common` root (imported as `aero_common::MODERATION_OUTBOUND_ACTION` in `audit_governance.rs:32`). |
| R9 drill + negative envelope + control half | ✅ True. | `drill_moderate_delete_none_workspace_refuses_like_rd1` (`crash.rs:22-149`): Err(Invalid) / message live / blocks intact / 0 audit / 0 governance / 0 Deleted + `Some(ws)` control half; harness runs ignored suite `--test-threads=1` (`test-integration.sh:623`). |
| Drills pinned as "slots in b5-pin.sh's 37-slot guard" | ⚠️ **Imprecise naming, substantively true.** | `moderation-in-first-batch` (`aero-audit-priority-drill.rs:276`) and `parity-501` (`:350`) are **PASS asserts inside the drill binary**, not slots. The actual b5-pin executed slots are `audit_governance::moderation_finalize_outbox_parity`, `t11-fail-closed`, `moderation-priority-drill` (`b5-pin.sh:37-41`); guard = exactly 37 slots, 15 executed + 22 [PROPOSED] (`assert_b5_contract_pin`). All remain PASS. |

**Design-critical discovery not stated in the evidence** — the 0239 governance enqueue is *conditional*:

- **Gate 1 (fail-open)**: `snaplink_commercial_runtime.enabled` must be `TRUE`, else the trigger returns early → audit row lands, **governance row does not** (0 rows).
- **Gate 2 (fail-closed)**: if enforcement is ON and the workspace has **no binding** (`aero_snaplink_binding_for_workspace`), the trigger **RAISEs, aborting the whole review tx** — the delete itself fails.
- The enforcement singleton is **GLOBAL**; a test that flips it ON must restore it (0235 metering trigger raises `P0001` on later message INSERTs in unbound workspaces — documented in `restore_enforcement_disabled`, `audit_governance.rs:161-171`).

Therefore the R3 test delta cannot be a bare "add assertions to the existing test": the existing test runs with enforcement OFF (fresh-DB default), so a naive "exactly 1 governance row" would fail with 0. The test must **enable enforcement + seed the binding**, then assert, then **restore**.

## §2 API changes

**Production API: none.** `ImService::moderate_delete` keeps signature
`(message_id: MessageId, workspace: Option<WorkspaceId>, reason: &str, digest: &str) -> Result<()>`
with the R-D1 `Error::Invalid` refusal (R1 — zero-change pin). All three producers stay symmetric fail-closed:

1. **AiWorker** — `moderation_delete_workspace(job)?` runtime Err → retry → bounded DLQ (paid call already finalized, retry replays without a second provider call).
2. **moderation_bot** — caller-side `SkipReason::WorkspaceUnresolvable` skip + always passes `Some(workspace)`.
3. **review_authorized** — type-level (non-Option `WorkspaceId`), in-tx audit actor = reviewer.

**Test-only API delta (R3)**: amend `crates/aero-storage/src/message_reports.rs` db_tests —
add a private `enable_enforcement_with_binding(p, ws)` / `restore_enforcement_disabled(p)` pair (byte-identical mirror of the canonical helpers in `audit_governance.rs` db_tests, with a comment pointing at the canonical location), and extend
`remove_review_commits_decision_delete_audit_and_outbox_once` with the governance-outbox parity legs.

## §3 Compatibility constraints

- **No migration, no `cargo build`-before-migrate cycle**: 0239/0240/0241 are landed; migration count unchanged. Test DB must be migrated through **0241** (0239 trigger + 0240 due index + 0241 reconciler all present).
- **No crate dependency changes, no root `Cargo.toml` changes**; `aero-storage` already depends on `aero-common` (const import `aero_common::MODERATION_OUTBOUND_ACTION` mirrors `audit_governance.rs:32`).
- **Existing test legs untouched as parity references**: the audit-count=1, event_outbox-count=1, second-review `NotFound`, and `audit_after=1` assertions keep their exact shapes. Enforcement ON does not alter them (proven by `moderation_finalize_outbox_parity`'s half-4: audit + v1 `snaplink_delivery_outbox` rows both land with enforcement ON).
- **Ordering**: `fixture()` (message INSERT) runs with enforcement OFF (fresh-DB default) → the 0235 metering trigger never fires for the fixture message; enforcement + binding are enabled *after* `fixture()`, *before* `review_authorized`. The review path only UPDATEs messages / INSERTs into `audit_events` + `event_outbox` — no message INSERT, so no entitlement projection is strictly required; the mirror helper includes it anyway (idempotent `ON CONFLICT DO NOTHING`) to stay byte-identical to the proven shape.
- **Query scoping**: governance assertions join `audit_governance_outbox g ON g.event_id = a.id` against the specific `audit_events` row (`action='message.moderated' AND target=$message AND workspace_id=$ws`) — order-independent on the shared harness DB (`event_id` is PK, 1:1), no `reset_governance_table` needed (that helper stays private to `audit_governance.rs`).
- **b5-pin guard untouched**: no slot added/renamed; R4 is a stay-PASS constraint, not an edit.

## §4 Failure modes (of the change itself and of the seam under test)

| # | Mode | Behavior | Detection / mitigation |
|---|---|---|---|
| F1 | Test panics after enabling enforcement, before restore | GLOBAL singleton left ON → every later ignored-suite test INSERTing a message in an unbound workspace fails `P0001` (0235 metering) | Restore **immediately after the new legs** (minimal panic window), not at function end. Backstop: `crash.rs` drill defensively re-asserts `restore_enforcement_disabled`. Harness runs `--test-threads=1`, so blast radius is contained to the suite run. |
| F2 | Enforcement enabled but binding seed missing | 0239 Gate 2 RAISE → review tx aborts → test fails at `review_authorized` with P0001 (and in production the review delete fails closed — designed, pinned by `moderation_finalize_without_binding_aborts_tx`) | Mirror helper seeds runtime + binding + entitlement in one idempotent unit; test failure is loud, never silent. |
| F3 | Governance row absent (0) with enforcement OFF | "exactly 1" assertion fails with count 0 | This is the trap the enable step prevents; a future editor deleting the enable step gets a red test, not a silent green. |
| F4 | `event_id` 1:1 violation (trigger rewritten to mint its own id) | JOIN yields 0 rows or `g.event_id != a.id` → assertion fails | 1:1 join leg is the parity contract's core (P2). |
| F5 | Const drift (`MODERATION_OUTBOUND_ACTION` edited) | Test follows automatically via the Rust const import (cross-pin); DDL literal in 0239 is pinned by `audit_governance.rs` tests separately | No bare literal in the new legs — import the const. |
| F6 | Replay/dup governance rows | `UNIQUE(event_id)` + `ON CONFLICT DO NOTHING` → at most 1; join-scoped count asserts exactly 1 | Dedup contract already pinned by 0239 DDL; test observes it. |

## §5 Migration steps

1. **None.** No new SQL. Precondition for running the new/amended test: a throwaway DB (`CREATE DATABASE`), `cargo build` (embed current migrations), `aero-cli migrate` (through 0241), then `DROP DATABASE` after — per §4.3 live-verification rules. `make migrate-smoke` already replays the full chain on a throwaway DB.
2. **No re-org**: `message_reports.rs` is outside sibling token sets; single-file change (`crates/aero-storage/src/message_reports.rs`).

## §6 Testable acceptance mapping

| Req | Gate (command / harness slot) | Pass condition |
|---|---|---|
| **R1** guard regression pin (zero-change) | `cargo test -p aero-ai --lib worker::tests::moderation_delete_workspace_refuses_none_fail_closed` + code inspection of `messages.rs:625-633` | Unit test green; `moderate_delete(None)` still `Error::Invalid`; signature stays `Option<WorkspaceId>`. |
| **R2** R9 drill envelope | Harness ignored leg: `cargo test -p aero-im-core --lib -- --ignored --test-threads=1 drill_moderate_delete_none_workspace_refuses_like_rd1` (`DATABASE_URL` set) | Full negative envelope (Err Invalid / message live / blocks intact / 0 audit / 0 governance / 0 Deleted frames) + `Some(ws)` control half (1 audit + 1 governance, status 0 / admin / 100). |
| **R3** review_authorized governance parity (the delta) | `DATABASE_URL=<throwaway migrated through 0241> cargo test -p aero-storage --lib -- --ignored --test-threads=1 message_reports::db_tests::remove_review_commits_decision_delete_audit_and_outbox_once` | New legs green: exactly **1** `audit_governance_outbox` row joined to the audit row, `(status,class,priority) = (0,'admin',100)`, `payload->>'action' = MODERATION_OUTBOUND_ACTION` (via const), `event_id` 1:1 with `audit_events.id`, `payload->'actor'->>'id'` = reviewer (differential vs system-actor AiWorker seam), `payload->>'event_id'` mirrors; enforcement restored to OFF; pre-existing legs unchanged. Non-vacuous: query must actually return the row (fails with 0 under enforcement OFF / missing binding). |
| **R4** pinned gates stay PASS | `scripts/b5-pin.sh` (37/37 guard, `assert_b5_contract_pin`) + `scripts/test-b5-pin-guard.sh` + `scripts/test-integration.sh` (0239-gated leg: `moderation-priority-drill` → drill exit 0 with `drill: moderation-in-first-batch: PASS`, `drain-501: PASS`, `parity-501: PASS`; `t11-fail-closed`; `audit_governance::moderation_finalize_outbox_parity` verdict) | All 15 executed slots report PASS; guard prints `B5 contract pin: 37/37 … : PASS`. |

**Full-suite regression bar** (unchanged from §4.3): `cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets` (no new warnings — new test code must be clippy-clean incl. pedantic) · `scripts/{truth-check,file-size-check,web-check}.sh` (0 violations; `file-size-check` threshold for `message_reports.rs` already far below 800 WARN).

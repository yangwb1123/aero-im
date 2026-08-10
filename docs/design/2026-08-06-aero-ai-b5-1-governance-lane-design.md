# Design — B5-1 (aero-ai slice): moderation governance-lane mapping primitive

- **Spec**: `docs/requirements/2026-08-06-aero-ai-b5-1-audit-governance-outbox.req.md`
- **Module**: `crates/aero-ai/src` (new sibling module `governance.rs`)
- **Status**: Design (evidence verified 2026-08-06)

## 0. Evidence verification verdict

All 8 cited items + the spec's supplementary anchors were re-checked against the repo on 2026-08-06. **All verified**; two minor path drifts (symbols exact):

| Evidence | Verdict | Notes |
|---|---|---|
| `audit.rs:113,127` `append`/`append_in_tx`; file = 1049 lines | ✅ | `append`@113, `append_in_tx`@127 (delegates to generic `append_on`@138). db_test precedents `moderation_delete_and_audit_commit_together`@814, `moderation_failed_audit_rolls_back_delete`@874, fixture@489 — all `#[ignore = "requires live Postgres"]`. |
| `message/events.rs::soft_delete_locked_outboxed_in_tx`@299, wrapper@267, `audit_action` param | ✅ | Wrapper `soft_delete_outboxed_system`@267; in-tx core@299 `pub(crate)`. Single tx: lock → soft-delete → `AuditRepo::append_in_tx` (`audit_action`) → `EventOutboxRepo::insert_room_event_in_tx` (RoomEvent::Deleted) → commit. Already-deleted ⇒ `Ok(None)` (idempotent). |
| `im-core/service/outbox.rs::dispatch_event_outbox_batch`@27 | ✅ | `OUTBOX_LEASE = 30s`; `claim_due(now, lease, limit)` → per-row `finish_claimed_outbox` (publish or durable backoff re-park); `dispatch_event_outbox_id`@61 (immediate post-commit, `false` harmless). |
| `0236:129-133` AFTER INSERT trigger → `aero_enqueue_snaplink_audit`@68; reconcile @~146 | ✅ | Trigger `audit_events_snaplink_delivery` AFTER INSERT FOR EACH ROW@129-133. Gates: runtime `enabled` (off ⇒ no row), binding lookup RAISE (missing ⇒ tx aborts, fail-closed). `delivery_id='audit:'\|\|id`, `idempotency_key=id`, `payload.action` verbatim — no mapping/stamping today. `aero_reconcile_snaplink_usage` `CREATE OR REPLACE`@~146 = the redirect precedent. |
| `0235:161` `snaplink_delivery_outbox` DDL — no status/priority/class/delivery_mode | ✅ | Confirmed. Due index@187 `(available_at, created_at, delivery_id) WHERE delivered_at IS NULL` — FIFO only. `UNIQUE (destination, idempotency_key)` + claim-state CHECK. |
| `ai_job.rs:57-68` `priority_for`; claim@179; lane test@421 | ✅ | **Path drift**: file is `crates/aero-storage/src/ai_job.rs` (not aero-ai). `priority_for`@62-68: Moderate=10 < Answer=20 < Sum=50 < Embed=100, "lower runs first". Claim@174-180 `ORDER BY priority ASC, scheduled_at ASC ... FOR UPDATE SKIP LOCKED`. Unit test `priority_lanes_order_user_facing_work_ahead_of_backfill`@421-429. |
| `snaplink_commercial.rs:77,302` `SnaplinkDeliveryClaim`/`claim_due` — no priority in ORDER BY@320 | ✅ | Claim struct@77; `claim_due`@302, `ORDER BY candidate.available_at, candidate.created_at, candidate.delivery_id`@320 — no priority lane. |
| `messages.rs:611,632` `moderate_delete` → `soft_delete_outboxed_system("message.moderated")` | ✅ | **Path drift**: file is `crates/aero-im-core/src/service/messages.rs` (not aero-storage). `moderate_delete`@611; call@632 with `audit_action = workspace.map(\|_\| "message.moderated")`, `actor = None`, `event_actor = ParticipantId::nil()`. `workspace=None` ⇒ `audit_action=None` ⇒ **delete commits with zero audit rows** (audit append gated on both `Some`, events.rs:337) — the reachable un-audited path (§4 row). |
| `message_reports.rs:305` `review_authorized` — **third** `message.moderated` producer (reviewer pin) | ✅ | Live admin report-queue route via `server/src/message_reports.rs:212`. Calls `soft_delete_locked_outboxed_in_tx` inside its own tx with `audit_actor=Some(reviewer)`, `event_actor=reviewer`, `audit_action=Some("message.moderated")`, `detail={room_id, report_id, reason, digest}` — **not** the `actor=None/event_actor=nil` shape. The mapping is token-keyed, so it still stamps identically; the earlier "all three actor=None" claim was false for this path (corrected in §3.4). |
| `scripts/test-integration.sh` `run_migrated_integration`@149-165 — empty-filter hole + 0239-gating precedent | ✅ | A non-matching filter exits 0 (`running 0 tests … 0 passed` → EXIT=0, verified empirically) — a named entry for a not-yet-existing test would be **green before any test exists** (vacuous-pass hole). The AUDIT_CONNECTOR block@206-223 already gates on `[ -f migrations/0239_audit_governance_outbox.sql ]` with a clear SKIP — the in-repo precedent for gating this slice's entries. |
| Spec E9-E12 (handle_moderate@352 + call@373-377; budget.rs CostBudget@98/KeyedCostBudget@215; NotifyBatch = fan-out; gate.md:63 "30 个忽略测试 CI 全绿（37/37）", Makefile:120 migrate-smoke, `run_migrated_integration`@149-165) | ✅ | `handle_moderate`@352; BLOCK branch calls `svc.messages().soft_delete_outboxed_system(id, workspace, None, workspace.map(\|_\| "message.moderated"), detail, ParticipantId::nil(), None)`@373-377. `MAX_ATTEMPTS=5`@68. `AiService::messages() -> &MessageRepo`@service_impl.rs:106. `RoomEvent::NotifyBatch`@common/model/event.rs:86 is fan-out (recipient list), not aggregation — L1 mechanics genuinely have no in-repo precedent. |
| Out-of-repo items (`admin.content.flag` vs `admin.moderation.action`; "30"/"37/37" lists) | ✅ as claimed | `docs/proposals/audit-contract-batch-aero-im.md:10` lists **both** tokens as the B5-3 mapping-table choice — no single in-repo decision exists. Gate row 1 (`implementation-gate.md:63`) references the 30/37 counts without an in-repo list. |

## 1. Design overview

Zero production-behavior change in aero-ai. The slice adds one **pure, DB-free module** — the authoritative moderation governance-lane mapping — plus its unit tests and the drill-fixture contract. It also records the security rulings for the `workspace=None` path and the silent-downgrade surface (§1.1, R-D1/R-D2) — decisions whose enforcement lands in the owning crates and is pinned by A2 halves (8)/(9). The mapping is *consumed* by the 0239 enqueue/stamping (storage/SQL slice, out of scope here) and *pinned* by cross-slice tests (A2 in aero-storage; A3 with B5-3; A4 with [PROPOSED] L1). **A3 and A4 are deferred cross-slice drills — they do not gate this slice** (their green-without-them conditions are stated in §6); this slice's acceptance = R1/R4 units + A1 harness pins + A2 parity. The `AiWorker::handle_moderate` call shape (worker/mod.rs:373-377) is **frozen**: local token `"message.moderated"` stays the audit action; the redirected 0239 enqueue does the class/priority/action stamping in the same transaction (R2).

```
AiWorker::handle_moderate (unchanged, worker/mod.rs:352)
  └─ MessageRepo::soft_delete_outboxed_system(..., audit_action=Some("message.moderated"), ...)
      [ one PG tx: soft-delete + audit append + RoomEvent::Deleted + 0239 governance row ]
      └─ stamping = aero_ai::governance::governance_lane_for("message.moderated")
           → { class='admin', priority=GOVERNANCE_PRIORITY_MODERATION, outbound=MODERATION_OUTBOUND_ACTION }
```

### 1.1 Security decision record (resolved 2026-08-06, security-review closeout)

**R-D1 — `workspace=None` is FAIL-CLOSED (option (a); option (b) documented-accept rejected).** A moderation finalize whose workspace cannot be resolved **must refuse to delete**. The behavior reachable today via `moderation_bot::process`'s lookup-failure degradation (moderation_bot.rs:434-446 → messages.rs:636 `workspace.map(...)` ⇒ `audit_action=None` ⇒ delete + `Deleted` broadcast commit with 0 `audit_events` rows ⇒ 0236 trigger never fires ⇒ 0 governance rows) is a governance hole, not an accepted trade-off. Basis:

1. **Spec R8 fail-closed intent** — R8's invariant is *no removal without its audit trail* ("no deleted but unqueued", "no audited but unqueued"); the `None` path violates it by never *attempting* the audit rather than failing it. A bypass that sidesteps the transaction is worse than a rollback case — it is invisible.
2. **0236 binding-RAISE precedent** — with enforcement on, `workspace=Some` + missing binding ⇒ `RAISE` aborts the whole tx: the delete does not happen. `None` is the **only** route around an otherwise fail-closed system, and it is exactly the route taken when the system is degraded (workspace lookup failing) — i.e., when an un-audited removal is least acceptable. Fail-closed must not have a documented back door.
3. **moderation_bot's fail-open is budget-scoped** — the deliberate fail-open (moderation_bot.rs:24-40) governs *screening*: budget exhausted ⇒ skip the LLM call, message stays visible. Applied to *removal*, the same "skip the action" keeps the message visible — a screening miss, not a governance hole. **Budget fail-open must not leak into the audit lane.**
4. **In-repo fail-closed precedents** — `soft_delete_moderated` (crud.rs:398) takes `workspace: WorkspaceId` non-optional; `moderation_failed_audit_rolls_back_delete` (audit.rs:874) proves missing-workspace audit ⇒ FK error ⇒ rollback ⇒ message retained; `side_effects.rs:161` resolves workspace with `.context(...)?`.

**Enforcement (code lands in the owning crates; the A2-pinnable half is verbatim at §6 A2 (8)):**

1. **Seam guard** — `soft_delete_locked_outboxed_in_tx` (events.rs:299): `(None, Some(audit_action))` ⇒ `Err` — a requested audit without a workspace must abort, never silently skip (`audit_events.workspace_id NOT NULL`, 0146). Zero current-caller impact — verified: no production caller constructs `(None, Some)` (all pass `workspace.map(...)` or `Some(ws)`).
2. **Caller guards** — `ImService::moderate_delete` (messages.rs:611): `workspace=None` ⇒ `Err` before any DB write; `AiWorker::handle_moderate` (worker/mod.rs:374): `job.workspace_id=None` ⇒ `Err` → existing retry path → bounded DLQ (verdict durably finalized; no second paid call); `moderation_bot::process` (moderation_bot.rs:434-446): on workspace-lookup failure, screening may proceed under the global gate (budget fail-open preserved) but a flagged verdict **skips the removal** — `warn!` + `record_skip` (message stays visible and reviewable); it must not call `moderate_delete` with `None`.

**R-D2 — silent-downgrade surface: explicit pins required; single-constant + frozen call shape is prevention, NOT sufficient mitigation.** A token typo (a moderation finalize audited as `"message.deleted"`) is exactly where the convention fails: the delete is audited (looks fine), `governance_lane_for` → `None`, no `admin.content.flag`, and L1's high-volume `message.*` window would merge it. Required pins: (a) unit `governance_lane_for("message.deleted") == None` && `is_admin_class("message.deleted") == false` (dedicated `user_delete_token_stays_out_of_admin_lane` + `"message.deleted"` added to `unknown_local_token_passes_through_unmapped`); (b) A2 user-path negative half (9). Together they pin the pass-through negative as deliberate — a future change routing `message.deleted` into the admin lane, or turning pass-through into a raise (second abort path), fails both sides; the moderation half's `action='message.moderated'` field assertion (A2 (5)) catches the typo on the audit side. Convention (single `LOCAL_ACTION_MODERATED` constant + frozen call shape) remains the prevention layer (§3.3).

## 2. API changes

### New file `crates/aero-ai/src/governance.rs` (pure; no DB, no I/O, no tokio)

```rust
//! Governance-lane mapping for local audit action tokens (B5-1 R1).
//! Pure + unit-pinned, mirroring `aero_storage::ai_job::priority_for`
//! (ai_job.rs:62-68) as the lane *model* — NOT its numeric direction.

/// Governance claim ordering is `priority DESC` (B5-3: highest = first) —
/// the INVERSE of ai_job's ASC lower-first model. Do not "align" these.
/// Moderation is the top lane.
pub const GOVERNANCE_PRIORITY_MODERATION: i16 = 100;
/// Default lane for message/room backlog rows (also the 0239 column default).
pub const GOVERNANCE_PRIORITY_BACKLOG: i16 = 10;

pub const GOVERNANCE_CLASS_ADMIN: &str = "admin";
pub const GOVERNANCE_CLASS_MESSAGE: &str = "message";
pub const GOVERNANCE_CLASS_ROOM: &str = "room";

/// Local audit token produced by every moderation finalize producer.
pub const LOCAL_ACTION_MODERATED: &str = "message.moderated";

/// Single outbound contract token. Contract decision (proposal
/// audit-contract-batch-aero-im.md:10 lists both candidates); locked here as
/// ONE constant — change is a one-line edit + A2 follows automatically.
pub const MODERATION_OUTBOUND_ACTION: &str = "admin.content.flag";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GovernanceLane {
    pub class: &'static str,          // one of GOVERNANCE_CLASS_*
    pub priority: i16,                // DESC lane value (higher = claimed first)
    pub outbound_action: &'static str,
    /// status 0 is the enqueue-time normative state (0239); a property of the
    /// outbox row lifecycle, pinned here as the drill fixture's start state.
    pub status: i16,                  // 0 = queued
}

/// Map local audit action token → governance tuple. Keyed on the token ONLY
/// (never caller identity): every `message.moderated` producer —
/// AiWorker::handle_moderate (worker/mod.rs:373), ImService::moderate_delete
/// (messages.rs:632, incl. moderation_bot@moderation_bot.rs:472), and
/// message_reports::review_authorized (message_reports.rs:305, which passes
/// actor=Some(reviewer)) — maps identically.
///
/// Unknown tokens → `None` (pass through unmapped). Never raises, never
/// blocks: non-moderation audit rows must keep flowing through the shared
/// 0239 trigger/redirect.
pub fn governance_lane_for(local_action: &str) -> Option<GovernanceLane> {
    match local_action {
        LOCAL_ACTION_MODERATED => Some(GovernanceLane {
            class: GOVERNANCE_CLASS_ADMIN,
            priority: GOVERNANCE_PRIORITY_MODERATION,
            outbound_action: MODERATION_OUTBOUND_ACTION,
            status: 0,
        }),
        _ => None,
    }
}

/// R5 classification for the [PROPOSED] L1 aggregation bypass: admin-class
/// rows must stay 1:1 (event_id = audit_events.id), never merged by the
/// high-volume `message.*` window.
pub fn is_admin_class(local_action: &str) -> bool {
    matches!(governance_lane_for(local_action),
             Some(lane) if lane.class == GOVERNANCE_CLASS_ADMIN)
}
```

### `crates/aero-ai/src/lib.rs`

Add, matching the existing `pub mod` + selective `pub use` pattern (lib.rs:16-40):

```rust
pub mod governance;
pub use governance::{
    governance_lane_for, is_admin_class, GovernanceLane,
    GOVERNANCE_CLASS_ADMIN, GOVERNANCE_PRIORITY_MODERATION,
    MODERATION_OUTBOUND_ACTION,
};
```

Names are unique in the crate root — no collision with the `webhook`/`scim`/`invitation` token-helper rule (§4.2). Re-exporting keeps the module part of the crate's public API, so the zero-call-in-production module is **not** dead code (lib crate; `pub` reachable items don't trip `dead_code`/`truth-check` — same standing as `budget`).

### Explicitly NOT changed (frozen contracts)

- `AiWorker::handle_moderate` (worker/mod.rs:352, call@373-377) — same call shape, same `audit_action`, no post-commit enqueue (R2).
- `MessageRepo::soft_delete_outboxed_system` / `soft_delete_locked_outboxed_in_tx` (message/events.rs:267/299).
- `AuditRepo::append_in_tx` (audit.rs:127) — `audit_events.action` stays `'message.moderated'`, `actor_id NULL` (R3).
- `EventOutboxRepo`, `dispatch_event_outbox_batch` pump, `SnaplinkDeliveryClaim::claim_due` (snaplink_commercial.rs:302) — untouched; B5-3 owns the priority ordering.

## 3. Compatibility constraints

1. **Dependency direction**: aero-ai → aero-storage (Cargo.toml:14). The mapping lives in aero-ai as fixture authority; the 0239 stamping runs in SQL/storage with **duplicated literals**. aero-storage production code must never import aero-ai. Cross-slice agreement is pinned two ways: (a) textual — the A2 db_test carries a `// cross-slice pin: must equal aero_ai::governance::{...}` comment beside each literal; (b) **behavioral** — the A3 drill asserts the stamped row is claimed first under B5-3's DESC ordering, which catches drift regardless of how the literals were duplicated. Optional hardening: aero-storage `[dev-dependencies] aero-ai` (Cargo-legal dev-dep cycle) so the A2 test asserts `row.priority == aero_ai::governance::GOVERNANCE_PRIORITY_MODERATION` directly — conservative default is literals + comment; adopt the dev-dep only if the campaign reviewer accepts the workspace-pattern change.
2. **Lane-direction inversion trap**: ai_job `priority_for` is ASC lower-first; B5-3 governance claim is `priority DESC` higher-first. The R1 constants and unit tests encode DESC; a comment in `governance.rs` forbids "fixing" the direction to match ai_job. The spec's R4 phrasing ("moderation lane < message/room default lane") is a mindset carry-over from the ASC model — the pinning property is *precedence* (moderation claimed first), which under DESC means `GOVERNANCE_PRIORITY_MODERATION > GOVERNANCE_PRIORITY_BACKLOG`.
3. **Pass-through invariant**: `governance_lane_for` returns `None` for any non-`message.moderated` token. The 0239 redirect must keep this fail-open mapping branch — a raise on unmapped tokens would block all `room.*`/`message.*` audit rows through the shared trigger (E4's binding RAISE already aborts fail-closed on missing binding; the mapping must not add a second abort path). `None` is also a **silent-downgrade surface**, not just an abort-avoidance property (security-review pin): a token typo (`Some("message.deleted")` via the raw `pub` seam) commits delete + audit under the *user-delete* classification — looks audited, no `admin.content.flag`, and L1's high-volume `message.*` window would merge it. **Ruling (R-D2, resolved 2026-08-06): convention is prevention, not mitigation — single constant + frozen call shape do NOT suffice alone** (a token typo is exactly where a convention fails). Required pins: (a) unit `governance_lane_for("message.deleted") == None` && `is_admin_class("message.deleted") == false` (dedicated `user_delete_token_stays_out_of_admin_lane`, plus `"message.deleted"` added to `unknown_local_token_passes_through_unmapped` — §6 R1 row); (b) A2 user-path negative half (9) `moderation_delete_with_wrong_token_stays_out_of_admin_lane` (1 audit row under `message.deleted`, 0 moderation/admin-lane governance rows — §6 A2 row). Together they pin the pass-through negative as deliberate — a future change routing `message.deleted` into the admin lane, or turning pass-through into a raise (second abort path), fails both sides; the moderation half's `action='message.moderated'` field assertion catches the typo on the audit side. Convention (single `LOCAL_ACTION_MODERATED` constant + frozen call shape) remains the prevention layer.
4. **Token-keyed, not caller-keyed** — three distinct production producers, all passing the exact literal `"message.moderated"` (reviewer-pinned inventory; `moderate_delete` and `moderation_bot` are **one call chain**, not two):
   - `AiWorker::handle_moderate` (worker/mod.rs:373-377): `actor=None`, `event_actor=nil`, `detail={reason, source:ai_worker}`.
   - `ImService::moderate_delete` (messages.rs:611/632) — the `moderation_bot` path (moderation_bot.rs:472): `actor=None`, `event_actor=nil`, `detail={room_id, reason, digest}`.
   - `message_reports::review_authorized` (message_reports.rs:305; live route server/src/message_reports.rs:212): `actor=Some(reviewer)`, `event_actor=reviewer`, `detail={room_id, report_id, reason, digest}` — the distinct-shape producer the earlier inventory missed.
   The mapping sees only the token ⇒ all three stamp identically; per-producer actor/detail differences must not leak into the mapping branch. A2 carries a half exercising the report-path shape (`actor=Some(reviewer)`) so the stamp is pinned independent of caller identity.
5. **v1/v2 coexistence**: `snaplink_delivery_outbox` (0235) stays live through transition. The 0239 `CREATE OR REPLACE` redirect (precedent: `aero_reconcile_snaplink_usage`@0236:146) owns whether moderation rows appear in v1, v2, or both — this direction requires exactly the v2 row per A2, with `UNIQUE (destination, idempotency_key)` + redirect as the duplicate-enqueue guard (E5).
6. **Gates preserved or re-verified**: A2/A3 setup needs `snaplink_commercial_runtime.enabled = true` + an enabled binding for the workspace (E4; missing ⇒ trigger RAISE aborts the whole tx — the fail-closed property R8 relies on). If the 0239 redirect drops these gates, the abort-on-missing-binding behavior changes and must be re-verified before this direction's A2 rolls back green. **Mechanical gate, not prose**: A2 halves (3)+(4) — `moderation_finalize_without_binding_aborts_tx` and `moderation_finalize_runtime_disabled_commits_1_plus_0` — pin both gates as named tests; a redirect that drops either gate fails A2.
7. **Lint discipline**: new module must compile warning-free under root lints (`cargo clippy --workspace --all-targets` — pedantic: `&'static str` in struct is fine; `matches!` preferred over manual match; doc-comment every pub item; `unreachable_pub` satisfied by the lib.rs re-export).
8. **No new crate deps** — the module uses only `std`. `crates/aero-ai/Cargo.toml` untouched.

## 4. Failure modes

| Failure | Behavior | Guard |
|---|---|---|
| Provider verdict failure in `handle_moderate` | `Err` propagates → job retry → bounded DLQ (`MAX_ATTEMPTS=5`@68, `is_over_attempt_cap`@~500); verdict durably finalized under the job's stable usage context ⇒ **no second paid call** (E9 comment invariant) | Unchanged worker semantics (R8) |
| Any in-tx failure (audit append, 0239 enqueue/trigger, binding lookup RAISE) | Whole tx rolls back: soft delete + audit + outbox together — no "deleted but unqueued", no "audited but unqueued" | `soft_delete_locked_outboxed_in_tx` single tx + `append_in_tx` (R8) |
| Replay of a committed finalize (worker retry, at-least-once) | `soft_delete_outboxed_system` returns `Ok(None)` (already deleted) → no second audit row; `UNIQUE (destination, idempotency_key)` = schema-level second guard | A2 replay half (R7) |
| Unknown local token reaches 0239 | Must pass through unmapped (`None`) — never raise/block | R1 unit test `unknown_local_token_passes_through_unmapped`; A2 non-moderation families (parallel slices) |
| **`workspace=None` moderation delete** — reachable today: `moderation_bot::process` workspace-lookup failure degrades to `None` (moderation_bot.rs:434-446, "global gate only") → messages.rs:636 `workspace.map(\|_\| …)` ⇒ `audit_action=None` | **FAIL-CLOSED (R-D1, resolved 2026-08-06 — supersedes the interim documented-accept pin): a moderation finalize whose workspace cannot be resolved must refuse to delete.** The old behavior (delete + `Deleted` broadcast **commit** with **0 audit + 0 governance rows**, audit append gated on *both* `Some` at events.rs:337) falsified moderation_bot.rs:7's "a removal can never succeed unaudited" and was the only route around the otherwise fail-closed 0236 binding RAISE — the single-tx guarantee covers "audit write fails after delete begins", not "audit never attempted" | **R-D1 enforcement**: (1) seam guard events.rs:299 — `(None, Some(audit_action))` ⇒ `Err`, audit requested without a workspace must abort, never silently skip (`audit_events.workspace_id NOT NULL`, 0146); zero current-caller impact (no production caller constructs `(None, Some)` — all pass `workspace.map(...)` or `Some(ws)`); (2) caller guards — `moderate_delete` (messages.rs:611) refuses `None` (Err before any DB write); `handle_moderate` (worker/mod.rs:374) treats `workspace_id=None` as `Err` → retry → DLQ (verdict durably finalized, no second paid call); `moderation_bot::process` skips the **removal** on lookup failure (`warn!` + `record_skip`, message stays visible) while budget-gated screening may still proceed — budget fail-open does not leak into the audit lane; (3) **A2 half (8) verbatim** — `moderation_finalize_workspace_none_refuses_delete` (§6); rationale §1.1 |
| Token-mismatch via raw `pub` seam (`audit_action=Some("message.deleted")` or any non-moderated literal) | Delete commits + 1 audit row under the *user-delete* token + **0 governance rows** — silently classified user-delete, would be merged by L1's `message.*` window | **Decision: R-D2 — pinned negative; convention is prevention only, not sufficient mitigation** — unit `user_delete_token_stays_out_of_admin_lane` (`governance_lane_for("message.deleted") == None` && `is_admin_class(...) == false`) + A2 half (9) `moderation_delete_with_wrong_token_stays_out_of_admin_lane` (1 audit row under `message.deleted`, 0 moderation/admin-lane governance rows); single constant + frozen call shape alone do **not** suffice (a typo is exactly where a convention fails, §3.3/§1.1 R-D2) |
| Lane-value drift aero-ai constant vs 0239 SQL literal | Moderation rows sort with backlog ⇒ B5-3 first-claim drill fails | A3 drill (behavioral pin); A2 field assertion (textual pin) |
| Contract later picks `admin.moderation.action` | One-line change in `governance.rs`; A2 asserts against the constant so it follows automatically. A3 does **not** depend on the token string (only priority/class) — token drift is caught by A2, lane drift by A3 | Single-constant design (§2) |
| Binding/runtime gates dropped by 0239 redirect | Currently-aborting fail-closed behavior changes; A2 rollback half may stop exercising the abort path | **Mechanical gate**: `moderation_finalize_without_binding_aborts_tx` fails if the binding RAISE is dropped ⇒ A2 cannot roll green against that 0239; `moderation_finalize_runtime_disabled_commits_1_plus_0` pins the fail-open runtime gate so a redirect can't silently flip either (A2 halves 3+4) |
| Stamped row never aggregated | L1 window must skip `class='admin'` rows (R5); `is_admin_class` is the classification authority | A4.2 bypass assertion; A2 set-difference stays green after L1 lands |

## 5. Migration steps

**This direction owns no migration** (0239 is the B5-1 storage/migrations slice; `ls migrations/*.sql | wc -l` = 238 today, next = 0239). Landing order per spec §8:

1. **This slice (aero-ai)**: add `governance.rs` + unit tests; `cargo check --workspace`, `cargo clippy --workspace --all-targets`, `cargo test --workspace --lib` (unit tests are in the plain run, not the `--ignored` set).
2. **B5-1 storage/migrations slice**: `0239_audit_governance_outbox.sql` (status 0/1/2/3 normative, class message/room/admin, priority with `GOVERNANCE_PRIORITY_BACKLOG` default, delivery_mode) + `CREATE OR REPLACE` enqueue/reconcile redirect stamping via the token→tuple literals + `audit_governance.rs` repo + claim pump (clone of `dispatch_event_outbox_batch`@27/`SnaplinkDeliveryClaim`@77). **Order: `cargo build` before `aero-cli migrate`** (migrations are compile-time embedded — §4.2). A2 parity db_test lands here (v1 shape first per spec §8.1, upgraded to v2 fields once 0239 replays).
3. **B5-3 (aero-storage)**: `claim_due ORDER BY priority DESC` + anti-starvation cap → unlocks A3.
4. **L1 aggregation ([PROPOSED])**: after A2 parity is green (fixed baseline for A4.2).
5. **B5-2 connector / B5-4 aero-cli**: relay + provisioning — campaign-level, not in this acceptance list.

Acceptance hooks this slice adds to the harness (`scripts/test-integration.sh`):
- **Empty-filter guard inside `run_migrated_integration`@149-165**: after the `cargo test` run, assert the `test result:` line shows ≥1 passed (or `cargo test … -- --list` + grep) — a non-matching filter currently exits 0 (`running 0 tests … 0 passed`), so a named entry would be green before any test exists (testing-reviewer pin, verified empirically).
- **Named `run_migrated_integration` entries** for the storage-side `audit_governance::*` and the moderation-finalize parity test (`moderation_finalize_outbox_parity`, §6 A2), each on its own throwaway migrated DB with `--test-threads=1`, **gated on `[ -f migrations/0239_audit_governance_outbox.sql ]`** — the AUDIT_CONNECTOR precedent@206-223 (clear SKIP while the table is absent); each entry must match ≥1 existing test (the guard above enforces it, A1.3).
- `make migrate-smoke` (Makefile:120) replays the full chain including 0239 (globs `migrations/*.sql`, no compile step; 0239 must be self-contained and not depend on post-0239 objects).

## 6. Testable acceptance mapping

| Acceptance | Artifact | Assertions (concrete) | Location / harness |
|---|---|---|---|
| **R1/R4 unit tests** | `governance.rs` `#[cfg(test)] mod tests` (clone of `priority_lanes_order_user_facing_work_ahead_of_backfill`@ai_job.rs:421) | `moderation_lane_preempts_backlog_under_desc_claim`: `priority("message.moderated") == 100 > GOVERNANCE_PRIORITY_BACKLOG` + comment pinning B5-3 DESC semantics. `unknown_local_token_passes_through_unmapped`: `governance_lane_for("room.create"/"message.create"/"message.deleted"/"") == None`, `is_admin_class(...) == false`. `user_delete_token_stays_out_of_admin_lane` (R-D2): `governance_lane_for("message.deleted") == None` && `is_admin_class("message.deleted") == false` — the pass-through negative is deliberate, not incidental. `mapping_is_token_keyed`: single arm, pure fn — same token ⇒ same tuple; `"message.moderated"` is the only admin-class token. `outbound_action_is_single_contract_token`: tuple's `outbound_action == MODERATION_OUTBOUND_ACTION`, `class == GOVERNANCE_CLASS_ADMIN`, `status == 0`. `admin_class_rows_never_aggregated` (R5 classification): `is_admin_class("message.moderated") == true` | `cargo test --workspace --lib` (plain run; no PG) |
| **A1** | `scripts/test-integration.sh` named entries + empty-filter guard | (1) `make migrate-smoke` exits 0 — 0239 replays on a brand-new throwaway DB (fresh-deploy chain; count = `ls migrations/*.sql | wc -l`, never hardcoded). (2) **A1.3 (rewritten — the unassertable 30-test minimum is dropped)**: every named `run_migrated_integration` entry (`audit_governance::*`, `moderation_finalize_outbox_parity`) **matches ≥1 existing test** and is green on its own throwaway migrated DB, `--test-threads=1`; the harness **empty-filter guard** fails the entry on `running 0 tests` — no vacuous green. (3) New entries **gated on `[ -f migrations/0239_audit_governance_outbox.sql ]`** (AUDIT_CONNECTOR precedent, clear SKIP). The contract's 30/37 counts stay out-of-repo contract text — no in-repo 30-test minimum is claimed or assertable (testing-reviewer pin) | shell |
| **A2** (moderation family, production seam) | aero-storage db_test `moderation_finalize_outbox_parity` next to `moderation_delete_and_audit_commit_together`@audit.rs:814 (must live in aero-storage: the rollback half needs the `pub(crate)` in-tx seam, unreachable from aero-ai); executes the **exact `handle_moderate` call shape** (`soft_delete_outboxed_system(..., Some("message.moderated"), detail, ParticipantId::nil(), ...)`). Fixture = `SnaplinkCommercialRepo::configure_enabled` + `desired_bindings` over **all** workspaces (snaplink_commercial_db_tests.rs `Fixture::new` — complete-coverage requirement), runtime enabled **before** the op, `audit_client_id ≠ client_id` (0237 separation RAISE), revision ≥ 1 | **All nine halves (reviewer-pinned)** — window = the test's own artifacts only: `A = audit_events WHERE workspace_id=$ws AND action='message.moderated' AND target=<message_id>` (precedent filter@audit.rs:825), `G = snaplink_delivery_outbox WHERE workspace_id=$ws AND destination='audit'` (v1) / v2 table; join `G.idempotency_key = A.id::text` (**not** `delivery_id`'s `audit:` prefix, **not** `target`); whole-table/time windows are invalid (shared step-4 SMOKE_DB, partitioned `audit_events`): (1) **commit 1+1** — baseline `\|A\|=0 ∧ \|G\|=0` *before* the op; after commit `\|A\|=1 ∧ \|G\|=1`; set-difference `A−G.event_id=∅ ∧ G.event_id−A=∅` (broken trigger ⇒ `\|G\|=0`; double-enqueue ⇒ `\|G\|=2` or UNIQUE violation; skipped audit ⇒ `\|A\|=0`); (2) **explicit ROLLBACK 0+0 via the pub(crate) in-tx seam** — `pool.begin()` → `lock_message_in_tx` (events.rs:388) → `soft_delete_locked_outboxed_in_tx` (events.rs:299) → `ROLLBACK` → `\|A\|=0 ∧ \|G\|=0` (fires the trigger in-tx; the failure-induction mirror `moderation_failed_audit_rolls_back_delete`@874 never fires the trigger — do **not** copy that shape); (3) **`moderation_finalize_without_binding_aborts_tx`** — runtime enabled, workspace **unbound** ⇒ op `Err` (binding RAISE P0001 aborts the tx) ⇒ message still present, `\|A\|=0 ∧ \|G\|=0`; if the 0239 redirect drops the binding lookup **this test fails ⇒ A2 cannot roll green against that 0239**; (4) **disabled-runtime 1+0 pair** — `moderation_finalize_runtime_disabled_commits_1_plus_0`: runtime disabled ⇒ commit succeeds with `\|A\|=1 ∧ \|G\|=0` (documents the fail-open runtime gate so a redirect can't silently flip either gate). Plus: (5) field assertions — governance row `class='admin'`, priority = `GOVERNANCE_PRIORITY_MODERATION`, outbound = `MODERATION_OUTBOUND_ACTION`, `status 0`; audit `action='message.moderated'`, `actor_id IS NULL`; (6) replay ⇒ `Ok(None)`, still 1+1 (R7); (7) **report-path half** — `review_authorized` call shape (`actor=Some(reviewer)`, `event_actor=reviewer`) stamps identically (same tuple) — pins token-keyed-not-caller-keyed; (8) **fail-closed half (R-D1, verbatim)** — `moderation_finalize_workspace_none_refuses_delete`: the seam-guard shape — `soft_delete_outboxed_system(id, None, None, Some("message.moderated"), detail, ParticipantId::nil(), None)` ⇒ **`Err`**, message retained (`deleted_at IS NULL`, blocks intact), `\|A\|=0 ∧ \|G\|=0` on the `(action='message.moderated', target=<id>)` window (no audit row ⇒ trigger never fires ⇒ no governance row). Pins the structural half of R-D1: an audit requested without a workspace must abort, never silently skip (`audit_events.workspace_id NOT NULL`, 0146) — the `if let (Some, Some)` silent skip (events.rs:337) becomes structurally impossible. The caller-level half (`moderate_delete`/`handle_moderate` refusing `None`; bot skipping the removal) is enforced in aero-im-core/aero-ai/aero-server tests — unreachable from aero-storage's A2, pinned by R-D1 (§1.1); (9) **user-path negative half (R-D2, verbatim)** — `moderation_delete_with_wrong_token_stays_out_of_admin_lane`: the user-delete shape (`authorization.rs:498`): `soft_delete_outboxed_system(id, Some(ws), Some(actor), Some("message.deleted"), detail, actor, None)` ⇒ exactly 1 `audit_events` row (`action='message.deleted'`, `actor_id=actor`) and **0 governance rows in the moderation/admin window** — no row stamped `class='admin'`, no `MODERATION_OUTBOUND_ACTION`, no `payload.action='message.moderated'`; the row stays out of the admin lane (v1: `payload.action='message.deleted'` verbatim; under 0239: backlog class/priority). Pins the pass-through negative; a moderation delete mis-tokened `message.deleted` fails the audit-side assertion in (5) and the governance-side assertion in (9) | PG `#[ignore]`, `run_migrated_integration` entry (0239-gated) |
| **R6 drill fixture** | Stamping path spec: the A2/A3 fixtures produce the row via the production seam with `audit_action=Some("message.moderated")`; expected stamp = R1 tuple + `status 0` (no bespoke INSERT). `workspace`/`audit_action` must both be `Some` — under R-D1 the seam guard turns the old silent skip (events.rs:337) into `Err` for `(None, Some(...))` (fail-loud), and `(None, None)` writes no audit ⇒ no governance row. **The one non-seam step**: A3's staggered `available_at` — the column defaults to `clock_timestamp()` (0235:171), so staggering is a backdating **UPDATE** on seam-produced rows (backdating ≠ bespoke INSERT). Counter-example not to copy: `aero-audit-relay-drill` (bin/aero-audit-relay-drill.rs:78-96) seeds via direct INSERT — its own doc comment flags that as the seam-to-be-replaced | Used by A2 and B5-3's 500-backlog drill | aero-storage db_tests |
| **A3** (with B5-3) — **DEFERRED, not gating this slice** | aero-storage db_test (owned by B5-3: `claim_due ORDER BY priority DESC` + anti-starvation cap are **explicitly B5-3's change, not this slice's**) | 500 backlog rows (message/room class, `GOVERNANCE_PRIORITY_BACKLOG`, staggered `available_at` all due) + 1 moderation row via R6 fixture ⇒ first `claim_due` returns the moderation row regardless of enqueue order; after K batches backlog keeps draining (anti-starvation cap per B5-3 DDL). **Green-without-them**: this slice's A3 contribution is the R6 fixture, which is exercised by A2 regardless of B5-3; the first-claim drill runs only after B5-3 lands (0239+B5-3-gated, SKIP-pattern like the AUDIT_CONNECTOR block) | PG `#[ignore]`, deferred |
| **A4** (with [PROPOSED] L1) — **DEFERRED, not gating this slice** | storage slice + this direction's bypass pin (`is_admin_class` unit) | N high-volume `message.*` events in one window ⇒ outbound rows ≤ 1, aggregate count == N; admin-class moderation rows never merged (each keeps `event_id` 1:1); A2 parity db_test unchanged and green with L1 enabled. **Green-without-them**: this slice's A4 contribution is the `is_admin_class` classification (unit-pinned: `admin_class_rows_never_aggregated`) + the A2 set-difference regression baseline that L1 must keep green; the aggregation-window assertions run only after L1 lands ([PROPOSED], no in-repo precedent — `NotifyBatch` is fan-out, not aggregation) | PG `#[ignore]`, deferred |

## 7. Open items (unchanged from spec §7)

- Exact outbound token: **locked in this design to `admin.content.flag`** as the single constant (listed first in gate row 3 and proposal:10); B5-3/contract confirmation can flip it with a one-line edit — the A2 assertion follows automatically.
- "30 db_tests / 37/37" lists remain out-of-repo contract text; **no in-repo 30-test minimum is claimed** — A1.3 requires every named `run_migrated_integration` entry to match ≥1 existing test and be green (harness empty-filter guard).
- **`workspace=None` un-audited delete — RESOLVED (R-D1, 2026-08-06): fail-closed** (§1.1, §4 row, A2 half (8)). The interim documented-accept pin is superseded: `moderate_delete`/`handle_moderate` refuse to delete when the workspace cannot be resolved (side_effects.rs:161 pattern), the seam guard aborts a requested-but-unwritable audit (events.rs:299), and `moderation_bot::process` skips the removal (message stays visible) on lookup failure. Enforcement code lands in the owning crates — no longer an open item.
- L1 aggregation mechanics remain [PROPOSED] — **A4 is deferred**, not gating this slice; the bypass classification + A2 regression baseline are the in-slice pins.
- **A3 is deferred** pending B5-3's `claim_due ORDER BY priority DESC` + anti-starvation cap; the R6 fixture (this slice's contribution) is exercised by A2 regardless.

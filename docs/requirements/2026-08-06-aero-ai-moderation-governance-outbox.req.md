# Requirements Spec — AI moderation finalize → B5 audit governance outbox (class + priority mapping, same transaction)

- **Module**: `crates/aero-ai`
- **Direction**: "Route AI moderation finalize into the B5 audit governance outbox with class + priority mapping (message.moderated → admin.content.flag / admin.moderation.action), same transaction"
- **Source analysis**: `docs/auto/analyses/crates-aero-ai-f8cd3622.json` (direction #1; value 9 / risk-reduction 8 / effort 4 / confidence 9)
- **Campaign**: `aero-im-b5-outbox-relay` (`docs/campaigns/campaign-aero-im-b5.yaml`); in-repo contract anchor `docs/proposals/audit-contract-batch-aero-im.md`
- **Status**: Requirements (verified evidence below)
- **Verification date**: 2026-08-06 (line numbers are as-of-verification anchors; drift is possible — the **file/symbol** is the stable grep anchor per AGENTS.md §0)

## 1. Evidence verification (every cited symbol checked against the repo)

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-ai/src/worker/mod.rs::handle_moderate` (soft_delete_outboxed_system with "message.moderated") | ✅ **Verified**. `handle_moderate` at line 352; BLOCK verdict → `soft_delete_outboxed_system(id, workspace, None, workspace.map(\|_\| "message.moderated"), detail, ParticipantId::nil(), None)` at lines 373–377. Note: audit action is **only set when `job.workspace_id` is `Some`** (`workspace.map(...)`); actor is `None` (system). `Err` propagates → job retried → bounded DLQ (lines 388–396); comment confirms provider verdict is durably finalized under the job's stable usage context, so a retry replays without a second paid call. |
| E2 | `crates/aero-storage/src/message/events.rs::soft_delete_outboxed_system` (line 267, same-tx delete + audit) | ✅ **Verified**. Line 267. One tx: `lock_message_in_tx` → `soft_delete_locked_outboxed_in_tx` (UPDATE `deleted_at`/`blocks`/`searchable_text`/`embedding`/`version`; cleanup associations; blob enqueue) → `AuditRepo::append_in_tx` at line 337 → `EventOutboxRepo::insert_room_event_in_tx` (RoomEvent::Deleted) → commit. Returns `Ok(None)` when already deleted (idempotent no-op → **no audit, no outbox row on replay**). |
| E3 | `crates/aero-storage/src/audit.rs::append_in_tx` | ✅ **Verified**. Line 127. Transaction-scoped INSERT into `audit_events`; id generated client-side (`AuditId::new()` in shared `append_on`). Existing db_test precedent at audit.rs:808 `moderation_delete_and_audit_commit_together` (exactly-one-row assertion, actor None, detail carries reason/digest) and audit.rs:852 `moderation_failed_audit_rolls_back_delete` (rollback removes both). |
| E4 | `migrations/0236_snaplink_governance_reconciliation.sql` (`aero_enqueue_snaplink_audit` fn line 68, trigger lines 129–133) | ✅ **Verified**. Function at line 68; trigger `audit_events_snaplink_delivery` `AFTER INSERT ON audit_events FOR EACH ROW` at lines 129–133. **Key behaviors**: (a) gated on `snaplink_commercial_runtime.enabled` — disabled ⇒ `RETURN NEW` with **no outbox row**; (b) `aero_snaplink_binding_for_workspace(NEW.workspace_id)` **RAISEs** `commercial binding is unavailable` when no enabled binding ⇒ aborts the whole moderation transaction (fail-closed); (c) enqueues `delivery_id = 'audit:' \|\| NEW.id`, `idempotency_key = NEW.id`, payload `action = NEW.action` (i.e., the **local** token 'message.moderated' passes through verbatim — no mapping exists today). |
| E5 | `migrations/0235_snaplink_commercial_control_plane.sql` (`snaplink_delivery_outbox` line 161: no status/priority/class columns) | ✅ **Verified**. DDL at line 161: `delivery_id/destination/workspace_id/tenant_id/client_id/source_system/idempotency_key/payload/occurred_at/available_at/attempts/claim_token/lease_expires_at/delivered_at/last_error/created_at` + `UNIQUE (destination, idempotency_key)`. **Confirmed: no status enum, no priority, no class**. Claim index `snaplink_delivery_due_idx (available_at, created_at, delivery_id) WHERE delivered_at IS NULL` (line 187); Rust claim at `crates/aero-storage/src/snaplink_commercial.rs:302` orders `ORDER BY candidate.available_at, candidate.created_at, candidate.delivery_id` (line 320) — **no priority lane**. |
| E6 | `docs/proposals/audit-contract-batch-aero-im.md` (B5-1 migration 0239_audit_governance_outbox.sql, B5-3 mapping table) | ✅ **Verified (as proposed)**. In-repo file is a 15-line gate summary (the 294-line full proposal is out-of-repo). Confirms: B5-1 = new migration `0239_audit_governance_outbox.sql` (status 0/1/2/3 normative, `class` message/room/admin, `priority`, `delivery_mode`, `CREATE OR REPLACE` redirect of enqueue/reconcile fns, new repo `audit_governance.rs`, P2 parity = `event_id` 1:1 assertion); B5-3 = `priority` column + `claim_due` `ORDER BY priority DESC`, local `message.moderated` → outbound `admin.content.flag`/`admin.moderation.action` mapping table, 500-backlog + 1-moderation drill + anti-starvation cap; gate note "T-11 与 moderation drill 本仓库可全绿". **The exact outbound token list is out-of-repo contract text — [PROPOSED]** (verified: `admin.content.flag` / `admin.moderation.action` appear nowhere in code or migrations). |
| E7 | (supplementary) `crates/aero-storage/src/ai_job.rs::priority_for` | ✅ **Verified**. Line 62: pure fn, **lower runs first**, `Moderate=10 < Answer=20 < Summarize=50 < Embed=100`; claim `ORDER BY priority ASC, scheduled_at ASC` (line 179); unit test `priority_lanes_order_user_facing_work_ahead_of_backfill` (line 421) and PG test `claim_prefers_higher_priority_lane_over_fifo` (line 564). This is the proven in-repo priority-lane pattern the mapping must clone. |
| E8 | (supplementary) worker test harness | ✅ **Verified**. `crates/aero-ai/src/worker/tests.rs` is **unit-only** (fake `UsageSink`; no PG). PG db_tests for this path live in aero-storage (`#[tokio::test] #[ignore = "requires live Postgres"]`, helpers `pool()` audit.rs:479, `fixture()` :489, `message_in_workspace()` :680). |
| E9 | (supplementary) other `message.moderated` producers | ✅ **Verified**. `crates/aero-storage/src/message/crud.rs::soft_delete_moderated` (line ~399, action at ~411) and `crates/aero-server/src/moderation_bot.rs` (line ~465) produce the **same** local token via a different entry point. The 0236 trigger is action-agnostic ⇒ a mapping **keyed on the local action token** automatically covers all producers; no caller-specific branching needed. |

## 2. Verified current state (the pipeline this direction modifies)

```
AiWorker::handle_moderate  (crates/aero-ai/src/worker/mod.rs:352)
  └─ BLOCK verdict, job.workspace_id Some
      └─ MessageRepo::soft_delete_outboxed_system  (crates/aero-storage/src/message/events.rs:267)
          [ one PG transaction ]
          ├─ soft-delete UPDATE (deleted_at, blocks='[]', embedding=NULL, version+1)
          ├─ AuditRepo::append_in_tx → audit_events row, action='message.moderated', actor=NULL  (audit.rs:127, :337)
          ├─ EventOutboxRepo::insert_room_event_in_tx → RoomEvent::Deleted (fan-out to room WS)
          └─ COMMIT
              └─ 0236 AFTER INSERT trigger aero_enqueue_snaplink_audit  (0236:68,129-133)
                  ├─ gate: snaplink_commercial_runtime.enabled  (disabled ⇒ NO outbox row)
                  ├─ binding lookup: raises 'commercial binding is unavailable' if missing ⇒ TX ABORTS (fail-closed)
                  └─ snaplink_delivery_outbox row: delivery_id='audit:'||id, idempotency_key=id,
                     payload.action = 'message.moderated' (unmapped), no status/priority/class (0235:161)
                      └─ claim: ORDER BY available_at, created_at, delivery_id  (snaplink_commercial.rs:302,320)
```

**Gaps the direction closes** (all verified): (1) no mapping local→outbound token — `payload.action` carries the local `message.moderated` verbatim; (2) v1 outbox has no `status` enum, no `class`, no `priority`; (3) claims order by `available_at/created_at` only — moderation rows cannot preempt backlog; (4) the row is written by a trigger after commit, not explicitly by the moderation finalize path (the same-tx property holds for the *audit row*; the *outbox* row is trigger-derived in-tx too — the v2 redirected enqueue must preserve this in-tx property while adding class/priority/mapped action).

## 3. Scope

**In scope (this direction, module `crates/aero-ai`)**:
- The local→outbound mapping table for the moderation action token and the call-site wiring in `AiWorker::handle_moderate` so the moderation finalize produces a **governance-outbox** row stamped `class='admin'`, moderation priority, mapped outbound action, `status 0`, in the **same transaction** as the soft delete + `message.moderated` audit row.
- Unit tests for the mapping; PG tests for the in-tx atomicity + mapping through the production seam; fixtures for the moderation row used by the B5-3 priority drill.

**Out of scope (parallel campaign directions / other modules — do not build here)**:
- `0239_audit_governance_outbox.sql` DDL + `audit_governance.rs` repo + enqueue/reconcile redirect + status machine 0→1→2/3 → **B5-1 (aero-storage)**.
- `priority` column semantics + `claim_due ORDER BY priority DESC` + anti-starvation cap → **B5-3 (aero-storage)**.
- Relay connector crate, 403→dead (T-11) delivery logic → **B5-2 (new crate)**.
- Scope provisioning `audit-provision-check` → **B5-4 (aero-cli)**.
- The `aero-server` `moderation_bot` entry point (`soft_delete_moderated`) — same token, same trigger, covered automatically by a token-keyed mapping; no changes required there.
- The usage relay (`snaplink_delivery_outbox` destination='usage') — untouched.

## 4. Requirements

### R1 — Action-token mapping table (pure, unit-testable)
A pure function/table (mirroring `ai_job.rs::priority_for` at line 62, "lower runs first", unit-tested without DB at ai_job.rs:421) that maps the **local audit action token** to the outbound governance tuple:
- `'message.moderated'` → `class = 'admin'` (B5-1 class enum), `priority` = moderation lane (must sort ahead of message/room backlog; exact lane value shared with B5-3's `claim_due`), outbound action = the **single contract token** resolved from the B5-3 mapping table — `admin.content.flag` or `admin.moderation.action` as the out-of-repo contract dictates (one constant, not a runtime choice; see §7 risk).
- **Unknown local tokens → no mapping (pass through / unmapped)** — the mapping must never raise or block non-moderation audit rows flowing through the shared trigger.
- Keyed on the **action token only** (not caller identity), so every `message.moderated` producer (AiWorker handle_moderate, `soft_delete_moderated`, moderation_bot) maps identically through the shared 0236 trigger/redirect (E9).

### R2 — Same-transaction governance enqueue on moderation finalize
When `handle_moderate` finalizes a BLOCK verdict (job has `workspace_id`, message exists), the governance-outbox row (v2, per 0239 DDL) must be written in the **same PG transaction** as the soft delete + `message.moderated` audit append (E1/E2/E3). No post-commit best-effort enqueue, no separate async step. The audit row keeps the local token; the mapping applies **at outbox enqueue time only**.

### R3 — Preserve the local audit trail contract
`audit_events.action` remains `'message.moderated'` with `actor_id NULL` (system) — the existing in-repo convention asserted by `moderation_delete_and_audit_commit_together` (audit.rs:808-850: exactly one row, `rows[0].actor_id == None`, detail carries `reason` + digest). The direction adds outbound mapping; it does **not** rename the local token.

### R4 — Class + priority stamping
The governance row for moderation finalize carries `class='admin'` and the moderation priority value such that B5-3's `claim_due` (priority-ordered) returns it before the backlog. The row is inserted with `status 0` (queued) per 0239 DDL.

### R5 — event_id 1:1 parity (P2)
The governance row's `event_id` equals the `audit_events.id` of the same transaction, exactly once (v1 already guarantees this shape: `delivery_id = 'audit:'||NEW.id`, `idempotency_key = NEW.id`, E4/E5). The v2 row must preserve 1:1 parity; no row without an audit row, no audit row without a governance row (for the moderation path under enforcement).

### R6 — Idempotency on replay
Replaying a **committed** moderation finalize (worker retry, at-least-once) must not create a second audit row or a second governance row: `soft_delete_outboxed_system` already returns `Ok(None)` when the message is deleted (E2) and the existing `UNIQUE(destination, idempotency_key)` (E5) is the schema-level guard. The new path must keep both properties.

### R7 — Fail-closed on enqueue failure
Any failure inside the transaction (audit append, governance enqueue, trigger, binding lookup) rolls back the soft delete + audit + outbox together — no "deleted but unqueued", no "audited but unqueued" state. `handle_moderate` must keep propagating `Err` → job retry → bounded DLQ (E1, worker/mod.rs:388-396), preserving the existing comment invariant: the retry replays the durably-finalized verdict without a second paid provider call.

### R8 — Preconditions (documented, not silently changed)
- Mapping/enqueue applies only when `job.workspace_id` is `Some` (the `workspace.map(|_| "message.moderated")` shape, E1) — workspace-less jobs remain un-audited.
- With commercial enforcement enabled but **no binding** for the workspace, the current 0236 trigger raises and aborts the moderation tx (verified fail-closed, E4). The B5-1 redirect owns whether v2 preserves this gate; this direction must not bypass it.
- With enforcement **disabled**, no outbox row is produced today (E4 gate). The B5-1 redirect owns the v2 gate semantics; A1 test setup must enable runtime + seed a binding to observe rows.

## 5. Acceptance checks (preserved from the direction, made testable)

All PG tests follow the existing aero-storage db_test harness: `#[tokio::test] #[ignore = "requires live Postgres"]`, throwaway migrated DB (`DATABASE_URL`), helpers `pool()`/`fixture()`/`message_in_workspace()` (audit.rs:479/489/680). A1 exercises the **production seam** `handle_moderate` calls (soft-delete + audit + mapped governance enqueue), not a bespoke test path.

### A1 — One AI-moderated message → one audit row + one governance row, same tx (rollback removes both)
**Setup**: throwaway DB, full migration chain; workspace + participant + message; `snaplink_commercial_runtime.enabled = true`; enabled binding for the workspace (else the trigger raises, E4).
**Act**: run the moderation finalize path exactly as `handle_moderate` does (BLOCK verdict with workspace, audit action `message.moderated`, system actor).
**Assert**:
1. Message is soft-deleted (`deleted_at` set, `blocks` empty).
2. Exactly **1** `audit_events` row: `action='message.moderated'`, `target=message id`, `actor_id IS NULL` (mirrors audit.rs:808 test).
3. Exactly **1** governance-outbox row: `class='admin'`, `priority` = moderation lane, outbound `action` = the mapping constant (`admin.content.flag` or `admin.moderation.action` per contract), `status=0`, `event_id` = the audit row's id (1:1 parity, R5).
4. **Rollback half** (mirrors `moderation_failed_audit_rolls_back_delete`, audit.rs:852): same operation inside an explicit tx, `ROLLBACK` → `COUNT(audit_events)=0` AND `COUNT(governance outbox)=0` (both removed together).
5. **Replay half**: second identical call returns the no-op (`Ok(None)`) → still exactly 1 audit row + 1 governance row (R6).

### A2 — Moderation-priority drill (B5-3): 500 backlog + 1 moderation → moderation claimed first; backlog still progresses
**Dependency**: B5-3 `claim_due` priority ordering (this direction only supplies the stamped row + fixture).
**Setup**: seed 500 backlog governance rows (message/room class, default priority, staggered `available_at` so they are due) + 1 moderation-class row **produced through the R1 mapping** (same class/priority the moderation path stamps).
**Assert**:
1. First `claim_due` call returns the moderation row (priority-ordered ahead of all 500 backlog rows regardless of enqueue order).
2. Anti-starvation: subsequent claim batches keep draining the backlog (after K batches, backlog rows are claimed/delivered — no row is permanently starved; cap semantics per B5-3 DDL).

### A3 — Status machine 0→1→2 / →3 per 0239 DDL + event_id 1:1 parity (P2)
**Dependency**: B5-1 status machine + repo.
**Assert** (over the row produced by the moderation path):
1. Enqueued row starts `status 0`; claim flips to `status 1` with claim token/lease; settle flips to `status 2` (delivered); dead path flips to `status 3` (terminal, excluded from future claims).
2. **P2 parity**: for every moderation-path row, `event_id` = `audit_events.id` of the same transaction — 1:1, no duplicates, no orphans (asserted with a set-difference query over the two tables).

### A4 — T-11 fail-closed: relay 403 → dead (terminal) exactly once
**Dependency**: B5-2 connector delivery semantics + B5-1 dead terminal.
**Assert**: simulate the relay receiving 403 on first delivery of a moderation-path row → row transitions to `status 3` (dead) exactly once; no further delivery attempt is made (attempts do not increment past the terminal transition); dead rows are excluded from claim/lag. Kept as an integration acceptance — the aero-ai contribution is only that its rows are normally claimable/deliverable (A3 covers the mechanics).

## 6. Test placement

| Test | Location | Harness |
|---|---|---|
| R1 mapping unit test (token → class/priority/outbound action; unknown token passes through) | `crates/aero-ai/src/worker/tests.rs` (or sibling module) | pure unit, no DB — clone of `priority_lanes_order_user_facing_work_ahead_of_backfill` (ai_job.rs:421) |
| A1 same-tx atomicity + mapping + replay | aero-storage db_tests (audit.rs test module, next to `moderation_delete_and_audit_commit_together`) | PG, `#[ignore]` + `DATABASE_URL` |
| A2 priority drill (fixture via R1 mapping) | aero-storage db_tests (with B5-3 claim tests) | PG |
| A3 status machine + parity | aero-storage db_tests (B5-1 repo tests) | PG |
| A4 T-11 403→dead | connector integration (B5-2) + `scripts/test-integration.sh` drill | integration |

## 7. Risks / [PROPOSED] items

- **Exact outbound token** (`admin.content.flag` vs `admin.moderation.action`): out-of-repo contract text (E6). The spec pins "one constant resolved from the B5-3 mapping table"; A1 compares against that same constant, so the test is self-consistent whichever token the contract dictates. Locking the token is a B5-3/contract decision, not this direction's.
- **Cutover semantics** between the 0236-trigger v1 row and the v2 governance row (both tables live during transition; B5-1 `CREATE OR REPLACE` redirect owns whether moderation rows appear in v1, v2, or both) — this direction requires the moderation path to produce exactly the v2 row per A1; duplicate-enqueue guard is `UNIQUE` + B5-1 redirect.
- **Runtime/binding gates** (E4): A1 requires enforcement enabled + binding seeded; the B5-1 redirect decides whether the v2 path keeps the same gates — if it drops them, A1's setup is simpler but the fail-closed binding behavior (currently aborting the moderation tx) changes and must be re-verified.
- **`message.moderated` also produced outside aero-ai** (E9): token-keyed mapping covers them automatically; any per-producer differences (e.g., actor, detail shape) must not leak into the mapping branch.

## 8. Sequencing

1. This direction: R1 mapping + handle_moderate wiring + unit tests (no new migration — B5-1 owns 0239).
2. B5-1 (aero-storage): 0239 DDL + `audit_governance.rs` + redirect — unlocks A1's v2-row assertions and A3.
3. B5-3 (aero-storage): priority claim + anti-starvation — unlocks A2.
4. B5-2 (connector): 403→dead — unlocks A4.
A1's PG test can be written against the v1 outbox shape first (asserting the mapped fields once 0239 lands); A2–A4 are gated on their dependencies.

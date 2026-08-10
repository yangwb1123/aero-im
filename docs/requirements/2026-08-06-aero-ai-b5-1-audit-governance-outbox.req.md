# Requirements Spec — B5-1: 审计治理 outbox（status 0/1/2/3 + class/priority），moderation 优先级映射随行

- **Module**: `crates/aero-ai/src`
- **Direction**: "B5-1: 复用已验证的 in-tx outbox 模式实现审计治理 outbox（status 0/1/2/3 + class/priority），并顺带打通 B5-3 的 moderation 优先级映射"
- **Source analysis**: `docs/auto/analyses/crates-aero-ai-src-1bbf99ce.json` (direction #1; value 9 / risk-reduction 8 / effort 7 / confidence 8)
- **Campaign**: `aero-im-b5-outbox-relay` (`docs/campaigns/campaign-aero-im-b5.yaml`); in-repo contract anchor `docs/proposals/audit-contract-batch-aero-im.md`; gate anchor `docs/campaigns/implementation-gate.md` (G6 row: "37/37、T-11、moderation 优先级")
- **Status**: Requirements (verified evidence below)
- **Verification date**: 2026-08-06 (line numbers are as-of-verification anchors; drift is possible — the **file/symbol** is the stable grep anchor per AGENTS.md §0)

## 1. Evidence verification (every cited symbol checked against the repo)

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-storage/src/audit.rs:113,127` (`append` / `append_in_tx`, 1049 行锚点) | ✅ **Verified**. `append` at 113, `append_in_tx` at 127 (transaction-scoped, generic `append_on` executor at 138), file is exactly **1049 lines** (the direction's "1049 行锚点" = `wc -l`, matching the audit.rs-file-size pin in the proposal). db_test precedents: `fixture` at 489, `moderation_delete_and_audit_commit_together` at 814, `moderation_failed_audit_rolls_back_delete` at 874 — all `#[ignore = "requires live Postgres"]`. |
| E2 | `crates/aero-storage/src/message/events.rs` (`soft_delete_locked_outboxed_in_tx` — 删+审计+outbox 单事务) | ✅ **Verified**. Wrapper `soft_delete_outboxed_system` at 267; the in-tx core `soft_delete_locked_outboxed_in_tx` at 299 (`pub(crate)`). One tx: `lock_message_in_tx` → soft-delete UPDATE → `cleanup_visible_associations_in_tx` → `enqueue_unreferenced_blobs_in_tx` → `AuditRepo::append_in_tx` with the **`audit_action` parameter** (the `message.moderated` 锚点) → `EventOutboxRepo::insert_room_event_in_tx` (RoomEvent::Deleted) → commit. Returns `Ok(None)` when already deleted (idempotent no-op → no audit/outbox row on replay). |
| E3 | `crates/aero-im-core/src/service/outbox.rs` (`dispatch_event_outbox_batch` claim/lease 泵) | ✅ **Verified**. `dispatch_event_outbox_batch` at 27: `EventOutboxRepo::claim_due(now, OUTBOX_LEASE=30s, limit)` → per-row `finish_claimed_outbox` → publish or `mark_failed` (durable backoff re-park, batch not failed); `dispatch_event_outbox_id` at 61 (immediate post-commit relay, `false` harmless). This is the claim/lease 泵 pattern the 0239 `audit_governance.rs` pump (B5-1 storage slice) must clone. |
| E4 | `migrations/0236_snaplink_governance_reconciliation.sql:129-131` (AFTER INSERT 触发器 → snaplink_delivery_outbox) | ✅ **Verified**. `aero_enqueue_snaplink_audit()` at 68 (`CREATE OR REPLACE FUNCTION ... RETURNS trigger`); trigger `audit_events_snaplink_delivery` `AFTER INSERT ON audit_events FOR EACH ROW` at 129-133. Behaviors: (a) gated on `snaplink_commercial_runtime.enabled` — disabled ⇒ `RETURN NEW`, **no outbox row**; (b) `aero_snaplink_binding_for_workspace` RAISEs when no enabled binding ⇒ **aborts the whole audit transaction** (fail-closed); (c) `delivery_id='audit:'\|\|NEW.id`, `idempotency_key=NEW.id`, `payload.action = NEW.action` verbatim — **no mapping, no class/priority stamping today**. The same file already contains a `CREATE OR REPLACE` reconcile function (`aero_reconcile_snaplink_usage`, line ~146) — the "同款 reconcile 函数可改" precedent for the 0239 redirect. |
| E5 | `migrations/0235_snaplink_commercial_control_plane.sql` (snaplink_delivery_outbox DDL，无 status/priority — 缺口所在) | ✅ **Verified**. DDL at 161: `delivery_id/destination/workspace_id/tenant_id/client_id/source_system/idempotency_key/payload/occurred_at/available_at/attempts/claim_token/lease_expires_at/delivered_at/last_error/created_at` + `UNIQUE (destination, idempotency_key)` + `CHECK (jsonb_typeof(payload)='object')` + claim-state CHECK. **Confirmed: no status enum, no class, no priority, no delivery_mode.** Due index at 187: `(available_at, created_at, delivery_id) WHERE delivered_at IS NULL` — FIFO-only. |
| E6 | `crates/aero-storage/src/ai_job.rs:57-68` (`priority_for`: Moderate=10 < Embed=100 车道优先级先例) | ✅ **Verified**. Doc 57-61, fn at 62-68: pure, "**lower runs first**", `Moderate=10 < Answer=20 < Summarize=50 < Embed=100`. Claim at 174-180: `ORDER BY priority ASC, scheduled_at ASC ... FOR UPDATE SKIP LOCKED`. Lane unit test `priority_lanes_order_user_facing_work_ahead_of_backfill` at 421-429. This is the proven in-repo lane model the governance outbox priority must clone. |
| E7 | `crates/aero-storage/src/snaplink_commercial.rs:77,302` (`SnaplinkDeliveryClaim` / `claim_due`) | ✅ **Verified**. `SnaplinkDeliveryClaim` struct at 77; `claim_due` at 302 with `ORDER BY candidate.available_at, candidate.created_at, candidate.delivery_id` at 320 — **no priority lane** (the B5-3 gap; this direction only stamps the priority column value). |
| E8 | `crates/aero-im-core/src/service/messages.rs:632` (`moderate_delete` → `soft_delete_outboxed_system` audit_action="message.moderated") | ✅ **Verified**. `moderate_delete` at 611; call at 632: `soft_delete_outboxed_system(message_id, workspace, None, workspace.map(\|_\| "message.moderated"), detail, ParticipantId::nil(), traceparent)` — second producer of the same local token (alongside the AiWorker). |
| E9 | (supplementary) `crates/aero-ai/src/worker/mod.rs::handle_moderate` — the module's own moderation finalize seam | ✅ **Verified**. `handle_moderate` at 352; BLOCK verdict → `self.svc.messages().soft_delete_outboxed_system(id, workspace, None, workspace.map(\|_\| "message.moderated"), detail, ParticipantId::nil(), None)` at 373-377; audit action **only when `job.workspace_id` is `Some`**; `Err` → warn + propagate → retry → bounded DLQ (`MAX_ATTEMPTS=5` at 68, `is_over_attempt_cap` ~500), comment invariant: provider verdict durably finalized under the job's stable usage context ⇒ retry replays **without a second paid call**. `svc.messages() -> &MessageRepo` at `service/service_impl.rs:106`; `run_loop` at 577 / `run_one` at 774 / `Semaphore` at 596,716. `crates/aero-ai/Cargo.toml` depends on `aero-storage` (⇒ **dependency direction: aero-ai → aero-storage; storage can never import from aero-ai**). |
| E10 | (supplementary) `crates/aero-ai/src/budget.rs` — the moderation lane's enforcement side | ✅ **Verified**. `CostBudget` at 98, `KeyedCostBudget` at 215 (per-ws 120 / global 120 over 60s gates used by `moderation_bot`). The aero-ai lane model (Moderate preempts backfill) is enforced here + in E6's claim ordering. |
| E11 | (supplementary) "仓库内无现成 L1 聚合" — NotifyBatch 是扇出不是聚合 | ✅ **Verified (as claimed)**. `RoomEvent::NotifyBatch` (`crates/aero-common/src/model/event.rs:86`) is a **fan-out** payload (explicit recipient list for hub delivery) — not an aggregation window. No outbox/event aggregation exists anywhere in aero-ai or aero-common (`aggregat` grep: only metrics counter aggregation and notification `AggregateReply` — a notification-shape flag, not outbox merging). **L1 aggregation mechanics are [PROPOSED] with no in-repo precedent.** |
| E12 | (supplementary) gate/harness: T-37/37 + 30 db_tests + `make migrate-smoke` + `scripts/test-integration.sh` | ✅ **Verified (as far as in-repo possible)**. `docs/campaigns/implementation-gate.md:63` (B5-1 row): "30 个忽略测试 CI 全绿（37/37）；P2 parity"; :78 gate G6: "37/37、T-11、moderation 优先级". `Makefile:120` `migrate-smoke` = "replay every migration on a throwaway DB". `scripts/test-integration.sh` runs named per-test throwaway-DB drills (pattern at 143-165, `run_migrated_integration`) plus the full `cargo test --workspace --lib --locked -- --ignored --test-threads=1` at 271. **The exact 37-test list and the 30-test set are out-of-repo contract text — [PROPOSED] counts** (in-repo total of `#[ignore = "requires live Postgres"]` tests is ~400 across crates; the contract's "30" is its named subset, not the repo total). |

## 2. Verified current state (the pipeline this direction modifies)

```
AiWorker::handle_moderate  (crates/aero-ai/src/worker/mod.rs:352)      ← THIS MODULE's seam
  └─ BLOCK verdict, job.workspace_id Some
      └─ MessageRepo::soft_delete_outboxed_system  (crates/aero-storage/src/message/events.rs:267)
          [ one PG transaction ]
          ├─ soft-delete UPDATE (deleted_at, blocks='[]', embedding=NULL, version+1)
          ├─ AuditRepo::append_in_tx → audit_events row, action='message.moderated', actor=NULL  (audit.rs:127)
          ├─ EventOutboxRepo::insert_room_event_in_tx → RoomEvent::Deleted (room WS fan-out)
          └─ COMMIT
              └─ 0236 AFTER INSERT trigger aero_enqueue_snaplink_audit  (0236:68,129-133)
                  ├─ gate: snaplink_commercial_runtime.enabled  (disabled ⇒ NO outbox row)
                  ├─ binding lookup: raises 'commercial binding is unavailable' if missing ⇒ TX ABORTS (fail-closed)
                  └─ snaplink_delivery_outbox row: delivery_id='audit:'||id, idempotency_key=id,
                     payload.action = 'message.moderated' (unmapped), no status/class/priority (0235:161)
                      └─ claim: ORDER BY available_at, created_at, delivery_id  (snaplink_commercial.rs:302,320)
```

**Gaps the direction closes** (all verified): (1) v1 outbox has **no status enum** (no 0/1/2/3 normative state, no dead terminal), **no class, no priority, no delivery_mode** (E5); (2) `claim_due` orders by `available_at/created_at` only — a moderation row cannot preempt backlog (E7); (3) `payload.action` carries the **local** token `message.moderated` verbatim — no `admin.content.flag` / `admin.moderation.action` mapping (E4); (4) **no L1 aggregation exists in-repo** for high-volume `message.*` events — `NotifyBatch` is fan-out, not aggregation (E11, [PROPOSED]); (5) the proven patterns to clone are all verified: `append_in_tx` (E1), `soft_delete_locked_outboxed_in_tx` (E2), `dispatch_event_outbox_batch` claim/lease 泵 (E3), `priority_for` lane model (E6).

## 3. Scope

**In scope (this direction, module `crates/aero-ai/src`)**:
- The **moderation governance-lane mapping primitive** (local token `message.moderated` → class/priority/outbound action), cloned from `priority_for`'s proven lane pattern (E6), unit-tested in this crate.
- The **call-site contract** of `AiWorker::handle_moderate` (E9): the moderation finalize path keeps producing the audit + governance outbox rows **in the same transaction** through the shared storage write point, with the local token preserved — nothing in aero-ai moves to a post-commit enqueue.
- The **lane-consistency requirement**: the governance moderation lane value must preempt message/room backlog exactly as `priority_for(Moderate)=10` preempts `Embed=100`, so the B5-3 `claim_due` (priority DESC) returns moderation rows first.
- The **moderation-row drill fixture** (stamped `class='admin'` + moderation priority + mapped outbound action + `status 0`) used by the B5-3 500-backlog drill and by this direction's parity test.
- **L1-aggregation bypass**: admin-class moderation rows must stay 1:1 (`event_id` = `audit_events.id`, P2 parity) — never merged by the proposed L1 window for high-volume `message.*`.
- Unit + PG tests for all of the above through the **production seam** (the exact `handle_moderate` call shape).

**Out of scope (parallel campaign slices / other modules — do not build here)**:
- `0239_audit_governance_outbox.sql` DDL (status 0/1/2/3 normative, `class` message/room/admin, `priority`, `delivery_mode`) + `CREATE OR REPLACE` enqueue/reconcile redirect + `audit_governance.rs` repo + claim pump → **B5-1 aero-storage/src + migrations slices** (implementation-gate.md:63 row 1).
- In-tx audit/outbox coverage for the `message.create/edit` and `room.*`/`admin.*` write points in `aero-server`/`aero-storage` (their P2-parity half of A2) → **parallel B5-1 slices for those modules**.
- `priority` column claim ordering (`claim_due ORDER BY priority DESC`) + anti-starvation cap → **B5-3 (aero-storage)**.
- Relay connector crate + 403→dead (T-11) delivery → **B5-2 (new crate)**; provisioning seam `audit-provision-check` → **B5-4 (aero-cli)**.
- L1 aggregation **implementation** (window/bookkeeping) → [PROPOSED], owned by the B5-1 storage/migrations slice; this direction only pins the bypass + counting assertion.
- The usage relay (`destination='usage'`) — untouched (proposal: "usage relay 原地不动").

## 4. Requirements

### R1 — Moderation governance-lane mapping primitive (pure, unit-testable)
A pure function/table in this crate (new sibling module, e.g. `governance.rs`, mirroring `ai_job.rs::priority_for` at E6: pure, no DB, unit-tested lane ordering) that maps the **local audit action token** to the governance tuple:
- `'message.moderated'` → `class = 'admin'` (B5-1 class enum), `priority` = moderation lane value (must sort ahead of message/room backlog; **the same value B5-3's `claim_due ORDER BY priority DESC` treats as top lane** — one constant, pinned by unit test), outbound action = the **single contract token** resolved from the B5-3 mapping table — `admin.content.flag` or `admin.moderation.action` as the out-of-repo contract dictates (one constant, not a runtime choice; see §7).
- **Unknown local tokens → no mapping (pass through / unmapped)** — the mapping must never raise or block non-moderation audit rows flowing through the shared trigger/redirect.
- Keyed on the **action token only** (not caller identity), so every `message.moderated` producer (`AiWorker::handle_moderate` E9, `moderate_delete` E8, `moderation_bot`) maps identically.
- **Dependency-direction note (verified, E9)**: aero-ai depends on aero-storage, so storage cannot import this primitive. The B5-1 storage/0239 slice applies the token→tuple translation at the enqueue point (E2/E4); this direction's module is the **authoritative moderation-lane constant + fixture** the cross-slice tests pin against (A3 fixture, A2 parity).

### R2 — Same-transaction governance enqueue through the production seam
When `handle_moderate` finalizes a BLOCK verdict (job has `workspace_id`, message exists), the governance-outbox row (v2, per 0239 DDL) must be written in the **same PG transaction** as the soft delete + `message.moderated` audit append (E1/E2/E9). No post-commit best-effort enqueue, no separate async step in aero-ai. The aero-ai call shape stays exactly as verified at worker/mod.rs:373-377 (local token passed as `audit_action`); the 0239 redirected enqueue does the class/priority/action stamping in-tx (storage slice).

### R3 — Preserve the local audit trail contract
`audit_events.action` remains `'message.moderated'` with `actor_id NULL` (system) — the in-repo convention asserted by `moderation_delete_and_audit_commit_together` (audit.rs:814). The direction adds outbound mapping; it does **not** rename the local token.

### R4 — Lane consistency with the ai_jobs lane model
The governance moderation lane must preempt message/room backlog exactly like `priority_for(Moderate)=10` preempts `Embed=100` (E6: lower runs first; Moderate is the only user-facing gate ahead of the flood). Unit test pins: moderation lane < message/room default lane; cross-slice assertion in A3 (the stamped row is claimed first). The claim-side ordering itself is B5-3's.

### R5 — Moderation rows bypass L1 aggregation (P2 parity preserved)
The proposed L1 aggregation (high-volume `message.*` only) must never merge admin-class moderation rows: every moderation governance row keeps `event_id` = `audit_events.id` of the same transaction, exactly once (v1 already guarantees this shape: `delivery_id='audit:'||id`, `idempotency_key=id`, E4/E5). No moderation row without an audit row, no audit row without a moderation governance row (under enforcement). This direction pins the **bypass**; the aggregation window itself is [PROPOSED] (E11).

### R6 — Drill fixture (moderation row through the R1 mapping)
A repository seam (test fixture, and the storage-side stamping path it exercises) producing a governance row stamped `class='admin'`, moderation lane priority, mapped outbound action, `status 0` — used by: (a) A2 parity test in this direction; (b) the B5-3 500-backlog + 1-moderation drill (which asserts this row is claimed first). The fixture must go through the **production seam** (`handle_moderate`'s exact call shape), not a bespoke insert.

### R7 — Idempotency on replay preserved
Replaying a **committed** moderation finalize (worker retry, at-least-once) must not create a second audit row or a second governance row: `soft_delete_outboxed_system` returns `Ok(None)` when the message is already deleted (E2) and the v1 `UNIQUE (destination, idempotency_key)` (E5) is the schema-level guard. The 0239 redirect must keep both properties (A2 replay half).

### R8 — Fail-closed propagation preserved
Any failure inside the transaction (audit append, redirected enqueue, trigger, binding lookup) rolls back soft delete + audit + outbox together — no "deleted but unqueued", no "audited but unqueued". `handle_moderate` must keep propagating `Err` → job retry → bounded DLQ (E9, worker/mod.rs:388-396) with the existing comment invariant: the retry replays the durably-finalized verdict **without a second paid provider call**.

## 5. Acceptance checks (preserved from the direction, made testable)

PG tests follow the existing db_test harness (`#[tokio::test] #[ignore = "requires live Postgres"]`, throwaway migrated DB, helpers `pool()`/`fixture()`/`message_in_workspace()` — audit.rs:489/814 precedents). A2/A3 exercise the **production seam** `handle_moderate` calls (E9), not a bespoke path.

### A1 — T-37/37: 0239 replays on throwaway DB; the audit-governance db_tests are green and pinned
**Assert**:
1. `make migrate-smoke` exits 0 — every migration including **0239** replays on a brand-new throwaway DB (fresh-deploy chain check; migration count = `ls migrations/*.sql | wc -l`, never hardcoded).
2. The **30 contract audit-governance db_tests** (contract list, [PROPOSED] count — in-repo minimum: the new `audit_governance.rs` db_tests + this direction's A2 parity test + R1 unit tests) are green under `bash scripts/test-integration.sh` (which runs the full `--ignored` set at line 271).
3. **Pinned**: named per-test entries added to `scripts/test-integration.sh` in the existing `run_migrated_integration` pattern (lines 143-165) for `audit_governance::*` and the moderation-finalize parity test, each on its own throwaway migrated DB with `--test-threads=1`.

### A2 — P2 parity: each message.create/edit/delete + room.*/admin.* operation → exactly 1 audit_events row + 1 governance outbox row, same tx (audit failure → whole op rolls back)
**Setup** (per op family): throwaway DB, full migration chain; workspace + actor + target; `snaplink_commercial_runtime.enabled = true`; enabled binding for the workspace (else the trigger raises, E4).
**Assert** (moderation-path family — this direction's half; the `message.create/edit` and `room.*`/`admin.*` write-point halves are owned by the parallel B5-1 slices, same assertions):
1. Running the operation inside an explicit tx then committing yields **exactly 1** `audit_events` row AND **exactly 1** governance-outbox row, with `event_id` = the audit row's id (set-difference query: no outbox `event_id` without an audit row and vice versa over the test window — 1:1, no dups, no orphans).
2. **Rollback half** (mirrors `moderation_failed_audit_rolls_back_delete`, audit.rs:874): same operation inside an explicit tx, `ROLLBACK` → `COUNT(audit_events)=0` AND `COUNT(governance outbox)=0` (both removed together).
3. For the moderation family specifically: governance row carries `class='admin'`, moderation lane priority, mapped outbound action (R1 constant), `status 0`; `audit_events.action='message.moderated'`, `actor_id IS NULL`.
4. **Replay half** (R7): second identical call returns `Ok(None)` → still exactly 1 audit row + 1 governance row.

### A3 — Moderation-priority drill (B5-3): 500 backlog + 1 message.moderated → admin.content.flag row claimed first (claim_due ORDER BY priority DESC), anti-starvation cap
**Dependency**: B5-3 priority claim ordering (this direction supplies the stamped row + R6 fixture).
**Setup**: seed 500 backlog governance rows (message/room class, default priority, staggered `available_at` so all are due) + 1 moderation row **produced through the R1 mapping / R6 fixture**.
**Assert**:
1. The first `claim_due` call returns the moderation row — priority-ordered ahead of all 500 backlog rows regardless of enqueue order.
2. Anti-starvation: after K batches the backlog keeps draining (backlog rows are claimed/delivered; no row is permanently starved; cap semantics per B5-3 DDL).

### A4 — L1 aggregation (proposed): N high-volume message.* events in a window → outbound aggregate rows ≤ 1; per-event 1:1 counting assertion (P2 parity not regressed)
**Dependency**: L1 aggregation implementation ([PROPOSED], E11 — no in-repo precedent; `NotifyBatch` is fan-out, not aggregation).
**Assert**:
1. Inject N high-volume `message.*` audit events in one aggregation window → outbound governance rows ≤ 1, and the aggregate row's count field == N (per-event 1:1 counting).
2. **Bypass**: admin-class moderation rows are never aggregated — each keeps its own row with `event_id` 1:1 parity (R5), so A2's set-difference assertion stays green after L1 lands.
3. P2 parity does not regress: the A2 parity db_test runs unchanged and green with L1 enabled.

## 6. Test placement

| Test | Location | Harness |
|---|---|---|
| R1 mapping unit test (token → class/priority/outbound action; unknown token passes through; lane ordering: moderation < message/room default) | `crates/aero-ai/src/governance.rs` (new module) or sibling | pure unit, no DB — clone of `priority_lanes_order_user_facing_work_ahead_of_backfill` (ai_job.rs:421) |
| R5 bypass + A4 counting assertion (admin rows never merged) | unit test on the R1 mapping (classification) + storage db_test with L1 ([PROPOSED]) | unit + PG `#[ignore]` |
| A2 parity + rollback + replay (moderation family, production seam) | aero-storage db_tests next to `moderation_delete_and_audit_commit_together` (audit.rs:814) — written against the v1 outbox shape first, upgraded to v2 fields once 0239 lands | PG `#[ignore]` |
| R6 drill fixture | shared with B5-3 claim tests (aero-storage) | PG |
| A3 drill (500 backlog + 1 moderation, first claim, anti-starvation) | aero-storage db_tests (with B5-3 claim ordering) | PG |
| A1 migrate-smoke + pinning | `make migrate-smoke` + `scripts/test-integration.sh` named entries | shell |

## 7. Risks / [PROPOSED] items

- **Exact outbound token** (`admin.content.flag` vs `admin.moderation.action`) and the **"30 个 db_tests" / "37/37" lists**: out-of-repo contract text (proposal doc: "8 处不可验证/[PROPOSED] 明确列出…37/37 测试清单…不在本仓库"; E12). Tests are self-consistent (A2 compares against the R1 constant; A1 pins whatever the contract's 30-test set is, in-repo minimum guaranteed by the named `run_migrated_integration` entries). Locking the token list is a B5-3/contract decision, not this direction's.
- **Cutover v1→v2**: both tables live during transition; the 0239 `CREATE OR REPLACE` redirect owns whether moderation rows appear in v1, v2, or both — this direction requires exactly the v2 row per A2; duplicate-enqueue guard = `UNIQUE` + redirect (E5).
- **Runtime/binding gates** (E4): A2/A3 setup requires enforcement enabled + binding seeded; the 0239 redirect decides whether the v2 path keeps the same gates — if it drops them, the currently-aborting fail-closed binding behavior changes and must be re-verified.
- **Dependency direction** (E9): the R1 primitive lives in aero-ai (test/fixture authority) while the 0239 stamping runs in storage/SQL — the two must agree on the lane value; pinned by A3 (fixture row claimed first with the B5-3 ordering) and the R1 unit test. If the contract later moves the mapping into SQL only, the aero-ai constant remains the drill fixture's source of truth.
- **L1 aggregation mechanics fully [PROPOSED]** (E11): A4's "≤ 1 aggregate row" and "1:1 counting" are the pinning assertions; the window/EWMA/counter bookkeeping is the storage slice's proposed design, and A2's parity test is the regression guard.
- **`message.moderated` also produced outside aero-ai** (E8, `moderation_bot`/`soft_delete_moderated`): token-keyed mapping covers them automatically; per-producer differences (actor, detail shape) must not leak into the mapping branch (R1).

## 8. Sequencing

1. **This direction (aero-ai)**: R1 mapping module + unit tests + R6 fixture + A2 parity db_test (written against the v1 outbox shape first, asserting the mapped fields once 0239 lands).
2. **B5-1 storage/migrations slices**: 0239 DDL + `audit_governance.rs` + enqueue/reconcile `CREATE OR REPLACE` redirect — unlocks A1's 0239 replay and A2's v2-row assertions.
3. **B5-3 (aero-storage)**: `claim_due ORDER BY priority DESC` + anti-starvation cap — unlocks A3.
4. **L1 aggregation ([PROPOSED])**: after A2 parity is green, so the bypass assertion (A4.2) has a fixed baseline.
5. **B5-2 (connector) + B5-4 (aero-cli)**: relay semantics and provisioning — not in this direction's acceptance list; A1's 37/37 gate G6 references them at the campaign level.

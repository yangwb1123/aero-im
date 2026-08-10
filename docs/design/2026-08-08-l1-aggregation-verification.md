# Verification — zero-DDL claim of `2026-08-08-l1-aggregation-wire-contract-leaf.design.md`

- **Design under test**: `docs/design/2026-08-08-l1-aggregation-wire-contract-leaf.design.md` (§0 TL;DR, §1 ledger, §2.5 R8 tests, §5 steps, §6 acceptance mapping)
- **DDL under test**: `migrations/0239_audit_governance_outbox.sql`, `0240_audit_governance_due_prio_idx.sql`, `0241_governance_reconcile.sql`
- **Method**: source re-read of `crates/aero-audit-connector/src/pg.rs` / `relay.rs` / `outbox.rs` / `client.rs`, `crates/aero-common/src/{ids.rs, model/audit.rs}`, `crates/aero-ai/src/governance.rs`, `crates/aero-storage/src/audit_governance.rs`, `scripts/truth-check-lib.sh`, `scripts/b5-pin.sh`; **live PG proof** on a throwaway DB (`aero_window_verify`, pgvector/pg17, full 241-migration replay via built `aero-cli migrate`, then dropped) executing the exact window row, the exact claim/fence SQL shapes, the AC4 mixed-priority scenario, and the AC5 requeue+reclaim shape; plus the existing `#[ignore]` PG tests run against that DB.
- **Date**: 2026-08-08. Verdict: **zero-DDL claim CONFIRMED** (live, not just textual); 3 corrections/nuances found (C1–C3), none breaking the design's substance.

## 1. Window-shaped row passes every CHECK and both partial indexes — CONFIRMED (live)

Inserted `(event_id = ULID-shaped uuid, status 0, class 'message', priority 10, delivery_mode 'push', payload = 19-field AuditWindowPayload-shaped jsonb object)` into the migrated 0239 table: **accepted**. Walk-through of every constraint:

| Constraint (0239) | Window row | Result |
|---|---|---|
| `event_id UUID PRIMARY KEY` | `AuditWindowId::to_uuid()` — ULID-shaped UUID | ✅ (any UUID; ULID form has no special status) |
| `CHECK (status IN (0,1,2,3))` | 0 | ✅ |
| `CHECK (class IN ('admin','message','room'))` | 'message' | ✅ |
| `CHECK (priority > 0)` SMALLINT | 10 | ✅ |
| `CHECK (delivery_mode IN ('push'))` | 'push' | ✅ |
| `CHECK (jsonb_typeof(payload) = 'object')` | `to_value(AuditWindowPayload)` → object | ✅ (19 keys, no constraint on keys) |
| `attempts >= 0` / `claim_state` (token⇔lease) | 0 / both NULL | ✅ |
| defaults (`status 0`, `class 'message'`, `priority 10`, `delivery_mode 'push'`, `available_at`/`created_at` `clock_timestamp()`) | read back | ✅ |
| negative probes (status 4, class 'administrator', priority 0, mode 'pull', non-object payload, token-without-lease) | all rejected | ✅ still red — the window row loosened nothing |

**Both partial indexes**: predicates are `WHERE status = ANY (ARRAY[0,1])`; the status-0 window row satisfies both. `EXPLAIN` of the exact claim query shows the planner resolves the due set via **`audit_governance_due_prio_idx`** (Bitmap Index Scan) matching `(priority DESC, available_at, created_at, event_id)` — no full Sort of the due set, LIMIT pushdown intact. Running the exact `claim_due` CTE against the window row claims it (status 1, token minted, attempts 1, payload `jsonb_typeof` = object) — so a window row is not just representable, it is **claimable by the current connector with zero code change** (incl. its `ClaimRow → Claim` wrap of the window UUID into `AuditId` — a type-level lie the design explicitly defers to the connector slice, §7).

## 2. Dead=3 terminal vs fence SQL; payload never rewritten — CONFIRMED (live)

Fence SQL (pg.rs:148/:169/:204/:233) and the claim filter all use `status IN (0, 1)`; Dead=3 is outside every re-entry path:

- mark_dead (exact fence: `claim_token + attempts + status IN (0,1) + lease_expires_at > clock_timestamp()`) on the claimed window row → 1 row, status 3.
- After Dead: claim filter excludes it, `settle` fence refuses (status IN (0,1)), `requeue` fence refuses (**no resurrection**), `mark_dead` again refuses, and both partial-index predicates drop it. 0241's reconciler `NOT EXISTS` sees the row and skips it (dead rows never resurrected — verified in §4).
- **payload immutability**: `claim_due`, `settle`, `requeue`, `mark_dead` UPDATE SET lists touch only `status/claim_token/lease_expires_at/attempts/available_at/delivered_at/last_error` — never `payload`. Live byte-compare `payload::text` before/after requeue and before/after mark_dead: **identical**, `idempotency_key` untouched. Consistent with the design's R5 (idempotency key derived from window identity, never rewritten by the claim machinery).

## 3. 0241 `ON CONFLICT (event_id) DO NOTHING` + triggers — scrutinized, 2 findings

- **No trigger on `audit_governance_outbox` itself**: the only trigger in 0239/0240/0241 is `audit_events_governance_enqueue` (AFTER INSERT on `audit_events`; sibling v1 trigger `audit_events_snaplink_delivery` is also on `audit_events`). Direct INSERT of a window row fires nothing; no v1 side effects. ✅
- **Window rows don't block the reconciler**: 0241 scans `audit_events` (`action='message.moderated'` + enabled binding) with `NOT EXISTS (outbox WHERE event_id = audit.id)`. A window row's `event_id` is a window UUID, never an `audit_events.id` (random ULIDs) → live proof: reconciler ran with 6 window rows present, backfill unaffected, window rows untouched (status/class/priority intact). ✅
- **C1 — residual exposure demonstrated at SQL level (design F2, honest but worth pinning)**: the Rust type-disjointness (`AuditWindowId` ≠ `AuditId`) has **no DB-level twin** — both are plain UUIDs in one PK. Live demo: a window row keyed with a real audit event id wins the `ON CONFLICT (event_id) DO NOTHING` and the 0239 trigger's moderation enqueue is **silently dropped** (same for 0241's `NOT EXISTS` backfill — a permanent gap). The design's F2 mitigation (compile-time) is the only guard; the future window producer must never derive `AuditWindowId` from an audit id. No schema change can express this today — zero-DDL holds, the exposure is inherent and documented.
- **Doc-level nuance**: 0241's comment "parity converges to `COUNT(outbox) == COUNT(audit)` for the mapped subset" becomes technically inaccurate once window rows exist (they inflate outbox count). No code asserts that parity (relay drill parity is delivered-set == seeded-set, self-contained), so no breakage — but the B5-1 mechanism should not rely on that sentence.

## 4. AC4 ordering vs real ORDER BY; do the planned PG tests exercise fenced ops? — CONFIRMED, 2 corrections

**AC4 shape (admin 100 vs window/backlog 10)** — verified against the real claim CTE (`pg.rs:117`: `ORDER BY candidate.priority DESC, candidate.available_at, candidate.created_at, candidate.event_id … FOR UPDATE SKIP LOCKED LIMIT $1`) and the matching 0240 index (EXPLAIN proof above). Live run of the exact scenario (6 window-shaped rows interleaved among 40 priority-10 rows with earlier `available_at` + inverted `created_at`; 10 admin priority-100 with later `available_at`; claim 25): claimed set = **{all 10 admin} ∪ {15 earliest of the 40 priority-10 incl. window rows}** — window rows ride the backlog lane, never preempt moderation. Matches the existing `mixed_priority_claim_orders_moderation_first_then_fifo` (:536) contract exactly.

**AC5 shape (fenced ops + lease expiry)** — live run with real sleeps: claim (1 s lease floor per `clamped_lease`, attempts 1, token A) → fenced requeue while lease live (returns 1 row; row not re-claimable during the re-park) → sleep past re-park → reclaim (attempts 2, fresh token B) → payload byte-identical → settle with old token A fails, token B settles. ✅ The two planned tests mirror the two existing template tests (:441/:536) which I ran green against the throwaway DB.

Two corrections for the design's acceptance mapping:

- **C2 — `--test-threads=1` is mandatory for the `--ignored` acceptance commands (§5 step 6, §6 AC4/AC5)**: proven — all 3 existing connector PG tests **fail when run in parallel** (shared `audit_governance_outbox` + per-test `TRUNCATE` race; 0/3 pass) and **3/3 pass with `--test-threads=1`**; same for storage (`audit_governance::` 6/7 fail parallel, 7/7 pass serial). The repo's own harness already encodes this (`scripts/test-integration.sh:170/193/623`). Adding the two planned tests to `pg.rs` makes the parallel failure mode worse, not better. Design's commands must read `cargo test … -- --ignored --test-threads=1`.
- **C3 — AC5 wording nuance**: after requeue the lease is cleared (`lease_expires_at = NULL`), so the "睡过 1s 租约" sleep actually covers the **backoff re-park** (`audit_backoff(1) = 1 s`, `relay.rs:56-63`) rather than the lease; the 1.2 s margin (template-test precedent) covers both since both are 1 s. Not a defect — a description fix. Also note AC4 by design exercises only claim ordering (no fence/lease ops) — that's the correct division; fence+lease is AC5's job.

## 5. Design ledger spot-checks (source re-read)

All substance claims confirmed: ORDER BY :117, claim subquery :129, `WHERE event_id = $1` :148/:169/:204/:233 (the design's "mark_dead 双查询" label is off — :204 is requeue's site, :233 mark_dead's single query; :169 is settle's second UPDATE — citation-level only), `STATUS_*` :34-37, tests :441/:536; `audit.rs` OutboxStatus :20 (design said :22), AuditClass :122 (design said :111), `GOVERNANCE_CLASS_MESSAGE` :166, `AuditClaimPayload` :268, 550 lines (D1 exact), `#[allow(clippy::too_many_arguments)]` :293 (design's "audit.rs:238 先例" is a citation of the allow's own comment; no allow at :238); `define_id!` full macro surface (new/from_ulid/as_ulid/to_uuid/from_uuid/nil/Default/Display=ULID base32/FromStr/From<Ulid>/From<id> for Uuid/serde transparent + Ord derives); `AuditId` at ids.rs:122; `client.rs:197` Idempotency-Key header; `Claim { event_id: AuditId, … }` outbox.rs; governance.rs `100`/`10`/`:90 is_admin_class`; `delivery_mode` = exactly 7 hits, all in the storage drill (lines 609–712); zero `window` outbox types in aero-common; zero collisions for `AuditWindowId/AuditWindowPayload/DeliveryMode/DELIVERY_MODE_PUSH/AGGREGATED_MESSAGE_ACTION` (rg = 0); rule 3d mechanism in truth-check-lib.sh (:126/:131-141/:166-171/:173-175/:194-196/:301-309) and b5-pin slots `audit_governance::` :37 / `t11-fail-closed` :40 / `moderation-priority-drill` :41 — 37-slot invariant and rule 3e plan structurally sound.

## 6. Verdict

- **Zero-DDL claim: PROVEN live** — the window row is representable, CHECK-legal, index-eligible, claimable, fence-able, and Dead-terminal-consistent on today's 0239/0240/0241 schema. The design's R1–R8 leaf-only scope holds.
- **Corrections to apply**: C1 (document the SQL-level key-collision exposure as an invariant on the future window producer — design F2 already covers it, add a drill-time guard later), C2 (**add `--test-threads=1` to §5 step 6 and §6 AC4/AC5 acceptance commands** — required, proven), C3 (AC5 sleep rationale wording: re-park, not lease).
- No changes to 0239/0240/0241, connector SQL, `OutboxRepo`/`Claim`, or the fence semantics are warranted by this verification.

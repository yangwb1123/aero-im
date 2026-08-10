# Sign-off — perf_reviewer durable-state mitigation vs 0241 resurrect semantics (pre-landing)

- **Mitigation under review**: delivered-row sweep `DELETE FROM audit_governance_outbox WHERE status = 2 AND class <> 'admin' AND delivered_at < now() - interval 'N days'` + v2 pending-backlog gauge, as proposed in `2026-08-08-aero-im-core-b5-1-seam-hot-path-cost.assessment.md` §3.3
- **Method**: every claim re-derived from current source — `migrations/0241_governance_reconcile.sql`, `migrations/0239_audit_governance_outbox.sql`, `crates/aero-audit-connector/src/pg.rs` + `relay.rs`, `crates/aero-common/src/model/audit.rs`, `crates/aero-server/src/metrics.rs` / `bin/boot/metrics_tasks.rs`, `crates/aero-server/src/snaplink_commercial/runtime.rs`, `crates/aero-storage/src/audit_governance.rs`
- **Status**: sign-off — all five questions verified; no correctness blockers. Two operational notes and one documentation-level carry-over item (non-blocking).

---

## 1. Sweep predicate provably never deletes (a) dead rows or (b) admin-class rows — ✅

**Resurrect semantics of 0241 (re-derived from source)**:
- `aero_reconcile_governance_audit` scans `audit_events` with `WHERE audit.action = 'message.moderated'` (token-keyed, R-D2) and `NOT EXISTS (SELECT 1 FROM audit_governance_outbox WHERE event_id = audit.id)`, then INSERTs hardcoded `class 'admin' / priority 100 / status 0`. Deleting an outbox row while its audit row survives ⇒ NOT EXISTS passes ⇒ re-insert as status 0 ⇒ re-claim ⇒ re-deliver (403-outage DLQ posture reversed).
- **Resurrectable set == admin class, exactly**:
  - The reconciler's scan is moderation-token-only; the only producers of outbox rows for moderation audit rows are the 0239 trigger (class 'admin') and 0241 itself (class 'admin') — never the seam (delete seam skips `message.moderated`; the 5 seam tokens are message.send/edited/deleted, room.created, room.member.add — none match the scan). `event_id` = `audit_events.id` 1:1, so a message/room-class row can never shadow a moderation audit row. Hence `class <> 'admin'` ⟺ "not resurrectable by 0241".

**Predicate analysis**:
- (a) Dead rows: predicate requires `status = 2`; status 3 never matches. Correct and necessary — the in-repo pin `governance_reconcile_backfills_disabled_window` (audit_governance.rs) asserts "dead row never resurrected"; the sweep must not contradict it. The resurrection rationale is strictly about admin-class dead rows; for message/room dead rows 0241 cannot resurrect them (scan miss), so excluding them is conservative-but-harmless and preserves the T-11 DLQ visibility.
- (b) Admin rows: `class <> 'admin'` excludes them unconditionally — the conservative choice. (Admin rows would only be resurrectable after the audit row's partition is dropped; sweeping them past the audit-retention window would be safe but is not required.)
- Edge: a hypothetical status=2 row with NULL `delivered_at` (only possible via manual insert) also survives — `NULL < cutoff` is NULL, filtered. Defense-in-depth: the sink dedups by `Idempotency-Key = event_id` anyway.

## 2. No race with the connector's claim/ack cycle — ✅ by construction

- All four connector transitions constrain on `status IN (0, 1)` (+ claim-token + lease fences): claim (0/1→1, `FOR UPDATE SKIP LOCKED`), settle (0/1→2, sets `delivered_at` in the same UPDATE inside the fenced tx), requeue (0/1→0), mark_dead (0/1→3). **No statement transitions 2→anything**; no production statement reads status-2 rows (reconcile's NOT EXISTS is per-audit-id and moderation-only).
- Therefore in-flight (0/1) and sweepable (2) are **disjoint row sets by construction**: the sweep's DELETE can never touch a row the connector is claiming/settling/requeueing/deading, and no connector statement can touch a row the sweep deletes. No lost update, no same-row lock contention, no deadlock cycle (row sets disjoint ⇒ no lock cycle possible).
- MVCC: the sweep only ever observes committed status=2 (delivered_at set atomically at settle commit); a concurrently settling row is either status 1 (predicate fails) or status 2 with a fresh delivered_at (fails any N-day cutoff). No in-flight delivery can be swept.
- Reconcile-before-claim (relay.rs `dispatch_batch`) does not interact: swept rows are non-admin, outside the reconciler's scan.

## 3. v2 backlog gauge covers the monotonic backlog — ✅

- Verified **no v2 gauge exists today**: `metrics_tasks.rs` samplers cover DB pool / index sizes / pg health / AI DLQ (`aero_ai_usage_outbox_backlog`) / NATS backlogs; the only outbox backlog gauge is v1's `aero_snaplink_delivery_outbox_backlog` (snaplink_commercial/runtime.rs:17, `pending_count()`). The aero-storage `audit_governance.rs` counts are test helpers, not metrics.
- Proposed predicate `WHERE status IN (0, 1)` matches the claim filter's status set exactly (pg.rs claim reads `status IN (0,1)`) — the v2 analog of v1's "undelivered" (`delivered_at IS NULL`). Under relay under-capacity the producer is unconditional, requeued rows stay in (0,1), so the pending population grows monotonically; dead rows exit into a bounded, separately observable population (aero-eng provision status distribution + `last_error`). Correct tripwire shape; mirror the two existing gauges (`set_gauge` + help registration).

## 4. Cold-start hole + late-binding `source_system` stamp inconsistency — ✅ sign-off (accurate), with one carry-over item

Both items as documented by audit_integrity (Q2 residual 1, Q3 note (b)) verified accurate against source:
- **Cold-start**: 0241 is moderation-token-only and the seam fires only on new writes ⇒ pre-landing audit rows (every existing `message.deleted` row, etc.) never receive v2 outbox rows; v1 lane unaffected (its reconciler backfills all actions); a v2 consumer asserting `COUNT(outbox)==COUNT(audit)` from t=0 sees a bounded historical hole. Accurate.
- **Late-binding**: `source_system` is stamped into the immutable envelope payload at INSERT time; the `"aero-im"` fallback materializes only when no binding row exists at write time (enforcement OFF ∧ unbound; under enforcement ON the 0236 trigger RAISEs, so no row survives); a later binding re-stamps nothing — old rows keep `"aero-im"` forever while new rows carry the real stamp. v1's reconciler (0236:230 `aero_reconcile_snaplink_audit`) joins the binding at backfill time, so the **same audit row** can carry different stamps in the two lanes — a real cross-lane divergence, accurately described.

**Carry-over item (non-blocking, documentation-level)**: the design doc (`2026-08-08-aero-im-core-b5-1-in-tx-audit-outbox-producer-seam.design.md`) currently carries neither one-liner — they live only in the review artifact. audit_integrity's own recommendation was "the design should state this explicitly". Carry both into the design before landing: cold-start in §2/§3, late-binding in §1.3 next to the fallback.

## 5. Zero-migration compatibility with the sweep as a code-only change — ✅

- Sweep = DELETE over existing columns (`status`/`class`/`delivered_at`, all in 0239); gauge = SELECT over `status`. No schema change, no new index ⇒ **zero migrations holds**, and the design's zero-migration acceptance ("0239/0240/0241 一字不改") is unaffected. No connector/ImService/harness change either; the sweep is a timer + repo method in the seam layer.
- **Operational note 1 (pre-0239 boot)**: the sweep must degrade warn-and-skip on `relation does not exist`, mirroring the relay's F13 pattern ("Until 0239 lands, booting the relay degrades to logged claim errors, not a crash") — a panic here would be the only new failure surface.
- **Operational note 2 (sweep cost)**: the predicate is not index-covered — 0240's partial index covers `status IN (0,1)` only, so the DELETE seq-scans the delivered population per run. Same cost class as the accepted 0241 reconcile scan and the existing retention sweeps (best-effort warn, `MissedTickBehavior::Skip`); steady-state delivered population is bounded by the sweep itself once it runs. Choose N so steady state stays small. If it ever becomes hot, a partial index `WHERE status = 2` would be a *future* migration — deliberately outside the zero-migration promise; as stated, the claim holds.

---

## Verdict

**Sign-off granted.** The sweep predicate is safe by construction against 0241's resurrect semantics (dead rows excluded by `status=2`; admin rows excluded by `class <> 'admin'`, and the admin class is exactly the resurrectable set); the sweep and the connector's claim/ack operate on provably disjoint row sets; the recommended gauge is the correct pending-population tripwire and none exists today; the cold-start and late-binding items are accurately documented (one design-doc carry-over recommended); zero-migration compatibility holds for the code-only sweep. No correctness blockers; land with the two ops notes and the one doc carry-over.

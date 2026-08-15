# SRE Review — R-D2: `message.deleted` → Governance Outbox (Rust writer)

> **Revision under review:** HEAD `e48a3c9` (design commit; change not landed). SRE review of the *proposed* change against the live tree, focused on the API backend's external frontend/proxy boundary: the in-process audit relay, its outbound IdP/events endpoints, and the `audit_governance_outbox` queue.
> **Note:** `prompts/README.md` does not exist in this tree (verified: no `prompts/` directory, no README under any `*prompt*` path); the review follows the prompt's own required-output structure.
> All claims below verified against live source unless marked [designed].

---

## 1. Service / dependency map and operational assumptions

```
                        ┌──────────────────── aero-server (gateway, WS, HLS, RTMP) ────────────────────┐
  browser SPA ──WSS──▶  │  ImService::delete_message ──▶ soft_delete_locked_outboxed_in_tx (PG tx)     │
                        │    [R-D2] audit INSERT → SELECT created_at → outbox INSERT  (SAME tx,        │
                        │    fail-closed: writer error ⇒ delete 500)                                   │
                        │  audit relay (in-process, presence-gated AERO_AUDIT_TOKEN_ENDPOINT):          │
                        │    claim_due (SKIP LOCKED, DB-clock) ──▶ AuditClient ──▶ IdP token endpoint   │
                        │                                   └─────▶ audit-governance events sink (HTTPS)│
                        └───────────┬──────────────┬───────────────┬───────────────────────────────────┘
                                    ▼              ▼               ▼
                                  Postgres 17    Redis 7         NATS JetStream      blob (LocalFs/S3)
                        audit_events (daily      presence/       im.room.*,          attachments
                        partitions, DROP@365d)   roster          live.stream.*
                        audit_governance_outbox  ── status 0/1/2/3, event_id 1:1 with audit_events.id
```

**Operational assumptions (verified):**

1. **Relay is in-process with the API server** (`bin/main.rs:245-264`), not a sidecar. Its liveness = server liveness; a server crash parks claimed rows until lease expiry (30s default) reclaims them — at-least-once, no loss.
2. **The audit boundary is deliberately fail-soft**: the sink/IdP is *not* a readiness dependency (`probe_deps` = PG/Redis/NATS/blob only); rows queue durably during sink outages. The *writer* side of R-D2 is deliberately fail-closed: the outbox INSERT failure aborts the user's delete.
3. **Queue health signals are currently absent**: no gauge for outbox depth/dead rows exists anywhere in the tree (see Finding 1). Detection today = `docs/runbooks/audit-relay-zero-delivery.md` §4 manual greps + the `aero-cli audit-provision-check` CLI.
4. **Single clock domain** for the relay (DB `clock_timestamp()`); the audit row's `created_at` is app-minted (`audit.rs:152` `OffsetDateTime::now_utc()`) and bound explicitly — the R-D2 re-select reads back that same in-tx value (no new skew class).
5. **DR consistency is free**: audit row + outbox row + soft-delete + room event commit atomically; a PITR restore to before the commit removes the delete itself; to after, keeps all four. No torn state possible.
6. **Retention asymmetry is pre-existing and R-D2 amplifies it**: `audit_events` partitions are DROPPED at 365d; `audit_governance_outbox` has **no sweep** (verified: `boot/retention.rs` sweep list of 20+ tables does not include it; 0246: "never delete while relay runs" applies to pending rows) — R-D2 makes every user delete a permanent row containing a 120-char content digest.
7. **Version-skew window is a silent gap**: old binary during rolling deploy writes audit rows but no outbox rows; the 0241 reconciler is `message.moderated`-only and the relay only claims existing rows → those deletes never enter the feed, and no parity check can see it (drills count *mapped* rows, not coverage).

---

## 2. Readiness table

| Signal | Dependency | Failure behavior | Alert today | Runbook |
|---|---|---|---|---|
| `/health/live` | none (process up) | 200 always | — | — |
| `/health/ready` | PG / Redis / NATS / blob (+ draining precedence) | 503 not_ready; 503 draining during shutdown | platform LB probe | standard |
| `/health` | same + snaplink_commercial | 200 degraded body | — | — |
| Message delete (user path) | **PG + `audit_governance_outbox` writability [R-D2]** | Writer error ⇒ delete 500 (fail-closed, new) | **none** — no error-rate alert; `MESSAGES_DELETED_TOTAL` + `MESSAGE_PROCESSING_DURATION_SECONDS{op="delete"}` exist but no alert rules reference them | none (new) |
| Audit relay claim/settle | PG (`audit_governance_outbox`) | SQL error ⇒ warn per tick, retry; rows stay durable | none | audit-relay-zero-delivery.md §6 (symptom table) |
| Audit relay delivery | IdP token endpoint + events sink + (JWKS) | Transient ⇒ bounded backoff (1→300s), **never dead**; 403 ⇒ immediate dead; permanent (422/409/ReceiptMismatch/PayloadGuard) ⇒ dead at ≤1 retry | **none** — runbook §3 alert rules reference series that do not exist in the tree | audit-relay-zero-delivery.md (but §3 is aspirational — see Finding 1) |
| Dead rows (status 3) | — | Terminal, never resurrected by claim/reconcile | **none** — no gauge; only manual `last_error` grep / CLI | §5 recovery SQL (token-endpoint-403 class only); ReceiptMismatch/PayloadGuard deads have **no documented recovery** |
| Reconcile (0241) | PG function `aero_reconcile_governance_audit` | **Function error aborts the entire claim batch** (`relay.rs: dispatch_batch` propagates `?`) — all lanes stall until next tick | warn log only | none |
| Retention sweep | PG | warn + retry next tick | none | AGENTS §2 |
| Outbox table growth | — | Unbounded (no sweep, no size gauge) | none | none |

---

## 3. Findings (severity-sorted)

### FINDING 1 — HIGH: the governance-feed monitoring surface documented in the runbook does not exist in the tree

**Evidence (all verified):**
- `docs/runbooks/audit-relay-zero-delivery.md` §3 defines alert rules on `aero_audit_outbox_transient_requeue`, `aero_audit_outbox_dead`, `aero_audit_token_rejections_total`, `aero_audit_delivery_outcomes_total`, citing `crates/aero-audit-connector/src/metrics.rs` — **that file does not exist**; `rg` across `crates/` for these names returns zero code hits (only the runbook + requirement docs).
- The connector crate has **zero** metrics references (`relay.rs`, `client.rs` contain no counters/gauges; only `warn!` logs).
- `metrics_tasks.rs` samplers cover DB pool / index size / PG stats / AI DLQ / NATS backlog / failed-pairs DLQ — **no audit outbox sampler**.
- The gap is *known and tracked but unlanded*: `docs/requirements/2026-08-09-aero-ai-governance-outbox-health-gauges.req.md` (status: Requirements; `audit_outbox_health` module absent from `crates/aero-ai/src/`) and `2026-08-08-aero-audit-connector-b5-4-fail-closed-operational.req.md`.

**Production impact:** R-D2 adds the highest-volume, gate-free producer (every user delete) to a queue with no depth, freshness, or dead-row signal. Sink outage / IdP misconfig / JWKS breakage = silent zero-delivery: rows requeue forever (transient posture) with warn logs only; the CRITICAL alert the runbook promises cannot fire because its series don't exist. The `audit_governance_failed_pairs` gauge is **not** a proxy — R-D2 is fail-closed and has no DLQ.

**Remediation:** Land the health-gauges requirement in the same release train (5 gauges: `aero_audit_outbox_{pending,claimed,delivered,dead,oldest_pending_secs}`, 15–30s sampler, read-only) plus counters in the client classify points; or, minimally, ship the runbook with an explicit "alert rules pending implementation" banner and an ops query for the watch window. Also add `audit_relay: enabled/disabled` to `/health` output (relay state is currently invisible to the probe).

**Recovery validation:** sink-down drill via `StubSink` (AC-2b harness): assert gauges rise while sink 500s, fall to zero after recovery; dead-row gauge > 0 within 2 poll ticks of a PayloadGuard dead.

### FINDING 2 — HIGH: no retention for `audit_governance_outbox`; R-D2 makes it a permanent content store

**Evidence:** `boot/retention.rs` sweep list (verified above) omits the table; `sweep_audit` + `sweep_audit_partitions` drop source `audit_events` at 365d; 0239 has no retention column contract; `due_idx` partial index covers only status IN (0,1) — a future status-2 sweep would full-scan.

**Production impact:** (a) GDPR/data-retention asymmetry — the outbox row's `payload.digest` (first 120 chars of message text) + actor/target/room survive forever, outliving the source trail by design; (b) unbounded growth — R-D2 rows are ~1–2 KB each and every user delete adds one; bloat, backup size, restore time all grow monotonically; (c) the `WHERE status IN (0,1)` claim query stays indexed, so *claim* latency is protected, but nothing else is.

**Remediation:** status-2/3 sweep on `delivered_at` age (safe: claim only touches 0/1; sink-side idempotency is keyed on `event_id` and the sink governs its own dedup window — confirm the sink's dedup retention before choosing the cutoff), or daily partitioning mirroring `audit_events`. Must be a tracked follow-up with an owner/deadline, not a deferred footnote; until then add a table-size gauge/ops query.

**Recovery validation:** seed status-2 rows past cutoff → sweep deletes only 2/3, never 0/1; relay claims + settles after a sweep without touching swept rows.

### FINDING 3 — MEDIUM: fail-closed writer couples a core user feature to a secondary table's writability

**Evidence:** [designed] writer error propagates ⇒ delete tx aborts ⇒ user gets 500 (mirrors trigger-abort semantics — deliberate, documented in §2.3/§3.3 of the design; the opposite of the auth slice's `append_pair_in_tx_fail_open`). Today `delete_message` succeeds independently of this table.

**Production impact:** table dropped/corrupt/out-of-space/CHECK-violated ⇒ **all** message deletes fail with no circuit breaker and no alert (Finding 1). This is a new single-point-of-failure for a core feature, bought for compliance completeness. Acceptable *if* the fail-closed rationale is an explicit product decision (it is) — but it must be paired with the Finding-1 gauges and an error-rate alert, otherwise the first symptom is a pager storm of delete-500 reports.

**Remediation:** alert on `MESSAGES_DELETED_TOTAL` flat/falling while delete-error rate > 0 (instrument writer errors with a counter); document the coupling in the runbook; the writer runs inside the same tx as the audit append, so the failure surface is exactly {INSERT fails, partitioned-probe fails} — both DB-side.

**Recovery validation:** Finding-1 gauge drill; plus a fail-closed drill: `DROP TABLE audit_governance_outbox` on a throwaway DB → `soft_delete_outboxed_authorized` → `Err` + `deleted_at IS NULL` + zero outbox rows (this is exactly QA Finding 1's proposed test — endorse and require it).

### FINDING 4 — MEDIUM: per-delete latency — the writer adds a partitioned-parent probe on the user path

**Evidence:** `append_message_delete_in_tx` [designed] executes `SELECT created_at FROM audit_events WHERE id = $1`. `audit_events` is daily-partitioned with PK `(id, created_at)`; `id` alone cannot prune partitions ⇒ ~365 index probes per delete (DB architect L1 concurs). Meanwhile `AuditRepo::append_on` (`audit.rs:152-166`) **already mints and binds `created_at` app-side in the same tx** — the value is in hand.

**Production impact:** +2 statements per user-facing delete (probe + INSERT), the probe being O(#partitions). At high delete rates this is measurable p95 on the hot path; with 365 daily partitions it's ~365 index lookups *per delete*, forever, unless partitions are dropped (they are, at 365d — so the cost is bounded by the retention window, but still material).

**Remediation:** change `AuditRepo::append_in_tx` to `RETURNING id, created_at` (same crate, same commit, zero behavior change) and pass `created_at` into the writer — eliminates the probe entirely, keeps the single-clock discipline (same value, same tx). Measure before/after with the existing `MESSAGE_PROCESSING_DURATION_SECONDS{op="delete"}` histogram.

**Recovery validation:** delete-path latency drill — N deletes at p95/p99 before vs after; assert p95 delta within budget (suggest < 5 ms mean delta).

### FINDING 5 — MEDIUM: rolling-deploy version skew silently drops deletes from the governance feed (QA Finding 2, SRE-validated)

**Evidence:** additive code-only change (§3.8); during the skew window old-binary deletes write the audit row but no outbox row; reconciler is `message.moderated`-only (0241); relay claims only existing rows; parity drills count *mapped* rows so SUM parity holds while coverage silently diverges.

**Production impact:** governance consumers see a permanent gap for the deploy window's deletes — invisible to every existing check. Bounded (deploy window) and strictly better than today's zero rows, but irrecoverable without backfill.

**Remediation:** (a) post-deploy one-shot backfill `INSERT … SELECT` from `audit_events` where `action='message.deleted'` and id not in outbox (standalone SQL ops step, 0239 shape, `ON CONFLICT DO NOTHING`); (b) AC-2a negative control: orphaned delete audit row → parity unchanged, exit 0 (pins "no silent fabrication"); (c) an ops query `audit_events.message.deleted ∖ outbox` for the skew window.

**Recovery validation:** run the AC-2a drill with seeded orphan; then the backfill SQL; assert 1:1 restored and drill still PASS.

### FINDING 6 — MEDIUM: reconcile failure stalls every lane (amplified blast radius)

**Evidence:** `relay.rs dispatch_batch`: `self.repo.reconcile(...).await?` — a failure of `aero_reconcile_governance_audit` (0241) aborts the whole batch: no claims of *any* class (admin/100 moderation rows included) until next tick; warn-logged only. Pre-existing; R-D2 increases the blast radius (deletes now ride the same loop).

**Remediation:** decouple — if reconcile fails, warn and proceed to `claim_due` (0241 backfill is idempotent best-effort; a stall of the *recovery* path shouldn't stall the *primary* path); or gauge+alert reconcile errors.

**Recovery validation:** drill: break the function (drop it on throwaway DB) → claims continue (post-fix) or visibly stall (pre-fix, documented); restore function → backlog drains.

### FINDING 7 — LOW: dead-row recovery is manual SQL with no documented path for the new permanent classes

**Evidence:** status-3 rows are terminal (`claim_due` filters 0/1; 0241 `NOT EXISTS` never resurrects); the only recovery SQL in the runbook targets token-endpoint-403 rows; R-D2's AC-2b pins ReceiptMismatch/PayloadGuard deads ≤1 retry — deterministic payload corruption, correctly not requeued, but with no runbook entry ("what does a ReceiptMismatch dead row mean and who fixes it" — answer: an envelope-bug investigation, not a requeue).

**Remediation:** one runbook paragraph: dead rows with `%ReceiptMismatch%`/`%PayloadGuard%` = producer bug (compare envelope to `governance_envelope`); with `%HTTP 403%` = identity/provisioning; with `%unprovisioned%` = scope. Recovery = fix cause; requeue only for the 403 false-kill class. Requires Finding-1's dead gauge to be actionable.

### FINDING 8 — LOW: retention-sweep deletes remain outside the feed (semantic gap to document)

**Evidence:** `workspace/sweep.rs::sweep_expired_messages` soft-deletes via raw `UPDATE` with **no audit row** (verified) — R-D2's choke point is not on that path. The automated mass-delete path (the most compliance-relevant deletes) stays invisible to the governance feed. Design explicitly defers.

**Remediation:** document the feed's semantics ("class='message' 1:1 rows = user/system-initiated deletes; retention-swept deletes are NOT mapped") in the runbook/design, and track the in-tx audit append as a follow-up. A compliance consumer must not read "no delete events" as "no deletes happened."

### FINDING 9 — Info: envelope timestamp spelling inconsistency across producers

**Evidence:** DB architect M2 — Rust `time` rows spell `…Z` (trimmed fraction) while trigger rows spell `…+00:00` (PG `to_jsonb`); AC-3's Value-equality re-serialization assertion will fail as specced and must assert semantically (auth.rs pattern). Ops relevance: sink-side parsers comparing `occurred_at` across producer types must normalize; note it in the wire-contract doc.

---

## 4. Failure drills

| Drill | Procedure (throwaway migrated DB, `--test-threads=1`, self-isolated fixtures) | Pass criteria |
|---|---|---|
| **Sink/IdP outage** | AC-2b harness with `StubSink` 500s/closed token endpoint (T-11 pattern); N delete-lane rows | rows stay status 0/1, attempts grow per claim, **never dead** (transient posture); after recovery all settle to status 2 with `{event_id}` set-parity; gauges (post-Finding-1) reflect depth/freshness |
| **Saturation** | Seed 40 low-lane + 190 admin rows (existing `sustained_mixed_lanes` fixture), batch 100 | per-round split 95/5 (D-CAP floor protects the delete lane from moderation storms); no starvation; backlog drains within bounds |
| **Bad rollout (version skew)** | AC-2a delete leg + orphaned delete audit row (QA Finding 2 negative control) | SUM parity holds, no fabrication, exit 0; then backfill SQL restores 1:1 |
| **Stale state (dead rows)** | AC-2b negatives: `payload.event_id ≠ PK` → ReceiptMismatch; `source_system` mismatch → PayloadGuard | dead at ≤1 retry (status 3, attempts ≤2), `last_error` records permanent class; dead gauge > 0 within 2 ticks; §5 runbook SQL recovers only the 403 class |
| **Stale state (crash-after-claim)** | Existing `expired_lease_is_reclaimed_with_fresh_token` + `crash_after_delivery` precedent | lease expiry → reclaim with fresh token; sink dedups via `Idempotency-Key: event_id`; at-least-once, exactly-once at sink |
| **Restore** | PITR to T; verify audit+outbox+delete consistency (same-tx atomicity); run `l1-aggregation-drill` post-restore | SUM parity PASS; no half-state rows; feed resumes from status 0/1 set |

## 5. Launch blockers, rollback triggers, monitoring gaps, residual risks

### Launch blockers
1. **Finding 1** — the governance feed ships with no continuous signal; the runbook's alert rules reference nonexistent series. Land the health-gauges requirement (or explicitly re-scope the runbook + provide ops queries) before R-D2 is the feed's highest-volume producer. This is the only true blocker.
2. **Finding 2** — retention for status 2/3 rows must be a tracked follow-up with an owner/deadline, not deferred indefinitely: R-D2 converts it from a low-volume table into a permanent content store (GDPR + growth).

### Rollback triggers (revert = revert commit; schema unchanged; rows already written stay claimable — old relay is class-agnostic, no drain needed)
- Delete p95 latency regression beyond budget (`MESSAGE_PROCESSING_DURATION_SECONDS{op="delete"}` before/after, Finding 4).
- Delete error rate > 0 sustained (fail-closed writer, Finding 3) — any writer error class.
- Dead rows with `%ReceiptMismatch%`/`%PayloadGuard%` (envelope bug, Finding 7).
- AC-1/AC-2a/AC-3 drills red; `audit_governance::` count < 39.
- Old-binary rollback is safe mid-skew: deletes revert to audit-only (Finding 5 window reopens — rerun the backfill after the final state settles).

### Monitoring gaps (all actionable)
- Outbox depth by status + oldest-pending age (the 5 planned gauges, unlanded).
- Delivery outcome counters + token-rejection reason counters (runbook series, nonexistent).
- Reconcile failure rate (Finding 6).
- Delete error rate / writer failure counter (Finding 3).
- Table size/growth gauge (Finding 2).
- 1:1 coverage drift query (`audit_events.message.deleted` ∖ outbox) for skew windows (Finding 5).
- `audit_relay` state in `/health` output.

### Residual risks
- **No SLO/RTO/RPO documented** for the governance feed anywhere in the tree. Recommended (measurable today): 99% of outbox rows delivered within 5 min of commit when sink healthy (from `created_at → delivered_at`; alert on oldest-pending age > 15 min). RPO for the feed = PG backup RPO (atomicity makes DR cutover consistent — the strong property).
- Deleted-content digest retained forever pending Finding 2.
- O(#partitions) probe per delete pending Finding 4 fix.
- Feed semantics exclude retention-sweep deletes (Finding 8) — document, don't surprise compliance consumers.
- Envelope timestamp spelling differs across producers (Finding 9) — normalize at the sink.

Review complete — saved to `docs/pi-batch/feature-reviews/distributed_systems_engineer.md`. Note: `prompts/README.md` does not exist (same finding as prior reviewers); the review was grounded in direct source inspection of every recall surface.

## Summary

### State map (recap of the load-bearing topology)
The recall's distributed spine is **PG transaction → event_outbox (durable queue) → relay (SKIP LOCKED lease 30s + attempts fencing) → NATS `im.room.{id}` (Msg-Id dedup, seq stamped once) → durable consumers → per-process Hub (bounded mpsc) → WS**. Seq is Redis `INCR` cluster-wide with documented fail-open (unstamped on outage). I verified all the fencing machinery is sound: `mark_published`/`mark_failed` fenced on `attempts`, `assign_seq_if_absent` uses `COALESCE` so re-claimed rows re-publish with the same seq, lease expiry leaves no permanent processing state.

### Key findings (beyond the prior reviewers' set)

**DS-1 (HIGH) — the two known defects interact to make things worse than reported.** The unfurl system-edit resurrection (`edit_locked_outboxed_in_tx`'s UPDATE WHERE lacks `recalled_at IS NULL`) doesn't just restore content — if the recall's fast-path dispatch had failed, the relay's version-based suppression heuristic (`materialize_outbox_payload` Recalled arm, `version > original_version → None`) then **silently discards the pending Recalled outbox row**. Result: no client ever renders the placeholder, entire room, no trace beyond audit. The suppression heuristic that correctly kills stale edits now kills the recall itself. Fix: storage-level fence (`AND recalled_at IS NULL`) so every editor is fenced, not just the user path.

**DS-2 (HIGH) — replay convergence gap confirmed from source**: `changes_since` keys on `GREATEST(edited_at, deleted_at)`, recall never bumps `edited_at`, and the client cursor advances before the reply — so offline clients permanently retain original content. The client (`applyChange`) already handles `m.recalled_at`; only the server predicate + migration 0125's expression index need extending (0239).

**DS-4 (MEDIUM, new) — rolling-deploy hazard**: poison policy is ack-drop with durable cursor advance, so a mixed fleet silently loses `recalled` events per old node — with no metric distinguishing it. Needs documented deploy ordering, not a code change.

**DS-3/5/6/7** — partition-cutover backfill omits recall columns (verified lineage 0148→0174, 0238 never reissues the function); cross-message room order is not commit-ordered (fast path overtakes backoff'd rows — self-healing create-materialization verified); clock assumptions bounded by fencing (node rollback → latency only); 409 conflation makes recall retry-ambiguous.

### Bottom line
The happy path, fencing, and at-least-once machinery are genuinely solid. But two live defects (both reproduced failing by the testing reviewer) violate the feature's core promise, and DS-1's suppression interaction makes the failure mode *silent and room-wide*. Neither is catchable by the existing suite — blocking tests listed in §4.

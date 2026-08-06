Compliance review complete — written to `docs/pi-batch/feature-reviews/compliance_officer-r3.md` (supersedes r2). 

## Key results

**Priming**: `prompts/README.md` does not exist in-repo (verified); applied the shared rules at `/home/u1/ai-batch-runner/prompts/README.md`. All material claims re-derived from current source, not prior reports.

**Round-4 delta — both closed, verified in source:**
- **B1 (was compliance F1, Medium, blocking)**: `redact_blocks_for_recall_snapshot` (`message/mod.rs:187`) is total over the only two blob-carrying `Block` variants; the recall tx (`authorization.rs:288`) snapshots redacted blocks while GC still enqueues the **original** `blob_id`s. The evidence story is now internally consistent — text/transcript evidence retained, byte evidence destroyed, no dangling history refs possible by construction. Regression (10/10 storage recall) asserts zero `"blob_id"` in snapshot + both blobs queued + placeholder row.
- **B2**: `web/package.json:8` test script includes `render_recall.test.js` → `npm test` 82/0.

**Findings (feature-level, 8)**: 1 new Medium (`message_edits` has no FK/sweep/cutover story — orphans accumulate, DB-architect F2), 1 upgraded Medium (`changes_since` lacks a keyset tie-break — bulk same-timestamp sweeps skip >200 mutations on reconnect, invalidating the "≤200 convergence guaranteed" claim), 1 carried Medium (deferred GDPR erasure net misses recalled rows, `participant.rs:644` — re-verified), 5 carried Low (recall window, disclosure, unthrottled recall + no alert, 120-char audit digest, migration 10s timeout).

**Deployment-level (carried, compliance-relevant)**: no backup/restore (High), feature uncommitted so CI never evaluated recall (High), version-skew ack-drop unenforced (Medium), secrets fragment/dev JWT keys (Medium), monitoring unwired (Low).

**Unchanged unknowns**: jurisdiction, data classification, and framework applicability all unknown — GDPR appears only as a design influence in code comments. No compliance or certification claim is made from repository evidence.

**Next decisions**: no P0 remains in feature scope (B1/B2 closed); schedule F3 tie-break (highest-value), F2 erasure predicate, F1 `message_edits` lifecycle; product/legal must decide disclosure of surviving text/transcripts (now the *entire* residual copy post-B1), recall window, and admin-recall surface.

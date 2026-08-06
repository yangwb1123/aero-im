Review complete — written to `docs/pi-batch/feature-reviews/security_engineer.md` (supersedes the round-3 review). I verified the round-4 tree by source inspection plus independent targeted test runs (`redact_blocks_for_recall_snapshot_removes_byte_references` 1/1; web ws/api/render recall suites 26/26).

## Verdict

**B1 (recall snapshot ↔ attachment GC dangling reference) is closed — correct by construction, not just by test.**
- `redact_blocks_for_recall_snapshot` is total over the only two `Block` variants that carry `blob_id` (`File`, `Voice` — verified against the full enum; no other blob-carrying variants exist).
- GC still enqueues the **original** blob ids in the same tx; the JSONB containment scan runs *after* the placeholder UPDATE, so the recalled row can't hold GC open, and `message_edits` is never scanned — exactly why redaction is the right fix vs. extending the reference scan (which would permanently protect deleted-message bytes).
- The B1 regression is meaningful: string-level blob-id absence + `"blob_id"` key count 0 + transcript preserved + both blobs enqueued + row == placeholder.

**B2 is closed**: `web/package.json` test script now includes `render_recall.test.js`; `npm test` 82/0.

## Findings (no Critical/High)
| # | Sev | Item |
|---|---|---|
| F1 | CLOSED | B1 redaction fix verified end-to-end |
| F2 | LOW | Cross-tenant existence oracle 404 vs 403 (pre-existing; ULIDs unguessable; doc claim "防存在性 oracle" is inaccurate for existence) |
| F3 | LOW | Recall unthrottled on both entry points (matches delete precedent; admin mass-recall unalerted) |
| F4 | INFO | Rolling-deploy ack-drop of `kind:"recalled"` on old binaries — converges on reconnect via 3-column `changes_since` |
| F5 | INFO (new) | Forward-compat double-parse: future blob-carrying block variants would leak bytes (never destroy) and empty the snapshot on old binaries |
| F6 | INFO (new) | Card/ToolCall payloads pass redaction untouched; GC containment predicate is pinnable by a Card embedding a known blob uuid (pre-existing on delete path) |

## Key positive controls verified
Tenant/role TOCTOU closure (role + membership re-checked `FOR UPDATE` in the only write tx, `aero_effective_room_access` includes deactivation + mandatory TOTP); content-resurrection fences inside the system-edit transactions (caller-independent); exactly-one-winner concurrency with stable 409; version-gated relay suppression; member-gated history route; textContent-only client rendering; additive/idempotent migration 0238 with complete backfill projection; no new headers/secrets/dependencies; GDPR-safe `recalled_by` (tombstone keeps FK valid).

**Residual risk to flag for the product/compliance track**: recall ≠ erasure — pre-recall *text* and voice transcripts remain member-visible via `/api/messages/:id/history` (bytes are destroyed, text evidence intentionally retained); disclosure copy is still pending per the plan.

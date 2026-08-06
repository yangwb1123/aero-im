Review complete — written to `docs/pi-batch/feature-reviews/security_engineer.md`. Summary of the security review:

## Security Engineer Review — Message Recall (撤回)

**Priming note:** `prompts/README.md` does not exist (confirmed with the other reviewers) — review grounded in AGENTS.md invariants + direct source inspection across service/storage/route/WS/webhook/client layers.

### Findings (by severity)

| # | Sev | Finding |
|---|---|---|
| **S1** | HIGH | **Content resurrection via unfurl system-edit race.** `edit_locked_outboxed_in_tx` fences `deleted_at` + `version` but not `recalled_at`; the unfurl bot holds event-time original blocks, does a slow network fetch (attacker controls the URL → arbitrarily wide window), re-reads the post-recall version, and `edit_outboxed_system` succeeds — original content + preview cards restored to the live row, FTS re-indexed, Embed/Moderate re-enqueued, `Edited` broadcast to all clients. Recall's core promise defeated *after* acknowledgment. |
| **S2** | HIGH | **Change-replay never delivers recalls.** `changes_since` keys on `GREATEST(edited_at, deleted_at)`; recall doesn't bump `edited_at` — offline/reconnecting clients keep original content indefinitely; the client's `applyChange` recall branch is dead server-side. |
| **S3** | MED | Recall ≠ withdrawal: (a) `GET /api/messages/:id/history` serves the full pre-recall snapshot to any member (plus 120-char plaintext audit digest); (b) recall-triggered blob GC deletes attachment bytes still referenced by the history snapshot (`has_live_references` doesn't scan `message_edits`) — broken history attachments. |
| **S4** | MED | Recall is an unthrottled, admin-scoped, terminal destruction primitive — WS frame bypasses `check_ws_rate_room`/slowmode (edit has both); no per-actor alert for mass-recall abuse. |
| **S5–S7** | LOW | `recalled_by` outside GDPR erasure; rolling-deploy poison-drop of `recalled` events on old nodes (cursor advances, no replay — compounds S2); API-level reactions/pin/forward on recalled messages unfenced (no leak). |
| **S8** | INFO | Global id existence oracle (404 vs 403) — matches edit/delete invariant, UUIDs unguessable, not exploitable. |

### Positive controls verified
Permission matrix enforced twice (pure fn + commit-time re-check under `FOR UPDATE` with identity re-validation and final `WHERE recalled_at IS NULL AND deleted_at IS NULL` fence); oracle-safe 404→403→409→409→403 ordering with state checks strictly after `assert_room_access`; single-tx atomicity (snapshot + placeholder + audit + blob enqueue + outbox, gap-free versions); relay suppression of superseded recalls; search/vector removal verified end-to-end (incl. in-flight Embed no-op via `should_skip_embed`); AI answer-cache purge at bus chokepoint; client out-of-order/resurrect guards; textContent-only rendering (no XSS); additive serde compat; webhook payloads carry placeholder.

### Bottom line
The authz/tenant-isolation core is solid and well-tested. But S1 and S2 — both reproduced as failing probes by the testing review — violate the feature's central promise (content stays gone for every viewer, permanently) and must be fixed failing-test-first before ship: fence `recalled_at` in the system-edit storage path, and add `recalled_at` to the `changes_since` mutation key. S3a/S4 are product/ops decisions with small engineering tails.

Re-review complete — written to `docs/pi-batch/feature-reviews/security_engineer.md` (supersedes the round-1 review). Summary:

## Security Engineer Re-Review — Message Recall (post-fix tree)

**Method**: source-level verification of every authz/tenant/state path (storage tx, im-core service, REST+WS entry, bus listener, webhooks/bots, blob GC, history route, client), cross-checked against AGENTS.md invariants. All claims Verified by inspection; gate greens per implementer evidence (2158 lib tests, 9/9 + 28/28 recall suites, authz_lint 6/6).

### Bottom line
No Critical/High findings remain. Round-1 **S1 (content resurrection) and S2 (replay never delivers recalls) are closed**: the fences live inside the only transactions that write the row (`events.rs:84-89` edit, `192-197` transcript — caller-independent, covering unfurl/transcribe/webhook), `changes_since` uses `GREATEST(edited_at, deleted_at, recalled_at)` with a matching reissued index, and the partition backfill projection carries both columns — each with a failing-test-first regression test in-tree.

### Findings
| # | Sev | Finding |
|---|---|---|
| **F1** | **MED** | **Recall GCs attachment bytes still referenced by the member-visible history snapshot.** `message_edits` (full original blocks incl. `blob_id`s) is written in the same tx that enqueues those blobs for GC (`authorization.rs:288,330`); `has_live_references` (`blob.rs:328`) and the enqueue check scan `messages` only — **not `message_edits`** — and `GET /api/messages/:id/history` is member-accessible for live-but-recalled messages. Bytes are gone within ~60s while history still points at them. Fix: redact File/Voice blocks in the recall snapshot (option 1) or protect `message_edits`-referenced blobs (option 2); regression test specified. |
| **F2** | LOW | Cross-tenant existence oracle: 404 (unknown) vs 403 (exists-but-inaccessible) — the plan's "防存在性 oracle" claim is inaccurate; state is protected, existence is not. Pre-existing on edit/delete; ULIDs unguessable; optional alignment + doc fix. |
| **F3** | LOW | Recall unthrottled on both entry points (WS frame has no `check_ws_rate_room`; matches delete precedent); member abuse bounded by send rate; admin mass-recall uncapped/unalerted — documented backlog. |
| **F4** | INFO | Rolling-deploy poison ack-drop of `kind:"recalled"` on old durable consumers (DS-4 release-note discipline); `recalled_by` outside GDPR erasure (documented compliance decision). |

### Abuse-case table (highlights)
Identity spoofing ✅ (all actors from `AuthUser`) · REST/WS/bus replay ✅ (row-lock + `WHERE recalled_at IS NULL` → stable 409, exactly one outbox row, relay suppression 4-branch unit test) · cross-tenant/IDOR ✅ (`assert_room_access` + commit-time `aero_effective_room_access`, identity re-validated under `FOR UPDATE`, role re-checked on the membership edge — TOCTOU-safe) · proxy/header forgery ✅ (no new header trust; ip_allowlist is global boot middleware) · resource exhaustion ⚠️ F3 · sensitive-data ⚠️ documented decisions (history visibility, audit digest admin-only, placeholder-only everywhere else) · resurrection ✅ closed · XSS ✅ (textContent-only) · migration drift ✅ (additive, index expression == predicate).

### Validation plan
P0: F1 regression test + redaction/reference-protection fix, re-run storage recall 9/9 + full gates. P1: F2 probe test + doc correction. P2: F3 per-actor cap/alert + DS-4 release note. P3: full gate re-run after any fix.

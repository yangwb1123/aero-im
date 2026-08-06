Stage 02 review complete. Full report written to `docs/pi-batch/reviews/stage-02.out.md` (repo convention, alongside `stage-06.out.md`).

## Summary

**Scope note:** the task's stated repository `/home/u1/ai-batch-runner` is the tooling repo; all subsystem files were reviewed at `/home/u1/aero-im` (current `git` HEAD). Read all 4 `ai/prompts-shared/` files, all 15 primary files, the design spec, AGENTS.md, and supporting surfaces (AI worker, bus listener, reaction/pin repos, message-history route, rate limiter, migration).

**Findings (0 Critical / 0 High / 3 Medium / 2 Low / 2 Info):**

| # | Sev | Finding |
|---|---|---|
| F1 | Medium | **In-flight AI embed can resurrect recalled content into FTS/vector search.** `update_embedding`/`update_searchable_text` fence only `deleted_at`, not `recalled_at`; a worker that read the message pre-recall writes the *original* text/vector post-recall (`search_tsv` is a STORED generated column; `search_vector` returns any non-NULL embedding). Recall's index clearing loses the race. |
| F2 | Medium | **撤回 ≠ content removal.** Pre-recall text/transcripts are snapshotted into `message_edits` (only blob ids redacted) and served to *any room member* via `GET /api/messages/:id/history`; audit retains a 120-char digest. Deliberate per code comments, but inconsistent with the placeholder semantic and GDPR/erasure claims — needs a product decision. |
| F3 | Medium | **Recall bypasses the rate gates send/edit enforce.** WS `RecallMessage` frame and REST handler skip `check_ws_rate_room`/slowmode; each attempt costs ~8–10 queries incl. row locks, with no per-frame WS cap (REST bounded only by the generic 20 rps client limiter). |
| F4 | Low | Cross-tenant existence oracle: 404 (unknown) vs 403 (exists, inaccessible) ordering on message-id routes; the recall docstring claims "no existence oracle" — contradicted. |
| F5 | Low | Recalled messages aren't terminal server-side: reactions/replies/pins re-check `deleted_at` only; recall doesn't clean visible associations (UI disables them; server accepts, even notifying the recalled message's author). |
| F6/F7 | Info | Recall allowed on expired (invisible) messages; placeholder is plain text (confusion only). |

**Verified-good:** author/owner/admin matrix with commit-time re-check under row locks (TOCTOU closed), access-before-state at commit (no cross-tenant *state* oracle), exactly-one-winner double-recall, outbox materialization (delivers at version / suppresses superseded), membership re-resolved at fan-out (no delivery to departed members), system-edit fences, blob redaction + GC (gate B1), change-replay on `GREATEST(edited_at, deleted_at, recalled_at)`.

**RFC matrix:** no OIDC/OIDF standard applies (custom authenticated op; no token minting/validation in the feature); no OIDF certification claim exists in-tree — conformance tracked against the design spec + AGENTS.md invariants (10 rows, 8 Pass / 2 Partial (F1 search, F3 rate) / 1 N/A).

**Tests run (all green):** im-core 82, storage 319, server 655, authz_lint 6, web `npm test` 121, web-check 0. The 10 recall DB-integration tests are `#[ignore]`d and not runnable here (no Postgres) — **must run in CI before merge evidence is complete**.

**Ship decision: Yes, with conditions** — no Critical/High blocks. Before GA: fix F1 (add `recalled_at IS NULL` to both index UPDATEs + race test) and F2 (encode the history-retention contract); strongly recommended F3 (gate recall like edit). F4/F5 next patch; F6/F7 backlog.

Security review complete — saved to `docs/pi-batch/ui-reviews/security_engineer.md`. Summary:

## Verdict: PASS with recommendations

**Prior gate findings (all verified fixed in this session):**
- **F-1 (HIGH, cross-account draft leak)** — `app.js:974` `resetDrafts()` runs before `auth.clear()` on `forceReauth`; `auth_ui.js:101` `onLogout()` before `auth.clear()`; `store.reset()` cancels debounce timers; `activeRoom=null` nulls all DOM paths. Tests: drafts.test.js:449–503. The bootstrap `auth.clear()` at app.js:995 is safe (store empty pre-`switchRoom`, mirrors pid-keyed).
- **F-2 (sticky 400 reply_to)** — bounded single self-heal retry (drafts_store.js:140), tested.
- **F-3 (403 never mirrored)** — 403 branch now mirrors, tested.
- **Sticky-forbidden re-grant** — `setClean`/`reauthorize` clear it, tested.

**New findings (no Critical/High):**
- **S-1 Medium** — `GET /api/drafts` is unbounded/unpaginated and materializes N×4 MiB drafts into a full `Value` tree; a member of ~500 rooms can force a multi-GiB allocation → OOM. Unused by the SPA. Fix: LIMIT/pagination or remove.
- **S-2 Low** — draft PUT accepts message-scale blocks (4 MiB) vs. 8 KB composer cap → per-account storage/WAL amplification. Fix: dedicated draft budget.
- **S-3 Low** — no draft TTL; a failed clear-DELETE resurrects discarded text on reload (mirror already cleared). Erasure/leave-purge paths are covered.
- **S-4/5/6 Info** — message-regex 400 heuristic, unreachable `'anon'` mirror fallback, intended pre-check/fence asymmetry (verified safe).

**Positive controls verified:** participant-scoped queries + in-tx fence + DB trigger backstop (`message_drafts_scope_guard`), deactivation/2FA included in both app and DB fences, bearer-only auth with active-session revocation (CSRF N/A), no XSS (textarea/textContent only), idempotent upsert/DELETE, authz_lint coverage, oracle-safe NotFound/Forbidden, baseline 20 rps limiter.

**Verified this session:** 36/36 draft node tests + API contract tests green, `web-check.sh` 0 violations. PG-gated suite and HTTP smoke were run by the backend engineer; S-1/S-2 remediation should be followed by a re-run.

All findings independently verified and dispositioned; gate report updated at `docs/pi-batch/sdlc-gate.md`.

**Verification performed**: the eight-review pack (DB, QA, compliance, security, perf, async, principal, QA-lead) was cross-checked against the tree. The one release condition — **M1** (principal reviewer; echoed by async_reviewer #1 Medium and qa_lead F1) — was **confirmed real**: `web/recall_errors.js` discriminated by blacklist, so any 409 not matching the window regex (a reworded server detail, an unknown future variant) collapsed to `null` = silent swallow, and the module comment's fail-open claim only covered prefix drift.

**Resolved in place**:
1. **M1** — whitelist inversion: `null` only for the two convergent literals (`message is already recalled` / `message is deleted`); window-expired → info toast; every other 409 → error toast (fail-open). Pinned by a new test (`reworded or unknown 409 variants surface, never silently swallow`).
2. **qa F4 / async #4** — recall catch now routes `401 → forceReauth()` per the app's convention (`app.js:600`).
3. **qa F2** — new service DB test `recall_in_window_author_succeeds_with_nonzero_window` (in-window success through a non-zero window).
4. **qa F3** — `docs/recall-window.md` now states author-identity-takes-precedence (author-as-owner still windowed; contract already pinned by the admin-override test which enrolls the author as owner).

**Explicitly accepted with evidence**: M2/0238 non-concurrent index rebuild (prerequisite migration, scale-gated deploy checklist), L1 WS frame throttle (pre-existing edit-preflight class; alert on rate-of-change), L5 recall ≠ erasure (documented product decision), L7/metric harness/demotion-race (backlog), existence oracle (rejected as fix — repo-wide contract).

**Gates re-run after the fixes — all green**: check/clippy clean · lib 0 failures · web **128/128** (+1) · web-check 0 violations · truth-check exit 0 · `test-integration.sh` PASS · targeted im-core recall suite **9/9** on a fresh 238-migration throwaway DB.

VERDICT: PASS - the sole release condition (M1: web 409 discriminator fails closed) was confirmed real and resolved by inverting to a whitelist — only the two convergent literals stay silent, every other 409 surfaces, pinned by new tests (128/128 web) — the three recommended Low items (401→forceReauth, in-window service test, author-as-owner doc clarification) were also resolved with evidence, all remaining findings are explicitly accepted scale-gated deploy items, documented product decisions, or hardening backlog, and every gate re-ran green on the final tree.

# SDLC Gate — Message Recall Time Window (implementation round, gate 2)

- **Artifact**: shipped tree (recall window; design v3 as-built)
- **Reviews (8)**: database_architect PASS · testing_reviewer PASS · compliance_officer (no blocker) · security_engineer PASS · performance_engineer PERF PASS · async_reviewer PASS · qa_lead PASS · principal_reviewer **CONDITIONALLY READY (one condition: M1)**
- **Date**: 2026-08-06 · **Verdict**: PASS (M1 and the recommended Low items resolved in place, gates re-run green)

## Blocking finding — M1 (principal reviewer's release condition; async #1, qa F1)

**Verified real**: `web/recall_errors.js` discriminated by blacklist — any 409 not matching `/recall window expired/` → `null` (silent success). A server reword of the window detail ("recall window elapsed") or any unknown future 409 variant was silently swallowed; the module comment's "fails open" claim covered only prefix drift.

**Resolved (whitelist inversion)**: `recallErrorToast` now returns `null` **only** for the two server-produced convergent literals (`message is already recalled` / `message is deleted`); window-expired → info toast; **every other 409 → error toast** (fail-open). Pinned by two new tests in `web/recall_errors.test.js` (`reworded or unknown 409 variants surface, never silently swallow`). Web suite re-run: 18/18 targeted, **128/128 full** (+1).

## Low findings resolved in place

| # | Finding | Resolution | Verification |
|---|---|---|---|
| qa F4 / async #4 | recall catch skips the app's 401→`forceReauth` convention | `web/app.js:671` routes 401 to `forceReauth()` before the toast mapping (convention at `app.js:600`) | `forceReauth` in scope (imported); web suite green |
| qa F2 | no service-level in-window author success with a non-zero window | new `recall_in_window_author_succeeds_with_nonzero_window` (60s window: preflight `Ok(room)`, recall placeholder, `recalled_by=author`) | targeted run 9/9 on fresh migrated throwaway DB (dropped) |
| qa F3 | docs ambiguous on author-who-is-also-owner/admin | `docs/recall-window.md` 语义: 作者身份优先 — the window applies to the sender even with owner/admin role; contract already pinned by `recall_window_expired_author_rejected_admin_override` (author enrolled as "owner" → 409) | doc updated; existing test is the pin |

## Explicitly accepted with evidence (non-blocking)

- **M2 (dedup DB F1 / security F3 / compliance F1)** — 0238 non-concurrent index rebuild: prerequisite migration, not in this diff; 746-row dev DB; scale-gated deploy-checklist item (`CREATE INDEX CONCURRENTLY` + batched shadow backfill).
- **L1** — WS frames unthrottled pre-preflight (same class as pre-existing edit preflight): alert on metric rate-of-change, not absolute; frame-budget hardening backlog.
- **L3** — timeout-uncertain: convergence-safe (retry → 409-already-recalled → silent; WS event resolves UI); no auto-retry maintained.
- **L5** — recall ≠ erasure (history member-visible): documented product decision.
- **L6/L7, metric-assert harness, demotion-race test** — documented/backlog; contract test + `authz_lint` + layer tests mitigate.
- **Existence oracle** — explicitly rejected as a fix (repo-wide 404/403 contract, unguessable v4 UUIDs).

## Gates re-run after resolutions (all green)

`cargo check --workspace --all-targets` clean · `cargo clippy --workspace --all-targets -- -D warnings` clean · `cargo test --workspace --lib` 0 failures · `node --test web/*.test.js` **128/128** · `scripts/web-check.sh` 0 violations · `scripts/truth-check.sh` exit 0 · `scripts/test-integration.sh` **PASS** (throwaway DBs, dropped) · targeted im-core recall service suite **9/9** on fresh 238-migration DB.

---

VERDICT: PASS - the sole release condition (M1: web 409 discriminator fails closed) was confirmed real and resolved by inverting to a whitelist — only the two convergent literals stay silent, every other 409 surfaces, pinned by new tests (128/128 web, +1); the three recommended Low items (401→forceReauth convention, in-window service test, author-as-owner doc clarification) were also resolved in place with evidence, and all remaining findings are explicitly accepted scale-gated deploy items, documented product decisions, or hardening backlog — every gate re-run green on the final tree.

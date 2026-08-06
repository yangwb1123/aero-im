Principal review complete. Full report: `docs/pi-batch/sdlc-reviews/principal_reviewer.md`.

## 1. Advisory recommendation

**CONDITIONALLY READY** — one condition: **M1** (web 409 discriminator whitelist inversion) or explicit maintainer acceptance with a pinning test.

**Evidence confidence: high (backend), medium (web discriminator / DB / E2E)**. I independently re-executed: `cargo check` clean; `cargo test --workspace --lib` = **2168/2168** (exactly matches the gate claim); `clippy --all-targets` 0 warnings; `node --test recall_errors.test.js api.test.js` = **17/17**; truth/file-size/web-check all 0 violations; migrations = **238 unchanged**. I verified every load-bearing anchor in source: silent-fallback config convention (`config.rs:168-170`), read-once + dual-field boot log (`orig.rs:316-322`), shared leaf-crate predicate, preflight-before-rate-gate on **both** transports (`handlers/messages.rs:186`, `ws/ws_impl/frame.rs:163`), row-locked tx fence with window appended last, unchanged UPDATE WHERE, 409 contract pinned server-side, metric single-emit.

## 2. Consolidated findings (deduplicated)

**Critical/High: none.** No supplied or in-tree reviewer found a blocker.

- **M1** — `web/recall_errors.js` 409 discriminator fails closed: any non-window 409 (reworded, transient race, unknown future variant) → `null` = silent swallow. Verified in code; whitelist fix breaks zero existing tests. *async_reviewer #1.*
- **M2** — migration 0238 non-concurrent index rebuild (dedup of compliance F1 Medium + security F3 Low): deploy-time SHARE-lock write stall at scale; prerequisite migration, not in this diff; scale-gated (746-row dev DB). Deploy-checklist item.
- **Low**: L1 WS frames unthrottled + inflatable rejection metric (dedup perf F3 + security F1 — alert on rate-of-change); L2 no in-flight guard on ↶ (server-convergent, harmless); L3 timeout-uncertain (convergence-safe); L4 recall catch skips the app's 401→`forceReauth` convention (verified `api.js` has no global 401 handling); L5 recall ≠ erasure (`message_edits` member-visible via history — product decision); L6 cross-instance clock skew bound; L7 no CI-retained E2E.
- **Info**: existence oracle (repo-wide, don't fix), admin no-UI-recall gap + dormant `ws.js` path, retention policy, audit-readiness statement, positive perf finding (window shortens its own failure class).

## 3. Trade-off ledger (highlights)

| Conflict | Recommendation | Owner |
|---|---|---|
| Silent-fallback vs fail-loud config | Silent + boot log (accepted at gate; typo → *longer* 24h window — product-visible direction, ops must check boot line) | Maintainers (decided); product visibility advised |
| App-level fence on locked row vs SQL predicate | App-level — SQL would mix clocks, duplicate role join, add zero atomicity (perf F4 agrees) | Architect (decided) |
| Blacklist vs whitelist 409 discriminator | **Whitelist** (M1 — the release condition) | Maintainers (open) |
| Recall ≠ erasure (history member-visible) | Keep by-design + document; gating = product decision | Product |
| WS frame budget / 0238 CONCURRENTLY | Backlog / scale-time deploy checklist | Maintainers, devops |

## 4. Preconditions, rollback, monitoring

- **Preconditions**: fix-or-pin M1; re-run gates at release commit (all green in my run); DB-gated suite on fresh 238-migration throwaway DB (in-tree green, not re-run by me); boot-log value check (typo → 86400 fallback); deploy checklist for 0238.
- **Rollback**: config-only, zero migrations — unset env or `0` = pre-feature behavior; trigger on unexpected 409s at scale, metric anomaly, policy reversal. Rolling deploys transiently mix windows per-instance (note).
- **Monitoring**: `aero_messages_recall_expired_total` rate-of-change only (absolute is inflatable); capture `MESSAGE_PROCESSING_DURATION_SECONDS{op="recall"}` baseline; US4 signal = expired/recall ratio.

## 5. Missing reviews / next actions

- Not in pack, in-tree only: security/database/compliance/testing reports (presence + verdicts verified; security F1-F4 and compliance F1-F7 read); QA live E2E corroborated by testing_reviewer's own 6/6 E2E but **not re-run by me** (needs booted server + DB).
- Genuinely absent: SRE/devops review (deploy runbook) and product/PM sign-off (T1/T5/I2 are product-visible).
- **Narrow next actions**: (1) maintainers fix-or-accept M1 (~30 min); (2) record M2/L1/L7 in ops checklist; (3) product notes recall ≠ erasure in `docs/recall-window.md`; (4) re-run gates at release commit.

No fabricated sign-offs or owners — all decision owners are roles, and release authority rests with maintainers.

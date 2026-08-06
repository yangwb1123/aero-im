Review complete — written to `docs/pi-batch/sdlc-reviews/testing_reviewer.md` (sibling of the three prior reviews). All anchors re-verified against the tree; two of my findings are **new** (not carried over from the sibling reviewers).

## 1. Verdict

`VERDICT: FAIL - the test plan as written cannot be executed as specified. Two self-defeating tests: (1) the wire msg for every Conflict carries the thiserror Display prefix ("conflict: "), so the design's web discriminator `err?.message === 'recall window expired'` and its planned 8.4.11 mock (both unprefixed) pass against the mock while production never matches — the 409-swallow regression returns with green tests; (2) the exact-instant boundary test 8.2.4 ("backdate to exactly now_utc() − 86400s → Ok(Some)") deterministically fails under the design's own strict `>` predicate, because the fence's `now_utc()` is always ε > 0 later than the backdate's — "t = window allowed" is unprovable at the storage layer with a real clock.`

## 2. Findings (top 5 of 10)

| # | Sev | Defect | Test that catches it |
|---|---|---|---|
| 1 | High | `Error::Conflict` Display = `"conflict: {0}"` (`error.rs:22`); both envelopes use `to_string()` (`aero-server/src/error.rs:26`, `frame.rs:523`) → real wire msg is `"conflict: recall window expired"`. Design §6/F7/§8.4.11 all use the unprefixed string; existing `api.test.js:145` passes only via prefix-tolerant regex | Server contract test: expired author recall → assert body `{"code":"conflict","msg":"conflict: recall window expired"}`; then the web test with the **real** envelope in the mock |
| 2 | High | `now_fence − created_at_backdated = 86400s + ε > 86400s` always (ε = SQL round-trip); F5's app-clock backdating only shrinks ε from clock-skew to latency — the "provable inclusive boundary" claim is false; the natural "fix" (`>=`) violates req §2.5 | Unit: single captured `now` reused for `t=window → not expired`. Storage: margins 86399s → Ok / 86401s → Err (deterministic), or a `now` seam on the fence |
| 3 | Medium | 19 call sites (15 + 4 in `recall_index_fence_tests.rs`), design says 15 and §9 omits the file | `cargo check --workspace` after following §9 → red build |
| 4 | Medium | Web branch (the feature's only user-visible surface) untested; `app.js` not importable in place | Extract `recallErrorToast(err)` pure module, `node:test` + fake-DOM (render_recall precedent): 409-window → toast, 409-recalled → null, 429/500/status-0 → error |
| 5 | Medium | Zero server/contract-layer tests; "window never leaked to non-authors" (§5.4) unpinned by any test | Expired + plain member → 403 Forbidden (REST + WS + storage), never the window string |

## 3. Risk-coverage matrix (excerpt)

✅ covered: admin override, author-vs-admin race partition (no sleeps, `tokio::join!`), double-submit converge, precedence pins (both layers), partial-success (single outbox row + version bump), future `created_at`. ❌ gaps: storage boundary (F2), REST/WS 409 contract (F1), web discriminator (F4), member-no-leak pin, rate-gate ordering (structural only; 8.3.8 over-claims), fixture env hermeticity (F7), parse overflow/whitespace cases (F8), metric emit (F10).

## 4. Honesty audit

- ❌ **False**: §6 wire msg `"recall window expired"` (prefix verified); V10/§9 "15 call sites" (19); F5 "inclusive boundary provable"; 8.3.8 "before any rate charge" (service test can't observe the server-layer limiter).
- ✅ Honest: "unimplemented" status, V14 no-`app.test.js` admission, gates listed as a plan with no results claimed, "no migration" (verified — `created_at` exists).
- Not executed with reason: no build/tests — feature is unimplemented; both High findings are provable by reading current code, no execution needed.

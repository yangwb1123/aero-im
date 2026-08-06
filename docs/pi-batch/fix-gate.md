All gates verified. Final assessment:

## Round 2 verification summary

**S1 (blocking) — FIXED and independently verified:**
- `assert_message_recall_preflight` now carries the recall-role gate (`role_of` + `recall_authorized`) *inside* the preflight, before any charge; plain member → `Forbidden` without consuming budget.
- Both entry points keep the pinned order: preflight → `check_ws_rate_room` → mutation.
- authz_lint hardened with an ordering check (preflight must precede charge in the arm region) + `matched >= 2` self-check.
- Preflight test extended (plain member → Forbidden with exact message; promoted admin → room resolves).
- **Live behavioral proof I ran myself**: booted the server with `AERO_WS_RATE_STANDARD_PER_MIN=3` on a fresh 238-migration DB → `scripts/smoke_recall_rate_gate.py` passed: 5 doomed member recalls all 403 with **zero budget consumed** (owner's own recall then succeeds — conservation proven), owner over budget → REST **429 + Retry-After**, WS → error frame **code=rate_limited**.

**Non-blocking findings — all resolved:**
- S2: `ApiError::into_response` adds Retry-After on 429 (per-client middleware overrides via `insert`, verified); WS error frames propagate stable `AeroError::code()` (`"rate_limited"` confirmed at `error.rs:88`) instead of generic `"handler"`; `web/app.js` maps 429/`rate_limited` to back-off toasts (998 lines, web-check green).
- Medium test gaps: behavioral rate-fire closed by the live smoke; outboxed-writer race tests added (`concurrent_recall_vs_system_edit/transcript_write_never_resurrects`, 8 rounds, final-state invariants) — green on fresh DB.
- Low: `transcribe_bot.rs` doc now names `update_voice_transcript_outboxed` + cross-references the fence tests.
- Database-architect Lows (SQL fence on transactional writers, migration lock window) documented as residual risks per reviewer guidance — lock+re-check now race-tested; migration window scale-dependent, non-blocking.

**Gates re-run by me:** `cargo check --workspace --all-targets` ✅ · clippy 0 warnings ✅ · authz_lint 7/7 ✅ · web-check 0 violations ✅ · file-size-check 0 violations ✅ · truth-check 0 ORPHAN ✅ · storage recall suite 14/14 ✅ · im-core recall suite 4/4 ✅ · full `cargo test --workspace --lib` 319 passed / 0 failed / 606 ignored ✅ (exactly matching the report) · live smoke ✅. Server stopped, throwaway DBs dropped, dev DB untouched.

**Minimality & honesty:** round-2 changes are precisely the finding fixes + regression tests + smoke script + docs; DECISIONS.md honestly records the round-1 gate FAIL and this round's history; the completion report's command list matches my re-runs exactly, with no hidden not-executed items.

VERDICT: PASS - The blocking S1 finding is resolved and proven end-to-end: the recall-role gate now lives inside `assert_message_recall_preflight`, so doomed recall attempts by non-author members are Forbidden before any workspace budget is charged (verified by code trace, the extended preflight test, the hardened authz_lint ordering pin, and a live smoke I executed that proves budget conservation plus REST 429/Retry-After and WS `rate_limited` behavior); all non-blocking findings (web 429 mapping, WS error-code propagation, outboxed-writer race tests, transcribe_bot doc drift) are closed, every gate I re-ran is green, the change set is minimal and the completion report is honest.

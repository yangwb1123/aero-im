All changes verified. Final summary:

---

## 1. Analysis

**Module boundary**: The defect spanned three layers with clear ownership:
- **Application** (`aero-im-core::ImService::assert_message_recall_preflight`) — owns the recall role policy (`recall_authorized`, pure + table-driven); the S1 fix belongs here because the preflight is the *only* gate between an unauthorized caller and the charge.
- **Transport** (`aero-server` WS frame arm + REST handler) — owns the charge sequence; the invariant to pin is ordering (preflight → `check_ws_rate_room` → mutation).
- **Data ownership**: no new tables/columns; the workspace budget counter (`WsRateStore`, Redis fixed window) is unchanged — only *who may be charged* changed.

**Change points**: nothing varies (no payment channels/storage/pricing axes) → **no pattern**; composition of existing pieces suffices (the repo's own edit path is the template). Per over-engineering red lines: no new abstraction, no signature changes to `recall_message` (commit-time authority stays), ~2 extra queries per recall attempt with the same cost profile as edit's preflight.

**Dependency direction**: unchanged (Transport → Application → Storage), no cycles, `aero-common` untouched.

**Persistence**: no schema change — persistence modeling gate N/A (no new tables/entities; the recall transaction and its fences were already reviewed and verified in round 1).

## 2. Implementation (S1 blocking + all non-blocking findings)

| Finding | Fix |
|---|---|
| **S1 (blocking)** — preflight omits role gate → F3 gate is a workspace-wide DoS amplifier | `role_of` + `recall_authorized` hoisted **inside** `assert_message_recall_preflight` (`messages.rs`): plain member → `Forbidden` *before* any budget charge. `recall_message` keeps its full re-check (authority, same as edit) |
| S1 regression test | `recall_preflight_resolves_room_and_early_errors` extended: plain member → `Forbidden("only author or room admin may recall")`; promoted admin → resolves room. **Mutation-proven**: removing the gate makes the test fail at the member assertion |
| S1 budget-conservation pin | `authz_lint` F3 scanner hardened: recall arm region must contain `assert_message_recall_preflight(` **before** `check_ws_rate_room(` before `.recall_message(` — reorder/remove either → lint fails |
| S1 behavioral proof | New `scripts/smoke_recall_rate_gate.py` (live server, `AERO_WS_RATE_STANDARD_PER_MIN=3`): 5 doomed member recalls → 403, zero charge (owner's recall still succeeds); owner over budget → **429 + Retry-After**; WS → error frame `code=rate_limited` — **all passed** |
| S2 (Low) — 429 unmapped / no Retry-After | `ApiError::into_response` adds `Retry-After` (fixed-window hint; per-client middleware overrides with its exact value via insert); WS catch-all now propagates the stable error code (`downcast_ref::<AeroError>` → `code()`), fallback `"handler"` preserved; `web/app.js` maps 429/`rate_limited` → back-off toast (app.js back to 998 lines) |
| Medium — no behavioral rate-fire test | Closed by the smoke above (REST 429 + header + WS error frame) |
| Medium — no race test for outboxed writers | `concurrent_recall_vs_system_edit_never_resurrects` + `concurrent_recall_vs_transcript_write_never_resurrects` (8 rounds each; row lock makes outcome deterministic under any interleaving; final-state invariants: placeholder + empty `searchable_text` + NULL `embedding`) |
| Low — `transcribe_bot.rs` doc drift | Comment now names `update_voice_transcript_outboxed` and cross-references the fence tests |

## 3. Self-check (architecture.md §8 / evolution.md §6) + verification

- Boundaries clean, dependency direction intact, no cross-module internal access, no DTO/domain/ORM mixing, no new one-implementation abstractions, state transitions centralized (role policy is the pure `recall_authorized` + storage re-check under locks), transaction boundaries at the use-case layer.
- Commands **actually run**:

| Gate | Result |
|---|---|
| `cargo check --workspace --all-targets` | ✅ |
| `cargo clippy --workspace --all-targets -- -D warnings` | ✅ 0 warnings |
| `cargo test --workspace --lib` | ✅ 319 passed / 606 ignored |
| `cargo test -p aero-server --test authz_lint` | ✅ 7/7 (incl. hardened S1 scanner) |
| Storage recall suite @ throwaway DB (238 migrations) | ✅ 14/14 incl. 2 new race tests |
| im-core recall suite @ same DB | ✅ 4/4 incl. extended preflight test |
| Mutation test (gate removed) | ✅ preflight test fails at member assert — non-vacuous |
| `bash scripts/test-integration.sh` | ✅ 599 ignored tests, DBs dropped |
| `web-check.sh` / `truth-check.sh` / `file-size-check.sh` | ✅ 0 violations / 0 ORPHAN / 0 violations |
| `python3 scripts/smoke_recall_rate_gate.py` (live server) | ✅ conservation proven + 429 + Retry-After + WS `rate_limited` |

Nothing could not run. Residual risks (documented in `docs/pi-batch/fix-implementation.md`): the live smoke requires the server booted with `AERO_WS_RATE_STANDARD_PER_MIN=3` (documented in the script header, matching the smoke-harness convention); the two transactional writers remain lock+re-check (no SQL fence) but are now covered by concurrent race tests that hold under any interleaving.

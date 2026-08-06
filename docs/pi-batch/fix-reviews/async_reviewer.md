All verification is complete. Here is the review.

---

# Async & Data Review — Recall Security Fixes (F1/F3) + Test Isolation

**Reviewed revision:** `0bc0838` (worktree diff on top), 14 changed files + 1 new test module.
**Specs applied:** `async-data.md` (rate/race/idempotency decision tables), `error-recovery.md` (error→action mapping), `defect-patterns.md`, `form-table-state.md`, `prompts/README.md` evidence standard.

## What I executed (this revision, not the report's claims)

| Check | Result |
|---|---|
| `cargo check --workspace --all-targets` | ✅ clean |
| `cargo clippy -p aero-storage -p aero-im-core -p aero-server --all-targets` | ✅ 0 warnings |
| recall suite on **fresh throwaway DB** (created + migrated `aero_review_f1f3`, 238 migrations) | ✅ 12/12 incl. both new fence tests |
| `aero-im-core` recall/preflight tests | ✅ 4/4 incl. `recall_preflight_resolves_room_and_early_errors` |
| `authz_lint` (incl. new rate-gate scanner) | ✅ 7/7 |
| `call::security_tests` + `dm::block_tests` (the 0227-guard cleanup fix) | ✅ 2/2 |
| `scripts/web-check.sh` | ✅ 0 violations |

Note: the local dev `aero` DB is stale (migration 0185 function missing) — verification was done per AGENTS.md §4.3 on a throwaway DB, which is the correct protocol.

## Attack checklist assessment

1. **Request frequency** — N/A (no new client-side request sources; recall uses direct POST on explicit button click — correct per async-data §2 "用户明确点击查询 → 直接请求").
2. **Race conditions** — F1 fixed at the SQL level: all three lockless index writers (`update_embedding`, `update_searchable_text`, `update_voice_transcript`) fence `recalled_at IS NULL`; the two transactional system paths (`edit_locked_outboxed_in_tx`, `update_voice_transcript_outboxed`) serialize on the row lock + re-check. Race test (`concurrent_recall_vs_embed_write_never_resurrects`, 8 rounds) empirically proves no interleaving resurrects index content. Readers are safe by construction: recalled rows carry `searchable_text=''` (STORED `search_tsv` → empty) and NULL `embedding`.
3. **Duplicate submit** — recall is a one-shot state transition; `concurrent_double_recall_has_exactly_one_winner`; web maps 409→convergence. Compliant.
4. **Retry matrix** — no auto-retry on recall; timeout path relies on WS `msg:recalled` convergence (no blind retry — compliant). **Gap: 429 from the new gate is unmapped in the web client (finding 2).**
5. **State model** — server-authoritative row + WS broadcast convergence; no client-side copy divergence (`handleRecalled` orders by `recalled_at`). Compliant.
6. **Lifecycle** — N/A (no new listeners).
7. **Data consistency** — recall clears `searchable_text`/`embedding`/`blocks` atomically in one UPDATE; all resurrection writers fenced. Verified.
8. **Error recovery** — 404/403/409 stable and preflight mirrors them; **but the preflight omits the recall-role gate (finding 1)**, and 429 has no client mapping (finding 2).

## Findings

| # | Sev | Pattern (defect-patterns.md) | Evidence | Root cause | Fix (decision-table referenced) | Test that catches it |
|---|---|---|---|---|---|---|
| 1 | **Medium** | rate-gate chargeable by unauthorized caller (async-data §2 公平性 / §3 "禁止仅依赖 loading 布尔"; error-recovery §1 429) | `assert_message_recall_preflight` — `crates/aero-im-core/src/service/messages.rs:452-476` checks room access + deleted/recalled but **not** recall role (author/owner-admin); charge happens in `routes/handlers/messages.rs:182-184` and `ws/ws_impl/frame.rs:163-165`; `check_ws_rate` increments the workspace counter unconditionally on every under-limit call (`ws_rate.rs:318-328`, `incr_current` before the mutation) | The preflight mirrors edit's *shape* but not edit's *author gate* (`editable_message` rejects non-sender at `messages.rs:394-396`). A room member who is not the author passes preflight → workspace budget increment → `recall_message` returns Forbidden. Repeated doomed attempts drain the **shared** workspace budget (default 1200/min), starving every member's send/edit/recall for the window, repeatable indefinitely — the F3 gate itself becomes a DoS amplifier, and no per-client WS limiter bounds it (send path also workspace-only, but send charges only on valid sends) | Add the recall-role check to the preflight, mirroring edit: after `assert_room_access`, read `role_of` + `recall_authorized(actor, sender, role)` (already done in `recall_message`; hoist the same ~2-query check) so **only authorized recallers are charged**. F3's own invariant says "Same gate shape as EditMessage" — the role gate is part of that shape | Extend `recall_preflight_resolves_room_and_early_errors` (aero-im-core `db_tests/recall_tests.rs:186`): non-author member → `Forbidden` from preflight; plus a storage test asserting unauthorized recall attempts do not increment the workspace ws-rate counter |
| 2 | **Low** | 429 unhandled (error-recovery §1 "429 → 稍后重试"; async-data §5 Retry-After) | `web/app.js:668` recall catch special-cases only 409; `web/api.js` has no 429/`rate_limited` handling anywhere; ws_rate rejections emit `AeroError::RateLimited` (429, `aero-common/src/error.rs:73`) **without Retry-After** (only `rate_limit.rs` sets it) | F3 made 429 reachable on recall (both REST and WS `msg:error` → `web/app.js:110` generic toast), but the client maps it to "撤回失败:…" with no retry guidance | Client: `err.status === 429` → "稍后重试" toast (optionally parse Retry-After). Server (optional): add `Retry-After` to ws_rate rejections like `rate_limit.rs` does | `web/api.test.js`: mock 429 → assert recall caller enters the retry-later branch; assert no retry is auto-fired |
| 3 | Info | — | `crud.rs` lockless `update_voice_transcript` has no production callers (transcribe bot uses `update_voice_transcript_outboxed`, `events.rs:175`, lock+re-check) | Fence + fence test exercise a test-only path; production path already covered by `system_edit_after_recall_is_fenced` | None required; note only | — |
| 4 | Info | — | worker pays provider then fenced write refused (`aero-ai/worker/mod.rs:293-319`) | Pre-recall read → post-recall fenced write: bounded wasted provider spend, same class as pre-existing deleted-message behavior; documented as ops acceptance in `docs/DECISIONS.md` | None (already tracked) | real-provider E2E (listed residual risk) |

## What passed cleanly (no findings)

- **F1 fence correctness**: all four content writers are closed (3 lockless SQL fences + 2 transactional lock/re-checks); backfill query additionally fenced (`query.rs:328-340`); verified empirically 12/12 on fresh DB.
- **Regression guardrail**: `every_recall_entry_point_charges_ws_rate_budget` (authz_lint) is hermetic, scans recursively, self-checks `matched >= 2` so a refactor can't silently disable it.
- **Test isolation**: skip narrowed from `db_tests::` to the two order-dependent modules; 0227-guard cleanups verified green. Evidence's "over-broad skip" root cause is consistent with what I reproduced (stale-DB aside, the two repaired tests now pass).

---

## VERDICT: FAIL - Finding 1 is blocking: the F3 rate gate charges the shared workspace budget before the recall-role check, so any room member can drain the entire workspace's per-minute budget (default 1200/min) with doomed recall attempts on other members' messages — the new gate is itself a repeatable workspace-wide availability drain, and it violates F3's own "same gate shape as edit" invariant (edit's preflight includes its author gate; recall's does not). Finding 2 (unmapped 429) is non-blocking. All F1 fixes, tests, and isolation changes verified correct and green.

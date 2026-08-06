All verification complete. I have everything needed for the review.

---

# QA Review — Recall Security Fixes (F1/F3) + Test Isolation

**Scope reviewed**: 14 changed files + 1 new test module; re-executed every claimed command; mutation-tested the F1 fix (removed the fence → tests went red → restored → green).

## 1. Verdict

```
VERDICT: PASS - F1 fences are SQL-enforced on all content-bearing writers
(4 WHERE fences + 2 lock+re-check paths, inventory-complete against every
UPDATE on messages); reproduce-first PROVEN (I removed the update_embedding
fence and both new tests failed at their exact assertions); F3 gates wired
on both entry points after an access-checking preflight with structural
regression lint; test-isolation fix verified by full integration suite
passing end-to-end (twice); all 6 claimed commands re-verified green.
Two non-blocking gaps: no behavioral test that the F3 gate fires (429/WS
error frame), and no concurrent-race test for the lock+re-check outboxed
paths (sequential coverage only).
```

## 2. Findings Table

| Sev | Defect pattern | Missing test | Test that would catch it |
|---|---|---|---|
| **Medium** | F3 gate wired but behavior unproven: no test asserts the rate limit actually fires on recall (REST 429 + Retry-After; WS `ServerFrame::Error` with rate code). Only a source-scan lint (structure) + preflight logic test exist. | A behavioral rate-fire test. Note: this mirrors a codebase-wide gap (no 429 tests for send/edit either) — not a regression, but the F3 fix's *behavior* is unverified. | Seed a workspace with a 1-msg/min tier; REST `POST /api/messages/:id/recall` N+1 times → assert 429 + `Retry-After` on the last; WS `RecallMessage` N+1 → assert error frame `code` is the rate-limit code. Controllable via `AERO_WS_RATE_*` env (no sleeps needed — budget is a window counter). |
| **Medium** | Race window unproven for production outboxed writers: `update_voice_transcript_outboxed` (transcribe bot's actual path) and `edit_outboxed_system` have **no SQL WHERE fence** — they rely on row-lock + re-check. The new race test covers only `update_embedding`/`update_searchable_text`/the unused `update_voice_transcript` (crud.rs). A refactor that drops the lock (or moves the re-check) would resurrect content with no test failing — the sequential `system_edit_after_recall_is_fenced` would still pass. | Concurrent interleaving test for the two outboxed writers. | Mirror `concurrent_recall_vs_embed_write_never_resurrects`: `tokio::join!(recall_outboxed_authorized, update_voice_transcript_outboxed)` and `join!(recall, edit_outboxed_system)` over N rounds, then assert placeholder blocks + empty `searchable_text` + NULL `embedding` + `recalled_at` set. Row locks make the outcome deterministic under any interleaving — no timing control needed. |
| **Low** | "No budget drain by non-members" is order-correct but unasserted: the preflight test proves `Forbidden` for outsiders, not that zero rate capacity was consumed (the charge happens after the access guard, so an outsider's spam cannot exhaust a victim's budget — but nothing pins this). | Budget-consumption assertion. | After an outsider's failed preflight, assert the workspace's Redis window counter is unchanged (read the counter the way `ws_rate` does) or that a member's subsequent N legitimate operations still fit the budget. |
| **Low** | Doc drift making the fence work misleading: `transcribe_bot.rs:7` says "patches the message in place via `MessageRepo::update_voice_transcript`" — the code calls `update_voice_transcript_outboxed` (the *unfenced* SQL variant). The distinction is now load-bearing (fenced vs lock+re-check). | N/A (doc) | Fix the comment; add a one-line cross-reference in `recall_index_fence_tests.rs` noting the production transcribe path is the outboxed variant covered by `system_edit_after_recall_is_fenced`. |
| **Info** | `backend-quality.json` `files_scanned` 677→666 drifts in the same commit (unrelated bookkeeping). | N/A | Keep quality-scan bookkeeping out of feature commits. |

## 3. Risk-Coverage Matrix

| Risky path | Covered? | Evidence |
|---|---|---|
| Embedding write landing post-recall (F1) | ✅ | SQL fence (`crud.rs` `update_embedding`) + deterministic test + 8-round race test; **proven red without fix** (line 47 panic) |
| Doc-fold `searchable_text` write post-recall | ✅ | Fence + same tests (line 62/129 assertions) |
| Voice-transcript write post-recall, crud.rs variant | ✅ | Fence + deterministic assertion (line 62) |
| Voice-transcript write post-recall, **outboxed production variant** | ⚠️ sequential only | `system_edit_after_recall_is_fenced` (recall-then-write); no concurrent interleaving test |
| System edit (unfurl) post-recall | ⚠️ sequential only | Same test; lock+re-check, no SQL fence, no race test |
| FTS resurrection (`search_tsv` STORED gen col) | ✅ | Recall sets `searchable_text=''` in-tx; fence tests assert `searchable.is_empty()` post-writes |
| Vector resurrection | ✅ | Recall sets `embedding=NULL`; tests assert `embedding.is_none()` |
| Backfill re-enqueue of recalled rows | ✅ | `query.rs` fence (defense-in-depth; not directly tested but unreachable — recalled rows already excluded by empty `searchable_text`) |
| F3: WS `RecallMessage` gate | ⚠️ | Gate wired + access-preflight-before-charge; `authz_lint` source scan (self-checked ≥2 sites); **no behavioral 429/error-frame test** |
| F3: REST recall gate | ⚠️ | Same — preflight test covers error ordering; no budget-exhausted test |
| F3: non-member budget drain | ⚠️ | Order correct (access → charge); `Forbidden` asserted, "no charge" not |
| TOCTOU preflight→mutation | ✅ | `recall_message` re-runs every check; storage re-checks role/identity/state under row locks |
| Concurrent double recall | ✅ | Existing `concurrent_double_recall_has_exactly_one_winner` (ran green) |
| Permission change mid-recall (demotion) | ✅ | Role re-checked under lock (`recall_role_allowed_in_tx`) + existing permission-matrix tests |
| Test-isolation order dependence | ✅ | Memberships-first cleanup (0227 guard); skip narrowed to exact `db_tests::{notifications,relay}_tests`; suite green twice |
| Determinism/flakiness | ✅ | No sleeps anywhere; race outcomes SQL-enforced (invariant holds under any interleaving — the 8-round loop is belt-and-suspenders) |

## 4. Honesty Audit (completion-evidence)

| Claimed command | Re-executed | Result |
|---|---|---|
| `cargo check --workspace --all-targets` | ✅ | passed (14s) |
| `cargo clippy --workspace --all-targets -- -D warnings` | ✅ | passed (22s, zero warnings) |
| `cargo test --workspace --lib` | ✅ | passed — 17 binaries, **2165 passed / 0 failed** (666 DB-gated ignored) |
| `bash scripts/web-check.sh` | ✅ | passed — 62 JS files, 0 violations |
| `bash scripts/truth-check.sh` | ✅ | 0 ORPHAN (3 UNWIRED = pre-existing allowlisted builders, exit 0) |
| `bash scripts/test-integration.sh` | ✅ | passed **twice** — migration regressions + 597 ignored tests on throwaway DBs, DBs dropped |
| *(not claimed)* `test-notification-fanout.sh` (invoked inside the above) | ✅ | passed — step 5 ran 41 db_tests on fresh DB incl. `recall_preflight_resolves_room_and_early_errors ... ok` |
| New fence tests on throwaway DB | ✅ | both `... ok`; **mutation: removing the `update_embedding` fence → both FAIL (lines 47/129); restored → PASS** |
| `authz_lint` (new F3 scanner) | ✅ | 7/7 passed incl. `every_recall_entry_point_charges_ws_rate_budget` |

**Gaps with reasons**:
- `not_executed`: real-provider AI worker E2E — correctly listed with reason; DB-level race is covered by deterministic repro. No "理论上应该通过" phrasing anywhere; residual risks and assumptions are concrete and code-verifiable.
- Positive honesty signal: `docs/DECISIONS.md` records **two FAILED implement stages** (validation exit=1; plan stage found "3 of 4 crud.rs hunks landed" and forced the residual `update_voice_transcript` hunk) — the tree's history shows a real validation loop, not fabricated green.
- One nuance: the worker's `Ok(false)` comment says "row missing or deleted at update" — behavior correct (benign debug log), comment slightly stale w.r.t. recall. Cosmetic.

**Bottom line**: F1 is closed with SQL-enforced invariants and reproduce-first tests that I independently proved non-vacuous. F3 is correctly wired with structural enforcement; its behavioral layer (rate fires) is the one remaining test gap, consistent with the codebase's existing pattern. The two Medium findings are cheap to add and would harden the exact residual risks the report honestly disclosed.

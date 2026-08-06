# Testing Review (Round 2) — Message Recall (消息撤回)

> Round-1 review (`testing_reviewer.md`) found two blocking defects (P1 system-edit
> resurrection, P2 changes_since replay gap), each reproduced with failing probes.
> This round re-verifies the fixes and the full test surface at the 0238 revision.
> All evidence below was re-derived by executing commands on the current tree
> (no claim taken on trust).

## Verdict

**VERDICT: PASS - both round-1 blocking defects are fixed and their in-tree regression tests provably catch the defect (mutation experiment: reverting either fix makes exactly its regression test fail, the other 7 recall tests stay green). Every claimed gate was re-executed and is green (2158/0 lib, 82/0 web, 9/9 + 28/28 recall DB suites, 238-migration fresh replay, authz_lint 6/6, check/clippy/web/truth/file-size/backend-quality/completion-report). Remaining findings are LOW/MED client-side test gaps (ordering funnel, /changes 200-clamp boundary, timeout-uncertain UX) with concrete test recipes — none blocks this feature's server-side correctness promise.**

## Reproduce-first verification (the core checklist item)

| Round-1 defect | Fix in tree | Regression test | Mutation experiment |
|---|---|---|---|
| P1: `edit_locked_outboxed_in_tx` + `update_voice_transcript_outboxed` lack `recalled_at` fence → unfurl/transcribe resurrect content | `events.rs` — both paths return `Ok(None)` when `recalled_at.is_some()` (caller-independent, covers unfurl/transcribe/webhook) | `system_edit_after_recall_is_fenced` (storage recall_tests) | **Removed both fences → test FAILED** (7/9 pass, exactly this one + changes_since fail); restored → 9/9 ✅ |
| P2: `changes_since` keys on `GREATEST(edited_at, deleted_at)` → recalls never replay to offline clients | `query.rs:180` — `GREATEST(edited_at, deleted_at, recalled_at)` + migration 0238 reissues `idx_messages_room_mutated` with the same expression; recall does not fake `edited_at` (asserted) | `changes_since_delivers_recalls` (storage recall_tests) | **Reverted predicate to GREATEST(edited_at, deleted_at) → test FAILED**; restored → 9/9 ✅ |

The in-tree tests prove "this defect cannot return": reverting the fix fails the test, and the test asserts the business result (placeholder body survives the late system edit, no version bump; replay row carries `recalled_at` + placeholder, `edited_at` stays `None`), not mere renders.

## Findings table

| # | Sev | Defect pattern | Evidence (source) | Missing test / exact case |
|---|---|---|---|---|
| F1 | **MED** | Async-reordering logic untested — the exact pattern testing.md §3 lists first ("异步乱序") | `applyMessageMutation`/`applyChange` (app.js:309–343) — timestamp-ordered mutation funnel + resurrect guard + recall-before-create convergence; **no test imports app.js** (node suites cover render.js / ws.js / api.js only) | Extract funnel to a testable module (or jsdom harness); two Deferred promises controlling response order: stale Edited after newer Recalled → dropped; stale Recalled after Delete → dropped (resurrect guard); recall delivered before create → converged on backfill (row carries `recalled_at`); double-click → 409 mapped to success, single convergence |
| F2 | **MED** | Pagination-boundary gap: `/changes` is single-page, `limit.clamp(1,200)` (query.rs:176); client `replayChanges` advances cursor before the reply and **never loops** (app.js:336–342) | >200 mutations per reconnect window are silently lost — including recalls → stale pre-recall content persists (db-architect F2 agrees) | Server: `changes_since(room, since, 201)` → assert clamp (or continuation token) documented/tested; client: stub `roomChanges` to return 250 rows → assert second-page fetch or an explicit cap (cursor-advance-first makes this lossy by design) |
| F3 | LOW/MED | Timeout-uncertain + authoritative response discarded | app.js:672–677 — `api.recallMessage(m.id).catch(...)`; REST response (full recalled Message) ignored; api.js 30s AbortController → network-like error. WS down + REST commit → "撤回失败" toast for a **succeeded** op; retry 409 silently swallowed; converges only on reconnect replay | Mock fetch resolving-after-commit with WS closed → assert row converges from the REST response and toast is uncertain-state, not 失败; 409 → success |
| F4 | LOW | Derived-state divergence on mutation replace | `replaceNodeForMsg` (app.js:789) never calls `refreshReactionsFor` (only handleReaction/loadHistory/rerenderCurrentRoom: 385/592/613); recall SQL touches no `message_reactions` → chips vanish permanently for recalled rows (pre-existing for edits, recall makes it terminal) | render + apply `msg:recalled` to a row with reactions → assert `.msg-reactions` children repopulated |
| F5 | LOW | Duplicate-submit backstop untested (no in-flight lock / `Idempotency-Key` on the recall button) | app.js:672–677; server transition is atomic and loser 409s (mapped to success), so this is hardening not correctness | Double-click recall → exactly one request, or two requests with the second 409 and no error toast |
| F6 | LOW | Error-mapping gap: 401 on recall not routed to `forceReauth` (compare loadHistory app.js:600) | app.js:674 catch maps only 409 | Mock 401 → assert `forceReauth` invoked; 403/404/429 mapping asserted |
| F7 | LOW | `sleep` in a test (testing.md §6: sleeps as timing control) | `changes_since_delivers_recalls` uses `tokio::time::sleep(20ms)` to place `before` strictly before `recalled_at` — a timestamp-ordering boundary, not async-condition waiting; safe (timestamptz µs precision) but avoidable | Derive `before = recalled_at - 1ms` post-hoc instead; no behavior change |
| F8 | INFO | TOCTOU role-revocation mid-flight not deterministically tested (membership revoked between preflight and commit) | commit-time `aero_effective_room_access` re-check under `FOR UPDATE` (authorization.rs); concurrent **state** race IS tested | Requires a test hook; structural re-check is in the only tx that writes the row — accept as covered-by-construction, document |
| F9 | INFO | No E2E browser flow; no load/bench for the recall path | honestly listed in `not_executed`; tree-wide non-functional layer gap (db-architect §5) | Real-browser two-client recall + reconnect replay when a harness exists |

## Risk-coverage matrix

| Risky path | Covered? | Evidence |
|---|---|---|
| Async reordering — server (double recall, edit-after-recall, delayed relay, replay) | ✅ | `concurrent_double_recall_has_exactly_one_winner` (tokio::join!, exactly-one winner/conflict/outbox-row), `system_edit_after_recall_is_fenced`, `recalled_payload_is_delivered_at_version_and_suppressed_when_superseded` (outbox.rs unit: delivered / superseded / tombstoned / gone arms), `changes_since_delivers_recalls` |
| Async reordering — client funnel (app.js) | ❌ | F1 — no test imports app.js |
| Duplicate submit | ✅ server / ⚠️ client | sequential 409 (storage + im-core), concurrent race (storage), api.test.js 409 → ApiError; client double-click lock untested (F5, benign) |
| Timeout-uncertain / partial success | ⚠️ server / ❌ client | tx atomicity asserted (placeholder+audit+outbox+history one tx, rollback-on-failure pattern pre-existing); WS-down REST recall UX untested (F3) |
| 409 conflicts (already recalled / deleted, ordering) | ✅ | stable-message Conflict asserted in storage + im-core; web 409→success mapping asserted in api.test.js |
| Permission matrix (author/admin/owner/member/stranger) | ✅ | storage `recall_permission_matrix_author_admin_owner_member` + im-core admin/member/stranger/NotFound; mid-flight revocation structural-only (F8) |
| Multi-tenant isolation / no state oracle | ✅ | storage `recall_cannot_cross_workspace_boundaries` + im-core cross-room (404 vs 403 ordering asserted) |
| Terminal-state re-entry (resurrection) | ✅ **proven** | mutation experiment above |
| Replay/resync convergence (offline clients) | ✅ **proven** | mutation experiment above |
| Unmount/cleanup, lifecycle | ✅ n/a | no new timers/listeners; ws teardown suites pre-existing |
| Optimistic-rollback | n/a | recall does no optimistic client update (server-authoritative) |
| Pagination boundaries (>200 changes) | ❌ | F2 |
| Timezone/precision edges | ✅ | timestamptz ordering asserted (`edited_at` stays None on recall; `recalled_at` equality); UTC-only paths |
| Migration drift (0238: index expr == predicate, shadow projection, kind CHECK) | ✅ | `recall_schema_columns_and_outbox_kind_are_applied`, `partition_backfill_carries_recall_columns`, fresh-DB replay 238/238 |
| Contract (WS frame, serde compat) | ✅ | `recalled_frame_shape_carries_placeholder_message` (ws_impl/tests.rs), `message_deserializes_without_recall_fields` (common), cursor-hygiene regression (ws.test.js) |
| E2E / non-functional | ❌ disclosed | no browser harness; no load tests (tree-wide) |

## Honesty audit (completion-evidence)

Every claimed command was **re-executed on this tree** — all green, none fabricated:

| Claimed | My re-run |
|---|---|
| `cargo check --workspace --all-targets` | ✅ EXIT 0 |
| `cargo clippy --workspace --all-targets -- -D warnings` | ✅ EXIT 0 |
| `cargo test --workspace --lib` 2158/0 | ✅ 2158 passed, 0 failed (summed across 24 suites) |
| `bash scripts/web-check.sh` 0 violations / 57 files | ✅ 0 violations |
| `bash scripts/truth-check.sh` 0 orphans | ✅ 0 ORPHAN, 3 pre-existing allowlisted UNWIRED |
| `bash scripts/file-size-check.sh` 0 violations | ✅ 0 violations (71 pre-existing WARN) |
| `node --test web/*.test.js` 82/0 | ✅ 82 pass, 0 fail |
| `cargo test --workspace --test authz_lint` 6/6 | ✅ 6 passed |
| `aero-cli migrate` fresh DB 238 migrations | ✅ 238/238 on throwaway DB (via `AERO__DATABASE__URL`; plain `DATABASE_URL` is not read by figment — env mismatch, not a code bug) |
| storage recall 9/9 | ✅ 9 passed (incl. race + regressions + schema) |
| im-core ignored 28/28 | ✅ 28 passed, 0 failed |
| `test-integration.sh` fresh-DB replay + ignored suite | ✅ EXIT 0, "All integration tests passed", DBs dropped (my run: 644 ignored passed/0 failed vs claimed 594 — count variance across environments, exit code and all-green confirmed) |
| backend-quality 674 files / completion-report OK | ✅ both |

- **`not_executed` is honest**: real-browser E2E (no harness) and migration rollback are listed with reasons; the coverage substitutes (frame contract test, tx-content assertions, hermetic render path) are accurately described.
- **No "理论上应该通过" phrasing**; no skipped-to-pass (`#[ignore]` DB tests genuinely ran green; the 8 script-level `--skip`s each have their own fresh-DB regression stage earlier in the script).
- **Residual risks are real product decisions** (terminal recall, no time window, placeholder-in-history, recall≠erasure), not hidden gaps.
- **Round-1 honesty gap is closed**: the report's "same invariant as existing edit/delete" claim now holds — the fence lives in the shared tx helpers, and the regression test is in-tree.
- Minor note: the claimed "594 ignored tests" vs my 644 — both all-green EXIT 0; worth quoting the run's actual count in future reports.
- One environment gotcha worth documenting: `aero-cli migrate` reads `AERO__DATABASE__URL` (Figment), **not** `DATABASE_URL` — running it bare against a shared dev DB fails with a pre-existing migration-172 checksum drift (unrelated to this feature; validation aborts before applying anything).

## Bottom line

Server-side correctness of recall — the core promise that recalled content stays gone for every viewer under at-least-once delivery — is now tested at the right layers with assertions on business results, and the two round-1 defects are proven unreproducible (mutation experiment). The remaining work is client-side: bring the ordering funnel under test (F1, the highest-value next test), decide/cover the `/changes` 200-cap behavior (F2), and apply the error-mapping/timeout-uncertain polish (F3/F5/F6) with the listed recipes — all non-blocking for this round.

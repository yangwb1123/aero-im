All evidence gathered. Here is the review.

---

# Testing Review — Composer Drafts (web/drafts.js)

Reviewed: `web/drafts.js` (400 lines), `web/drafts.test.js` (20 tests), wiring diff (api.js/app.js/index.html/package.json/render.js), backend contract (`crates/aero-server/src/drafts.rs`, `crates/aero-storage/src/draft.rs`, `migrations/0028_drafts.sql` — pre-existing, committed at `e0f8a23`; this task is frontend-only).

Verified live: `node web/drafts.test.js` → **20/20 pass, 0 skip**; `web-check.sh` → 0 violations; `check-frontend-quality.py` → 0 violations; `cargo check --workspace --all-targets` → clean; `truth-check.sh` → 0 orphan. Backend wire contract manually verified: `Block` is `#[serde(tag="type")]` → `{type:'text'|'mention', ...}` matches `textToDraftBlocks`; GET envelope `{draft: {blocks, reply_to, updated_at} | null}` matches `restoreRoom`; PUT body `{blocks, reply_to}` matches `SaveDraftReq`; no 409 exists (upsert, last-write-wins).

## VERDICT: FAIL — blocking test gaps

Store-level logic is genuinely well tested (fake clock, manual deferreds, zero sleeps, per-test isolation, business-result assertions). But one real defect exists in a checklist-mandated risk (permission changes), the headline "stale-response protection" claim is untested at the flow where it lives (`restoreRoom`/`draftRoomSwitched`), and the claimed "API contract tests" do not exist.

## Findings

| Sev | Defect pattern | Evidence | Missing test that would catch it |
|---|---|---|---|
| **High** | **Permission change: `room.forbidden` is sticky — never reset.** After any 403 (save/del/restore), `input()`, `flush()`, `retry()` all no-op forever; `setClean()` (successful re-restore after re-grant) does **not** clear it. Autosave silently dies for that room for the rest of the session with no UI indication (a successful restore clears the status indicator, masking the dead state). Text survives only in mirror. | `drafts.js`: `forbidden` set in `handleSaveError`/`markForbidden`; grep confirms zero reset sites. Existing test only asserts the no-op *while* forbidden. | After `net.saves[0].reject({status:403})`, simulate re-grant: `store.setClean('r1','restored',null,0); store.input('r1','new text',null); clock.tick(800); await flush();` then `assert.equal(net.saves.length, 2)` — **fails today** (forbidden blocks the save). |
| **High** | **Async-reordering guards untested at the flow level.** `restoreRoom`'s `activeRoom !== roomId` early-returns and `inputRevOf !== inputRevAtStart` guard — the entire "stale-response protection" claim — are browser-only glue with zero tests. The one test ("restore guard primitives") asserts only that `inputRevOf` *advances*, not that a stale GET response is actually refused. | `drafts.test.js` — no test imports `restoreRoom`/`draftRoomSwitched`/`fillComposer`; `drafts.js` DOM section (lines ~150–400) is 100% uncovered. | Extract the decision core (e.g. `applyRestore(roomId, res, revAtStart)`): (a) GET deferred in flight → `store.input()` → resolve GET → assert composer untouched; (b) restore A in flight → `draftRoomSwitched(B)` → resolve A → assert B's composer untouched. Both fail if the guards are removed — today they pass vacuously. |
| **Medium** | **Contract layer missing (claimed but absent).** Report's `not_executed` reason cites "API contract tests" for the draft flow; no such tests exist — `web/api.test.js` has zero draft cases, `drafts.rs` has no handler tests, only `#[ignore]`-gated PG storage/security tests (`draft_upsert_get_replace_list_delete`, `personal_state_security_tests.rs`). `api.getDraft/saveDraft/deleteDraft` (method/path/body shape) are untested. | `rg draft web/api.test.js` → 0; `drafts.rs` no `#[cfg(test)]`. | `api.test.js`: stub fetch; assert `saveDraft('r1',{blocks,reply_to:'m'})` issues `PUT /api/rooms/r1/draft` with `{blocks:[…],reply_to:'m'}`, `getDraft` parses `{draft:null}` → `null`, `deleteDraft` uses DELETE. |
| **Medium** | **Clear-on-send depends on listener order.** Drafts' `onSubmit` discards only because `initDrafts` runs after app.js's `submit` listener (module eval order, lines 844 vs 965). Any reorder silently breaks "send clears draft" (draft resurrects after every send) with no test able to catch it — and both listeners read the same cleared composer, so even the empty-check can't distinguish "send succeeded" from "nothing to send". | `app.js:844` vs `app.js:965`; `drafts.js onSubmit`. | jsdom-style test: dispatch `submit` on composer with text → assert order-sensitive behavior (app.js listener first clears, drafts listener then DELETEs). Plain-node cannot cover; needs DOM harness or an explicit hook. |
| **Low** | **Unmount/cleanup untested.** `onPageHide`/`onVisibilityHidden`/`keepaliveSave` (raw `fetch(..., {keepalive:true})` — bypasses `api.request`, has no abort, no auth-failure handling) are browser-only, zero coverage. | `drafts.js` pagehide section. | Node test with injected `fetch` spy: call the extracted keepalive function with a token → assert `Authorization` header, `PUT` path, body blocks. |
| **Low** | **Duplicate submit untested.** Double-Enter → second `discard` → second DELETE (idempotent, benign by inspection). | — | `store.discard('r1')` twice → exactly 2 dels, no crash, status idle. |

Non-issues checked: 409 — no server conflict semantics (upsert), correctly N/A; timeout — 30s `AbortController` in `api.request` → `ApiError(0)` mapped to error+retry, tested; timezone/precision — only `hh:mm` status display, no edge; optimistic rollback — no optimistic draft UI; multi-device overwrite — documented deliberate residual risk. `validate_blocks` rejects empty blocks / >8 KiB text — frontend never PUTs empty, and >8 KiB CJK text fails draft save *and* send identically (pre-existing composer `maxlength=8000` mismatch), degraded-but-safe.

## Risk-coverage matrix

| Risky path | Covered | Where / gap |
|---|---|---|
| Async reordering (stale PUT after DELETE) | ✅ | `discard … deletes AFTER in-flight PUT`; per-room serialization test |
| Stale response vs typing (inputRev) | ⚠️ primitive only | flow-level guard in `restoreRoom` untested |
| Duplicate submit | ❌ | benign by inspection |
| Timeout-uncertain | ✅ | `{status:0}` failure tests + 30s abort |
| 409 conflict | N/A | server upsert, no 409 |
| Permission changes | ❌ **defect** | sticky `forbidden`, no reset, no re-grant test |
| Unmount/cleanup (pagehide) | ❌ | `onPageHide`/`keepaliveSave` untested |
| Optimistic rollback | N/A | none |
| Partial success (flush fail → mirror replay) | ⚠️ partial | dirty retention + mirror primitives tested; replay-and-resave flow untested |
| Debounce coalescing | ✅ | fake-clock test |
| Cross-tab / multi-device | ⚠️ | documented residual risk (deliberate precedence) |

## Honesty audit

| Claimed command | Result | Audit |
|---|---|---|
| `bash scripts/web-check.sh` → passed | ✅ verified rerun, 0 violations | honest |
| `node web/drafts.test.js` → passed | ✅ verified rerun, 20/20, 0 skipped | honest |
| `check-frontend-quality.py` → passed | ✅ verified rerun, 0 violations | honest |
| `cargo check --workspace --all-targets` → passed | ✅ verified rerun, clean | honest |
| `bash scripts/truth-check.sh` → passed | ✅ verified rerun, 0 orphan | honest |
| `not_executed`: real-browser E2E, reason given | ✅ | honest, correct |
| "API contract tests" cited as draft-flow coverage | ❌ **overstated** | no draft contract tests exist anywhere |
| residual_risks | ⚠️ incomplete | lists mirror-overwrite + regex duplication, omits sticky-forbidden and untested restore races |

No fabricated results, no skipped tests, no sleep-based timing, no shared mutable state across tests (per-test `FakeClock`/`FakeNet`/`withStorage` teardown is clean). The report's 5/5 command claims are real; the single overstatement is the phantom "API contract tests," and the two most important unclaimed gaps are the permission-re-grant defect (High) and the untested restore-flow guards (High) — both are why this is FAIL rather than PASS.

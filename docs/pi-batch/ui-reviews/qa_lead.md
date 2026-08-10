All probes complete. I have a full, measured picture. Final QA report:

---

# QA Lead Review — Composer Drafts (ui-implementation round)

**Revision reviewed**: working tree at `01591f7` (no Rust changes; web/ + docs only). All claims below are **Verified** by commands I ran this session unless labeled otherwise.

## 1. Test inventory & commands actually run (this session)

| Command | Result |
|---|---|
| `cd web && node --test drafts.test.js drafts_restore.test.js api.test.js` | **42/42 pass** (24 store + 10 restore/mirror/decision + 8 api incl. 3 draft contract) |
| `cd web && node ws.test.js` | **16/16 pass** |
| `cd web && npm test` (full suite) | **116/116 pass** |
| `bash scripts/web-check.sh` | 0 violations (62 files, 119 imports) |
| `npx eslint` on 10 touched files | exit 0, 0 problems |
| `check-frontend-quality.py --dir web --json` | 0 violations (61 scanned) |
| `bash scripts/file-size-check.sh` | exit 0 (0 violations; 71 pre-existing warnings; drafts files 78–397 lines, all ≤400) |
| `bash scripts/truth-check.sh` | 0 ORPHAN, 3 UNWIRED (pre-existing allowlisted seams) |
| `cargo check --workspace --all-targets` | clean (cached, current tree) |
| `check-completion-report.py docs/pi-batch/ui-implementation.md` | COMPLETION: OK |
| **My own probes** (`/tmp/probe*.mjs` against `createDraftStore`) | 3 targeted scenarios — **found a real re-grant bug (Finding 1)** |

**Not run** (no harness in sandbox): browser E2E, PG-gated tests (`-- --ignored`, need `DATABASE_URL`) — inventory verified to exist: `draft.rs::db_tests` (CRUD/privacy) and `personal_state_security_tests.rs` (authorized-variant fence, cross-room 403, deactivation filter, deleted-root reply fail-closed).

## 2. Requirement-to-test matrix

| # | Requirement | Status | Evidence |
|---|---|---|---|
| R1 | Per-room debounced autosave (800ms, coalescing) | ✅ | `drafts.test.js` "debounced autosave coalesces", "flush cancels the debounce", "saves serialize per room" |
| R2 | Restore with race guards (room switch / newer input) | ⚠️ Partial | Pure `pickRestoreAction` guards + `inputRevOf` primitives tested; **the actual early-return wiring in `drafts.js::restoreRoom` (`activeRoom !== roomId`, `inputRevOf` check) is untested glue** |
| R3 | Restore precedence local > mirror-newer > server > mirror > empty | ✅ | `drafts_restore.test.js` "restore precedence", "mirror newer than server wins" |
| R4 | Clear on send, Enter **and** button (single funnel) | ⚠️ Partial | Store-level DELETE-after-in-flight-PUT tested; `app.js` funnel hook (lines 893/910) verified by code review only — `submitComposer` clears input then calls `draftComposerCleared`; hook re-guards via `shouldDiscardOnSend` (tested) |
| R5 | Error mapping 401/403/409/5xx/network | ⚠️ Partial | 401-on-save, 403, 500, network tested; **401-on-DELETE untested**; 409 verified defensive (backend never emits it — grep: only canvas/messages/emoji/integrations use 409); 400 handling untested |
| R6 | Visible status indicator | ❌ Missing | DOM glue only; no harness. Code-reviewed (text+color dual channel, `role="status"`, click-retry) |
| R7 | Mirror lifecycle (write on failure, clear on confirmed save) | ✅ | Failure-write, rev-guard clear, per-room/per-pid keys, malformed payload — all tested |
| R8 | Logout isolation | ⚠️ Partial | `reset()` drops rooms/cancels timers (tested); `auth_ui.js` `onLogout` wiring untested (reviewed: runs before token teardown, mirror key pid-scoped) |
| R9 | API contract (routes, body shape, error surface) | ✅ | `api.test.js` 3 contract tests (encoded ids, `reply_to` omission, 401/403/409 → `ApiError`) |
| R10 | Backend privacy + fences | ⚠️ Partial | Repo-level PG-gated tests exist (Verified present, not run); **HTTP handler layer (401/403/400/404 statuses) has zero tests** |
| R11 | Keepalive chaining | ⚠️ Partial | `afterInflight` ordering tested; full `onPageHide`→`keepaliveSave` fetch path untested |

## 3. Findings

**F1 — Medium (behavioral bug, empirically confirmed). Re-grant after a save-403 leaves autosave silently dead.**
- Path: `saveNow` → 403 → `handleSaveError` sets `forbidden=true` → user keeps typing → access re-granted → next `restoreRoom` succeeds with **local** (or mirror) source → `fillComposer(..., {clean:false})` never calls `setClean` → `flush()`/`saveNow()` early-return on `forbidden` forever. Verified by probe: saves stay at 1 after re-granted restore; dirty text retained but never persisted. Compounding: the 403 branch is the **only** failure path that skips `mirrorWrite` (probe (a): mirror null after 403), and no status transition gives the user feedback — the indicator is silent. Only escape hatches: server-source restore (`setClean`, F3's covered path — probe (c) works) or logout `reset`.
- Regression risk: F3's stated fix ("setClean clears forbidden") is incomplete; a re-grant through the local/mirror path (the *common* path, since typing while forbidden is what made it dirty) never resumes saving. Data loss only in narrow corners (crash without pagehide/room-switch), but silent autosave death is the material defect.
- **Test to add** (`drafts.test.js`): 403 → `input()` newer text → simulate re-granted local restore (`fillComposer`-equivalent: input + `flush` after `setClean`-free path). Acceptance assertion: **after a successful restore, `store.flush()` issues a PUT with the post-403 text (net.saves.length === 2) and status returns to `saved`; `mirrorRead` holds the text on the 403 branch** (mirror parity with 500/network paths).
- Suggested fix: in `restoreRoom`'s local/mirror branches, clear `forbidden` on GET success (a successful GET proves access), or add a store `reauthorize(roomId)`; plus mirror-write on 403.

**F2 — Low. Mirror replay with a deleted reply target enters a permanent 400 retry livelock.** `restoreRoom`'s mirror branch re-stages `action.replyTo` into the store even though `resolveReplyTarget` already dropped the stale parent from the chip. Every `saveNow`/`retry` re-sends the dead `reply_to` → server 400 (`upsert_authorized` checks `deleted_at IS NULL`; handler check `reply_parent_exists_in_room` does **not** filter deleted, so the 400 surfaces from the repo fence) → error state until the user types (chip-null heals it). No data loss (composer + mirror retain text). Test: mirror with stale `reply_to` → flush rejects 400 → retry re-fails; acceptance: **the replayed save omits the stale `reply_to`** (pass the *resolved* chip replyTo into `store.input`).

**F3 — Low. Cross-clock comparison.** `pickRestoreAction` compares client-clock `saved_at` against server-clock `updated_at`, and the PUT response echoes no server timestamp, so clock skew can misorder precedence (client-clock-ahead makes a mirror of older text win and overwrite a genuinely newer server draft via self-heal replay). Documented as residual risk; bounded by last-writer-wins semantics.

**F4 — Low (coverage). Race guards live in untested glue.** The two guards that actually protect restore (`activeRoom !== roomId`, `inputRevOf` mismatch in `restoreRoom`; plus `draftRoomSwitched` flush/mirror and `onPageHide`) have no automated tests — only their pure primitives do. This is the same class of gap as the acknowledged `app.js` hook. Test: with a harnessable `restoreRoom` (DOM stubbed) — fetch in flight → room switch → resolve → assert no fill, no indicator clobber.

**F5 — Low (coverage). No HTTP-layer tests for draft endpoints.** `drafts.rs` handlers (400 invalid blocks / bad `reply_to`, 403 non-member, 404 unknown room, 401 unauth, tenant isolation via `assert_room_access`) are untested; only repo-level PG-gated tests exist. Regression risk: an authz regression would be caught only by the lint + manual review.

**F6 — Info.** `room.lastSavedAt` is write-only state (never read outside the store); error-indicator div is clickable but has no keyboard/`role="button"` (a11y); `keepaliveSave` hand-rolls the auth/body contract instead of `api.saveDraft` (drift risk if the API changes).

## 4. Prioritized scenario list

1. **Recovery (High):** 403 → type → re-grant → local restore → autosave resumes (F1 — currently fails)
2. **Race:** typing during restore fetch (inputRev guard) — pure guard tested; glue untested
3. **Race:** PUT in flight at send → DELETE ordered after (tested ✅)
4. **Race:** PUT in flight at pagehide → keepalive chained (store-level tested ✅; fetch path untested)
5. **Happy:** type→debounce→PUT→indicator saved→reload→restore precedence (partial — no E2E)
6. **Boundary:** empty/whitespace composer → DELETE not PUT (tested ✅); server draft with empty text (API-client-only; cosmetic "已恢复草稿" with empty composer)
7. **Error:** 401 on DELETE / 400 on stale reply (F2 — untested/failing)
8. **Error:** network failure → mirror → reload → mirror replay self-heal (store tested ✅, glue untested)
9. **Recovery:** failed discard → retry-as-delete (tested ✅)
10. **Isolation:** logout → different account in same SPA session (store tested ✅; `auth_ui` wiring untested)

## 5. Gaps, flake risks, fixtures, exit criteria

- **CI gaps**: no browser E2E (the "type → Enter-send → GET confirms deleted → switch room → restore" flow is only code-verified); PG-gated draft tests excluded from default CI (`--ignored`); no handler-level tests; indicator/DOM behavior untested (no jsdom harness in repo).
- **Flake risk: low.** All new tests are deterministic (FakeClock, manual-deferred FakeNet, `setImmediate` drains, per-file process isolation; the nested `withDraftFetch` restore pattern in `api.test.js` is safe — fetch calls bind synchronously during `map` setup). No real timers/network in unit tests.
- **Fixtures needed**: a DOM stub (or jsdom) to exercise `restoreRoom`/`draftRoomSwitched`/`onPageHide`; a PG fixture for handler tests (mirrors `draft.rs::db_tests::fixture`).
- **Exit criteria**: all 10 gates green (✅ re-verified this session); F1 is the only behavioral defect — **recommend fixing before release sign-off**; F2–F5 as planned work; no release blockers otherwise.

**Bottom line**: implementation quality is high — per-room serialization, rev guards, mirror lifecycle, and ordering invariants are genuinely tested and deterministic. One real bug (F1, empirically reproduced) and one livelock corner (F2) should be fixed; everything else is coverage debt, not correctness.

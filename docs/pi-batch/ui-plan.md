Plan written to `docs/pi-batch/ui-plan.md`. Summary:

## Key finding — the premise was stale

The tree already contains a full drafts implementation from a prior batch session (`web/drafts.js` 430 lines + `web/drafts.test.js` 444 lines, untracked; api.js/app.js/auth_ui.js/index.html/style.css wired). But its **gate verdict was FAIL** (`docs/pi-batch/ui-gate.md`), and I re-verified every finding against the live tree:

| # | Sev | Verified defect |
|---|---|---|
| **F1** | blocking | **Enter-send never clears the draft** — the only discard site is the form `submit` listener, but Enter goes `preventDefault(); submitComposer()` directly (textarea Enter fires no implicit submit). Sent text resurrects on next visit → duplicate-send risk. Button path works. |
| F2 | medium | pagehide keepalive PUT is an un-serialized raw `fetch` racing in-flight PUTs; restore unconditionally prefers server + clears the mirror → newer text can be lost. |
| F3 | high | `forbidden` is sticky: `setClean` doesn't clear it, so an in-session permission re-grant leaves autosave silently dead. |
| F4 | high | restore-flow guards (`restoreRoom`/`draftRoomSwitched`) are untested browser glue. |
| F5 | medium | api.test.js has 0 draft contract cases. |

## Plan (per your required sections)

- **Classification**: IM SPA composer sub-feature · main-view embedded component · desktop web · medium density · low motion · **risk low→medium** (F1 causes duplicate sends; F2 loses text). Demo-tier per assess; specs read and applied (async-data decision tables drive the design).
- **Module placement**: `web/drafts.js` gets `draftComposerCleared(roomId)` (replaces the fragile form-submit hook — Enter and button share the single `submitComposer` funnel, so the hook goes at its exit, ±3 lines in `app.js`), plus pure `pickRestoreAction()` and `resolveReplyTarget()` for testability. `ws.js` untouched (pure REST). No backend changes.
- **State model**: stays module-local per-room `Map` (delivery.js DI pattern), `inflight` serialization chain + `inputRev` version guard, mirror-vs-server decided by numeric timestamp comparison.
- **Interaction chain**: all 6 failure paths mapped (401→reauth, 403→forbidden + re-grant fix, 409/5xx/network→dirty-retain + click-retry indicator, restore races→skip, flush failure→mirror replay, DELETE idempotent).
- **Change radius**: 4 files (drafts.js, app.js, drafts.test.js, api.test.js); gates listed (web-check, `node --test` suite incl. api/ws/delivery regression, eslint, uiquality non-strict, truth-check, cargo check); 7 new test groups incl. F1 flow test asserting DELETE lands after any in-flight PUT.
- Deliberately **not** doing: `/api/drafts` list, splitting the 430-line module to satisfy the noisy strict-mode metric (project gate is non-strict; 60 baseline violations across 58 files), multi-device conflict resolution (backend has no version field).

Residual risks honestly listed: last-writer-wins across devices, duplicated mention-ULID regex, best-effort keepalive under hard teardown, and the app.js hook wiring having no static gate (code review + live check).

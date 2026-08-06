Review complete. All evidence gathered, three reproductions confirmed empirically against the actual store code.

# Async & Data Review — Composer Drafts (web/drafts.js + drafts_store.js)

**Scope reviewed**: `drafts_store.js` (store/races/mirror), `drafts.js` (DOM glue/restore/keepalive), `api.js` draft wrappers + error model, `app.js` submitComposer funnel/forceReauth, `auth_ui.js` logout ordering, server `drafts.rs` contract, all 42 tests. I ran the full suite (42/42 green) plus three adversarial repro scripts against the live store — **two of which reproduced real defects**.

## Checklist coverage

| # | Attack item | Result |
|---|---|---|
| 1 | Request frequency | ✅ 800ms debounce coalesces (timer re-armed per keystroke); send button never debounced (async-data §2: 提交按钮禁止 debounce); typing visual events throttled elsewhere (3.5s) |
| 2 | Race conditions | ✅ inputRev guards + activeRoom guards on restore; per-room inflight serialization (stale PUT can't land after DELETE). ❌ One race class found: **session lifecycle** (F-1) |
| 3 | Duplicate submit | ✅ `submitComposer` clears input synchronously after first accepted send → second click no-ops on empty; `draftComposerCleared` guarded by `shouldDiscardOnSend` (room match + empty value) |
| 4 | Retry matrix | ✅ 401 → forceReauth (no auto-replay); 403 → forbidden, no retry; 5xx/network → dirty retain + manual retry, never auto (mutations); timeout → ApiError(0), and PUT/DELETE are idempotent upserts so blind retry is safe (no timeout-uncertain hazard). ❌ 400-validation sticky case (F-2); 409 retry would blindly overwrite (accepted risk — backend can't emit it) |
| 5 | State model | ✅ Per-room `status` union (idle/saving/saved/error/forbidden) + `restoreFailed`, not boolean soup; no global loading |
| 6 | Lifecycle | ✅ Timers cancelled on input/discard/reset/setClean/saveNow; pagehide/visibilitychange installed once; `restoredHintTimer` guarded + cleared. ❌ **Store not torn down on 401 session teardown** (F-1) |
| 7 | Data consistency | ✅ Single store source + mirror as failure cache cleared on confirmed save; `setClean` reconciles store/DOM; `revAtSave`/`clean` prevents older saves clearing newer mirror |
| 8 | Error recovery | ✅ Specific messages (草稿保存失败,点击重试 / 无法保存草稿(无权限) / 已恢复草稿), dirty retained, mirror safety net. ❌ F-2 (sticky 400) + F-3 (403 never mirrored) |

## Findings

| # | Sev | Pattern | Evidence | Root cause | Fix (per decision tables) | Catching test |
|---|---|---|---|---|---|---|
| F-1 | **HIGH** | async-race 变体 / 生命周期缺口 — 跨账号草稿泄露 | `app.js:976` `forceReauth()`（401 / `auth_expired` app.js:77）不调 `resetDrafts`，只有登出按钮走 `auth_ui.js:101` → `onLogout`；`drafts_store.js:48` `pickRestoreAction` 的 `local` 分支无 pid/session 守卫；store 仅按 roomId 建键 | 401 会话销毁只清 localStorage（`auth.clear()`），不清内存 store；同 SPA 会话内换账号登录后 `restoreRoom` 把旧用户 dirty 文本填进新用户 composer 并 `flush` 到**新用户的**服务端草稿（REPRO1 复现）；未取消的 debounce 定时器也会在新会话下 PUT（REPRO2 复现） | forceReauth 顶部先 `resetDrafts()`（在 `auth.clear()` 前，使 active room 快照以旧 pid 落 mirror）；更稳的是 store 按 pid 键控并在 pid 变化时 reset（async-data §8 生命周期成对释放；§7 禁止跨生命周期更新状态；error-recovery §1 401→重登，但禁止把旧用户数据重放到新用户） | store 级：401 后 `resetDrafts()` → `snapshot`/`inputRevOf` 归零；集成级：401→forceReauth→以另一 pid 登录→开同房间→composer 为空、无 PUT 发出 |
| F-2 | MED | error-recovery — 过期 reply_to 造成不可恢复的保存错误 | `drafts_store.js:129-135` `handleSaveError` 无 400/422 分支，`retry()` 带同一 `replyTo` 重存；服务端 `drafts.rs:79` 拒绝房内不存在的 reply_to（400） | 回复目标被删后 autosave 永远 400；点击重试必然再次失败，且提示"点击重试"误导。restore 路径有 `resolveReplyTarget` 自愈，save 路径没有 | 收到 400（body 含 reply_to 语义）时：清 reply 上下文、mirror 文本、去掉 `replyTo` 重试一次并显示明确提示（error-recovery §1 422 行"修正字段或业务条件"，禁止盲目重试 4xx；§4 操作可恢复性） | save reject `{status:400, body:{message:'reply_to must…'}}` → 下一次 enqueue 的 blocks 请求不带 reply_to |
| F-3 | LOW | form-dirty-state-loss — 403 路径从不写 mirror，刷新丢文本 | `drafts_store.js:130` 403 分支在 `mirrorWrite`（:134）之前 return | 403 被当作"无需保留"，但文本是用户输入的，且 `setClean` 已实现权限恢复自愈——刷新后应能从 mirror 找回 | 403 分支同样 `mirrorWrite`（保持 forbidden 标志与现状，仅补 mirror；form-table-state §1 草稿恢复 / 保存失败保留输入） | 403 on save → `mirrorRead` 持有文本 |
| F-4 | INFO | （已审查通过）DELETE 完成无条件 `mirrorClear` | `drafts_store.js` saveNow 空文本路径 | 语义上正确：清空 composer = 丢弃意图，mirror 内容要么是被丢弃文本要么不存在；与 PUT 路径 `clean` 守卫的不对称是合理的（已推理 + 测试覆盖） | — | 已有 |
| F-5 | INFO | 409 → retry 会盲目覆盖 | `drafts_store.js` 409 走通用 error 路径 | 后端无版本字段、当前不发 409（已文档化为 residual risk）；若未来加版本，需按 error-recovery §2 刷新数据后交用户决策 | 保持文档化风险 | — |

## Verified strengths (non-defects)

- **Debounce/serialization**: 800ms coalescing, per-room chain (`enqueue` tail never rejects), `revAtSave` clean-guard — all tested and correct.
- **Keepalive ordering**: `afterInflight` chains the pagehide PUT behind the room tail; microtask FIFO preserves order vs. later debounced PUTs; synchronous mirror write is the teardown backstop. Sound.
- **Clear-on-send**: single funnel hook covers Enter + button; rejected/offline sends keep the draft (`shouldDiscardOnSend`); discard DELETE chains after in-flight PUT (tested).
- **Timeout-uncertain**: all draft ops are idempotent upserts/deletes → no duplicate-execution hazard on blind retry.
- **Restore races**: activeRoom + inputRev double guards prevent stale GET clobbering; precedence (local > newer-mirror > server > mirror) with numeric timestamp comparison is correct.
- **Logout button path**: `onLogout` runs before `auth.clear()` → mirror written under the correct pid. Ordering verified.

---

**VERDICT: FAIL - F-1 is a blocking defect: the 401/`auth_expired` session-teardown path never resets the draft store, so a second account logging in on the same SPA session restores and server-saves the previous user's private draft text (reproduced: REPRO1 restore leak + REPRO2 pending-timer PUT under the new account). This directly contradicts the requirement "logout isolation: in-memory draft state never crosses accounts," which is only implemented for the logout-button path. F-2 (sticky unrecoverable 400 on stale reply_to) and F-3 (403 never mirrors, reload loses text) are secondary but should be fixed in the same pass — all three fixes are small and reference existing decision-table rules.**

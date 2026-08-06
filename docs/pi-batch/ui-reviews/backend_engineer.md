Done. Final report saved to `docs/pi-batch/backend-review-drafts.md`. Summary:

## 1. Analysis

**Module boundary / data ownership** — 草稿 = IM 消息域的私有暂存态，按 AGENTS.md §4.1 配方落地：`migrations/0028`（表）→ `aero-storage/src/draft.rs`（`DraftRepo`，所有操作 `*_authorized` 且以 `participant_id` 作用域化，无 IDOR 面）→ `aero-server/src/drafts.rs`（`.merge` 进 `routes::build`，先 `assert_room_access`）。公共契约与 `web/api.js` 严格对齐（GET `{draft|null}` / PUT upsert / DELETE，错误映射 401/403/404/400）。

**强一致** — 一次事务 = 有效访问 fence（`aero_effective_room_access` 锁链）+ reply 目标 FOR SHARE 存活校验 + upsert；0214 迁移补齐 DB 兜底（FK ON DELETE CASCADE、`message_drafts_scope_guard` 触发器、离房清扫、父消息墓碑清 reply 边）、GDPR 删号覆盖（participant.rs:408）。`reply_to` 刻意不设 FK — ephemeral 硬删会被 FK 阻塞，由触发器+应用校验替代（规格 §9 要求已满足）。

**变化点 / 模式** — 无变化轴。**明确 "no pattern — composition suffices"**：三层（Route → ImService guard → Repo）即完整；fence 是第 2 副本（rule of three 未到期）；依赖方向单向无环。

**Persistence Design 复核**（§12 模板逐项）— 复合主键即业务键、无幂等键需求（PUT 天然幂等）、索引覆盖 WHERE+ORDER 非冗余、无历史（暂存态非事实）、删除路径四路齐备、迁移幂等可重放。

## 2. Implementation（本轮两处真实门禁违规，均修复）

1. **`web/drafts.test.js` 死锁** — `reauthorize` 测试 `await store.flush('r1')` 等待的 op promise 在 await 之后才 resolve → 永久挂起（上轮报告称 passed，实测 `node --test` 60s 仍 cancelled 1）。修复为「先发起、泵微任务、断言、resolve、再 await」；28/28 通过，52ms。
2. **`web/app.js` 1001 行** — 超 JS HARD(1000)；HEAD 恰 1000，本轮 +1 越线。压缩两处纯注释 → 998 行，0 违规。

## 3. Self-check（命令实测）

| 门禁 | 结果 |
|---|---|
| cargo check / clippy / test --lib / authz_lint | ✅ / ✅ 0 新警告 / ✅ 319 passed / ✅ 6 passed |
| truth-check / file-size-check / web-check | ✅ 0 ORPHAN / ✅ 0 违规 / ✅ 0 违规 |
| `npm test` 全量 18 文件 | ✅ 121 passed（修复前挂起） |
| PG 门控（一次性 pgvector 容器，全链迁移） | ✅ 238 迁移 ✓；draft 6 passed；personal_state 4 passed |
| 活服务 HTTP smoke（真 server+PG/Redis/NATS） | ✅ PUT/GET/upsert/reply/隐私/403/404/400/幂等 DELETE/401 全过 |

**未能运行**：真实浏览器 E2E（无 harness）；`make migrate-smoke`（等价动作已做）。另发现全仓 eslint 3 个存量 error（`oidc_callback.js`、vendor SDK）——均非本轮文件，未顺手改（变更范围纪律）。

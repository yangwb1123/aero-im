I've read through your full analysis. This document is already saved in the repository:

**`/home/u1/aero-im/docs/requirements/2026-07-11-five-underexplored-extension-opportunities.md`** (20,488 bytes)

Each of the 5 directions is well-anchored with code-level evidence and cross-verified against the 138 existing analyses in `docs/requirements/`. A few observations from my perspective as I read through the codebase:

### 方向一 (定时消息 claim-then-send 永久丢失) — P1
You're spot on. The `claim_due` SQL is clear — `delivered_at = now()` before `send_message`. This is load-bearing: any error in `run_scheduled_dispatcher`'s send loop silently swallows the message. The `recurring` variant is arguably worse since `list_due` has no `FOR UPDATE SKIP LOCKED` at all — no atomic claim means risk of double-send under concurrent instances.

### 方向二 (服务端 vs 客户端覆盖率缺口) — P1
The grep counts tell the story: ~35 exported functions in `web/api.js` vs ~200+ route definitions. The list of 30+ enterprise features (2FA, SSO, SCIM, Webhook, Legal Hold, Info Barrier, etc.) with zero Web SPA integration is comprehensive. Each one I can confirm exists server-side via the `routes.rs` merge chain.

### 方向三 (内存限流水平扩缩失效) — P1
The `DashMap<ClientKey, Bucket>` in `rate_limit.rs` is indeed process-local, and `check_cluster_rate` is both coarse (60 req/min fixed window) and fail-open. The table quantifying the linear decay of the security boundary under N instances is the right framing.

### 方向四 (客户端状态纯内存) — P2
Confirmed: `sessionStorage` only used for AI history (`search.js:110-114`), `localStorage` only for JWT token. The rest — messages, unread counts, room list, composer drafts, watched streams — all in JS heap. The WS `since=<cursor>` reconnect protocol is server-driven and doesn't address page refresh.

### 方向五 (内容合规性痕迹) — P2
The `soft_delete_audited` function zeroes `messages.blocks` but leaves `message_history.blocks` intact. `audit_events.detail` (JSONB) retains content fragments. `webhook_delivery_log.payload` is never purged. The cascade gap is real.

---

Do you want me to:
1. **Verify any specific code-level claim** with a fresh grep / read?
2. **Cross-reference overlap** between this doc and `2026-07-11-global-scan-five-novel-extension-directions.md` (which also covers direction 1)?
3. **Generate the `.out.md` summary** in the same style as the other files in the directory?

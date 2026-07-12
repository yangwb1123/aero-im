这是一份分析深度极佳的文档，我逐方向过了一遍，结合 `AGENTS.md` 和代码库理解给出反馈。

---

## 整体评价

**方法论扎实**：全量扫描 + 代码证据引用 + 边界情况枚举 + 分级，达到了真正架构评审的水准。不是「我觉得应该做 X」，而是「这里缺什么 → 数据在哪里 → 不做的后果是什么」。

几个我**不完全认同或觉得可以补充**的点，按方向分述。

---

## 方向一（多租户配额）—— 基本同意，但一关键细节

> 「聚合限流的 fail-open 策略允许无限 throughput」

这个说得对，但有 nuance：`ws_rate.rs` 里的 `FAIL_OPEN` 是 Redis 连接失败时的退化策略，不是正常路径行为。日常运行中 Redis 正常时 per-workspace 1200/min 是**硬限制**（`check_ws_rate_room` 返回 429 且 `check_blocked`）。问题在于：Redis 故障 + fail-open = 无限，这个窗口期确实是 DoS vector。

不过我觉得这里**优先级上不如安全面紧急**，原因是：目前单 server 实例场景下（部署拓扑=单节点，port 3030），per-process rate limit 实际就是全局 limit。攻击者 spray 多节点的威胁模型在当前部署拓扑下尚不成立。**按部署拓扑排优先级**，单节点时期方向三的 WS token 泄露 / SAML 无签名验签是更大威胁。

**补充缺失边界**：

| 你漏掉的场景 | 影响 |
|---|---|
| `AeroStorage::store_blob` 返回 `Err(StorageQuotaExceeded)` 但 web 端无对应 UI（`web/blobs.js` 无 quota error handler） | 用户上传失败静默吞掉 |
| 配额 middleware 前置 gate 与 `put_blob` 内 `blob_gc_drain` 后置清理产生 race（quota check 通过 → GC 清理 → 实际可用空间变大 → 其他操作插入） | 非一致 snapshot 导致配额误判 |
| `create_room_in_workspace` 当前在 `ImService` 内，配额检查需要 `storage` 层 repo → 业务层 `ImService` 耦合 | 如果要加 quota，得先在 `ImService` 和 `routes` 之间插一层 `QuotaMiddleware`，不然每条 create 路径都要改 |

---

## 方向二（API 生命周期）—— 部分同意，但优先级需要降

你说：

> 「无法独立演进」「破坏性变更即停服」

我**部分不同意紧急程度**。当前系统**唯一的消费者是 `web/` 同仓库 SPA**，部署时 server + web 一起发布。破坏性变更只要 SPA 端同时改，不存在「旧客户端断裂」问题。是**当且仅当**有了第三方开发者 / 移动端 / 桌面端时，API 版本化才变成生死问题。

所以我建议：

1. **统一响应信封**（`{ok, data, meta}` / `{ok, error}`）应该做——这是 API hygiene，不是版本化，且当前 `ApiResult` 模式确实不一致（有的 handler 返回 `Json<Vec<...>>`，有的用 `ApiResult<Json<Value>>`）。
2. **WS 协议版本协商**（`?v=N`）应该做——因为 WS 帧格式变更直接导致断连。
3. **REST 版本前缀**（`/api/v1/`）——**可以延后到第一个外部消费者之前再做**。

另外一点你的分析没提：**OpenAPI 生成**。Axum 0.7 + `utoipa` （或 `aide`）可以零成本从类型系统生成 OpenAPI spec。如果做了统一信封，OpenAPI schema 就能自动同步——这是 API 契约自动化的捷径，建议放进方向二的「近期」而非「远期」。

---

## 方向三（安全纵深）—— 最认同，但有四个重要补充

### 补充 1：WS token 的完整风险链

你说：

> 「WS JWT 在 query string 明文」

这是**正确但低估了严重性**。完整攻击面：

```
nginx access log:  "GET /ws?token=eyJhbGciOiJSUzI1NiJ9... HTTP/1.1" 200
                         ↑ JWT 全文明文落盘
```

- Nginx 日志默认记录完整 URI → token 持久化在日志文件 → 日志泄漏 = 永久凭证泄漏
- `Referer` header 在 WS 握手时不会发送，但**初始 HTTP 页面**如果通过 `new WebSocket("wss://host/ws?token=...")` 建立连接，`WebSocket` 构造函数的 URL 可能在 devtools / error stack trace 中暴露
- 代理/CDN（Cloudflare、AWS ALB）query string 日志默认开启

**修复方案**你说到了 `Sec-WebSocket-Protocol`，我补充一个更务实的路径：

```
阶段 1（本周可做）：
  ws/mod.rs connect handler 先 parse ?token=，移入 X-Forwarded-Token 或直接
  在 handshake 成功后立即从 query string 取出 JWT 验证，不持久化到日志

阶段 2（下月）：
  改用 Sec-WebSocket-Protocol: "aero, bearer.{jwt}" 
  服务端校验 sub-protocol，废弃 query string 路径
```

### 补充 2：SAML 签名验证——你漏了关键代码证据

你说「SAML 无签名验证」。`AGENTS.md` 中：

> `saml.rs`：`xmlsec1` 未安装，`bergshamra` 实验性且无审计

但这其实有一个**更致命的问题**：`saml.rs` 目前的实现路径是**fail-closed on misconfiguration** 还是 **fail-open on missing dependency**？

如果 `xmlsec1` 未安装时 SAML login 仍然接受未签名断言，那这是**神级漏洞**（CVSS 9.9+：攻击者只需要知道合法 IdP 的 EntityID + ACS URL 就能冒充任意用户）。**应该立即验证这一点并加 test**。

### 补充 3：PAT scope 模型

你说「PAT 无 scope、无 expiry 下限」。我补充一个具体的 scope 提案（基于现有代码结构）：

```
现有 `assert_room_access(participant, room)` 已经是 per-room 级别的 gate。
PAT 如果加 scope，scope 模型应该是：

scopes = [
  "messages:read",     // GET /api/rooms/:id/messages
  "messages:write",    // POST /api/rooms/:id/messages  
  "messages:delete",   // DELETE /api/messages/:id
  "workspace:read",    // GET /api/workspaces/:id
  "workspace:admin",   // 管理操作
  "streams:watch",     // WHEP / stream watcher
  "webhooks:manage",   // CRUD webhook
  "bots:receive",      // bot 接收事件
]
```

每个 PAT 持有 `Vec<Scope>`，handler 层 middleare `require_scope!("messages:write")` 在 `assert_room_access` 之后做二次检查。这恰好复用 Axum 的 `FromRequestParts` 机制——当前 `AuthUser` extractor 可以轻松扩展返回 `ScopedUser`。

### 补充 4：CSRF——你的评估偏轻

你说：

> 「Bearer auth 部分缓解了传统 CSRF」

这个不完全对。如果用户同时用 cookie-based session（比如未来加了管理控制台 web UI）和 PAT bearer 在第三方集成上，CSRF 的威胁模型就成立了。目前确实只有 Bearer，但**没有 SameSite cookie 策略、没有 Origin 头校验、没有 CSRF token**——一旦某个路由被配置成接受 cookie auth（比如管理面板），CSRF 就变成真实威胁。可以考虑现在就把 `SameSite=Lax` 和 `Origin` 校验加上去，是几分钟的事。

---

## 方向四（数据层韧性）—— 分析到位，补充一个被低估的点

你说：

> 「pgvector HNSW 索引质量下降」

这个非常正确且**被大多数人低估**。pgvector HNSW 在大量 INSERT/UPDATE 后 recall 确实会退化，而且**没有内置机制告诉你 recall 跌了**。补充建议：

```
1. 定期 pgvector 索引健康检查：
   SELECT * FROM check_index('message_embeddings_idx');
   -- 对比 recall @10 vs baseline，偏差 > 5% → 告警 → REINDEX CONCURRENTLY

2. 更重要的：ai_jobs 表里有 embeds 生成时间戳，
   可以按时间窗口监控 embedding 插入速率，预测 HNSW 退化窗口
```

另外你**没说但值得提**的是 `connection::invalidate` 模式。当前代码里 `participant_cache.invalidate` 是缓存一致性的关键路径。如果加了读写分离 + PgBouncer，需要确保写后读的一致性——`send_message → list_since` 路径当前在同一连接上保证，跨副本时需要 `SET transaction_read_only = off` 或 `SELECT ... FOR SHARE` 确保读到自己的写。

---

## 方向五（第三方应用平台）—— 最完整的一个方向分析

基本没什么要纠正的。我说几个补充的**代码层面的好消息**，让这个方向的成本估算更精确：

### 好消息：大量基础设施已就位

| 你列的需要建的东西 | 已有 scaffolding |
|---|---|
| `apps` 表 | 复用 `bot_registry` 模式 + `pat.tokens` 表结构 |
| OAuth token 颁发 | 复用 `pat.rs` 的 `hash_token` + `generate_token` 静态方法 |
| webhook 签名 | `bot_dispatch.rs:build_delivery` **已经接收** `secret: &str` 参数，只是调用处传了 `""`——实装配 HMAC-SHA256 签名是加几行的事 |
| retry/DLQ | `bot_dispatch.rs` 走的 `webhook_delivery` 已有 backoff + DLQ |
| per-app rate limit | `ws_rate.rs` 的 Redis 窗口支持任意 key namespace |

你的估算 8-12 周可能**偏保守**——如果复用既有基础设施，MVP 可以在 4-6 周完成（1 周 apps 表 + 2 周 OAuth 端点 + 1 周 bot 签名 + 1 周配额）。

### 一个你漏掉的致命 gap

> **Bot 安装的权限授予机制**

OAuth flow 授权的是**用户代理身份**（user acts as app），但 Slack/GitHub Apps 模型需要的是**工作区级安装**（app acts on behalf of workspace）。这是两种不同授权模型：

```
User token (OAuth):   app 以用户身份在房间发消息
Workspace install:    app 注册事件订阅、监听所有房间消息、不用用户在场

当前 bot_subscription 模型 = workspace install 模式，
但没有 workspace install 的授权 UI（用户在工作区设置页点「安装」→ scope 确认 → 
subscription 创建 → workspace 级 bot token 返回）
```

这个 gap 让 OAuth + bot 订阅无法对齐——OAuth token 和 bot subscription 的权限模型需要统一。建议 MVP 只做 workspace install 路径（跳过 OAuth），把 OAuth 留给 V2。

---

## 总结：我的优先级排序 vs 你的

```
你的排序：           我的调整：
P0 配额              P0 安全（WS token + SAML + PAT scope）
P0 安全              P1 配额（绑定价需要时才做）
P1 生命周期          P1 数据层 + 统一信封（h1 可并行）
P1 数据层            P2 应用平台（先等有外部开发者）
P1 应用平台          P2 API 版本化（等有外部消费者）
```

核心分歧是：**单节点部署阶段，安全漏洞是实的，配额超限是虚的**。你每个方向分析本身都非常棒，但我建议安全方向提到最高优先级，配额方向可以等定价/计费需求落地时再启动。

---

要我帮你把这份文档转成 `docs/requirements/` 下的 markdown 文件，或者针对某个方向展开更细的实现方案/代码级迁移计划吗？

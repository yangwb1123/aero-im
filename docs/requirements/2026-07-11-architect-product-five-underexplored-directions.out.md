现在我已经完成了全面的交叉验证。以下是对该文档的逐项审查：

---

## 文档审查：5 个高价值方向

日期：2026-07-12 | 对照 `master` 分支验证

---

### 方向 1：WS 空闲超时与资源泄露（P0）

**判定：确认并强化建议**

`run_socket` 主循环（`ws/ws_impl/mod.rs:336`）确实没有空闲超时：
```rust
loop {
    tokio::select! {
        () = close.cancelled() => break,
        incoming = receiver.next() => {
            // 没有 timeout 包裹
```

- ✅ 无 `AERO_WS_IDLE_TIMEOUT_SECS` 配置项
- ✅ 无最后活跃时间戳追踪
- ✅ 应用层 ping（`ws.js` 每 25 秒发送）的回复处理了，但从未更新任何最后活跃标记
- ✅ 建议的修复方案（在 select 中加入 timeout + `ws.js` 侧 `lastActivity` + 指标暴露）与该架构完全吻合

**附加发现：** `hub.rs:114` 中的 `DashMap<ParticipantId, Vec<WsSender>>` 会因为僵尸连接而产生脏条目。`register()` 会追加到 `Vec`；`unregister()` 需要精确匹配的 `WsSender` 才能移除。如果驱逐逻辑存在 bug，连接泄露会导致 Vec 无限增长。

---

### 方向 2：注册防护与滥用治理（P0）

**判定：核心结论有效，但速率限制指控有误**

**❌ 不准确声明：** "注册请求不受任何速率限制保护"

实际结果：`/api/auth/register` **确实**有速率限制。`rate_limit::layer` 中间件（`serve.rs:98` 通过 `from_fn_with_state` 挂载）在 `rate_limit.rs:96` 将 `register` 归类到 `SENSITIVE_AUTH_PATHS`，并使用 `auth_rate_limiter`（默认 3 req/s，突发 5）。以下是路由分发：

```rust
// rate_limit.rs:92-96
let limiter = match request.uri().path() {
    "/api/auth/login" => &state.login_rate_limiter,
    "/api/auth/forgot-password" => &state.forgot_rate_limiter,
    p if SENSITIVE_AUTH_PATHS.contains(&p) => &state.auth_rate_limiter,
    _ => &state.rate_limiter,
};
```

其中 `register` 包含在 `SENSITIVE_AUTH_PATHS` 中（第 75 行）。而且还通过 Redis INCR 实现了集群级速率限制（`check_cluster_rate`，第 135 行）。

**✅ 准确声明：**

| 声明 | 验证结果 |
|-------|---------|
| 无人机验证（CAPTCHA） | ✅ 确认——没有 Turnstile/reCAPTCHA/hCaptcha 的 code/config |
| 无邮箱验证 | ✅ 确认——`participants` 表中没有 `email_verified_at` 列 |
| 无邀请码要求 | ✅ 确认——`RegisterReq` 没有 `invite_code` 字段 |
| `signup_policy` 配置不存在 | ✅ 确认——没有这样的配置项 |

**额外细节：** 注册后自动将新用户加入默认工作区（`routes.rs:781`，`add_member` with `ON CONFLICT DO NOTHING`）——注册限流还附加了一层 DoS 防护，防止工作区成员关系表爆炸。该文档正确指出了邀请制模式应利用已有的 `invitations.rs`（迁移 0017）。

---

### 方向 3：存储配额与运营治理（P1）

**判定：确认并补充发现**

该文档在存储配额缺失方面准确无误。`blob_upload` 处理器（`routes.rs:1623`）验证：
- 单 blob 大小限制（`MAX_BLOB_BYTES`，50MB）
- MIME 类型 + content-sniffing + ClamAV
- 通过 SHA256 实现所有者级别去重
- 工作区速率限制（`check_ws_rate_participant`）

但从未检查累计存储用量。

**补充发现：**
1. `BlobRepo`（`storage/src/blob.rs`）暴露了 `count_by_owner`——因此按用户聚合在 SQL 层面是可行的，但未被调用
2. 已存在一个 `blob_gc_drain` 定时器（`boot/background.rs`，固定 60 秒间隔），负责清理已解除引用的 blob——配额子系统可以重用相同的 `BlobRepo::delete_unlinked`
3. 该文档关于 `workspace_quota` 表设计以及用 `blobs.deleted`（软删）而非磁盘上文件大小来计算配额的提示是正确的

---

### 方向 4：进程崩溃恢复（P1）

**判定：多处实质性不准确定，但核心洞察仍然有效**

**❌ 不准确声明 #1：** `CallOrchestrator` 包含 `active: DashMap<CallId, ActiveCall>`

实际结构（`aero-im-call/src/lib.rs:200`）：
```rust
pub struct CallOrchestrator {
    calls: CallRepo,       // ✅ 数据库持久化
    sfu: Option<SfuRouter>,
    routes: Option<CallRoutes>,
}
```

没有 `DashMap`。通话状态写入 Postgres 中的 `call_sessions` 表（迁移 0002，`call_sessions` 和 `call_participants`）。通话结束时通过 `end_call` 打上 `ended_at` 时间戳。因此，**崩溃恢复的这个问题，其基本前提是不正确的**——通话数据库记录在进程重启后仍然存在。

**❌ 不准确声明 #2：** `LiveSession` 存储在 `process-local HashMap<StreamId, LiveSession>` 中

实际的 `LiveService`（`live.rs:29`）不包含任何会话映射——它只是一个封装了存储库 + 总线的门面类。直播推流会话由底层的 WHIP/RTMP 摄入处理器管理，不在这个特定的模块中。

**✅ 准确声明：**

| 组件 | 状态 | 验证 |
|-------|--------|------|
| `SfuRouter`（`aero-live-webrtc`） | 进程内存 ↔ 无持久化 | ✅ `Arc<RwLock<HashMap<CallId, CallState>>>` |
| Hub 连接注册表 | 进程内存 ↔ DashMap | ✅ `conns: DashMap<ParticipantId, Vec<WsSender>>` |
| 流摄入会话（RTMP/WHIP） | 进程内存 | ✅ 体现在每个 ingest-handler task 中 |

**核心洞察仍然有效：** SFU 路由器和 Hub 连接注册表是脆弱的进程内存状态。但修复建议应聚焦于：
1. **SfuRouter 崩溃 →** 对端 web 客户端在 ICE 超时（~30 秒）后察觉，然后应该发起 `call_end`
2. **Hub 恢复 →** 客户端已经通过 `?since=` 回填实现了自动重连 + 重放
3. **滚动更新 →** `serve.rs:190` 通过 `with_graceful_shutdown` 实现了优雅关闭——但缺少一个可配置的 `AERO_DRAIN_WAIT_SECS`

该文档关于 30 秒 UX 空白和通话中断的分析是准确的，但需要更新代码证据以反映真实的架构。

---

### 方向 5：API 开发者体验与测试（P2）

**判定：多处不准确定，但核心痛点成立**

**❌ 不准确声明 #1：** "OpenAPI 规范生成器（openapi.rs）存在但未被挂载"

实际结果：它**已**被挂载。`routes.rs:490-492`：
```rust
// GET /api/openapi.json 返回静态 OpenAPI 文档，以便 API 客户端
// 和文档生成器无需凭据即可访问。
.merge(crate::openapi::router());
```

并且 `openapi.rs` 在第 38 行定义了 `GET /api/openapi.json` 路由。即使规范是手动维护且不完整的（如该文档正确指出的那样），端点本身确实可用。

**❌ 不准确声明 #2：** "无 X-RateLimit-Remaining / Retry-After 头"

`rate_limit.rs:182-194` 显示了完整的头注入：
```rust
fn attach_rate_limit_headers(headers: &mut HeaderMap, status: &RateLimitStatus) {
    for (name, val) in [
        ("x-ratelimit-limit", status.limit),
        ("x-ratelimit-remaining", status.remaining),
        ("x-ratelimit-reset", status.reset_secs),
    ] { ... }
    // 拒绝时还设置了 "retry-after"
}
```

`RateLimitStatus` 结构体（第 46-56 行）明确包含 `limit`、`remaining`、`reset_secs` 字段。这些头在速率限制中间件的允许路径和拒绝路径中都被注入。

**✅ 准确声明：**

| 声明 | 验证 |
|-------|--------|
| Web SPA 零测试 | ✅ `web/` 中没有 `.test.js` 或 `.spec.js` 文件 |
| 无 API 版本前缀 | ✅ 所有路由都是裸 `/api/...` |
| 无 TypeScript SDK | ✅ 没有 TS 类型文件 |
| `api.js` 手动映射（57 个方法） | ✅ `api.js` 行数较多，未经 OpenAPI 代码生成 |

**关于 Swagger UI 的说明：** 该文档说 "只需要一行挂载即可暴露 GET /openapi.json"——但这一行已经存在。剩下的工作是（a）补充规范的完整程度，以及（b）在前端添加一个 Swagger UI 包装器。

---

## 修正事实后的优先级重排

| 方向 | 优先级 | 文档准确性 | 仍然有效的风险 |
|------|----------|-------------|----------------|
| ① WS 空闲超时 | **P0** | ✅ 证据充分 | 内存泄露、抗 DoS |
| ② 注册防护 | **P0** | ⚠️ 速率限制声明有误，但 CAPTCHA/邮件验证/邀请制是有效的缺口 | DoS、垃圾消息、AI 成本耗尽 |
| ③ 存储配额 | **P1** | ✅ 证据充分 | 磁盘 DoS、S3 成本 |
| ④ 崩溃恢复 | **P1** | ❌ 基础前提有误（CallOrchestrator 已持久化）；SFU 路由器问题有效 | 通话 UX 空白、滚动更新 |
| ⑤ API DX | **P2** | ❌ OpenAPI 已挂载 + 限流头已实现；测试缺失 + 版本化 + SDK 缺失有效 | 开发者入职、契约偏差 |

**建议：** 用你拥有的最新代码重新扫描代码库（看起来该文档是在过时的代码库版本上编写的）。OpenAPI 挂载和限流头已经被实现，方向 4 对通话编排器架构的假设不准确。然而，核心产品洞察（WS 空闲超时、注册防护、存储配额、SfuRouter 崩溃风险、E2E 测试覆盖）一旦去除过时的理解，仍然提供了宝贵的指导。

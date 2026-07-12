# 架构分析报告：Aero IM

> 基于交叉验证文档（2026-07-12）与 `master` 分支实况对照的架构级分析
> 角色：架构师 | 范围：全系统骨架 + 5 个高价值方向

---

## 1. 架构评估

### 1.1 当前架构的核心优势

**事件驱动 DAG 设计合理、成熟：**

| 层级 | 决策 | 评价 |
|------|------|------|
| 跨实例事实源 | NATS JetStream durable/ephemeral consumer | ✅ 区分耐久与尽力投递（`im.room.*` durable vs `live.stream.*` ephemeral）是正确的业务权衡 |
| 进程内扇出 | Hub::fan_out_raw bounded mpsc | ✅ 天然背压 + 水平扩展无共享状态 |
| 集群状态 | Redis sorted-set（presence/roster/viewer） | ✅ 正确地将易失状态与持久状态分离；TTL 过期自动清理 |
| 持久状态 | Postgres + sqlx | ✅ 迁移编译期嵌入、幂等 DDL、仓储模式清晰 |
| Crate 分层 | 自下而上的依赖 DAG（common → bus/storage → im-core → server） | ✅ 无循环依赖；新领域开新 crate 而非膨胀 `src/` |
| AI 适配器 | HashEmbedder 退化 + 预算控制 + 有界 worker | ✅ 功能性退化优先于熔断；预算防止成本失控 |

**关键设计决策合理：**

- **`assert_room_access(participant, room)` 统一守卫**：将 workspace 解析、membership、停用门、2FA 门整合到一个调用点，而非散布在各 handler——这是正确的「门面守卫」模式。
- **迁移编译期嵌入**：避免了运行时迁移版本漂移，代价是改迁移后必须 re-build。
- **MLS 仅 scaffold 不实现**：正确地将客户端密码学边界保持在 server 之外。
- **有界队列 + Skip（moderation_bot）**：宁可丢事件也不背压 WebSocket，符合实时优先级。

### 1.2 架构债务与技术债

| 债务类型 | 位置 | 严重程度 | 说明 |
|----------|------|----------|------|
| **脆弱进程状态** | Hub 连接注册表（`DashMap<ParticipantId, Vec<WsSender>>`） | **P0** | 崩溃后所有 WS 连接丢失，客户端虽能重连但中间有 UX 空白；`Vec` 随僵尸连接无限增长→内存泄露 |
| **脆弱进程状态** | SfuRouter（`Arc<RwLock<HashMap<CallId, CallState>>>`） | **P0** | 崩溃后通话的 SFU 路由状态完全丢失；对端需 ICE 超时（~30 秒）才察觉 |
| **WS 资源泄露** | `run_socket` 主循环无空闲超时 | **P0** | 僵尸 WebSocket 不被清理，`DashMap` 脏条目累积 |
| **注册安全缺口** | 无 CAPTCHA/邮箱验证/邀请码 | **P0** | 虽有限流兜底，但缺乏人机验证和身份验证，存在枚举攻击面 |
| **存储配额缺失** | `blob_upload` 不检查累计用量 | **P1** | 单次 50MB 限制足够，但累计无上限→磁盘/S3 成本 DoS |
| **API 版本化缺失** | 所有路由裸 `/api/...` | **P2** | 无法优雅地做 breaking change；OpenAPI 手写会与实现漂移 |
| **零 E2E 测试** | `web/` 无 `.test.js`/`.spec.js` | **P2** | 前端逻辑仅靠 eslint 和手动测试保障 |
| **enum `kind` 标签撞名** | serde `tag="kind"` 的 enum variant 含 `kind` 字段 | **已规避** | 用 `#[serde(rename=...)]` 解决，但新 variant 容易踩坑 |
| **token helper 同名** | `generate_token`/`hash_token` 多模块冲突 | **已规避** | 只 re-export webhook 的，其余走子路径；新模块容易忽略约定 |

### 1.3 架构层面最值得关注的信号

> 按风险从高到低

1. **进程内存状态没有恢复契约** : Hub 连接注册表、SfuRouter、摄入会话管理器都是 `Arc<RwLock<HashMap>>` 或 DashMap，崩溃后全部丢失。虽然客户端重连和 ICE 超时能最终恢复，但 30 秒 UX 空白 + 中间状态不一致是已知问题。
2. **WS 资源生命周期管理不完整** : `register()` 追加，`unregister()` 匹配删除——没有兜底驱逐机制。一个 bug 的 `unregister` 调用（例如 `WsSender` 比较失败）会永久泄露。
3. **注册流程缺少「门」的层次** : 限流是 DoS 防护，不是身份验证。缺少验证邮箱/CAPTCHA/邀请码意味着任何人都可以程序化批量注册。
4. **配额系统完全缺失** : 在 SaaS 产品中，存储配额是运营的基本要求。当前架构虽暴露了 `count_by_owner`，但没有调用方。

---

## 2. 扩展方向

### 方向 A：连接生命周期管理与优雅降级基础设施（P0）

**为什么需要：**
- 当前 WS 连接无限期存活，僵尸连接泄露内存
- 进程崩溃导致所有 Hub 注册丢失，客户端需自行恢复
- 滚动更新时缺乏可配置的 drain 等待期

**核心挑战和技术难点：**
1. **空闲超时机制**：需要区分「用户静默但活跃的 WS」（可能正在等待通知）与「真僵尸连接」。仅靠应用层 ping 不足——ping 证明 TCP 通，不代表上层逻辑活跃。
2. **优雅驱逐 vs 暴力断连**：超时后应先发 `ServerFrame::IdleTimeout` 让客户端有机会重连保留 `?since=` 游标，而非直接断连。
3. **进程恢复后连接注册表重建**：Hub 进程重启后，DashMap 为空。需要确保客户端重连路径能恢复一切（`?since=` 重放做了，但 `call_rosters` 和 `stream_watchers` 呢？）
4. **Drain 期间的新事件缓冲**：`with_graceful_shutdown` 停止了新请求，但已经在处理中的事件需要 drain。NATS consumer 需要妥善处理 ack/nack。

**预期的架构变更：**

```
当前：
  run_socket → loop { select { close, incoming } }  // 无超时

建议：
  run_socket → loop {
      select {
          close,
          incoming → update_last_active,
          idle_timeout → send_idle_frame → close,
      }
  }
  register() + unregister() → 统一 ConnectionRegistry 管理生命周期
  ConnectionRegistry 暴露活跃连接数作为指标
```

- 引入 `ConnectionRegistry` 结构体，封装 `DashMap` + 最后活跃时间 + 空闲驱逐逻辑
- `register()` 返回 `ConnectionHandle`（而非直接操作 Vec），`drop` 时自动注销
- 配置项 `AERO_WS_IDLE_TIMEOUT_SECS`（默认 5 分钟？）
- 指标：`ws_connections_active`、`ws_idle_timeouts_total`

**对现有系统的影响：**
- 影响面中等：`ws/ws_impl/` 的 `mod.rs`（run_socket）和 `hub.rs`（注册/注销）需要改动
- 向后兼容：新 config 默认 0 表示不启用，现有行为不变
- web 端需要增加 `ServerFrame::IdleTimeout` 处理，重连即可

**备选方案 A1：应用层心跳驱动**
- web 端每 25 秒发 ping，server 在 ping 处理后更新 `last_active`
- 简化：server 不需要额外 timer，只需在 ping handler 里更新 timestamp
- 代价：僵尸连接在应用层 keepalive 周期内仍存在（25 秒），但比无限制好

**备选方案 A2：TCP keepalive + 内核级超时**
- `tokio::net::TcpStream::set_keepalive` 设置 `tcp_keepalive_time`
- 优点：无需应用层改动，内核处理
- 缺点：粗粒度（分钟级），无法与业务逻辑交互（发 IdleTimeout 帧）

**推荐：A1 + A2 组合**——TCP keepalive 兜底 + 应用层 5 分钟无任何帧则发 IdleTimeout。

---

### 方向 B：注册安全与身份验证门（P0）

**为什么需要：**
- 当前注册流程：邮箱 + 密码 → 直接创建用户 → 加入默认 workspace
- 缺少人机验证（CAPTCHA）、邮箱所有权验证、邀请码
- 攻击者可以：批量注册消耗 AI 预算、撑爆 workspace 成员表、枚举有效邮箱

**核心挑战和技术难点：**
1. **CAPTCHA 服务选择**：需要权衡隐私（Turnstile）vs 准确度（reCAPTCHA v3），且需要 server 端验证 token
2. **邮箱验证流程**：需要发送验证邮件 → 存储 token → 用户点击链接 → 标记验证 → 允许登录。引入异步流程（事务性邮件服务 + 临时 token 存储）。
3. **邀请码系统集成**：`invitations.rs` 已有仓储，但需接入注册流——`RegisterReq.invite_code` 字段校验。
4. **配置化策略**：不是所有部署都需要所有门。需要 `signup_policy` 配置控制门槛高低（`open` / `email_verified` / `invite_only`）。

**预期的架构变更：**

```
注册流变更（当前）：
  POST /api/auth/register → validate → insert → add_to_default_workspace → return token

注册流变更（建议）：
  POST /api/auth/register → 
    1. verify_captcha(token)  // if signup_policy requires
    2. validate → insert (email_verified_at = NULL) 
    3. generate_verification_token → send_email
    4. return "please verify email"
  
  GET /api/auth/verify-email?token=... →
    1. validate token → set email_verified_at = NOW()
    2. (可选) auto-login or redirect to login
  
  POST /api/auth/login →
    1. validate credentials
    2. if signup_policy requires email_verified AND email_verified_at IS NULL → 403 "verify your email"
```

- 新增枚举 `SignupPolicy { Open, EmailVerified, InviteOnly }`
- `invitations.rs` 的 `verify_invite_code` 接入注册 handler
- 新增 `email_verifications` 表（token hash + expiry + participant_id）
- 事务性邮件服务 seam：`trait EmailGateway { send_verification_email }`，可注入 `FakeEmailGateway`

**对现有系统的影响：**
- 影响中等偏大：注册 handler 重构、login handler 加验证门
- 需新增 migration：`participants.email_verified_at` 列 + `email_verifications` 表
- 向后兼容：`signup_policy` 默认 `open`，行为与当前一致
- 不引入新外部依赖（除 CAPTCHA 服务端验证库外）

**关键权衡：CAPTCHA 选型**

| 选项 | 隐私 | 免费额度 | 易用性 |
|------|------|----------|--------|
| Cloudflare Turnstile | ✅ 最佳（无 cookie） | ✅ 完全免费 | ✅ 一行代码 |
| Google reCAPTCHA v3 | ❌ 隐私担忧 | ✅ 免费（10M/月） | ✅ 广泛支持 |
| hCaptcha | ⚠️ 中等 | ✅ 免费 | ⚠️ 稍有摩擦 |

**建议：Turnstile 作为首选集成，reCAPTCHA 作为备选（可配置）。**

---

### 方向 C：运营治理与配额系统（P1）

**为什么需要：**
- 累计上传无限制 → 磁盘/S3 成本不可控
- 缺乏工作区级/用户级配额管理 → SaaS 运营基本功能缺失
- 已有基础设施（`BlobRepo::count_by_owner`、`blob_gc_drain`）但未连接

**核心挑战和技术难点：**
1. **配额定在哪个粒度**：用户级 vs 工作区级 vs 全局。文档提示 `workspace_quota` 表设计合理。
2. **怎么算使用量**：按 blob 磁盘大小（bytes）还是计数？软删 blob 是否计入？
3. **配额检查时机**：上传前检查 vs 上传后校验 → 前者防浪费（上传大文件后才发现超配额），后者实现简单。
4. **超额处理**：拒绝新上传还是触发告警？SaaS 通常软硬配额（warning at 80%, hard block at 100%）。
5. **配额重置周期**：按月/按总用量？需要可配置。

**预期的架构变更：**

```
新增表：
  workspace_quotas (
    workspace_id UUID PK → workspaces,
    max_bytes BIGINT NOT NULL,     // 0 = unlimited
    max_blobs INTEGER NOT NULL,    // 0 = unlimited
    storage_class TEXT DEFAULT 'standard'
  )

 使用量计算（物化视图或实时聚合）：
  CREATE MATERIALIZED VIEW workspace_storage_usage AS
  SELECT workspace_id, 
         SUM(file_size) as used_bytes,
         COUNT(*) as used_blobs
  FROM blobs WHERE NOT deleted
  GROUP BY workspace_id;
  -- 或实时查询（blobs 表不大时）

 上传拦截：
  blob_upload handler → 
    1. check single blob size (已有)
    2. check workspace_quota: 
       IF usage >= quota → 413 Payload Too Large with "storage quota exceeded"
    3. accept file → persist → update usage cache
```

- `BlobRepo` 新增 `check_storage_quota(participant, room) -> Result<(), QuotaExceeded>`
- `redis` 缓存当前用量（避免每次上传都扫 pg 求和）
- 配置项 `AERO_STORAGE_QUOTA_ENABLED`（默认 false，启用以免旧数据触发 quota）
- 可选的定时任务：生成用量报告（`scripts/quota_report.sh` 或 server 内置）

**对现有系统的影响：**
- 影响较小：`blob_upload` handler 加一个检查调用
- 新 migration：`workspace_quotas` 表
- 向后兼容：默认不禁用，新行为 opt-in
- `blob_gc_drain` 定时器可以复用做配额使用量更新触发器

---

### 方向 D：进程恢复能力与有状态服务治理（P1）

**为什么需要：**
- SfuRouter 和 Hub 连接注册表在崩溃后完全丢失
- 当前 30 秒 UX 空白（ICE 超时/重连）可优化
- 滚动更新时无 drain 等待期配置

**核心挑战和技术难点：**
1. **SfuRouter 状态恢复**：通话的 SFU 路由表（CallId → Subscriber list）是纯运行时的。崩溃后，所有通话参与者需要重新协商 SDP。
2. **优雅降级 vs 优雅恢复**：有些状态不应该持久化（弹幕丢失是可接受的），有些需要（正在进行的通话）。
3. **Drain 超时配置**：`serve.rs:190` 已有 `with_graceful_shutdown`，需加上 `AERO_DRAIN_WAIT_SECS`（当前硬编码或默认 30 秒）。

**预期的架构变更：**

```
选项 1（低投入）：优化客户端恢复速度
  - 减少 ICE 超时检测（20s → 10s）
  - 重连后自动发起 call_rejoin 恢复 SFU 订阅
  - 代价：通话短暂中断不可避免

选项 2（中投入）：SfuRouter 定期快照到 Redis
  - 每 5 秒 `SET sfu_router:{call_id}` 存 subscriber list
  - 崩溃后从 Redis 恢复 SfuRouter 状态
  - 代价：增加 Redis 写入负载，不一致窗口 5 秒

选项 3（高投入）：NATS-backed SfuRouter
  - SfuRouter 状态的每次变更发布 NATS 事件
  - 恢复时重放事件构建状态
  - 代价：增加 NATS subject、事件序列、幂等消费
```

**建议：** 选项 1 作为 P0 立即实施，选项 2 作为 P1 后续迭代。通话中断不可避免，重点是加速恢复。

- 新增配置 `AERO_DRAIN_WAIT_SECS`（默认 30）
- Hub 注册表增加「重连指纹」：客户端重连时携带 `last_conn_id`，若 `last_conn_id` 仍在注册表中，直接复用而非全量重放
- SfuRouter 增加 `session_recovery` 端点：重连客户端可以查询哪些通话仍活跃

**对现有系统的影响：**
- 影响中等：Hub.register/unregister 加 conn_id；SfuRouter 加快照逻辑
- 通话模块需新增 `call_rejoin` 帧
- web 端需新增 `CallFrame::Rejoin` 处理

---

### 方向 E：API 平台化与开发者体验（P2）

**为什么需要：**
- 无 API 版本前缀 → 无法做 breaking change
- OpenAPI 规范手写 → 必然与实现漂移
- 零 E2E 测试 → 前端逻辑仅靠 eslint
- 无 SDK → 集成者需要手搓 HTTP 请求

**核心挑战和技术难点：**
1. **版本化策略选择**：URL prefix（`/api/v1/...`）最直观但需要路由层重构；Header-based（`Accept: application/vnd.aero.v1+json`）灵活但不够显眼。
2. **OpenAPI 生成 vs 手写**：手写会漂移；代码生成需要类型注解宏（如 `utoipa`、`okapi`）或全量重构。
3. **E2E 测试基础设施**：需要可重复的测试环境（临时数据库 + NATS + Redis）、测试夹具（fixtures）、测后清理。

**预期的架构变更：**

```
版本化（建议 URL prefix）：
  当前：/api/rooms/:id/messages
  建议：/api/v1/rooms/:id/messages
  
  内部路由：
    routes()
      .nest("/api/v1", v1_routes())
      .nest("/api/v2", v2_routes())

OpenAPI 改进：
  选项 A：切换到 utoipa（注解式）—— 逐步为关键端点加注解
  选项 B：保持手写 + 添加 CI 验证（diff diff）
  建议：A 为主，B 兜底

前端测试基建：
  web/tests/
    ├── integration/    (Playwright/Puppeteer, 端到端)
    ├── unit/           (Jest/Vitest, 纯 JS 逻辑)
    └── fixtures/       (测试数据)
  CI 中 `make test-web`
```

**对现有系统的影响：**
- 版本化影响大：所有 API 路由 prefix 改动，但可做重定向（`/api/*` → `/api/v1/*`）
- OpenAPI 影响中等：utipa 集成需排期，但可逐步替换手写端
- E2E 测试影响小：不影响生产代码，仅开发流程变化

---

## 3. 接口设计建议

### 3.1 连接注册表接口

当前 `hub.rs` 的 DashMap 操作暴露为 `pub fn register/unregister` 函数。建议封装为 `ConnectionRegistry`：

```rust
// 当前（散布 + 容易用错）：
let sender = /* WsSender */;
hub.conns.entry(pid).or_default().push(sender);
// 后面需要精确匹配才能移除

// 建议（RAII 句柄）：
let handle: ConnectionHandle = registry.register(pid, sender).await;
// ConnectionHandle drop 时自动注销
// handle 包含最后活跃时间、创建时间、关联的 pid
```

**接口原则：**
- RAII 管理生命周期（`ConnectionHandle` 析构时自动 `unregister`）
- 暴露指标：`active_connections() -> usize`、`connections_by_participant(pid) -> usize`
- 空闲驱逐逻辑内聚：`fn sweep_idle(timeout: Duration) -> Vec<ParticipantId>`

### 3.2 存储配额检查接口

```rust
// 当前：BlobRepo 有 count_by_owner 但无人调用
// 建议：
pub trait QuotaEnforcer: Send + Sync {
    /// 检查上传是否在配额内。返回 Err 说明超额。
    async fn check_upload_quota(
        &self, 
        owner: ParticipantId, 
        workspace: WorkspaceId,
        file_size: u64,
    ) -> Result<(), QuotaExceeded>;
    
    /// 记录用量（上传成功后调用）
    async fn record_usage(
        &self, 
        owner: ParticipantId, 
        workspace: WorkspaceId, 
        file_size: u64,
    ) -> Result<(), StorageError>;
}
```

**设计原则：**
- trait 化：可测试（`MockQuotaEnforcer`）、可替换（Redis-backed vs PG-backed）
- 检查与记录分离：防止 TOCTOU（检查通过后、记录前并发上传）
- 配额检查在 `blob_upload` handler 的最前端，避免浪费 I/O

### 3.3 注册门接口

```rust
pub enum SignupPolicy {
    Open,
    EmailVerified,
    InviteOnly,
}

#[async_trait]
pub trait RegistrationGate: Send + Sync {
    async fn verify_captcha(&self, token: &str) -> Result<(), GateError>;
    async fn verify_invite(&self, code: &str) -> Result<(), GateError>;
    // email verification 走独立的 send_verify_email + verify_email_token 流程
}
```

**设计原则：**
- Policy enum 分离配置与实现
- 每个 Gate 独立 trait，方便 mock 和可配置组合
- CAPTCHA provider 可替换（Turnstile / reCAPTCHA / none）

### 3.4 API 版本化接口

```rust
// 建议方案：使用 axum 的 nest，内部转发到 v1
fn routes() -> Router<AppState> {
    Router::new()
        // 原来 /api/* 的挂载点
        .nest("/api/v1", v1::routes())
        // 向后兼容重定向（可选的，过渡期后移除）
        .fallback(redirect_v1::handler)
}
```

**设计原则：**
- URL prefix 版本化（最直观、最易调试）
- `v1::routes()` 保持与当前 `/api/*` 一致的行为（加 prefix 后内部路由不变）
- 过渡期 `Redirect` 老路径到 `/api/v1/...`，客户端逐步迁移
- 废弃端点通过 `deprecated` 头标记（`Sunset: Sat, 01 Jan 2027 00:00:00 GMT`）

### 3.5 是否需要新的抽象层

**需要引入的抽象层：**

| 抽象层 | 原因 | 现有替代 |
|--------|------|----------|
| `ConnectionRegistry` | 封装 WS 连接生命周期管理 | 散布在 Hub 的 DashMap 操作 |
| `QuotaEnforcer` trait | 存储配额检查可测试、可替换 | 无 |
| `EmailGateway` trait | 事务性邮件服务可替换、可 mock | 无（FakeGateway 在 push 中有类似模式） |
| `CaptchaVerifier` trait | CAPTCHA provider 可替换 | 无 |

**不需要的抽象层：**
- 不引入「通用 Pub/Sub 抽象」：NATS 已经足够，再加一层只是增加复杂度
- 不引入「通用 KV 存储抽象」：Redis 和 PG 各司其职，强行统一得不偿失

---

## 4. 技术选型

### 4.1 需要引入的新技术

| 组件 | 技术选项 | 推荐 | 原因 |
|------|----------|------|------|
| CAPTCHA | Turnstile / reCAPTCHA / hCaptcha | **Turnstile** | 隐私友好、免费、无 cookie、server-side verify 简单 |
| 事务性邮件 | Resend / SendGrid / Mailgun / SES | **Resend**（轻量）或 **SES**（自有 AWS） | Rust SDK 可用；Resend API 最简洁；SES 成本最低 |
| OpenAPI 工具 | utoipa / okapi / paperclip | **utoipa** | 最活跃的 Rust OpenAPI 生态，与 axum 集成好，支持宏注解 |
| 前端测试 | Playwright / Puppeteer / Vitest | **Playwright**（e2e）+ **Vitest**（unit） | Playwright 跨浏览器、速度好；Vitest 与 Vite 原生集成 |
| CI 验证 OpenAPI | opt-deploy-diff / spectral | **Spectral** | lint OpenAPI 规范的质量 + breaking change 检测 |

### 4.2 自建 vs 采购决策

| 功能 | 建议 | 依据 |
|------|------|------|
| CAPTCHA 验证 | **集成第三方** | 自建 CAPTCHA 需要 AI 图像识别/行为分析——这是独立的 ML 领域，不值 |
| 事务性邮件 | **集成第三方** | 邮件投递可靠性和 SPF/DKIM/DMARC 配置复杂，不值得自建 SMTP |
| OpenAPI 规范 | **自建微框架（utoipa）** | 已在 Rust 生态内，宏注解成本低，数据模型与代码共存 |
| E2E 测试框架 | **用标准工具（Playwright）** | 不发明新 dsl，直接写 JS/TS 测试 |
| 配额系统 | **自建** | 已在存储层内，不需要单独服务；用现有 `BlobRepo` + Redis 缓存 |

### 4.3 第三方依赖评估标准

```
评估维度（按优先级）：
1. 安全性 —— Rust 生态是否有已知 CVE？维护频率？
2. 许可证 —— MIT/Apache 2.0 首选，GPL 类避免
3. 维护活跃度 —— 最近 commit/issue 响应/MATS
4. MSRV 兼容 —— 项目 MSRV 1.80，所选依赖需 >=1.70
5. 依赖树大小 —— 避免引入重框架（如添加 actix 作为第二个 web 框架）
6. 与现有栈兼容 —— 复用了什么？tokio/axum/sqlx？不引入冲突的运行时
```

---

## 5. 实施路线图

### 5.1 优先级排序

| 优先级 | 方向 | 理由 |
|--------|------|------|
| **P0** | A. WS 空闲超时 + 连接生命周期 | 当前有内存泄露和 DoS 风险，影响所有生产部署 |
| **P0** | B. 注册安全（CAPTCHA + 邮箱验证） | 公共部署的注册滥用是高优先级安全风险 |
| **P1** | D. 进程恢复能力（SfuRouter 快照 + drain 配置） | 影响通话体验，但已有降级路径（ICE 超时重连） |
| **P1** | C. 存储配额系统 | 运营刚需，但非紧急（当前单文件限制兜底） |
| **P2** | E. API 平台化 + 测试覆盖 | 开发者体验改进，不影响运行 |

### 5.2 阶段划分

```
Phase 1（2 周）—— P0：连接安全 + 注册防护
  里程碑：配置 WS_IDLE_TIMEOUT 后僵尸连接被清理；注册 CAPTCHA 集成
  交付物：
    - ConnectionRegistry 重构
    - IdleTimeout 帧 + 客户端重连逻辑
    - CaptchaVerifier trait + Turnstile 实现
    - signup_policy 配置项（open/captcha/invite_only）
    - metrics: ws_connections_active, captcha_verifications_total

Phase 2（3 周）—— P1：运营治理 + 恢复能力
  里程碑：存储配额可配置；SfuRouter 崩溃后通话加速恢复
  交付物：
    - workspace_quotas 表 + migration
    - QuotaEnforcer trait + BlobRepo 集成
    - AERO_DRAIN_WAIT_SECS 配置 + serve.rs 集成
    - SfuRouter 5s Redis 快照（选项 2）
    - Hub 重连指纹（conn_id）

Phase 3（4 周）—— P2：API 平台化
  里程碑：API 版本化 + OpenAPI 自动化 + E2E 测试基线
  交付物：
    - /api/v1/ 路由挂载 + /api/ → /api/v1/ 重定向
    - utoipa 注解覆盖关键端点（消息、认证、直播）
    - CI 中 Spectral lint + breaking change 检测
    - Playwright E2E 测试基线（3-5 个核心场景）
    - api.js → OpenAPI Client Generator 迁移指南
```

### 5.3 风险点与缓解策略

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| ConnectionRegistry 重构影响现有 WS 稳定性 | 中 | 高 | 先加 `#[cfg(test)]` mock，行为与旧 DashMap 相同；渐进替换而非重写 |
| CAPTCHA 集成导致注册失败率上升（合法用户被拦） | 高 | 中 | signup_policy = open 兜底；Turnstile 无感验证降低摩擦 |
| 配额系统在并发上传下 checker 和 recorder 时序 | 中 | 中 | 使用 `SELECT SUM ... FOR UPDATE` 或 Redis INCR 原子检查；允许 5% 超额软限制 |
| SfuRouter Redis 快照增加延迟 | 低 | 低 | 快照异步写 Redis（不阻塞 SDP 协商），不一致窗口 < 5 秒可接受 |
| API 版本化导致现有客户端不可用 | 高（过渡期） | 高 | /api/* 作为 v1 别名保留至少一个发布周期；加 Sunset 头通知 |

### 5.4 关键决策点

**决策 1：空闲超时是否发通知帧？**

| 选项 | 优点 | 缺点 |
|------|------|------|
| 直接断连 | 简单，立即释放资源 | 合法静默用户被断，重连增加网络往返 |
| 发 IdleTimeout 帧 | 友好，客户端可静默重连 | server 需等待客户端响应（多一个 select! 分支） |
| 先发警告再断连 | 最友好 | 复杂度增加；30 秒 vs 立即 |

**建议：** 发 `ServerFrame::IdleTimeout` + 等待 5 秒再断连。客户端收到后立即重连（重用 `?since=` 游标），用户体验无感知。

**决策 2：配额按用户还是按工作区？**

| 选项 | 优点 | 缺点 |
|------|------|------|
| 按用户 | 精细控制，防止个人用户耗尽存储 | 管理成本高（每用户配置） |
| 按工作区 | SaaS 模式自然粒度（团队共享配额） | 一个用户可能消耗所有团队配额 |
| 双粒度 | 灵活 | 复杂性高；检查链长 |

**建议：** 先按工作区实现，阶段 2 添加用户级软限制（warning only）。工作区配额与 SaaS 定价模型对齐。

**决策 3：OpenAPI 自动生成 vs 手写维护？**

| 选项 | 优点 | 缺点 |
|------|------|------|
| utoipa 注解 | 规范与代码同步，type-safe | 需要逐 handler 加注解，初期工作量大 |
| 手写 + CI diff | 当前即可实施 | 必然漂移，CI diff 只能检测已有端点的变化 |
| 混合 | 关键端点注解 + 次要端点点手写 | 需要权衡哪些是「关键」 |

**建议：** 关键端点（auth、messages、rooms、streams）用 utoipa 注解；内部/管理端点保持手写。CI 中 Spectral 确保所有已注解端点与实际 API 一致。

---

## 总结

Aero IM 当前架构在核心设计决策上是正确的：事件 DAG + NATS + 分层 crate + Redis 做集群状态。主要缺失不在「做错了」，而在「没做完」：

1. **资源生命周期管理**（P0）—— WebSocket 连接无空闲驱逐。这是所有生产级实时系统的基本要求。
2. **安全注册门**（P0）—— 限流挡住了 DoS，但挡不住人机滥用。CAPTCHA + 邮箱验证是 SaaS 的基础设施。
3. **运营治理**（P1）—— 存储配额缺失。SaaS 没有配额系统等于没有计费的基础。
4. **恢复能力**（P1）—— 进程内存状态没有恢复契约。当前靠客户端重连兜底，可以更好。
5. **API 平台化**（P2）—— 开发者体验是长期壁垒。版本化、测试覆盖、SDK 才让系统成为平台。

**建议的第一行动：** 实施 Phase 1（WS 空闲超时 + 注册 CAPTCHA）。这两个方向技术风险最低、对生产影响最直接、业务价值最清晰。

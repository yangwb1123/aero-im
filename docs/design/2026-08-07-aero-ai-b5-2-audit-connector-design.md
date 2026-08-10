# Design — B5-2: aero-audit-connector（leased relay + dead terminal + claim validation）

- **Direction**: B5-2 — 把内嵌 Snaplink relay 抽取为 `crates/aero-audit-connector`，用 AiWorker/ConsumerEventReceipt 的 lease/backoff/dead 状态机补齐 §1.2 语义
- **Requirements**: `docs/requirements/2026-08-07-aero-ai-b5-2-audit-connector.req.md`（R1–R9, A1–A6）
- **Status**: Design（证据全部逐条复核 + 测试实跑；工作树已有实现，本文档为验收/提交前置设计）
- **Verification date**: 2026-08-07

## 0. Evidence adjudication（untrusted claims → verified anchors）

| Evidence claim | Verdict | Verified anchor（实际） |
|---|---|---|
| `snaplink_commercial/http.rs:23,148` — `SCOPE_AUDIT`、bearer + 401 失效 | ✅ 成立（路径实为 `crates/aero-server/src/…`） | `SCOPE_AUDIT` :23；Audit 臂 :144-149（scope :146, `ACCEPTED` :149）；401→`invalidate_token` :155-158 区域 |
| `validate_token` :353-364 仅形状校验（claim 校验缺席） | ✅ 成立（行号漂移 → 实际 :430） | `validate_token` :430-441：非空/≤16KiB/无控制字符/bearer 类型——无 iss/aud/scope/sub 检查 |
| `runtime.rs` — relay 循环/from_env/ready；**无 dead 终态** | ✅ 成立（行号漂移 :42/:87/:174） | `run_delivery_relay` :174-195 无限 loop；`dispatch_batch` → `claim_due` → `deliver_claim` :263-293（Ok→`mark_delivered`，Err→`mark_failed`）；**无任何路径移出行出重试集合** |
| `config.rs` — 端点/退避；lease>2×timeout+2s boot bail | ✅ 成立（endpoints struct 实为 :44，:97-118 是 `CommercialBinding`；bail 精确命中） | `check` 不变量 `:247-251`（`delivery_lease <= 2×request_timeout+2s` → `bail!`）；`AERO_TASK_DRAIN_SECS` 同款 :253-255 |
| 300s 退避 cap 已在 v1 存储层 | ✅ 成立（勘误属实） | `crates/aero-storage/src/snaplink_commercial.rs`：`MAX_BACKOFF_SECS=300` :18；`commercial_delivery_backoff` :710-719 |
| `worker/mod.rs` — MAX_ATTEMPTS=5/run_loop/run_one/cap | ✅ 成立 | `MAX_ATTEMPTS=5` :69；`is_over_attempt_cap` :523-524（`attempts > MAX_ATTEMPTS`）；`run_loop` :601；`run_one` :798；cap guard :824/:827（超限 dead-letter 不 spend） |
| `ai_job.rs:174-180` — SKIP LOCKED + ORDER BY priority | ✅ 成立（文件实为 `crates/aero-storage/src/ai_job.rs`） | claim :171-203（ORDER BY :179、SKIP LOCKED :180）；`fail` :205-221 `CASE WHEN attempts >= $3 THEN 'dead'` + `LEAST(attempts,5)*5s` 退避 |
| `consumer_event_receipt.rs:29-69` — lease + fencing | ✅ 成立 | `ConsumerEventClaim` :17-41；`claim` :43-99（`ON CONFLICT … WHERE lease_expires_at <= $4`，attempts 即 fence）；renew :101 / complete :134 双栅栏 |
| `ai_usage.rs` — reserve/finalize/cancel/claim_due/mark_failed | ✅ 成立（实为 `crates/aero-storage/src/ai_usage.rs`） | reserve :160 / finalize :289 / cancel :362 / claim_due :451 / settle :497 / mark_failed :576（:574-575 明言「deliberately no dead-letter」）/ backoff :683 / clamp :693 |
| v1 outbox 无 dead 终态 | ✅ 成立 | `migrations/0235_snaplink_commercial_control_plane.sql:161-184`：`snaplink_delivery_outbox` 无 status 列，`delivered_at IS NOT NULL` 是唯一终态 |
| relay 内嵌 aero-server | ✅ 成立 | `crates/aero-server/src/lib.rs:106 pub mod snaplink_commercial;`；`state_builder.rs:16-17`（`Option<Arc<SnaplinkCommercialRuntime>>`）；boot spawn |
| 工作树 B5-2 实现 24/24 测试通过 | ✅ 实跑验证 | `cargo test -p aero-audit-connector --all-targets`：lib 7（config 2 + relay 5，含 3 项 transient 中继测试）+ `claim_validation` 11 + `state_machine` 6 = **24/24 ok**；另 1 项 `--ignored` PG 并发双 claim 测试（`pg.rs`，需 `DATABASE_URL`）；`cargo check --workspace` clean；`cargo clippy -p aero-audit-connector --all-targets` 无警告 |
| 零新第三方依赖（base64 0.22 已在图内） | ✅ 实跑验证 | `git diff Cargo.lock` 仅 +1 package（`aero-audit-connector` 自身）；`HEAD:Cargo.lock` 已有 `base64 0.22.1`；`cargo tree` 直接声明仅 workspace-pinned + base64 |
| usage 路径 no-touch | ✅ 实跑验证 | `git diff` 为空：`aero-storage/src/ai_usage.rs`、`aero-ai/src/usage.rs`、`aero-server/src/ai_usage.rs`、`aero-storage/src/snaplink_commercial.rs` |
| 0239 未落库（B5-1 门控） | ✅ 实跑验证 | `migrations/` 尾号 0238；`ls migrations/0239*` 不存在 |
| aero-ai 零改动（B5-2 方向约束） | ✅ 实跑验证 | aero-ai 工作树改动（`governance.rs`、`lib.rs` re-export、worker R-D1、tests）全部是 **B5-1** 的，B5-2 未碰 |

**勘误汇总**（对 direction 问题陈述）：① lease>2×timeout boot bail 已存在于 v1（config.rs:247-251）；② 300s 退避 cap 已存在于 v1 存储层（snaplink_commercial.rs:18,710-719）。**真实缺口四项**：relay 非独立 crate、无 dead 终态、无 JWT claim 校验、403 被当 transient 无限重试（fail-open）。

## 1. Design overview

新 crate `crates/aero-audit-connector`（root workspace member），把 v1 内嵌 audit relay（`aero-server/src/snaplink_commercial/` 的 Audit 腿）抽取为独立可测单元，同时把状态机从「无限重试」升级为 AiWorker/ConsumerEventReceipt 模式：

```
                     ┌──────────────────────────────────────────────┐
  B5-1 0239          │  crates/aero-audit-connector                 │
  audit_governance_  │                                              │
  outbox (status     │  RelayConfig::from_env (AERO_AUDIT_*)        │
   0/1/2/3)          │        │                                     │
     ▲ claim_due     │        ▼                                     │
     │ SKIP LOCKED   │  AuditRelay (poll loop + shutdown drain)     │
     │ rotated token │        │ claim ≤ batch → for_each_concurrent │
     │               │        ▼                                     │
     │ settle/       │  AuditClient.deliver(claim)                  │
     │ requeue/      │   cc token (缓存+401失效) → JWT claim 校验    │
     │ mark_dead     │   (iss/aud/scope/sub, POST 前置 fail-closed) │
     │  (fenced)     │   → POST events_url (Idempotency-Key=event_id)│
     └───────────────┘   → 202 + 回执校验 → Ok/Transient/Permanent/  │
                         Forbidden → 每行恰一个终态/重试转换         │
                         │                                          │
                         ▼                                          │
                boot 装配: aero-server main.rs（presence-gated）     │
                test doubles: FakeOutbox + StubSink（无 DB 单测）    │
                A3 drill: bin/aero-audit-relay-drill.rs（PG 端到端）  │
                └──────────────────────────────────────────────────┘
```

状态机策略（每行**恰一个**转换，fenced）：

| 投递结果 | 转换 | 终态性 |
|---|---|---|
| Ok（202 + 有效回执） | `settle`（status 0/1→2，清 token/lease） | 终态 |
| HTTP 403 | `mark_dead`（status→3）**attempt 1 即死** | 终态（T-11 fail-closed） |
| Permanent：422 / 409 / 回执不匹配 / payload guard | attempt 1 → `requeue`（退避 1s）；attempt ≥ 2 → `mark_dead` | ≤1 次重试后终态 |
| Transient：transport / timeout / 5xx / 未指定 4xx / 401-after-refresh / claim 校验漂移 | `requeue`（`2^(attempts-1)` cap 300s） | 永不 dead（保留 v1 retry-forever 姿态） |
| fence 丢失（stale token / 过期 lease / 行已 settle） | 仅 warn；lease expiry 重领 | — |

## 2. API changes

### 2.1 新 crate 公共 API（`crates/aero-audit-connector`，全部为新增，无既有 API 变更）

**`config.rs`**
```rust
pub struct RelayConfig {
    pub token_endpoint: Url, pub events_url: Url, pub resource: String,
    pub client_id: String, pub client_secret: String,
    pub expected_iss: String, pub expected_aud: String,
    pub expected_scope: String, pub expected_sub: String,
    pub source_system: String,
    pub request_timeout: Duration, pub delivery_lease: Duration,
    pub poll_interval: Duration, pub shutdown_drain: Duration,
    pub batch_size: i64, pub concurrency: usize,
}
impl RelayConfig {
    /// Ok(None) = 功能关闭（无 AERO_AUDIT_TOKEN_ENDPOINT 且无任何其他 AERO_AUDIT_* 残留）；
    /// 有残留变量 → Err（fail-loud，防静默降级）。
    pub fn from_env() -> anyhow::Result<Option<Self>>;
}
pub fn check_lease_invariant(timeout: Duration, lease: Duration) -> anyhow::Result<()>;
pub fn check_drain_invariant(timeout: Duration, shutdown: Duration, drain: Duration) -> anyhow::Result<()>;
```

**`outbox.rs`**（async trait seam，签名镜像 `AiUsageRepo` + dead 扩展，**唯一有意偏离：不接收调用方 `now`**）
```rust
pub struct Claim { pub event_id: Uuid, pub claim_token: Uuid,
                   pub lease_expires_at: OffsetDateTime, pub attempts: i64, pub payload: Value }
#[async_trait] pub trait OutboxRepo: Send + Sync {
    async fn claim_due(&self, lease: Duration, limit: i64) -> Result<Vec<Claim>, Error>;
    async fn settle(&self, event_id: Uuid, claim_token: Uuid) -> Result<bool, Error>;
    async fn requeue(&self, event_id: Uuid, claim_token: Uuid, attempts: i64,
                     error: &str) -> Result<bool, Error>;
    async fn mark_dead(&self, event_id: Uuid, claim_token: Uuid, attempts: i64,
                       error: &str) -> Result<bool, Error>;
}
// 契约：fence 违规返回 false 而非 Err；settle/requeue/mark_dead 栅栏于
// claim_token AND attempts AND 未过期 lease（attempts 自增即 fencing token）。
// **单一时钟域**（B5-2 修复点，相对 ai_usage 的有意偏离）：repo 独占时钟，
// 调用方永远不传 wall-clock——PG 侧 claim 过滤/lease 铸造/退避算术/fence 全部
// 用 clock_timestamp()；fake 侧全部用其注入时钟（set_now）。若 lease 由 app
// now 铸造而 fence 读 DB 时钟，则 |skew| >= lease 时第一道 fence 即败且 claim
// 过滤永不重新暴露该行 = claim→POST→fence-fail 活锁（无 settle 无 dead）。
```

**`pg.rs`** — `PgOutboxRepo::new(pool: PgPool)`；status 常量 `STATUS_ENQUEUED=0 / STATUS_CLAIMED=1 / STATUS_DELIVERED=2 / STATUS_DEAD=3`（B5-1 0239 规范值，connector 只消费不拥有）。claim CTE：`status IN (0,1) AND available_at <= clock_timestamp() AND (lease IS NULL OR lease <= clock_timestamp())`、`ORDER BY (available_at, created_at, event_id)`、`FOR UPDATE SKIP LOCKED`、batch clamp `[1,500]`；**`lease_expires_at = clock_timestamp() + make_interval(secs => lease)`——DB 时钟铸造，绝不绑定 app now**。settle 为单事务 fenced 重读 + UPDATE（`lease_expires_at > clock_timestamp()` 栅栏 + `delivered_at = clock_timestamp()`）。requeue：`available_at = clock_timestamp() + make_interval(secs => backoff)`——退避算术与 claim 过滤同域（单时钟域审计：`available_at`、`lease_expires_at`、`delivered_at`、全部 fence 均出自 `clock_timestamp()`；attempts 位移是纯算术不涉时钟）。

**`relay.rs`**
```rust
pub const MAX_LEASE_SECONDS: i64 = 86_400;  // 克隆 ai_usage
pub const MAX_BACKOFF_SECONDS: i64 = 300;
pub const MAX_CLAIM: i64 = 500;
pub const MAX_ERROR_CHARS: usize = 2_048;
pub fn audit_backoff(attempts: i64) -> Duration;   // 2^(attempts-1) cap 300
pub fn clamped_lease(lease: Duration) -> Duration; // [1s, 86400s]
pub fn truncate_error(error: &str) -> String;
pub struct AuditRelay { /* private */ }
impl AuditRelay {
    pub fn new(repo: Arc<dyn OutboxRepo>, client: AuditClient, config: RelayConfig) -> Self;
    pub fn spawn(self, cancel: CancellationToken) -> JoinHandle<()>;  // poll loop + bounded drain
    pub async fn dispatch_batch(&self) -> Result<usize, Error>;      // 单批 claim→并发投递→每行恰一转换
}
// relay 不持有任何时钟 seam：无 app-side `now` 输入（`dispatch_batch` 只传
// lease 与 batch_size；requeue 不传时间）。测试钉 `FakeOutbox::set_now`（repo
// 侧时钟），不钉 relay。
```

**`client.rs`**
```rust
pub const SCOPE_AUDIT: &str = "audit:event:write";
pub enum PermanentKind { Unprocessable, Conflict, ReceiptMismatch, PayloadGuard }
pub enum DeliveryError { Transient(#[from] anyhow::Error), Permanent(PermanentKind), Forbidden }
pub struct ClaimRejection { pub reason: String }
pub struct AuditClient { /* private */ }
impl AuditClient {
    pub fn new(config: RelayConfig) -> anyhow::Result<Self>;
    pub async fn deliver(&self, claim: &Claim) -> Result<(), DeliveryError>;
    pub fn validate_token_claims(&self, token: &str) -> Result<(), ClaimRejection>; // 纯函数，可单测
}
```

**`fake.rs` / `stub.rs`**（test-only 公共面）：`FakeOutbox`（内存 `OutboxRepo`，注入时钟 `set_now`，`FakeStatus::{Ready,Claimed,Delivered,Dead}` + `code()`）；`StubSink`（`start()/token_url()/events_url()/posts()/set_behavior()/shutdown()`，`SinkBehavior{token_claims, events_status, receipt_valid, unauthorized_once}`，`make_jwt(claims)`）。

**`bin/aero-audit-relay-drill.rs`** — A3 drill：`DATABASE_URL` 必填；`AERO_AUDIT_DRILL_ROWS`（默 3）；0239 表缺失 exit 2 显式 SKIP；断言 `COUNT(status=2)==N` + event_id 集合 parity。

### 2.2 环境变量面（新增，单下划线 plain env — AGENTS.md §4.3 例外类）

`AERO_AUDIT_TOKEN_ENDPOINT`（presence gate）/ `AERO_AUDIT_EVENTS_URL` / `AERO_AUDIT_RESOURCE` / `AERO_AUDIT_CLIENT_ID` / `AERO_AUDIT_CLIENT_SECRET`（≥32 字节 fail-loud）/ `AERO_AUDIT_EXPECTED_ISS` / `AERO_AUDIT_EXPECTED_AUD` / `AERO_AUDIT_EXPECTED_SCOPE`（默 `audit:event:write`）/ `AERO_AUDIT_EXPECTED_SUB` / `AERO_AUDIT_SOURCE_SYSTEM` / `AERO_AUDIT_REQUEST_TIMEOUT_SECS`（默 10）/ `AERO_AUDIT_DELIVERY_LEASE_SECS`（默 30）/ `AERO_AUDIT_SHUTDOWN_DRAIN_SECS`（默 5）/ `AERO_AUDIT_TASK_DRAIN_SECS`（默 30）/ `AERO_AUDIT_POLL_INTERVAL_SECS`（默 5）/ `AERO_AUDIT_BATCH_SIZE`（默 100）/ `AERO_AUDIT_CONCURRENCY`（默 4）/ `AERO_AUDIT_ALLOW_INSECURE_LOOPBACK`（默 false，HTTP 仅 loopback 且显式开启）。端点拒绝凭据/query/fragment/超 2048 字节。

### 2.3 既有系统改动（最小面）

| 文件 | 改动 |
|---|---|
| root `Cargo.toml` | +workspace member `crates/aero-audit-connector`；+`aero-audit-connector = { path = … }` |
| `crates/aero-server/Cargo.toml` | +`aero-audit-connector.workspace = true` |
| `crates/aero-server/src/bin/main.rs` | +B5-2 段（:245-272）：`RelayConfig::from_env()` → `Ok(Some)` 时 `PgOutboxRepo` + `AuditClient` + `AuditRelay::spawn(ai_shutdown.clone())` 入 tracker；`Ok(None)` 静默关闭；`Err` 传播 = boot 失败 |
| `scripts/test-integration.sh` | +3 个 throwaway DB 名 + `run_migrated_integration` 空过滤守卫（防 vacuous green）+ B5-1 门控的 governance 条目与 A3 drill 段（0239 存在才跑，否则显式 SKIP） |
| `Cargo.lock` | +1 package（connector 自身）；`base64 0.22.1` 已在图内，无新第三方项 |

**零改动（硬约束）**：`crates/aero-ai/*`（B5-2 方向约束；工作树 aero-ai 改动均属 B5-1）、`crates/aero-storage/src/{ai_usage.rs,snaplink_commercial.rs}`、`crates/aero-server/src/{ai_usage.rs,snaplink_commercial/*}`（v1 relay 原地不动，含 Usage 腿与 entitlement 投影）。

## 3. Compatibility constraints

1. **双 relay 共存期**：`AERO_AUDIT_*` 与 `AERO_SNAPLINK_*` env 面完全不相交；connector 只 claim `audit_governance_outbox`（0239），**绝不写 v1 `snaplink_delivery_outbox`**。v1 退役/0239 触发器重定向是 B5-1/campaign 决策，不在本设计。
2. **B5-1 0239 未落库 = 既定降级态**：relay 照常 spawn，claim 报错仅 warn（F13），不 crash；drill 与 integration 段被迁移文件存在性门控（exit 2 SKIP）。status 0/1/2/3 常量以 B5-1 DDL 为规范，connector 不得硬编码别处数字。
3. **依赖供应链**：全部 workspace-pinned；唯一直接声明 `base64 = "0.22"`（已在图内，aero-server/aero-live-srt 已钉）。不引 jsonwebtoken——JWT 只做 base64url payload 解码 + claim 比对，**签名不校验**（信任姿态与 v1 形状校验一致：IdP 经 client_credentials 可信；JWKS 验证是 [PROPOSED]）。
4. **工程规则**：`unsafe_code=forbid`；workspace lints；`cargo clippy --workspace --all-targets` 不得新增警告；`scripts/{truth-check,file-size-check,web-check}.sh` 0 违规。新文件遵守尺寸阈值（`file-size-check.sh` 唯一来源）。
5. **Rust 2021 / MSRV 1.80**；tokio/axum/sqlx 0.8/async-nats 栈一致。
6. **event_id = `audit_events.id`**（B5-1 1:1 parity）= 稳定幂等键 + 出站 `Idempotency-Key` + 回执 `event_id` 匹配对象——三处同一标识，保证 sink 侧去重与回执校验闭合。
7. **claim 校验期望值来自仓外契约（[PROPOSED]）**：本设计以 `RelayConfig.expected_*` 配置注入实现 seam；契约若改判（如 claim 失败直接 dead），只动 `client.rs` 分类 + A2 参数化测试，状态机/表结构不动。
8. **每次投递前置校验，非一次性 boot 检查**：token 缓存内的 token 在每次 POST 前重新过 claim 校验（`deliver` 循环内），防 IdP 侧 claim 漂移期间携带坏 token 出网。

## 4. Failure modes（每行 = 故障 → 行为）

| # | 故障 | 行为 | 终态性 |
|---|---|---|---|
| F1 | claim SQL 错（0239 表未落 / 连接抖动） | `dispatch_batch` Err → warn「durable rows remain queued」→ 下个 tick 重试 | 行保持 enqueued |
| F2 | transport / timeout / 5xx / 未指定 4xx | `Transient` → `requeue`（`2^(attempts-1)` cap 300s） | 永不 dead（v1 姿态保留） |
| F3 | 401 on delivery | 失效缓存 token → 刷新一次 → 同一次尝试内重试（`unauthorized_once` 单测钉 2 POST）；刷新后再 401 | `Transient`（requeue） |
| F4 | 403（配给被拒 / 身份被拒） | **attempt 1 即 `mark_dead`**，无 requeue、无第二次 POST（T-11 fail-closed；配给门本身属 B5-4） | dead（终态） |
| F5 | 422 / 409 / 回执不匹配 / payload guard | attempt 1 → requeue（1s）；attempt ≥ 2 → `mark_dead` | ≤1 次重试后 dead |
| F6 | fence 丢失（stale token / 过期 lease / 已 settle 行） | 转换返回 false → warn「lease expiry will reclaim」；重领轮换新 token，旧 token 永不可 ack | 行由新 claim 接管 |
| F7 | POST 成功但 ack 失败（settle 前崩溃） | at-least-once：`Idempotency-Key: event_id` 使 sink 去重；重投携同一 event_id | 重试直至 settle |
| F8 | `AERO_AUDIT_*` 部分配置（无 token endpoint 但有残留） | `from_env` → Err → **boot 失败**（fail-loud，防静默关闭） | — |
| F9 | lease ≤ 2×timeout+2s / drain 不变量破坏 | boot `bail!`（克隆 v1 config.rs:247-251 / :253-255） | — |
| F10 | token claim 校验失败（iss/aud/scope/sub 漂移） | 该 token 丢弃 + 失效缓存，**无 POST**；行按 `Transient` requeue（下次 claim 轮换新 token = IdP 修复后唯一前进路径） | 行不 dead（A5「不重试」读法：坏 token 不重试，行重试） |
| F11 | opaque 非 JWT token | claim 校验无法运行 → fail-closed 无 POST（`opaque_non_jwt_token_is_rejected_before_any_post`） | 同 F10 |
| F12 | shutdown / cancel | 有界 drain（`shutdown_drain` timeout 内循环 dispatch 至 0 行）；超时 warn「rows remain durable」 | 行留库，重启重领 |
| F13 | 0239 未落时 boot | relay spawn 成功但每 tick claim Err → warn（同 F1）；drill exit 2 | 既定降级，非缺口 |
| F14 | sink 侧重复回执 / 冲突（`conflict=true` 或 status 越界） | 回执校验失败 → `ReceiptMismatch` permanent（F5 路径） | ≤1 次重试后 dead |
| F15 | 出站 payload 选择 tenant / source 不匹配 | 本地 `PayloadGuard` permanent，POST 前拦截（`posts()==0` 断言） | ≤1 次重试后 dead |

## 5. Migration steps

**无 DB 迁移属于本 direction**（0239 DDL/enqueue 触发器是 B5-1 的；B5-2 只消费）。迁移 = 代码集成 + 门禁：

1. **复核（当前即可，无 B5-1 依赖）**：
   - `git diff` 核对工作树实现与 §2 API 面逐字段一致（已核）；
   - `cargo test -p aero-audit-connector --all-targets` → 24/24（lib 7：config 2 + relay 5——含 3 项 transient 中继测试；claim-validation 11；state-machine 6；含 `skew_gt_lease_cannot_livelock_claim_fence_settle`）（已实跑）；
   - `cargo test -p aero-audit-connector --lib -- --ignored`（`DATABASE_URL` + 可写 PG）→ 含 `pg.rs::concurrent_double_claim_across_two_sessions_is_impossible`（自建 0239 等效表，无需 B5-1 落库；已实跑 ×4）；
   - `cargo check --workspace` clean（已实跑）；`cargo clippy --workspace --all-targets` 无新警告（connector crate 已验 clean）；
   - A6 no-touch：`git diff` 为空（已核）四个 usage 路径文件；
   - `scripts/{truth-check,file-size-check,web-check}.sh` 0 违规。
2. **提交**：工作树 B5-2 产物（crate + root Cargo 变更 + main.rs 接线 + test-integration.sh + 设计/需求文档）与 B5-1 的 aero-ai 改动**分开提交**（B5-1 的 `governance.rs`/worker R-D1 是另一个 direction 的交付物）。
3. **B5-1 落库后**（0239 迁移 + `audit_governance_outbox` 仓储 + enqueue 触发器）：
   - 解开 `scripts/test-integration.sh` 门控：governance db_tests、moderation finalize parity、A3 drill（`COUNT(status=2)==N` + event_id parity；可改为经 enqueue 函数种 `room.create` 审计行）；
   - `cargo test --workspace --lib -- --ignored`（DATABASE_URL + 已迁移）全绿。
4. **B5-3（priority 反饥饿）/ B5-4（`aero-cli audit-provision-check` 配给 seam）** 并行，不改 connector 状态机；B5-4 消费本设计的 403→dead（T-11）。
5. **v1 退役决策**（campaign 级）：connector 与 v1 audit 腿并存期无冲突（表/环境变量不相交）；双跑验证一致后再切。

## 6. Testable acceptance mapping（A1–A6 → 精确测试符号）

| 验收 | 测试符号（已实跑绿） | 断言要点 |
|---|---|---|
| **A1** T-11：无配给 → 403 → dead ≤1 次尝试，不再重试 | `tests/state_machine.rs::forbidden_dead_on_first_attempt` | `FakeStatus::Dead`、`attempts==1`、`stub.posts()==1`、后续 `claim_due` 空；`pg.rs::mark_dead` 置 status=3，claim CTE `status IN (0,1)` 排除 |
| **A2** 422/409/回执错 → dead ≤1 | `tests/state_machine.rs::permanent_error_dead_after_exactly_two_attempts`（三参数化：422 / 409 / `receipt_valid=false`） | attempt1→`Ready`（`attempts==1`、`available_at∈[t0+backoff(1), t0+backoff(1)+δ]`——生产侧 PG 以 `clock_timestamp()+backoff` 盖戳，相对捕获的 t0 是**窗口**非精确等式；单测钉死 fake 时钟（=PG `clock_timestamp()` 模拟）断言精确 `==t0+1s`、无 wall-clock 窗口、`last_error` 含类名）；attempt2→`Dead`（`attempts==2`）；后续 `claim_due` 空 |
| **A3** lease>2×timeout fencing + **单一时钟域（skew>lease 无活锁）**：并发两 claim 仅一者完成，过期旧 claim 无法完成；app/DB 时钟偏斜 ≥ lease 不得锁死行 | `stale_token_cannot_ack_after_reclaim` + `skew_gt_lease_cannot_livelock_claim_fence_settle` + `config.rs::lease_must_exceed_two_request_timeouts_plus_slack`（+ `task_drain…`）+ `pg.rs::concurrent_double_claim_across_two_sessions_is_impossible`（`--ignored`，真双 session 并发） | token 轮换（`token_a != token_b`）；`settle(id, token_a)==false`、`settle(id, token_b)==true`；`lease_expires_at == repo_clock + lease` 精确等式（fake 时钟 = PG `clock_timestamp()` 模拟，钉在 relay wall clock 前 90s = 3×lease）；同刻第一道 fence 必须成功；lease ≤ 2×timeout+2s → `check_lease_invariant` Err；claim CTE `FOR UPDATE SKIP LOCKED`——两独立 session（双 pool）各 `LIMIT 25` 并发 claim 50 行：各自恰 25、集合零交集、`attempts==1` 全行、DB 层 `attempts>1` 计数 == 0（零双 claim，实跑 ×4） |
| **A4** 首个 room.create 经 cc+scope 到 mock sink 202 回执；无新第三方依赖 | `happy_path_settles_and_removes_from_claimable` + `claim_validation.rs::valid_token_is_accepted_and_delivery_proceeds` + `bin/aero-audit-relay-drill.rs` + `scripts/test-integration.sh:236-270` | `FakeStatus::Delivered`、token/lease 清空、claimable 空；`posts>=1`；drill：`COUNT(status=2)==N` + event_id parity（0239 门控，缺表 exit 2）；`cargo tree` 直接依赖仅 workspace + base64 0.22（lock diff 仅 +1 package） |
| **A5** claim 校验：iss/aud/scope/sub 任一不匹配 → 拒绝且不重试 | `tests/claim_validation.rs`（11 tests）：`wrong_issuer_…` / `missing_audience_…` / `missing_audit_scope_…` / `wrong_subject_…` / `opaque_non_jwt_…` / `unauthorized_refreshes_once_…` / `refreshed_token_must_repass_claim_validation_before_retry_post` / `client_classifies_statuses` / `payload_guard_…` / `jwt_claims_decode_roundtrip` / `valid_token_…` + `relay.rs::transient_{5xx,timeout,claim_drift}_requeues_and_rotates_a_fresh_token`（中继级 Transient 臂：500 / 超时 / claim 漂移 → requeue + 下次 claim 新 token 轮换，`token_a != token_b`） | 四场景 + opaque **`posts()==0` 字面测量**（拒绝发生在任何 POST 前）；401 刷新 = 同一次尝试内 2 POST 后成功；「不重试」读法钉死：坏 token 永不出网，行按 transient requeue 轮换新 token——此前仅靠代码形状（与 A1-3 permanent 臂同 requeue 路径）钉的后半句，现由中继级测试直接驱动 `deliver_claim` Transient 臂钉死 |
| **A6** usage relay 行为不变 | no-touch `git diff` 空（`ai_usage.rs`×2 + `aero-server/src/ai_usage.rs` + `snaplink_commercial.rs`）+ 既有 `cargo test --workspace --lib -- --ignored`（`ai_usage/tests.rs` 的 `stable_reservation_and_settlement_are_exactly_once`、`stale_claim_cannot_ack_after_reclaim`、`finalized_outcome_replays_…` 等 + v1 `snaplink_commercial/http.rs` tests） | 零 diff + 全绿；唯一允许的 aero-server delta = `main.rs:245-272` 接线段 |

**未闭合项**（验收依赖 B5-1）：PG 侧 drill 端到端、`room.create` 经 enqueue 触发器的全链路（`audit.rs::append_in_tx` + 0239 触发器）。这些在 0239 落库前保持显式 SKIP，禁止以「0 测试匹配」的方式虚绿（`run_migrated_integration` 空过滤守卫已防）。**例外**：A3 的并发双 claim PG 测试（`pg.rs::concurrent_double_claim_across_two_sessions_is_impossible`）自带 0239 等效表探测+自建（`to_regclass` 命中即复用真实表），无需 B5-1 即可实跑；drill 仍保持 0239 门控。

## 7. Risks / decisions

- **§1.2 契约原文在仓外**：本设计钉 in-repo 规范化读法（permanent → requeue 一次 → dead；403 → attempt 1 dead；transient 永不 dead；claim 校验失败 = 行 transient + token 永不出网）。契约若冲突只改 R5 阈值 + `client.rs` 分类 + A2 参数化。
- **签名不校验**：claim 校验是「payload 内容匹配」而非密码学验证——与 v1 信任姿态一致（IdP 可信），但被中间人篡改 JWT 的场景不在防护面；JWKS 验证列为 [PROPOSED]，不得在未评审时声称完成。
- **上一批次实现未提交 ≠ 验收可跳过**：A1–A6 全量重跑是提交前置（本设计 §5 步骤 1 即此）。
- **单一时钟域（B5-2 相对 ai_usage 的有意偏离，分布式评审 finding #1 的修复）**：claim 的 `lease_expires_at` 与 requeue 的 `available_at` 均在 SQL 内以 `clock_timestamp()` 铸造（`make_interval`），trait 不接收 app `now`——`|skew| >= lease` 的 claim→POST→fence-fail 活锁结构性不可能。`check_lease_invariant`（lease > 2×timeout+2s）是纯时长算术，与时钟域无关，修复后依然精确界定单次尝试内预算（首 POST + 401-refresh-once 必须在同一 lease 内、以同一 DB 时钟度量）。**ai_usage.rs 的同类混合时钟域是存量债务，本批次不碰（A6 no-touch）**——connector 先修，立前例。
- **行号漂移**：本文档全部锚点以「文件/符号」为准（AGENTS.md §0），行号仅核对时快照。

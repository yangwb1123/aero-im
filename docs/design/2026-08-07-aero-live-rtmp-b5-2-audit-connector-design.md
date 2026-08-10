# Design — B5-2: `crates/aero-audit-connector`（leased relay + dead terminal + claim validation, porting the three proven machines）

- **Direction**: B5-2 — build the audit connector by porting the repo's three proven lease/backoff/dead-terminal machines (snaplink relay · webhook_delivery · stream_go_live_outbox) instead of inventing a new state machine
- **Requirements**: `docs/requirements/2026-08-07-aero-live-rtmp-b5-2-audit-connector.req.md`（R1–R9, A1–A7）
- **Status**: Design（evidence 全部复核 + 测试实跑；工作树已有未提交 port，本文档为其验收/提交前置设计 + R5 缺口修正）
- **Verification date**: 2026-08-07
- **Analysis root**: `crates/aero-live-rtmp/src`（B5-1 的 in-tx audit 源，本方向只消费 outbox，不碰该模块）

## 0. Evidence adjudication（untrusted claims → verified anchors）

| # | Evidence claim | Verdict | Verified anchor |
|---|---|---|---|
| E1 | `snaplink_commercial/http.rs:23` `SCOPE_AUDIT` | ✅ | `const SCOPE_AUDIT: &str = "audit:event:write";` 精确在 :23 |
| E1 | cc grant `:233-254`（basic_auth + form cc/scope/resource） | ✅ | `request_token` :226-254：`basic_auth(client_id, Some(client_secret))` + `grant_type=client_credentials` + `scope` + `resource` |
| E1 | token cache + 401 invalidation `:182-232` | ✅ | `access_token` :182-231：per-key 缓存 `refresh_at`（ttl-30 或 ttl/2），**锁不跨网络 I/O**（:194 注释明言） |
| E1 | audit 期望 202 + 回执 `:133-180/:330-351` | ✅ 语义成立，**行号漂移 ~80** | Audit 臂 :146-160（`StatusCode::ACCEPTED` :153）；`validate_audit_receipt` 实际 :410-427（event_id 匹配、tenant 匹配、`accepted_at` 存在、`conflict=false`、status ∈ {ledgered,indexed,archived}） |
| E1 | `validate_token` `:353-364` 仅 shape 校验 | ✅ 语义成立，**行号漂移**（实际 :430-439） | 非空 / ≤16KiB / 无控制字符 / bearer 类型——**无任何 JWT claim 校验** |
| E1 | `runtime.rs:221-277` lease claim / lease-loss warn / fenced re-park | ✅ | `dispatch_batch` :215-232（`claim_due(now, delivery_lease, batch)`）；`deliver_claim` :247-281：Ok→`mark_delivered`，`Ok(false)`→warn "delivery lost its lease before acknowledgement"（:255-260）；Err→`mark_failed`，非 `Ok(true)`→warn "lease expiry will reclaim"（:267-279） |
| E2 | `webhook_delivery.rs` 生命周期 / MAX_ATTEMPTS / 纯函数 | ✅ | 模块 doc :8-10（`pending → delivered \| failed → dead`，claim_token 拥有每个 pending 代）；`MAX_ATTEMPTS: i32 = 6` :38；`backoff_delay` :60-67 纯函数；`is_dead_at(attempts) -> bool` :76-81 **无时钟纯谓词**；:21-26 明言 DB-free 测试 seam |
| E3 | `stream_go_live_outbox.rs` MAX_BACKOFF=300 / claim CTE / fenced mark_failed | ✅ | `MAX_CLAIM=500` :14、`MAX_LEASE_SECONDS=86_400` :15、`MAX_BACKOFF_SECONDS=300` :16；claim CTE :140-165（`FOR UPDATE SKIP LOCKED` :157、`attempts+1` :163、`claim_token = gen_random_uuid()` :162）；`mark_failed` :286-311（fence `attempts = $3 AND completed_at IS NULL`，`available_at = now + retry_delay(attempts)` **Rust 侧计算** :294）；`retry_delay` 纯函数 + 单测（1→1s, 2→2s, 99→300s） |
| E4 | `consumer_event_receipt.rs` attempts-as-fence lease | ✅ | `claim` :43-99：`ON CONFLICT … WHERE lease_expires_at <= $4 RETURNING attempts`；renew/complete 双栅栏 `attempts = $3 AND lease_expires_at > clock_timestamp()`（:101-132/:134-170） |
| E5 | `background.rs:99-140` golive relay spawn 模板 | ✅ | `stream_live_poll_ms` 门控 spawn：`interval` + `MissedTickBehavior::Skip` + `tokio::select!{biased; cancel}` + per-tick `dispatch_batch(&s, 100)` warn-not-panic |
| S2 | `lease > 2×timeout + 2s` boot bail 已存在 | ✅ | `snaplink_commercial/config.rs:246-251`（`delivery_lease <= request_timeout*2 + 2s ⇒ bail!`）；task-drain 变体 :253-255 |
| S4 | 工作树已有未提交 port；workspace member；boot 接线；24/24 测试 | ✅ **实跑** | `Cargo.toml:29` member + `:77` dep；`main.rs:251-259` 接线；§3.3 delta 落地后 `cargo test -p aero-audit-connector` = lib 8 + claim_validation 11 + state_machine 6 = **25/25 pass** + 1 `--ignored` PG 测试；`base64 0.22.1` 已在 `Cargo.lock:857-859`（aero-server/aero-live-srt 已 pin）→ **零新第三方依赖** |
| S5 | no-touch guard 成立 | ✅ **实跑** | `git diff --stat` over `snaplink_commercial/`、`aero-storage/src/snaplink_commercial.rs`、`migrations/0235_*.sql` = **空**；aero-server 唯一 delta 是 main.rs 接线 |
| S1 | v1 outbox 无 status 列、无 dead 终态 | ✅ | `snaplink_commercial.rs` claim/mark_delivered/mark_failed :302-383，`delivered_at` 是唯一终态；`commercial_delivery_backoff` :710-719 cap 300s |

**勘误汇总**：direction 的「crate 不存在」已被工作树 port 推翻（S4）；「lease>2×timeout」「300s cap」已存在（S2/E3/S1）；**真实缺口四项**——① relay 内嵌 aero-server 非独立 crate；② v1 无 dead 终态；③ 无 claim 校验（v1 仅 shape-check）；④ 403 被当 retryable（fail-open，须改 fail-closed T-11）。

**本次复核新增发现（spec 未列）**：
- `http.rs` 中 `validate_audit_receipt`/`validate_token` 的实际行号比 spec 引用的漂移 ~80 行（:410/:430 vs :330/:364）——符号与语义完全一致，spec 已声明行号可漂移，不构成证伪。
- **connector 回执校验弱于 v1**：v1 校验 `receipt.tenant_id != claim.tenant_id`（**精确匹配**）；port 的 `client.rs:386` 只查 `receipt.tenant_id.is_empty()`（**存在性**），因为 `Claim` 结构不携带 tenant（B5-1 0239 payload 无 tenant 字段，且出站 payload 禁止 tenant_id）。R7 的 "tenant_id matches" 实际实现为 "tenant_id present"。见 §5 开放项 O1。
- **R5 缺口属实**：`relay.rs:193` 内联 `attempts < 2`，无命名纯函数 `is_dead_at`，与 direction 明确要求的 webhook_delivery.rs 形状不符（spec §7 已标注）。本设计把它列为唯一强制代码 delta（§3.3）。

## 1. Design overview

新 crate `crates/aero-audit-connector`（root workspace member，`publish=false`）。状态机 = 三个 proven machine 的合成，**每行恰好一个转换**（settle / requeue / mark_dead），全部 fenced：

```
                     ┌──────────────────────────────────────────────┐
  B5-1 0239          │  crates/aero-audit-connector                 │
  audit_governance_  │                                              │
  outbox (status     │  RelayConfig::from_env (AERO_AUDIT_*)        │
   0/1/2/3)          │        │  presence-gated + fail-loud         │
     ▲ claim_due     │        ▼                                     │
     │ SKIP LOCKED   │  AuditRelay (poll loop + shutdown drain)     │
     │ rotated token │        │ claim ≤ batch → for_each_concurrent │
     │ clock_timestamp│       ▼                                     │
     │ (single clock)│  AuditClient.deliver(claim)                  │
     │               │   cc token (缓存+401失效) → JWT claim 校验    │
     │ settle/       │   (iss/aud/scope/sub, POST 前置 fail-closed) │
     │ requeue/      │   → POST events_url (Idempotency-Key=event_id)│
     │ mark_dead     │   → 202 + 回执校验 → Ok/Transient/Permanent/  │
     │  (fenced)     │     Forbidden → 恰一个转换                    │
     └───────────────┘                                              │
                boot: aero-server main.rs:251-259（已接线）          │
                test doubles: FakeOutbox + StubSink（无 DB 单测）    │
                drills: bin/aero-audit-{relay,t11,priority}-drill    │
                └──────────────────────────────────────────────────┘
```

**三条 proven machine 的出处映射**（port 语义 = 下列每条的精确克隆 + 一个刻意偏差）：

| 机器 | 出处 | port 中的克隆 |
|---|---|---|
| a) 租赁 claim 循环 + 300s 退避 + token 轮换 | `stream_go_live_outbox.rs:140-165,286-311` | `pg.rs::claim_due/requeue`（+ `relay::audit_backoff` 纯函数） |
| b) 纯函数 dead 谓词 + 生命周期 | `webhook_delivery.rs:60-81`（`backoff_delay`/`is_dead_at`） | `relay::audit_backoff`（✅ 已纯）；**`is_dead_at` 缺失 → §3.3 强制 delta** |
| c) cc 令牌 + 202 回执 + 缓存/401 | `snaplink_commercial/http.rs:23,133-180,182-254` | `client.rs`（+ 新增 claim 校验） |
| d) attempts-as-fence 租赁 | `consumer_event_receipt.rs:43-99` | `outbox.rs` 契约（fence = `claim_token AND attempts`） |

**刻意偏差（单一时钟域）**：`OutboxRepo` 四个方法**均不接受调用方 `now`**——lease mint、availability 过滤、fence 全在 repo 时钟（PG `clock_timestamp()`；fake 可注入时钟）上求值。v1/ai_usage 模式用 app 时钟 mint lease、DB 时钟做 fence；|app↔DB skew| ≥ lease 时每次 claim 都 mint 一个已过期 lease → claim→POST→fence-fail 活锁（既不 settle 也不 dead）。`outbox.rs` doc + `skew_gt_lease_cannot_livelock_claim_fence_settle` 测试钉死该不变量（A4）。

## 2. API changes

### 2.1 新增 crate 公共面（全部已存在，除 §3.3）

```rust
// outbox.rs —— repo seam（async trait, Send+Sync）
pub struct Claim { pub event_id: Uuid, pub claim_token: Uuid,
                   pub lease_expires_at: OffsetDateTime, pub attempts: i64,
                   pub payload: serde_json::Value }
#[async_trait]
pub trait OutboxRepo {
    async fn claim_due(&self, lease: Duration, limit: i64) -> Result<Vec<Claim>, Error>;
    async fn settle(&self, event_id: Uuid, claim_token: Uuid) -> Result<bool, Error>;
    async fn requeue(&self, event_id: Uuid, claim_token: Uuid, attempts: i64, error: &str) -> Result<bool, Error>;
    async fn mark_dead(&self, event_id: Uuid, claim_token: Uuid, attempts: i64, error: &str) -> Result<bool, Error>;
}
// Error 只包存储故障；fence 违例返回 false，永不当错误
```

- `relay.rs`：`AuditRelay::{new, spawn(cancel), dispatch_batch}`；纯函数 `audit_backoff(i64)->Duration`（1→1s…10→300s）、`clamped_lease`、`truncate_error`；常量 `MAX_BACKOFF_SECONDS=300 / MAX_LEASE_SECONDS=86_400 / MAX_CLAIM=500 / MAX_ERROR_CHARS=2048`（测试引用）。
- `client.rs`：`AuditClient::new(config)`、`deliver(&Claim) -> Result<(), DeliveryError>`、`validate_token_claims(&str) -> Result<(), ClaimRejection>`；`DeliveryError::{Transient, Permanent(PermanentKind), Forbidden}`；`PermanentKind::{Unprocessable, Conflict, ReceiptMismatch, PayloadGuard}`；`SCOPE_AUDIT = "audit:event:write"`。
- `config.rs`：`RelayConfig::from_env() -> Result<Option<Self>>`（presence-gated）；`check_lease_invariant` / `check_drain_invariant`（v1 克隆，纯 duration 算术）。
- `pg.rs`：`PgOutboxRepo::new(PgPool)`；`STATUS_ENQUEUED=0 / CLAIMED=1 / DELIVERED=2 / DEAD=3`（B5-1 0239 DDL 规范值）。
- `fake.rs` / `stub.rs`：`FakeOutbox`（可 pin 时钟）+ `StubSink`（POST 计数器 + 可注入 behavior）——dev-deps，仅测试。
- `lib.rs` re-export：`pub mod {client, config, fake, outbox, pg, relay, stub}`（fake/stub 供 tests 目录用）。

### 2.2 装配（已接线，`main.rs:251-259`）

`RelayConfig::from_env()` → `Ok(Some)` 时：`Arc<dyn OutboxRepo>` = `PgOutboxRepo`，`AuditClient`（config 校验失败 → boot error fail-loud），`AuditRelay::spawn(ai_shutdown.clone())`；`Ok(None)` → 静默不启；`Err` → boot 失败。background.rs:99-140 模板（interval + `MissedTickBehavior::Skip` + biased select + warn-not-panic）。

### 2.3 环境变量面（AGENTS.md §4.3 单下划线 plain env 例外族）

`AERO_AUDIT_TOKEN_ENDPOINT`（presence 门）+ `AERO_AUDIT_EVENTS_URL` + `AERO_AUDIT_RESOURCE/CLIENT_ID/CLIENT_SECRET/EXPECTED_ISS/EXPECTED_AUD/EXPECTED_SCOPE(默认 audit:event:write)/EXPECTED_SUB/SOURCE_SYSTEM` + `AERO_AUDIT_REQUEST_TIMEOUT_SECS(10,1..120)/DELIVERY_LEASE_SECS(30,5..300)/POLL_INTERVAL_SECS(5,1..300)/SHUTDOWN_DRAIN_SECS(5,1..60)/TASK_DRAIN_SECS(30,1..600)/BATCH_SIZE(100,1..500)/CONCURRENCY(4,1..32)/ALLOW_INSECURE_LOOPBACK(默认 false)`。**与 `AERO_SNAPLINK_*` 完全不相交** → 双 relay 并存过渡期。任何 `AERO_AUDIT_*` 在无 TOKEN_ENDPOINT 时出现 = boot error（防静默禁用）。

### 2.4 非改动面（compatibility）

- **零改动**：`aero-server/src/snaplink_commercial/{mod,http,runtime,config}.rs`、`aero-storage/src/snaplink_commercial.rs`、`migrations/0235_*.sql`（A6 no-touch，已实测 diff 为空）。snaplink usage relay 与 audit connector 零交叉：connector 只 claim `audit_governance_outbox`，永不写 v1 表。
- **零新第三方依赖**：全部 workspace-pinned；唯一直接声明 `base64 = "0.22"` 已在 lock（aero-server/aero-live-srt 同版本）。
- 不碰 `aero-live-rtmp`（B5-1 的 in-tx 审计写入面）；不碰 root Cargo.toml 的 str0m 规则。

## 3. 状态机规范（含唯一强制 delta）

### 3.1 分类 → 转换（`client.rs` + `relay.rs::deliver_claim`）

| 结果类 | 触发 | 转换 | 依据 |
|---|---|---|---|
| Ok | 202 + 回执校验通过 | `settle`（status 0/1→2，fenced 单事务） | E1 :144-153 |
| `Forbidden` | HTTP 403 | `mark_dead` **attempt 1 即死**，无重试（T-11 fail-closed） | R5 |
| `Permanent` | 422 / 409 / 回执不匹配 / 本地 payload guard | attempt 1 → `requeue`（backoff(1)=1s）；attempt ≥2 → `mark_dead`（**≤1 次重试后终态**） | R5 |
| `Transient` | 传输/超时/5xx/未指明 4xx/claim 漂移/401-after-refresh | `requeue`（backoff(attempts)，**永不 dead**——保留 v1 retry-forever 姿态） | R5/R6 |

每行 **恰好一个** 转换；`settle` fenced 于 **`claim_token + lease_expires_at > clock_timestamp()`（不含 `attempts`**——claim 每次轮换 `gen_random_uuid()` token 并自增 attempts，token 唯一标识 claim 代，stale worker 无法以（现 token, 旧 attempts）出现；fenced 重读 + UPDATE 单事务，`pg.rs::settle`/`fake.rs::settle` 同形）；`requeue/mark_dead` 带 **完整三重栅栏** `claim_token AND attempts AND lease_expires_at > clock_timestamp()`。所有 fence 违例 stale worker 得 `false` 而非错误（E1 :352-354 / E3 :297-304 / E4 :134-170 克隆）。出站 `Idempotency-Key = event_id`（E1 :157 克隆）——lost-ack 重投在 sink 侧幂等，**前提是 0239 sink 契约保证已投递 Idempotency-Key 的重放返回 202 + 原始回执（绝不可 409/conflict）**（见 §5 O6）。

### 3.2 纯函数（无时钟、无 DB，单测）

- `audit_backoff(attempts: i64) -> Duration`：`2^(attempts-1)` 秒 cap 300s；`1→1s, 2→2s, 3→4s, …, 9→256s, 10..=i64::MAX→300s`（E3 `retry_delay` 同序列，i32 版本 99→300s 已对齐）。
- `clamped_lease`：`[1s, 86_400s]`。
- ~~当前缺~~ ✅ 已落地：**`is_dead_at` 命名纯函数**（webhook_delivery.rs:76-81 形状，见 §3.3）。

### 3.3 ⚠️ R5 强制 delta（✅ 已落地，本文档为验收基线）

§3.3 落地前 `relay.rs:193` 为内联 `if attempts < 2`。要求（spec R5 + direction 明言）按 webhook_delivery.rs 形状提取，已实现如下：

```rust
/// Permanent-class dead threshold: dead after ≤1 retry (attempt 1 → requeue,
/// attempt ≥ 2 → mark_dead). Pure + total, unit-tested without clock/DB
/// (webhook_delivery.rs::is_dead_at shape).
pub const PERMANENT_DEAD_AT: i64 = 2;

#[must_use]
pub fn is_dead_at(attempts: i64) -> bool { attempts >= PERMANENT_DEAD_AT }
```

- `deliver_claim` Permanent 臂改为 `if is_dead_at(attempts) { mark_dead } else { requeue }`（单决策点，SQL 分支模式同 E2 :236-277）。
- 新增单测：`is_dead_at(1)==false, is_dead_at(2)==true, is_dead_at(i64::MAX)==true`（`relay.rs` cfg(test)，无时钟无 DB）。
- **403 臂不用 is_dead_at**（403 语义是 fail-closed 立死，与重试预算无关）——保持 `Forbidden` 单独臂，注释钉死区别。
- 连带：`PERMANENT_DEAD_AT` 导出供 `tests/state_machine.rs::permanent_error_dead_after_exactly_two_attempts` 引用（断言 `attempts == 2` 处用常量而非裸数字）。

## 4. Failure modes（F 表，与 aero-ai B5-2 design 的 F1–F13 编号对齐）

| # | 故障 | 行为 | 终态 |
|---|---|---|---|
| F1 | claim SQL 错误（0239 未落 / DB 抖动） | 该 tick warn，下 tick 重试；**不 crash** | 行保持 durable |
| F2 | settle 丢 fence（lease 过期 / token 被轮换） | warn "lost its lease before acknowledgement; idempotent retry will recover" | 行可被重 claim → 重投（sink 幂等） |
| F3 | POST 成功后 settle 报错 | warn；lease 过期后重 claim 重投 | 同上（Idempotency-Key 兜底） |
| F4 | transient（传输/超时/5xx/未指明 4xx） | `requeue` + backoff(attempts)，**永不 dead** | Ready（可重 claim） |
| F5 | 401 on delivery | invalidate token → refresh once → **重验 claim 后**重试 POST；二次 401 → transient requeue | Ready |
| F6 | claim 校验拒绝（iss/aud/scope/sub 不匹配或 opaque 非 JWT） | **fail-closed：该 token 不发任何 POST**（`stub.posts()==0`）；invalidate + transient requeue（IdP 漂移，B5-4 provisioning 修） | Ready |
| F7 | HTTP 403 | `mark_dead` attempt 1（T-11）；丢 fence → warn "lease expiry will reclaim"（重 claim 重投 = 错误配置下的恢复路径） | **Dead** |
| F8 | permanent 类（422/409/回执不匹配/payload guard） | attempt 1 requeue（1s）；attempt ≥2 dead；`last_error` 记录类别 | **Dead**（永不再 claim；status 3 可查询，运维回收属 B5-1/工具面） |
| F9 | requeue/mark_dead 丢 fence（superseded attempt 迟到） | warn，不覆盖新 claim；lease 过期后由新 claim 决定命运 | 取决于新 claim |
| F10 | token endpoint 非 200 / 畸形 | transient（未发 POST），requeue | Ready |
| F11 | lease 不变量违例 / env 不全 / 非 HTTPS | **boot error fail-loud**，relay 不启（operator 错误启动期即暴露） | — |
| F12 | shutdown drain 超时 | warn "rows remain durable"；下次 boot 重新 claim | 行 durable |
| F13 | 0239 未迁移时 boot | relay 照常 spawn，每 tick claim Err → warn（既定降级，非缺口）；drill exit 2 | — |
| F14 | \|app↔DB skew\| ≥ lease 活锁 | **结构上不可能**：trait 无 `now` 参数，单一时钟域；`skew_gt_lease_cannot_livelock_claim_fence_settle` 钉死 | — |
| F15 | 并发超订 / 批超限 | `MAX_CLAIM=500` 批上限 + concurrency 1..32 配置钳制；`SKIP LOCKED` 两会话各拿不相交行（PG 并发测试钉死） | — |

## 5. Open items / 刻意偏差

- **O1（tenant 回执校验弱化）**：port 只查 `tenant_id` 非空，v1 是精确匹配 `claim.tenant_id`。根因：`Claim` 无 tenant 字段（0239 payload 无 tenant 且出站禁止 tenant_id）。决议：**接受现状**（存在性检查仍挡住「缺 tenant 的畸形回执」），待 B5-1 0239 DDL 落地时若 outbox 行携带 tenant 绑定则升级为精确匹配——升级点唯一，回执校验函数一处。R7 措辞 "tenant_id matches" 在 §6 验收映射中按「非空」解释，并向 B5-1 传递该协调项。
- **O2（签名不验证）**：claim 校验是 base64url 解码 payload 比较（v1 同信任姿态）；JWKS 签名验证属 IdP/B4-2 面，[PROPOSED]。fail-closed = 任何不匹配不发 POST（A5 钉死）。
- **O3（v1 退役）**：双表并存过渡；退休/redirect v1 内嵌 relay 是 B5-1/campaign cutover 决策，本方向不做。
- **O4（重复是刻意的）**：A6 禁止碰 snaplink，故 connector 在自己的 outbox 上重实现 proven SQL 形状；A1–A5 镜像 proven machine 断言来钉住等价性。
- **O5（0239 状态常量）**：status 0/1/2/3 数值以 B5-1 DDL 为规范；`pg.rs` 常量 + PG 测试的 `ensure_outbox_table`（0239 缺失时建最小等价表，仅 throwaway DB）是 seam，不是本方向拥有的 DDL。
- **O6（重复投递重放契约，钉给 B5-1 0239 sink）**：本方向每 POST 携带 `Idempotency-Key = event_id`（`client.rs:143`，含 401-refresh 重试），但 `client.rs:170-171` 把 **HTTP 409 → `Permanent(Conflict)`**、回执 `conflict=true` → permanent 类 → `is_dead_at` 后 status 3。因此 **0239 sink 契约必须保证：已投递 Idempotency-Key 的重放返回 202 + 原始回执（含原 `event_id`/`accepted_at`/status，`conflict=false`），绝不可回 409 或 `conflict=true`**——否则 delivered-but-unacknowledged 事件（F2/F3 lost-ack 经 lease 过期重投）会被误分类 permanent 而 false-dead。同 O1 的协调类别：契约文本在 out-of-repo 0239 文档，本方向只钉 in-repo 要求；若契约与 in-repo 规范读法矛盾，改的是 §8 风险节的分类阈值而非 Idempotency-Key 机制。

## 6. Migration steps（无本方向拥有的 DB 迁移；0239 归 B5-1）

1. **本方向落地（✅ 已完成）**：按 R1–R9 核对 port → §3.3 `is_dead_at` 纯函数 + 单测已落地（`relay.rs:42/48`，403 臂单独保留 `:190`）→ `cargo test -p aero-audit-connector` = **25 pass + 1 ignored**（lib 8 / claim_validation 11 / state_machine 6）→ `cargo check --workspace` / `cargo clippy --workspace --all-targets`（零新警告）→ `scripts/{truth-check,file-size-check}.sh` → 提交（port 目前未提交未评审，spec 验收集即评审清单）。
2. **B5-1 落地后**：0239 DDL + `audit_governance.rs` repo + RTMP 生命周期 in-tx 入队；`pg.rs` 绑定真实表（`ensure_outbox_table` 回落路径可保留给 throwaway）；status 常量与 DDL 规范值对齐；跑 A7 drill（`bin/aero-audit-relay-drill.rs`：N 行 enqueue → 全 status 2，`SELECT event_id FROM outbox WHERE status=2` set-equals `SELECT id FROM audit_events`，无 0/1/3 残留）。
3. **并行**：B5-3（claim 优先级，`aero-audit-priority-drill` 是 seam 非本方向产物）、B5-4（provisioning gate，消费本方向的 403→dead T-11 行为）。
4. **A6 常驻门**：每步 `cargo test --workspace --lib -- --ignored`（snaplink 组绿）+ `git diff` 空（usage relay no-touch）。

## 7. Testable acceptance mapping（A1–A7 → 测试符号 → 状态）

| 验收 | 测试符号 | Harness | 状态 |
|---|---|---|---|
| A1 403→dead ≤1（T-11，`posts()==1`、`attempts==1`、不再可 claim、last_error 记录） | `tests/state_machine.rs::forbidden_dead_on_first_attempt` | fake+stub，无 DB | ✅ 绿 |
| A2/A3 422/409/回执错 → 恰 2 次尝试后 dead（参数化 3 臂：attempt1 Ready+`available_at==t0+1s`+last_error 含类别；attempt2 Dead+`attempts==2`） | `tests/state_machine.rs::permanent_error_dead_after_exactly_two_attempts` | 同上 | ✅ 绿（§3.3 后引用 `is_dead_at`/`PERMANENT_DEAD_AT`） |
| A4 lease fencing：stale token 不能 ack（`token_b != token_a`、`settle(id, token_a)==false`）；|skew|≥lease 不活锁；PG 双会话并发 50 行各 25 零双 claim；`audit_backoff` 纯函数 1→1s…i64::MAX→300s 单调有界 | `stale_token_cannot_ack_after_reclaim`、`skew_gt_lease_cannot_livelock_claim_fence_settle`、`pg.rs::concurrent_double_claim_across_two_sessions_is_impossible`（--ignored）、`relay.rs` backoff/clamp 单测 | ✅ 绿（PG 项已在 throwaway DB 实跑通过：50 行、2×25 不相交、token 互异、attempts 全 1） |
| A5 claim 校验：6 拒绝 + 1 正向 + 2 刷新路径；**拒绝时 `stub.posts()==0` 字面 0** | `tests/claim_validation.rs`（wrong_issuer / missing_audience / missing_audit_scope / wrong_subject / opaque_non_jwt / valid_token / unauthorized_refreshes_once / refreshed_token_must_repass / client_classifies_statuses / payload_guard_rejects_tenant_selection_as_permanent） | 同上 | ✅ 绿 |
| A6 usage-relay 回归 + no-touch | snaplink 既有单测/db_tests（`cargo test --workspace --lib -- --ignored`）+ `git diff` 空门 | workspace + review gate | ✅ 当前成立（diff 已实测空） |
| A7 boot 接线 + relay drill（1:1 event_id parity，无 0/1/3 残留） | `main.rs:251-259` + `bin/aero-audit-relay-drill.rs` + `test-integration.sh` throwaway-DB 槽 | integration | ⏸ 等 B5-1 0239 落地（F13 降级态：drill exit 2） |

## 8. Risks

- 422/409 语义与 claim 契约全文在 out-of-repo v2 文档（[PROPOSED]）：本设计钉 in-repo 规范读法（permanent ⇒ 重试一次 ⇒ dead；403 ⇒ 首试即死）；契约文本若矛盾只改 R5 阈值，A2/A3 参数化测试是强制点。
- 「37/37」清单 out-of-repo：A7 先钉 in-repo drill，契约落地后按 proposal line 15 命名钉入 `test-integration.sh`。
- port 未提交未评审：验收集即评审清单；R5 是唯一已知代码缺口。
- 0239 落地前 drill 不能全跑：F13 是既定降级不是缺陷。

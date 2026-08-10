# Requirements Spec — B5-2: 抽取 aero-audit-connector（AiWorker/ConsumerEventReceipt lease/backoff/dead 状态机补齐 §1.2 语义）

- **Module (analysis root)**: `crates/aero-ai/src` — read-only pattern source; **zero aero-ai code changes**
- **Direction**: "B5-2: 把内嵌 Snaplink relay 抽取为 crates/aero-audit-connector，用 AiWorker/ConsumerEventReceipt 的 lease/backoff/dead 状态机补齐 §1.2 语义（422/409→dead≤1、403→dead=T-11、cc+claim 校验）"（value 8 / risk_reduction 8 / effort 6 / confidence 7）
- **Source analysis**: `docs/auto/analyses/crates-aero-ai-src-1bbf99ce.json` (direction #2)
- **Campaign**: `aero-im-b5-outbox-relay`（`docs/campaigns/campaign-aero-im-b5.yaml`）；in-repo contract anchor `docs/proposals/audit-contract-batch-aero-im.md`（v2 契约正文与 §1.2 原文在仓外，[PROPOSED]）
- **Status**: Requirements（下述证据全部经源码 grep 核对 + 测试实跑验证）
- **Verification date**: 2026-08-07。行号是核对时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点（AGENTS.md §0）。

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-server/src/snaplink_commercial/http.rs:23,148`（`SCOPE_AUDIT`、bearer 投递 + 401 失效） | ✅ **Verified**。`:23` = `const SCOPE_AUDIT: &str = "audit:event:write";`。`:144-149` = `deliver` 的 Audit 分支（`CredentialRole::Audit` + `SCOPE_AUDIT` + `StatusCode::ACCEPTED`，:148 在该臂内）。`deliver` 全貌 :133-180：POST `audit_events_url` + `bearer_auth` + `Idempotency-Key: claim.idempotency_key`；`UNAUTHORIZED` → `invalidate_token`（:155-158）。token 缓存 :182-232（ttl 折算 `refresh_at`，锁不跨网络 I/O）；`request_token` :233-254（`basic_auth` + `grant_type=client_credentials` + `scope` + `resource`）。`validate_token` :353-364 **只做形状校验**（非空/≤16KiB/无控制字符/bearer 类型）——**无 JWT claim 校验**，direction 的「该部分为 proposed」属实。回执校验 `validate_audit_receipt` :330-351（event_id/tenant_id 匹配、`accepted_at` 必在、`conflict=false`、status ∈ {ledgered,indexed,archived}）。 |
| E2 | `crates/aero-server/src/snaplink_commercial/runtime.rs`（relay 循环、from_env、ready） | ✅ **Verified**。`from_env` :37-72（`CommercialMode` 三态：Unspecified→`require_disabled` / Disabled→`configure_disabled` / Enabled→`configure_enabled` + 建 `MachineBinding`）；`ready` :74-76；`run_delivery_relay` :209-229（`interval(delivery_poll_interval)` + `MissedTickBehavior::Skip` + cancel 时 `shutdown_drain`）；`dispatch_batch` :238-258（`reconcile_usage` → `reconcile_audit` → `claim_due` → `for_each_concurrent`）；`deliver_claim` :263-293（Ok→`mark_delivered`，Err→`mark_failed`，fence 丢失仅 warn「lease expiry will reclaim」）；`binding_for_claim` :295-312（tenant/client/source 三方身份核对）。**无任何代码路径把行移出重试集合**——dead 终态确实缺席。 |
| E3 | `crates/aero-server/src/snaplink_commercial/config.rs`（端点/退避配置） | ✅ **Verified**。端点 :97-118（`AERO_SNAPLINK_TOKEN_ENDPOINT` / `BILLING_BASE_URL` / `AUDIT_BASE_URL`，audit 拼 `api/v1/events?wait_for=ledgered`）；限值 :236-268：`request_timeout`（默 10s，1..120）、`delivery_lease`（默 30s，5..300）、**`delivery_lease <= 2×request_timeout + 2s` 即 boot-time `bail!`**（:247-251，`AERO_TASK_DRAIN_SECS` 同款不变量 :253-255）、`delivery_poll_interval`（默 250ms）、`batch_size`（默 100，1..500）、`concurrency`（默 16，1..128）、`shutdown_drain`（默 5s）。**「退避 cap 300s」在存储层已存在**（见 E7）。 |
| E4 | `crates/aero-ai/src/worker/mod.rs`（`run_loop`/`run_one`: claim→defer→process→complete/fail、`MAX_ATTEMPTS=5` dead-letter、`is_over_attempt_cap` 防重复付费） | ✅ **Verified**。`pub const MAX_ATTEMPTS: i32 = 5` :69；`is_over_attempt_cap` :523（`attempts > MAX_ATTEMPTS`，:827 run_one 超 cap 即 dead-letter **不 spend**）；`run_loop` :601-745（claim 按剩余预算限量 → per-ws `KeyedCostBudget` defer（不耗 retry）→ 全局加权 `try_acquire_n` all-or-nothing defer → `process_batch`）；`process_batch` :747-796（`Semaphore` 限并发 + cancel 优雅 drain）；`run_one` :798-880（`JOB_TIMEOUT=120s` :812 → `Disposition::{Done,Failed}` → `complete`/`fail`）；`fail_job` :882-885 传 `MAX_ATTEMPTS`；`JobQueue` trait :141-151（claim/complete/fail/defer）；`run` :291-327（CancellationToken + `CostBudget`）。**dead-letter 判定 = 存储层 `fail` 的 attempts 阈值 + 进程内 `is_over_attempt_cap` 双保险**——connector 的 dead 语义克隆此模式。 |
| E5 | `crates/aero-storage/src/ai_job.rs:174-180`（`FOR UPDATE SKIP LOCKED` + `ORDER BY priority ASC` claim） | ✅ **Verified**。`claim` :171-203：`UPDATE ai_jobs SET status='running', attempts=attempts+1 … WHERE id IN (SELECT … WHERE status='queued' AND scheduled_at <= NOW() ORDER BY priority ASC, scheduled_at ASC FOR UPDATE SKIP LOCKED LIMIT $1)`（ORDER BY :179，SKIP LOCKED :180）。`fail` :205-221：`status = CASE WHEN attempts >= $3 THEN 'dead' ELSE 'queued' END` + 退避 `LEAST(attempts,5)*5s`——**in-repo dead 终态先例**。`defer` :226+ 不耗 retry。 |
| E6 | `crates/aero-storage/src/consumer_event_receipt.rs:29-69`（lease + fencing token claim/expire） | ✅ **Verified**。`ConsumerEventClaim::{Claimed{attempts},Completed,Busy}` :17-41；`claim` :43-99（:29-69 覆盖 enum 尾部 + claim 头部）：INSERT … `ON CONFLICT (consumer,event_id) DO UPDATE … WHERE state='processing' AND lease_expires_at <= $4` RETURNING attempts——**attempts 即 fencing token，过期可重领、重领后旧 attempts 失效**（renew :101-132 与 complete :134-170 均带 `attempts = $3 AND lease_expires_at > clock_timestamp()` 双栅栏）。 |
| E7 | `crates/aero-storage/src/ai_usage.rs`（稳定 operation id + reserve/finalize 付费幂等模式） | ✅ **Verified**。`MAX_LEASE_SECONDS=86_400` :14、`MAX_BACKOFF_SECONDS=300` :15；`reserve` :160 / `finalize` :289 / `cancel` :362 / `recover_expired_reservations` :401（稳定 operation id + `gen_random_uuid()` reservation_token 栅栏，成功按真实用量 finalize、普通失败 cancel、超时保守结算——**付费调用先 reserve 再出网**）；`claim_due` :451（CTE + SKIP LOCKED + 轮换 token）、`settle` :497（单事务 fenced ack）、`mark_failed` :576（文档 :574-575 明言 **「deliberately no dead-letter」**——审计 outbox 必须反其道补 dead 终态）、`ai_usage_backoff` :683-688（`2^(attempts-1)` 秒 cap 300s）、`clamped_lease` :693。 |
| E8 | （补充）v1 outbox DDL 无 dead 终态 | ✅ **Verified**。`migrations/0235_snaplink_commercial_control_plane.sql:161-184`：`snaplink_delivery_outbox` 有 `attempts`/`claim_token`/`lease_expires_at`/`delivered_at`/`last_error` + `UNIQUE(destination,idempotency_key)` + claim-state CHECK，**无 status 列、无 dead 状态**；`delivered_at IS NOT NULL` 是唯一终态。存储层 `snaplink_commercial.rs`：`SnaplinkDeliveryClaim` :77、`claim_due` :302（同一 SQL 形状）、`mark_delivered` :344、`mark_failed` :361、`commercial_delivery_backoff` :710-719（`MAX_BACKOFF_SECS=300` :18）——**退避 300s cap 在 v1 存储层已存在**。 |
| E9 | （补充）relay 内嵌 aero-server，非独立 crate | ✅ **Verified**。`crates/aero-server/src/lib.rs:106 pub mod snaplink_commercial;`（{mod,http,runtime,config}.rs 四文件）；boot 装配 `crates/aero-server/src/bin/boot/state_builder.rs:16-17`（`Option<Arc<SnaplinkCommercialRuntime>>`）、`main.rs` spawn。 |
| E10 | （补充）`room.create` 审计动作存在 | ✅ **Verified**。`crates/aero-ai/src/governance.rs:140`（admin 类映射表含 `room.create`）、:180（`room.create` 属未映射 pass-through token——B5-3 关注点；B5-2 的 relay 与 action 无关，只投递 outbox 行）。 |

**实跑验证**（2026-08-07）：`cargo test -p aero-audit-connector` → **24/24 通过** + 1 项 `--ignored` PG 并发双 claim 测试（见 §2.3）。

### 1.1 对 direction problem 陈述的勘误（evidence-backed）

- 「无 §1.2 的 lease>2×timeout」**部分不成立**：v1 `config.rs:247-251` 已有 boot-time `bail!`（`delivery_lease > 2×request_timeout + 2s`），`AERO_TASK_DRAIN_SECS` 同款（:253-255）。新 connector 的 config 克隆此不变量（`config.rs:72` `check_lease_invariant`）。
- 「退避 cap 300s」**在存储层已存在**：`snaplink_commercial.rs:18,710-719`（`MAX_BACKOFF_SECS=300`）。connector 的 `audit_backoff`（`relay.rs:44-52`）克隆 `ai_usage_backoff` 同序列。
- **真实缺口（本 direction 要补的）**：① relay 内嵌 aero-server 非独立 crate（E9）；② 无 dead 终态——所有失败无限重试（E8），422/409/回执错无终态语义；③ 无 JWT claim 校验（iss/aud/scope/sub），v1 只形状校验 bearer（E1）；④ 403（配给被拒）被当作普通失败重试而非 fail-closed 终态。

## 2. Verified current state

### 2.1 本仓可移植的状态机素材（direction 引用的两套 + 一套补充）

```
a) AiWorker run_loop（进程内调度 + dead-letter 双保险）  crates/aero-ai/src/worker/mod.rs
   run_loop :601  claim(按预算限量) → per-ws/全局 budget defer（不耗 retry）→ process_batch（Semaphore）
   run_one  :798  is_over_attempt_cap 超限不 spend 直接 dead → timeout(120s) → Done/Failed → complete/fail
   MAX_ATTEMPTS=5 :69；storage fail() 的 CASE 阈值 :205 才是 dead 落库点（E5）

b) ConsumerEventReceipt lease + fencing      crates/aero-storage/src/consumer_event_receipt.rs
   claim :43   attempts 自增即 fencing token；ON CONFLICT … WHERE lease_expires_at <= now 过期可重领
   renew/complete :101/:134  双栅栏 attempts = $3 AND lease_expires_at > clock_timestamp()

c) AiUsageRepo 付费幂等 + 退避（补充模式源）  crates/aero-storage/src/ai_usage.rs
   reserve/finalize/cancel :160/:289/:362；claim_due :451（SKIP LOCKED + 轮换 token）
   settle :497（单事务 fenced ack）；mark_failed :576（明言无 dead-letter——审计场景须反其道）
   ai_usage_backoff :683（2^(attempts-1) cap 300s）；clamped_lease :693
```

### 2.2 v1 内嵌 audit relay（被抽取对象）

`aero-server/src/snaplink_commercial/`：cc token 缓存 + 401 失效（http.rs）、claim→deliver→mark_delivered/mark_failed（runtime.rs）、端点/限值配置 + lease 不变量（config.rs）；存储 `snaplink_delivery_outbox`（0235）无 status/dead（E8）。usage 目的地（`SnaplinkDeliveryDestination::Usage`）与 entitlement 投影路径**原地不动**。

### 2.3 工作树中已有的 B5-2 实现（上一批次产物，未提交）

本 direction 的交付物 `crates/aero-audit-connector/` **已存在于工作树**（untracked，连同 `docs/design/2026-08-06-aero-audit-connector.design.md` 与上一版 req 文档）。现状盘点（均已核对源码）：

| 文件 | 内容 |
|---|---|
| `src/outbox.rs` | `OutboxRepo` trait（claim_due/settle/requeue/mark_dead，全部 fencing token 栅栏，签名镜像 `AiUsageRepo`） |
| `src/relay.rs` | 状态机策略（F1-F13）：成功→settle；transient→requeue 永不 dead；permanent（422/409/receipt/payload-guard）→第 1 次 requeue、第 2 次 mark_dead；**403→立即 mark_dead（T-11）**；`audit_backoff`/`clamped_lease`/`MAX_*` 常量；`spawn(cancel)` + `shutdown_drain` |
| `src/client.rs` | cc token（缓存+401 失效）、**JWT claim 校验（iss/aud/scope/sub）POST 前置**、`DeliveryError::{Transient,Permanent(422/409/ReceiptMismatch/PayloadGuard),Forbidden}`、回执校验（克隆 `validate_audit_receipt`）、payload guard（克隆 `validate_delivery_payload`） |
| `src/pg.rs` | PG impl 绑定 B5-1 的 `audit_governance_outbox`（status 0/1/2/3 常量）；**不碰 v1 `snaplink_delivery_outbox`**；0239 未落库时 claim 报错降级不崩溃（F13） |
| `src/fake.rs` / `src/stub.rs` | 内存 fake outbox（Ready/Claimed/Delivered/Dead）+ stub sink（token 端点发 JWT、events 端点 202+回执、POST 计数、`unauthorized_once`） |
| `src/config.rs` | `AERO_AUDIT_*` 环境配置，presence-gated（无 `AERO_AUDIT_TOKEN_ENDPOINT` 且无其他 `AERO_AUDIT_*` → `Ok(None)`；有残留变量 → boot 错误）；`check_lease_invariant` :106 / `check_drain_invariant` :118 克隆 v1 |
| `src/bin/aero-audit-relay-drill.rs` | A3 drill：throwaway DB 种子 N 行 → stub sink → 断言 `COUNT(status=2)==N` + event_id 集合 parity；表缺失 exit 2 |
| `tests/state_machine.rs`（6 tests） | `stale_token_cannot_ack_after_reclaim` / `backoff_is_bounded_and_exponential` / `permanent_error_dead_after_exactly_two_attempts`（422/409/坏回执参数化，fake 时钟钉死 → `available_at` 精确 `==t0+1s`）/ `forbidden_dead_on_first_attempt` / `happy_path_settles_and_removes_from_claimable` / `skew_gt_lease_cannot_livelock_claim_fence_settle` |
| `src/relay.rs` tests（5 tests） | backoff 序列 / lease clamp / **`transient_{5xx,timeout,claim_drift}_requeues_and_rotates_a_fresh_token`**（中继级 Transient 臂：500 / 超时（stub `delay_ms`）/ claim 漂移（坏 iss）→ requeue 永不 dead + 下次 claim 新 token 轮换） |
| `tests/claim_validation.rs`（11 tests） | wrong iss/aud/scope/sub、opaque 非 JWT、payload guard、401 刷新重试（含刷新后 token 重过 claim 校验）、有效 token 放行——拒绝场景全部断言 **POST 计数 == 0** |
| `src/pg.rs` tests（1 test，`--ignored`） | `concurrent_double_claim_across_two_sessions_is_impossible`：两独立 session（双 pool + barrier）各 `LIMIT 25` 并发 claim 50 行 → 各自恰 25、零交集、`attempts==1` 全行、`attempts>1` 计数 == 0（SKIP LOCKED 实测）；自带 0239 等效表探测+自建，无需 B5-1 |
| 接线 | `aero-server/src/bin/main.rs:245-272`（B5-2 段：presence-gated spawn，`CancellationToken` 共享）；`aero-server/Cargo.toml:36`；`scripts/test-integration.sh:236-271`（0239 存在时跑 drill，否则显式 SKIP）；root `Cargo.toml:29` workspace member |

**依赖**：全部 workspace-pinned；唯一直接声明 `base64 = "0.22"`（已由 `aero-server/Cargo.toml:78` 与 `aero-live-srt/Cargo.toml:35` 钉住——**零新增第三方供应链项**，JWT 校验用 base64url 手解 payload，不引 jsonwebtoken）。

**测试实跑**（`cargo test -p aero-audit-connector`）：lib 7（config 2 + relay 5）+ claim_validation 11 + state_machine 6 = **24/24 ok**（含 `lease_must_exceed_two_request_timeouts_plus_slack`、`forbidden_dead_on_first_attempt`、`permanent_error_dead_after_exactly_two_attempts`、`skew_gt_lease_cannot_livelock_claim_fence_settle`、3 项 transient 中继测试等全部验收映射测试）；另 `pg.rs::concurrent_double_claim_across_two_sessions_is_impossible`（`--ignored`，需 `DATABASE_URL`，实跑 ×4 绿）。

**未闭合项**（本 spec 的验收据此定位）：0239 迁移未落（migrations 尾号 0238）→ PG 侧 drill 与 room.create 全链路被 B5-1 门控（并发双 claim PG 测试除外——自带 0239 等效表探测+自建，已实跑）；上一批次实现未经 `cargo clippy`/`truth-check` 全量回归（工作树未提交状态）。

## 3. Scope

**In scope（B5-2）**：
- 独立 crate `crates/aero-audit-connector`（已在工作树；验收 = 补全/回归到全绿）。
- 状态机 = AiWorker/ConsumerEventReceipt 模式移植：leased claim（轮换 token + SKIP LOCKED + attempts 作 fence）、fenced settle、退避 requeue（cap 300s）、**dead 终态**（克隆 `AiJobRepo::fail` 的 attempts 阈值语义）。
- §1.2 语义：422/409/回执错 → **dead ≤1 次重试**（attempt 1 requeue、attempt 2 dead）；**403 → dead 于首次尝试（T-11 fail-closed）**；transient 永不 dead。
- cc + claim 校验：`client_credentials` + `audit:event:write` + **POST 前 JWT iss/aud/scope/sub 校验**（[PROPOSED] 契约的 in-repo 规范化读法）。
- 配置 fail-loud（`AERO_AUDIT_*`，lease > 2×timeout + 2s boot bail，presence-gated）。
- 回归：usage relay 原地不动（v1 `SnaplinkDeliveryDestination::Usage` 路径零改动）。

**Out of scope**：
- 0239 DDL（status 0/1/2/3、class/priority、enqueue 重定向）与 `audit_governance.rs` 仓储 → **B5-1**；connector 经 `OutboxRepo` trait 消费，不拥有 DDL。
- priority 排序/反饥饿 → **B5-3**；`aero-cli audit-provision-check` 配给 seam → **B5-4**（403→dead 的 fail-closed 行为是本 direction 的，配给门是 B5-4 的）。
- **`crates/aero-ai` 任何代码改动：禁止**（`worker/mod.rs` 等仅作模式源，A6 no-touch 守卫）。
- v1 内嵌 relay 退役/0236 触发器重定向 → B5-1/campaign 决策。
- 仓外 v2 契约文档与 IdP scope registry → [PROPOSED] seam。

## 4. Requirements

### R1 — 独立 crate + `OutboxRepo` trait seam（已在工作树，验收锚定）
`crates/aero-audit-connector`（root `Cargo.toml:29` member）：`OutboxRepo`（async trait）：`claim_due(now, lease, limit) -> Vec<Claim>`（`Claim{event_id, claim_token, lease_expires_at, attempts, payload}`，event_id 即 `audit_events.id` = 稳定幂等键 + 出站 `Idempotency-Key`）、`settle(event_id, claim_token) -> bool`、`requeue(event_id, claim_token, attempts, now, error) -> bool`、`mark_dead(event_id, claim_token, attempts, error) -> bool`——全部 fenced（stale token 返回 `false` 非错误）。内存 fake 供单测；PG impl 绑定 B5-1 0239 表，**绝不写 v1 `snaplink_delivery_outbox`**。relay 循环 = `dispatch_batch`（claim ≤ batch → 有界并发投递 → 每行恰一个 settle/requeue/mark_dead）+ 共享 `CancellationToken` 优雅 drain。

### R2 — claim 契约：稳定 event-id + fencing token
每 claim 轮换 `gen_random_uuid()` token（克隆 `ai_usage.rs:475`）；`settle`/`requeue`/`mark_dead` 均栅栏于 `claim_token AND attempts` + 未过期 lease（克隆 `consumer_event_receipt.rs` renew/complete 双栅栏 + `ai_usage.rs` settle/mark_failed）；过期重领 → 新 token，旧 token 永不可 ack（`stale_token_cannot_ack_after_reclaim` 不变量）。

### R3 — leased claim 循环 + `lease > 2×timeout` boot 不变量
`check_lease_invariant`（`config.rs:106`，克隆 `snaplink_commercial/config.rs:247-251`）：`delivery_lease > 2 × request_timeout + 2s` 否则 boot `bail!`；`check_drain_invariant` :118 同款。lease clamp `[1s, 86_400s]`（`clamped_lease`）。claim SQL：`available_at <= now AND (lease IS NULL OR <= now)`、`ORDER BY (available_at, created_at, event_id)`、`FOR UPDATE SKIP LOCKED`、batch clamp（`pg.rs`，克隆 `ai_usage.rs:451-496`）。

### R4 — 退避 cap 300s
`audit_backoff`（`relay.rs:44-52`）：`2^(attempts-1)` 秒 cap `MAX_BACKOFF_SECONDS=300`，与 proven 序列对齐（1→1s, 2→2s, 3→4s, 9→256s, 10..→300s）。

### R5 — dead 终态（新增 vs ai_usage；语义克隆 `AiJobRepo::fail`）
- **permanent 类**（HTTP 422 / 409 / 回执校验失败 / 本地 payload guard）：attempt 1 → requeue（退避）；attempt ≥ 2 → `mark_dead`（**≤1 次重试后终态**；dead 行永不被 claim/lag 查询选中；`last_error` 记录）。
- **HTTP 403 → attempt 1 即 `mark_dead`**（T-11 fail-closed：无 requeue、无第二次投递）。
- **transient 类**（transport/timeout/5xx/未指定 4xx/401-after-refresh）→ requeue 永不 dead（保留 v1 retry-forever 姿态）。
- 与 ai_usage 的「deliberately no dead-letter」刻意相反：审计行不得无限重发。

### R6 — cc + claim 校验（POST 前置，fail-closed）
- token 请求克隆 v1（`basic_auth` + `grant_type=client_credentials` + `scope=audit:event:write` + `resource`），ttl 缓存 + 401 失效。
- **每个 token 在首次 POST 前校验 JWT claims**：`iss`=配置 issuer、`aud` 含配置 audience、`scope` 含 `audit:event:write`、`sub`=配置身份；任一不匹配 → 拒绝（该 token 永不出网，无 POST）；opaque 非 JWT → 同样 fail-closed。校验是**每次投递的前置**，非一次性 boot 检查。
- 401 on delivery → 失效 + 刷新一次 + 同一次尝试内重试（`unauthorized_refreshes_once_and_retries_within_the_attempt`）。

### R7 — 投递 + 回执校验（克隆 v1）
POST `events_url` + `Bearer` + `Idempotency-Key: {event_id}`；期望 **202 ACCEPTED**；payload guard（禁 tenant 选择、`source_system` 必须等于绑定值）；回执校验（event_id/tenant_id 匹配、`accepted_at` 必在、`conflict=false`、status ∈ {ledgered,indexed,archived}）。回执不匹配 = permanent 类（R5）。

### R8 — usage relay 原地不动
零改动：`crates/aero-storage/src/ai_usage.rs`、`crates/aero-ai/src/usage.rs`、`crates/aero-server/src/ai_usage.rs`（PgUsageSink）、`crates/aero-storage/src/snaplink_commercial.rs` 及 v1 usage 目的地（claim/settle/reconcile_usage）。已核 `git diff` 上述路径为空。

### R9 — 配置 fail-loud
`AERO_AUDIT_*` env：token_endpoint、events_url、resource、client_id/secret、expected iss/aud/scope/sub、request_timeout、delivery_lease（R3 bail）、batch_size、concurrency、poll_interval、shutdown_drain/task_drain（R3 bail）、source_system。presence-gated：无 `AERO_AUDIT_TOKEN_ENDPOINT` 且无其他 `AERO_AUDIT_*` → 功能关闭（`Ok(None)`）；有残留变量 → boot 错误（防静默降级）。无新第三方依赖（`base64 = "0.22"` 已在 workspace 供应链内）。

## 5. Acceptance checks（direction 原样保留，逐条 testable）

> 现状注：工作树已有实现且 24 项单测全绿 + 1 项 `--ignored` PG 并发测试（§2.3）。下述每条给出**精确测试/断言锚点**；未闭合项（PG drill、room.create 全链路）标注 B5-1 门控。

### A1（T-11）— 无 relay 配给（无 client_credentials/scope）→ 403 → dead 终态 ≤1 次尝试，不再重试（fail-closed）
- **测试**：`tests/state_machine.rs::forbidden_dead_on_first_attempt` — stub 返回 403：`row.status == Dead`、`row.attempts == 1`、`stub.posts() == 1`、后续 `claim_due` 空。
- **配给缺席面**：`config.rs::from_env` presence-gated（无 `AERO_AUDIT_TOKEN_ENDPOINT` → `Ok(None)`，relay 不启动 = fail-closed 不投递）；sink 侧身份被拒（403）→ 终态而非无限重试。
- **SQL 面**：`pg.rs::mark_dead` 置 status=3；claim/lag 查询 `WHERE status = 0/1`（dead 排除）。`mark_dead` 亦栅栏于 claim_token，fence 丢失仅 warn。

### A2 — 422/409/回执错 → dead ≤1 次（一个周期内终态，claim/lag 查询排除 dead 行）
- **测试**：`tests/state_machine.rs::permanent_error_dead_after_exactly_two_attempts` — 对 422 / 409 / `receipt_valid=false` 三参数化：attempt 1 → `FakeStatus::Ready`（requeue，`attempts==1`，`available_at ∈ [t0+backoff(1), t0+backoff(1)+δ]`——生产侧 PG 以 `clock_timestamp()+backoff` 盖戳，相对捕获的 t0 是**窗口**；单测钉死 fake 时钟断言精确 `==t0+1s`，`last_error` 含类名）；`make_due_now` 后 attempt 2 → `FakeStatus::Dead`（`attempts==2`）；后续 `claim_due` 空 = dead 排除。
- **回执错类别**：`client.rs::PermanentKind::ReceiptMismatch`（`validate_audit_receipt` 失败路径，`client_classifies_statuses` 单测覆盖）。
- **claim 排除面**：`pg.rs` claim CTE `WHERE status = 0 AND …`（dead=3 永不被选中）+ `FakeOutbox::claim_due` 同语义；A3 drill 的 `COUNT(status=2)==N` 隐含 dead 行不计入。

### A3 — lease > 2×timeout 的 fencing：并发两 claim 仅一者完成，过期后旧 claim 无法完成
- **测试**：`tests/state_machine.rs::stale_token_cannot_ack_after_reclaim` — lease=1s claim at t0（token A）→ t0+2s 重领（token B）：`token_a != token_b`；`settle(id, token_a) == false`（stale 永不可 ack）、`settle(id, token_b) == true`；settle 后 claimable 为空。
- **配置不变量**：`config.rs::lease_must_exceed_two_request_timeouts_plus_slack`（`check_lease_invariant`：lease ≤ 2×timeout+2s 即 Err）——`cargo test -p aero-audit-connector config::tests::*`。
- **并发面（真双 session，in-repo PG 测试）**：`pg.rs::concurrent_double_claim_across_two_sessions_is_impossible`（`--ignored`，需 `DATABASE_URL`）——50 行种子，两独立 session（双 pool + barrier）各 `LIMIT 25` 并发跑 claim CTE：各自恰 25、集合零交集、`attempts==1` 全行、DB 层 `attempts>1` 计数 == 0（`FOR UPDATE SKIP LOCKED` 实测；自带 0239 等效表探测+自建，B5-1 未落库也可跑）。拿到者的 token 是唯一可 settle/requeue/mark_dead 的（fence 见 R2）。

### A4 — 首个 room.create 事件经 cc+scope 到 mock sink 202 回执；无新第三方依赖（`cargo tree` 校验）
- **单元面**：`tests/state_machine.rs::happy_path_settles_and_removes_from_claimable`（202 + 有效回执 → `FakeStatus::Delivered`、token/lease 清空、claimable 为空）+ `tests/claim_validation.rs::valid_token_is_accepted_and_delivery_proceeds`（正确 iss/aud/scope/sub → `posts >= 1`）。
- **端到端面**（room.create 专属链路由 B5-1 门控）：`src/bin/aero-audit-relay-drill.rs` + `scripts/test-integration.sh:236-270` — throwaway DB 全链迁移 → 种子 N 行 → stub sink → `COUNT(*) WHERE status = 2 == N` + event_id 集合 parity（无重复无孤儿）。0239 未落时 drill exit 2 显式 SKIP。`room.create` 经 `audit_events` 触发器 → outbox 的 enqueue 段属 B5-1（`audit.rs::append_in_tx` + 0239 触发器）；0239 落库后 drill 可改为经 enqueue 路径种 `room.create` 审计行。
- **依赖面**：`cargo tree -p aero-audit-connector` — 全部 workspace-pinned，唯一直接声明 `base64 = "0.22"`（`aero-server/Cargo.toml:78` / `aero-live-srt/Cargo.toml:35` 已在图内）→ **零新增第三方依赖**。

### A5（proposed）— claim 校验：iss/aud/scope/sub 任一不匹配 → 拒绝且不重试
- **测试**：`tests/claim_validation.rs`（11 tests）— `wrong_issuer_is_rejected_before_any_post` / `missing_audience_is_rejected_before_any_post` / `missing_audit_scope_is_rejected_before_any_post` / `wrong_subject_is_rejected_before_any_post`：四场景 **`posts == 0`**（拒绝发生在任何 POST 之前，字面测量）+ `opaque_non_jwt_token_is_rejected_before_any_post`（不可校验即 fail-closed）+ `refreshed_token_must_repass_claim_validation_before_retry_post`（刷新后 token 重过校验）。
- **「不重试」的 in-repo 读法**（钉死）：被拒的**token** 永不用于投递（不携带坏 claim 的 token 重试 POST）；行本身按 transient requeue（下次 claim 轮换新 token——这是 IdP 侧修复后的唯一前进路径）。**中继级钉**：`relay.rs::transient_{5xx,timeout,claim_drift}_requeues_and_rotates_a_fresh_token` 直接驱动 `deliver_claim` Transient 臂（500 / 超时 / 坏 iss）→ 行 requeue 永不 dead（`available_at == repo 时钟 + backoff(1)` 精确）+ 下次 claim `token_b != token_a`（此前该半句仅靠代码形状钉，现为测试直接驱动）。若 v2 契约要求 claim 校验失败直接 dead，只改 `client.rs` 的分类（`DeliveryError::Permanent` 新成员），A2 参数化测试即 enforcement point。

### A6 — usage relay 行为不变（回归：既有 metering:write 路径测试全绿）
- **no-touch 守卫**：`git diff` 为空（已核）：`crates/aero-storage/src/ai_usage.rs`、`crates/aero-ai/src/usage.rs`、`crates/aero-server/src/ai_usage.rs`、`crates/aero-storage/src/snaplink_commercial.rs`（v1 usage 目的地）。唯一允许的 aero-server delta = 新增 connector 接线（`main.rs:245-272`）。
- **回归套件**：`cargo test --workspace --lib -- --ignored`（DATABASE_URL + 已迁移，经 `scripts/test-integration.sh`）：ai_usage db_tests 全绿（`stable_reservation_and_settlement_are_exactly_once`、`stale_claim_cannot_ack_after_reclaim`、`backoff_is_bounded_and_exponential`、`finalized_outcome_replays_after_business_commit_failure` 等，`ai_usage/tests.rs`）+ v1 snaplink 单测（`snaplink_commercial/http.rs` tests：`audit_delivery_requires_a_matching_durable_receipt` 等）+ `cargo test -p aero-audit-connector` 24 项 + `pg.rs` 并发双 claim 1 项（`--ignored`）。

## 6. Test placement

| Test | Location | Harness |
|---|---|---|
| A1 403→dead / A2 permanent≤1 / A3 fencing / A4 happy path / skew>lease 无活锁 | `crates/aero-audit-connector/tests/state_machine.rs`（6 tests）+ `src/relay.rs` 单测（backoff/clamp + 3 项 transient 中继测试）+ `src/config.rs` 单测（lease 不变量） | `cargo test -p aero-audit-connector`，无 DB |
| A5 claim 校验（iss/aud/scope/sub/opaque）+ 401 刷新 + 分类 | `crates/aero-audit-connector/tests/claim_validation.rs`（11 tests） | 同上（stub token/events 端点 + POST 计数） |
| A3 真并发双 claim（两 session × SKIP LOCKED） | `crates/aero-audit-connector/src/pg.rs::concurrent_double_claim_across_two_sessions_is_impossible` | `--ignored`，需 `DATABASE_URL` + 可写 PG；自带 0239 等效表探测+自建，B5-1 未落库也可跑 |
| A4 drill（N 行 → status 2 + event_id parity） | `crates/aero-audit-connector/src/bin/aero-audit-relay-drill.rs` + `scripts/test-integration.sh:236-270` | integration，throwaway DB + 全链迁移 + stub sink；0239 门控（缺表 exit 2） |
| A6 回归 | 既有 `crates/aero-storage/src/ai_usage/tests.rs` + `snaplink_commercial/http.rs` tests；review no-touch diff | `cargo test --workspace --lib -- --ignored`（DATABASE_URL） |

## 7. Risks / [PROPOSED]

- **§1.2 契约原文与「37/37」清单在仓外**（`docs/proposals/audit-contract-batch-aero-im.md` 明示）。本 spec 钉 in-repo 规范化读法：permanent → requeue 一次 → dead（≤2 总尝试）；403 → attempt 1 dead；transient 永不 dead。契约若冲突，只动 R5 阈值 + A2 参数化测试。
- **0239 未落库**（migrations 尾号 0238）：PG drill 与 room.create 全链路被 B5-1 门控；connector 现状降级为 logged claim errors（F13）而非崩溃——这是既定行为，不是缺口。
- **claim 校验 [PROPOSED]**：`iss/aud/scope/sub` 期望值来自仓外契约；本仓以配置注入（`RelayConfig.expected_*`）实现 seam，A5 测试以 stub JWT 验证。opaque token → fail-closed（无 POST）已钉。
- **上一批次实现未提交**：工作树中的 connector 是前一批次产物，本 direction 的落地动作 = 复核 + 全量门禁（`cargo check/test/clippy` + `scripts/{truth-check,file-size-check,web-check}.sh`）+ 提交；实现已存在不代表验收可跳过——A1-A6 全部重跑为提交前置。
- **状态枚举归属**：status 0/1/2/3 常量（`pg.rs::STATUS_*`）是 0239/B5-1 的规范值；connector 只是消费者，A3 断言按 B5-1 常量而非硬编码数字。

## 8. Sequencing

1. **复核阶段（本 direction 立即可做，无 B5-1 依赖）**：核对工作树 connector 实现与 R1-R9 逐条对齐（§2.3 已盘点）；重跑 `cargo test -p aero-audit-connector`（24/24 已验 + `pg.rs` 并发双 claim 1 项 `--ignored` 实跑 ×4）+ `cargo clippy --workspace --all-targets` 无新警告 + A6 no-touch diff 为空。
2. **B5-1 落库后**：0239 迁移 + 审计 outbox 仓储 + enqueue 触发器 → 解开 `scripts/test-integration.sh` drill 门（A4 端到端：`COUNT(status=2)==N` + parity；可换经 enqueue 种 `room.create` 行）；`cargo test --workspace --lib -- --ignored` 全绿。
3. **B5-3（priority）/ B5-4（provisioning seam）** 并行，不改 connector 状态机；B5-4 的配给门消费本 direction 的 403→dead（T-11）。
4. **A6** 是每个阶段的 standing gate。

# Requirements Spec — B5-1 producer side：AuditGovernanceOutboxRepo（H3）+ auth 域 in-tx 审计对 + L1 聚合（module: crates/aero-audit-connector）

- **Module (analysis root)**: `crates/aero-audit-connector`（consumer 侧已完整；本 direction 落地 producer 侧，机制全部落 `crates/aero-storage`，aero-auth/aero-server 只做调用与编排）
- **Direction**: "Land the B5-1 producer side: AuditGovernanceOutboxRepo (H3) with in-tx append + L1 aggregation so the relay has message.*/room.*/admin.* rows to claim"（value 10 / risk_reduction 8 / effort 8 / confidence 9）
- **Source analysis**: `docs/auto/analyses/crates-aero-audit-connector-40d338d1.json`（direction #1，proposed: append_in_tx / aggregate_login_failure_buckets / aero-auth in-tx pairs / L1 aggregation）
- **Campaign**: `aero-im-b5-outbox-relay`；contract anchor `docs/proposals/audit-contract-batch-aero-im.md:8`（B5-1 逐项设计行）；gate anchor `docs/campaigns/implementation-gate.md:64`（"`message.*`/`room.*`/`admin.*` 同事务写入；‡ 类走 L1"）
- **Sibling requirements/design**（同工作，模块根不同）：`docs/requirements/2026-08-08-aero-auth-b5-1-in-tx-audit-outbox.req.md`（R1–R8, AC1–AC5）+ `docs/design/2026-08-08-aero-auth-b5-1-in-tx-audit-outbox.design.md`（Design 状态，§2 API 签名、§2.6 L1、§2.7 token 表、§6 验收映射）——本 spec 与之同锚，不重复其论证，只钉住可验证需求与验收 oracles
- **Status**: Requirements（下述证据全部经源码 grep + 逐文件复核；行号为复核锚点，会漂移——**文件/符号**为准，AGENTS.md §0）
- **Verification date**: 2026-08-08

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `migrations/0239_audit_governance_outbox.sql`（trigger token-keyed 至 `message.moderated`；未知 token fail-open） | ✅ **Verified（精确）**。140 行。`aero_enqueue_governance_audit()`：Gate 1 runtime（disabled ⇒ RETURN NEW 零行）→ token 门 `NEW.action <> 'message.moderated' ⇒ RETURN NEW`（fail-open pass-through，零 RAISE）→ Gate 2 binding RAISE（fail-closed）→ `INSERT … ON CONFLICT (event_id) DO NOTHING`（class 'admin'、priority 100、status 0、完整 v2 信封含 `idempotency_key = NEW.id::text`）。AFTER INSERT 触发器 `audit_events_governance_enqueue`。表：`status CHECK (0,1,2,3)`、`class CHECK ('admin','message','room')`、`priority SMALLINT > 0 DEFAULT 10`、`delivery_mode CHECK ('push')`、`payload JSONB CHECK (jsonb_typeof='object')`、claim-state CHECK、due 部分索引 |
| E2 | `crates/aero-storage/src/audit_governance.rs:1-30`（模块 doc：`AuditGovernanceOutboxRepo` 经 handoff H3 落地；现仅 fixtures） | ✅ **Verified（精确）**。934 行；doc :22-24 原文 "The B5-1 storage direction's `AuditGovernanceOutboxRepo` lands in a sibling slice (handoff H3 of the 0239 design doc); this module carries the DDL-contract fixtures only — **add the repo to this same module when it lands**"。全仓 grep：`AuditGovernanceOutboxRepo` 仅此 doc 提及，**符号尚不存在**。既有 db_tests 6 个：`moderation_finalize_outbox_parity` :247、`ddl_contract_defaults_and_checks` :424、`moderation_finalize_runtime_disabled_commits_1_plus_0` :583、`non_moderation_action_passes_through_unmapped` :628、`duplicate_event_id_is_deduped_by_on_conflict` :693、`governance_reconcile_backfills_disabled_window` :774（`#[ignore = "requires live Postgres"]`） |
| E3 | `crates/aero-common/src/model/audit.rs:165`（`GOVERNANCE_CLASS_MESSAGE` 'L1-aggregatable'） | ✅ **Verified（行号微漂）**。`GOVERNANCE_CLASS_MESSAGE` :166，doc 原文 "High-volume message backlog class (L1-aggregatable)"；同模块 `OutboxStatus`（0..3，serde 整数 wire 契约）、`AuditClass`、`MODERATION_OUTBOUND_ACTION = "admin.content.flag"` :150、`LOCAL_ACTION_MODERATED = "message.moderated"` :156、`GOVERNANCE_CLASS_{ADMIN,ROOM}` :163/:168 |
| E4 | `crates/aero-ai/src/governance.rs:86-96, 195-211`（L1 bypass [PROPOSED]；`admin_class_rows_never_aggregated`） | ✅ **Verified（精确）**。`governance_lane_for` :86-99 唯一映射（`message.moderated` → admin/100/`admin.content.flag`/0；未知 → `None`）；`is_admin_class` :102-106，doc :84-85 "R5 classification for the [PROPOSED] L1 aggregation bypass"；单测 `admin_class_rows_never_aggregated` :195-201。常量 `GOVERNANCE_PRIORITY_MODERATION=100` :31 / `GOVERNANCE_PRIORITY_BACKLOG=10` :33。实跑：`cargo test -p aero-ai` 6/6 绿 |
| E5 | `docs/design/2026-08-08-aero-auth-b5-1-in-tx-audit-outbox.design.md`（Design 状态；AC1 register 原子性、AC3 L1 N→1 测试已命名未落地） | ✅ **Verified（精确）**。Status: Design。§6 验收映射：AC1 测试名 `registration_commit_half_writes_audit_and_outbox` / `registration_rollback_half_leaves_zero_rows` / `registration_audit_failure_fail_open_commits_domain`（另 login/PAT/TOTP 三路）、AC3 测试名 `login_failure_l1_aggregation_n_to_one`、AC2 命名条目 `auth_outbox_parity_1to1`——**全部未落地**（`rg` 全仓零命中）。§2.1 API 签名齐备（`append_in_tx`/`governance_envelope`/`append_pair_in_tx_fail_open`/`record_pair_standalone`/`aggregate_login_failure_buckets`）；§2.7 auth token 表（auth.* 本地 token → admin.auth.* 出站 token、class admin、priority **10**）；§2.6 L1（uuid-v5 确定性 event_id、class 'message'、`ON CONFLICT DO NOTHING`、无 audit_events 行） |
| E6 | `crates/aero-audit-connector/src/outbox.rs`（`OutboxRepo` trait 就绪：`claim_due` 带 priority/class lanes） | ✅ **Verified（精确）**。trait 五方法 `reconcile`/`claim_due`/`settle`/`requeue`/`mark_dead`；`Claim` 结构含 `priority: i16` + `class: String`（:90-104）；`pg.rs` 实现 `claim_due` ORDER BY `priority DESC, available_at, created_at, event_id` + `FOR UPDATE SKIP LOCKED`，单时钟域（`clock_timestamp()`）；`STATUS_ENQUEUED=0/CLAIMED=1/DELIVERED=2/DEAD=3`。**connector 零改动**——producer 侧新增行天然进 claim 车道 |

### 1.1 补充核对（本 spec 亲自复核的落地面）

| 锚点 | 核实结果 |
|---|---|
| aero-auth 零 audit/outbox 命中 | ✅ `rg -ic "audit\|outbox" crates/aero-auth/src/` = 0。`service.rs`：`register` :184、`register_enrolled` :221、`login` :260、`refresh` :334 |
| `AuditRepo::append_in_tx` :127（in-tx append 先例） | ✅ `audit.rs`：`append` :113、`append_in_tx(tx, workspace, actor, action, target, detail) -> Result<AuditId>` :127、generic executor `append_on` :140——auth 对必须骑它（返回 ULID `AuditId` 做 outbox event_id，1:1 前提） |
| `RegistrationRepo::create` 单事务 | ✅ `registration.rs`：`NewRegistration` :13、`create` :37（begin :38 → participants + credentials + workspace_members + 默认频道 + auth_sessions → commit :109）。`NewRegistration` 构造点 3 处（db_tests :156/:257、aero-auth `service.rs:233`） |
| login 2FA 门在 handler 侧 | ✅ `routes/handlers/auth.rs`：`finalize_login(&email, false)` :182 / `finalize_login(&email, true)` :197；`record_login_event` :35（`auth.login.new_ip`，pool 级 best-effort）；`DEFAULT_WORKSPACE_ID` :86/:211/:219（nil UUID，0006:61 种子行）——**`auth.login` 审计点必须定在 handler 侧 2FA 门之后**（design §2.4、D5） |
| PAT/TOTP/session 现状 | ✅ `pat.rs` `create` :104 / `revoke` :190（pool 级单语句）；`totp.rs` `upsert_secret` :54 / `activate` :124 / `disable` :141（pool 级）；`auth_session.rs` `revoke_and_blacklist` :499（CTE 吊销，pool 级）；`session.revoked` 三处全部 best-effort 事后 append（design 勘误①）；`admin_sessions.rs` 零 audit |
| `login_failures`（L1 底座） | ✅ `migrations/0140_login_failures.sql`：account/ip/user_agent/created_at + `(account, created_at DESC)`、`(ip, created_at DESC)` 双索引，**无 created_at 单列索引** → L1 需要新索引迁移（**编号定稿见 design §7 D8：0243**）；`LoginFailureRepo::record` 实为 `login_failure.rs:63` |
| harness `run_migrated_integration` | ✅ `scripts/test-integration.sh:169-202`：硬编码 `cargo test -p aero-storage --lib --locked "$test_name" -- --ignored --test-threads=1` + 空过滤器守卫（`test result: ok. [1-9]* passed` 判 FAIL）；B5-1 条目 :306-318（`audit_governance::` + `moderation_finalize_outbox_parity`，0239 文件门控，已 un-gated）；T-11 drill 段 :347-397 |
| `aero-audit-t11-drill` | ✅ `crates/aero-audit-connector/src/bin/aero-audit-t11-drill.rs`：T-11 不变量 = `COUNT(status=0)==N`、`COUNT(status IN (1,2,3))==0`、`SUM(attempts)==N`（round 1）/`2N`（round 2）、`transport_error rows==N`；`to_regclass` 缺席 exit 2（0239 已落 → 无 SKIP 分支）；TRUNCATE-at-start 自隔离 |
| `scripts/b5-pin.sh` | ✅ :37-38 钉 `audit_governance::` + `moderation_finalize_outbox_parity`；37 槽守卫（恰 37/无重复/malformed/非空转/verdict 行） |
| 迁移计数 | ✅ `ls migrations/*.sql | wc -l` = 241，尾号 0241 → next 号位 0242。**冲突风险（已定稿）**：auth 切片计划 `0242_login_failures_created_at_idx.sql`，sibling aero-ai 切片（`2026-08-08-aero-ai-b5-1-l1-aggregation-in-tx-audit.design.md` §2.4）计划 `0242_audit_governance_l1_aggregate.sql`——**design §7 D8 定稿：本切片无条件取 0243（索引）+ 0244（DLQ），sibling 保留 0242**，不再条件式错开（§7 风险 ①） |
| uuid v5 可用 | ✅ root `Cargo.toml:110` `uuid = { version = "=1.18.1", features = ["v4","v5","v7","serde"] }`——L1 合成 event_id 零依赖改动 |

### 1.2 对 direction 表述的勘误（evidence-backed）

- **「auth 行 priority 100」不确**：direction acceptance 写 "relay `claim_due` returns class='admin' auth rows (priority 100)"。实测钉死（design §2.7 表 + req R2 + `governance.rs:33`）：**auth 显式写行 = class 'admin'、priority 10（`GOVERNANCE_PRIORITY_BACKLOG`）**；priority 100 是**既有 moderation 触发器戳**（`GOVERNANCE_PRIORITY_MODERATION`）。claim `ORDER BY priority DESC` 下 moderation 先于 auth，符合「审核车道不被打断」。验收 AC2 按此钉死。
- **「relay 会 claim 空表」的现状面**：0239 触发器已为 `message.moderated` 产行（relay drill/T-11 已实跑全绿），非绝对空表；但 **auth 域（register/login/refresh/PAT/2FA/session）零 outbox 行、L1 聚合零行**——B5-1 parity 契约（mapped subset 的 `COUNT(outbox)==COUNT(audit)`）对 auth 域仍 unmet。本 direction 闭合的正是这个缺口。
- **scope 边界**：direction 标题含 "message.*/room.*"，但 **proposed 清单 = append_in_tx / aggregate_login_failure_buckets / aero-auth in-tx pairs / L1 aggregation**——`message.create/edit` 生产路径 in-tx 写属 sibling 切片（`2026-08-08-aero-ai-b5-1-l1-aggregation-in-tx-audit.design.md`，Status: proposed），**不在本 direction 范围**（§3 红线）。本 direction 交付的 `append_in_tx` 原语是两条切片的共享 H3 落点。

## 2. Verified current state

```
入队现状（两条路）：
a) 触发器路（已落地）  audit_events AFTER INSERT → aero_enqueue_governance_audit()
                       └─ 仅 NEW.action='message.moderated' → outbox（class admin/priority 100）；其余 pass-through
b) 显式写路（本 direction）  AuditGovernanceOutboxRepo —— 尚不存在（H3 手记：落 audit_governance.rs 同模块）

producer 缺口（全部验证）：
  register → 无 audit；事务在 RegistrationRepo::create（单事务，可骑）
  login    → 成功仅新 IP 时 best-effort auth.login.new_ip；失败入 login_failures（0140）——L1 底座已就绪
  refresh  → 无 audit
  PAT/2FA  → pool 级单语句，无 audit、无事务变体
  session  → 用户侧/admin 侧 best-effort 事后 append，非 in-tx
  workspace → 账户级安全事件落 DEFAULT_WORKSPACE_ID（nil，0006:61 种子行）

consumer 侧（已落地，零改动）：
  OutboxRepo trait + PgOutboxRepo（claim_due priority DESC lanes、fenced settle/requeue/mark_dead、
  STATUS 0..3、单时钟域）；relay 在 main.rs presence-gated spawn；drills（relay/t11/priority）实跑全绿
```

**Gaps this direction closes**（all verified）：① `AuditGovernanceOutboxRepo`（H3，含 `append_in_tx`/`governance_envelope`/`append_pair_in_tx_fail_open`/`record_pair_standalone`/`aggregate_login_failure_buckets`）不存在；② auth 域 6 类操作无 in-tx 审计对（register 骑既有事务、login/refresh 独立事务对、PAT/TOTP/session 事务化变体）；③ 高容量 login 失败无 L1 聚合（`aggregate_login_failure_buckets` + 0243 索引 + boot timer）；④ parity 契约对 auth 域 unmet（AC2 oracle 缺失）。

## 3. Scope

**In scope（direction proposed 四项，effort 8）**：
- **aero-storage `audit_governance.rs`（H3 落点）**：`AuditGovernanceOutboxRepo` 新增 `append_in_tx`（镜像 `AuditRepo::append_in_tx` 形态）、`governance_envelope`（0239 触发器信封的 Rust 镜像，source_system 参数化）、`append_pair_in_tx_fail_open`（SAVEPOINT 语义：审计+outbox 原子对、审计失败整对消失 fail-open）、`record_pair_standalone`（login/refresh 独立事务对）、`aggregate_login_failure_buckets`（L1：uuid-v5 确定性 event_id、class 'message'、`ON CONFLICT DO NOTHING`、只聚合已关闭桶）。db_tests 与既有 6 测试同模块共存，**既有名字/字面量逐字不动**。
- **aero-auth in-tx pairs**：`NewRegistration.auth_audit`（`create()` 既有事务内嵌对，3 构造点更新）；`refresh` 成功后 `record_pair_standalone`；login 审计点定在 **handler 侧 `finalize_login(true)` 之后**（2FA 终局可见，D5）；PAT/TOTP/session 路由事务化（`_in_tx` 仓储变体 + 审计对同事务）。
- **L1 聚合**：`aggregate_login_failure_buckets(pool, window)` 按 `login_failures.created_at` 分桶（`date_trunc + epoch % window`），每已关闭桶恰 1 行 outbox（`class 'message'`、`priority 10`、payload.count=N、`aggregated=true` 豁免键）；**无 audit_events 行**（‡ 类 1:1 豁免，避免 0236 v1 触发器副作用）；boot timer（env `AERO__SERVER__LOGIN_FAILURE_L1_AGGREGATE_SECS`，默认 30、0 禁，`MissedTickBehavior::Skip`，持共享 cancel token）。
- **索引迁移 0243**：`login_failures_created_at_idx`（`CREATE INDEX IF NOT EXISTS`，幂等；**编号定稿见 design §7 D8**）。**顺序：`cargo build` → `aero-cli migrate`**（§4.2 编译期嵌入）。
- **harness**：test-integration.sh B5-1 段追加一条命名条目 `auth_outbox_parity`（镜像 `moderation_finalize_outbox_parity` 条目形态）；`audit_governance::` 既有条目自动覆盖其余新测试（同模块）。b5-pin.sh 37 槽逐字不动（无新槽名）。

**Out of scope（不扩）**：
- **`message.create/edit` / `room.*` 生产路径 in-tx 写**——sibling 切片（aero-ai `2026-08-08-aero-ai-b5-1-l1-aggregation-in-tx-audit.design.md`）交付，本 direction 只落共享 `append_in_tx` 原语。
- **不改 0239/0240/0241 DDL 与触发器**、不改 `governance.rs` 映射权威（auth 走显式写路，不扩触发器 token 集）、不改 `aero-audit-connector/` 任何代码（claim lanes 已就绪）、不动 v1 `snaplink_delivery_outbox` 路径。
- **不做 `record_login_event`（`auth.login.new_ip`）/ `auth.login.recovery_code` 的 in-tx 化**——其 best-effort 语义既定（design §2.4）。
- 不新增除 0243/0244（索引 + DLQ，design 定稿）外的任何迁移；auth 事件 payload 信封不新增仓外契约（沿用 0239 信封形状）。

## 4. Requirements

**R1（in-tx 原子对，register）**：`RegistrationRepo::create` 既有事务内（domain 行之后、commit 之前）经 `append_pair_in_tx_fail_open` 写审计对：恰 1 行 `audit_events`（action=`auth.register`，actor=新 participant，workspace=注册 workspace）+ 恰 1 行 `audit_governance_outbox`（`event_id = audit_events.id`、status 0、class 'admin'、priority 10、payload 信封含 `idempotency_key = event_id::text`）。任一失败 → 三组行整体回滚（0 行逃逸）。`NewRegistration` 增 `auth_audit: Option<NewRegistrationAudit>`，3 构造点更新（db_tests ×2 传 `None`、`service.rs:233 register_enrolled` 传 `Some`）。

**R2（SAVEPOINT fail-open 边界，唯一实现点）**：`append_pair_in_tx_fail_open(tx, workspace, actor, action, target, detail, outbound_action) -> Result<Option<AuditId>, sqlx::Error>` 用 RAW SQL `SAVEPOINT aero_audit_pair` / `ROLLBACK TO SAVEPOINT aero_audit_pair`（成功路径 RELEASE）包裹 audit+outbox 两行。审计写入失败（CHECK 违例如非 object detail）→ 两行俱无，返回 `Ok(None)`（R7 语义：「整对没写」，域照常提交）；仅连接级错误向上传播。**禁止半对**。

**R3（独立事务对，login/refresh）**：`record_pair_standalone`（begin → `append_in_tx` audit → `append_in_tx` outbox → commit）失败内部 warn + `Ok(None)`（fail-open，不翻 auth 结果）。login 写点：handler 侧 `finalize_login(true)` 之后（:197 区）、`record_login_event` 旁（:208 区），workspace=`DEFAULT_WORKSPACE_ID`、action=`auth.login`、outbound=`admin.auth.login`、target=participant id。refresh 写点：`AuthService::refresh` 成功后，Err 仅 warn。

**R4（PAT/TOTP/session 事务化）**：路由编排 begin → 域语句 `_in_tx` 变体 → 审计对 → commit；审计对失败 = SAVEPOINT 跳过，域照常提交。token 表（design §2.7 定稿）：`auth.pat.issue`/`admin.auth.pat.issue`、`auth.pat.revoke`/`admin.auth.pat.revoke`、`auth.totp.enroll`/`admin.auth.totp.enroll`、`session.revoked`（沿用既有 token，兼容消费方）/`admin.auth.session.revoke`、admin 侧新增 `session.revoked.admin`。替换后删除 sessions.rs/session.rs 的 pool 级事后 `audit_session_revoked`（action 字符串不变 → 轨迹连续）。

**R5（L1 聚合）**：`aggregate_login_failure_buckets(&self, window_secs: i64) -> Result<usize, sqlx::Error>` 只聚合**已关闭桶**（桶尾 ≤ now − window）；每桶恰 1 行 outbox：`event_id = Uuid::new_v5(NAMESPACE_URL, "aero.im.audit.l1:{workspace}:{action}:{bucket_start_epoch}")`（确定性 → 重跑/并发 `ON CONFLICT DO NOTHING` 幂等）、status 0、class 'message'、priority 10、payload = `governance_envelope(…, action "auth.login.failure", outbound "admin.auth.login.failure", outcome "failure", detail {count, window_start_epoch, window_secs, aggregated:true})`。**无 audit_events 行**（‡ 豁免；`audit_governance_outbox.event_id` 无 FK，合法）。不触 `admin_class_rows_never_aggregated`（聚合行自身是 message 域）。

**R6（不新增消费者改动）**：`aero-audit-connector/` 零改动——`claim_due` 已按 `priority DESC` 返回带 lanes 的 `Claim`；auth 行（admin/10）与 L1 行（message/10）进既有状态机（退避 cap 300s、MAX_ATTEMPTS=5 → dead、403 首次 mark_dead）。

**R7（既有 pin 逐字保持）**：`moderation_finalize_outbox_parity`、`ddl_contract_defaults_and_checks`、`admin_class_rows_never_aggregated`、`governance_lane_for`、0239/0240/0241、`audit_governance.rs` 模块 doc cross-slice pin 注释——名字/字面量不动。`auth.login.new_ip`/`auth.login.recovery_code`/`LoginFailureRepo::record`/`record_login_event` 行为不变。

**R8（迁移纪律，编号定稿 design §7 D8）**：索引迁移 = `migrations/0243_login_failures_created_at_idx.sql` 仅 1 条 `CREATE INDEX IF NOT EXISTS login_failures_created_at_idx ON login_failures (created_at)`；DLQ 表 = `migrations/0244_audit_governance_failed_pairs.sql`（design §5.3 DDL）。**顺序：先 `cargo build` 再 `aero-cli migrate`**（§4.2 编译期嵌入——否则新迁移静默 no-op）。迁移计数勿在文档硬编码（`ls migrations/*.sql | wc -l` 即得）。

## 5. Acceptance checks（direction 原样保留，逐条 testable）

> direction acceptance 四条全部保留；AC1/AC3 为 PG 门控 db_tests（`#[ignore]` + `DATABASE_URL`，落 `audit_governance.rs` 的 `db_tests` 模块），AC2 为 claim 车道断言（connector 侧或 storage 侧），AC4 为 harness drill 回归。

- **AC1（in-tx 原子性三测试，direction 命名原样）**：db_tests —— `registration_commit_half_writes_audit_and_outbox`：`RegistrationRepo::create`（带 `auth_audit: Some(…)`）提交后 `audit_events` 恰 1 行（action=`auth.register`）+ `audit_governance_outbox` 恰 1 行（status=0、class='admin'、priority=10、`event_id`=audit id、payload.action=`admin.auth.register`、`idempotency_key`=event_id::text）。`registration_rollback_half_leaves_zero_rows`：同事务制造失败（重复 email → create Err）→ 两表均 0 行逃逸（same-transaction proof，镜像 `moderation_finalize_outbox_parity` 的 A1-RB 半部）。`registration_audit_failure_fail_open_commits_domain`：`auth_audit.detail` 传非 object（触发 payload CHECK）→ **create 成功**、两表 0 行（SAVEPOINT 证明 R7）。
- **AC3（L1 聚合 N→1，direction 命名原样）**：db_test —— `login_failure_l1_aggregation_n_to_one`：种子 N=3 行同桶 + 2 行异桶（window=60s）→ `aggregate_login_failure_buckets(60)` → 恰 2 行（每桶 1 行）、payload.count=3/2、class='message'、status=0、priority=10、event_id 确定性（uuid-v5）；**重跑 → 仍 2 行、count 不变**（幂等）；`payload->>'aggregated' = true` 存在（parity 豁免键）。
- **AC2（claim 车道 + P2 parity）**：claim 车道 —— auth 行（class='admin'、priority=10）与 L1 聚合行（class='message'、priority=10）入 `claim_due` 后按 lanes 原样返回（`Claim.priority`/`Claim.class` 字段断言），moderation 行（priority=100）仍最先 claim（DESC 序，既有 pin `mixed_priority_claim_orders_moderation_first_then_fifo`（connector pg.rs:536）+ drill 槽 `moderation-priority-drill` 覆盖——**勘误：原引用的 `moderation_lane_preempts_backlog_under_desc_claim` 全仓零命中，系幽灵名**）。P2 parity —— 新命名 harness 条目 `auth_outbox_parity_1to1`：共享豁免谓词定稿见 design §6 AC2/§7 D13（outbox 侧 `payload->>'aggregated'='true'` 两切片共用；audit 侧 allowlist 之外合法 audit-only）；信封逐字段钉死（schema_id/schema_version/aggregate_type/aggregate_id/actor/targets/data_classification/retention_class/idempotency_key/`source_system="aero-auth"`）。经 `scripts/test-integration.sh` `run_migrated_integration` 跑（B5-1 段追加条目；空过滤器守卫生效）。
- **AC4（T-11 drill 回归）**：`aero-audit-t11-drill` 全绿 —— `COUNT(status=0)==N`（pending 永不静默成功）、`COUNT(status IN (1,2,3))==0`（永不虚假 dead）、`SUM(attempts)==N`→`2N`（round 1/2，retry-forever 证据）、transport-error rows==N（关闭 token 端点确被尝试）。同 throwaway 库上 `aero-audit-relay-drill` 与 `aero-audit-priority-drill` 亦全绿（B5-1 段 :306-318/:325-341/:441-461，0239 已落 → 无 SKIP 分支）。

## 6. Test placement

- **AC1/AC3 全部 db_tests**：`crates/aero-storage/src/audit_governance.rs` 的 `db_tests` 模块（H3 落点；harness 过滤器 `audit_governance::` 既有条目自动覆盖，仅补一条命名条目 `auth_outbox_parity`）。PG 门控：`#[ignore = "requires live Postgres"]` + `DATABASE_URL`。仓储直测（不经 aero-auth 包）→ `run_migrated_integration` 硬编码 `-p aero-storage` 零参数化。
- **AC2 信封断言**：`auth_outbox_parity_1to1`（命名 parity 测试，镜像既有 `moderation_finalize_outbox_parity` 条目形态）；`test-integration.sh` B5-1 段追加条目（:318 后）。
- **AC4**：既有 drill 条目，零新增（t11/relay/priority 各自 `b5_check` 槽）。
- **接线测试**（非 PG 门控）：`aero-auth` 单测保持基线（81 passed）；connector `state_machine.rs`/`claim_validation.rs` 套件不动、全绿。

## 7. Risks / 决策点

- **① 迁移编号冲突（本 spec 新增发现，定稿见 design §7 D8）**：auth 切片计划 `0242_login_failures_created_at_idx.sql`，sibling aero-ai 切片计划 `0242_audit_governance_l1_aggregate.sql`——两切片同时认领 0242。**定稿：本切片无条件取 0243（索引）+ 0244（DLQ），sibling 保留 0242**（两文件同一未提交工作树 → 固定分配消除条件式竞态；落地前 `ls migrations/*.sql | grep -cE '^(0242|0243|0244)_'` ≤1）。
- **② 前置批次未提交**：0239/0240/0241、`audit_governance.rs`、`governance.rs`、`model/audit.rs`、`aero-audit-connector/`、test-integration.sh 改动全在 untracked 工作树——落地前须先提交（design §5.1 硬前置），否则 `git reset --hard` 校准丢 B5-1 地基。
- **③ `NewRegistration` 加字段 = 3 构造点**（编译强制，低风险）；**④ 0243/0244 迁移必须 build→migrate 顺序**（§4.2）；**⑤ login 审计点若被重构移入 `AuthService::login` 会误记 2FA 失败**——AC1 login 测试 + D5 注释钉死；**⑥ auth 行 priority=10 与 moderation 100 同表**——claim 排序由 0240 索引 + `priority DESC` 保证（auth 排后，安全事件不抢占审核车道）。
- **⑦ fail-open 与原子性的边界**：R7 允许「整对没写」，禁止「半对」——SAVEPOINT 是 R1/R7 唯一自洽实现（D4）。login/refresh 的「事务」= 独立事务对自身回滚（D3）。
- **⑧ L1 聚合行无 audit_events 行**：‡ 类 1:1 豁免 + 避免 0236 v1 触发器副作用 + 避免 default-workspace 审计视图污染（D7）；`login_failures` 保留逐尝试行作 forensic 底座，聚合是 near-real-time 信号非账本（F7）。

## 8. Sequencing

1. **前置**：提交上一批次 untracked 物（0239/0240/0241、`audit_governance.rs`、`governance.rs`、`model/audit.rs`、`aero-audit-connector/`、test-integration.sh、b5-pin.sh 改动）。
2. **迁移 0243 + 0244**（编号定稿 design §7 D8）：`cargo build` → `aero-cli migrate`（§4.2 顺序；落地前 `grep -cE '^(0242|0243|0244)_'` ≤1，风险 ①）。
3. **aero-storage**：`AuditGovernanceOutboxRepo`（H3）+ `governance_envelope` + `append_pair_in_tx_fail_open` + `record_pair_standalone` + `aggregate_login_failure_buckets` + `_in_tx` 仓储变体（pat/totp/auth_session）+ `NewRegistration.auth_audit` + db_tests（AC1-AC3 全部落此，parity 名/字面量不动）。
4. **aero-auth**：`register_enrolled` 审计 spec + `refresh` 写点（fail-open 包装）。
5. **aero-server**：login handler 审计点（`finalize_login(true)` 后）；pat/twofa/sessions/session 路由 tx 编排（删 pool 级事后审计）；admin revoke 存储侧嵌对；boot timer（`AERO__SERVER__LOGIN_FAILURE_L1_AGGREGATE_SECS`）；test-integration.sh 追加 `auth_outbox_parity` 条目。
6. **门禁**：`cargo check --workspace` / `cargo test --workspace --lib`（基线 81 保持；PG 门控 `-- --ignored` + 一次性库）/ clippy 无新增警告 / `scripts/{truth-check,file-size-check,web-check}.sh`；`test-integration.sh` B5 段全 PASS（AC4 un-gated 无 SKIP）；`scripts/test-b5-pin-guard.sh` 绿（37 槽逐字不动）。

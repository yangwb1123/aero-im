# Design — B5-1 producer side：AuditGovernanceOutboxRepo（H3）+ auth 域 in-tx 审计对 + L1 聚合（module: crates/aero-audit-connector）

- **Direction**: "Land the B5-1 producer side: AuditGovernanceOutboxRepo (H3) with in-tx append + L1 aggregation so the relay has message.*/room.*/admin.* rows to claim"（value 10 / risk_reduction 8 / effort 8 / confidence 9）
- **Requirements**: `docs/requirements/2026-08-08-aero-audit-connector-b5-1-producer-side.req.md`（R1–R8, AC1–AC4）
- **Sibling design**（同工作，模块根不同）: `docs/design/2026-08-08-aero-auth-b5-1-in-tx-audit-outbox.design.md`（Status: Design）——本设计是 audit-connector 模块根的落地设计，与 sibling 同锚、共用 §2.7 token 表与 §2.6 L1 定稿；本设计负责把 H3 落点（aero-storage）的 API 钉死并独立完成验收映射。
- **Status**: Design
- **Verification date**: 2026-08-08。行号为复核锚点，会漂移——**文件/符号**为准（AGENTS.md §0）

## 0. Evidence adjudication（untrusted claims → verified anchors，本设计现场复核）

| Evidence claim | Verdict | Verified anchor（实际） |
|---|---|---|
| 0239 触发器 token-keyed（仅 `message.moderated` 入 outbox），未知 token fail-open `RETURN NEW`，Gate 2 binding RAISE fail-closed，`ON CONFLICT (event_id) DO NOTHING`，class 'admin'/priority 100/status 0 + 完整 v2 信封 | ✅ 精确 | `migrations/0239_audit_governance_outbox.sql`（140 行）：`aero_enqueue_governance_audit()` Gate 1 runtime → token 门（`NEW.action <> 'message.moderated' ⇒ RETURN NEW`）→ Gate 2 binding RAISE → INSERT … `ON CONFLICT (event_id) DO NOTHING`，`idempotency_key = NEW.id::text`；表 CHECK（status 0..3 / class admin\|message\|room / priority > 0 / delivery_mode push）+ claim-state CHECK + due 部分索引 |
| `audit_governance.rs:1-30` H3 handoff，仅 fixtures；`AuditGovernanceOutboxRepo` 全仓不存在 | ✅ 精确 | 模块 doc :22-24 原文 "The B5-1 storage direction's `AuditGovernanceOutboxRepo` lands in a sibling slice (handoff H3 of the 0239 design doc); … **add the repo to this same module when it lands**"。`rg AuditGovernanceOutboxRepo crates/` 仅 doc 命中。既有 db_tests 6 个：`moderation_finalize_outbox_parity` :247、`ddl_contract_defaults_and_checks` :424、`moderation_finalize_runtime_disabled_commits_1_plus_0` :583、`non_moderation_action_passes_through_unmapped` :628、`duplicate_event_id_is_deduped_by_on_conflict` :693、`governance_reconcile_backfills_disabled_window` :774 |
| `model/audit.rs:165` `GOVERNANCE_CLASS_MESSAGE` 'L1-aggregatable' | ✅ 微漂 | 实为 :166，doc 原文 "High-volume message backlog class (L1-aggregatable)"；`OutboxStatus` 0..3 serde 整数 wire 契约、`AuditClass`、`MODERATION_OUTBOUND_ACTION` :150、`LOCAL_ACTION_MODERATED` :156、`GOVERNANCE_CLASS_{ADMIN,ROOM}` :163/:168 |
| `governance.rs:86-96, 195-211` [PROPOSED] L1 bypass；`admin_class_rows_never_aggregated` | ✅ 精确 | `governance_lane_for` :86-99 唯一映射（`message.moderated` → admin/100/`admin.content.flag`/0；未知 → `None`）；`is_admin_class` :102-106（doc :84-85 "R5 classification for the [PROPOSED] L1 aggregation bypass"）；单测 `admin_class_rows_never_aggregated` :195-201；`GOVERNANCE_PRIORITY_MODERATION=100` :31 / `GOVERNANCE_PRIORITY_BACKLOG=10` :33 |
| aero-auth 设计 doc：Status Design，AC1/AC3 命名未落地 | ✅ 精确 | `docs/design/2026-08-08-aero-auth-b5-1-in-tx-audit-outbox.design.md` Status: Design；§6 验收映射 AC1 三测试名 + AC3 `login_failure_l1_aggregation_n_to_one` + AC2 `auth_outbox_parity_1to1`——`rg` 全仓零命中（未落地） |
| connector `OutboxRepo` 就绪（priority/class lanes），零改动 | ✅ 精确 | `crates/aero-audit-connector/src/outbox.rs`：trait 五方法 `reconcile`/`claim_due`/`settle`/`requeue`/`mark_dead`；`Claim { event_id, claim_token, lease_expires_at, attempts, payload, priority: i16, class: String }`（:90-104）；`pg.rs` `claim_due` `ORDER BY candidate.priority DESC, candidate.available_at, candidate.created_at, candidate.event_id`（:117）+ `FOR UPDATE SKIP LOCKED`（:119），单时钟域 `clock_timestamp()` |
| **迁移编号冲突**：auth 切片 `0242_login_failures_created_at_idx.sql` vs sibling aero-ai 切片 `0242_audit_governance_l1_aggregate.sql` | ✅ **新发现成立** | `ls migrations/*.sql | wc -l` = 241，尾号 0241，**0242 尚不存在**；sibling `docs/design/2026-08-08-aero-ai-b5-1-l1-aggregation-in-tx-audit.design.md` §2.4（:147-149）明确计划 `migrations/0242_audit_governance_l1_aggregate.sql`（Status: proposed）。两切片同时认领 0242 |
| 前置批次（0239/0240/0241、`governance.rs`、`model/audit.rs`、`aero-audit-connector/`、test-integration.sh、b5-pin.sh）未提交 | ✅ 成立 | `git status --short` 94 项：0239/0240/0241、`crates/aero-ai/src/governance.rs`、`crates/aero-common/src/model/audit.rs`、`crates/aero-audit-connector/`、`scripts/b5-pin.sh`、`scripts/test-b5-pin-guard.sh` 全部 **untracked** |
| aero-auth 零 audit/outbox 命中 | ✅ 精确 | `rg -ic "audit\|outbox" crates/aero-auth/src/` = 0 |
| `RegistrationRepo::create` 单事务；`NewRegistration` 3 构造点 | ✅ 精确 | `registration.rs`：`NewRegistration` :13、`create` :37（begin :38 → participants + credentials + workspace_members + 默认频道 + auth_sessions → commit :109）；构造点 `registration.rs:156/:257`（db_tests）+ `aero-auth/service.rs:233`（register_enrolled） |
| login 2FA 门在 handler 侧；`record_login_event` 账户级审计落 `DEFAULT_WORKSPACE_ID` | ✅ 精确 | `routes/handlers/auth.rs`：`finalize_login(&email, false)` :182 / `finalize_login(&email, true)` :197；`record_login_event` :35（`auth.login.new_ip`），调用点 :208-213 `Some(DEFAULT_WORKSPACE_ID)`；nil 种子行 `0006_workspaces.sql:61` |
| `login_failures` 无 created_at 单列索引 → L1 需 0242 | ✅ 精确 | `migrations/0140_login_failures.sql`：account/ip/user_agent/created_at + `(account, created_at DESC)`、`(ip, created_at DESC)` 双索引，无单列；`LoginFailureRepo::record` 实为 `login_failure.rs:70` |
| `AuditRepo::append_in_tx` :127（in-tx append 先例） | ✅ 精确 | `audit.rs`：`append` :113、`append_in_tx(tx, …) -> Result<AuditId>` :127、generic executor `append_on` :140——auth 对骑它（返回 ULID `AuditId` 做 outbox event_id，1:1 前提） |
| PAT/TOTP/session pool 级无事务；session.revoked 三处 best-effort；admin 侧零审计 | ✅ 精确 | `pat.rs` `create` :104 / `revoke` :190；`totp.rs` `upsert_secret` :54 / `activate` :124 / `disable` :141；`auth_session.rs` `revoke_and_blacklist` :499；`session.rs:212` + `sessions.rs:108` 事后 pool 级 `audit_session_revoked`；`admin_sessions.rs` 零 audit |
| harness `run_migrated_integration` 硬编码 `-p aero-storage` + 空过滤器守卫；B5-1 条目 :306-318 | ✅ 精确 | `scripts/test-integration.sh:169-202`（`cargo test -p aero-storage --lib --locked "$test_name" -- --ignored --test-threads=1` + `grep -Eq 'test result: ok\. [1-9][0-9]* passed'` 守卫）；B5-1 段 :306-318（`audit_governance::` + `moderation_finalize_outbox_parity`，0239 文件门控） |
| b5-pin.sh 37 槽 | ✅ 精确 | `scripts/b5-pin.sh`：`B5_CONTRACT_TEST_LIST` 37 项（15 executed + 22 [PROPOSED]），`audit_governance::` :37、`moderation_finalize_outbox_parity` :38；`assert_b5_contract_pin` 只对列表内 executed 槽 grep verdict——**新增 verdict 行（如 `B5-CHECK auth_outbox_parity: PASS`）不触发守卫失败**（守卫不检查多余行），槽列表逐字不动即可 |
| 0236 v1 触发器对**所有** audit 行产 v1 行（enforcement 门） | ✅ 补充确认 | `0236_snaplink_governance_reconciliation.sql`：`aero_enqueue_snaplink_audit()` :68、AFTER INSERT 触发器 :129-133，唯一门 = `snaplink_commercial_runtime.enabled`；`aero_snaplink_audit_payload(detail)` :41（object 透传 + 64KB omit/sha256 守卫）——**auth 显式对在 enforcement 开时也会自然产 v1 行**（与 moderation 行同等待遇，v1/v2 共存，非本切片改动） |
| `audit_events.workspace_id NOT NULL REFERENCES workspaces` | ✅ 补充确认 | `0007_audit.sql`（22 行）：workspace_id NOT NULL；nil UUID 种子行 `0006_workspaces.sql:61`——auth 账户级事件落 `DEFAULT_WORKSPACE_ID` 合法 |
| boot 定时器惯例（env 门控、0 禁、Skip、共享 cancel token） | ✅ 补充确认 | `bin/boot/retention.rs`：`AERO__SERVER__LOGIN_FAILURE_RETENTION_DAYS` 等 env 读取 + 0-disables 注释；`background.rs` 多处 `MissedTickBehavior::Skip`（:75-76/:110-113/:152-155/:264-265/:451-452/:503-506/:544-545） |
| uuid v5 已启用；migrate 编译期嵌入；`AppState.pg` | ✅ 补充确认 | root `Cargo.toml:110` `uuid = { version = "=1.18.1", features = ["v4","v5","v7","serde"] }`；`aero-storage/db.rs:58` `sqlx::migrate!("../../migrations").run(pool)`；`aero-server/src/state.rs:383` `pub pg: PgPool` |
| **F1 机制复核**：enforcement 开关/绑定 DDL + RAISE 点；0236 对每条 audit INSERT 生效；0239 token 门先放行 auth | ✅ 本修订复核 | `0235_snaplink_commercial_control_plane.sql`：`snaplink_commercial_runtime`（singleton/enabled）:8-14；`aero_snaplink_binding_for_workspace` :193-215（`NOT FOUND → RAISE EXCEPTION 'commercial binding is unavailable'` P0001）；`0236_snaplink_governance_reconciliation.sql` `audit_events_snaplink_delivery` :129-133（唯一门 = runtime.enabled，AFTER INSERT FOR EACH ROW）——auth 对只能被 0236 raise（0239 token 门对 auth action 先 `RETURN NEW`） |
| **F1「既有同款洞」**：`record_login_event` 已写 nil-workspace audit 行 | ✅ 本修订复核 | `routes/handlers/auth.rs` `record_login_event` :35：`auth.login.new_ip` 经 `AuditRepo::append` 写 `audit_events`（workspace = 调用点 `Some(DEFAULT_WORKSPACE_ID)`，:208-213），`let _ =` best-effort——enforcement on + 无 nil 绑定**已静默吞 P0001**（本切片把同款洞升级为部署错误：D13 库层不变量使其经受支持路径不可达 + D10 强制 ERROR/计数） |
| **D10 预检落点**：boot fail-loud 惯例 + `from_env` 同步点 | ✅ 本修订复核 | `crates/aero-server/src/bin/main.rs`：`SnaplinkCommercialRuntime::from_env(persistence.pg.clone()).await.context(...)?`（fail-loud `?` 惯例）；`snaplink_commercial/runtime.rs` `from_env` 先同步 desired state（`configure_enabled`/`require_disabled`）——**boot fail-loud 已存在于库层**：`configure_enabled` 内 `require_complete_workspace_coverage`（`snaplink_commercial.rs:191/453`：所有 `workspaces` 行含 nil 必须有 enabled binding）与开关翻转同事务，缺覆盖即 Err → `from_env` 传播 → 进程拒绝启动。本切片**零新增预检代码**，仅文档化（F14）+ 测试钉死（F1 回归组） |
| **§2.8 可观测落点**：metrics 注册/命名惯例；aero-storage 现状零 metric | ✅ 本修订复核 | `aero-common/src/metrics.rs`：`names` 常量 + `register_known_metrics` :791 + labeled helpers；`metrics_tasks.rs` 既有 30s AI DLQ 深度 gauge sampler（:172-190 模式）；`rg metrics crates/aero-storage/src/` 仅 room.rs:391 注释——pair 写路径将成 storage 首个 metric（deliberate 例外，§2.8） |

**证据结论**：direction 六条引用全部精确（E3 仅行号微漂）；两条补充事实（0242 迁移冲突、前置批次 untracked）均为真。本设计在 req 基础上把 API 签名、SAVEPOINT 错误分类、L1 桶公式与 AC2 claim-lane 测试落点钉死。

## 1. Design overview

```
                    ┌────────────── aero-storage（全部 in-tx 机制落家，harness 零参数化）──────────────┐
                    │  audit_governance.rs（H3 落点，与既有 6 fixtures db_tests 同模块共存）            │
                    │    AuditGovernanceOutboxRepo::new(pool)                                        │
                    │    append_in_tx(tx, event_id, class, priority, payload)                        │
                    │    governance_envelope(...)  ← 0239 触发器信封的 Rust 镜像（source_system 参数化）│
                    │    append_pair_in_tx_fail_open(tx, …) ← SAVEPOINT 语义（R1 原子对 + R7 fail-open）│
                    │    record_pair_standalone(pool, …)    ← login/refresh 独立事务对（Option<AuditId>）  │
                    │    FailedPairRepo(pool) ← 同 tx DLQ（0244，F1/F3 补偿：丢对可重放）             │
                    │    aggregate_login_failure_buckets(pool, window) ← L1（uuid-v5 确定性 event_id） │
                    └──────────────────────────────────────────────────────────────────────────────────┘
  register:  AuthService::register_enrolled ── NewRegistration.auth_audit ──▶ RegistrationRepo::create 既有事务
                                                              （domain 行 + 审计对，SAVEPOINT fail-open）── commit
  login:     handler 2FA 门全过 → finalize_login(true) ──▶ record_pair_standalone（独立事务对）── commit
  refresh:   AuthService::refresh 成功 ──▶ record_pair_standalone ── commit
  PAT:       pat.rs 路由: begin → PatRepo::{create_in_tx,revoke_in_tx} → 审计对 → commit
  TOTP:      twofa.rs 路由: begin → TotpRepo::{upsert_secret_in_tx,activate_in_tx} → 审计对 → commit
  session:   sessions.rs / session.rs(logout) 路由: begin → revoke_and_blacklist_in_tx → 审计对 → commit
             admin_sessions.rs: auth_session.rs 既有事务内嵌审计对（storage 侧，路由零改动）
  L1:        boot timer（env 门控）→ aggregate_login_failure_buckets → login_failures 已关闭桶 → 聚合 outbox 行
             （class 'message'、priority 10、payload.count=N、aggregated=true、无 audit_events 行 = ‡ 豁免）
  fail-open: Database 类丢对 → ROLLBACK TO SAVEPOINT → 同 tx 入 audit_governance_failed_pairs（DLQ）
             → 域照常提交（R7）；重放 = FailedPairRepo::replay（新 AuditId 重建对，D9）
  relay:     aero-audit-connector 零改动——claim_due 按 priority DESC lanes 原样 claim；sink Idempotency-Key = event_id
```

**三条硬线**（与 sibling design 一致）：① 所有 in-tx 机制在 aero-storage（H3 手记落点 + `run_migrated_integration` 硬编码 `-p aero-storage` 的 harness 约束），aero-auth / aero-server 只做调用与编排；② 审计对 = 恰 1 行 `audit_events` + 恰 1 行 `audit_governance_outbox`（`event_id = audit_events.id` 1:1，P2 parity），**审计写失败经 SAVEPOINT 整对消失（R7「整对没写」），绝不半对**——被丢对的**原始输入同 tx 入 DLQ `audit_governance_failed_pairs`（D9，可重放，非永久丢失）**；③ 唯一新表 = DLQ `audit_governance_failed_pairs`（0244），不改 0239/0240/0241、不改 `governance.rs` 映射权威、不改 connector。

**与 sibling design 的分工**：sibling（aero-auth 模块根）负责 auth 域编排细节（§2.3-§2.5、§2.7 token 表）；本设计（audit-connector 模块根）钉死共享落点 API（§2.1-§2.2）与验收 oracles（§6），并把 §0 的两条新发现（0242 冲突、前置批次）落成确定性处置（§5、§7）。

## 2. API changes（逐 crate 签名）

### 2.1 `crates/aero-storage/src/audit_governance.rs`（H3 落点：新代码 + 既有 fixtures 同模块共存，既有名字/字面量逐字不动）

```rust
/// H3 交付：v2 出站写仓储。与 connector 的 `OutboxRepo`（claim/settle/dead）互补：
/// 本仓储只负责**入队**（append / 聚合），消费侧零改动。
#[derive(Clone)]
pub struct AuditGovernanceOutboxRepo {
    pool: PgPool,
}

impl AuditGovernanceOutboxRepo {
    pub fn new(pool: PgPool) -> Self;

    /// 事务内写 outbox 行（镜像 `AuditRepo::append_in_tx` 形态）。event_id = audit_events.id。
    /// 0239 已内置 `ON CONFLICT (event_id) DO NOTHING` → 重放幂等（连接级 Err 传播）。
    pub async fn append_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        event_id: AuditId,
        class: &str,
        priority: i16,
        payload: serde_json::Value,   // 必含 idempotency_key（CHECK jsonb_typeof='object'）
    ) -> Result<(), sqlx::Error>;
}

/// 0239 触发器信封的 Rust 镜像（schema_id 'aero.im.security' / schema_version 1 /
/// event_type 'aero.im.security' / aggregate_type 'workspace' / actor /
/// targets / data_classification 'confidential' / retention_class 'security' /
/// idempotency_key=event_id::text / occurred_at）。source_system 参数化：
/// auth 显式写 = "aero-auth"；moderation 触发器 = binding.source_system。
/// detail 必须是 object（auth 调用点全为最小 object；镜像 `aero_snaplink_audit_payload`
/// 的 object 透传语义，非 object 调用方自行负责——R2 SAVEPOINT 兜底）。
/// **L1 聚合行额外在 envelope 顶层带 `"aggregated": true`**（parity 豁免键，§2.6/§6 AC2）——
/// 1:1 对（auth 显式写 / moderation）绝无此键；聚合标志必须位于顶层 `payload->>'aggregated'`，
/// **不得**下沉进 detail（`payload->'payload'->>'aggregated'` 会让 AC2 豁免谓词失配，且与
/// sibling 0242 聚合行无法共享同一豁免表达式）。
/// db_test 与 moderation parity 测试同形逐字段断言——这是两条入队路
/// （SQL 触发器 vs Rust 显式写）的漂移守卫。
pub fn governance_envelope(
    event_id: AuditId,
    workspace: WorkspaceId,
    actor: Option<ParticipantId>,
    target: Option<&str>,
    detail: serde_json::Value,
    outbound_action: &str,      // 'admin.auth.*'（sibling §2.7 表）
    outcome: &str,              // 'success'（auth 成功事件）| 'failure'（L1 聚合行）
    occurred_at: time::OffsetDateTime,
    source_system: &str,        // auth = "aero-auth"
) -> serde_json::Value;

impl AuditGovernanceOutboxRepo {
    /// R1 + R7 的唯一实现点：RAW SQL SAVEPOINT 包裹 audit+outbox 两行。
    ///
    /// 错误分类（本设计钉死，§2.8 分类表）：
    ///   * `sqlx::Error::Database(_)`（RAISE P0001、CHECK 23514、FK 23503、唯一 23505、
    ///     瞬时 40P01/40001/55P03 等）→ `ROLLBACK TO SAVEPOINT aero_audit_pair`，
    ///     两行俱无，**原始输入同 tx 入 DLQ `audit_governance_failed_pairs`**（可重放，
    ///     非永久丢失），按 §2.8 分类表日志 + 强制计数，返回 `Ok(None)`
    ///     （fail-open，R7「整对没写」，域照常提交）；
    ///   * 其余 `sqlx::Error`（IO / Protocol / 连接层）→ `ROLLBACK TO SAVEPOINT`
    ///     后向上传播（事务本身已不可用，调用方域操作整体失败——与现状行为一致）。
    /// 入口先做契约预检（非 object `detail` → 计数 `sqlstate="contract"` + `Ok(None)`，
    /// 不碰 DB——F2 定稿：`audit_events.detail` 无 CHECK、信封恒为 object，非 object
    /// detail 不会触发任何 DB 错误，SAVEPOINT 对它是不可达的死路径）。
    /// 成功路径 `RELEASE SAVEPOINT aero_audit_pair`，两行随调用方事务原子提交。
    /// 禁止半对：任何单行失败都回滚整对。
    pub async fn append_pair_in_tx_fail_open(
        tx: &mut Transaction<'_, Postgres>,
        workspace: WorkspaceId,
        actor: Option<ParticipantId>,
        action: &str,               // 本地 token（sibling §2.7 表）
        target: Option<&str>,
        detail: serde_json::Value,
        outbound_action: &str,      // 出站 token（sibling §2.7 表）
    ) -> Result<Option<AuditId>, sqlx::Error>;   // Ok(None) = 审计被 fail-open 跳过（已入 DLQ，§2.8）
                                                 // class 'admin'、priority 10 常量内置

    /// login/refresh 用：无域事务的独立事务对（begin → audit → outbox → commit）。
    /// **签名定稿（F4/D12）：返回 `Option<AuditId>`——吞掉一切，无 `Err` 分支**。
    /// 内部 = `begin()` → 复用 `append_pair_in_tx_fail_open(&mut tx, …)` → `Ok` 时
    /// commit（Database 类失败已由共享写路径入 DLQ + 分类日志，DLQ 行随本独立事务
    /// 提交）；`Err`（连接级）时 rollback + warn → `None`。R7 由类型强制：调用方
    /// 拿不到 `Err`，无法传播（`Err` 只留在 in-tx 变体，供域事务向上传播）。
    /// `Some(id)` = 对已提交；`None` = 审计被 fail-open 跳过；无半对（独立事务自身回滚）。
    pub async fn record_pair_standalone(
        &self,
        workspace: WorkspaceId,
        actor: Option<ParticipantId>,
        action: &str,
        target: Option<&str>,
        detail: serde_json::Value,
        outbound_action: &str,
    ) -> Option<AuditId>;

    /// L1 聚合（§2.6）：按 `login_failures.created_at` 分桶，只聚合已关闭桶，
    /// 每桶恰 1 行 outbox（uuid-v5 确定性 event_id、class 'message'、priority 10、
    /// payload.count=N + aggregated=true）。返回本次新建行数；Err 仅连接级。
    pub async fn aggregate_login_failure_buckets(&self, window_secs: i64) -> Result<usize, sqlx::Error>;
}

/// 共享错误分类器（D4/D9，§2.8）：`sqlx::Error::Database(_)` → fail-open（SAVEPOINT 回滚
/// + DLQ + 分类日志/计数）；其余（IO/Protocol/PoolTimedOut/连接层）→ in-tx 传播 /
/// standalone warn+`None`。两写路径共用，G1 单测钉死传播支。`PgDatabaseError` 是
/// `pub(crate)`（sqlx-postgres 0.8.6 error.rs:12），测试代码无法构造 `Database(_)`——
/// Database 侧由真实 PG db_tests（AC1 fail-open 三测试 + F1/F3 组）行为钉死。
pub fn is_fail_open_error(err: &sqlx::Error) -> bool;

/// DLQ 补偿（D9，§2.8）：`audit_governance_failed_pairs` 表（**0244**——本切片第二迁移；
/// 0243 = login_failures 索引，编号定稿见 §7 D8）。被 fail-open 丢弃的
/// 审计对原始输入 + 错误 SQLSTATE 落此；无触发器、无 FK（防递归、防坏行阻塞），
/// connector 不 claim 本表（producer 侧只写；重放 = 运维循环，本切片无路由/CLI 消费方）。
pub struct FailedPairRepo { pool: PgPool }
impl FailedPairRepo {
    pub fn new(pool: PgPool) -> Self;
    /// 同 tx 入队（in-tx 写路径在 `ROLLBACK TO SAVEPOINT` 后调用；随调用方事务原子提交：
    /// 域提交 ⇒ DLQ 行在，域回滚 ⇒ DLQ 行无——与被丢对同一致）。
    pub async fn enqueue_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        workspace: WorkspaceId,
        actor: Option<ParticipantId>,
        action: &str,
        target: Option<&str>,
        detail: serde_json::Value,
        outbound_action: &str,
        error_sqlstate: &str,
        error_message: &str,
    ) -> Result<(), sqlx::Error>;
    /// 独立 best-effort tx 入队（standalone 写路径用；自身失败仅 warn——与 F8 崩溃窗口
    /// 同类，记录为已接受代价：DLQ 覆盖 Database 类，连接级窗口不覆盖）。
    pub async fn enqueue_standalone(
        &self,
        workspace: WorkspaceId,
        actor: Option<ParticipantId>,
        action: &str,
        target: Option<&str>,
        detail: serde_json::Value,
        outbound_action: &str,
        error_sqlstate: &str,
        error_message: &str,
    ) -> Result<(), sqlx::Error>;
    /// 重放：以 DLQ 行输入重建审计对（新事务、新 AuditId——原对从未提交，无幂等冲突），
    /// 成功后置 `replayed_at`。db_test 证明机制；消费面留给后续 aero-cli/管理路由
    /// （`docs/design/2026-08-07-aero-cli-b5-4-audit-provision-check*` 是自然落点）。
    pub async fn replay(&self, id: i64) -> Result<Option<AuditId>, sqlx::Error>;
    /// DLQ 深度（gauge 采样 + 测试用）。
    pub async fn count(&self) -> Result<i64, sqlx::Error>;
}

/// `SnaplinkCommercialRepo` 新增（D13 boot fail-loud 锚点 + F1 db_tests 共用）：
/// `SELECT EXISTS(SELECT 1 FROM snaplink_commercial_bindings WHERE workspace_id = $1 AND enabled)`。
pub async fn has_enabled_binding(&self, workspace: WorkspaceId) -> Result<bool, sqlx::Error>;
```

实现要点：

- `append_pair_in_tx_fail_open` 内 audit 行经 `AuditRepo::append_in_tx`（generic `append_on` 先例）写 `audit_events`，返回的 `AuditId` 即 outbox `event_id`（1:1 前提）；outbox 行经 `append_in_tx` 写，payload = `governance_envelope(…, outbound_action, "success", occurred_at=audit created_at, source_system="aero-auth")`。
- **入口契约预检（F2 定稿）**：`detail.is_object()` 非真 → warn + 计数器 `sqlstate="contract"` + `Ok(None)`（不碰 DB；原「R2 SAVEPOINT 兜底」表述作废——SAVEPOINT 对非 object detail 不可达，见函数 doc）。
- **Database 错误分支（F1/D9）**：`ROLLBACK TO SAVEPOINT aero_audit_pair` → `FailedPairRepo::enqueue_in_tx`（同 tx，`error_sqlstate = PgDatabaseError::code()`）→ 按 §2.8 分类表日志 + 计数 → `Ok(None)`。连接级：回滚 + 传播（in-tx）/ warn + `None`（standalone）。
- SAVEPOINT 命名 `aero_audit_pair`（事务内唯一，禁止嵌套调用本函数——调用方约定单层）；**约束即时性 pin（§2.8）**：0239/0236 的 CHECK/RAISE 均 immediate——若未来改 `DEFERRABLE`，错误推迟到 COMMIT，SAVEPOINT 分支失效（整事务 fail-closed 回滚，违反 R7）；DDL 注释与本模块 doc 各钉一句。
- `record_pair_standalone` = `begin()` → 复用 `append_pair_in_tx_fail_open(&mut tx, …)` → `Ok` 时 commit（DLQ 行随独立事务提交）、`Err`（连接级）时 rollback + warn → `None`（**签名 `Option<AuditId>`，F4/D12**）。两写路径共享同一写逻辑、分类器与 DLQ 入队点。
- `aggregate_login_failure_buckets` 的 SQL（§2.6 公式）与 `governance_envelope` 的 `payload.detail` 形状由 db_test `login_failure_l1_aggregation_n_to_one` 钉死。

### 2.2 `crates/aero-storage/src/registration.rs`

```rust
pub struct NewRegistrationAudit {
    pub action: &'static str,          // "auth.register"
    pub target: String,                // participant_id::text
    pub detail: serde_json::Value,     // json!({})（最小化，无邮箱明文）
    pub outbound_action: &'static str, // "admin.auth.register"
}
pub struct NewRegistration { /* 既有字段不变 */ , pub auth_audit: Option<NewRegistrationAudit> }
```

`create()` 在既有事务内（domain 行之后、`tx.commit()` 之前）调用
`AuditGovernanceOutboxRepo::append_pair_in_tx_fail_open(&mut tx, workspace_id, Some(participant_id), audit.action, Some(&audit.target), audit.detail, audit.outbound_action).await?`——**`Ok(None)` 静默继续（R7），`Err` 传播（连接级）**。3 处构造点更新（`registration.rs:156/:257` db_tests 传 `None`、`service.rs:233` register_enrolled 传 `Some`）。

### 2.3 `crates/aero-auth/src/service.rs`

- `register_enrolled` :221：构造 `NewRegistration { …, auth_audit: Some(NewRegistrationAudit { action: "auth.register", target: participant_id.to_string(), detail: json!({}), outbound_action: "admin.auth.register" }) }`。公开签名不变。
- `refresh` :334：成功后
  `AuditGovernanceOutboxRepo::new(self.repo.pool().clone()).record_pair_standalone(WorkspaceId::from_uuid(uuid::Uuid::nil()), Some(pid), "auth.refresh", Some(session_id.to_string()), json!({}), "admin.auth.refresh").await`——返回 `Option<AuditId>`（F4/D12：吞掉一切，无 `Err` 分支；Database 类已入 DLQ + 分类日志，连接级内部 warn）。`DEFAULT_WORKSPACE_ID` 经 `aero_common::WorkspaceId::from_uuid(uuid::Uuid::nil())`（aero-auth 无常量，照 sibling §2.3 写法）。
- `register` :184（库级路径）与 `login` :260：**不改**（login 审计点在 handler 侧，2FA 终局可见性，D5）。

### 2.4 `crates/aero-server/src/routes/handlers/auth.rs`（login 成功审计点）

`finalize_login(&email, true)`（:197）之后、`record_login_event`（:208）旁：

```rust
let _ = AuditGovernanceOutboxRepo::new(s.pg.clone())
    .record_pair_standalone(
        DEFAULT_WORKSPACE_ID, Some(out.participant.id),
        "auth.login", Some(out.participant.id.to_string()),
        serde_json::json!({}), "admin.auth.login")
    .await; // 内部已 fail-open（Option<AuditId>：Database 类入 DLQ + 分类日志，连接级 warn——均不翻 login 结果）
```

既有 `auth.login.new_ip`（:208 区）/ `auth.login.recovery_code`（:215-227）best-effort 路径**原样保留**（不同 action，不冲突，不得改成 in-tx——语义既定）。

### 2.5 `crates/aero-server/src/{pat.rs, twofa.rs, sessions.rs, session.rs, admin_sessions.rs}` + storage 侧 tx 变体

路由统一编排模式（begin → 域语句 `_in_tx` 变体 → 审计对 → commit；审计对失败 = SAVEPOINT 跳过，域照常提交）。**token 表 = sibling §2.7 定稿，本设计直接引用**：

| 路由 | 域语句 tx 变体（storage） | 本地 token / 出站 token | target / detail |
|---|---|---|---|
| `POST /api/pat`（pat.rs） | `PatRepo::create_in_tx(tx, participant, token_hash, name, scopes, expires_at) -> Result<PatId>` | `auth.pat.issue` / `admin.auth.pat.issue` | pat_id / `{"scopes":[…]}`（不含 name——用户自由文本不进审计） |
| `DELETE /api/pat/:id` | `PatRepo::revoke_in_tx(tx, id, participant) -> Result<bool>` | `auth.pat.revoke` / `admin.auth.pat.revoke` | pat_id / `{}` |
| `POST /api/me/2fa/enroll` | `TotpRepo::upsert_secret_in_tx(tx, …)` | `auth.totp.enroll` / `admin.auth.totp.enroll` | participant_id / `{"stage":"enroll"}` |
| `POST /api/me/2fa/verify`（激活） | `TotpRepo::activate_in_tx(tx, participant)` | `auth.totp.enroll` / `admin.auth.totp.enroll` | participant_id / `{"stage":"activate"}` |
| `DELETE /api/auth/sessions/:sid` | `SessionRepo::revoke_and_blacklist_in_tx(tx, id, participant)`（auth_session.rs:499 CTE 的 executor 泛化） | **`session.revoked`（沿用既有，兼容消费方）** / `admin.auth.session.revoke` | session_id / `{"session_id":…}` |
| logout（`POST /api/auth/logout`） | 同上（按 token_hash 的吊销变体 `_in_tx`） | `session.revoked` / `admin.auth.session.revoke` | session_id / `{}` |
| admin force-revoke（admin_sessions.rs → auth_session.rs） | **在既有事务内嵌审计对**（路由零改动） | `session.revoked.admin`（新增 token）/ `admin.auth.session.revoke` | 被吊销 participant_id / `{"admin_revoked":true}` |

替换后删除 `sessions.rs:108` / `session.rs:212` 的 pool 级事后 `audit_session_revoked`（被 in-tx 对取代；action 字符串不变 → 审计轨迹连续）。admin_sessions.rs 是**新增**审计（现状零审计，勘误①）。

`_in_tx` 变体实现 = 镜像 `AuditRepo::append_on` 的 generic-executor 模式（`sqlx::query(...).execute(executor)`），**不改既有 pool 方法**（`create`/`revoke`/`revoke_and_blacklist` 保持——verify 等读路径无事务需求）。

### 2.6 L1 聚合（login 失败）

- **底座**：`login_failures`（0140，逐尝试已持久化）。
- **桶公式（本设计钉死）**：`bucket_start_epoch = floor(extract(epoch from created_at) / window) * window`（整数秒，跨窗口无漂移）。**已关闭桶** = `bucket_start_epoch + window <= extract(epoch from clock_timestamp()) - window`（桶尾 ≤ now − window，即最近一个开桶永不聚合，迟到行无截半风险）。
- **聚合 SQL 形状**（实现于 `aggregate_login_failure_buckets`，`ON CONFLICT DO NOTHING` 幂等）：

```sql
-- 第一步：已关闭桶 + 计数
SELECT floor(extract(epoch from created_at) / $1)::bigint * $1 AS bucket_start,
       COUNT(*)::bigint AS cnt
  FROM login_failures
 GROUP BY bucket_start
HAVING bucket_start + $1 <= extract(epoch from clock_timestamp()) - $1;

-- 第二步（每桶一条，确定性 event_id 由 Rust 侧 Uuid::new_v5 合成）：
INSERT INTO audit_governance_outbox (event_id, status, class, priority, payload)
VALUES ($1, 0, 'message', 10, $2)
ON CONFLICT (event_id) DO NOTHING
RETURNING event_id;
```

- **event_id**：`Uuid::new_v5(&Uuid::NAMESPACE_URL, "aero.im.audit.l1:{workspace}:{action}:{bucket_start_epoch}")`，workspace=DEFAULT nil、action=`auth.login.failure`——确定性 → 重跑/并发幂等（`ON CONFLICT DO NOTHING` 双保险）。
- **payload** = `governance_envelope(event_id, DEFAULT_WORKSPACE_ID, None, None, detail, "admin.auth.login.failure", "failure", occurred_at=bucket_start, source_system="aero-auth")`，detail = `{"count": N, "window_start_epoch": …, "window_secs": …}`，**envelope 顶层合并 `"aggregated": true`**（`payload->>'aggregated' = 'true'`）——这是 AC2 parity 的**跨切片共享豁免键**（§6 AC2）：sibling aero-ai 0242 触发器行（含 spill 行）同样在 envelope 顶层带 `aggregated=true`（协调项，见 sibling design §2.4），两切片聚合行用同一表达式豁免，**不得**改用 `window_start`/`count` 等形状特征做联合谓词（会随任一切片改形状静默收窄）。
- **无 audit_events 行**：‡ 类 1:1 豁免；避免 0236 v1 触发器副作用；避免 default-workspace 审计视图污染（`audit_governance_outbox.event_id` 无 FK，合法）。`login_failures` 保留逐尝试行作 forensic 底座。
- **不触 `admin_class_rows_never_aggregated`**：聚合行自身 class 'message'（message 域本就是 L1 聚合窗候选域），该 pin 的断言对象（admin 行永不进 message 窗）不受影响。
- **定时器**：`bin/boot/background.rs` 新 ticker（与 sibling design §2.6 一致的文件选择），env `AERO__SERVER__LOGIN_FAILURE_L1_AGGREGATE_SECS`（默认 30，0 禁），`MissedTickBehavior::Skip`，best-effort warn（§2 定时器惯例，持共享 cancel token）。调用 `AuditGovernanceOutboxRepo::new(state.pg.clone()).aggregate_login_failure_buckets(secs)`。
  **F5（D11 定稿）**：`=0` 禁用时 closed 桶永不聚合，`sweep_login_failures`（默认 180d）最终删除底座行 ⇒ 该窗口 v2 失败信号永久丢失——**接受并记录**：聚合是 near-real-time 信号非账本；**保留窗口内重开定时器即自愈**（确定性闭桶全扫描 + `ON CONFLICT DO NOTHING` 自带 backfill——与 moderation 需 0241 reconciler 不同，L1 是 pull 全扫描，从底座行随时可重建）；超窗口不可恢复（行已删，无机制能重建），且保留清扫本身是 PII/GDPR 特性，不为信号保行。boot 时若本 env 解析为 0 且 `AERO__SERVER__LOGIN_FAILURE_RETENTION_DAYS > 0`，`tracing::warn!` 一次明示该语义（§4 F15）。

### 2.7 auth 事件 token 表

= sibling design §2.7 定稿（本设计逐字引用，不重复论证）：`auth.register`/`admin.auth.register`、`auth.login`/`admin.auth.login`、`auth.refresh`/`admin.auth.refresh`、`auth.pat.issue`/`admin.auth.pat.issue`、`auth.pat.revoke`/`admin.auth.pat.revoke`、`auth.totp.enroll`/`admin.auth.totp.enroll`、`session.revoked`（沿用既有）/`admin.auth.session.revoke`、`session.revoked.admin`/`admin.auth.session.revoke`、L1 `—`/`admin.auth.login.failure`（class **message**、priority 10）。修正点：R3 原提案 `auth.session.revoke` 改为沿用既有 `session.revoked`；登录失败 class 定稿 message（D2 结局 A）。

### 2.8 错误分类表与可观测（D4/D9/D10 落点）

> aero-storage 现状零 metric（`rg metrics crates/aero-storage/src/` 仅 room.rs:391 注释）；pair 写路径成为 storage 首个 metric 是 deliberate 例外（§0 复核）。常量名照 `aero-common/src/metrics.rs` 的 `names` + `register_known_metrics`（:791）惯例，labeled helper 照 `metrics_tasks.rs` 既有 30s AI DLQ 深度 gauge sampler（:172-190）模式。

**错误分类表**（`append_pair_in_tx_fail_open` 内唯一判定点，`is_fail_open_error` 分类器；**SQLSTATE 是分类键**，日志行必带 `error_sqlstate`，计数器按 category 折叠保基数有界）：

| SQLSTATE / 错误类 | 日志级别 | 计数器（`audit_auth_write_failures_total`） | DLQ | 返回 |
|---|---|---|---|---|
| 非 object detail（Rust 契约预检，F2） | warn（`sqlstate="contract"`） | `{category="contract"}` | 否（未碰 DB） | `Ok(None)` |
| `P0001` + constraint=`snaplink_binding_required`（0236 v1 触发器，F1 确定性配置类） | **error**（经 D13 库层不变量经受支持路径不可达，到达即配置损坏/手动 SQL 信号） | `{category="binding"}`（**强制**） | 是 | `Ok(None)` |
| 契约 bug 类：`23514` CHECK / `23503` FK / `23505` 唯一 / 其他未知 Database | **error**——代码缺陷，审计丢失应告警（优于 warn：到达即信封/引用/重复 bug） | `{category="database"}` | 是 | `Ok(None)` |
| 瞬时并发类：`40P01` deadlock / `40001` serialization / `55P03` lock timeout | warn——预期并发噪声，无配置/bug 含义 | `{category="database"}` | 是（重放通常成功） | `Ok(None)` |
| IO / Protocol / PoolTimedOut / 连接层 | in-tx：传播不记（上层日志）；standalone：warn | 不计数（无写入发生，与 F8 同类） | 否（连接不可用） | in-tx 传播 / standalone warn+`None` |

**可观测点（全部强制，D10）**：① 计数器 `audit_auth_write_failures_total`（labels `category` ∈ {contract, binding, database}；常量 `AUDIT_AUTH_WRITE_FAILURES_TOTAL` 入 `aero-common/src/metrics.rs` `names` + `register_known_metrics` help）；② DLQ 深度 gauge `aero_audit_governance_failed_pairs`（`FailedPairRepo::count`，`metrics_tasks.rs` 30s sampler，镜像 AI DLQ 深度模式）——非零即丢对待重放信号；③ boot 时 `AERO__SERVER__LOGIN_FAILURE_L1_AGGREGATE_SECS=0` 且 `AERO__SERVER__LOGIN_FAILURE_RETENTION_DAYS>0` 的 `tracing::warn!` 一次（F15/D11）。**日志契约**：ERROR/warn 行必含 `workspace_id`、`action`、`path`（in_tx|standalone）、`error_sqlstate`、DLQ 行 id（入队后）；**不记 detail**（纵深防御，detail 只进 DLQ 表）。**约束即时性 pin**（D4 复核）：0239/0236 的 CHECK/RAISE 均 immediate——若未来改 `DEFERRABLE`，错误推迟到 COMMIT，SAVEPOINT 分支失效（整事务 fail-closed 回滚，违反 R7）；DDL 注释与本模块 doc 各钉一句。

## 3. Compatibility constraints
1. **唯一新表 = DLQ `audit_governance_failed_pairs`（0244）；零既有 DDL 改动**：0239/0240/0241 逐字不动；`governance.rs` 映射权威不动（auth 走显式写路，不扩触发器 token 集——D1）。
2. **本切片两新迁移（编号定稿）**：`migrations/0243_login_failures_created_at_idx.sql`——`CREATE INDEX IF NOT EXISTS login_failures_created_at_idx ON login_failures (created_at)`（幂等、纯加索引）；`migrations/0244_audit_governance_failed_pairs.sql`——DLQ 表（DDL 见 §5.3，无触发器、无 FK，防递归/防坏行阻塞）。**编号规则见 §5.2 / §7 D8**：本切片**无条件取 0243 + 0244**，`0242_audit_governance_l1_aggregate.sql` 归 sibling aero-ai 切片（`2026-08-08-aero-ai-b5-1-l1-aggregation-in-tx-audit.design.md` §2.4）。**顺序：`cargo build` → `aero-cli migrate`**（§4.2 编译期嵌入，`sqlx::migrate!("../../migrations")` 实测于 `aero-storage/db.rs:58`）。
3. **connector/relay 零改动**：`claim_due` 已按 `priority DESC` 返回带 lanes 的 `Claim`；auth 行（admin/10）与 L1 行（message/10）进既有状态机（退避 cap 300s、MAX_ATTEMPTS=5 → dead、403 首次 mark_dead）。payload 原样转发；sink Idempotency-Key = event_id 对 auth 行与 L1 合成 event_id 同样成立。
4. **既有 pin 测试逐字保持**：`moderation_finalize_outbox_parity`、`ddl_contract_defaults_and_checks`、`admin_class_rows_never_aggregated`、`governance_lane_for`、0239/0240/0241、`audit_governance.rs` 模块 doc cross-slice pin 注释——名字/字面量不动。b5-pin.sh 37 槽逐字不动（新 verdict 行 `B5-CHECK auth_outbox_parity: PASS` 不影响守卫——守卫只对列表内 executed 槽 grep，实测 `assert_b5_contract_pin` 逻辑）。
5. **既有审计轨迹连续**：`auth.login.new_ip`、`auth.login.recovery_code`、`session.revoked`（用户侧）action 字符串不变；`LoginFailureRepo::record`、`record_login_event` 行为不变。
6. **login throttle / finalize_login 语义不变**：审计写在 `finalize_login(true)` 之后（handler 侧），2FA 失败依旧只走 `login_failures`（不记成功登录）。
7. **v1 路径共存**：auth 显式对在 enforcement 开时经 0236 触发器自然产 v1 `snaplink_delivery_outbox` 行（与 moderation 行同等待遇）——v1/v2 双轨并存，本切片不动 v1 路径；L1 聚合行无 audit_events 行 → 天然不进 v1。
8. **harness 约束**：所有新 db_tests 落在 aero-storage `audit_governance.rs` 模块 → `run_migrated_integration` 硬编码 `-p aero-storage` 零参数化；仅 test-integration.sh 追加一条命名条目（§6）。
9. **auth 公共 API 无破坏**：`AuthService` 公开签名不变（`NewRegistration` 加字段 = crate 内 3 处构造点更新，编译强制）；`PatRepo`/`TotpRepo`/`SessionRepo` 既有方法不动（纯新增 `_in_tx` 变体）。
10. **无新依赖**：uuid v5 已启用（root :110）；sqlx 运行时 query（非宏）→ 无 offline 数据重建；`time` 已是 aero-storage 依赖（`audit.rs` 用 `time::OffsetDateTime`）。

## 4. Failure modes

| # | 失效 | 机制 | 后果 / 处置 |
|---|---|---|---|
| F1 | Database 类写失败：**确定性配置类**（enforcement on + 无 enabled 绑定 → 0236 v1 触发器 P0001 `snaplink_binding_required`，`aero_snaplink_binding_for_workspace` 0235:193）+ 契约 bug 类（23514/23503/23505）+ 瞬时类（40P01/40001/55P03） | SAVEPOINT → `ROLLBACK TO aero_audit_pair`（`is_fail_open_error` 分类，§2.1）→ **同 tx 入 DLQ `audit_governance_failed_pairs`（0244，D9）** | 「整对没写」+ 原始输入入 DLQ（**可重放，非丢失**）；域行照常提交（R7 fail-open）。**日志/计数按 §2.8 分类表**：P0001 与契约 bug 类 **ERROR** + 强制计数，瞬时类 warn + 计数（均带 `error_sqlstate`）。**不变量锚点见 §7 D13**（`configure_enabled` 的 `require_complete_workspace_coverage` 与开关翻转同事务——enforcement on ⇒ nil binding 是库层强制，非 ops 期望）；boot fail-loud 见 F14 |
| F2 | 连接级错误（DB 断、tx 死） | in-tx：`Err` 向上传播（§2.1 分类连接支）；standalone：内部 warn + `None`（D12） | in-tx：域操作整体失败（R1 三组行俱回滚，0 行逃逸）——与现状行为一致（域写本来就依赖 DB）；standalone：token 照常返回，该事件审计缺（与 F8 崩溃窗口同类，已接受）；不计数（无写入发生） |
| F3 | login/refresh 审计对写失败 | `record_pair_standalone`（D12：`Option<AuditId>` 吞掉一切）内部：Database 类 → 共享写路径入 DLQ + 分类日志/计数；连接级 → warn；均返回 `None` | token 照常返回（auth 不可被审计 DoS，R7）；无半对（独立事务回滚）；Database 类丢对**可重放**（DLQ） |
| F4 | outbox event_id 冲突（ULID 碰撞，理论级） | 显式写路径普通 INSERT → 23505 → SAVEPOINT 回滚整对（0239 的 `ON CONFLICT DO NOTHING` 只保护触发器路径；**F4 定稿：整对消失，绝不保留单行**——半对违反 R1/P2，原「审计行保留」表述自相矛盾已删） | 同 F1 处置：DLQ（0244）+ ERROR + 计数 `category="database"`（`error_sqlstate='23505'`） |
| F5 | relay 投递失败/重放 | 既有 connector 状态机（退避 cap 300s、MAX_ATTEMPTS=5 → dead + last_error） | auth 行同 moderation 行待遇；dead 行 last_error 可观测 |
| F6 | L1 聚合并发/重跑（多实例 timer） | 确定性 uuid-v5 event_id + `ON CONFLICT DO NOTHING` | 恰 1 行；count 不因重跑翻倍（已关闭桶不可再收行，count 确定性） |
| F7 | 时钟倾斜致迟到行落已关闭桶 | 桶键 = created_at 地板（`floor(epoch/window)*window`） | 迟到行计入下一开桶或永失——可接受（`login_failures` 保留逐尝试行作 forensic 底座）；聚合是 near-real-time 信号非账本；**保留窗口内重开聚合即自愈**（确定性闭桶全扫描自带 backfill，见 F15/D11） |
| F8 | handler 在 `finalize_login(true)` 与审计写之间崩溃 | 无行（写入未发生） | 登录成功但缺审计行——fail-open 既定代价（R7）；不阻塞登录、无半对 |
| F9 | L1 行被 relay 在窗口关闭前 claim | 只聚合**已关闭**桶（`bucket_start + window <= now - window`） | 行首次出现即完整（count 终值）；relay claim 时序无法截半 |
| F10 | admin revoke 审计失败 | 既有事务内 SAVEPOINT | revoke 照常（R7）；admin 侧从零审计变为 fail-open 审计 |
| F11 | 审计写占用长事务（慢） | 对短（3 语句） | 无新风险；savepoint 开销 ~µs 级 |
| F12 | 0242 编号冲突（sibling 已占号） | **§5.2/§7 D8 定稿：本切片取 0243（索引）+ 0244（DLQ），sibling 保留 0242** | 两迁移互不依赖（索引/DLQ vs 触发器）；同版本嵌入在迁移期硬失败（`_sqlx_migrations` version PK），故用固定分配消除条件式竞态；harness 门控挂在 0239 文件上，与编号无关 |
| F13 | 前置批次未提交被 `git reset --hard` 清掉 | §5.1 硬前置（先 commit 再动工） | 0239/0240/0241 + fixtures + connector 全部丢失 → 必须先提交 |
| F14 | enforcement on × nil workspace 无 enabled 绑定（确定性配置类） | **boot fail-loud 已存在于库层（D13，本切片零新增代码）**：`configure_enabled`（`snaplink_commercial.rs:175-200`）= `reject_omitted_enabled_bindings`（:434）+ `require_complete_workspace_coverage`（:453，所有 `workspaces` 行含 nil 必须有 enabled binding）+ 开关翻转**同事务**；缺覆盖 → Err → `from_env`（runtime.rs:66）`?` 传播 → main.rs `context(...)?` 拒绝启动 | 经受支持路径**不可达**（生产写开关仅 `configure_enabled`/`configure_disabled`/`require_disabled` 三 repo 方法，rg 实证无 HTTP 端点）；残余面 = 手动 SQL 直改开关（任何不变量都防不了）——到达时由 F1 的 ERROR + 计数 + DLQ 覆盖 |
| F15 | L1 聚合禁用（`AERO__SERVER__LOGIN_FAILURE_L1_AGGREGATE_SECS=0`）× 保留清扫（`AERO__SERVER__LOGIN_FAILURE_RETENTION_DAYS`，默认 180d） | closed 桶永不聚合 → `sweep_login_failures` 删底座行 | v2 失败信号该窗口永久丢失——**接受并记录（D11）**：聚合是 near-real-time 信号非账本；**保留窗口内重开定时器即自愈**（pull 全扫描自带 backfill，异于 moderation 需 0241 reconciler）；超窗口不可恢复（行已删，无机制能重建）；保留清扫是 PII/GDPR 特性，不为信号保行；boot 一次 warn 明示（§2.6） |

## 5. Migration steps

1. **前置（必须先做）**：提交上一批次 untracked 物——`migrations/0239/0240/0241`、`crates/aero-ai/src/governance.rs`、`crates/aero-common/src/model/audit.rs`、`crates/aero-audit-connector/`、`crates/aero-storage/src/audit_governance.rs`、`scripts/b5-pin.sh`、`scripts/test-b5-pin-guard.sh`、test-integration.sh 改动（§0 实测 94 项在途）。否则 `git reset --hard` 校准丢 B5-1 地基。
2. **0242/0243 编号定稿（§7 D8 落地规则，非条件式）**：本切片**无条件取 0243**（`migrations/0243_login_failures_created_at_idx.sql`），sibling aero-ai 保留 0242（`0242_audit_governance_l1_aggregate.sql`）。理由（对 sibling design §2.4 的复核结论）：① 两文件同在**一个未提交工作树**——编号是单点编辑决策，不存在「先落先占」的分布式竞争，D8 原「renumber unless already taken」条件式在单工作树下要么永不触发（sibling 后写 0242 → 重复版本 → 迁移期硬失败），要么取决于落地先后这一非设计因素；② sibling 设计全文（§0 证据行、§2.4 标题、§5 step 4、F8）逐字引用 `0242_audit_governance_l1_aggregate.sql`，**零让号语言**；本设计已携带完整顺延机制（D8/F12/§5），预让号零额外工作；③ 本切片迁移是纯索引（行为中性，AC3 不依赖它，test-plan 复核 D6 已证；顺带服务既有 retention 清扫），sibling 0242 是写路径触发器——让触发器保住其文档中的号位，索引让号代价最低。**落地前检查**：`ls migrations/*.sql | grep -cE '^(0242|0243|0244)_'` 必须 ≤1（本切片写文件前 0243/0244 必须为 0、0242 为 0 或 1）；**DLQ 表随 0244 落**（同一批次，两迁移互不依赖；0244 是 fail-open 写路径的硬前置——代码落地前未迁移则 DLQ INSERT 报 42P01 undefined_table 且发生在 SAVEPOINT 回滚后，会以 Database 错误逃逸破坏 R7，故必须 build→migrate 同批交付）。两文件同版本不是静默 no-op 而是**迁移期硬失败**（sqlx 0.8.6 `_sqlx_migrations` 以 version 为主键，第二个同版本 INSERT 撞 PK → `MigrateError::ExecuteMigration`，实测于 sqlx-postgres-0.8.6 `execute_migration`）。harness 无 0242/0243/0244 文件门控（只门控 0239）。
3. **迁移 0243**：`migrations/0243_login_failures_created_at_idx.sql`（1 条 `CREATE INDEX IF NOT EXISTS`；文件头注释写明「原计划 0242，因 sibling aero-ai 切片占用 0242（L1 聚合触发器）顺延 0243」，交叉引用 sibling design §2.4）。**顺序：`cargo build` → `aero-cli migrate`**（§4.2 编译期嵌入——漏 build 则新迁移静默 no-op）。
4. **迁移 0244（DLQ 表）**：`migrations/0244_audit_governance_failed_pairs.sql`——`CREATE TABLE IF NOT EXISTS audit_governance_failed_pairs (id BIGSERIAL PRIMARY KEY, workspace_id UUID NOT NULL, actor_id UUID, action TEXT NOT NULL, target TEXT, detail JSONB NOT NULL, outbound_action TEXT NOT NULL, error_sqlstate TEXT NOT NULL, error_message TEXT, created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(), replayed_at TIMESTAMPTZ)`；**无触发器、无 FK**（防递归——本表写入发生在 0236/0239 触发器作用域外；防坏行阻塞重放）；connector 不 claim 本表。**顺序：`cargo build` → `aero-cli migrate`**（§4.2）。
5. aero-storage：`AuditGovernanceOutboxRepo`（H3）+ `governance_envelope` + `append_pair_in_tx_fail_open`（含契约预检 + DLQ 入队 + 分类日志/计数）+ `record_pair_standalone`（`Option<AuditId>`）+ `FailedPairRepo`（enqueue/replay/count）+ `is_fail_open_error` 分类器 + `SnaplinkCommercialRepo::has_enabled_binding` + `aggregate_login_failure_buckets` + `_in_tx` 仓储变体（pat/totp/auth_session）+ `NewRegistration.auth_audit` + 全部 db_tests（§6）。
6. aero-auth：`register_enrolled` 审计 spec（`NewRegistration.auth_audit: Some(…)`）+ `refresh` 写点（fail-open 包装）。
7. aero-server：login handler 审计点（`finalize_login(true)` 后）；pat/twofa/sessions/session 路由 tx 编排（删 pool 级事后审计）；admin revoke 存储侧嵌对；boot timer（`AERO__SERVER__LOGIN_FAILURE_L1_AGGREGATE_SECS`）+ 禁用时一次 `tracing::warn!`（F15/D11）；`metrics_tasks.rs` DLQ 深度 gauge sampler + aero-common 两新 metric 注册（§2.8/D10）；test-integration.sh B5-1 段追加 `auth_outbox_parity` 条目。
8. **无 backfill**：auth 行从部署起才产生（历史 auth 操作无审计——如实记录，不回填伪造）；旧二进制（无 auth 写入）与新版 relay 混跑无冲突（0243 纯加索引；auth 行只是新出现的 outbox 行）。**回滚**：代码回退即可；已写行继续被 relay 消费（status 机自洽）；0243 索引与 0244 DLQ 表可留（无害）。
9. 门禁：`cargo check --workspace` / `cargo test --workspace --lib`（PG 门控 `-- --ignored` + 一次性库）/ clippy 无新增警告 / `scripts/{truth-check,file-size-check,web-check}.sh`；`test-integration.sh` B5 段全 PASS（AC4 un-gated 无 SKIP）；`scripts/test-b5-pin-guard.sh` 绿（37 槽逐字不动）。

## 6. Testable acceptance mapping（AC1–AC4 → 具体测试）

> 全部新 db_tests 位于 `crates/aero-storage/src/audit_governance.rs` 的 `db_tests` 模块（H3 落点；既有 `audit_governance::` harness 条目自动覆盖，仅补一条命名条目）。PG 门控：`#[ignore = "requires live Postgres"]` + `DATABASE_URL`。测试名即 harness 过滤器。

| 验收 | 测试（名字 = harness 过滤器） | 断言形态 |
|---|---|---|
| **AC1** register 原子性 | `registration_commit_half_writes_audit_and_outbox` / `registration_rollback_half_leaves_zero_rows` / `registration_audit_failure_fail_open_commits_domain` | commit 半部：`RegistrationRepo::create`（带 `auth_audit: Some(…)`）提交后 `audit_events` 恰 1 行（action=`auth.register`）+ `audit_governance_outbox` 恰 1 行（status=0、class='admin'、priority=10、`event_id`=audit id、payload.action=`admin.auth.register`、payload.idempotency_key=event_id::text）；rollback 半部：重复 email → create Err → 两表均 0 行逃逸（same-tx proof，镜像 `moderation_finalize_outbox_parity` 的 A1-RB 半部）；fail-open 半部：`auth_audit.detail` 传非 object → **入口契约预检**（F2 定稿：`audit_events.detail` 无 CHECK、信封恒为 object——非 object detail 不产生任何 DB 错误，SAVEPOINT 对它是死路径）→ **create 成功**、两表 0 行 + DLQ 0 行（预检 `Ok(None)` 证明 R7）；SAVEPOINT 真 DB 错误路径由 F1 回归组（0236 P0001）覆盖 |
| **AC1** login 原子性 | `login_pair_commit_and_rollback` | `record_pair_standalone` 提交后恰 1+1 行（返回 `Some(audit_id)`）；非 object detail 变体 → 契约预检 `None`、0 行（不碰 DB）；P0001 变体（enforcement on + 无 nil binding）→ SAVEPOINT 回滚 + **DLQ 恰 1 行**（`error_sqlstate='P0001'`）+ `None`、0 行（真 DB 错误路径，与 F1 回归组同一注入） |
| **F1 回归**（v1 共存 + fail-open 分类，§7 D13） | `auth_pair_enforcement_on_with_nil_binding_commits_pair_and_v1` / `auth_pair_enforcement_on_without_nil_binding_fails_open_drops_pair` | (a) enforcement 开 + **nil workspace（`DEFAULT_WORKSPACE_ID`，0006:61 种子行）enabled binding**：`record_pair_standalone(DEFAULT_WORKSPACE_ID, …, "auth.login", …)` → audit 对 1+1 行 **且** 0236 v1 行 1 条（`snaplink_delivery_outbox`，`idempotency_key=audit id`）——钉死 §3.7「v1 共存」声明（现零覆盖）；(b) enforcement 开 + **无 nil binding**（fixture 直接翻开关，绕开 `configure_enabled` 覆盖检查——该状态经控制平面不可达，测试钉 fail-open 为有意分类）：pair → `None`、0 行，域行照常提交；**DLQ 恰 1 行（`error_sqlstate='P0001'`）**；**断言计数器 `audit_auth_write_failures_total{category="binding"}` 增量 ≥1**（process-global registry，`>=` 语义防跨测试污染——强制计数行为钉死）；同时是 SAVEPOINT 路径的**真 DB 错误注入**（0236 P0001，替代 AC1 里不可实现的非 object detail 注入，复核 F2）。两测试结束后 `restore_enforcement_disabled` |
| **D4 分类器**（非 PG 单测，G1） | `audit_pair_error_classification_io_protocol_propagate` | `is_fail_open_error`：`sqlx::Error::Io(io::Error::new(ErrorKind::ConnectionReset, "x"))` / `Protocol("conn gone".into())` / `PoolTimedOut` → **false**（传播支）；`sqlx::Error::Database(_)` 侧**不可构造**（`PgDatabaseError` 为 `pub(crate)`，sqlx-postgres 0.8.6 error.rs:12）——由上述真 PG 测试行为钉死，测试 doc 注明（防 `matches!(err, Database or Io)` 式回归） |
| **AC1** PAT/TOTP 原子性 | `pat_revoke_in_tx_audit_pair_commit_and_rollback` / `totp_enroll_in_tx_audit_pair_commit_and_rollback` | 单 tx：`_in_tx` 域语句 + 审计对 → commit 后域行变更 + 1+1 行 + DLQ 0 行；fail-open 半部（0236 P0001 注入，同 F1 回归组）：SAVEPOINT 丢对 + DLQ 1 行 + **tx 继续 commit**（域行变更保留——fail-open 语义的最强证明）；「域行随 tx 整体回滚、0 行逃逸」由域级失败注入证明（镜像重复 email 模式：域语句自身违例 → tx abort → 域行 + 对 + DLQ 全 0） |
| **AC2** claim 车道（storage 侧 SQL 镜像） | `claim_lane_order_moderation_then_auth_then_l1` | 种子三行：moderation（admin/100）、auth 显式行（admin/10）、L1 聚合行（message/10）→ 按 `pg.rs:117` claim ORDER BY 的 SQL 镜像（`WHERE status IN (0,1) ORDER BY priority DESC, available_at, created_at, event_id`）断言 moderation 最先、10-lane 两行按 FIFO；行字段断言 class/priority 与种子一致。connector 侧 `Claim.priority/class` 字段级断言由既有 connector 套件（fake-backed，零改动）覆盖——依赖方向（storage 不依赖 connector）决定本测试落 storage 侧 |
| **AC2** P2 parity | `auth_outbox_parity_1to1`（**新命名 harness 条目**，追加于 test-integration.sh :318 后） | **共享豁免谓词（§7 D13，与 sibling 协调定稿）**，双向集合相等在受控库上成立：
  - **outbox → audit**（无孤儿）：`SELECT count(*) FROM audit_governance_outbox o WHERE NOT (o.payload->>'aggregated' = 'true') AND NOT EXISTS (SELECT 1 FROM audit_events a WHERE a.id = o.event_id)` = 0——豁免键 = **两切片共享的顶层 `payload->>'aggregated' = 'true'`**（本切片 L1 行 §2.6 + sibling 0242 行/ spill 行，协调项已入 sibling §2.4）；
  - **audit → outbox**（无漏写）：`SELECT count(*) FROM audit_events a WHERE a.action IN ('message.moderated','auth.register','auth.login','auth.refresh','auth.pat.issue','auth.pat.revoke','auth.totp.enroll','session.revoked','session.revoked.admin') AND NOT EXISTS (SELECT 1 FROM audit_governance_outbox o WHERE o.event_id = a.id)` = 0——**审计侧豁免 = allowlist 之外一切 action 合法 audit-only**（sibling 的 `message.create/edit`、`room.*`、`auth.login.new_ip`、`auth.login.recovery_code`、`message.deleted`、0239 前存量行），allowlist 字面量交叉引用 §2.7 token 表 + 0239 token 门；测试在 enforcement on + binding 下运行（fixture），故 `message.moderated` 方向成立；
  - 信封逐字段钉死（schema_id/schema_version/aggregate_type/aggregate_id/actor/targets/data_classification/retention_class/idempotency_key/`source_system="aero-auth"`）；
  - **oracle 在 sibling 0242 落地前后均有效**：audit 侧 allowlist 不含 `message.create/edit`（无论聚合行存在与否都豁免），outbox 侧豁免谓词在无聚合行时真空成立——**无需 `to_regprocedure` 自门控**；
  - **生产形态注记**（非测试 SQL）：若部署启用 `AERO__SERVER__AUDIT_RETENTION_DAYS`，audit 行被清扫后存量 outbox 对会合法变孤儿——oracle 升为运行时探针时，audit 侧须按保留窗口截断（`a.created_at > now() - retention`）并将清扫行视为豁免；db_test 不跑清扫，不受影响 |
| **AC3** L1 聚合 N→1 | `login_failure_l1_aggregation_n_to_one` | 种子 N=3 行同桶 + 2 行异桶（window=60s，created_at 显式铺开）→ `aggregate_login_failure_buckets(60)` → 恰 2 行（每桶 1 行）、payload.count=3/2、class='message'、status=0、priority=10、event_id 确定性（uuid-v5 公式重算相等）；**重跑 → 仍 2 行、count 不变**（幂等）；**`payload->>'aggregated' = 'true'` 在 envelope 顶层**（§2.6 定稿——不在 detail 内；parity 共享豁免键）；开桶（created_at ∈ (now−window, now]）不聚合 |
| **AC4** drill 回归 | 既有条目零新增 | `aero-audit-t11-drill` 全绿——`COUNT(status=0)==N`、`COUNT(status IN (1,2,3))==0`、`SUM(attempts)==N`→`2N`（round 1/2）、transport-error rows==N；`aero-audit-relay-drill` / `aero-audit-priority-drill` 亦全绿（0239 已落 → 无 SKIP 分支） |

harness 改动（test-integration.sh，已在树内）：B5-1 段 :318 后追加一条 `run_migrated_integration` 条目——`run_migrated_integration "$AUTH_OUTBOX_PARITY_INTEGRATION_DB" "auth_outbox_parity" "auth outbox parity 1:1"`（0239 文件门控块内）。空过滤器守卫（`test result: ok. [1-9]* passed` 判 FAIL）对新条目生效——测试未落地则条目红。b5-pin.sh 37 槽逐字不动（§3.4 实测：新增 verdict 行不触发守卫失败）。

## 7. Decision points / risks

- **D1（token 映射归属）——维持 req 决议**：auth 显式写路不触碰 `governance_lane_for`（aero-ai 切片 + campaign 决策）；token 表（sibling §2.7）为 aero-storage 常量，漂移由 AC2 逐字段断言兜底。
- **D2（L1 与 pin 冲突）——定稿结局 A**：聚合行 class='message'；`admin_class_rows_never_aggregated` 逐字不动（其断言对象 = admin 行永不进 message 窗；聚合行自身是 message class，无冲突）。结局 B（修订 pin）拒绝——避免动 aero-ai 已钉测试。
- **D3（login/refresh 的「事务」定义）——维持**：独立事务对；rollback 证明 = 对自身回滚（非域行）。
- **D4（fail-open 边界）——机制定稿 = SAVEPOINT + `is_fail_open_error` 分类 + DLQ**：`sqlx::Error::Database` → `ROLLBACK TO SAVEPOINT` + **同 tx DLQ**（D9）+ 分类日志/强制计数（D10，§2.8）→ `Ok(None)`；其余（IO/Protocol/连接）→ in-tx 传播 / standalone warn+`None`（D12）。这是 R1 与 R7 唯一自洽的实现（域行提交 + 审计对消失 + 输入可重放）。若不做 savepoint，in-tx 审计失败会翻转 auth 结果（违反 R7）；若 pool 级写，则无原子对（违反 R1）。**F2 注入勘误**：非 object `detail` 不触发任何 DB 错误（`audit_events.detail` 无 CHECK、信封恒 object）→ 契约预检改在 Rust 入口（确定性 `Ok(None)` + 计数 `category="contract"`）；真 DB 错误注入 = enforcement-on-no-binding（0236 P0001）或坏 workspace FK（23503）。
- **D5（login 审计写点）——handler 侧**（`finalize_login(true)` 之后）：`AuthService::login` 看不到 2FA 终局，写在那里会把「密码对但 2FA 错」记成成功登录。
- **D6（session.revoked token 连续性）——沿用既有 token**；admin 侧新增 `session.revoked.admin`；出站 token 一律 `admin.auth.session.revoke`。
- **D7（聚合行无 audit_events 行）——‡ 豁免定稿**：1:1 豁免 + 避免 0236 v1 触发副作用 + 避免 default-workspace 审计视图污染；合成 event_id 确定性幂等。副作用：GDPR `GET /api/me/export` 不含登录失败——`login_failures` 已独立留存（既有行为，未改变）。
- **D8（0242/0243 编号定稿，本设计新钉——替代条件式处置）**：**本切片无条件取 0243 + 0244**（`migrations/0243_login_failures_created_at_idx.sql` + `migrations/0244_audit_governance_failed_pairs.sql`），sibling aero-ai 保留 0242。依据（复核 sibling design §2.4 后的定稿）：两文件同在**一个未提交工作树**，「renumber unless already taken」在单工作树下是伪条件——要么永不触发而把重复版本硬失败留给 sibling（其文档零让号语言），要么按落地先后而非设计决策定号；固定分配消除该竞态，且本设计已携带全部顺延机制（D8 本文、F12、§5.2/5.3），sibling 逐字引用其 0242 无需改动。索引（行为中性）让号，触发器保住号位。落地检查见 §5.2。
- **D9（F1/F3 补偿机制定稿，本设计新钉）——组合方案**：**(i) 库层不变量**（D13：`configure_enabled` 覆盖检查与开关翻转同事务——enforcement on ⇒ 每 workspace 含 nil 有 enabled binding，经受支持路径不可达）+ **强制 ERROR 日志 + 强制计数器**（D10）+ **(ii) 同 tx DLQ `audit_governance_failed_pairs`（0244，原始输入 + `error_sqlstate`，可重放）**。否决「仅 (i) + 计数」（复核者允许的 defer 路径）：确定性类虽库层不可达，但残余面（手动 SQL、未来新增开关写者）与罕见 bug 类（CHECK/FK/唯一）仍会静默丢事件——审计轨迹的补偿应是「延迟 + 可重放」而非「日志 + 计数」，且 DLQ 成本低（1 表 + 1 插入 + 重放方法，无消费方也零负担）。否决「仅 DLQ」：确定性类应在 boot 即 fail-loud 显性化（已存在，F14），DLQ 增长失控时无预警面。**组合** = 预防（不变量 + boot fail-loud）+ 补偿（DLQ）+ 可见性（ERROR/warn + 计数 + 深度 gauge）。
- **D10（可观测契约，本设计新钉）——全部强制**：计数器 `audit_auth_write_failures_total{category}`（category ∈ {contract, binding, database}；**SQLSTATE 为分类键**、日志行必带 `error_sqlstate`）；DLQ 深度 gauge `aero_audit_governance_failed_pairs`（`metrics_tasks.rs` 30s sampler）；日志级别按类：P0001(binding) 与契约 bug 类（23514/23503/23505/other）→ **error**，瞬时类（40P01/40001/55P03）→ warn，契约预检（contract）→ warn，连接级 → in-tx 传播 / standalone warn（不计数）。两 metric 注册进 `aero-common` `names` + `register_known_metrics`；计数发点在 aero-storage 共享写路径（storage 首个 metric，deliberate 例外——SQLSTATE 分类只存在于写路径内，调用点分发会复制分类逻辑）。详情 §2.8。
- **D11（F5，L1 保留交互）——接受并记录**：`AERO__SERVER__LOGIN_FAILURE_L1_AGGREGATE_SECS=0` 时 v2 失败信号在底座行过保留（默认 180d）后永久丢失；**保留窗口内重开定时器即自愈**（pull 全扫描自带 backfill，无需 0241 式 reconciler——聚合函数本身就是 backfill）；超窗口不可恢复且保留清扫是 PII/GDPR 特性。boot 一次 warn 明示（§2.6/F15）。否决独立 backfill 机制（重复代码）。
- **D12（F4，`record_pair_standalone` 签名）——swallow-all `Option<AuditId>`**：删除 `Result`（原文档「任何失败 → Ok(None)」与「Err 仅连接级」自相矛盾——两个调用点都 `let _ =`，Err 无消费者）；R7 由类型强制（调用方拿不到 Err 无法传播）；连接级内部 warn。`Err` 分支只留在 `append_pair_in_tx_fail_open`（域事务需传播）。配套：§4 F4 行「审计行保留」勘误为「整对消失」（23505 → SAVEPOINT 回滚整对，保留单行违反 R1/P2）。
- **D13（共享表/车道所有权 + F1 不变量锚点，本设计新钉——三个复核共同盲区的闭合）**：
  - **`login_failures`（0140）**：唯一写者 = `LoginFailureRepo::record`（`login_failure.rs:63`）；本切片新增**只读**消费者 `aggregate_login_failure_buckets` + 0243 索引（顺带服务 retention 清扫）；删除权属 retention 定时器（`AERO__SERVER__LOGIN_FAILURE_RETENTION_DAYS`）。sibling 切片不触此表。**L1 永不自删底座行**（F7 forensic 语义）。
  - **`audit_governance_outbox` 四类写者**：0239 触发器（moderation，admin/100）· 0241 reconciler（moderation 回填，token 白名单 = `message.moderated` 仅此）· sibling 0242 触发器（message L1 聚合，md5 event_id）· 本切片仓储（auth 1:1 对，ULID event_id + login-failure L1，v5 event_id）。**event_id 命名空间分区**：ULID（audit id）/ md5(`{ws}|message|{epoch}`)/ v5(`aero.im.audit.l1:{ws}:{action}:{bucket_epoch}`)/ `gen_random_uuid()`（sibling spill）互不复用——新合成 scheme 是跨切片设计变更，须入两份 design。**车道值**（class/priority）受 0239 CHECK 钉死：admin/100=moderation 独有、admin/10=auth 对、message/10=两切片 L1 共用（claim `ORDER BY priority DESC, available_at, created_at, event_id` 下按 created_at 交错 FIFO，有意为之，无按切片过滤）；新车道值必须走迁移。**0241 白名单属 moderation 车道**，两设计均明言不得回填 message 类（sibling F8）；本切片 L1 是定时器驱动，不扩 reconciler。**relay/webhook 车道**：audit_governance_outbox 的唯一消费者是 aero-audit-connector relay（两切片零改动，新 payload 形状 `admin.auth.*`/`message.activity` 原样出仓）；NATS `im.room.*` webhook dispatcher（`ConsumerEventReceiptRepo`）是另一车道，勿混。**v1 共存**：0236 对两切片新增 audit 行照常 1:1 产 v1 行（volume 增长见 sibling risk ⑤）；L1 行无 audit 行、天然不进 v1。
  - **F1 不变量锚点（nil-workspace binding 种子与保证）**：**没有任何迁移/代码自动种子 nil binding**——bindings 仅由商业控制平面 desired-state 激活创建（`SnaplinkCommercialRuntime::from_env` → `SnaplinkCommercialRepo::configure_enabled`，`snaplink_commercial.rs:175`），种子者 = 运维的 Snaplink 商业配置须含 `workspace_id = 00000000-0000-0000-0000-000000000000` 的 binding（0006:61 的 nil 行是真实 `workspaces` 行，`require_complete_workspace_coverage` 会数到它；config 解析器不特判 nil uuid）。**「enforcement on ⇒ nil binding」是库层强制不变量，非 ops 期望**：`configure_enabled` 在同一事务内 `require_complete_workspace_coverage`（`snaplink_commercial.rs:453`，所有 `workspaces` 行必须有 enabled binding，否则整个开关翻转回滚）+ `reject_omitted_enabled_bindings`（:434，存量 enabled binding 必须出现在 desired 中）→ 缺 nil binding 时 boot **fail-closed 大声失败**（`main.rs:51-56`）；生产写开关仅这两个 repo 方法（rg 实证，无 HTTP 端点）。残余面：手动 SQL 直改开关（防不了，任何不变量都防不了）+ 测试 fixture 直翻（绕开检查——F1 回归测试正是经此构造不可达态验证 fail-open 分类）。设计文档言明：auth 对在 enforcement on 下落在 nil（账户级 login/refresh/PAT/TOTP/session）或注册/目标 workspace（register/admin revoke）——覆盖检查保证两者都有 enabled binding → 0236 v1 触发总成功；`audit_auth_write_failures_total` 的 P0001 类计数器见 F1。
- **Risks**：① 前置批次未提交则 `git reset --hard` 校准丢地基（§5.1 硬前置，实测 94 项在途）；② `NewRegistration` 加字段 = 3 构造点（编译强制，低风险）；③ 0243/0244 两迁移必须 build→migrate 顺序（§5.3），且落地前确认 sibling 0242 已落或未落（`grep -cE '^(0242|0243|0244)_'` ≤1，§5.2）；④ handler 侧审计点若被后续重构移到 `AuthService::login` 内会误记 2FA 失败——AC1 login 测试 + D5 注释钉死；⑤ auth 行 priority=10 与 moderation 100 同表——claim 排序由 0240 索引 + `priority DESC` 保证（auth 排后，安全事件不抢占审核车道）；⑥ `audit_governance::` harness 条目已 un-gated——新 db_tests 未落地时该条目仍绿（过滤器匹配既有 6 测试），因此 AC1/AC3 的**命名测试**必须各自有断言密度，靠新命名条目 `auth_outbox_parity` 的 empty-filter 守卫兜底注册类测试。

## 8. Sequencing

1. **前置**：提交上一批次 untracked 物（0239/0240/0241、`governance.rs`、`model/audit.rs`、`audit_governance.rs`、`aero-audit-connector/`、test-integration.sh、b5-pin.sh、test-b5-pin-guard.sh）。
2. **迁移 0243 + 0244**（编号定稿，§7 D8）：`ls migrations/*.sql | grep -cE '^(0242|0243|0244)_'` ≤1 → 写 `migrations/0243_login_failures_created_at_idx.sql`（文件头注释写明原计划 0242 与让号理由）+ `migrations/0244_audit_governance_failed_pairs.sql`（DLQ 表）→ `cargo build` → `aero-cli migrate`。
3. **aero-storage**：H3 仓储 + envelope + fail-open 对（契约预检 + DLQ 入队 + 分类日志/计数）+ standalone 对（`Option<AuditId>`）+ `FailedPairRepo`（enqueue/replay/count）+ `is_fail_open_error` + `SnaplinkCommercialRepo::has_enabled_binding` + L1 聚合 + `_in_tx` 变体（pat/totp/auth_session）+ `NewRegistration.auth_audit` + db_tests（AC1–AC3 + F1/F3 组 + G1 单测全部落此，parity 名/字面量不动）。
4. **aero-auth**：`register_enrolled` 审计 spec（`Some` 构造点）+ `refresh` 写点（`Option<AuditId>` fail-open 包装）。
5. **aero-server**：login handler 审计点（`finalize_login(true)` 后）；pat/twofa/sessions/session 路由 tx 编排（删 pool 级事后审计）；admin revoke 存储侧嵌对；boot timer（`AERO__SERVER__LOGIN_FAILURE_L1_AGGREGATE_SECS`）+ 禁用时一次 warn（D11/F15）；`metrics_tasks.rs` DLQ 深度 gauge sampler + aero-common 两新 metric 注册（D10/§2.8）；test-integration.sh 追加 `auth_outbox_parity` 条目（:318 后）。
6. **门禁**：`cargo check --workspace` / `cargo test --workspace --lib`（基线 81 保持；PG 门控 `-- --ignored` + 一次性库）/ clippy 无新增警告 / `scripts/{truth-check,file-size-check,web-check}.sh`；`test-integration.sh` B5 段全 PASS（AC4 un-gated 无 SKIP，新命名条目过 empty-filter 守卫）；`scripts/test-b5-pin-guard.sh` 绿（37 槽逐字不动）。

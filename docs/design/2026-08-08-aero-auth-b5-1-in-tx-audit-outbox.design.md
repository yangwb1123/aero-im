# Design — aero-auth B5-1：auth 域 in-tx 审计 outbox 写入（status 0/1/2/3、class admin、L1 聚合）

- **Direction**: "B5-1: In-tx audit outbox writes (status 0/1/2/3, class admin) for auth-domain operations — register/login/refresh/PAT-issue/PAT-revoke/2FA — with L1 aggregation for high-volume events"（value 6 / risk_reduction 6 / effort 6 / confidence 5）
- **Requirements**: `docs/requirements/2026-08-08-aero-auth-b5-1-in-tx-audit-outbox.req.md`（R1–R8, AC1–AC5）
- **Status**: Design（证据全部逐条复核；四处证据勘误见 §0；两处 req 细化见 §7 D2/D5）
- **Verification date**: 2026-08-08。行号为复核锚点，会漂移——**文件/符号**为准（AGENTS.md §0）

## 0. Evidence adjudication（untrusted claims → verified anchors）

| Evidence claim | Verdict | Verified anchor（实际） |
|---|---|---|
| aero-auth zero audit/outbox hits | ✅ 精确 | `rg -i "audit\|outbox" crates/aero-auth/src/` 零命中 |
| `service.rs:184 register / :260 login / :334 refresh` + `register_enrolled` :221 + `login_inner` :312 | ✅ 精确（login_inner 实为 :307） | `crates/aero-auth/src/service.rs`：`register` :184、`register_enrolled` :221、`login` :260、`login_inner` :307、`refresh` :334 |
| `audit.rs:127 append_in_tx / :51 new / :64 sweep`，1049 行 | ✅ 精确（符号 = `sweep_before`） | `crates/aero-storage/src/audit.rs`：`new` :51、`sweep_before` :64、`append` :113、`append_in_tx(tx,…) -> Result<AuditId>` :127、`append_on` :140（generic executor，事务内 INSERT audit_events 返回新 ULID `AuditId`） |
| `im-core/outbox.rs:27-45` EventOutboxRepo | ⚠️ 部分（实体位置漂移） | `crates/aero-im-core/src/service/outbox.rs`（631 行）:27-45 是 `dispatch_event_outbox_batch`（`impl ImService`）；**`EventOutboxRepo` 结构体实为 `crates/aero-storage/src/event_outbox.rs:166`**。实质成立：RoomEvent outbox 与审计 outbox 是两条链路 |
| `0162_event_outbox.sql`、`0236 :68-130` 触发器、`0007_audit.sql` | ✅ 精确 | `0162` = 39 行（event_outbox 表 + pending/published 部分索引）；`0236`（303 行）`aero_enqueue_snaplink_audit()` :68、AFTER INSERT 触发器 :129-133（唯一门 = `snaplink_commercial_runtime.enabled` :78-86）；`0007` = 22 行（audit_events，`workspace_id NOT NULL REFERENCES workspaces`） |
| `proposals/audit-contract-batch-aero-im.md:8` | ✅ 精确 | :8 = B5-1 逐项设计行（0239 DDL status 0/1/2/3、class message/room/admin、P2 parity event_id 1:1） |
| 「migrations 止于 0238，0239 未落」 | ❌ **SUPERSEDED** | migrations 现止于 **0241**，0239/0240/0241 全在 untracked 工作树；`audit_governance.rs`（933 行）、`aero-audit-connector/`、`governance.rs`（211 行）同在。drill SKIP 门已 un-gated（relay drill :57-64 `to_regclass('audit_governance_outbox')` → 缺席 exit 2） |
| 0239 触发器 token-keyed（仅 `message.moderated`） | ✅ 精确（新事实成立） | `migrations/0239_audit_governance_outbox.sql`（140 行）：`aero_enqueue_governance_audit()` 仅 `NEW.action = 'message.moderated'` 入 outbox（class 'admin'、priority 100、status 0、完整 v2 信封 + `idempotency_key = event_id::text`、`ON CONFLICT DO NOTHING`），未知 token `RETURN NEW` fail-open。**auth token 必须显式 in-tx 写** |
| `AuditGovernanceOutboxRepo` 尚不存在，H3 手记落同模块 | ✅ 精确 | `audit_governance.rs:1-30` 模块 doc 明言："The B5-1 storage direction's `AuditGovernanceOutboxRepo` lands in a sibling slice (handoff H3…); **add the repo to this same module when it lands**"。既有 db_tests：`moderation_finalize_outbox_parity` :247、`ddl_contract_defaults_and_checks` :424、`moderation_finalize_runtime_disabled_commits_1_plus_0` :583、`non_moderation_action_passes_through_unmapped` :628、`duplicate_event_id_is_deduped_by_on_conflict` :693 |
| `record_login_event` scopes 账户级审计到 `DEFAULT_WORKSPACE_ID` | ✅ 精确 | `routes/handlers/auth.rs`：`record_login_event` :35（`auth.login.new_ip` :59）、调用点 :208-213 `Some(DEFAULT_WORKSPACE_ID)` 注释明言 "the all-zero default is where account-level security events land"；register 路由 :86 `register_enrolled(req, DEFAULT_WORKSPACE_ID, …)`；nil 种子行 `0006_workspaces.sql:61` |
| `login_failures` 表（0140）为 L1 底座 | ✅ 精确（record 行号漂移） | `migrations/0140_login_failures.sql`（account/ip/user_agent/created_at + `(account, created_at DESC)`、`(ip, created_at DESC)` 双索引）；`LoginFailureRepo::record` 实为 `crates/aero-storage/src/login_failure.rs:63`（evidence 记 :70） |
| **「`session.revoked` 已在 admin 侧 in-tx」** | ❌ **错误** | 无 `auth_session/admin_revoke.rs` 文件。全仓 `session.revoked` 仅三处，**全部 pool 级 best-effort 事后 append（非 in-tx）**：`session.rs:212/220-236`（logout）、`sessions.rs:108/118-135`（用户侧会话清单吊销，target=session id）。`admin_sessions.rs`（165 行）**零 audit 引用**（域变更在 `auth_session.rs` 事务内 :172-206/:291-347/:370-423，但无审计）。req 文档 E13⑦ 沿袭了此错误——本设计 §2.5 修正 |
| 「用户侧 sessions.rs 路由无 audit」 | ❌ **错误**（与上条同源） | `sessions.rs:108` 已写 best-effort `session.revoked`（target = session id）。本设计将其**升级**为 in-tx 对，token 不变（兼容既有审计轨迹消费方） |
| `run_migrated_integration` 硬编码 `-p aero-storage` | ✅ 精确 | `scripts/test-integration.sh:169-202`：`cargo test -p aero-storage --lib --locked "$test_name" -- --ignored --test-threads=1` + empty-filter 守卫（"running 0 tests" 判 FAIL）；B5 条目 :306-318 已 un-gated（0239 文件存在） |
| `admin_class_rows_never_aggregated`（governance.rs:205） | ✅ 精确 | `crates/aero-ai/src/governance.rs:200-211`：admin class（`message.moderated`）绝不进 L1 `message.*` 聚合窗；`message.create`/`room.create` 非 admin。常量 :31/33/37/39/41/49/56；`governance_lane_for` :86-94 唯一映射 |
| connector 状态机常量 + drill | ✅ 精确 | `crates/aero-audit-connector/src/pg.rs:27-30` `STATUS_ENQUEUED=0/CLAIMED=1/DELIVERED=2/DEAD=3`；`outbox.rs:51` `OutboxRepo` trait、`claim_due` :72；relay drill `to_regclass` SKIP（exit 2）:57-64 |
| `RegistrationRepo::create` 单事务 | ✅ 精确 | `crates/aero-storage/src/registration.rs:37` create、begin :38、commit :109——participants + credentials + workspace_members + 默认频道 + auth_sessions 一个 PG 事务。**register 的审计对必须骑此事务** |
| PAT pool 级无事务 | ✅ 精确 | `crates/aero-storage/src/pat.rs`：`create` :104、`revoke` :190 均为 pool 单语句 |
| TOTP 变更 pool 级 | ✅ 精确 | `crates/aero-storage/src/totp.rs`：`upsert_secret` :54、`activate` :124、`disable` :141 单语句 |
| `session.revoked` 用户侧既有 best-effort + `auth.login.recovery_code` 轨迹 | ✅ 补充确认 | 见上；recovery-code 登录额外写 `auth.login.recovery_code`（handlers/auth.rs:215-227，pool 级 best-effort） |
| 基线 `cargo test -p aero-auth --lib` = 81 passed | ✅ 实测 | 本次复核现场运行：`test result: ok. 81 passed; 0 failed` |
| login 2FA 门在 handler 侧 | ✅ 关键新事实 | `handlers/auth.rs`：`s.auth.login()` → 2FA 门（:150-199，失败 `finalize_login(false)` + `LoginFailureRepo::record`）→ `finalize_login(true)` → `record_session` → `record_login_event`。**`AuthService::login` 看不到 2FA 门——`auth.login` 审计必须写在 handler 侧**（§2.3 偏离 req §3 的「AuthService 写入」表述，理由见 §7 D5） |
| root uuid 已启用 v5 | ✅ 补充确认 | root `Cargo.toml:110` `uuid = { version = "=1.18.1", features = ["v4","v5","v7","serde"] }` ——L1 合成 event_id 零依赖改动 |
| 0241 reconciler 语义 | ✅ 补充确认 | `0241_governance_reconcile.sql`：token-keyed 扫描（仅 `message.moderated` 回填）、event_id PK dedup、INSERT 与 0239 触发器逐字节同构、dead 行不复活。**auth 显式写路与之互不相扰**（触发器不回填非 moderation action） |
| `NewRegistration` 构造点 | ✅ 补充确认 | 3 处：`registration.rs:156/:257`（db_tests）、`aero-auth/src/service.rs:233`（register_enrolled）——加字段 = 3 处更新 |

**勘误/细化汇总**：① **`session.revoked` in-tx 为假**（三处全部 best-effort pool 级；admin 侧零审计）——本设计把「补 outbox 行」改为「三处全做 in-tx 对，admin 侧新增」；② `EventOutboxRepo` 实体在 aero-storage `event_outbox.rs:166`；③ `login_failure.rs` record 实为 :63；④ `run_migrated_integration` 实为 :169-202（B5-4 设计文档记 :153-165，漂移）。其余 20+ 条全部精确命中。

## 1. Design overview

```
                      ┌────────────── aero-storage（全部 in-tx 机制落家，harness 零参数化）──────────────┐
                      │  audit_governance.rs（H3 落点）                                                  │
                      │    AuditGovernanceOutboxRepo::append_in_tx(tx,event_id,class,prio,payload)      │
                      │    governance_envelope(...)   ← 0239 触发器信封的 Rust 镜像（db_test 逐字段钉死） │
                      │    append_pair_in_tx_fail_open(tx,…) ← SAVEPOINT 语义（R1 原子对 + R7 fail-open）│
                      │    record_pair_standalone(pool,…)  ← login/refresh 独立事务对                   │
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
  L1:        boot timer（env 门控）→ aggregate_login_failure_buckets → login_failures 窗口 → 聚合 outbox 行
             （class 'message'、payload.count=N、无 audit_events 行 = ‡ 类 1:1 豁免）
  relay:     aero-audit-connector 零改动——payload 原样转发，sink Idempotency-Key = event_id
```

**三条硬线**：① 所有 in-tx 机制在 aero-storage（`run_migrated_integration` 硬编码 `-p aero-storage` 的 harness 约束 + H3 手记落点），aero-auth / aero-server 只做调用与编排；② 审计对 = 恰 1 行 `audit_events` + 恰 1 行 `audit_governance_outbox`（`event_id = audit_events.id` 1:1，P2 parity），**审计写失败经 SAVEPOINT 整体消失（R7「整对没写」），绝不半对**；③ 不新增表、不改 0239/0240/0241、不改 `governance.rs` 映射权威。

## 2. API changes（逐 crate 签名）

### 2.1 `crates/aero-storage/src/audit_governance.rs`（H3 手记落点，新代码 + db_tests 同模块共存）

```rust
/// H3 交付：v2 出站仓储。class/priority 入参由调用方给（auth 固定 'admin'/10）。
impl AuditGovernanceOutboxRepo {
    pub fn new(pool: PgPool) -> Self;

    /// 事务内写 outbox 行（镜像 AuditRepo::append_in_tx 形态）。event_id = audit_events.id。
    /// 0239 已内置 ON CONFLICT (event_id) DO NOTHING → 重放幂等。
    pub async fn append_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        event_id: AuditId,
        class: &str,
        priority: i16,
        payload: serde_json::Value,   // 必含 idempotency_key（CHECK jsonb_typeof='object'）
    ) -> Result<(), sqlx::Error>;
}

/// 0239 触发器信封（schema_id 'aero.im.security' / aggregate_type 'workspace' / actor /
/// targets / data_classification 'confidential' / retention_class 'security' /
/// idempotency_key=event_id::text / source_system）的 Rust 镜像。source_system 参数化
/// （auth 显式写 = "aero-auth"；moderation 触发器 = binding.source_system）。db_test
/// 与 moderation parity 测试同形逐字段断言——这是两条入队路（SQL 触发器 vs Rust 显式写）
/// 的漂移守卫。
pub fn governance_envelope(
    event_id: AuditId,
    workspace: WorkspaceId,
    actor: Option<ParticipantId>,
    target: Option<&str>,
    detail: serde_json::Value,
    outbound_action: &str,      // 'admin.auth.*'（§2.7 表）
    outcome: &str,              // 'success'（auth 成功事件）| 'failure'（L1 聚合行）
    occurred_at: time::OffsetDateTime,
    source_system: &str,        // auth = "aero-auth"
) -> serde_json::Value;

/// R1 + R7 的唯一实现点：SAVEPOINT 包裹 audit+outbox 两行。
/// 成功 → 两行随调用方事务提交（原子对）；审计写入失败（CHECK 违例等）→
/// ROLLBACK TO SAVEPOINT，两行俱无，返回 Ok(None)（fail-open，R7「整对没写」）；
/// 仅连接级错误向上传播（事务本身已不可用）。
/// sqlx 无 first-class savepoint：RAW SQL `SAVEPOINT aero_audit_pair` /
/// `ROLLBACK TO SAVEPOINT aero_audit_pair`（成功路径 RELEASE）。
pub async fn append_pair_in_tx_fail_open(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    actor: Option<ParticipantId>,
    action: &str,               // 本地 token（§2.7 表）
    target: Option<&str>,
    detail: serde_json::Value,
    outbound_action: &str,      // 出站 token（§2.7 表）
) -> Result<Option<AuditId>, sqlx::Error>;   // Ok(None) = 审计被 fail-open 跳过；class 'admin'、priority 10 常量内置

/// login/refresh 用：无域事务的独立事务对（begin → append_in_tx(audit) →
/// append_in_tx(outbox) → commit），失败 warn + Ok(None)（fail-open，不翻 auth 结果）。
impl AuditGovernanceOutboxRepo {
    pub async fn record_pair_standalone(
        &self,
        workspace: WorkspaceId,
        actor: Option<ParticipantId>,
        action: &str,
        target: Option<&str>,
        detail: serde_json::Value,
        outbound_action: &str,
    ) -> Result<Option<AuditId>, sqlx::Error>;
}

/// L1 聚合（§2.6）。返回本次新建行数；Err 仅连接级。
impl AuditGovernanceOutboxRepo {
    pub async fn aggregate_login_failure_buckets(&self, window_secs: i64) -> Result<usize, sqlx::Error>;
}
```

lib.rs：`pub mod audit_governance;` 已存在（模块在树）——re-export 遵循 §4.2（无 generate_token 撞名风险；`AuditGovernanceOutboxRepo` 经 `pub use` 或子路径均可，照 `AuditRepo` 惯例）。

### 2.2 `crates/aero-storage/src/registration.rs`

```rust
pub struct NewRegistrationAudit {
    pub action: &'static str,          // "auth.register"
    pub target: String,                // participant_id::text
    pub detail: serde_json::Value,     // json!({})（最小化，无邮箱明文）
    pub outbound_action: &'static str, // "admin.auth.register"
}
pub struct NewRegistration { /* 既有字段 */ , pub auth_audit: Option<NewRegistrationAudit> }
```

`create()` 在既有事务内（domain 行之后、`tx.commit()` 之前）：
`append_pair_in_tx_fail_open(&mut tx, workspace_id, Some(participant_id), audit.action, Some(&audit.target), audit.detail, audit.outbound_action).await?` ——**Ok(None) 静默继续（R7），Err 传播（连接级）**。3 处构造点更新（`registration.rs:156/:257` db_tests 传 `None`、`service.rs:233` 传 `Some`）。

### 2.3 `crates/aero-auth/src/service.rs`

- `register_enrolled` :221：构造 `NewRegistration { …, auth_audit: Some(NewRegistrationAudit{…}) }`。**公开签名不变**。
- `refresh` :334：成功后 `AuditGovernanceOutboxRepo::new(self.repo.pool().clone()).record_pair_standalone(DEFAULT_WORKSPACE_ID, Some(pid), "auth.refresh", Some(session_id.to_string()), json!({}), "admin.auth.refresh").await`，**Err 仅 warn 不传播**（fail-open）。`DEFAULT_WORKSPACE_ID` 经 `aero_common::WorkspaceId::from_uuid(uuid::Uuid::nil())`（aero-auth 无常量，照 session.rs:221 写法）。
- `register` :184（无 workspace 的库级路径）与 `login` :260：**不改**。`login` 的审计写点在 handler 侧（2FA 门后，§2.4）——`AuthService::login` 无法得知 2FA 终局，写在这里会把「密码对但 2FA 错」的尝试记成成功（§7 D5）。`finalize_login` 语义不变。

### 2.4 `crates/aero-server/src/routes/handlers/auth.rs`（login 成功审计点）

`finalize_login(&email, true)` 之后、`record_login_event` 旁（:208 区）：

```rust
let _ = AuditGovernanceOutboxRepo::new(s.pg.clone())
    .record_pair_standalone(
        DEFAULT_WORKSPACE_ID, Some(out.participant.id),
        "auth.login", Some(out.participant.id.to_string()),
        serde_json::json!({}), "admin.auth.login")
    .await; // 内部已 fail-open
```

既有 `auth.login.new_ip` / `auth.login.recovery_code` best-effort 路径**原样保留**（不同 action，不冲突，不得改成 in-tx——其语义既定）。

### 2.5 `crates/aero-server/src/{pat.rs, twofa.rs, sessions.rs, session.rs, admin_sessions.rs}` + storage 侧 tx 变体

路由统一编排模式（begin → 域语句 → 审计对 → commit；审计对失败 = savepoint 跳过，域照常提交）：

| 路由 | 域语句 tx 变体（storage） | 本地 token / 出站 token | target / detail |
|---|---|---|---|
| `POST /api/pat`（pat.rs:132） | `PatRepo::create_in_tx(tx, participant, token_hash, name, scopes, expires_at) -> Result<PatId>` | `auth.pat.issue` / `admin.auth.pat.issue` | pat_id / `{"scopes":[…]}`（不含 name——用户自由文本不进审计） |
| `DELETE /api/pat/:id`（pat.rs:172） | `PatRepo::revoke_in_tx(tx, id, participant) -> Result<bool>` | `auth.pat.revoke` / `admin.auth.pat.revoke` | pat_id / `{}` |
| `POST /api/me/2fa/enroll`（twofa.rs:36，`upsert_secret` :119） | `TotpRepo::upsert_secret_in_tx(tx, …)` | `auth.totp.enroll` / `admin.auth.totp.enroll` | participant_id / `{"stage":"enroll"}` |
| `POST /api/me/2fa/verify`（twofa.rs:134，`activate` :152；激活 pending enrollment） | `TotpRepo::activate_in_tx(tx, participant)` | `auth.totp.enroll` / `admin.auth.totp.enroll` | participant_id / `{"stage":"activate"}` |
| `DELETE /api/auth/sessions/:sid`（sessions.rs:46-48，handler :91，`revoke_and_blacklist` :95） | `SessionRepo::revoke_and_blacklist_in_tx(tx, id, participant)`（auth_session.rs:499 CTE 的 executor 泛化） | **`session.revoked`（沿用既有 token，兼容消费方）** / `admin.auth.session.revoke` | session_id / `{"session_id":…}`（沿用现有 detail 形状） |
| logout（`POST /api/auth/logout` session.rs:40，审计调用点 :212 区） | 同上（按 token_hash 的吊销变体 `_in_tx`） | `session.revoked` / `admin.auth.session.revoke` | session_id / `{}` |
| admin force-revoke（admin_sessions.rs → auth_session.rs:600/:622） | **在既有事务内嵌审计对**（路由零改动） | `session.revoked.admin`（新增 token） / `admin.auth.session.revoke` | 被吊销 participant_id / `{"admin_revoked":true}` |

替换后删除 sessions.rs / session.rs 的 pool 级事后 `audit_session_revoked`（被 in-tx 对取代；action 字符串不变 → 审计轨迹连续）。admin_sessions.rs 是**新增**审计（现状零审计，证据勘误①）。

`_in_tx` 变体实现 = 镜像 `AuditRepo::append_on` 的 generic-executor 模式（`sqlx::query(...).execute(executor)`），不改既有 pool 方法（`create`/`revoke`/`revoke_and_blacklist` 保持——verify 等读路径无事务需求）。

### 2.6 L1 聚合（login 失败，D2 结局 A）

- **底座**：`login_failures`（0140，逐尝试已持久化）。
- **聚合函数**（§2.1 `aggregate_login_failure_buckets`）：按 `date_trunc('second', created_at) - (epoch % window)` 分桶；仅聚合**已关闭桶**（桶尾 ≤ now − window）；每桶恰 1 行 outbox：`event_id = Uuid::new_v5(NAMESPACE_URL, "aero.im.audit.l1:{workspace}:{action}:{bucket_start_epoch}")`（确定性 → 重跑/并发 `ON CONFLICT DO NOTHING` 幂等）、`status 0`、**`class 'message'`**（D2 结局 A——不触 `admin_class_rows_never_aggregated`，聚合行自身就是 message 域）、`priority 10`、payload = `governance_envelope(…, action "auth.login.failure", outbound "admin.auth.login.failure", outcome "failure", detail {"count":N,"window_start_epoch":…,"window_secs":…})`，**envelope 顶层带 `"aggregated": true`**（`payload->>'aggregated'='true'`——AC2 parity 跨切片共享豁免键，与 aero-ai 0242 聚合行同一表达式；**不得**下沉进 detail，编号/键定稿见 connector-root design §2.6/§6 AC2/§7 D13）。
- **无 audit_events 行**：‡ 类豁免 1:1；同时避免 0236 v1 触发器副作用与 default-workspace 审计视图污染（聚合行对 `list_for_workspace` 不可见，语义正确）。合成 event_id 不引用任何 audit 行——`audit_governance_outbox.event_id` 无 FK，合法。
- **定时器**：`bin/boot/background.rs` 新 ticker，env `AERO__SERVER__LOGIN_FAILURE_L1_AGGREGATE_SECS`（默认 30，0 禁），`MissedTickBehavior::Skip`，best-effort warn（§2 定时器惯例，持共享 cancel token）。
- **索引**：扫描按 created_at 分桶，现有 `(account, created_at DESC)`/`(ip, created_at DESC)` 不领 created_at 单列 → 新迁移 **0243**（编号定稿：connector-root design §7 D8——本切片无条件取 0243，sibling aero-ai 保留 0242）：`CREATE INDEX IF NOT EXISTS login_failures_created_at_idx ON login_failures (created_at)`（幂等、加列式；唯一新增迁移，见 §5）。

### 2.7 auth 事件 token 表（R3 [PROPOSED] → 本设计定稿）

| 操作 | 本地 token（audit_events.action） | 出站 token（payload.action） | class | priority | workspace |
|---|---|---|---|---|---|
| 注册成功（register_enrolled） | `auth.register` | `admin.auth.register` | admin | 10 | 注册 workspace |
| 登录成功（handler 侧，2FA 全过） | `auth.login` | `admin.auth.login` | admin | 10 | DEFAULT |
| 刷新成功 | `auth.refresh` | `admin.auth.refresh` | admin | 10 | DEFAULT |
| PAT 签发 | `auth.pat.issue` | `admin.auth.pat.issue` | admin | 10 | DEFAULT |
| PAT 吊销 | `auth.pat.revoke` | `admin.auth.pat.revoke` | admin | 10 | DEFAULT |
| TOTP 登记/激活 | `auth.totp.enroll` | `admin.auth.totp.enroll` | admin | 10 | DEFAULT |
| session 吊销（用户侧） | `session.revoked`（**沿用既有**） | `admin.auth.session.revoke` | admin | 10 | DEFAULT |
| session 吊销（admin 侧） | `session.revoked.admin` | `admin.auth.session.revoke` | admin | 10 | DEFAULT |
| 登录失败（L1 聚合行） | —（无 audit 行） | `admin.auth.login.failure` | **message** | 10 | DEFAULT |

**修正点**：R3 表原提案 `auth.session.revoke` 改为沿用既有 `session.revoked`（审计轨迹兼容）；登录失败 class 从 [PROPOSED] 定稿为 message（D2 结局 A，§7）。

## 3. Compatibility constraints

1. **零新表、零 DDL 改动**：0239/0240/0241 逐字不动（token-keyed 触发器、CHECK、默认值、索引）；`governance.rs` 映射权威不动（auth 走显式写路，不扩触发器 token 集——D1 拒绝范围外实现）。
2. **唯一新迁移 0243**（编号定稿：connector-root design §7 D8）：`login_failures_created_at_idx`（IF NOT EXISTS，纯加索引）。**加迁移后必先 `cargo build` 再 `aero-cli migrate`**（§4.2 编译期嵌入）。
3. **既有 pin 测试逐字保持**：`moderation_finalize_outbox_parity`、`ddl_contract_defaults_and_checks`、`admin_class_rows_never_aggregated` 的名字/字面量不动（harness 过滤器钉死）；`audit_governance.rs` 模块 doc 的 cross-slice pin 注释不动。
4. **既有审计轨迹连续**：`auth.login.new_ip`、`auth.login.recovery_code`、`session.revoked`（用户侧）action 字符串不变；`LoginFailureRepo::record`、`record_login_event` 行为不变。
5. **login throttle / finalize_login 语义不变**：审计写在 `finalize_login(true)` 之后（handler 侧），失败 2FA 依旧只走 `login_failures`。
6. **connector/relay 零改动**：payload 原样转发；sink Idempotency-Key=event_id 对 auth 行与 L1 聚合行同样成立（合成 event_id 也是稳定 UUID）。
7. **harness 约束**：所有新 db_tests 落在 `aero-storage`（`audit_governance.rs` 模块内）→ `run_migrated_integration` 硬编码 `-p aero-storage` 无需参数化；仅 test-integration.sh 增一条命名过滤器条目（§6）。
8. **auth 公共 API 无破坏**：`AuthService` 公开签名不变（`NewRegistration` 加字段为 crate 内 3 处构造点更新）；`PatRepo`/`TotpRepo`/`SessionRepo` 既有方法不动（纯新增 `_in_tx` 变体）。
9. **无新依赖**：uuid v5 已启用（root :110）；sqlx 运行时 query（非宏）→ 无 offline 数据重建。
10. **v1 路径不动**：`snaplink_delivery_outbox`、0236 触发器、v1 relay 原样；聚合行无 audit_events 行 → 天然不进 v1。

## 4. Failure modes

| # | 失效 | 机制 | 后果 / 处置 |
|---|---|---|---|
| F1 | 审计/outbox INSERT 违例（payload 非 object、坏 target 等） | SAVEPOINT → ROLLBACK TO | 「整对没写」：域行照常提交（R7 fail-open），`tracing::warn` + 可选 `audit_auth_write_failures_total` 计数器（observability 惯例，非强制） |
| F2 | 连接级错误（DB 断、tx 死） | Err 向上传播 | 域操作整体失败（R1 三组行俱回滚，0 行逃逸）——与现状行为一致（域写本来就依赖 DB） |
| F3 | login/refresh 审计对写失败 | `record_pair_standalone` 内部 warn + Ok(None) | token 照常返回（auth 不可被审计 DoS）；无半对（独立事务） |
| F4 | outbox event_id 冲突（ULID 碰撞，理论级） | 0239 `ON CONFLICT DO NOTHING` | 幂等跳过；审计行保留（审计轨迹优先） |
| F5 | relay 投递失败/重放 | 既有 connector 状态机（退避 cap 300s、MAX_ATTEMPTS=5 → dead + last_error） | auth 行同 moderation 行待遇；dead 行 last_error 可观测 |
| F6 | L1 聚合并发/重跑 | 确定性 uuid-v5 event_id + `ON CONFLICT DO NOTHING` | 恰 1 行；count 不因重跑翻倍 |
| F7 | 时钟倾斜致迟到行落已关闭桶 | 桶键 = created_at 地板 | 迟到行计入下一开桶或永失——可接受（login_failures 保留逐尝试行作 forensic 底座）；聚合是 near-real-time 信号非账本 |
| F8 | handler 在 `finalize_login(true)` 与审计写之间崩溃 | 无行 | 登录成功但缺审计行——fail-open 既定代价（R7）；不阻塞登录、无半对 |
| F9 | L1 行被 relay 在窗口关闭前 claim | 只聚合**已关闭**桶（桶尾 ≤ now − window） | 行首次出现即完整（count 终值）；relay claim 时序无法截半 |
| F10 | admin revoke 审计失败 | 既有事务内 SAVEPOINT | revoke 照常（R7）；admin 侧从零审计变为 fail-open 审计 |
| F11 | 审计写占用长事务（慢） | 对短（3 语句） | 无新风险；savepoint 开销 ~µs 级 |

## 5. Migration steps

1. **前置（必须先做）**：提交上一批次 untracked 物——0239/0240/0241、`audit_governance.rs`、`governance.rs`、`aero-audit-connector/`、test-integration.sh 改动（req §8 义务；否则 `git reset --hard` 校准丢 B5-1 地基）。
2. **迁移 0243**：`migrations/0243_login_failures_created_at_idx.sql`（1 条 `CREATE INDEX IF NOT EXISTS`，幂等；文件头注释写明原计划 0242 与让号理由）。**顺序：`cargo build` → `aero-cli migrate`**（§4.2）。
3. aero-storage：`AuditGovernanceOutboxRepo`（H3）+ `governance_envelope` + `append_pair_in_tx_fail_open` + `record_pair_standalone` + `aggregate_login_failure_buckets` + `_in_tx` 仓储变体（pat/totp/auth_session）+ `NewRegistration.auth_audit` + 全部 db_tests（§6）。
4. aero-auth：`register_enrolled` 审计 spec + `refresh` 调用（fail-open 包装）。
5. aero-server：login handler 审计点；pat/twofa/sessions/session 路由 tx 编排（删 pool 级事后审计）；admin revoke 存储侧嵌对；boot timer；test-integration.sh 新条目。
6. **无 backfill**：auth 行从部署起才产生（历史 auth 操作无审计——如实记录，不回填伪造）；旧二进制（无 auth 写入）与新版 relay 混跑无冲突（0243 纯加索引；auth 行只是新出现的 outbox 行）。
7. **回滚**：代码回退即可；已写行继续被 relay 消费（status 机自洽）；0243 索引可留（无害）或后续迁移删。
8. 门禁：`cargo check --workspace` / `cargo test --workspace --lib`（PG 门控 `-- --ignored` + 一次性库）/ clippy / `scripts/{truth-check,file-size-check,web-check}.sh`；`test-integration.sh` B5 段全 PASS（AC4 已 un-gated）。

## 6. Testable acceptance mapping（AC1–AC5 → 具体测试）

全部新 db_tests 位于 `crates/aero-storage/src/audit_governance.rs` 的 `db_tests` 模块（H3 落点；**既有 `audit_governance::` harness 条目自动覆盖**，仅补一条命名条目）。PG 门控：`#[ignore = "requires live Postgres"]` + `DATABASE_URL`。

| 验收 | 测试（名字 = harness 过滤器） | 断言形态 |
|---|---|---|
| **AC1** register 原子性 | `registration_commit_half_writes_audit_and_outbox` / `registration_rollback_half_leaves_zero_rows` / `registration_audit_failure_fail_open_commits_domain` | commit 半部：`RegistrationRepo::create`（带 auth_audit）后 audit_events 恰 1 行（action=`auth.register`）+ outbox 恰 1 行（status=0、class='admin'、priority=10、`event_id`=audit id、payload.action=`admin.auth.register`、idempotency_key=event_id）；rollback 半部：重复 email → create Err → 两表均 0 行逃逸（same-tx proof）；fail-open 半部：auth_audit.detail 传非 object（触发 CHECK）→ **create 成功**、两表 0 行（SAVEPOINT 证明 R7） |
| **AC1** login 原子性 | `login_pair_commit_and_rollback` | `record_pair_standalone` 提交后恰 1+1 行；非 object detail 变体 → Ok(None)、0 行（独立事务对回滚证明） |
| **AC1** PAT-revoke 原子性 | `pat_revoke_in_tx_audit_pair_commit_and_rollback` | 单 tx：`revoke_in_tx` + 审计对 → commit 后 `revoked_at` 置位 + 1+1 行；rollback 半部（非 object detail）→ tx abort → **`revoked_at` 仍 NULL**（域行自身回滚 = 最强 same-tx proof）+ 0 行 |
| **AC1** TOTP-enroll 原子性 | `totp_enroll_in_tx_audit_pair_commit_and_rollback` | 同 PAT 形：`upsert_secret_in_tx` + 审计对 commit/rollback 两半 |
| **AC2** P2 parity | `auth_outbox_parity_1to1`（**新命名 harness 条目**） | 共享豁免谓词定稿见 connector-root design §6 AC2/§7 D13：outbox 侧 `payload->>'aggregated'='true'`（两切片共用顶层键）；audit 侧 allowlist（§2.7 token 表 + `message.moderated`）之外合法 audit-only——非「集合相等」裸断言；包络逐字段钉死（schema_id/schema_version/aggregate_type/aggregate_id/actor/targets/data_classification/retention_class/idempotency_key/source_system="aero-auth"） |
| **AC3** L1 聚合 N→1 | `login_failure_l1_aggregation_n_to_one` | 种子 N=3 行同桶 + 2 行异桶（window=60s）→ 跑 `aggregate_login_failure_buckets` → 恰 2 行（每桶 1 行）、payload.count=3/2、class='message'、status=0、event_id 确定性；**重跑 → 仍 2 行 count 不变**（幂等）；**`payload->>'aggregated' = 'true'` 在 envelope 顶层**（parity 豁免键，§2.6 定稿） |
| **AC4** drill | 既有条目零新增 | `aero-audit-relay-drill` / `aero-audit-t11-drill` / `aero-audit-priority-drill` 在 throwaway 库全 PASS（0239 已落库 → 无 SKIP 分支）；`b5_check` 三连绿 |
| **AC5** 全仓门禁 | 既有 | `cargo test --workspace --lib`（基线 81 保持）+ `-- --ignored` 一次性库 + truth-check/file-size/web-check + clippy 无新增警告 |

harness 改动（test-integration.sh，已在树内改动的文件）：在 B5-1 段追加一条 `run_migrated_integration` 条目，过滤器 `auth_outbox_parity`（命名 parity 测试，镜像既有 `moderation_finalize_outbox_parity` 条目形态）；`audit_governance::` 既有条目自动覆盖其余新测试。空过滤器守卫（"0 tests" 判 FAIL）对两条目均生效。

## 7. Decision points / risks

- **D1（token 映射归属）——维持 req 决议**：auth 显式写路不触碰 `governance_lane_for`（aero-ai 切片 + campaign 决策）；token 表（§2.7）为 aero-storage 常量，漂移由 AC2 逐字段断言兜底。
- **D2（L1 与 pin 冲突）——定稿结局 A**：聚合行 class='message'（message 域本就是 L1 聚合窗的候选域），`admin_class_rows_never_aggregated` 逐字不动（其断言对象 = audit/outbox 中的 admin 行永不进 message 窗；聚合行自身是 message class，无冲突）。结局 B（修订 pin）拒绝——避免动 aero-ai 已钉测试。AC3 无 SKIP 分支（本设计落地实现）。
- **D3（login/refresh 的「事务」定义）——维持**：独立事务对；rollback 证明 = 对自身回滚（非域行）。
- **D4（fail-open 边界）——机制定稿 = SAVEPOINT**：这是 R1 与 R7 唯一自洽的实现（域行提交 + 审计对消失）。若不做 savepoint，in-tx 审计失败会翻转 auth 结果（违反 R7）；若 pool 级写，则无原子对（违反 R1）。
- **D5（login 审计写点，req 细化）**：req §3 写「AuthService login 写入」，但 2FA 门在 handler 侧（证据勘误⑤）——**审计点定在 handler 侧** `finalize_login(true)` 之后。AuthService::login 仅保留 refresh 写点。这使「密码对但 2FA 失败」不被记为成功登录。
- **D6（session.revoked token 连续性，req 修正）**：R3 原提案 `auth.session.revoke` 改为沿用既有 `session.revoked`（sessions.rs 轨迹消费方兼容）；admin 侧新增 `session.revoked.admin`。出站 token 一律 `admin.auth.session.revoke`。
- **D7（聚合行无 audit_events 行，req 细化）**：‡ 类 1:1 豁免 + 避免 v1 触发副作用 + 避免 default-workspace 审计视图污染；event_id 合成（uuid-v5）确定性幂等。副作用：audit_events 全量导出（GDPR `GET /api/me/export`）不含登录失败——login_failures 已独立留存（既有行为，未改变）。
- **Risks**：① 前置批次未提交则 `git reset --hard` 校准丢地基（§5.1 硬前置）；② `NewRegistration` 加字段 = 3 构造点（编译强制，低风险）；③ 0243 索引迁移必须 build→migrate 顺序（§5.2，编号定稿 connector-root design §7 D8）；④ handler 侧审计点若被后续重构移到 `AuthService::login` 内会误记 2FA 失败——AC1 login 测试 + D5 注释钉死；⑤ auth 行 priority=10 与 moderation 100 同表——claim 排序由 0240 索引保证（auth 排后，符合「安全事件不抢占审核车道」）。

## 8. Sequencing

1. **前置**：提交上一批次 untracked 物（0239/0240/0241、`audit_governance.rs`、`governance.rs`、`aero-audit-connector/`、test-integration.sh）。
2. 迁移 0243（编号定稿 connector-root design §7 D8：`cargo build` → `aero-cli migrate`）。
3. aero-storage：H3 仓储 + envelope + fail-open 对 + standalone 对 + L1 聚合 + `_in_tx` 变体 + `NewRegistration.auth_audit` + db_tests（AC1-AC3 全部落此，parity 名/字面量不动）。
4. aero-auth：`register_enrolled` spec + `refresh` 写点（fail-open）。
5. aero-server：login handler 写点；pat/twofa/sessions/session 路由 tx 编排；admin revoke 存储嵌对；boot timer（env 门控）；test-integration.sh 追加 `auth_outbox_parity` 条目。
6. 门禁：全仓 test（含 `-- --ignored` 一次性库）+ clippy + 三脚本；`test-integration.sh` B5 段全 PASS（AC4 un-gated 无 SKIP）。

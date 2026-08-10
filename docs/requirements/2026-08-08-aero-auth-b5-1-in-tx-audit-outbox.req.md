# Requirements Spec — aero-auth B5-1：auth 域操作 in-tx 审计 outbox 写入（status 0/1/2/3、class admin、L1 聚合）

- **Module (analysis root)**: `crates/aero-auth/src` — 待接线面是 `AuthService`（`service.rs` register/login/refresh）；PAT/2FA/session 变更在 aero-server 路由侧（`pat.rs`/`twofa.rs`/`sessions.rs`）经 `aero_storage::PatRepo`/`TotpRepo`/`SessionRepo`
- **Direction**: "B5-1: In-tx audit outbox writes (status 0/1/2/3, class admin) for auth-domain operations — register/login/refresh/PAT-issue/PAT-revoke/2FA — with L1 aggregation for high-volume events"（value 6 / risk_reduction 6 / effort 6 / confidence 5）
- **Source analysis**: `docs/auto/analyses/crates-aero-auth-0b9b4b9f.json`（direction #3，proposed）
- **Campaign**: `aero-im-b5-outbox-relay`；contract anchor `docs/proposals/audit-contract-batch-aero-im.md:8`（B5-1：0239 DDL status 0/1/2/3、class message/room/admin、P2 parity event_id 1:1）；gate anchor `docs/campaigns/implementation-gate.md:64`（"`message.*`/`room.*`/`admin.*` 同事务写入；‡ 类走 L1"）、:78（G6）
- **Status**: Requirements（下述证据全部经源码 grep 核对；0239 已落工作树，方向引用中的「0239 not landed」已过时——见 §1 勘误）
- **Verification date**: 2026-08-08。行号是核对时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点（AGENTS.md §0）

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-auth/src/` — zero audit/outbox references | ✅ **Verified**。`rg -i "audit\|outbox" crates/aero-auth/src/` 零命中（exit 0 无输出）。aero-auth 是叶子 crate（`Cargo.toml` 仅 aero-common/aero-storage + 工具链），`AuthService` 结构 :57-91（`repo`/`sessions`/`jwt`/`pat_verifier`/`bot_verifier`/`login_throttle` 均为 Option 或 repo 句柄，无审计面）。 |
| E2 | `service.rs:184 register / :260 login / :334 refresh` | ✅ **Verified（精确）**。`pub async fn register` :184、`pub async fn login` :260、`pub async fn refresh` :334；另 `register_enrolled` :221（HTTP register 路由实际调用点，带 `workspace: WorkspaceId`）与 `login_inner` :312（login 的凭据核对本体）。 |
| E3 | `crates/aero-storage/src/audit.rs:127 append_in_tx / :51 new / :64 sweep`（模块 1049 行） | ✅ **Verified（精确）**。`AuditRepo::new` :51、`sweep_before` :64、`append` :113、`append_in_tx(tx, workspace, actor, action, target, detail) -> Result<AuditId>` :127；`append_on` :141-172 在事务内 `INSERT INTO audit_events (id, workspace_id, actor_id, action, target, detail, created_at)` 并返回新生成的 `AuditId`（ULID）——**auth 切片可用返回值做 outbox 的 event_id（1:1 前提）**。 |
| E4 | `crates/aero-im-core/src/service/outbox.rs:27-45`（EventOutboxRepo 用法） | ✅ **Verified**。`dispatch_event_outbox_batch` :27-45：`EventOutboxRepo::new(self.messages.pool.clone())` + `claim_due` + `finish_claimed_outbox`（模块 631 行）——im-core 的 outbox 循环模式。注意：im-core 的 outbox 是 `event_outbox`（RoomEvent relay），**与审计 outbox（`audit_governance_outbox`）是两个表两个链路**（E9）；im-core 本身零 audit 引用。 |
| E5 | `migrations/0162_event_outbox.sql` | ✅ **Verified**。39 行：`event_outbox` 表（`event_id UUID NOT NULL UNIQUE`、attempts/available_at/claimed_at/published_at/last_error + pending/published 部分索引）——事务生产者 outbox 的 DDL 先例（本 direction 不新增表，复用 0239）。 |
| E6 | `migrations/0236_snaplink_governance_reconciliation.sql:68-130`（AFTER INSERT 触发器） | ✅ **Verified**。`aero_enqueue_snaplink_audit()` :68-130 + `audit_events_snaplink_delivery` AFTER INSERT 触发器 :129-133：唯一门 = `snaplink_commercial_runtime.enabled`（:80-86），入 v1 `snaplink_delivery_outbox`，payload.action = 本地 token 原样。 |
| E7 | `migrations/0007_audit.sql` | ✅ **Verified**。22 行：`audit_events`（`workspace_id UUID NOT NULL REFERENCES workspaces`、`actor_id`、`action TEXT NOT NULL`、`target`、`detail JSONB`、ULID 主键 + `audit_events_workspace_idx`）。**workspace_id NOT NULL 是 auth 事件写入的硬约束**（§4 R4 的 workspace 解析规则由此而来）。 |
| E8 | `docs/proposals/audit-contract-batch-aero-im.md:8`（B5-1 DDL class message/room/admin、L1 聚合） | ✅ **Verified（as proposed）**。:8 = "B5-1：新迁移 `0239_audit_governance_outbox.sql` —— 专用审计 outbox 表（status 0/1/2/3 normative、`class` message/room/admin、`priority`、`delivery_mode`）…… P2 parity = `event_id` 1:1 断言"。campaign gate `docs/campaigns/implementation-gate.md:64` 行 1 = "Outbox + in-tx：DDL（status 0/1/2/3 normative）；`message.*`/`room.*`/`admin.*` 同事务写入；‡ 类走 L1"。 |
| E9 | ⚠️ **勘误（已过时）**：`migrations list ends at 0238 (0239 audit_governance_outbox not landed)` | ❌ **Superseded**。migrations 现止于 **0241**：`0239_audit_governance_outbox.sql`（140 行）、`0240_audit_governance_due_prio_idx.sql`、`0241_governance_reconcile.sql` 均已在工作树（`git status` = untracked，未提交）。0239 落地内容 = B5-1 的 DDL 本体：`audit_governance_outbox`（`event_id UUID PK` = `audit_events.id` 1:1、`status INTEGER NOT NULL DEFAULT 0 CHECK (status IN (0,1,2,3))`、`class TEXT NOT NULL DEFAULT 'message' CHECK (class IN ('admin','message','room'))`、`priority SMALLINT NOT NULL DEFAULT 10 CHECK (priority > 0)`、`delivery_mode`、`payload JSONB NOT NULL CHECK (jsonb_typeof = 'object')`、attempts/claim_token/lease_expires_at/delivered_at/last_error + claim-state CHECK + due 部分索引）+ **token-keyed 触发器** `aero_enqueue_governance_audit()`：**仅 `NEW.action = 'message.moderated'` 入 outbox**（class 'admin'、priority 100、status 0、payload 完整 v2 信封含 idempotency_key=event_id），未知 token `RETURN NEW` pass-through 不 raise（fail-open）。**auth 域 token 不会经触发器入 outbox**——auth 切片必须显式 in-tx 写（§4 R2/R3），且不得改动已落地的触发器/映射（E10/E12 钉死）。 |
| E10 | （补充）`crates/aero-storage/src/audit_governance.rs`（933 行，untracked）——B5-1 存储切片的契约夹具 | ✅ **Verified**。模块 doc 明言："The B5-1 storage direction's `AuditGovernanceOutboxRepo` lands in a sibling slice (handoff H3 of the 0239 design doc); this module carries the DDL-contract fixtures only — **add the repo to this same module when it lands**"。db_tests 已含：`moderation_finalize_outbox_parity` :247（A1 同事务 oracle：恰 1 audit + 1 outbox、event_id 1:1、status 0、class 'admin'、priority 100、rollback 半部证明两行俱回滚、replay 半部幂等）、`ddl_contract_defaults_and_checks` :424、`moderation_finalize_runtime_disabled_commits_1_plus_0` :583、`non_moderation_action_passes_through_unmapped` :628、`duplicate_event_id_is_deduped_by_on_conflict` :693。**`AuditGovernanceOutboxRepo` 尚不存在**（全仓 grep 仅模块 doc 提及）——auth 切片所需 in-tx 写入仓储是新增物（§6 落点 = 同一模块，H3 明示）。 |
| E11 | （补充）connector 状态机常量 | ✅ **Verified**。`crates/aero-audit-connector/src/pg.rs:27-30`：`STATUS_ENQUEUED=0 / STATUS_CLAIMED=1 / STATUS_DELIVERED=2 / STATUS_DEAD=3`，与 0239 CHECK 逐字一致；`OutboxRepo` trait（outbox.rs:51）`claim_due`。relay drill `aero-audit-relay-drill.rs` :40-50 以 `to_regclass('audit_governance_outbox')` 为 SKIP（exit 2）门——0239 落库后门自动解开。 |
| E12 | （补充）`crates/aero-ai/src/governance.rs`（211 行，untracked）——映射唯一权威 | ✅ **Verified**。`GOVERNANCE_CLASS_{ADMIN,MESSAGE,ROOM}` :37-41、`GOVERNANCE_PRIORITY_{MODERATION=100,BACKLOG=10}` :31-33、`LOCAL_ACTION_MODERATED="message.moderated"` :49、`MODERATION_OUTBOUND_ACTION="admin.content.flag"` :56；`governance_lane_for(local_action)` :86-94 **唯一映射** = `message.moderated` → `{class:'admin', priority:100, outbound:'admin.content.flag', status:0}`，未知 token → `None`。单测钉死：`unknown_local_token_passes_through_unmapped`（`room.create`/`message.create`/`message.edit`/`message.deleted`/`call.join`/`""` 全 None）、`admin_class_rows_never_aggregated` :205（**admin class 绝不进 [PROPOSED] L1 message.* 聚合窗**——auth 事件 class 选择与 L1 聚合的冲突点，§7 D1）。 |
| E13 | （补充）auth 域现状审计行为盘点 | ✅ **Verified**。① login 成功：`routes/handlers/auth.rs::record_login_event` :35-65 仅在新 IP 时 best-effort 写 `audit_events`（action `auth.login.new_ip`，pool 级非 in-tx，失败仅 warn）+ `LoginEventRepo::record`；**login 成功本身无 audit 行**。② login 失败：`LoginFailureRepo::record`（`login_failure.rs:70`，`login_failures` 表，migration 0140，逐尝试一行，pool 级）——**高容量失败事件已有独立持久表**（L1 聚合的天然底座，§7 D2）。③ 注册：无 audit。④ refresh：无 audit。⑤ PAT issue/revoke：`PatRepo::create` :104 / `revoke` :190 为 pool 级单语句，**无事务变体、无 audit**（路由 `crates/aero-server/src/pat.rs`）。⑥ TOTP enroll：`twofa.rs` 路由 + `TotpRepo`，无 audit。⑦ session revoke：`auth_session/admin_revoke.rs:112` 已 in-tx 写 `session.revoked`（admin 主动吊销；用户侧 `sessions.rs` 路由无 audit）。既有 action token 风格 = 点分小写（`auth.login.new_ip`/`session.revoked`/`message.moderated`）。 |
| E14 | （补充）workspace 解析先例 | ✅ **Verified**。register 路由 = `register_enrolled(req, DEFAULT_WORKSPACE_ID, user_agent)`（handlers/auth.rs:86）；`record_login_event` 的 audit 行 scoped 到 `Some(DEFAULT_WORKSPACE_ID)` 并注释明言 "login is workspace-agnostic …… the all-zero default is where account-level security events land"（handlers/auth.rs:208-213）。`DEFAULT_WORKSPACE_ID` = nil UUID；nil workspace 行由 `migrations/0006_workspaces.sql:61` 种子插入。**「账户级安全事件落 default workspace」是既有约定，本切片沿用**（§4 R4）。 |
| E15 | （补充）`RegistrationRepo::create` 已有单事务 | ✅ **Verified**。`registration.rs:37`：begin → participants + credentials + workspace_members + refresh session 行 → commit（一个 PG 事务）。**register 的 in-tx 审计写入必须骑这个事务**（§4 R1a / §6 落点）。 |
| E16 | （补充）harness 门 | ✅ **Verified**。`scripts/test-integration.sh`：B5-1 条目（:306-318 `audit_governance::` 与 `moderation_finalize_outbox_parity` 的 `run_migrated_integration`，**仅 `cargo test -p aero-storage --lib`**，空过滤器守卫禁 vacuous green）、A3 relay drill（:325-341，文件存在即 PASS 断言）、T-11 fail-closed（:349+）。所有 B5 条目以 `migrations/0239_audit_governance_outbox.sql` 文件存在为门——**文件已存在 → 全部 un-gated，SKIP 分支已死**。 |

### 1.1 对 direction 问题陈述的勘误/注解（evidence-backed）

- **「0239 not yet landed」已过时**：0239/0240/0241 + `audit_governance.rs` + `aero-audit-connector/` + `governance.rs` 均已在工作树（untracked，未提交）。acceptance 中 "drill gate: SKIP (exit 2) while 0239 absent, un-gated after migration lands" 的**后半个条件已满足**——本切片验收的 drill 面 = 「已 un-gated 且必须 PASS」，不再有 SKIP 分支（§5 AC4）。
- **「which auth events map to admin.* and the L1 aggregation shape are [PROPOSED]」保持成立**，且新增一条已落地的硬约束：0239 触发器是 token-keyed（仅 `message.moderated`），`governance_lane_for` 是唯一映射权威（E12）——**auth 事件不能靠触发器入 outbox**，必须显式 in-tx 写（这反而是本 direction 标题所要求的形态）；「auth token → admin class」的映射表本身仍是 [PROPOSED]（§4 R3 给出提案）。
- **「the write must be added either in AuthService transactions or via server-side orchestration — neither exists today」仍准确**：`AuthService` 无任何事务编排（register 的事务在 `RegistrationRepo::create` 内部、login/refresh 无事务、PAT/2FA/session 变更在 server 路由 pool 级调用）——本切片必须引入 in-tx 写入点。
- **P2 parity 与 L1 聚合的契约关系**：contract 的 parity 断言（E8）适用于非聚合行；implementation-gate 明言 "‡ 类走 L1"（高容量事件豁免 1:1）——auth 切片里 register/login 成功/PAT/2FA 属 admin class 1:1 行，**login 失败属高容量 ‡ 类**，L1 形态 [PROPOSED]（§7 D2 与 `admin_class_rows_never_aggregated` 的调和是 campaign 决策，不是本切片可单方拍板）。

## 2. Verified current state

```
audit_governance_outbox（0239 已落工作树，未提交）：
  event_id PK = audit_events.id（1:1）· status 0/1/2/3（pg.rs:27-30 钉死）· class admin|message|room
  · priority > 0（DEFAULT 10 = BACKLOG；moderation 100）· payload JSONB object · 幂等 ON CONFLICT DO NOTHING

入队现状（两条路，互不相认）：
a) 触发器路（E9）   audit_events AFTER INSERT → aero_enqueue_governance_audit()
                   └─ 仅 NEW.action='message.moderated' → outbox（class admin/priority 100）；其余 pass-through
b) 显式写路（E10）  AuditGovernanceOutboxRepo —— 尚不存在（H3 手记：落 audit_governance.rs 同模块）

auth 域审计现状（E13/E14）：
  register  → 无 audit；事务在 RegistrationRepo::create（E15）
  login     → 成功仅新 IP 时 best-effort 写 auth.login.new_ip（pool 级）；失败入 login_failures 表（0140，非 audit_events）
  refresh   → 无 audit
  PAT/2FA   → PatRepo/TotpRepo pool 级单语句，无 audit、无事务变体
  session revoke → admin 侧已 in-tx 写 session.revoked（admin_revoke.rs:112）；用户侧无
  workspace 约定：账户级安全事件落 DEFAULT_WORKSPACE_ID（nil，0006:61 种子行）—— E14

门禁现状（E16）：test-integration.sh 的 B5 条目已全部 un-gated（0239 文件存在）；
  run_migrated_integration 仅跑 -p aero-storage（§6 落点约束）
```

**Gaps this direction closes**（all verified）：① `AuditGovernanceOutboxRepo`（含 in-tx 变体）不存在——auth 切片需要它（H3 明示落 `audit_governance.rs`）；② `AuthService` register/login/refresh 无审计写入——在事务内（register）或新事务对（login/refresh）补 audit+outbox 两行；③ PAT issue/revoke、TOTP enroll、session revoke 的 server 路由侧无 in-tx 审计——需事务化（tx-scoped 仓储变体或路由侧 tx 编排）；④ 高容量 login 失败事件无 L1 聚合——[PROPOSED]，与 E12 pin 调和后落地。

## 3. Scope

**In scope（B5-1 auth 切片，effort 6 的完整范围）**：
- aero-storage：`audit_governance.rs` 模块新增 `AuditGovernanceOutboxRepo`（H3 手记明示落点）：`append_in_tx(tx, event_id, class, priority, payload)`（payload 必含 `idempotency_key`，与 0239 触发器信封同构：`schema_id 'aero.im.security'`、`schema_version 1`、`aggregate_type 'workspace'`、`aggregate_id`、`action`、`occurred_at`、`actor`、`targets`、`outcome`、`data_classification 'confidential'`、`retention_class 'security'`——relay 原样转发、sink 以 Idempotency-Key=event_id 去重）；`db_tests` 与既有 `moderation_finalize_outbox_parity` 同模块共存，**不改其名/字面量**（harness 过滤器钉死）。
- aero-auth：`AuthService` 的 auth 域变更路径写入 audit+outbox 两行（同一 PG 事务）：
  - `register`/`register_enrolled`：审计行 + outbox 行**骑 `RegistrationRepo::create` 的既有事务**（E15）；workspace = 注册 workspace；actor = 新 participant
  - `login`（成功）：新事务对（audit+outbox 原子成对；login 本身无事务，`auth.login` 事件与 `auth.login.new_ip` 并存不冲突——后者保留）
  - `refresh`：同 login（新事务对）
- aero-server 路由侧（经 aero-storage 仓储变体或路由 tx 编排）：
  - `POST /api/pat` / `DELETE /api/pat/:id`：`PatRepo` 增 tx-scoped 变体（或路由开事务），PAT issue/revoke + audit+outbox 同行提交
  - `POST /api/me/2fa/enroll`（含 activate）：TOTP 变更 + audit+outbox 同行
  - `sessions.rs`/`admin_sessions.rs` 用户侧 session revoke：audit+outbox 同行（admin 侧 `session.revoked` 已 in-tx，补 outbox 行即可）
- 高容量 login 失败 L1 聚合：[PROPOSED] 形态落地或明确 deferred（§7 D2 决策点，验收 AC3 以测试形态钉住两种结局）。
- 测试/门禁：db_tests + harness 条目（§6）+ 既有 drill 回归保持。

**Out of scope（不扩）**：不改 0239/0240/0241 已落地 DDL 与触发器（token-keyed 映射、CHECK、默认值逐字保持——E12 单测是漂移守卫）；不改 `governance.rs` 映射权威（auth token 若需进 `governance_lane_for` 是 aero-ai 切片 + campaign 决策，本切片走显式写路，§7 D1）；不做 v1 重定向/兼容（H4 属别的切片）；不新增迁移（0239 已含本切片所需表）；不动 v1 `snaplink_delivery_outbox` 路径；auth 事件的 payload 信封不做仓外契约新增（沿用 0239 信封形状）。

## 4. Requirements

**R1（in-tx 原子性）**：`register`/`register_enrolled` 的成功路径在**同一 PG 事务**内提交：① 域变更行（participant + credentials + workspace_members + refresh session，E15 既有事务）；② 恰 1 行 `audit_events`；③ 恰 1 行 `audit_governance_outbox`。任一失败 → 三组行全部回滚（0 行逃逸）。`login`（成功）与 `refresh` 的 audit+outbox 两行在**同一事务**成对提交/回滚。PAT issue/revoke、TOTP enroll、session revoke 的域语句 + audit + outbox 三者在同一事务。**每个非成功分支不得留下已写行**（§4.2 at-least-once 状态机纪律：无行 = 无副作用，天然幂等）。

**R2（outbox 行契约）**：每条 audit 事件对应恰 1 条 outbox 行：`event_id = audit_events.id`（`AuditRepo::append_in_tx` 返回的 `AuditId`，E3）、`status = 0`（`STATUS_ENQUEUED`，E11）、`class = 'admin'`、`priority = 10`（`GOVERNANCE_PRIORITY_BACKLOG`，E12；moderation 100 车道不动）、`payload` 为 0239 同构 v2 信封（§3）且 `idempotency_key = event_id::text`、`action` = 出站 token（R3）。重复投递由 `ON CONFLICT (event_id) DO NOTHING` 幂等（0239 已内置，E9）。

**R3（auth 事件 → action token 映射，[PROPOSED]）**：本地 token 沿用既有点分风格（E13），出站 action 沿用 `admin.*` 前缀约定（`admin.content.flag` 先例，E12）。提案：

| 操作 | 本地 token（audit_events.action） | 出站 action（payload.action） | class | actor |
|---|---|---|---|---|
| 注册成功 | `auth.register` | `admin.auth.register` | admin | 新 participant |
| 登录成功 | `auth.login` | `admin.auth.login` | admin | participant |
| 刷新成功 | `auth.refresh` | `admin.auth.refresh` | admin | participant |
| PAT 签发 | `auth.pat.issue` | `admin.auth.pat.issue` | admin | 持 token 者 |
| PAT 吊销 | `auth.pat.revoke` | `admin.auth.pat.revoke` | admin | 持 token 者 |
| TOTP 登记/激活 | `auth.totp.enroll` | `admin.auth.totp.enroll` | admin | participant |
| session 吊销（用户侧） | `auth.session.revoke` | `admin.auth.session.revoke` | admin | participant |
| 登录失败（高容量） | `auth.login.failure` | （L1 聚合行，§7 D2） | [PROPOSED] | 无（pre-auth） |

目标字段：`target` = 资源 id（PAT id / session id / 邮箱串 / 参与者 id 文本）；失败事件的 detail 含尝试账号与来源（对齐 `LoginFailureRepo` 现有字段，E13）。

**R4（workspace 解析规则）**：沿用既有约定（E14）——账户级事件（login/refresh/PAT/TOTP/session）落 `DEFAULT_WORKSPACE_ID`；`register_enrolled` 落注册 workspace。`audit_events.workspace_id NOT NULL`（E7）由此恒满足。

**R5（不动已落地映射/DDL）**：0239 触发器、CHECK、默认值、`governance_lane_for` 映射、`audit_governance.rs` 既有 db_tests 名称/字面量**逐字保持**（E9/E10/E12）；auth 写入走显式 in-tx 写（R2），不扩触发器 token 集。

**R6（行为语义，非投递门）**：auth outbox 行**无条件入队**（不 consult `snaplink_commercial_runtime.enabled`、不依赖 binding、无 Gate 2 RAISE）——账户安全事件不因商业开关被抑制；这与 0236/0239 触发器门的语义正交（触发器只管 snaplink v1 投递与 moderation 行）。[PROPOSED] 决策点（§7 D1 备选已记），默认按本 R 执行。

**R7（失败不阻塞）**：审计写入错误**不得翻转 auth 操作结果**——register/login 的审计写失败按 fail-open 处理（log-warn，操作照常成功）——但**写了一半（audit 行已写、outbox 行失败）的事务必须整体回滚**（R1 的原子性由事务保证，fail-open 只允许「整对没写」，不允许「半对」）。PAT revoke 等已存在的幂等语义（`revoked_at IS NULL` 谓词）不得因审计失败而改变。

**R8（L1 聚合，[PROPOSED]）**：高容量 `auth.login.failure` 事件按聚合窗（[PROPOSED]：workspace × action × 窗口，窗口大小默认 60s）合并为**一条** outbox 行（payload 带 `count`），而非逐事件一行。**约束**：不得与 `admin_class_rows_never_aggregated`（E12 :205）冲突——若 admin class 永不聚合成立，则失败事件 class 取 message（L1 聚合窗的既有候选域）或经 campaign 决策修订 pin；两种结局都必须满足 AC3 的「N 事件 → 1 行」断言形态。聚合底座可复用 `login_failures` 表（E13，0140 迁移，逐尝试已持久化）做窗口扫描。

## 5. Acceptance checks（direction 原样保留，逐条 testable）

> 方向 acceptance 五条全部保留；AC1-AC3 为 PG 门控 db_tests（`#[ignore = "requires live Postgres"]` + `DATABASE_URL` 门控，仿 `audit_governance.rs` 既有夹具），AC4 为 harness drill，AC5 为全仓门禁。

- **AC1（T-11/B5-1 in-tx 原子性）**：db_test —— `register_enrolled` 提交后：`audit_events` 恰 1 行（action=`auth.register`）+ `audit_governance_outbox` 恰 1 行（`status=0`、`class='admin'`、`event_id` = 该 audit 行 id）；**回滚半部**：同一事务内制造失败（如注入非法 `target` 长度/约束违例或强制 rollback），断言 `audit_events` 与 `audit_governance_outbox` **均 0 行逃逸**（same-transaction proof，镜像 `moderation_finalize_outbox_parity` 的 A1-RB 半部，E10）。同型断言覆盖 login（成功）、PAT-revoke、TOTP-enroll 三路（direction 明确点名的操作）。
- **AC2（P2 parity，contract :8）**：db_test —— 每个非聚合事件：`audit_governance_outbox.event_id` 与 `audit_events.id` **1:1**（SELECT 两表 join 断言集合相等，无孤儿无重复；镜像 E10 parity 测试的 Half 3 断言形态）。L1 聚合行豁免（contract "‡ 类走 L1"）。
- **AC3（L1 聚合，[PROPOSED]）**：db_test —— 同窗口内 N（≥2）条 `auth.login.failure` 事件 → outbox **恰 1 行**（payload.count = N）而非 N 行；跨窗口分离正确。若 D2 决策为 deferred：测试以 SKIP 分支存在，且 harness 空过滤器守卫不触发（§7 D2 写明两种结局的落地形态）。
- **AC4（drill gate）**：`aero-audit-relay-drill.rs` 的 SKIP（exit 2）仅当 `to_regclass('audit_governance_outbox')` 为 NULL（E11 :40-50）；**0239 已落库 → gate 已 un-gated**：harness `b5_check "a3-relay-drill" PASS`（`scripts/test-integration.sh:325-341`）在 throwaway 库上全绿（`COUNT(status=2) == N` + event_id 集合 parity）。本切片不得重新引入 SKIP 分支。
- **AC5（全仓门禁）**：`cargo test --workspace --lib` 全绿（当前基线：`cargo test -p aero-auth --lib` = 81 passed；PG 门控测试经 `-- --ignored` + 一次性库跑）；`scripts/truth-check.sh` 0 违规；`cargo clippy --workspace --all-targets` 无新增警告（AGENTS §4.3）。

## 6. Test placement

- **`AuditGovernanceOutboxRepo` 的 db_tests + AC1/AC2/AC3 的 in-tx 断言**：`crates/aero-storage/src/audit_governance.rs` 同模块（H3 手记明示落点；harness 过滤器 `audit_governance::` 已存在且 un-gated，E16）——仓储测试走既有 `run_migrated_integration`（`-p aero-storage --lib`）零 harness 改动。
- **AuthService 接线测试**（register/login/refresh 的写入点）：若写入编排在 `AuthService`（aero-auth），加 `#[ignore]` PG db_tests + harness 参数化（`run_migrated_integration` 现硬编码 `-p aero-storage`，需加包名参数）；若写入点下沉到 aero-storage 仓储（tx-scoped 变体，R1a），则 AC1 的 register/login/PAT 断言可全部落在 aero-storage db_tests，harness 零改动。**推荐后者**（harness 面最小、与 H3 手记一致），AuthService 仅做调用。
- **AC4**：既有 `aero-audit-relay-drill` + `test-integration.sh` 条目，零新增。
- **AC5**：既有全仓门禁，零新增。

## 7. Risks / [PROPOSED] / 决策点

- **D1（auth token 映射归属）**：R3 的 token 表是 [PROPOSED]。显式写路（本切片）不触碰 `governance_lane_for`；若 campaign 要求 auth token 也进映射权威（`governance.rs` 扩展 + 0239 触发器扩 token 集），则是 aero-ai 切片 + 迁移改动，本切片拒绝范围外实现。风险：两处 token 清单漂移 → 由 R5 的逐字保持 + AC2 1:1 断言兜底。
- **D2（L1 聚合形态与 pin 冲突）**：`admin_class_rows_never_aggregated`（E12）与「login 失败走 L1」冲突。结局 A：失败事件 class='message' 入 L1 窗（与既有 pin 自洽，聚合窗在 message 域）；结局 B：修订 pin（governance.rs 单测改名/改义，aero-ai 切片）。**两种结局都须满足 AC3 的 N→1 断言**。若 campaign deferred：AC3 的测试以 SKIP 落地且 harness 空过滤器守卫不红（条目需有 ≥1 匹配测试——deferred 时不得注册 harness 条目）。底座 `login_failures`（0140）已就绪，扫描窗口成本可测。
- **D3（login/refresh 的「事务」定义）**：login/refresh 今天无域事务（E13）——R1 要求 audit+outbox 两行原子成对（新事务对）。「rollback of the mutation removes the row」对这两路 = 该事务对回滚。register/PAT/2FA 的 rollback 证明覆盖域行本身（更强的 same-transaction proof）。
- **D4（fail-open 与原子性的边界）**：R7 允许「整对没写」，禁止「半对」——实现上靠「先写 audit 行，同 tx 写 outbox 行，任一失败整体 rollback」；调用方捕获错误后仅 warn 不翻结果。风险：写失败静默丢事件 → `tracing::warn` +（可选）`audit_auth_write_failures_total` 计数器（observability_gauge_samplers 惯例，非本切片强制）。
- **Risks**：① 0239 等文件 untracked（E9/E10/E12 全在未提交工作树）——本切片落地前须先提交上一批次（verified-landing 设计 doc 的 Oracle 已列此义务），否则 `git reset --hard` 校准会丢 B5-1 地基；② harness `run_migrated_integration` 硬编码 `-p aero-storage`（E16）——若 AC1 必须走 aero-auth 包，harness 需参数化（小改，风险低）；③ 既有 `record_login_event` 的 `auth.login.new_ip`（pool 级 best-effort）与新增 `auth.login`（in-tx）并存——不冲突（不同 action），但不得把 new_ip 逻辑改成 in-tx（其 best-effort 语义是既定的）。

## 8. Sequencing

1. **前置**：提交上一批次 untracked 物（0239/0240/0241、`audit_governance.rs`、`governance.rs`、`aero-audit-connector/`、test-integration.sh 改动）——B5-1 地基。
2. aero-storage：`AuditGovernanceOutboxRepo`（`append_in_tx`）+ db_tests（AC1/AC2/AC3 仓储面；parity 测试名/字面量不动）。
3. aero-auth：`AuthService` register/login/refresh 写入点（R1/R4），fail-open 边界（R7）。
4. aero-server 路由侧：PAT/2FA/session revoke 的 tx-scoped 变体 + 接线（R1）。
5. L1 聚合（D2 决策后）：窗口扫描 + 聚合行写入 + AC3。
6. 门禁：`cargo test --workspace --lib`（含 `-- --ignored` 一次性库）+ `scripts/truth-check.sh` + clippy；`test-integration.sh` B5 段全 PASS（AC4 已 un-gated）。

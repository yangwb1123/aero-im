# Requirements Spec — aero-auth fail-closed relay-provisioning gate（B5-4）：builder-hook seam + `aero-cli audit-provision-check` 落家

- **Module (analysis root)**: `crates/aero-auth/src` — gate seam 所在（`AuthService` builder-hook 模式复制）；落地动作跨 aero-auth（seam）/ aero-storage（心跳状态）/ aero-server（接线 + CLI + 心跳记录）/ migrations（0240）
- **Direction**: "Add a fail-closed relay-provisioning gate on AuthService (B5-4) using the existing builder-hook pattern"（value 7 / risk_reduction 9 / effort 5 / confidence 7）
- **Source analysis**: `docs/auto/analyses/crates-aero-auth-src-650f2e56.json`（direction #3）
- **Campaign**: `aero-im-b5-outbox-relay`（`docs/campaigns/campaign-aero-im-b5.yaml:39-42`："scope provisioning (grant audit:event:write only after relay works; fail-closed without relay)"）；contract anchor `docs/proposals/audit-contract-batch-aero-im.md`（B5-4 行 :11）；gate anchor `docs/campaigns/implementation-gate.md`（:64 aero-im 行 "T-11（无 relay 配给被拒）"、:78 G6 "37/37、T-11、moderation 优先级"）
- **Status**: Requirements（下述证据全部经源码 grep 核对）
- **Verification date**: 2026-08-07。行号是核对时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点（AGENTS.md §0）

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-auth/src/service.rs:102-127` — `with_login_throttle`/`sweep_login_throttle` builder+lifecycle 模式 | ✅ **Verified**。`AuthService.login_throttle: Option<Arc<LoginThrottle>>` :78；`new()` 置 `None`（:93，opt-in 默认关）；builder `#[must_use] with_login_throttle(mut self, …)` :102-113；lifecycle hook `sweep_login_throttle(now) -> usize` :116-123（`None` 时 no-op 返回 0，驱动方在 `boot/serve.rs:203`）。同族先例：`with_pat_verifier` :134、`with_bot_verifier` :146。**接线点**：`crates/aero-server/src/bin/boot/services.rs:71`（`auth = auth.with_login_throttle(Arc::new(throttle))`，env 门控）。 |
| E2 | `crates/aero-auth/src/extractor.rs` — AuthUser 链 JWT→PAT→bot，无 machine/client-credentials 身份 | ✅ **Verified**。`AuthUser{participant_id, session_id, exp}` :31-37（无机器身份 kind、无 scope 字段）；`from_request_parts` :65-134：JWT `svc.verify` → `assert_access_claims_active` → 失败后 `verify_pat` → `verify_bot_token`（:97-119）。**全仓唯一 audit-scoped 机器凭据流 = connector relay 的出站 cc token**（E7），不存在任何入站 audit-scope 机器令牌验收路径——「no machine-identity path at all」准确。 |
| E3 | `migrations/0236_snaplink_governance_reconciliation.sql` — AFTER INSERT 触发器无条件入队 | ✅ **Verified**。`aero_enqueue_snaplink_audit()` :68-130：唯一门 = `snaplink_commercial_runtime.enabled`（:80-86，关则 `RETURN NEW`）；**无任何 relay 健康检查**——enabled 即入 `snaplink_delivery_outbox`；触发器绑定 `audit_events_snaplink_delivery` `AFTER INSERT ON audit_events FOR EACH ROW` :129-133。`payload.action` = 本地 token 原样（无 class/priority 戳）。「enqueues regardless of relay health」准确。 |
| E4 | `crates/aero-server/src/snaplink_commercial/runtime.rs` — relay 运行期状态在 server 内 | ✅ **Verified**。`SnaplinkCommercialRuntime`：`from_env` :26-63（boot 门：Unspecified→`require_disabled`、Disabled→`configure_disabled`、Enabled→`configure_enabled`——**fail-closed 一致性门已存在**）；`ready()` :87（projection 式，刻意排除远端可达性）；`spawn` :95；`run_delivery_relay` :174；`binding_for_claim` :283（身份不匹配 bail）；`DELIVERY_BACKLOG_METRIC` :19。接线：`boot/state_builder.rs:16-17`（`Option<Arc<…>>`）+ `routes/health.rs::probe_commercial` :67-77（"disabled"/"ok"/"not_ready"/"fail"/"timeout"）。**readyz 不翻转**（proposal :11）✅：`health_ready` :154-199 只对 `matches!(commercial, "ok" \| "disabled")` 判 ready，relay 健康从不翻 readyz。 |
| E5 | `docs/proposals/audit-contract-batch-aero-im.md` B5-4 — `aero-cli audit-provision-check` [PROPOSED] 无家 | ✅ **Verified (as proposed)**。:11 = "B5-4：本仓库交付配给验证 seam（`aero-cli audit-provision-check`，[PROPOSED]）+ fail-closed（boot 门已验证存在；readyz 不翻转）；registry 本体在 IdP 仓"。**全仓 grep `audit-provision-check` 仅命中 docs**（proposal + 6 份 req/design 文档），零代码。 |
| E6 | （补充）`aero-cli` 的落家盘点 | ✅ **Verified**。两个候选：(a) `crates/aero-server/src/bin/aero-cli.rs`（702 行，`[[bin]] name="aero-cli"`，`aero-server/Cargo.toml:20-24`）——**唯一 DB 能力的 aero-cli**（`migrate` 走 `aero_storage::migrate`，`health` 探 PG/Redis/NATS；15 命令；completion 列表 :685）——`test-integration.sh` 的 `cargo run --bin aero-cli -- migrate` 即此；(b) `crates/aero-cli/` 包 = aero-eng 工程 CLI（`[[bin]] name="aero-eng"`，无 DB 依赖）。**audit-provision-check 的家 = (a)**。 |
| E7 | （补充）B5-2 connector relay 现状（T-11 已实现的投递侧） | ✅ **Verified**。`crates/aero-audit-connector/` 已在工作树并**已接线生产**：`crates/aero-server/src/bin/main.rs:245-269`（`RelayConfig::from_env()` presence-gated :251 → `PgOutboxRepo` :254 + `AuditClient` + `AuditRelay` :259 → `tracker.spawn` :260）。`relay.rs::deliver_claim`：`DeliveryError::Forbidden` → `mark_dead` attempt 1 :177-190（**403→dead = T-11 已实现**）；`client.rs` `SCOPE_AUDIT="audit:event:write"` :31、`deliver` 403 分类 :166、claim 校验 :193-242。**无任何心跳/配给状态**——`AuditRelay` 只做 claim→deliver→settle/requeue/dead；connector `Cargo.toml` 无 aero-storage 依赖（raw SQL）。 |
| E8 | （补充）`Error::Forbidden` = 403（gate 拒绝的规范状态码） | ✅ **Verified**。`crates/aero-common/src/error.rs`：`Forbidden(String)` :19、HTTP code 403 :70、code 串 "forbidden" :85。extractor 的 `AuthRejection` 是 401（凭据缺失/无效，正交）。acceptance 的 "401/403" 在仓内钉死读法：**gate 拒绝 = `Error::Forbidden`（403）**。 |
| E9 | （补充）门禁/契约锚点 | ✅ **Verified**。`docs/campaigns/implementation-gate.md:64`（aero-im 行：Rust relay + "T-11（无 relay 配给被拒）"）、:78（G6 = "37/37、T-11、moderation 优先级"）；`docs/requirements/2026-08-06-aero-ai-b5-1-audit-governance-outbox.req.md` §3（"provisioning seam `audit-provision-check` → **B5-4 (aero-cli)**"）；`docs/requirements/2026-08-07-aero-auth-b5-2-machine-token-verification-seam.req.md` §8.3（"B5-4 配给门消费本 direction 的 403/scope-deficient → dead（T-11）"——**因果链**：gate 不健康 → IdP 拒绝配给 → token endpoint 403 → connector dead ≤1，B5-2 已实现）。 |
| E10 | （补充）moderation 优先级面（B5-3，[PROPOSED]） | ✅ **Verified（as proposed）**。`2026-08-06-aero-ai-b5-1-audit-governance-outbox.req.md` R1/R6：`message.moderated` → class='admin' + 出站 `admin.content.flag`/`admin.moderation.action`（仓外契约 token 清单，[PROPOSED]）+ R6 drill fixture；`claim_due ORDER BY priority DESC` + 反饥饿上限 = **B5-3（aero-storage）**，未落库（0239 未落地，migrations 尾号 0238；`scripts/test-integration.sh` :236-270 A3 drill 与 :253 B5-1 条目均以 `migrations/0239_audit_governance_outbox.sql` 存在为门）。**本 direction 只钉「gate 不得阻塞」约束，不实现优先级机制**。 |

### 1.1 对 direction 问题陈述的勘误/注解（evidence-backed）

- **「无 relay 配给被拒」的仓内落点 = 三层**：① `assert_audit_scope_provisioned`（新 gate 拒绝点，`Error::Forbidden` 403，E8）；② `aero-cli audit-provision-check` 非零退出（E6 落家）；③ connector 403→dead ≤1（E7 已实现，回归保持）。仓外 IdP registry 本体（proposal :11）不在本仓——本 direction 交付「验证 seam + fail-closed 门」，与 B5-2 spec 的 in-repo 钉死读法一致。
- **「boot 门已验证存在」= v1 商业开关一致性门**（E4 `from_env` 的 require/configure_disabled），**不是**配给门——本 direction 新增的是配给验证 seam，不动既有 boot 门。
- **「readyz 不翻转」是约束不是缺口**：`probe_commercial` 明确「reachable central service is intentionally not a probe dependency」（health.rs:67-70 注释）——本 direction 必须**保持** readyz 语义（§4 R6 回归钉死），配给状态只走 CLI/拒绝点/（可选）健康体信息字段。
- **心跳语义钉死为「成功 settle」而非「轮询存活」**：`dispatch_batch` 的 Ok 只表示 claim 泵活着（`relay.rs` 逐行错误全被吸收，batch 仍 Ok），403-loop 的 relay 也会 Ok——「relay works」必须 = **至少一次 fenced `settle` 成功**（投递真实成功），否则 403-loop 会把门一直开着，与 T-11 fail-closed 相悖。安静期无事件 → 无 settle → 门在 freshness 窗口后关闭（只影响配给验收，**从不阻塞投递**，事件到达后自愈）——行为写入 §7。
- **「without the gate, audit-scoped machine issuance/acceptance is indistinguishable…」**：仓内现状 = 无任何 audit-scope 机器令牌验收路径（E2），v1 触发器只查 enabled 不查 relay 健康（E3）——门补齐的是**可观测、可执行的配给判定**，而非新入站端点（入站端点不在本 direction 范围，§3）。

## 2. Verified current state

```
gate 缺席现状（三条独立事实，互不相认）：
a) 入队面（E3）  audit_events AFTER INSERT → aero_enqueue_snaplink_audit → snaplink_delivery_outbox
                 └─ 唯一门 = snaplink_commercial_runtime.enabled（无 relay 健康参与）
b) 投递面（E7）  main.rs:245-269：RelayConfig::from_env presence-gated → AuditRelay（B5-2 connector）
                 └─ 403→dead ≤1（T-11 已实现）；无心跳/配给状态；不 consult 任何 gate
c) 验收面（E2）  AuthUser extractor：JWT→PAT→bot（无机器身份）；无 audit-scope 机器令牌验收路径
                 aero-cli（E6）：15 命令，无 audit-provision-check

AuthService builder-hook 模式（E1，本 direction 复制的模板）：
  Option<Arc<T>> 字段（:78，new() 置 None = opt-in 默认关）
  → #[must_use] with_*  builder（:102/:134/:146）
  → 生命周期 hook（:116 sweep_login_throttle；驱动 boot/serve.rs:203）
  → boot 装配（boot/services.rs:71，env 门控）

readyz（E4）：probe_commercial :67-77 → health_ready :154-199 matches!("ok"|"disabled")
  —— relay 健康从不翻 readyz（约束，本 direction 保持）
```

**Gaps this direction closes**（all verified）：① `audit-provision-check` seam 无家（E5/E6）——落家 = `crates/aero-server/src/bin/aero-cli.rs` 第 16 命令；② 无「relay 已验证」的持久事实（E7 无心跳）——新增 durable heartbeat 行（0240 迁移 + `AuditRelayProvisionRepo`）供 gate/CLI 判定；③ `AuthService` 无配给门——按 E1 模式新增 `relay_provision_gate` builder-hook + `assert_audit_scope_provisioned` fail-closed 拒绝点（403，E8）；④ v1 触发器/投递面与配给状态互不相认（E3/E7）——本 direction 明确 gate 的**唯一消费面**（验收/CLI），入队与投递永不 consult gate（R7，AC3 钉死）。

## 3. Scope

**In scope（B5-4，effort 5 的完整切片）**：
- aero-auth：`src/relay_gate.rs`（新模块）——`RelayProvisionGate` trait + `SharedRelayProvisionGate` + `AuthService.relay_provision_gate` 字段/builder/lifecycle + `assert_audit_scope_provisioned` 拒绝点 + 具体 `PgRelayProvisionGate`（读 durable 心跳，`freshness` 构造参数）；`lib.rs` re-export。**零 extractor 改动**（无机器身份路径存在，不新增入站端点）。
- 迁移 `0240_audit_relay_provisioning.sql`（幂等、singleton 心跳行）+ `aero-storage/src/audit_relay_provision.rs`（`AuditRelayProvisionRepo`：`record_heartbeat` / `verified_at` / `provision_check`）。
- aero-server：`main.rs:245-269` 的 relay 装配处加 `HeartbeatOutboxRepo` 装饰器（**成功 fenced settle 时记录心跳**，委托其余；零 connector 改动——connector 无 aero-storage 依赖）；`boot/services.rs::build` 在 connector 配置存在时 `.with_relay_provision_gate(…)`（镜像 :71 的 env 门控接线）；`aero-cli audit-provision-check` 子命令（16 命令 + completion 列表）。
- 配置面：`AERO_AUDIT_PROVISION_FRESHNESS_SECS`（默认 300s，单下划线 `AERO_AUDIT_*` 家族，E7 connector 先例）——进 `RelayConfig`（connector 一个解析点），gate/CLI 经 boot/CLI 读 config 传入。
- 测试：aero-auth 单测（无 DB）+ aero-storage PG `#[ignore]` db_tests + `scripts/test-integration.sh` 命名条目（throwaway DB + 退出码断言，`run_migrated_integration` 模式 :153-165）。

**Out of scope（并行切片 / 其他模块——勿在本 direction 建造）**：
- 入站 audit-scope 机器令牌 HTTP 验收端点 / `AuthUser` 加机器身份 kind → **不存在，也不新建**；本 direction 只交付 `assert_audit_scope_provisioned` 拒绝点 + 未来路径必须调用的契约（R8）。
- 0239 治理 outbox DDL + class/priority 戳 + `claim_due ORDER BY priority DESC` + 反饥饿上限 → **B5-1（storage/迁移）/ B5-3（aero-storage）**；`admin.content.flag`/`admin.moderation.action` 映射 token 清单（仓外契约）→ **B5-3/契约决策**。
- 403/scope-deficient → dead ≤1 的 connector 状态机语义 → **B5-2 已实现**（E7），本 direction 只回归保持 + 消费其 T-11 行为。
- v1 `snaplink_commercial/` 运行时、0236 触发器、usage relay → **原地不动**（B5-2 A6 同款 no-touch 守卫）。
- IdP scope registry 本体（proposal :11 "registry 本体在 IdP 仓"）→ 仓外，本仓只交付验证 seam。

## 4. Requirements

### R1 — Gate seam on `AuthService`（builder-hook 模式，fail-closed 默认）
新模块 `crates/aero-auth/src/relay_gate.rs`，逐点复制 E1 模式：
- `#[async_trait] pub trait RelayProvisionGate: Send + Sync { async fn verified(&self) -> bool; }`——`true` 仅当 durable 心跳存在且新鲜（`now - verified_at ≤ freshness`）；**任何异常（行缺失/过期/读错）→ `false`**（fail-closed，绝不 "verified on error"）。
- `pub type SharedRelayProvisionGate = Arc<dyn RelayProvisionGate>`（`pat.rs`/`bot.rs` 同款 `Shared*` 惯例）。
- `AuthService.relay_provision_gate: Option<SharedRelayProvisionGate>`；`new()` 置 `None`（:93 同款——**未注入 = fail-closed 拒绝**）；`#[must_use] pub fn with_relay_provision_gate(mut self, gate: SharedRelayProvisionGate) -> Self`（:102 同款签名）。
- 生命周期 hook `pub async fn refresh_relay_provision(&self)`：`None` 时 no-op（:116 同款语义）。**无进程内 sweep**——与 `login_throttle` 不同，gate 状态是 durable + 时间有界（freshness），无需驱逐 per-key map；心跳记录（R3）即生命周期驱动（偏差有文档，§7）。
- 拒绝点 `pub async fn assert_audit_scope_provisioned(&self) -> aero_common::Result<()>`：`None` → `Err(Error::Forbidden("audit:event:write is not provisioned: relay-provisioning gate is not installed (T-11)"))`；`verified()==false` → `Err(Error::Forbidden("audit:event:write is not provisioned: no fresh relay heartbeat (T-11)"))`；否则 `Ok(())`。**403 规范状态码**（E8）。这是未来任何 audit-scoped 机器令牌验收路径的唯一必过闸（R8）。
- 具体实现 `PgRelayProvisionGate { repo: aero_storage::AuditRelayProvisionRepo, freshness: time::Duration }`——`verified()` = `provision_check(freshness)` 为 `Verified`。aero-auth → aero-storage 依赖已存在（`Cargo.toml:14`），无新增依赖。

### R2 — Durable heartbeat 状态（迁移 0240 + 仓储）
- `migrations/0240_audit_relay_provisioning.sql`（幂等 `CREATE TABLE IF NOT EXISTS`）：
  ```sql
  CREATE TABLE IF NOT EXISTS audit_relay_provisioning (
      singleton        BOOLEAN      PRIMARY KEY DEFAULT TRUE CHECK (singleton),
      verified_at      TIMESTAMPTZ  NOT NULL,
      provision_state  TEXT         NOT NULL DEFAULT 'provisioned',
      updated_at       TIMESTAMPTZ  NOT NULL DEFAULT clock_timestamp()
  );
  ```
- `crates/aero-storage/src/audit_relay_provision.rs`（AGENTS.md「每功能一个 XRepo」）：`AuditRelayProvisionRepo { pool }`：
  - `record_heartbeat(&self) -> Result<(), sqlx::Error>`——singleton UPSERT：`verified_at = clock_timestamp()`；
  - `verified_at(&self) -> Result<Option<OffsetDateTime>, sqlx::Error>`；
  - `provision_check(&self, freshness: Duration) -> Result<ProvisionCheck, sqlx::Error>`——`Verified(ts)` / `NotVerified(None)` / `Stale(ts)`（仓储不做 env 解析，freshness 由调用方传入；DB 错误向上传播，调用方按 fail-closed 处理）。
- `lib.rs` `pub mod` + `pub use`（token helper 撞名规则不涉此模块）。

### R3 — 心跳记录：成功 settle 即「relay works」（aero-server 装饰器，零 connector 改动）
- `crates/aero-server` 新装饰器（如 `audit_relay_heartbeat.rs`）`HeartbeatOutboxRepo`：持 `inner: Arc<dyn aero_audit_connector::outbox::OutboxRepo>` + `AuditRelayProvisionRepo`；实现 `OutboxRepo` trait，**仅 `settle` 返回 `Ok(true)`（fenced 成功）时 `record_heartbeat()`**，其余方法纯委托（claim_due/requeue/mark_dead 不碰心跳——失败/死行不刷新，403-loop 的 relay 门保持关闭）。
- 接线：`main.rs:245-269` relay 装配处把 `PgOutboxRepo` 包进装饰器再交 `AuditRelay`（`Arc<dyn OutboxRepo>` 已是 trait object，无签名变更）。
- **connector crate 零改动**（无 aero-storage 依赖，E7）——B5-2 的 A3/A4 测试与 drill 原样保持绿。

### R4 — `aero-cli audit-provision-check` 落家（`crates/aero-server/src/bin/aero-cli.rs`）
- 第 16 命令，薄壳包 `AuditRelayProvisionRepo::provision_check` + connector 配置存在性；completion `cmds` 串（:685）同步加。
- 退出码契约（**非零 = 未配给，fail-closed**）：
  1. `RelayConfig::from_env()` 为 `Ok(None)`（无 `AERO_AUDIT_TOKEN_ENDPOINT`）→ 打印 "audit relay is not configured" → **exit 1**；
  2. `Err`（AERO_AUDIT_* 半配置，fail-loud）→ **exit 1**；
  3. `Ok(Some(_))` + `provision_check` = `NotVerified`/`Stale` → 打印原因（含 `verified_at`，若有）→ **exit 1**；
  4. `Verified(ts)` → 打印 `verified_at` + freshness → **exit 0**；
  5. 仓储 DB 错误 → 打印错误 → **exit 1**（错误绝不当作已验证）。
- 判定逻辑全部在仓储/测试层，bin 只做 IO 与退出码——退出码可被 shell 测试断言。

### R5 — Boot 装配 + 配置面
- `boot/services.rs::build`：`RelayConfig::from_env()?.is_some()` 时 `auth = auth.with_relay_provision_gate(Arc::new(PgRelayProvisionGate::new(deps.pg.clone(), cfg.provision_freshness)))`（镜像 :71 接线；`from_env` 已被 main.rs 调用，幂等纯 env 读）；**未配置 → 不注入 → None → fail-closed**（R1）。
- `RelayConfig` 增字段 `provision_freshness: Duration`（`AERO_AUDIT_PROVISION_FRESHNESS_SECS`，默认 300，界 [60, 86400]，解析校验进 `config.rs` 既有 presence-gated 家族——`AERO_AUDIT_*` 半配置无 token endpoint 仍 boot Err，E7 不变量保持）。
- 顺序约束：gate 构造在 services 装配（main.rs:88 前），relay 心跳在 main.rs:245 后——互不依赖，顺序无关。

### R6 — readyz 不翻转（回归钉死）
- `routes/health.rs` 的 `readiness_decision`/`probe_commercial`/`health_ready` **零改动**：readyz 判定只依赖既有五探针（E4），配给状态**永不参与** `deps_ok`。
- 可选（不强制）：`/health` 体增信息字段（如 `audit_relay_provisioning: "verified"|"not_provisioned"|"stale"`）供运维观察——**只进 body，不进 readyz**；若实现，`probe_commercial` 旁新增同名探针函数，`deps_ok` 逻辑不动。
- 回归守卫：`readiness_decision` 既有单测保持绿 + 新单测断言「gate 缺席/不健康时 readyz 判定与 gate 无关」。

### R7 — Gate 的消费面边界（moderation 不阻塞的根约束）
Gate（`assert_audit_scope_provisioned` + CLI）是配给验收的**唯一**消费面。以下路径**永不 consult gate**：
- 入队面：0236/0239 触发器（SQL，E3）——audit 行 → outbox 行同事务，gate 状态无关（moderation 行照常入队）；
- 投递面：connector `AuditRelay` 状态机（E7）——claim/settle/requeue/dead 不读 gate（relay 自证：首次成功 settle 即建立心跳，见 §7 chicken-and-egg 注解）；
- 排序面：B5-3 `claim_due ORDER BY priority DESC`——不读 gate。
**可测试推论**：gate 不健康（心跳缺失/过期）时，(a) `message.moderated` 仍同事务产出治理行；(b) 500 积压 + 1 moderation 的 B5-3 drill 仍先 claim moderation 行；(c) relay 仍照常投递（stub 202 → settle；stub 5xx → transient requeue；绝无 gate 引起的 dead）。

### R8 — 未来机器令牌验收路径的必过闸（契约声明）
任何未来新增的 audit:event:write-scoped 机器令牌验收点（新入站端点 / `AuthUser` 扩展 / 内部 API）**必须**在签发/接受前调用 `assert_audit_scope_provisioned()`（403）或等价 gate 判定——否则违反 T-11「无 relay 配给被拒」。本 direction 不实现该路径，只钉契约 + 提供单一测试过的拒绝点（AC1.1）。

## 5. Acceptance checks（direction 原样保留，逐条 testable）

PG 测试沿用既有 db_test harness（`#[tokio::test] #[ignore = "requires live Postgres"]`，throwaway 已迁移 DB，`scripts/test-integration.sh` 的 `run_migrated_integration` 命名条目模式 :153-165）。**迁移落库顺序**：加 0240 后**必先 `cargo build` 再 `aero-cli migrate`**（AGENTS.md §4.2——迁移编译期嵌入）。

### AC1（T-11）— gate 缺席/不健康（无 relay 已验证）：audit:event:write-scoped 机器令牌被拒（401/403），`aero-cli audit-provision-check` 非零退出
**AC1.1 — 拒绝点单测（aero-auth，无 DB）**：三态参数化——(a) 未注入 gate（`AuthService::new` 裸构造）→ `assert_audit_scope_provisioned` 返回 `Err(Error::Forbidden)`（403，E8）；(b) 注入恒 `false` gate → 同 `Err(Forbidden)`；(c) 注入恒 `true` gate → `Ok(())`。断言错误消息含 "not provisioned"。
**AC1.2 — CLI 退出码（shell，throwaway DB，`scripts/test-integration.sh` 命名条目）**：`migrate` 后五子例——① 无 `AERO_AUDIT_TOKEN_ENDPOINT` → exit ≠ 0 且输出含 "not configured"；② 有配置无心跳行 → exit ≠ 0 且含 "no verified relay heartbeat"；③ 心跳过期（`verified_at = now - 2×freshness` 手工 UPDATE）→ exit ≠ 0 且含 "stale"；④ 心跳新鲜 → exit 0 且输出含 `verified_at`；⑤ PG 不可达/表缺失 → exit ≠ 0（DB 错误不误报 verified）。
**AC1.3 — 投递侧 T-11 回归保持（B5-2 已实现，E7）**：`cargo test -p aero-audit-connector` 全绿（含 `forbidden_dead_on_first_attempt`/scope-deficient → dead ≤1、无 retry loop）；`scripts/test-integration.sh` A3 drill（:236-270）保持绿。**因果链钉死**（B5-2 spec §8.3）：gate 不健康 ⇒ IdP 拒配给 ⇒ token endpoint 403 ⇒ connector dead ≤1——本 direction 不改状态机，只让「未配给」在仓内可观测（AC1.1/AC1.2）。

### AC2 — 已验证 relay 心跳后：配给放行，matrix 端到端跑 403-free
**AC2.1 — 心跳建立（drill 扩展）**：throwaway DB + stub sink（202 回执）→ 跑 relay 投递 N 行全部 settle → 断言 `audit_relay_provisioning.verified_at` 存在且新鲜（≤ freshness）；随后 `aero-cli audit-provision-check` **exit 0**（AC1.2 ④ 同一条目顺序执行）。
**AC2.2 — matrix 端到端 403-free（in-repo 钉死读法 = `test-integration.sh` A3 drill 扩展）**：gate 已装且心跳新鲜的前提下跑完整 drill——(a) `COUNT(status=2) == N`；(b) event_id set-parity（无孤儿/无重复）；(c) **零 403 面**：stub 侧无任何 403 响应（stub 默认 202，断言无 `DeliveryError::Forbidden`/`Unprovisioned` 分类、无行 `last_error` 含 403）——「矩阵配给后端到端无 403」的仓内断言面。
**AC2.3 — 回归**：AC1.2 ④ 与 AC2.1 同一条目（先投递后检查），防止「检查通过但心跳从未建立」的假绿。

### AC3（moderation 优先级 [PROPOSED]，B5-3 交叉切片）— gate 不阻塞：relay 健康劣化时 moderation 行照常入队/优先/投递
**AC3.1 — 入队不阻塞**：gate 不健康（无心跳行）时，`message.moderated` 审计行仍同事务产出治理 outbox 行（B5-1 A2 parity db_test 在 gate 安装且不健康的进程态下原样绿：恰 1 audit 行 + 1 outbox 行、event_id 1:1、rollback 双双归零）。
**AC3.2 — 优先不阻塞（依赖 B5-3 claim 排序，以 0239 + priority 列存在为门，`test-integration.sh` 既有 SKIP 模式 :250-270）**：500 积压 + 1 条经 R6 fixture 产出的 moderation 行（class='admin'、HIGH 优先级、映射出站 token）→ gate 不健康下 `claim_due` 仍先返回 moderation 行；K 批后积压持续排空（反饥饿上限语义不变）。
**AC3.3 — 投递不阻塞**：心跳过期（gate 不健康）时 relay 仍照常 claim 与投递——stub 202 → settle（行 status=2，心跳随即重建）；stub 5xx → transient requeue（非 gate 引起的 dead）——与 AC1.3 同套件并跑。
**AC3.4 — 消费面边界回归**：`git diff` 守卫——本 direction 变更集**不含** `migrations/0236_*`、`crates/aero-audit-connector/src/relay.rs`、`snaplink_commercial/runtime.rs`、`routes/health.rs::readiness_decision`（gate 零参与证据）。

## 6. Test placement

| Test | Location | Harness |
|---|---|---|
| AC1.1 拒绝点三态（None / false / true gate → Forbidden/Ok） | `crates/aero-auth/src/relay_gate.rs` tests（或 service.rs tests 同款） | 纯单测，无 DB（fake gate） |
| `provision_check` 三态 + `record_heartbeat` UPSERT 幂等（Verified/NotVerified/Stale，重复心跳刷新） | `crates/aero-storage/src/audit_relay_provision.rs` db_tests | PG `#[ignore]` |
| AC1.2 CLI 退出码五子例 + AC2.1 心跳建立 + AC2.3 先投递后检查 | `scripts/test-integration.sh` 命名条目（`run_migrated_integration` 模式 :153，throwaway DB + `--test-threads=1`） | shell |
| AC2.2 matrix 403-free（N settle、parity、零 Forbidden 断言） | `scripts/test-integration.sh` A3 drill 扩展（沿用 :236-270 结构） | shell + stub sink |
| AC3.1 入队不阻塞（gate 不健康下 parity 绿） | 复用 B5-1 A2 parity db_test（跨切片同套件，0239 门控） | PG `#[ignore]` |
| AC3.2 500 积压 + moderation 先 claim（gate 不健康） | B5-3 claim 排序 db_test 扩展（0239 + priority 门控） | PG `#[ignore]` |
| AC3.3 投递不阻塞（心跳过期仍 settle/requeue） | connector relay 测试 + drill（A3） | 无 DB（fake/stub）+ shell |
| AC3.4 no-touch 守卫（gate 零参与） | review gate + `git diff --stat` | 人工/CI |
| R6 readyz 不翻转回归 | `routes/health.rs` 既有 `readiness_decision` 单测 + 新「gate 无关」断言 | 单测 |

## 7. Risks / [PROPOSED] / 决策点

- **心跳语义：settle 成功 vs 轮询存活**（本 spec 钉 settle）。`dispatch_batch` Ok 只证明 claim 泵存活（relay.rs 逐行错误吸收）；403-loop 的 relay 也会 Ok——若按轮询记心跳，门在配给被拒时保持开着，与 T-11 相悖。settle 语义的代价 = 安静期（无事件）门会过期关闭——**只影响配给验收，从不阻塞投递**（R7），事件到达并成功投递后自愈（AC3.3 顺带证明）；freshness 默认 300s ≫ poll 5s，抖动窗口可控。
- **chicken-and-egg 注解**：relay 自己的首次投递先于任何心跳存在——「先 settle 后 verified」是设计内顺序（relay 自证），gate 不拦 relay 自己的投递（R7 投递面不 consult gate）；被 gate 拦的是**其他** audit-scoped 机器令牌验收（R8 未来路径）+ 运维检查（AC1.2）。staging 须验证该顺序符合 IdP 侧「先配给后投递」的编排（AGENTS.md §4.5 真实凭据面）。
- **`admin.content.flag`/`admin.moderation.action` 精确 token 与 500 积压 drill 细节**：仓外契约文本（proposal :13 [PROPOSED]；B5-1 spec R1 已按同一 token 清单钉 in-repo 常量）——AC3 断言对 B5-1 R1 常量比较，不依赖仓外清单落库；0239/priority 列未落地时 AC3.1/AC3.2 按既有 SKIP 模式（test-integration.sh :250-270 先例）跳过，AC1/AC2 不依赖 0239 **必须全绿**（0240 独立迁移）。
- **lifecycle hook 形态偏差**（R1）：gate 无进程内 sweep（与 `sweep_login_throttle` 不同）——状态 durable + freshness 有界，sweep 无对象；`refresh_relay_provision` 保留为显式刷新/未来缓存位（当前 no-op），避免将来 TTL 缓存化时改公共面。
- **freshness 并入 `RelayConfig` 的连带**：`AERO_AUDIT_PROVISION_FRESHNESS_SECS` 属 `AERO_AUDIT_*` presence-gated 家族——无 token endpoint 时设置它 = boot Err（E7 不变量），文档化即可，无行为回退。
- **v1 relay（snaplink_commercial）不参与本门**：v1 的投递健康仍由既有 projection readyz/backlog gauge 观察（E4），本 gate 只对 B5-2 connector（v2）建模——v1/v2 cutover 期间两套并存，门覆盖 v2 配给面；若契约要求 v1 也进 gate，是后续决策（本 direction 不扩）。
- **「matrix end-to-end」的仓内钉死读法**：全量矩阵（跨仓 IdP/vault/sink）在仓外（proposal :13）；AC2.2 钉 in-repo 面 = A3 drill 扩展（N settle + parity + 零 403 断言），与 B5-2 spec 的 "37/37 in-repo 子集" 同法。

## 8. Sequencing

1. **迁移 + 仓储**：`0240_audit_relay_provisioning.sql` + `AuditRelayProvisionRepo`（**先 `cargo build` 再 migrate**，AGENTS.md §4.2）+ db_tests。
2. **aero-auth seam**：`relay_gate.rs`（trait + builder + `assert_audit_scope_provisioned` + `PgRelayProvisionGate`）+ lib.rs re-export + AC1.1 单测。
3. **connector 配置**：`RelayConfig.provision_freshness` + 校验（config.rs 既有家族）。
4. **aero-server 接线**：`HeartbeatOutboxRepo` + main.rs relay 装配包裹 + `boot/services.rs` gate 注入；R6 回归（health.rs 不动）。
5. **CLI**：`aero-cli audit-provision-check`（bin + completion 列表）+ `test-integration.sh` 命名条目（AC1.2 五子例 + AC2.1/AC2.3）。
6. **drill 扩展**：AC2.2 零-403 断言进 A3 drill。
7. **交叉切片（0239/priority 落库后）**：AC3.1/AC3.2 门解开（B5-1 A2 parity + B5-3 claim 排序在 gate 不健康态下跑）。
8. **门禁**：`cargo check --workspace`（干净）· `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check}.sh` · `scripts/test-integration.sh`（AC1/AC2 全绿）· AC3.4 no-touch 守卫。

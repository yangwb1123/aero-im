# Design — B5-4 scope provisioning：`audit:event:write` 仅 relay 可用时配给，无 relay fail-closed（Option-verifier seam）

- **Direction**: "Scope provisioning (B5-4): encode 'grant `audit:event:write` only after the relay works; fail-closed without relay' as an auth capability via aero-auth's Option-verifier seam"（value 8 / risk_reduction 7 / effort 4 / confidence 7）
- **Requirements**: `docs/requirements/2026-08-08-aero-auth-b5-4-scope-provisioning.req.md`（R1–R6, AC1–AC5）
- **Status**: Design（证据全部复核；三处新事实补正见 §0.2——依赖环约束、`AuditRelay` 构造面、测试计数）
- **Verification date**: 2026-08-08。行号为复核锚点，会漂移——**文件/符号**为准（AGENTS.md §0）

## 0. Evidence adjudication（untrusted claims → verified anchors）

### 0.1 Requirements-spec 引用逐条复核（2026-08-08 现场 grep）

| Evidence claim | Verdict | Verified anchor（实际） |
|---|---|---|
| E1 `service.rs` `pat_verifier`/`bot_verifier` Option seam + builder | ✅ 符号精确（行微漂） | `crates/aero-auth/src/service.rs`：字段 :66/:73；`new()` 置 `None` :85-95（None = disabled 注释原文）；`#[must_use] with_pat_verifier` :134、`with_bot_verifier` :152；`pat_and_bot_token_gates_are_disjoint` :701 |
| E2 `jwt.rs` `Claims` 无 scope | ✅ 精确 | `Claims` :36-52：sub/iss/iat/exp/kind/sid/jti，无 scope；测试 `issue_and_verify_roundtrip` :295、`session_pair_shares_sid_but_not_jti` :414 |
| E3 `extractor.rs` `AuthUser` 无 scopes | ✅ 精确 | `AuthUser` :34-40：participant_id/session_id/exp |
| E4 `oidc.rs` `required_scopes` 强制点 | ✅ 语义不变（行漂移已由 req 勘误） | struct `ClientCredentialsTokenConfig` :378-381（`required_scopes` :381）；`validate_client_credentials_token` :420，all-or-nothing 检查 :476-483（`granted_scopes()` 缺失任一 → Invalid "token lacks a required application scope"）；测试在 `oidc/tests.rs:398` |
| E5 `aero-eng/src/audit_provision.rs` Q0 + Verdict | ✅（文案行号 :139 非 :141，漂移 2） | `Q0_SQL` :34（`runtime.enabled` + enabled bindings count，注释 "deliberately not `ready()`"）；`Verdict` :110-116；`verdict()` :125；dead-priority :128-135；FailClosed 文案 "no audit:event:write grant issued" :139 |
| E6 `config.rs` `AERO_AUDIT_EXPECTED_SCOPE` | ✅（行微漂） | `expected_scope` 字段 :35；`from_env` 默认 `"audit:event:write"` :119-120；`client.rs::SCOPE_AUDIT` :43；全仓 `audit:event:write` 字面量不在 aero-auth（单源在 connector） |
| E7 `relay.rs` claim_due/requeue 循环 | ✅ 精确 | `AuditRelay` :84；`dispatch_batch` :164（reconcile → `claim_due` :171 → `for_each_concurrent deliver_claim` :176）；403→`mark_dead` 立即 :201-216（"Deliberately NOT `is_dead_at`" 注释原文） |
| E8 `aero-eng/tests/audit_provision.rs:43` | ✅ **精确命中** | :43 = `assert!(reason.contains("no audit:event:write grant issued"));`（:42 还断言 "relay disabled"） |
| E9 `state_machine.rs` 无 no-claim-when-disabled case | ✅ 确认（8 用例，无该用例） | `stale_token_cannot_ack_after_reclaim` / `backoff_is_bounded_and_exponential` / `permanent_error_dead_after_exactly_two_attempts` / `forbidden_dead_on_first_attempt` :255（403→Dead 立即）/ `happy_path_settles_and_removes_from_claimable` / `skew_gt_lease_cannot_livelock_claim_fence_settle` / `priority_first_claim_preempts_fifo_and_limit1_keeps_top_lane` / `signature_rejected_dead_after_exactly_two_attempts`。`fake.rs` `FakeStatus::Ready => 0` :42；`FakeRowSnapshot{status, claim_token}` :52-65；行访问器 `fake.row(id)` :175 |
| E10 `Error::Forbidden` = 403 + connector→aero-auth 依赖 | ✅ 精确 | `aero-common/src/error.rs`：variant :19、HTTP 403 :70、code "forbidden" :85；`aero-audit-connector/Cargo.toml:14` `aero-auth.workspace = true` |
| AC2 第二锚点 `claim_validation.rs:143` | ✅ 精确 | `missing_audit_scope_is_rejected_before_any_post` :143（`assert_eq!(posts, 0)` :146 附近） |
| 迁移 0235 含 Q0 表 | ✅ | `migrations/0235_snaplink_commercial_control_plane.sql` 含 `snaplink_commercial_runtime`/`bindings`（grep 命中） |
| sibling req/design 存在 | ✅ | `docs/requirements/2026-08-07-aero-auth-b5-4-relay-provisioning-gate.req.md` + `docs/design/2026-08-07-aero-auth-b5-4-relay-provisioning-gate.design.md` |

### 0.2 新事实（req 未覆盖，本设计补正）

- **依赖环约束 → Q0 实现不能落 aero-storage trait impl**：`aero-auth/Cargo.toml:14` 依赖 `aero-storage` → aero-storage 反向依赖 aero-auth 即成环。sibling 设计已用同款解法：**trait + Pg impl 都放 aero-auth，aero-storage 只放裸 SQL repo**（`PgRelayProvisionGate { repo: aero_storage::AuditRelayProvisionRepo }`，`docs/design/2026-08-07-…gate.design.md` §2.3）。本设计镜像（§2.1/§2.2）。
- **`AuditRelay` 构造面 = 6 处**（新增门字段的编译强制面）：生产 `aero-server/src/bin/main.rs:258`；connector 3 个 drill（`bin/aero-audit-relay-drill.rs:127`、`bin/aero-audit-t11-drill.rs:155`、`bin/aero-audit-priority-drill.rs:203`）；`relay.rs` 内部测试 :374/:565；`tests/state_machine.rs` helper `relay()` :65。`AuditRelay` 的 `Debug` 是手写 `finish_non_exhaustive`（:90-96）——加字段不破坏。
- **生产接线两点**：`AuthService` 在 `boot/services.rs::build` 构造（pat/bot 链 :59-64），`main.rs:185` 把 `services.auth` **move** 进 state（之后不可 builder）→ AuthService seam 必须在 boot 构造时接线；relay 在 `main.rs:245-269` 的 `Ok(Some(relay_cfg))` 臂装配。`Services` struct（boot/services.rs:17-23）是两者间传递 provisioner 的现成通道。
- **「既有 22 项测试」是漂移计数**：connector 现测 = state_machine 8 + claim_validation 10 + relay.rs 内部 5 = **23**（AGENTS §0：计数现场取，`cargo test -p aero-audit-connector` 实测为准）。
- **`Q0_SQL` 是 aero-eng 私有 const**（:34，无 `pub`）；aero-auth 不能依赖 aero-eng（"零新增依赖"约束 + 语义倒挂）→ Q0 SQL 在 aero-storage repo 内**复制**并交叉注释（§7 决策点 D2）。
- **truth-check 语义**：unwired builder = ⚠️ 警告不计 exit 码（`scripts/truth-check.sh` §2）；`.with_relay_scope_provisioner(`（boot/services.rs）与 `.with_scope_provisioner(`（main.rs）均有调用点 → 不新增警告。

## 1. Design overview

```
                    ┌─────────────────── 生产接线（aero-server）───────────────────┐
                    │ boot/services.rs::build          main.rs:245-269            │
                    │ RelayConfig::from_env()?         Ok(Some(relay_cfg)) 臂      │
                    │  Ok(Some) → 构造 RelayScopeRepo   PgOutboxRepo ─┐            │
                    │   → PgRelayScopeProvisioner        AuditRelay::new(..)      │
                    │   → Services.relay_scope_           .with_scope_             │
                    │      provisioner (Option)            provisioner(p)          │
                    │   → AuthService.with_relay_scope_      └─ claim 边界门        │
                    │      provisioner(p)                      （dispatch_batch 顶） │
                    │            │                                │                 │
                    │            ▼                                ▼                 │
                    │  assert_audit_scope_provisioned   dispatch_batch:            │
                    │   （403，未来机器端点必过闸）       未配给 → Ok(0) 不 claim     │
                    └──────────────────────────────────────────────────────────────┘
                    ▲                                  ▲
   aero-auth（seam 的家）                        aero-audit-connector（消费方）
   src/relay_scope.rs:                           relay.rs（仅 additive）:
     trait RelayScopeProvisioner                    field: Option<SharedRelayScopeProvisioner>
     SharedRelayScopeProvisioner                    #[must_use] with_scope_provisioner
     PgRelayScopeProvisioner                        dispatch_batch 顶部门（deliver_claim 零改动）
     AuthService.relay_scope_provisioner
        （new() 置 None = fail-closed）
                    ▲
   aero-storage（裸 SQL，无 aero-auth 依赖——环安全）
   src/relay_scope.rs: RelayScopeRepo.q0_provisioned()  （Q0_SQL 镜像，DB Err 上抛）
```

**门的三层语义**（与 req §1.1/§3 一致）：① **claim 边界**（本方向主闸）——`AuditRelay::dispatch_batch` 未配给时不 claim，outbox 行保持 status 0；② **auth 能力闸**——`AuthService::assert_audit_scope_provisioned` 403 拒绝点（现状无入站机器端点，契约钉未来路径）；③ **既有 403→dead**（T-11，已实现）回归保持。**注入门 = 静态 boot 判定**（`RelayConfig::from_env() == Ok(Some(_))` → 注入）；**配给判定 = 每次消费时复评 Q0 谓词**（DB 错误 → false，fail-closed；不做 sibling 的心跳表/freshness 机制——边界见 §7 D6）。

## 2. API changes（逐 crate 签名）

### 2.1 `crates/aero-auth/src/relay_scope.rs`（新模块，lib.rs `pub mod` + re-export）

```rust
/// Grants the `audit:event:write` capability. `true` only when the relay
/// provisioning predicate actually holds; ANY anomaly (predicate read error /
/// DB error / unprovisioned) → `false` (fail-closed — never "provisioned on
/// error"). Mirrors the pat/bot verifier seam shape (E1).
#[async_trait]
pub trait RelayScopeProvisioner: Send + Sync {
    async fn audit_event_write_provisioned(&self) -> bool;
}
pub type SharedRelayScopeProvisioner = Arc<dyn RelayScopeProvisioner>;

/// Q0-backed impl (sibling-consistent: Pg impl lives in aero-auth, raw SQL in
/// aero-storage — aero-auth → aero-storage dep already exists, no cycle).
pub struct PgRelayScopeProvisioner { repo: aero_storage::RelayScopeRepo }
impl PgRelayScopeProvisioner {
    pub fn new(pool: sqlx::PgPool) -> Self;
}
#[async_trait]
impl RelayScopeProvisioner for PgRelayScopeProvisioner {
    // matches!(repo.q0_provisioned().await, Ok(true)) — Err → false (fail-closed)
    async fn audit_event_write_provisioned(&self) -> bool;
}
```

`AuthService` 增量（逐点复制 E1 模式，`service.rs`）：

```rust
relay_scope_provisioner: Option<SharedRelayScopeProvisioner>,  // new() 置 None（未注入 = 能力关闭 = fail-closed）
#[must_use] pub fn with_relay_scope_provisioner(mut self, p: SharedRelayScopeProvisioner) -> Self
/// 403 规范状态码（aero-common::error.rs:70）。任何 audit-scoped 机器路径
/// （现状无入站端点；未来端点）的必过闸。
pub async fn assert_audit_scope_provisioned(&self) -> aero_common::Result<()> {
    match &self.relay_scope_provisioner {
        None => Err(Error::Forbidden("audit:event:write is not provisioned: \
            relay-scope provisioner is not installed (T-11)".into())),
        Some(p) if !p.audit_event_write_provisioned().await => Err(Error::Forbidden(
            "audit:event:write is not provisioned: relay provisioning predicate not satisfied (T-11)".into())),
        Some(_) => Ok(()),
    }
}
```

`lib.rs`：`pub mod relay_scope;` + `pub use relay_scope::{PgRelayScopeProvisioner, RelayScopeProvisioner, SharedRelayScopeProvisioner};`（无 root re-export 撞名——§4.2 token-helper 规则不适用，符号唯一）。

**零改动**：`jwt.rs`（Claims 无 scope，R3）、`extractor.rs`（AuthUser 无 scopes，R3）、`oidc.rs`（required_scopes 语义不动，R5）、`pat.rs`/`bot.rs`。

### 2.2 `crates/aero-storage/src/relay_scope.rs`（新模块，lib.rs `pub mod` + re-export）

```rust
/// 每功能一个 XRepo（AGENTS §4.1）。裸 SQL，不依赖 aero-auth（环安全）。
pub struct RelayScopeRepo { pool: PgPool }
impl RelayScopeRepo {
    pub fn new(pool: PgPool) -> Self;
    /// Q0 relay-health 谓词：snaplink_commercial_runtime.enabled AND
    /// enabled bindings > 0。语义镜像 aero_eng::audit_provision::Q0_SQL（:34，
    /// "deliberately not ready()"）。DB Err 上抛，由 aero-auth 的 Pg impl
    /// 映射为 false（fail-closed）。
    pub async fn q0_provisioned(&self) -> Result<bool>;
}
```

模块名 `relay_scope.rs` 与 sibling 的 `audit_relay_provision.rs` 区分（两方向并存不撞名）。Q0_SQL 字面量复制 + 交叉注释（§0.2/§7 D2）。

### 2.3 `crates/aero-audit-connector/src/relay.rs`（仅 additive）

```rust
pub struct AuditRelay {
    repo: Arc<dyn OutboxRepo>,
    client: AuditClient,
    config: RelayConfig,
    scope_provisioner: Option<aero_auth::SharedRelayScopeProvisioner>,  // None = fail-closed
}
// `new()` 签名不变，置 scope_provisioner: None；`Debug` 手写 finish_non_exhaustive 不破。
#[must_use]
pub fn with_scope_provisioner(mut self, p: aero_auth::SharedRelayScopeProvisioner) -> Self

pub async fn dispatch_batch(&self) -> Result<usize, Error> {
    // B5-4 scope-provisioning gate（T-11）：未注入 provisioner 或谓词不成立
    // → 零 claim、零 reconcile 副作用，行保持 status 0（never claimed）。
    // deliver_claim 状态机（403→dead / requeue / backoff）零改动。
    match &self.scope_provisioner {
        Some(p) if p.audit_event_write_provisioned().await => {}
        _ => {
            tracing::debug!("audit relay paused: audit:event:write not provisioned; rows stay status 0");
            return Ok(0);
        }
    }
    // ……既有 body（reconcile → claim_due → deliver_claim）原样……
}
```

**零改动**：`deliver_claim` 各臂、`client.rs`（claim 校验 + `SCOPE_AUDIT`）、`config.rs`（env 面）、`outbox.rs` trait、`fake.rs`、`stub.rs`。

### 2.4 `crates/aero-server`（接线，无新 crate 依赖——aero-server → aero-auth/aero-storage/aero-audit-connector/aero-eng 均已存在）

`boot/services.rs`：
- `Services` 增字段 `pub(crate) relay_scope_provisioner: Option<aero_auth::SharedRelayScopeProvisioner>`（main.rs 经部分 move 后仍可读——`services.auth` move 不影响其他字段）。
- `build()` 的 Auth 段（`with_login_throttle` 旁）：

```rust
// B5-4 scope provisioning（T-11）：仅 relay 配置存在时注入 provisioner；
// 配给判定（Q0 谓词）在每次消费时复评，DB 错误 → false（fail-closed）。
if let Ok(Some(_)) = aero_audit_connector::config::RelayConfig::from_env() {
    let p: aero_auth::SharedRelayScopeProvisioner = Arc::new(
        aero_auth::PgRelayScopeProvisioner::new(deps.pg.clone()));
    auth = auth.with_relay_scope_provisioner(p.clone());
    services_relay_scope = Some(p);   // 存入 Services 供 main.rs relay 装配
}
```

`main.rs` relay 臂（:258 行）：

```rust
let relay = aero_audit_connector::relay::AuditRelay::new(repo, client, relay_cfg);
let relay = match services.relay_scope_provisioner.clone() {
    Some(p) => relay.with_scope_provisioner(p),
    None => { warn!("audit relay running WITHOUT scope provisioner — claims disabled (fail-closed)"); relay }
};
```

（`Ok(None)` 与 `Err` 臂不动；provisioner 单实例构造点 = boot，main.rs 只消费。）

## 3. Compatibility constraints

| 面 | 约束 | 处置 |
|---|---|---|
| `AuditRelay::new` 签名 | **不变**；新字段默认 None | 6 处构造点编译期不破 |
| **None 语义 = fail-closed**（§7 D1） | 无 provisioner 的 relay 零 claim | 3 个 drill（relay/t11/priority）各 +1 行 `.with_scope_provisioner(Arc::new(AlwaysProvisioned))` + 6 行本地 struct；`relay.rs` 内部测试 :374/:565 同款；`state_machine.rs` helper `relay()`/`relay_with_keys()` 注入恒 true |
| 既有测试断言 | 断言零改动，helper 注入即绿 | 23 项 connector 测试（计数现场实测）回归绿，含 `forbidden_dead_on_first_attempt`（T-11 pin） |
| drill 语义 | drill 是 claim 路径的端到端验证；None=fail-closed 下未更新会**静默 0 claim**（比报错更糟） | 更新 drill + 迁移步骤含「跑 drill 确认仍行使 claim 路径」（drill 内部断言会暴露静默 no-op） |
| `AuthService` | 纯 additive：新字段 + builder + 方法；`new()`/`from_pem` 不变 | 既有构造点零改动；None = fail-closed |
| `Claims`/`AuthUser`/`oidc.rs`/`extractor.rs` | **零改动**（AC3/AC2 钉死） | 不新增 scope claim/scopes 字段/机器端点 |
| `client.rs`/`config.rs`/`outbox.rs`/`fake.rs` | **零改动** | `audit:event:write` 字面量单源保持 connector |
| 迁移 / env var | **零新增** | Q0 读既有 0235 表；无 build-then-migrate 次序问题 |
| 路由 / 鉴权 | 零新路由 | `authz_lint` 不受影响 |
| 依赖 | 零新 crate 依赖 | aero-auth 用既有 sqlx（:49）+ aero-storage（:14）；connector 用既有 aero-auth（:14） |
| truth-check | 新 builder 均有调用点 | `.with_relay_scope_provisioner(`（boot/services.rs）+ `.with_scope_provisioner(`（main.rs）→ 无 unwired 警告 |

## 4. Failure modes（fail-closed 审计）

| 场景 | 行为 | 证据/锚点 |
|---|---|---|
| 谓词 DB 错误 | `q0_provisioned` Err → Pg impl 返回 false → 零 claim；`assert_audit_scope_provisioned` → 403 | R1 契约：任何异常 → false，无 "provisioned on error" 路径 |
| relay 未配置（`RelayConfig::from_env` = None/Err） | relay 根本不装配（既有语义）；AuthService 不注入 → None | 零行为变化 |
| relay 配置存在但 Q0 不成立（runtime 关/零 bindings） | provisioner 注入但每次判定 false → 行保持 status 0；`assert_audit_scope_provisioned` → 403（文案 "predicate not satisfied (T-11)"） | Q0 = 0235 事实源，非进程内 `ready()` |
| boot 接线遗漏（AuthService 未注入） | AuthService 侧 None → 403（fail-closed）；relay 侧仍有 provisioner（同一构造点） | 单构造点（§2.4）消除两半失配 |
| relay 构造遗漏 provisioner | None → 零 claim + `warn!`（main.rs 防御分支） | §2.4 的 `None => warn!` 臂 |
| 运行中 relay 劣化（runtime 关） | 下一 tick 复评 Q0 → 零 claim（≤1 poll_interval 收敛）；DB 错误同样 fail-closed | 每 dispatch_batch 复评（§7 D3） |
| 403→dead（已配给但 sink 拒身份） | 既有 T-11 语义不变：403 → 立即 mark_dead | `deliver_claim` Forbidden 臂零改动 |
| dead rows | provisioner 只看 0235 runtime/bindings，**不看 outbox**——dead 行永不折算为已配给 | 与 `verdict()` dead-priority :128-135 方向一致（AC5） |
| 每次 tick 额外 SELECT | 每 dispatch_batch +1 次 Q0 查询（`RelayScopeRepo` 直查，无连接池压力） | poll_interval 有界，可忽略 |
| 门禁跳过时日志噪声 | gate 命中走 `debug!`（每 tick 一条 debug）；boot 装配/未装配各一条 info/warn | 不刷 warn 日志 |

## 5. Migration steps（无 SQL 迁移；rollout 顺序 = 依赖序）

> 本方向**零迁移文件、零新 env var**。以下为代码落地顺序（每步可独立编译 + 测）。

1. **aero-auth seam**：`src/relay_scope.rs`（trait + Shared + `PgRelayScopeProvisioner`）+ `AuthService` 字段/builder/`assert_audit_scope_provisioned` + `lib.rs` 导出。单测：AC4 三态 + 组合、AC3 序列化守卫（`cargo test -p aero-auth`）。
2. **aero-storage repo**：`src/relay_scope.rs`（`RelayScopeRepo::q0_provisioned`，Q0_SQL 镜像）+ `lib.rs` 导出 + db_test（`#[ignore]` + `DATABASE_URL` 门控，AGENTS §4.1）。`cargo check --workspace`（迁移零新增，无 build-then-migrate 次序）。
3. **connector 消费**：`relay.rs` 字段 + builder + dispatch_batch 门；**同步更新 6 处构造点**（3 drill + 2 内部测试 + `state_machine.rs` helper 注入恒 true；AC1a 用无注入/恒 false 构造）。`cargo test -p aero-audit-connector`（23 项 + AC1a 全绿）。
4. **boot 接线**：`boot/services.rs`（Services 字段 + 条件注入 + AuthService 链）+ `main.rs` relay 臂消费。`cargo check --workspace`。
5. **回归 + 门禁**：AC1b/AC2/AC5 既有 pin 全绿；`cargo test --workspace --lib`（PG 门控加 `-- --ignored`）；`cargo clippy --workspace --all-targets`（无新警告）；`scripts/{truth-check,file-size-check,web-check}.sh`（0 违规）。跑三个 drill 确认 claim 路径仍行使。

## 6. Testable acceptance mapping

| AC | 验收 | 落点 | 断言/命令 |
|---|---|---|---|
| **AC1a**（新增 pin，E9 补缺） | relay 禁用/未配给 → 零 claim，行保持 status 0、claim_token None | `crates/aero-audit-connector/tests/state_machine.rs` 新用例 `relay_disabled_rows_never_claimed` | fake 插入 1 行（`fake.insert` + `make_due_now` 或 due 即到）→ 无注入 relay 与恒 false relay 各跑一次 `dispatch_batch()` → `Ok(0)`；`fake.row(id)` 断言 `status == FakeStatus::Ready`（==0）且 `claim_token.is_none()`；无 DB |
| **AC1b**（既有 pin） | FailClosed 文案不变 | `crates/aero-eng/tests/audit_provision.rs:42-43` | `assert!(reason.contains("relay disabled")); assert!(reason.contains("no audit:event:write grant issued"));` 保持绿 |
| **AC2**（映射，非新代码） | 已配给时缺 scope 令牌在任何 POST 前被拒 | `oidc/tests.rs:398` `rejects_client_credentials_token_without_required_scope`；`claim_validation.rs:143` `missing_audit_scope_is_rejected_before_any_post` | 两处保持绿（`posts == 0`）；`validate_client_credentials_token` :476-483 零改动 |
| **AC3**（回归 + 守卫） | participant JWT 永不携带 audit scope | `jwt.rs` 既有 `issue_and_verify_roundtrip` :295、`session_pair_shares_sid_but_not_jti` :414 + 新增守卫测试 | 新守卫：`serde_json::to_value(&Claims{..})` 结果对象**不含** `"scope"` 键；既有两用例保持绿 |
| **AC4**（新增，无 DB） | Option-injection 三态 + 与 pat/bot 组合不干扰 | `relay_scope.rs` tests（镜像 `pat_and_bot_token_gates_are_disjoint` :701 构造模式） | 裸 `AuthService::new` → `Err(Error::Forbidden)` 含 "not provisioned"；恒 true → `Ok(())`；恒 false → `Err(Forbidden)` 含 "predicate not satisfied"；`with_pat_verifier(..).with_bot_verifier(..).with_relay_scope_provisioner(..)` 各能力独立 |
| **AC5**（回归） | dead rows 无论 relay 状态永远 fail-closed | `aero-eng/tests/audit_provision.rs` 既有 `verdict_dead_is_fail_closed_even_with_relay_on`、`verdict_dead_priority_over_relay_disabled` | 保持绿；provisioner 不看 outbox（§4 dead rows 行）→ 方向一致 |
| **R2**（注入契约） | config 存在 ⇔ 注入；Q0 谓词落点 | boot 接线 + `RelayScopeRepo` db_test | db_test（PG `#[ignore]`）：runtime.enabled + bindings>0 → `Ok(true)`；关/零 → `Ok(false)`；表缺失 → `Err`（上层映射 false） |
| 门禁 | 提交前全绿 | 命令 | `cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets` · `scripts/{truth-check,file-size-check,web-check}.sh` · AC1a 无 DB 可直接跑 |

## 7. Risks / 决策点 / 与 sibling direction 的合并契约

- **D1（None 语义，本设计裁决）**：relay 侧 `scope_provisioner: None` = **fail-closed 零 claim**（不是 "无门照常 claim"）。理由：① AC1a 首分支「不注入 provisioner → 0 claim」只有该语义成立；② boot 接线遗漏若 fail-open 即整个 campaign 失效（unwired 静默放行正是 truth-check 要抓的死代码类）；③ 与 AuthService seam「None = 能力关闭」哲学一致。代价 = 6 处构造点 +1 行注入（§3），drill 更新后其内部断言会暴露任何静默 no-op。
- **D2（Q0 实现落家）**：`PgRelayScopeProvisioner` 在 aero-auth（镜像 sibling `PgRelayProvisionGate` 落家），裸 SQL 在 aero-storage `RelayScopeRepo`（环安全）。Q0_SQL 字面量在 aero-storage 复制 + 交叉注释（aero-eng 的 :34 为私有 const；aero-auth 依赖 aero-eng 违背零新增依赖且语义倒挂）。**单一化后续项**：把 Q0_SQL 上移到 aero-storage 并让 aero-eng 引用（aero-eng → aero-storage 无环，但 aero-eng 目前刻意只依赖 aero-common——留给独立重构，本方向不做）。
- **D3（判定时机）**：**每消费复评**（每次 `dispatch_batch` / 每次 `assert_audit_scope_provisioned` 各查一次 Q0），注入门只判 config 存在。R2 的「与」条件在消费点生效（未配给的可观察结果与不注入完全一致），且 relay 运行中劣化 ≤1 tick 收敛。静态 boot 恒值（构造时查一次缓存 bool）是兼容替代（§7 req 原文允许），但丧失运行中劣化收敛——不推荐。每次 tick +1 次 SELECT，有界可忽略（§4）。
- **D4（门位置）**：`dispatch_batch` **顶部、reconcile 之前**——未配给时连 reconcile/backfill 副作用都不发生（0241 backfill 幂等，relay 恢复后首 tick 自动补，无数据丢失）。
- **D5（单构造点）**：provisioner 只在 `boot/services.rs` 构造一次（config 存在时），经 `Services.relay_scope_provisioner` 传给 main.rs relay 臂——杜绝「AuthService 注入而 relay 没注入」的两半失配；main.rs 防御分支 `None => warn!`。
- **D6（与 sibling 的合并契约——已关闭，2026-08-08 现场复核；本方向不实现 sibling 任何件）**：sibling（`2026-08-07-…relay-provisioning-gate.design.md`）同名定义 `assert_audit_scope_provisioned` + `relay_provision_gate` 字段 + `with_relay_provision_gate` builder + `refresh_relay_provision` 生命周期 hook（其 `RelayProvisionGate::verified()` = 心跳新鲜度，语义 = 配给**验证/运维**门；本方向 = 配给**授予/广告**门）。sibling 仍在自身 review 循环中（未对合并表态），**本契约即合并的规范文本**：
  1. **一个 seam 族，族名 = 本方向**：AuthService 只保留 `relay_scope_provisioner: Option<SharedRelayScopeProvisioner>` 单字段（`new()` 置 None）+ `#[must_use] pub fn with_relay_scope_provisioner(mut self, p: SharedRelayScopeProvisioner) -> Self` 单 builder + `pub async fn assert_audit_scope_provisioned(&self) -> aero_common::Result<()>` 单拒绝点（403 语义两者完全一致；错误文案以本方向为准——None 臂 "relay-scope provisioner is not installed (T-11)"、false 臂 "relay provisioning predicate not satisfied (T-11)"，两臂均含 "is not provisioned" 子串 → sibling 的 AC1.1「not provisioned」子串断言保持绿）。**族家 = `aero-auth/src/relay_scope.rs`**（sibling 的 `relay_gate.rs` 不再含任何 `impl AuthService` 块——字段/builder/assert/refresh 定义全部并入族，E0592 强制此点，无法静默残留）。
  2. **`refresh_relay_provision` 裁决 = 存活为 no-op，由 sibling 落地时加入族；本方向不抢先定义**：sibling req §7 / design §7 D3 已把该 hook 定为「未来 TTL 缓存位、当前 no-op」（无进程内 sweep，状态 durable + freshness 有界）。保留理由：单定义符号不参与 E0592 面（零碰撞贡献）；pub 方法在 lib crate 无 dead_code 风险；truth-check 不扫非 `with_*` 方法；§4.4 零调用 seam 先例。**本方向不得定义它**——否则并行落地会多出一个 E0592，破坏「恰好一个碰撞」契约。sibling 落地时在族内补：
     ```rust
     /// Lifecycle hook（sibling 2026-08-07 契约）：显式刷新 / 未来 TTL 缓存位。
     /// 当前 no-op seam——谓词每次调用复评、门状态 durable + freshness 有界，无 per-key map 可驱逐。
     pub async fn refresh_relay_provision(&self) {}
     ```
  3. **sibling 的 gate 落为同一 trait 的组成式实现**（sibling 落地时加入 `relay_scope.rs` 族；其 `RelayProvisionGate` trait / `SharedRelayProvisionGate` / `PgRelayProvisionGate` 在 `relay_gate.rs` 保留为 heartbeat 腿契约）：
     ```rust
     /// D6 merged family：grant = q0 ∧ heartbeat-fresh（任何一腿失败 → false → 零 claim / 403）。
     pub struct ComposedProvisioner {
         q0: PgRelayScopeProvisioner,
         heartbeat: SharedRelayProvisionGate,
     }
     impl ComposedProvisioner {
         pub fn new(q0: PgRelayScopeProvisioner, heartbeat: SharedRelayProvisionGate) -> Self;
     }
     #[async_trait]
     impl RelayScopeProvisioner for ComposedProvisioner {
         async fn audit_event_write_provisioned(&self) -> bool {
             self.q0.audit_event_write_provisioned().await && self.heartbeat.verified().await
         }
     }
     ```
     **合并后 boot 接线**（sibling 落地时把本方向的注入行换成；`?` 传播半配置 Err——与 main.rs 同一幂等 env 读，`if let Ok(Some(_))` 吞 Err 形态作废，本方向首落即用 `?`）：
     ```rust
     if let Some(relay_cfg) = aero_audit_connector::config::RelayConfig::from_env()? {
         let q0 = aero_auth::PgRelayScopeProvisioner::new(deps.pg.clone());
         let heartbeat = Arc::new(aero_auth::relay_gate::PgRelayProvisionGate::new(
             deps.pg.clone(), relay_cfg.provision_freshness));
         let p: aero_auth::SharedRelayScopeProvisioner =
             Arc::new(aero_auth::ComposedProvisioner::new(q0, heartbeat));
         auth = auth.with_relay_scope_provisioner(p.clone());
         services_relay_scope = Some(p);
     }
     ```
  4. **并行未合并落地 = 恰好一个 rustc E0592，别无其他**：两方向各自分支独立编译绿；合并树上 `impl AuthService` 重复方法定义 `assert_audit_scope_provisioned`（sibling `relay_gate.rs` vs 本方向 `relay_scope.rs`）→ aero-auth crate **E0592 duplicate definitions**——硬编译错误，强制收敛，无静默语义冲突。成对符号/模块现场复核全部互异（2026-08-08 grep）：aero-auth 内 `relay_gate`/`relay_scope` 模块、`RelayProvisionGate`/`RelayScopeProvisioner`、`SharedRelayProvisionGate`/`SharedRelayScopeProvisioner`、`PgRelayProvisionGate`/`PgRelayScopeProvisioner`、字段 `relay_provision_gate`/`relay_scope_provisioner`、builder `with_relay_provision_gate`/`with_relay_scope_provisioner`——六对全异；`refresh_relay_provision` 单定义（sibling 侧）。aero-storage：`audit_relay_provision.rs`/`relay_scope.rs`、`AuditRelayProvisionRepo`/`RelayScopeRepo`、`ProvisionCheck`——全异。connector：sibling `heartbeat.rs`（`HeartbeatRecorder`/`HeartbeatOutboxRepo`）+ `RelayConfig.provision_freshness` vs 本方向 `AuditRelay.scope_provisioner` + `with_scope_provisioner`——全异（且跨 crate）。aero-server：`Services.relay_scope_provisioner`（本方向）vs `PgHeartbeatRecorder`（sibling）——全异。两方向同改 `boot/services.rs::build` 与 `main.rs` relay 臂 = 预期内的 git hunk 冲突（非 rustc 碰撞），按本契约 rebase。relay 侧正交：本方向门在 `dispatch_batch` 顶，sibling 装饰器 `HeartbeatOutboxRepo` 包 repo 记 settle，可共存。
  5. **落地顺序 = 本方向先、sibling 后；改名方 = sibling**：本方向先落（定族名：模块 `relay_scope.rs`、字段 `relay_scope_provisioner`、builder `with_relay_scope_provisioner`、拒绝点 `assert_audit_scope_provisioned`）。sibling 后落，承担全部改名/删重/补件义务：① 删自身 AuthService 面（字段/builder/assert/refresh 定义，从 `relay_gate.rs` 移除）② 族内补 `refresh_relay_provision` no-op ③ 加 `ComposedProvisioner` + `impl RelayScopeProvisioner` ④ boot 注入换成组成式（`?` 传播）⑤ **迁移序号 0240 → 0242**（`0240_audit_governance_due_prio_idx.sql`、`0241_governance_reconcile.sql` 已落地占用；sqlx 重复版本号 migrate 时硬错）⑥ **放弃 `aero-cli audit-provision-check` 交付**（已由 aero-eng 独立方向落地于 `crates/aero-cli/src/main.rs:489`，bin `aero-eng`；再建即双份命令）⑦ 保留 heartbeat 腿（connector `heartbeat.rs` / `RelayConfig.provision_freshness` 4 处 literal / main.rs 装饰器 + `PgHeartbeatRecorder` / storage `AuditRelayProvisionRepo` + db_test）。
- **边界重申（不复建 sibling 件）**：不建 `audit_relay_provisioning` 心跳表、不建 `AERO_AUDIT_PROVISION_FRESHNESS_SECS`、不建 `aero-cli audit-provision-check`（该 CLI 已由 aero-eng 独立方向落地）、不建 freshness 过期语义。本方向 = Option-injection 门 + claim 边界消费，effort 4。
- **残余风险**：`assert_audit_scope_provisioned` 现状无调用方（未来机器端点）——AC4 单测是其行为契约；truth-check 不会因 relay 侧调用点存在而标记 AuthService 侧遗漏（条件接线），靠 boot 单构造点 + 评审兜底。已配给后 scope 字面量单源仍在 connector（R5），aero-auth 只表达布尔配给。

## 8. Sequencing

1. **aero-auth**：`relay_scope.rs` + AuthService seam + lib.rs 导出；AC4 三态/组合 + AC3 守卫测试 → `cargo test -p aero-auth`。
2. **aero-storage**：`RelayScopeRepo` + db_test（`#[ignore]`）→ `cargo check --workspace`。
3. **connector**：relay 门 + 6 处构造点注入 + AC1a → `cargo test -p aero-audit-connector`（既有 23 项 + AC1a）。
4. **boot**：Services 字段 + 条件注入 + main.rs 消费 → `cargo check --workspace`。
5. **门禁**：AC1b/AC2/AC5 pin 全绿；全量 test/clippy/truth-check/file-size-check/web-check；跑 3 个 drill 验证 claim 路径仍行使。

# Requirements Spec — aero-auth scope provisioning（B5-4）：`audit:event:write` 仅 relay 可用时配给，无 relay fail-closed（Option-verifier seam）

- **Module (analysis root)**: `crates/aero-auth` — 新增 relay-scope provisioner 的 `Option<Shared…>` seam（复制 `pat_verifier`/`bot_verifier` 模式）；消费面 = aero-audit-connector 的 claim 边界（依赖已存在）+ aero-server boot 注入点
- **Direction**: "Scope provisioning (B5-4): encode 'grant `audit:event:write` only after the relay works; fail-closed without relay' as an auth capability via aero-auth's Option-verifier seam"（value 8 / risk_reduction 7 / effort 4 / confidence 7）
- **Source analysis**: `docs/auto/analyses/crates-aero-auth-0b9b4b9f.json`（direction #2）
- **Campaign**: `aero-im-b5-outbox-relay`（`docs/campaigns/campaign-aero-im-b5.yaml:39-42`："(4) scope provisioning (grant audit:event:write only after relay works; fail-closed without relay)"）；gate anchor `docs/campaigns/implementation-gate.md:64`（aero-im 行 T-11 "无 relay 配给被拒"）
- **Status**: Requirements（下述证据全部经源码 grep 核对，2026-08-08）
- **Current implementation status (2026-08-19)**: 已落地并通过无数据库回归测试。`aero-auth::relay_scope` 提供 Q0 + settle-heartbeat 组合 provisioner；`AuthService::assert_audit_scope_provisioned` 未注入/谓词失败时 fail-closed；`AuditRelay` 在 claim 前复评同一 gate。生产 boot 将同一 `Arc` 注入认证服务与 relay，connector 另有“未配给零 claim”回归 pin。
- **Verification date**: 2026-08-08。行号为核对时锚点，会漂移——**文件/符号**才是稳定 grep 锚点（AGENTS.md §0）

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-auth/src/service.rs:66,73,131-149` — `pat_verifier`/`bot_verifier` Option seam + builder | ✅ **Verified（行号微漂）**。`pat_verifier: Option<SharedPatVerifier>` :66、`bot_verifier: Option<SharedBotVerifier>` :73；`new()` 置 `None` :85-95（**None = 能力关闭 = fail-closed**，注释原文）；`#[must_use] with_pat_verifier` :134、`with_bot_verifier` :152（cited 131-149，漂移 ≤18 行，符号精确）。测试 `pat_and_bot_token_gates_are_disjoint` 存在（service.rs tests）——AC4 的镜像模板 |
| E2 | `crates/aero-auth/src/jwt.rs:36-52` — `Claims` 无 scope claim | ✅ **Verified**。`Claims` :36-52：`sub` :37 / `iss` :38 / `iat` :39 / `exp` :40 / `kind` :41 / `sid` :46 / `jti` :51——**无 scope 字段**。测试 `issue_and_verify_roundtrip`、`session_pair_shares_sid_but_not_jti` 存在（jwt.rs tests）——AC3 锚点 |
| E3 | `crates/aero-auth/src/extractor.rs:33-42` — `AuthUser` 无 scopes 字段 | ✅ **Verified**。`AuthUser` :34-40：`participant_id` :35 / `session_id` :38 / `exp` :40——无 scopes、无机器身份 kind。extractor 链 JWT→PAT→bot 不变 |
| E4 | `crates/aero-auth/src/oidc.rs:364-371` — `ClientCredentialsTokenConfig.required_scopes` 全有或全无强制点 | ⚠️ **行号漂移 14 行**。struct 实际 :378-381（`issuer`/`audience`/`required_scopes`）；强制点 = `validate_client_credentials_token` :420 的 all-or-nothing 检查 :477-481（`granted_scopes()` 并集后任一 required 缺失 → `OidcError::Invalid("token lacks a required application scope")`）。测试 `rejects_client_credentials_token_without_required_scope` 实际在 `crates/aero-auth/src/oidc/tests.rs:398`（oidc.rs 本体无 tests）——AC2 锚点 |
| E5 | `crates/aero-eng/src/audit_provision.rs:60-90` — Q0 relay-health 谓词 + Verdict 三态 | ⚠️ **行号漂移**。`Q0_SQL` :34（`runtime.enabled` + enabled bindings count，注释"deliberately not `ready()`"）；`Verdict` enum :110-116（`FailClosed(String)`/`Consistent`/`Healthy`）；`verdict()` :125；FailClosed 文案 "no audit:event:write grant issued" :141。**post-hoc 检查，非运行期 grant**——direction 问题陈述准确 |
| E6 | `crates/aero-audit-connector/src/config.rs:87-90` — `AERO_AUDIT_EXPECTED_SCOPE` 静态配给 | ⚠️ **行号漂移**。字段 `expected_scope` :35；`from_env` 默认 `"audit:event:write"` :119-120（optional_env 回退）。**全仓 `audit:event:write` 字面量不在 aero-auth**（grep 零命中）——单源在 connector（config.rs :119-120 + client.rs `SCOPE_AUDIT`） |
| E7 | `crates/aero-audit-connector/src/relay.rs` — `claim_due`/`requeue` 循环（"relay works" 就绪面） | ✅ **Verified**。`AuditRelay::dispatch_batch`（reconcile → `repo.claim_due(lease, batch)` → `for_each_concurrent deliver_claim`）；`deliver_claim` 403→`mark_dead` 立即（T-11 已实现）；transient→`requeue`、permanent 第 2 次→dead。`OutboxRepo::claim_due` trait 在 `src/outbox.rs`（fenced、repo 时钟）。**claim 路径今天无任何配给门**——与 direction 问题陈述一致 |
| E8 | （补充）`aero-eng/tests/audit_provision.rs:43` — "no audit:event:write grant issued" | ✅ **精确命中**。:43 = `assert!(reason.contains("no audit:event:write grant issued"));`，位于 `verdict_relay_off_with_undelivered_is_fail_closed`（:42 还断言 `"relay disabled"`）。AC1 的既有 pin |
| E9 | （补充）`tests/state_machine.rs` "no-claim-when-disabled case" | ❌ **不存在——需新增**。`crates/aero-audit-connector/tests/state_machine.rs` 现有 8 用例：stale_token_cannot_ack_after_reclaim / backoff / permanent_error_dead_after_exactly_two_attempts / **forbidden_dead_on_first_attempt**（403→dead，最近似的 T-11 pin）/ happy_path / skew_gt_lease / priority_first_claim / signature_rejected。**无「relay 禁用时不 claim」用例**。`fake.rs` 的 `FakeStatus::Ready = 0`（:42）——"rows stay status 0" 的可断言语义已具备（`row.status == FakeStatus::Ready` 且 `claim_token == None`） |
| E10 | （补充）消费面依赖与 403 状态码 | ✅ **Verified**。`crates/aero-audit-connector/Cargo.toml` 依赖 `aero-auth.workspace = true`——seam 可被 connector claim 边界直接消费，无新依赖。`crates/aero-common/src/error.rs`：`Forbidden(String)` :19、HTTP 403 :70、code "forbidden" :85——gate 拒绝的规范状态码 |

### 1.1 对 direction 引用/陈述的勘误（evidence-backed）

- **「no-claim-when-disabled case」是拟议 pin 而非既有 pin**（E9）：direction acceptance 表述为 "pinned by connector `tests/state_machine.rs` (no-claim-when-disabled case)"——该用例**今天不存在**，必须作为本方向的新增测试落库（AC1a）。既有 pin 是 `forbidden_dead_on_first_claim` 系（403→dead）与 `aero-eng/tests/audit_provision.rs:43`。
- **`required_scopes` 强制点行号**：direction 引 oidc.rs:364-371 为"the all-or-nothing enforcement point"——struct 定义在 :378-381，**强制逻辑**在 :477-481（`granted_scopes()` 缺失任一 → Invalid）。语义不变，符号不变。
- **`Claims`/`AuthUser` 无 scope 是"现状"而非"缺口"**：direction 说 "no token path can express or withhold the audit capability"——准确；但本方向**不**给 participant JWT 加 scope claim（AC3 明确禁止），能力只经 provisioner seam 表达（R3）。
- **Q0 谓词 = DB 事实源**（`snaplink_commercial_runtime.enabled` + enabled bindings > 0，E5 Q0_SQL :34）：本方向的注入条件镜像它，但**不做** sibling direction（`2026-08-07-aero-auth-b5-4-relay-provisioning-gate.req/design.md`）的心跳表/CLI/freshness 机制——本方向只有 Option-injection 门 + claim 边界消费，effort 4 与 sibling effort 5 的边界在此（§3）。
- **403 语义**：`Error::Forbidden` = HTTP 403（E10），非 extractor 的 401——gate 拒绝点用 `Error::Forbidden`。

## 2. Verified current state（gap 确认）

```
现状（四条独立事实，互不相认）：
a) 静态 scope 契约（E6）  AERO_AUDIT_EXPECTED_SCOPE（默认 "audit:event:write"）→ RelayConfig.expected_scope
                          → client.rs token 请求 scope + claim 校验 —— 纯 env 静态，无运行期 grant 判定
b) 运行期强制点（E4）     oidc.rs validate_client_credentials_token + required_scopes all-or-nothing :477-481
                          —— 只验证"token 带了没"，不决定"该不该配给"
c) post-hoc 检查（E5）    aero-eng audit_provision.rs：psql 3-state verdict —— relay 禁用 + 积压才 FailClosed；
                          是检查不是闸：不阻止 claim/forward 先行发生
d) claim 路径（E7/E9）    relay.rs dispatch_batch → claim_due → deliver —— 无任何配给门；403→dead 是事后惩罚

AuthService Option-seam 模板（E1，本方向复制的模式）：
  Option<SharedX> 字段（:66/:73，new() 置 None = 默认关）
  → #[must_use] with_x_verifier builder（:134/:152）
  → extractor 链按序尝试；None = 该能力完全关闭
```

**Gap this direction closes**：把 "grant `audit:event:write` only after the relay works" 编码为 **auth 层可注入能力**——provisioner 注入 ⇔ 配给可用；未注入/谓词不成立 ⇒ 无配给 ⇒ claim 边界不 claim（行保持 status 0）⇒ fail-closed without relay。aero-auth 是 seam 的家（module 范围），aero-audit-connector 是消费方（依赖已存在，E10），aero-server boot 是注入点（R2）。

## 3. Scope

**In scope（B5-4 scope-provisioning，effort 4 的最小切片）**：
- aero-auth：新模块 `src/relay_scope.rs`（`RelayScopeProvisioner` trait + `SharedRelayScopeProvisioner` + `AuthService.relay_scope_provisioner` 字段/builder + `assert_audit_scope_provisioned` fail-closed 拒绝点 403）；`lib.rs` `pub mod` + re-export。**零 `Claims`/`AuthUser`/`oidc.rs` 改动**。
- aero-audit-connector：claim 边界消费 provisioner（未配给 → `claim_due`/`dispatch_batch` 不 claim，行保持 status 0）；**relay.rs 状态机、client.rs claim 校验、config.rs 全部不动**（零行为回退）。新增 `tests/state_machine.rs::relay_disabled_rows_never_claimed` 用例（AC1a）。
- aero-server：boot 注入点——`RelayConfig::from_env()` 存在 **且** Q0 谓词成立时 `.with_relay_scope_provisioner(…)`（镜像 `boot/services.rs:59-64` 的 pat/bot 接线模式）；否则不注入（None = fail-closed）。具体谓词实现（读 DB 的 `snaplink_commercial_runtime`/`snaplink_commercial_bindings`）放消费层（aero-storage XRepo 或 boot 闭包，AGENTS §4.1 配方；DB 错误 → false）。
- 测试：aero-auth 单测（无 DB，AC4）+ connector fake 测试（无 DB，AC1a）+ 既有 pin 回归（AC1b/AC2/AC3/AC5 全绿）。

**Out of scope（勿在本 direction 建造）**：
- **Sibling direction 全套**：durable 心跳表（`audit_relay_provisioning`）、`AERO_AUDIT_PROVISION_FRESHNESS_SECS`、`HeartbeatOutboxRepo` 装饰器、`aero-cli audit-provision-check` 命令、freshness 过期语义 → `docs/requirements/2026-08-07-aero-auth-b5-4-relay-provisioning-gate.req.md`（独立 direction，effort 5）。本 direction 的注入门是**静态 boot 判定**（配给谓词），不做运行期心跳轮转。
- `Claims` 加 scope claim / `AuthUser` 加 scopes 字段 / 新机器身份入站端点 → **明确禁止**（AC3 钉死：participant JWT 永不携带 audit scope）。
- oidc.rs `validate_client_credentials_token`/`required_scopes` 语义改动 → 不动（AC2 只要求回归绿 + 映射）。
- relay.rs 状态机 / client.rs claim 校验 / config.rs env 面 → 零改动（AC1a 断言即回归证明）。
- 迁移 / 新 env var → 零新增（Q0 谓词读既有 0235 表）。
- IdP scope registry 本体（proposal "registry 本体在 IdP 仓"）→ 仓外。

## 4. Requirements

### R1 — Relay-scope provisioner Option seam on `AuthService`（aero-auth，复制 E1 模式）
新模块 `crates/aero-auth/src/relay_scope.rs`：
- `#[async_trait] pub trait RelayScopeProvisioner: Send + Sync { async fn audit_event_write_provisioned(&self) -> bool; }`——`true` 仅当 audit:event:write 配给真实可用；**任何异常（谓词读错/DB 错误/未配给）→ `false`**（fail-closed，绝不 "provisioned on error"）。
- `pub type SharedRelayScopeProvisioner = Arc<dyn RelayScopeProvisioner>`（`pat.rs`/`bot.rs` 同款 `Shared*` 惯例）。
- `AuthService.relay_scope_provisioner: Option<SharedRelayScopeProvisioner>`；`new()` 置 `None`（:85-95 同款——**未注入 = 能力关闭 = fail-closed**）；`#[must_use] pub fn with_relay_scope_provisioner(mut self, p: SharedRelayScopeProvisioner) -> Self`（:134 同款签名）。
- 拒绝点 `pub async fn assert_audit_scope_provisioned(&self) -> aero_common::Result<()>`：
  - `None` → `Err(Error::Forbidden("audit:event:write is not provisioned: relay-scope provisioner is not installed (T-11)"))`；
  - `audit_event_write_provisioned()==false` → `Err(Error::Forbidden("audit:event:write is not provisioned: relay provisioning predicate not satisfied (T-11)"))`；
  - `true` → `Ok(())`。
  **403 规范状态码**（E10）。任何 audit-scoped 机器路径（现状无入站端点；未来端点）的必过闸。

### R2 — 注入契约：仅 relay 配置存在 **且** Q0 谓词成立时注入（boot）
- 注入条件（**与**，任一不满足 → 不注入 → None → fail-closed）：
  1. relay 配置存在：`aero_audit_connector::config::RelayConfig::from_env()` 为 `Ok(Some(_))`（`AERO_AUDIT_TOKEN_ENDPOINT` presence-gated，config.rs 既有语义）；
  2. Q0 relay-health 谓词成立：`snaplink_commercial_runtime.enabled` 且 enabled bindings > 0（**镜像 `aero_eng::audit_provision` Q0_SQL :34**——读 DB 事实源，不是进程内 `ready()`）。
- 接线点：`crates/aero-server/src/bin/boot/services.rs::build` 的 `AuthService` 构造链（:59-64 pat/bot 接线模式旁）。
- 具体谓词实现落消费层：读 DB 的 XRepo（AGENTS §4.1「每功能一个 XRepo」配方）或 boot 闭包——**trait + seam 是 aero-auth 的，实现注入是 boot 的**；DB 错误向上传播为 `false`（fail-closed）。

### R3 — Participant JWT 永不携带 audit scope（零 Claims/AuthUser 改动）
- `jwt.rs::Claims`（:36-52）**不新增 scope 字段**；`extractor.rs::AuthUser`（:34-40）**不新增 scopes 字段**。
- audit 能力只经 provisioner seam（R1）表达与 withhold——participant 令牌路径与 B5-4 配给判定**零耦合**。
- 理由：participant JWT 是身份凭证非能力清单；scope 语义属 client-credentials 面（E4）。AC3 回归钉死。

### R4 — Claim 边界 fail-closed 消费：未配给 → 行保持 status 0（never claimed）
- aero-audit-connector 的 claim 边界（`AuditRelay::dispatch_batch` 的 `claim_due` 前，E7）在未配给时**不 claim**：`dispatch_batch` 返回 0，outbox 行保持 status 0（0239 `status=0`；fake 语义 `FakeStatus::Ready == 0`，E9）、`claim_token` 保持 `None`。
- **relay.rs 状态机、client.rs、config.rs 零改动**——门只设在 claim 入口，不改变 403→dead/requeue/backoff 语义（AC1a 断言 = 回归证明）。
- 消费方式：boot 把 `SharedRelayScopeProvisioner` 传给 relay 装配（connector 已依赖 aero-auth，E10），或经 `AuthService::assert_audit_scope_provisioned`（R1）判定——实现选择留设计，契约是「未配给 ⇒ 零 claim」。

### R5 — 配给后强制不变：缺 scope 的令牌在任何 POST 前被拒（回归钉死）
- 已配给（relay wired）时，既有 all-or-nothing 强制点原样生效：`validate_client_credentials_token`（oidc.rs :477-481）拒绝缺 `audit:event:write` 的令牌；connector claim 校验（client.rs `validate_token_claims`）同语义——**任何 POST 前拒绝**。
- `audit:event:write` 字面量**不迁入 aero-auth**：单源保持 connector（config.rs :119-120 `AERO_AUDIT_EXPECTED_SCOPE` 默认 + client.rs `SCOPE_AUDIT`）——aero-auth 只表达「配给是否成立」的布尔能力，不重复 scope 字面量。

### R6 — Verdict 矩阵一致性：dead rows 永远 fail-closed（与 audit_provision.rs 对齐）
- 配给判定方向与 `aero_eng::audit_provision` 的 verdict 矩阵一致：**dead rows 是终态，无论 relay 开/关都 fail-closed**（verdict() :125 的 dead-priority 分支）；provisioner 的 `audit_event_write_provisioned()` 不得把 dead rows 折算为已配给。
- FailClosed 文案 "no audit:event:write grant issued"（audit_provision.rs :141，tests :43）**保持不变**——CLI 检查与本 direction 的 auth 门共享同一配给语义。

## 5. Acceptance checks（direction 原样保留，逐条 testable）

### AC1 — Relay disabled/unconfigured → 无 `audit:event:write` 配给广告，outbox 行保持 status 0（never claimed）：T-11 fail-closed
**AC1a（新增 pin，E9 补缺）**：`crates/aero-audit-connector/tests/state_machine.rs` 新用例 `relay_disabled_rows_never_claimed`——`FakeOutbox` 插入 1 行 → **不注入 provisioner**（或注入恒 `false` provisioner）→ `dispatch_batch()` 返回 0 且 `claim_due` 返回空 → `row.status == FakeStatus::Ready`（== 0）且 `row.claim_token == None`。无 DB。
**AC1b（既有 pin，必须保持绿）**：`crates/aero-eng/tests/audit_provision.rs:43`——`verdict_relay_off_with_undelivered_is_fail_closed` 断言 reason 含 "no audit:event:write grant issued" 且 "relay disabled"（R6 文案不变即绿）。

### AC2 — Relay wired → 缺 `audit:event:write` 的令牌在任何 POST 前被拒（映射既有 oidc 强制点）
`crates/aero-auth/src/oidc/tests.rs:398` `rejects_client_credentials_token_without_required_scope` 保持绿（R5：required_scopes all-or-nothing 不动）；connector `tests/claim_validation.rs:143` `missing_audit_scope_is_rejected_before_any_post` 保持绿（posts == 0）。**本 direction 不改这两处**——AC2 是映射/回归声明，不是新代码。

### AC3 — Participant JWT 永不携带 audit scope
`crates/aero-auth/src/jwt.rs` 测试 `issue_and_verify_roundtrip`、`session_pair_shares_sid_but_not_jti` 保持绿且 **`Claims` 无 scope 字段**（R3）——serde 往返不变、`kind`/`sid`/`jti` 语义不变。可加一条编译期守卫测试：`Claims` 结构体不含 scope（如 serde_json 序列化结果不含 `"scope"` 键），钉死"participant JWT 面与配给零耦合"。

### AC4 — 配给经与 PAT/bot verifier 相同的 Option-injection 契约切换（None = disabled）
`crates/aero-auth/src/relay_scope.rs`（或 service.rs tests）新单测，镜像 `pat_and_bot_token_gates_are_disjoint`（service.rs tests）的构造模式，无 DB：
- 裸 `AuthService::new`（None）→ `assert_audit_scope_provisioned()` == `Err(Error::Forbidden)`，消息含 "not provisioned"；
- 注入恒 `true` provisioner → `Ok(())`；
- 注入恒 `false` provisioner → `Err(Error::Forbidden)`；
- 组合断言：`with_pat_verifier(...).with_bot_verifier(...).with_relay_scope_provisioner(...)` 各能力互不干扰（与 `pat_and_bot_token_gates_are_disjoint` 同构）。

### AC5 — Verdict 矩阵与 audit_provision.rs 一致：dead rows 无论 relay 状态永远 fail-closed
`crates/aero-eng/tests/audit_provision.rs` 既有 `verdict_dead_is_fail_closed_even_with_relay_on`、`verdict_dead_priority_over_relay_disabled` 保持绿（R6）；provisioner 判定与 verdict 矩阵方向一致（dead 终态永不折算为已配给）——以「配给判定单测 + verdict 矩阵单测共享同一 fail-closed 方向」为断言面。

## 6. Test placement

| Test | Location | Harness |
|---|---|---|
| AC4 三态 + 组合（None/false/true → Forbidden/Ok；与 pat/bot 组合不干扰） | `crates/aero-auth/src/relay_scope.rs` tests（或 service.rs tests） | 纯单测，无 DB |
| AC3 守卫（Claims 序列化无 scope 键） | `crates/aero-auth/src/jwt.rs` tests（既有两用例 + 新增守卫） | 纯单测 |
| AC1a no-claim-when-disabled（未配给 → 0 claim、status 0、token None） | `crates/aero-audit-connector/tests/state_machine.rs` 新用例 | fake，无 DB |
| AC1b/AC5 verdict 文案与 dead-priority | `crates/aero-eng/tests/audit_provision.rs`（:43 等既有用例） | 纯单测，保持绿 |
| AC2 缺 scope 拒 POST | `crates/aero-auth/src/oidc/tests.rs:398` + `crates/aero-audit-connector/tests/claim_validation.rs:143` | 既有，保持绿 |
| R2 注入契约（config 存在 + Q0 谓词 ⇔ 注入；否则 None） | boot 层接线 + aero-storage XRepo db_tests（PG `#[ignore]`，谓词实现落点） | 视实现落点 |

**门禁**：`cargo check --workspace`（干净）· `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check}.sh`（新 builder 须有 boot 调用点，防 unwired）· AC1a 无 DB 可直接跑。

## 7. Risks / 决策点 / 与 sibling direction 的边界

- **「outbox claimable」的读法**：direction 原文 "injected only when relay config is present and outbox claimable (mirroring the … relay-health predicate on `snaplink_commercial_runtime.enabled` + bindings)"——本 spec 把 "outbox claimable" 落实为 Q0 谓词（enabled + bindings > 0）这一**静态 boot 判定**（R2）。不做运行期心跳轮转（sibling direction 的 freshness 机制）——若后续要"运行中 relay 劣化即收回配给"，是 sibling 心跳机制 + 本 seam 的组合，属新方向。
- **注入时判定 vs 每次 claim 判定**：seam 返回 `bool`，两种实现都兼容（静态注入恒值 / 每 claim 复查 DB）。effort 4 最小切片 = boot 时注入、claim 时消费 provisioner 返回值；每-claim 复查留给设计（可强化不破坏契约）。
- **AC1a 是新增 pin**（E9 勘误）：direction 引用它时表述为既有 pin，实际不存在——本 spec 明确其为新增用例，既有最近似 pin 是 `forbidden_dead_on_first_attempt`（403→dead，保持绿）。
- **与 sibling direction（`2026-08-07-aero-auth-b5-4-relay-provisioning-gate.{req,design}.md`）的重叠面**：都动 `AuthService` Option-seam 模式、都叫 `assert_audit_scope_provisioned`-类拒绝点。**边界**：本 direction = 配给**授予/广告**门（claim 边界零 claim，静态 Q0 谓词）；sibling = 配给**验证/运维**门（心跳 + CLI + freshness）。若并行落地，两个 builder（`with_relay_scope_provisioner` vs `with_relay_provision_gate`）与两个拒绝点须合并为一个 seam 族（设计阶段裁决），但**本 spec 不实现 sibling 的任何件**。
- **fail-closed 方向无例外**：谓词 DB 错误 → `false`（R1）；注入缺失 → `None`（R1）；dead rows → 永不配给（R6）。没有 "provisioned on error" 路径。
- **零新增依赖**：connector→aero-auth 依赖已存在（E10）；aero-auth 无新 crate 依赖（trait + seam 纯 async-trait，已用）。

## 8. Sequencing

1. **aero-auth seam**：`src/relay_scope.rs`（trait + `SharedRelayScopeProvisioner` + `AuthService` 字段/builder + `assert_audit_scope_provisioned`）+ `lib.rs` `pub mod` + re-export；AC4 单测 + AC3 守卫测试。
2. **connector 消费**：claim 边界接 provisioner（未配给 → 0 claim）；`tests/state_machine.rs` 新增 AC1a 用例；relay.rs/client.rs/config.rs 零改动（AC1a + 既有 22 项测试回归绿）。
3. **boot 注入**：aero-server `boot/services.rs::build` 按 R2 条件注入（config 存在 + Q0 谓词）；谓词实现（XRepo 或闭包）按 AGENTS §4.1 配方落 aero-storage 或 boot。
4. **回归**：AC1b/AC2/AC5 既有 pin 全绿；`cargo test -p aero-audit-connector`（含 `forbidden_dead_on_first_attempt`）+ `cargo test -p aero-eng` + aero-auth 单测。
5. **门禁**：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh`。

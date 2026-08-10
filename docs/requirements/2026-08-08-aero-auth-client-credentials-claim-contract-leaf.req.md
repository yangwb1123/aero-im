# Requirements Spec — RFC 9068 client_credentials claim 契约单源化到 aero-common leaf（B5-2：config + claims 类型共享，双验证器消费同一契约）

- **Module (analysis root)**: `crates/aero-auth` — `oidc.rs` 的 `ClientCredentialsTokenConfig` / `ClientCredentialsClaims` / `validate_client_credentials_token` 是契约两份实现之一；落地动作在 `crates/aero-common`（leaf 单源）+ `crates/aero-audit-connector`（消费侧）+ `crates/aero-auth`（leaf 类型 re-export，行为零变化）
- **Direction**: "Single-source the RFC 9068 client_credentials claim contract in aero-common (leaf) shared by oidc.rs and the connector's claim validator"（value 6 / risk_reduction 6 / effort 5 / confidence 6）
- **Source analysis**: `docs/auto/analyses/crates-aero-auth-src-650f2e56.json`（direction #2）
- **Prior art（leaf 模式范本）**: `crates/aero-common/src/model/audit.rs`（`MODERATION_OUTBOUND_ACTION` 单字面量 + 值 pin + aero-storage db_tests 交叉 pin）+ `docs/requirements/2026-08-08-aero-common-b5-contract-vocabulary-leaf-types.req.md`
- **并行方向（本 spec 不实施，见 §7）**: `docs/requirements/2026-08-08-aero-auth-b5-2-relay-claims-unification.req.md` + `docs/design/2026-08-08-aero-auth-b5-2-relay-claims-unification.design.md`（删除 connector 手写验证器、整体委托 aero-auth 验证器——另一种架构，status = Proposed）
- **Status**: Requirements（全部引用经源码 grep + 实跑测试复核）
- **Verification date**: 2026-08-08。行号是复核时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点（AGENTS.md §0）

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-auth/src/oidc.rs:367-455` — `ClientCredentialsTokenConfig` + `validate_client_credentials_token`（iss/aud/scope/sub + at+jwt type） | ✅ 符号命中（范围末端漂移 ~30 行）：`ClientCredentialsTokenConfig { issuer, audience, required_scopes: Vec<String> }` :378-383；`ClientCredentialsClaims { sub, client_id, iat: u64, jti: String, scopes, scope }` :386-394；`granted_scopes()` BTreeSet 并集 :397-406；`validate_client_credentials_token` :420-487：at+jwt typ 白名单 :428-434 → alg 白名单 RS256/EdDSA :434-441 → `Validation::set_issuer/set_audience`（精确 iss/aud）:447-450 → `required_spec_claims` 含 iss/aud/exp/nbf/iat/jti/sub/client_id :452-455 → `valid_identity_component` + `sub == client_id` :461-469 → future-iat 拒 :471-475 → jti 约束 :477-481 → `required_scopes ⊆ granted_scopes` :482-487。`LEEWAY_SECS = 60` :307。**实跑**：`cargo test -p aero-auth --lib` = **85 passed / 0 failed** |
| E2 | `crates/aero-audit-connector/src/client.rs:1-60` — 自有 claim 校验；"'JWKS verification is [PROPOSED]' comment" | ⚠️ **半成立**：模块注释 :1-12 确认 "claim validation (iss/aud/scope/sub) before every POST"；手写验证器 `validate_token_claims` :238 / `validate_token_claims_at` :248-336（shape → base64url 解 payload → iss/aud/scope/sub + exp/nbf validated-when-present，leeway 60s）；POST 前 fail-closed 拒绝臂 :166-176（`invalidate_token` + `DeliveryError::Transient` + "no delivery attempted"）；`ClaimRejection` :79-82；`decode_jwt_claims` :570。**但 "[PROPOSED]" 注释不存在**（前一批次已落地签名面 `verify_token_signature` :346-402，JWKS-on 强制 + Permanent(SignatureRejected) 分类；残留 JWKS-off 跳过臂 :347-349）——核心事实「两份独立实现并存」仍成立（见 §3 对照表） |
| E3 | `crates/aero-audit-connector/tests/claim_validation.rs` — parameterized claim assertions（重复契约的测试 pin） | ✅ 符号命中：24 个测试函数，每测试钉一个 claim 维度且 `assert_eq!(posts, 0)`（wrong_issuer :117 / missing_audience :130 / missing_audit_scope :143 / wrong_subject :156 / opaque :169 / valid :178 + B5-2 exp/nbf :364-444 + JWKS 面 :465-690）。**措辞勘误**：非字面 parameterized（无 proptest 表），是逐维度独立测试函数；夹具 `config()` :21-40 直构 `RelayConfig { expected_iss, expected_aud, expected_scope, expected_sub, jwks_uri: None, … }`——connector 侧无 scope-array 用例（正向全为标量 `scope`）。**实跑**：24 passed / 0 failed |
| E4 | `crates/aero-common/src/model/audit.rs` — 既有 leaf 单源模式 + "truth-check AC4 hard-fail on literal drift" | ✅ leaf 模式成立：`MODERATION_OUTBOUND_ACTION = "admin.content.flag"` :150（"Action-token vocabulary" 区）+ 值 pin `vocabulary_consts_are_pinned` :264-273 + aero-storage `audit_governance.rs` db_tests 交叉 pin（:320/:884，leaf ↔ DDL）。⚠️ **"truth-check AC4 hard-fail" 未实现**：`scripts/truth-check.sh`（212 行，本次全读）只有孤儿模块 + 零调用 builder 两项检查，**无任何字面量扫描**——audit.rs :5-6 的 doc 注释超前于实现（AC4 guard 是文档声称、非现存机制）；本 spec 的 R3 将其落实。另：`audit.rs` 在 git status 中为 **untracked**（leaf 模式本身属前一批次未提交改动） |
| E5 | `crates/aero-audit-connector/src/lib.rs` — 刻意隔离："does not import AiUsageRepo nor touch the v1 table" | ✅ 逐字命中 lib.rs :18-19："The connector deliberately does not import `AiUsageRepo` nor touch the `snaplink_delivery_outbox` (v1) table"。补充：connector `Cargo.toml` **已依赖 `aero-common.workspace = true` + `aero-auth.workspace = true`**（`use aero_common::AuditId`、`use aero_auth::{JwksKeyProvider, KeyProvider}`）——leaf 在 aero-common 消费**零新依赖边**，隔离面（不 import aero-auth 的验证器、不 import aero-storage）保持不变 |

**补充核对（direction 未列、spec 必须处理的现场事实）**：
- aero-auth `Cargo.toml` 已依赖 `aero-common.workspace = true`——leaf 共享零新依赖边；`lib.rs:27-31` 已 re-export `ClientCredentialsClaims` / `ClientCredentialsTokenConfig`（R2 保持此公共面）。
- 生产唯一消费方：`crates/aero-server/src/integrations.rs` `authenticate_machine` :236-247 调 `validate_client_credentials_token`（`rg` 全仓仅 oidc.rs / oidc/tests.rs / lib.rs / integrations.rs 4 文件命中）——R2 必须保持其签名与返回类型可编译。
- 工作树状态：前一批次未提交改动包括 `M crates/aero-auth/{Cargo.toml,src/oidc.rs,src/oidc/tests.rs}`（additive：`decoding_key_fallible` + D10 未知-kid 节流）与 `?? crates/aero-common/src/model/audit.rs` + `M aero-common/{lib.rs,model/mod.rs}`（audit leaf 接线）。本 spec 的 aero-auth 基线 = **当前工作树**；R2 只做类型 re-export 替换，不再增 aero-auth 语义 diff（§5 R2 红线）。
- 门禁接线：`scripts/truth-check.sh` 仅挂在 `make check-truth`（Makefile:66-68），**未入 CI jobs**（ci.yml 无 truth-check）——R3 guard 的"fail"以非零退出码 + 既有 pre-commit 门禁为准（AGENTS §4.3 提交前必过清单含 truth-check）。

## 2. 勘误（2026-08-08 现场 vs direction 引用）

| # | 引用声称 | 实际（已复核） | 影响 |
|---|---|---|---|
| ① | client.rs 有 "'JWKS verification is [PROPOSED]' comment" | 注释已不存在；签名面已落地（`verify_token_signature` :346，JWKS-on 强制 + D10 节流 + Permanent/Transient 分类），残留面 = JWKS-off 跳过臂 + 手写 claims 平面 | 问题陈述的该半句过期；**重复实现的核心事实不变**（§3 对照表） |
| ② | "truth-check AC4 hard-fail on literal drift — the pattern to copy" | audit.rs 的 doc 注释如此声称，但 `scripts/truth-check.sh` 现无字面量扫描；AC4 guard 需**新建** | R3 是新增机制，非"复制现有脚本段"；guard 形态仍照 audit.rs 注释描述的模式落 |
| ③ | "claim_validation.rs parameterized tests" | 24 个独立测试函数（逐维度 pin），非 parameterized 表 | 措辞修正；共享类型后测试面语义不变 |
| ④ | connector "own claim validation" 范围 | 手写面含 iss/aud/scope/sub + exp/nbf when-present；**无 typ/iat/jti/sub==client_id**——比 aero-auth 契约窄（fail-open 漂移面正是 direction 预警的） | §3 对照表逐项列出；本 spec 只统一 iss/aud/scope/sub 四 claim（direction 边界），exp/nbf/iat/jti/typ 均等化属并行方向（§7） |
| ⑤ | "lets the connector consume it without importing aero-auth/aero-storage（preserving its deliberate isolation）" | connector **已依赖 aero-auth + aero-common**（`KeyProvider` 消费）——"不 import aero-auth" 已不成立；"不 import aero-storage / 不 import aero-auth 验证器" 的隔离面保持 | leaf 落 aero-common 后 connector 零新依赖；方向论证方向不变，措辞按现状修正 |

**核心结论成立**：iss/aud/scope/sub 契约的确以**独立语义**实现两份——connector `validate_token_claims_at`（4 claim + when-present exp/nbf，无 typ/iat/jti/sub==client_id）vs aero-auth `validate_client_credentials_token`（8 claim 全契约 + sub==client_id）。两份的 iss/aud/scope/sub 比较逻辑、scope 并集、sub 语义各自手写，任何一侧语义演化（如 required_scopes 从 Vec 改 BTreeSet、aud 从精确改成员）另一侧静默不跟随——direction 预警的 drift 风险属实。

## 3. Verified current state

```
a) aero-auth 验证器（契约实现 #1，已测 85/85）        crates/aero-auth/src/oidc.rs
   validate_client_credentials_token :420 —— typ gate(at+jwt) → alg 白名单(RS256/EdDSA)
     → jsonwebtoken Validation：iss/aud 精确 + exp/nbf/iat/jti/sub/client_id 全 required
     → 手写：valid_identity_component + sub==client_id :461-469 → future-iat 拒 :471-475
     → jti 约束 :477-481 → required_scopes ⊆ granted_scopes() :482-487（scopes 数组 ∪ 空格 scope）
   ClientCredentialsTokenConfig :378 / ClientCredentialsClaims :386 / granted_scopes :397
   契约测试面（oidc/tests.rs）：accepts_..._scopes_array :338 / space_delimited_scope :356 /
     wrong_typ :372 / sub_differs_from_client_id :384 / without_required_scope :398 /
     wrong_issuer_or_audience :412 / expired_and_without_nbf :435 / missing_or_invalid_iat_jti :458 /
     algorithm allowlist :502/:521 —— 共 85 测试全绿（本次实跑）

b) connector 手写验证器（契约实现 #2，已测 24+8+14）  crates/aero-audit-connector/
   validate_token_claims_at :248-336：shape(≤16KB/无控制字符) → decode_jwt_claims :570
     → iss==expected_iss → aud 字符串|数组成员 → scope 字符串|数组成员 → sub==expected_sub
     → exp/nbf validated-when-present（leeway 60s）
   deliver :155-235：签名面(:346, JWKS-off 跳过) → claims 面(:166) → POST；claims 拒 → Transient requeue
   RelayConfig.expected_iss/aud/scope/sub :31-37（config.rs）；TIME_CLAIM_LEEWAY_SECS :40；SCOPE_AUDIT :43
   claim_validation.rs 24 测试（posts==0 pin）+ state_machine.rs 8 测试（lease/backoff/422→dead/403-dead）
   实跑：cargo test -p aero-audit-connector --all-targets = 46 passed / 3 ignored（lib 14 / cv 24 / sm 8）

c) 两份实现语义对照（drift 风险面）
   | 维度 | aero-auth（契约 #1） | connector（契约 #2） |
   | iss | 精确（set_issuer） | 精确（== expected_iss） |
   | aud | 精确单字符串（set_audience） | 字符串|数组成员 |
   | scope | required_scopes ⊆ granted_scopes() 并集 | expected_scope ∈ scope 字符串|数组 |
   | sub | sub == client_id（机器身份） | sub == expected_sub（可配置） |
   | typ | at+jwt 强制 | 不查 |
   | iat | required + future-iat 拒 | 不查 |
   | jti | required + 约束 | 不查 |
   | exp/nbf | required（jsonwebtoken） | validated-when-present（手写，leeway 60） |
   注：四 claim 的"比较逻辑"各自手写实现（connector 无 jsonwebtoken 层）——正是 R1-R3 消除面；
   exp/nbf/iat/jti/typ 的均等化**不在本 direction**（§7 并行方向处理）

d) leaf 模式范本（audit.rs，本次复核）+ 缺口
   MODERATION_OUTBOUND_ACTION 单字面量 :150 + 值 pin 测试 :264-273 + aero-storage db_tests 交叉 pin
   + doc 注释声称的 "truth-check.sh 字面量 hard-fail" —— 脚本内**不存在**（R3 新建）
   aero-common 导出面：model/mod.rs `pub use audit::*`（未提交）；lib.rs 显式 `pub use model::{...}` 清单（未提交）

e) 基线（本次实跑）：aero-auth 85/85；connector 46 passed/3 ignored；b5-pin.sh 37/37（15 executed + 22 [PROPOSED]）
```

## 4. Scope

**In scope（本 direction）**：
- `crates/aero-common/src/model/client_credentials.rs`（新 leaf，镜像 audit.rs 模式）：RFC 9068 client_credentials claim 契约的**类型 + 语义函数 + 值 pin** 单源——`ClientCredentialsTokenConfig`（issuer/audience/required_scopes）、`ClientCredentialsClaims`（sub/client_id 必填；iat/jti Option 化；scopes/scope 双形状）、`granted_scopes()` 并集、四 claim 检查函数（iss 精确 / aud 字符串|数组成员 / scope 字符串|数组成员 / sub 匹配 + `sub==client_id` 助手）、claim 名字常量（`"iss"`/`"aud"`/`"scope"`/`"sub"`/`"client_id"`/`"iat"`/`"jti"`/`"at+jwt"` 等）。
- `crates/aero-auth/src/oidc.rs`：`ClientCredentialsTokenConfig`/`ClientCredentialsClaims` 定义**删除**，改 `pub use aero_common::model::client_credentials::*`（`aero_auth::*` 公共面经 lib.rs:27-31 re-export 链**名字不变**）；验证器签名/行为零变化，仅其手写比较（granted_scopes、sub==client_id、valid_identity_component、required-scope 循环、claim 名字字面量）改调 leaf。`integrations.rs` 编译零改动（返回类型同型）。
- `crates/aero-audit-connector/src/client.rs`：`validate_token_claims_at` 的 payload 解码改走 leaf claims 类型（serde），四 claim 比较改调 leaf 检查函数；`RelayConfig` 构造期建一次 leaf `ClientCredentialsTokenConfig`（issuer=expected_iss、audience=expected_aud、required_scopes=vec![expected_scope]）。**行为 delta（均 fail-closed，见 §5 R2）**：缺 sub/client_id 的 token 由"通过/明确拒"变为"serde 解码即拒"；iat/jti 缺席仍容忍（Option 字段）；exp/nbf when-present 检查**保留在 client.rs**（不在本 direction 契约内）。
- `crates/aero-audit-connector/tests/claim_validation.rs` + `src/stub.rs`：夹具随迁（stub 默认 token_claims 补 `client_id`；逐 claim 用例语义不变）；**新增 1 个 pin**：缺 client_id 的合法形状 token → 拒 + posts==0。
- `scripts/truth-check.sh`：**新增字面量/类型重复 hard-fail 段**（R3）——落实 audit.rs doc 注释声称的 AC4 模式，覆盖本契约。

**Out of scope（并行方向/既有面——本 direction 不建、不改）**：
- **删除 connector 手写验证器 / 整体委托 `aero_auth::validate_client_credentials_token` / JWKS-off 面关闭 / `AERO_AUDIT_EXPECTED_SUB` 删除 / exp-nbf-iat-jti-typ 必填化**——全部属 `docs/requirements/2026-08-08-aero-auth-b5-2-relay-claims-unification.req.md`（Proposed，未实施），本 spec 不预支其行为变更（§7）。
- aero-auth 验证器语义（typ/alg/required_spec_claims/iat/jti/sub==client_id 策略）零变化；`integrations.rs` 零改动；snaplink v1 零改动。
- relay 状态机（lease/backoff/Forbidden/Permanent/Transient 三臂、422→dead、403→dead）**零行为变更**。
- 0239-0241 / `audit_governance.rs` / priority（B5-3）/ 配给门（B5-4）——不动。
- `valid_identity_component` 之外的 connector 行为（payload guard、receipt、token cache）——不动。
- CI 接线策略（truth-check 目前仅 pre-commit 门禁）——不动；guard 落 `make check-truth` 既有门禁。

## 5. Requirements

### R1 — aero-common leaf：claim 契约（类型 + 语义 + 常量 + 值 pin）单源

`crates/aero-common/src/model/client_credentials.rs`（镜像 `audit.rs` 模式：doc 头声明唯一合法字面量站点 + 常量 + 类型 + 语义函数 + `#[cfg(test)]` 值 pin）：

```rust
/// RFC 9068 client_credentials claim 名字（唯一合法字面量站点；消费方经此常量引用，
/// 禁止裸字符串）。oidc.rs 的 required_spec_claims 与 connector 的 Value 读取均走常量。
pub const CLAIM_ISS: &str = "iss";
pub const CLAIM_AUD: &str = "aud";
pub const CLAIM_SCOPE: &str = "scope";          // OAuth 空格分隔兼容 claim
pub const CLAIM_SCOPES: &str = "scopes";        // RFC 9068 数组 claim
pub const CLAIM_SUB: &str = "sub";
pub const CLAIM_CLIENT_ID: &str = "client_id";
pub const CLAIM_IAT: &str = "iat";
pub const CLAIM_JTI: &str = "jti";
pub const TOKEN_TYPE_AT_JWT: &str = "at+jwt";   // JOSE typ（application/at+jwt 变体亦常量）

/// 共享验证配置（oidc.rs 现 ClientCredentialsTokenConfig 同构迁入）。
#[derive(Debug, Clone)]
pub struct ClientCredentialsTokenConfig {
    pub issuer: String,
    pub audience: String,
    pub required_scopes: Vec<String>,
}

/// 共享 claims 类型。sub/client_id 必填（RFC 9068 核心 + aero-auth 现行契约）；
/// iat/jti Option 化 = 两消费方的严格度策略各自保留（aero-auth 经 jsonwebtoken
/// required_spec_claims 强制；connector 维持 when-present 容忍）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ClientCredentialsClaims {
    pub sub: String,
    pub client_id: String,
    pub iat: Option<u64>,
    pub jti: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

impl ClientCredentialsClaims {
    /// scopes 数组 ∪ scope 空格分隔并集（唯一实现点；oidc.rs 与 connector 都经此）。
    #[must_use]
    pub fn granted_scopes(&self) -> BTreeSet<&str> { /* 迁自 oidc.rs:397-406 */ }
    /// 机器身份约束：sub == client_id（oidc.rs 唯一消费方）。
    #[must_use]
    pub fn subject_is_client_id(&self) -> bool { /* 迁自 oidc.rs:461-469 的 sub 半边 */ }
    /// 身份组件合法性（非空/无控制字符/无首尾空白）。
    #[must_use]
    pub fn is_valid_identity_component(&self, value: &str) -> bool { /* 迁自 oidc.rs */ }
    /// required_scopes 全部命中 granted_scopes()（唯一实现点）。
    #[must_use]
    pub fn has_required_scopes(&self, cfg: &ClientCredentialsTokenConfig) -> bool { /* 迁自 oidc.rs:482-487 */ }
}

/// 四 claim 检查（connector 消费面；iss 精确 / aud 字符串|数组成员 / scope 字符串|数组成员 /
/// sub 精确）。输入 `&serde_json::Value` 以保留两消费方的解码形状差异。
pub fn check_issuer(claims: &Value, cfg: &ClientCredentialsTokenConfig) -> bool;
pub fn check_audience(claims: &Value, cfg: &ClientCredentialsTokenConfig) -> bool;
pub fn check_scope(claims: &Value, cfg: &ClientCredentialsTokenConfig) -> bool;
pub fn check_subject(claims: &Value, expected_sub: &str) -> bool;
```

导出：`model/mod.rs` `pub mod client_credentials; pub use client_credentials::*;` + `lib.rs` 显式清单追加（镜像 audit.rs 的未提交接线）。

### R2 — 双消费方只经 leaf 消费契约（行为零变化 + 2 个 fail-closed delta）

**`crates/aero-auth/src/oidc.rs`**：
- 删除 `ClientCredentialsTokenConfig`/`ClientCredentialsClaims` 定义，改 `pub use aero_common::model::client_credentials::{...}`（`aero_auth::ClientCredentialsClaims`/`ClientCredentialsTokenConfig` 经 lib.rs:27-31 re-export 链名字不变——`integrations.rs` 编译零改动）。
- `validate_client_credentials_token` **签名与行为不变**：typ gate / alg 白名单 / jsonwebtoken Validation（含 required_spec_claims）逐字保留；其**手写比较**（granted_scopes、sub==client_id、valid_identity_component、required-scope 循环、claim 名字字面量）改调 leaf（`claims.granted_scopes()`、`subject_is_client_id()`、`is_valid_identity_component`、`has_required_scopes`、`CLAIM_*` 常量）。
- 红线：本 direction 的 aero-auth diff **仅限**上述 re-export/调用替换——不增语义、不改签名、不动 `decoding_key_fallible`/D10（前一批次未提交 additive 改动为基线）。

**`crates/aero-audit-connector/src/client.rs`**：
- 构造期建一次 leaf 配置：`ClientCredentialsTokenConfig { issuer: config.expected_iss.clone(), audience: config.expected_aud.clone(), required_scopes: vec![config.expected_scope.clone()] }`。
- `validate_token_claims_at`：payload 解码改 `serde_json::from_value::<ClientCredentialsClaims>(...)`（缺失 sub/client_id → 拒，与今日"token has no sub claim"同臂）；iss/aud/scope/sub 比较改调 `check_issuer/check_audience/check_scope/check_subject`；exp/nbf when-present 检查**保留**（不在本契约）；`ClaimRejection` 形态不变（reason 文案可保留）。
- **行为 delta（全部 fail-closed，逐条 pin）**：① 缺 `client_id` 的 token 由"通过"变"拒"（RFC 9068 对齐，direction 预警的 fail-open 面收口）；② 缺 `sub` 的 token 由显式检查变 serde 解码拒（同臂同结果）；③ 形状错误（如 `iat` 为字符串）由容忍变拒。三条均落 `DeliveryError::Transient` → requeue，**永不 dead**——relay 三臂分类零变化。

### R3 — truth-check 字面量/类型重复 hard-fail（AC4 模式落实）

`scripts/truth-check.sh` 新增第三段（audit.rs doc 注释声称的机制，本次落地）：
- **类型单源**：`struct ClientCredentialsClaims` / `struct ClientCredentialsTokenConfig` 的**定义**只允许出现在 `crates/aero-common/src/model/client_credentials.rs`（其余 crate 命中 = 硬违规，计入退出码）。
- **字面量单源**：`CLAIM_*`/`TOKEN_TYPE_AT_JWT` 的**裸字符串值**（`"iss"`/`"aud"`/`"scope"`/`"scopes"`/`"sub"`/`"client_id"`/`"iat"`/`"jti"`/`"at+jwt"` 作为 Rust 字符串字面量）只允许出现在 leaf 文件内；消费方（oidc.rs / client.rs）必须经常量引用。豁免清单（镜像既有 design 的"测试夹具字面量豁免"）：`tests/` 目录、`src/stub.rs`、`#[cfg(test)]` 夹具区——夹具 JSON 里的 claim 名是**被测数据**非逻辑。
- 脚本自带负例自测（镜像 `scripts/test-b5-pin-guard.sh` 模式）：临时在非豁免生产文件种一个 `"iss"` 字面量 → guard 非零退出；种一个 `struct ClientCredentialsClaims` → 非零退出。

### R4 — relay 状态机零行为变更（约束）

- relay 状态机（lease/backoff/Forbidden→dead/Permanent 422-409-receipt→requeue×1→dead/Transient→requeue）**零改动**；claims 拒绝仍先于任何 delivery POST（`deliver` :166-176 臂结构不变，仅内部改调 leaf）。
- 测试面：`state_machine.rs` 8 用例（fixture 随 stub 默认 claims 补 `client_id`，断言不变）、`claim_validation.rs` 24 用例（语义不变，新增 1 个缺 client_id pin）。

## 6. Testable acceptance mapping（direction 三条 acceptance 原样保留，逐条可执行化）

| # | Acceptance（direction 原文） | 可执行断言 / 命令 |
|---|---|---|
| A1 | "Both validators consume the shared leaf types: oidc.rs tests and connector claim_validation.rs parameterized cases run against the same ClientCredentialsTokenConfig/claims structs — a required-scope or audience semantic change in one is a compile error in the other, not a silent drift." | ① **类型同一性**：`rg -n "struct ClientCredentialsClaims" crates/ --glob '*.rs'` 恰 1 命中且位于 `crates/aero-common/src/model/client_credentials.rs`（`ClientCredentialsTokenConfig` 同）；oidc.rs 以 `pub use` 引用（`rg -n "pub use aero_common::model::client_credentials" crates/aero-auth/src/oidc.rs` ≥1），client.rs 以 `use aero_common::model::client_credentials::` 引用——**同一类型，无第二定义**。② **编译错误性质**：修改 leaf `ClientCredentialsClaims` 字段（如删 `client_id`）或 `ClientCredentialsTokenConfig.required_scopes` 类型 → `cargo check -p aero-auth -p aero-audit-connector` 两侧同时编译失败（负例 drill，验证后还原）。③ **语义单点**：`granted_scopes()` / `has_required_scopes` / `check_*` 函数体仅存在于 leaf（R3 guard 同扫）；oidc/tests.rs 与 claim_validation.rs 全量绿（A1 面语义变化 = leaf 单点编辑）。 |
| A2 | "truth-check-style hard-fail (mirroring AC4 for admin.content.flag): duplicate iss/aud/scope validation logic outside the leaf fails CI." | ① guard 落 `scripts/truth-check.sh`（R3），`bash scripts/truth-check.sh` 在种植重复（非豁免文件出现 `"iss"` 字面量或重复 struct 定义）时**非零退出**、干净树零退出；guard 负例自测随 `scripts/test-b5-pin-guard.sh` 模式跑绿。② 接线：guard 经既有 `make check-truth` 门禁（AGENTS §4.3 提交前必过清单）+ 说明：truth-check 目前不在 CI jobs（现场核实），"fails CI"按仓库既有门禁语义执行——若实施期将 check-truth 折入 CI，guard 自动随行。③ 镜像 pin：`crates/aero-common/src/model/audit.rs` doc 声称的 AC4 字面量 hard-fail 与本 guard 同段落实（`"admin.content.flag"` 裸字面量只允许在 audit.rs），使 audit.rs 注释与脚本一致。 |
| A3 | "No behavior change to relay state machine: state_machine.rs (lease/backoff/422→dead-terminal/403-dead) still passes with the shared types; fail-closed claim rejection still precedes any delivery POST." | ① `cargo test -p aero-audit-connector --all-targets` = **46 passed / 3 ignored 不降**（lib 14 / claim_validation 24+1 新增 / state_machine 8；仅夹具字面量随迁，断言零改动）；`state_machine.rs` 的 lease/backoff/`permanent_error_dead_after_exactly_two_attempts`/403-dead 用例逐字绿。② **claims 拒绝先于 POST**：claim_validation.rs 全部 `assert_eq!(posts, 0)` 断言保持绿（wrong_issuer :117 / missing_audience :130 / missing_audit_scope :143 / wrong_subject :156 / opaque :169 / expired :364 / future_nbf :389 / non_numeric :430 + **新增 `token_without_client_id_is_rejected_before_any_post`**（posts==0，pin R2 delta ①））。③ `cargo test -p aero-auth --lib` = **85/85 不降**；`cargo clippy --workspace --all-targets` 无新增警告；`cargo check --workspace` 干净。 |

## 7. 并行方向关系（不实施，仅记录）

`docs/requirements/2026-08-08-aero-auth-b5-2-relay-claims-unification.req.md`（Proposed）设计的是**另一种架构**：删除 connector 手写验证器、`deliver` claims 面整体委托 `aero_auth::validate_client_credentials_token`、JWKS-off 面关闭、`expected_sub` 删除、exp/nbf/iat/jti/typ 必填化、aero-auth 零 diff 红线。本 spec（leaf 单源）与其**兼容但不重合**：leaf 类型是两者共同的底层契约——本 spec 落地后，并行方向实施时其 claims 面替换/删除面自动获得共享类型基础（`ClientCredentialsTokenConfig` 已在 leaf，构造点已存在）。本 spec 明确不预支并行方向的任何行为变更（删除面、必填化、JWKS 强制），避免两个未评审设计相互耦合。

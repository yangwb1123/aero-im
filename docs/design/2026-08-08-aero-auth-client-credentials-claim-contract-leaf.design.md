# Design — RFC 9068 client_credentials claim 契约单源化到 aero-common leaf（B5-2 leaf 方向）

- **Requirements**: `docs/requirements/2026-08-08-aero-auth-client-credentials-claim-contract-leaf.req.md`（R1–R4，本设计逐条落地）
- **Module (analysis root)**: `crates/aero-common`（新 leaf 单源）· `crates/aero-auth`（re-export 消费方，行为零变化）· `crates/aero-audit-connector`（typed-gate 消费方，3 个 fail-closed delta）· `scripts/truth-check.sh`（§3 guard 新建）
- **Design status**: Proposed（待 design-gate 评审）；**§1.5 guard（R3）本批次已实现并验证**（leaf/消费方/夹具迁移已在工作树落地为在途改动，guard 是剩余的最后一步）
- **Verification date**: 2026-08-08。行号为复核时锚点，可漂移——**文件/符号**才是稳定 grep 锚点（AGENTS.md §0）。

## 0. Evidence verification（untrusted claims → live tree 逐条复核）

| Evidence（需求 spec 引用） | Live-tree 复核结果 |
|---|---|
| E1 `oidc.rs:367-455` — `ClientCredentialsTokenConfig` / `ClientCredentialsClaims` / `granted_scopes()` / `validate_client_credentials_token` | ✅ 符号全命中，范围尾漂移 ~30 行（与 spec 自述一致）：config :378-383 / claims :386-394 / `granted_scopes` :401-406 / validator :420-487（typ gate :428-434 → alg 白名单 :434-441 → iss/aud 精确 :447-450 → `required_spec_claims` 8 项 :452-455 → sub==client_id :461-469 → future-iat :471-475 → jti :477-481 → scope ⊆ granted :482-487）。`cargo test -p aero-auth --lib` 实跑 **85/85** |
| E2 `client.rs` 手写验证器 + "[PROPOSED] JWKS verification" 注释 | ⚠️ 与 spec 勘误一致：`validate_token_claims_at` :248-336 属实（shape → `decode_jwt_claims` :570 → iss 精确 :262-267 → aud 字符串\|数组 :270-284 → scope 字符串空格拆分\|数组 :287-300 → sub 精确 :303-311 → exp/nbf when-present :317-334）；**"[PROPOSED]" 注释不存在**（`rg PROPOSED` = 0）；签名面 `verify_token_signature` :346 已落地，JWKS-off 跳过臂仍残留（:127-165 `jwks_uri.as_ref().map(...)`、:347-349 `let Some(keys) = … else { return Ok(()) }`）；POST 前 fail-closed 拒绝臂 :166-176 属实 |
| E3 `claim_validation.rs` 24 tests + `posts==0` pins | ✅ **路径勘误：文件在 `tests/claim_validation.rs`（非 `src/`）**。24 个独立测试函数（非字面 parameterized，与 spec 勘误一致）；`posts==0` 逐维 pin 属实（:126/:139/:152/:165/:174/:384/:409/:440/:453/:485）；connector 侧无 scope-array 用例（`rg '"scopes"' crates/` = 0 命中，正向全为标量 `scope`）。实跑 **24/24** |
| E4 `audit.rs` leaf 模式 + "truth-check AC4 hard-fail" | ✅/⚠️ leaf 模式属实：`MODERATION_OUTBOUND_ACTION = "admin.content.flag"` :150 + 值 pin `vocabulary_consts_are_pinned` :264-273。⚠️ **AC4 hard-fail 在旧 spec 时点未实现**（truth-check.sh 只有孤儿+unwired 两段）——**本批次已实现**：§1.5 guard 落地（`truth-check-lib.sh` + 接线 + 自测），`"admin.content.flag"` 镜像扫描实扫 2 命中全在 audit.rs |
| E5 `lib.rs` 隔离注记 | ✅ 逐字命中 :18-19（"deliberately does not import `AiUsageRepo` nor touch the `snaplink_delivery_outbox`"）；connector `Cargo.toml` **已依赖 `aero-common` + `aero-auth`**——leaf 消费零新依赖边；隔离面（不 import aero-auth 验证器、不 import aero-storage）保持 |

**基线（本次实跑）**：aero-auth `85 passed`；connector `46 passed / 3 ignored`（lib 14 / claim_validation 24 / state_machine 8——relay 状态机 lease/backoff/422→dead/403-dead 全绿）。

### 设计级新发现（spec 未覆盖、本设计必须处理的现场事实）

| # | 发现 | 对设计的影响 |
|---|---|---|
| F1 | **R3 全局字面量扫描会误伤**：`"iss"`/`"aud"`/`"scope"`/`"sub"`/`"client_id"`/`"iat"`/`"jti"` 是通用 JWT/OAuth 词汇，已在**其他契约**的生产代码中使用——`sso.rs`（OIDC authorization-code 表单参数 `"client_id"`/`"scope"` :385-407 + id_token claim 解析 `"iss"` :545）、`snaplink_commercial/http.rs`（v1 `"scope"`）、`aero-storage`（vault 表单参数与记录列 `"scope"`/`"client_id"`：`blob.rs`/`blob_gc.rs`/`admin_revoke.rs`/`aero_vault_blob_store.rs`/`integration*.rs`）、`live.rs:582`（聊天频道名 `"sub"`——纯词汇误报）、`extractor.rs:173/191`（`#[test]` 夹具 `jti: "jti".into()`）、`jwt.rs:5`（仅注释）。req R3 字面措辞「只允许出现在 leaf 文件内」**不可直接实现**——需豁免清单（§1.5 修正） | §1.5 guard 设计为「全局扫描 + 手工策展 allowlist（逐条契约理由）」，任何 allowlist 外新命中 = 硬违规 |
| F2 | **夹具迁移面比 spec 记的大**：stub 默认 `token_claims`（:91-96）**无 `client_id`**（仅 iss/aud/scope/sub）；`claim_validation.rs` 17 处 `json!(`；`relay.rs` :464-467 测试夹具；`state_machine.rs` 8 用例全走 `SinkBehavior::default()`。typed gate 上线后缺 `client_id` 的 token 全部拒——**不迁夹具 = 46→0 全崩** | §1.5 夹具随迁是 R2 的**前置步骤**（与测试断言变更分离） |
| F3 | `oidc/tests.rs` 用**本地铸币结构** `ClientAccessTokenClaims`（:44-63，非生产类型字面构造）——leaf 将 `iat`/`jti` Option 化**不破坏任何 aero-auth 构造点**；测试只经 `validate_client_credentials_token` 返回值消费生产类型（`granted_scopes()` :352/:368）。**勘误**：:351 有一处**断言站点**（`assert_eq!(claims.jti, "access-token-1")`）在 Option 化后不编译，需适配为 `as_deref() == Some(...)`——见 §1.4 | Option 化安全，`oidc/tests.rs` 仅 1 处断言适配 |
| F4 | connector 现行契约**完全不查** `iat`/`jti`/`client_id`（任意类型容忍）；`exp`/`nbf` when-present 是 `client.rs` 内 `Value` 读取（:317-334） | 3 个 fail-closed delta 精确化（§1.4）：缺 `client_id` 拒（闭 fail-open）、缺 `sub` 改 serde 拒（同臂同结果）、`iat`/`jti` 非字符串拒（新增面）；`exp`/`nbf` 面**保留在 client.rs**（不在统一契约内） |
| F5 | `truth-check.sh` 退出码 = 仅 `orphan_violations`（末行 `exit "$orphan_violations"`）；UNWIRED 只警告 | §3 guard 必须是硬违规，需**并入退出码计算**（`exit $((orphan_violations + literal_violations))`） |
| F6 | `oidc.rs` 的 `required_spec_claims` 循环含 `"exp"`/`"nbf"`（:452-455），不在 req R1 的 8 常量清单内；connector 侧 exp/nbf 也是 Value 读取 | 统一契约 = iss/aud/scope/sub 四 claim（direction 边界）；`exp`/`nbf` 字面量保留在 oidc.rs 循环（带注释），**不入 guard 扫描清单**——避免「消费方必须全常量」与「exp/nbf 不在契约」矛盾 |
| F7 | **工作树已是 post-implementation**：leaf（含 `ClientCredentialsGateClaims`——security_engineer delta ④ 已落地）、oidc.rs/client.rs 常量替换、stub/relay/claim_validation 夹具随迁全部在途未提交；实扫 122 命中已无 cc-token 裸字面量 | §3 guard 是剩余的最后一步（旧 §4 step 5）；allowlist 按**当前行号** pin，自测「真实树干净」用例使其可复现；行号漂移由 STALE 警告兜底 |
| F8 | 旧 §1.5(a) 命令 `rg -nE 'struct ...'` **不可执行**：ripgrep 的 `-E` = `--encoding`，报 `unknown encoding` | 改用 `rg -n -e`（ERE 为 rg 默认）；§1.5 已勘误 |
| F9 | usage 侧扫描的 allowlist 需含 `crates/aero-common/src/lib.rs`（导出面逐名列出 leaf 函数）与 `crates/aero-auth/src/oidc/id_token.rs`（id_token 验证器共享 `is_valid_identity_component`） | 已纳入 §1.5(c) |

## 1. API changes（before → after，逐符号）

### 1.1 NEW — `crates/aero-common/src/model/client_credentials.rs`（leaf 单源）

镜像 `model/audit.rs` 模式（doc 头声明唯一合法站点 + 常量 + 类型 + 语义函数 + `#[cfg(test)]` 值 pin）：

```rust
//! RFC 9068 client_credentials claim contract — single source of truth.
//! Twin consumers: aero-auth `oidc.rs` (full validator) and the audit
//! connector's claims plane. This file is the only legal definition site for
//! the types and the only legal spelling of the claim names —
//! `scripts/truth-check.sh` §3 hard-fails on drift (AC4 pattern, mirroring
//! `model/audit.rs`). `exp`/`nbf` are deliberately NOT here: they are outside
//! the unified four-claim contract (connector keeps its when-present Value
//! checks; aero-auth keeps its jsonwebtoken Validation).

/// Claim-name constants — the only legal spelling. Consumers must reference
/// these; bare literals are a truth-check §3 violation outside this file.
pub const CLAIM_ISS: &str = "iss";
pub const CLAIM_AUD: &str = "aud";
pub const CLAIM_SCOPE: &str = "scope";
pub const CLAIM_SCOPES: &str = "scopes";
pub const CLAIM_SUB: &str = "sub";
pub const CLAIM_CLIENT_ID: &str = "client_id";
pub const CLAIM_IAT: &str = "iat";
pub const CLAIM_JTI: &str = "jti";
pub const TOKEN_TYPE_AT_JWT: &str = "at+jwt";
pub const TOKEN_TYPE_AT_JWT_APPLICATION: &str = "application/at+jwt";

/// Shared verification policy (moved verbatim from oidc.rs:378-383).
#[derive(Debug, Clone)]
pub struct ClientCredentialsTokenConfig {
    pub issuer: String,
    pub audience: String,
    pub required_scopes: Vec<String>,
}

/// Shared claims type (moved from oidc.rs:386-394). `sub`/`client_id`
/// required (`client_id` is a **Snaplink extension claim** — its RFC-9068
/// consistency comes only from the `sub == client_id` rule); `iat`/`jti`
/// Option-ized so each consumer's strictness policy is preserved:
/// aero-auth enforces presence in the validator body (jsonwebtoken's
/// `required_spec_claims` does NOT cover iat/jti — see §1.2 future-iat
/// row), the connector tolerates absence as today. A present non-numeric
/// `iat` (including fractional NumericDate) or non-string `jti` now fails
/// serde on both sides (connector delta ③, fail-closed).
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

/// **Minimal typed gate** (the connector's typed-gate target, delta ④
/// resolution): `sub`/`client_id`/`iat`/`jti` ONLY. `scope`/`scopes` are
/// deliberately absent — the connector's scope check stays on the Value
/// path ([`check_scope`]) so array-form `scope` tokens keep passing
/// (dual-shape tolerance; gating on a `String`-typed `scope` would
/// regress them and dead the leaf's array branch on the connector face).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ClientCredentialsGateClaims {
    pub sub: String,
    pub client_id: String,
    #[serde(default)]
    pub iat: Option<u64>,
    #[serde(default)]
    pub jti: Option<String>,
}

impl ClientCredentialsClaims {
    /// `scopes` array ∪ space-delimited `scope` union (moved from
    /// oidc.rs:401-406). Sole implementation point.
    #[must_use]
    pub fn granted_scopes(&self) -> std::collections::BTreeSet<&str> { /* 原样迁移 */ }

    /// Machine identity: `sub == client_id` (moved from oidc.rs:461-469).
    #[must_use]
    pub fn subject_is_client_id(&self) -> bool { /* 原样迁移 */ }

    /// Every configured required scope granted (moved from oidc.rs:482-487).
    #[must_use]
    pub fn has_required_scopes(&self, cfg: &ClientCredentialsTokenConfig) -> bool { /* 原样迁移 */ }
}

/// Identity component validity: non-empty / no control chars / trimmed
/// (moved from oidc.rs free fn `valid_identity_component`; kept a free fn —
/// 修 req 草稿里 `&self` 未用的方法签名).
#[must_use]
pub fn is_valid_identity_component(value: &str) -> bool { /* 原样迁移 */ }

/// Value-shape claim checks — the connector consumption face. Input
/// `&serde_json::Value` preserves the two consumers' decode-shape
/// differences (connector reads the raw payload; aero-auth checks via
/// jsonwebtoken Validation + typed face above). Semantics = verbatim move
/// of client.rs:262-311 (iss 精确 / aud 字符串|数组成员 / scope 字符串
/// 空格拆分|数组成员 / sub 精确). Return `bool`; the connector keeps its
/// "has no X claim" vs "does not match" reason split via a presence
/// pre-check (presentation, not contract logic).
#[must_use]
pub fn check_issuer(claims: &serde_json::Value, cfg: &ClientCredentialsTokenConfig) -> bool;
#[must_use]
pub fn check_audience(claims: &serde_json::Value, cfg: &ClientCredentialsTokenConfig) -> bool;
#[must_use]
pub fn check_scope(claims: &serde_json::Value, cfg: &ClientCredentialsTokenConfig) -> bool;
#[must_use]
pub fn check_subject(claims: &serde_json::Value, expected_sub: &str) -> bool;

#[cfg(test)]
mod tests {
    // 值 pin（镜像 audit.rs `vocabulary_consts_are_pinned`）：
    // CLAIM_ISS=="iss" … TOKEN_TYPE_AT_JWT_APPLICATION=="application/at+jwt"
    // + granted_scopes 并集（数组 ∪ 空格串）+ check_* 双形状（字符串|数组）+
    // 缺 sub/client_id 的 Value → serde 拒（typed gate 负例）。
}
```

**导出接线**（镜像 audit.rs 的未提交接线）：`model/mod.rs` 增 `pub mod client_credentials; pub use client_credentials::*;`（现 :6-20 模式）；`lib.rs` :40-49 显式清单追加 `ClientCredentialsClaims, ClientCredentialsGateClaims, ClientCredentialsTokenConfig, CLAIM_ISS, …, TOKEN_TYPE_AT_JWT_APPLICATION, check_issuer, check_audience, check_scope, check_subject, is_valid_identity_component`（**不可**只靠 `pub use client_credentials::*`——aero-common 的公共面是显式清单，漏加 = 消费方编译错误，属良性失败）。

### 1.2 `crates/aero-auth/src/oidc.rs`（re-export + 调用替换，行为零变化）

| Symbol | Before | After |
|---|---|---|
| `ClientCredentialsTokenConfig` :378-383 | 本地定义 | **删除定义**；`pub use aero_common::model::client_credentials::{ClientCredentialsClaims, ClientCredentialsTokenConfig};`（lib.rs:28-29 re-export 链 `pub use oidc::{…}` 名字不变 → `aero_auth::*` 公共面不变 → `integrations.rs` 编译零改动） |
| `ClientCredentialsClaims` :386-394 + `granted_scopes` :397-406 | 本地定义 | 同删除/re-export |
| validator typ gate :428-434 | `Some(kind) if kind.eq_ignore_ascii_case("at+jwt") \|\| kind.eq_ignore_ascii_case("application/at+jwt")` | 字面量换 `TOKEN_TYPE_AT_JWT` / `TOKEN_TYPE_AT_JWT_APPLICATION`（`eq_ignore_ascii_case` 语义保留） |
| `required_spec_claims` 循环 :452-455 | `for claim in ["iss", "aud", "exp", "nbf", "iat", "jti", "sub", "client_id"]` | `for claim in [CLAIM_ISS, CLAIM_AUD, "exp", "nbf", CLAIM_IAT, CLAIM_JTI, CLAIM_SUB, CLAIM_CLIENT_ID]`（exp/nbf 保留字面量 + `// outside the unified four-claim contract` 注释，F6） |
| 机器身份检查 :461-469 | `!valid_identity_component(&claims.sub) \|\| !valid_identity_component(&claims.client_id) \|\| claims.sub != claims.client_id` | `!is_valid_identity_component(&claims.sub) \|\| !is_valid_identity_component(&claims.client_id) \|\| !claims.subject_is_client_id()` |
| future-iat :471-475 | `claims.iat > get_current_timestamp() + LEEWAY` | `!claims.iat.is_some_and(\|iat\| iat <= get_current_timestamp() + LEEWAY)`（**必改，否则 fail-open**：jsonwebtoken 9.3.1 的 `required_spec_claims` 只对硬编码集合 exp/sub/iss/aud/nbf 生效，iat/jti/client_id 命中 `_ => continue`——其存在性此前是**非 Option serde 字段结构性强制的**。Option 化后缺 iat 的 token 会解码为 `None` 并放行；合并检查把「缺 iat」与「future iat」一并拒，行为等价，`rejects_missing_or_invalid_client_credentials_iat_and_jti` 五用例原样绿。jti 缺省由下方有效性块 `!is_some_and` 拒，client_id 仍是 String 结构性强制） |
| jti 有效性 :477-481 | `claims.jti.is_empty() \|\| claims.jti.len() > 1_024 \|\| claims.jti.chars().any(char::is_control)` | `!claims.jti.as_deref().is_some_and(\|jti\| !jti.is_empty() && jti.len() <= 1_024 && !jti.chars().any(char::is_control))`（Option 化后唯一可编译形式；语义逐字等价：空串/超长/控制字符仍拒，缺省（None）也拒——fail-closed） |
| scope 检查 :482-487 | `cfg.required_scopes.iter().any(\|r\| !granted.contains(...))` | `!claims.has_required_scopes(cfg)`（`granted_scopes` 并集逻辑已在 leaf） |
| `valid_identity_component` 自由函数 :487-491 | oidc.rs 私有 fn | **删除**；两处消费方改指 leaf `is_valid_identity_component`：cc-validator 机器身份块（本表上行）+ **id_token.rs :56**（ID-token sub 有效性——设计初版漏掉的第二消费点，同一语义同一单源） |

**红线**：aero-auth diff 仅限上述 re-export/调用替换——不增语义、不改 `validate_client_credentials_token` 签名（`Result<ClientCredentialsClaims, OidcError>` 不变，返回类型同型）、不动 `decoding_key_fallible`/D10（前一批次 additive 基线）。检查**顺序**逐字保留（typ → alg → key → Validation → 身份 → future-iat → jti → scope），85/85 是顺序/语义不变的证明。

### 1.3 `crates/aero-audit-connector/src/client.rs`（typed gate + check_* 调用，3 个 fail-closed delta）

- **构造期**（`new` :126 / `with_key_provider` :135）各建一次 leaf 配置：
  `ClientCredentialsTokenConfig { issuer: config.expected_iss.clone(), audience: config.expected_aud.clone(), required_scopes: vec![config.expected_scope.clone()] }`（`RelayConfig` 四个 `expected_*` 字段 :33-36 **不动**——`AERO_AUDIT_*` env 语义零变化）。
- `validate_token_claims_at` :248-336 内部替换：
  1. shape 检查（空/≤16KB/控制字符）**保留**；
  2. `decode_jwt_claims` :570 → `Value` **保留**；
  3. **新增 typed gate（minimal 结构体，修复 array-form scope 回归）**：`serde_json::from_value::<ClientCredentialsGateClaims>(claims.clone())`（leaf 的 minimal 门结构体，**只含 `sub`/`client_id`/`iat`/`jti`**）→ Err → `ClaimRejection { reason: "token claims missing required fields (sub/client_id)" }`（fail-closed，delta ①②③）。`scope`/`scopes` **刻意不进** 门结构体——scope 检查继续走 Value 路径 `check_scope`（双形状），否则 `"scope": [...]` 数组形 token 从今日「通过」变「拒」，且 leaf `check_scope` 的数组成员臂在 connector 面成死代码（自相矛盾）。备选方案（untagged `ScopeValue` enum 并入全量结构体）**否决**：那会放宽 aero-auth 的 decode 容忍度（字符串→字符串|数组），破坏「aero-auth 行为零变化」红线；
  4. iss/aud/scope/sub 四个比较块 :262-311 → `check_issuer(&claims, &self.cc_cfg)` / `check_audience` / `check_scope` / `check_subject(&claims, &self.config.expected_sub)`；reason 文案保留（"has no X claim" vs "does not match" 经 presence 预检选择）；
  5. exp/nbf when-present 块 :317-334 **逐字保留**（不在统一契约，F6）。
- 拒绝分类**不变**：仍走 :166-176 臂 → `DeliveryError::Transient` → requeue，**永不 dead**。

**行为 delta（全部 fail-closed，逐条 pin）**：

| delta | 面 | 今日 | 门后 | pin |
|---|---|---|---|---|
| ① 缺 `client_id` | fail-open 面 | 通过（connector 从不查 client_id） | serde 拒（Transient → requeue，永不 dead） | `token_without_client_id_is_rejected_before_any_post`（`posts==0`） |
| ② 缺 `sub` | 显式检查面 | 拒（reason "token has no sub claim"） | serde 拒（reason "token claims missing required fields (sub/client_id)"，同臂同结果；reason 文案不 pin） | 同上（断言 Transient + `posts==0`） |
| ③ `iat` 非整数（含小数 NumericDate）/ `jti` 非字符串 | fail-open 面 | 容忍（完全不查） | serde 拒（`Option<u64>`/`Option<String>`） | `fractional_iat_is_rejected_before_any_post`（`posts==0`） |
| ④ **scope 数组形**（评审发现的第 4 个 delta，已解析） | 双形状容忍面 | 接受（Value 字符串空格拆分\|数组成员） | **仍接受**——门结构体不含 scope，Value 路径 `check_scope` 保留；**非回归**，正例 pin | `array_form_scope_token_is_accepted_and_delivery_proceeds`（`posts>=1`） |

四条均 Transient → requeue 轮换新 token（与今日 claim-drift 臂同一语义，relay 日志 `"audit token claim validation failed"` 不变）；①②④ 与 IdP-drift 同族，③ 的小数形态见 FM1b（轮换不恢复）。

### 1.4 夹具随迁（R2 前置，F2）

| 文件 | 改动 |
|---|---|
| `src/stub.rs` `SinkBehavior::default` :91-96 | `token_claims` 增 `"client_id": "aero-im.source"`（== 既有 `"sub"`，机器身份一致） |
| `tests/claim_validation.rs` | 17 处 `json!(` 中**仅 12 处是 token-claims 夹具**（:118/:131/:144/:157/:179/:333/:366/:391/:416/:431/:453/:469）抽 `base_claims()` 基座（iss/aud/scope/sub/client_id）逐用例覆盖差异字段——最小 diff；**其余 5 处不动**（:57 事件 payload、:172/:448 `json!("opaque-bearer-token")` 非 claims（不可 base_claims() 化，会改测试语义）、:351 decode-only roundtrip）；**新增 3 pin**：`token_without_client_id_is_rejected_before_any_post`（delta ①，base 删 client_id → `Err(Transient)` + `posts==0`）、`array_form_scope_token_is_accepted_and_delivery_proceeds`（delta ④，`scope: ["audit:event:write", "metering:read"]` → Ok + `posts>=1`）、`fractional_iat_is_rejected_before_any_post`（delta ③+FM1b，`iat: 1234567890.5` → `Err(Transient)` + `posts==0`） |
| `src/relay.rs` :464-467 测试夹具 | 增 `"client_id"`（同 stub 默认）。**静默面注记**：该用例断言只依赖固定 wrapper `"claim validation failed"` + `posts==0`，即使 gate 因缺 client_id 而非 iss 漂移拒绝也全绿——夹具迁移是 reason-fidelity 卫生，非 outcome-critical；无任何验收命令能捕获漏迁，同提交排序是唯一保护 |
| `tests/state_machine.rs` | 零改动（全走 `SinkBehavior::default()`，断言不变） |
| `oidc/tests.rs` | **F3 修正**：零**构造点**改动属实（本地铸币结构 `ClientAccessTokenClaims`）；但有一处**断言站点**须适配——:351 `assert_eq!(claims.jti, "access-token-1")`（生产类型 `jti: Option<String>`，`Option<String> == &str` 不编译）→ `assert_eq!(claims.jti.as_deref(), Some("access-token-1"))` |

### 1.5 `scripts/truth-check.sh` §3 — AC4 字面量/类型/usage 单源 hard-fail（R3 落地，**本批次已实现并验证**）

**实现位置**：`scripts/truth-check-lib.sh`（sourceable 库，镜像 b5-pin.sh 模式——纯函数+数据，无 exit 无副作用，`set -euo pipefail` 安全）+ `scripts/truth-check.sh` 接线（source + 折入退出码）+ `scripts/test-claim-contract-guard.sh`（20 个自测用例，全部实测通过）。

**(a) 类型单源（全局、无豁免）**：`rg -n -e '^\s*(pub(\([^)]*\))?\s+)?struct\s+(ClientCredentialsClaims|ClientCredentialsTokenConfig|ClientCredentialsGateClaims)\b' crates --glob '*.rs'` 各**恰 1 命中**且位于 `crates/aero-common/src/model/client_credentials.rs`（`ClientCredentialsGateClaims` 是 delta ④ 的 minimal 门结构体，同一单源规则）；否则硬违规。⚠️ **命令勘误**：ripgrep 的 `-E` 是 `--encoding` 不是 extended-regex——旧 spec 的 `rg -nE 'struct ...'` 会报 `unknown encoding` 直接失败；必须用 `rg -n -e`（ERE 本就是 rg 默认）。正则锚定 `^\s*(pub...)?struct\s+Name\b` 使 doc 注释/`use` 提及不误报（false positive 消除）、别名遮蔽不逃逸（false negative 收窄）。

**(b) 字面量单源（全局扫描 + 策展 allowlist）**：
- 扫描 10 个字面量：`"iss" "aud" "scope" "scopes" "sub" "client_id" "iat" "jti"`（8 个大小写敏感）+ `"at+jwt"` / `"application/at+jwt"`（**2 个大小写不敏感**——媒体类型大小写无关 RFC 6838，验证器本身用 `eq_ignore_ascii_case`；带引号匹配使 `"scope"` 永不误中 `"scopes"`、`"at+jwt"` 永不误中 `"application/at+jwt"`）。**不含** `"exp"`/`"nbf"`（F6）。
- **跳过规则**（先于 allowlist 判定）：① 整文件豁免 = leaf 文件本身（`crates/aero-common/src/model/client_credentials.rs`——常量/值 pin 是单源本体）+ `crates/aero-audit-connector/src/stub.rs`（测试替身，夹具 JSON 是数据非逻辑）；② **任意路径分量名为 `tests` 或 `tests.rs`**（覆盖 `crates/*/tests/` 目录、`src/**/tests.rs` 模块、`src/**/tests/*.rs` 子模块——旧规则 `crates/*/tests/` 只匹配顶层目录，漏掉 `oidc/tests.rs`/`id_token_policy.rs`/`sso/tests.rs` 三个 src 内测试模块）；③ 注释行（`^\s*//`，覆盖 `//!`/`///`——jwt.rs:5 doc 自动豁免）。
- **allowlist（file:line:literal 粒度，本批次从实扫导出，逐条真契约理由；过期条目 ⚠️ STALE 警告强制 re-pin，永不静默遮蔽移动的字面量）**：

| 文件:行:字面量 | 真契约理由 |
|---|---|
| `crates/aero-auth/src/oidc.rs:173:"sub"` / `:182:"iat"` | **Debug formatter 字段标签**（`OidcClaims` id_token 结构体 Debug impl，PII 脱敏）——非 claim 访问，不同类型不同契约（旧 spec 漏掉的生产命中，Q4a #1） |
| `crates/aero-auth/src/oidc.rs:365:"iss"/"aud"/"sub"` | **id_token `set_required_spec_claims`**（RFC 7519/OIDC Core id_token 契约，非 cc-token 契约——cc-token 的循环已换 `CLAIM_*` 常量；旧 spec 漏掉，Q4a #2） |
| `crates/aero-audit-connector/src/client.rs:434:"scope"` | **RFC 6749 §4.4 token-request 表单参数**（`request_token` 的 wire 契约，与 claims 校验不同面；旧 spec 漏掉，Q4a #6） |
| `crates/aero-audit-connector/src/relay.rs:464-468`（`"iss"/"aud"/"scope"/"sub"/"client_id"`） | `#[tokio::test]` 夹具 JSON（claim-drift 测试数据；:468 `client_id` 是 F2 夹具随迁新增） |
| `crates/aero-auth/src/extractor.rs:173/:191:"jti"` | `#[cfg(test)]` `Claims` 测试结构体夹具字段值 |
| `crates/aero-server/src/sso.rs:385/:387/:405/:407/:545` | OIDC **authorization-code**（RFC 6749 §4.1）表单参数（RESERVED 列表 + append_pair）+ IdP 响应 `iss` 解析（OIDC Core §3.1.3.7）——不同 flow 不同契约 |
| `crates/aero-server/src/snaplink_commercial/http.rs:234:"scope"` | **RFC 6749 §4.4 token-request 表单参数**（v1 Snaplink 客户端；「冻结面」是策略不是契约理由——**旧理由纠正**，Q4b #4） |
| `crates/aero-server/src/integrations.rs:806/:816/:847:"client_id"` | `#[test]` 夹具 JSON（`CreateInstallationReq` payload） |
| `crates/aero-storage/src/aero_vault_blob_store.rs:60/:62` | **Debug formatter 字段标签**（`ClientCredentials` vault Debug impl）——旧理由「表单参数」一半是错的（**纠正**，Q4b #5）；`:291` 才是 vault OAuth token-request 表单参数 |
| `crates/aero-storage/src/auth_session/admin_revoke.rs:120:"scope"` | **audit-event payload 字段名**（`session.revoked` 审计 JSON schema）——旧理由「vault OAuth 表单参数」**错误**（纠正，Q4b #2） |
| `crates/aero-storage/src/integration.rs:234:"client_id"` | **audit-event payload 字段名**（`integration.installation.created` 审计 JSON）——旧理由「集成注册记录 DB 列」**错误**（纠正，Q4b #3） |
| `crates/aero-storage/src/integration.rs:417/:546` + `integration/support.rs:295` + `integration/machine.rs:546:"client_id"` | `validate_identity_component` 的**错误消息字段名实参**——旧理由「DB 列」**错误**（纠正，Q4b #3） |
| `crates/aero-storage/src/blob.rs:876` / `blob_gc.rs:289:"scope"` | `expect("scope")` **panic 消息字符串**，位于 `#[tokio::test]`——旧理由「vault 表单参数」**错误**（纠正，Q4b #1） |
| `crates/aero-storage/src/live.rs:582:"sub"` | 聊天频道行标签（subscriber 行），`#[tokio::test]` 纯词汇误报 |

- **skip 规则覆盖的测试模块**（不入 allowlist）：`crates/aero-audit-connector/tests/claim_validation.rs`（15 命中）、`crates/aero-auth/src/oidc/tests.rs`（12 命中含 `"AT+JWT"` 大小写变体）、`crates/aero-auth/src/oidc/tests/id_token_policy.rs`（3 命中）、`crates/aero-server/src/sso/tests.rs`（2 命中）。

**(c) usage 侧扫描（新增，封 logic-duplication 洞）**：`rg -n -e '\b(check_issuer|check_audience|check_scope|check_subject|granted_scopes|subject_is_client_id|has_required_scopes|is_valid_identity_component)\b' crates --glob '*.rs'`——调用点仅允许在 **leaf + `crates/aero-common/src/lib.rs`（导出面）+ `crates/aero-auth/src/oidc.rs` + `crates/aero-auth/src/oidc/id_token.rs`（id_token 验证器共享 `is_valid_identity_component`）+ `crates/aero-audit-connector/src/client.rs`**（+ tests 分量/注释跳过）。常量复制（`CLAIM_ISS` 挪进 sso.rs）不产生字面量命中也不产生 struct 定义，但**调用点**必然暴露——A2「duplicate validation logic outside the leaf fails CI」从拼写/类型漂移升级到逻辑入口漂移（security_engineer FM3 最大洞）。

**(d) audit.rs 镜像（A2③）**：`rg -n -F '"admin.content.flag"' crates --glob '*.rs'` 只允许在 `crates/aero-common/src/model/audit.rs`（实扫 2 命中都在 audit.rs——:150 定义 + :269 值 pin；audit.rs doc 注释由声明变事实）。

**(e) 负例自测（`scripts/test-claim-contract-guard.sh`，20 用例全绿）**：
- **孤儿混淆消除**：负例在**真实树的拷贝**上把字面量**追加到既有非豁免文件**（jwt.rs / sso.rs / oidc/tests.rs / claim_validation.rs）——不新建文件，truth-check §1 的孤儿扫描（新文件本身就是违规）不再污染断言；且**断言违规消息模式**（`grep -q '<pattern>'`，非只查退出码），镜像 test-b5-pin-guard.sh 的 `check <name> <pattern> <output>` 惯例。
- **sourceability 决策**：**抽取 sourceable 函数**（`truth-check-lib.sh`），非子进程——truth-check.sh 是顶层可执行文件、末行 `exit`，source 它会在测试 shell 里跑真树再杀 shell（migration_testing_reviewer 指出的不可行路径）；抽取后 truth-check.sh / 自测脚本双消费，与 b5-pin.sh 先例一致。
- 用例清单：① 真实树干净 → 0 违规 + 无 ❌ 行（allowlist 从实扫导出的可复现证明）；② 干净拷贝 → 0 违规 + 无 STALE；③ jwt.rs 追加 `"iss"` → `❌ CLAIM LITERAL: ...jwt.rs:<n>:"iss"`；④ jwt.rs 追加重复 `struct ClientCredentialsClaims` → `❌ CLAIM TYPE: ... has 2 definition`；⑤ sso.rs 追加 `check_issuer(` 调用 → `❌ CLAIM USAGE: ...sso.rs:...check_issuer`；⑥ jwt.rs 追加 `"AT+JWT"` → `❌ CLAIM LITERAL: ..."at+jwt"`（大小写不敏感命中）；⑦ jwt.rs 追加 `"exp"/"nbf"` → **0 违规**（F6 排除 pin）；⑧ oidc/tests.rs + claim_validation.rs 追加字面量 → 0 违规（tests 分量跳过 pin）；⑨ jwt.rs 追加 `"admin.content.flag"` → `❌ AUDIT FLAG`；⑩ sso.rs:385 字面量被移除 → `⚠️ STALE ALLOWLIST`（漂移 → re-pin）；⑪ sso.rs:405 的 `"client_id"` 改成 `"scope"` → 违规 + 旧 pin STALE（**file:line:literal 三要素粒度**）；⑫ 接线副本退出码：干净拷贝 exit 0 / 注入 1 个违规 exit 1（orphan 0 + guard 1）。

**(f) 退出码折入（F5，已接线）**：`scripts/truth-check.sh` source lib 后 `exit $((orphan_violations + CLAIM_GUARD_VIOLATIONS))`（UNWIRED 仍只警告）。**接线时机**：本批次即接线——当前工作树已是 post-implementation（leaf + 消费方 + 夹具随迁全部落地为在途未提交改动），guard 实扫 0 违规，`make check-truth` 保持绿（实测 exit 0）。

**验证证据（本批次实测）**：实扫 122 个字面量命中 → 47 leaf 豁免 + 32 tests 分量跳过（claim_validation 15 / oidc/tests.rs 12 / id_token_policy 3 / sso/tests 2）+ 3 jwt.rs 注释跳过 + 6 stub.rs 豁免 + 34 allowlist → **0 违规**；类型扫描恰 1+1 于 leaf；usage 扫描 0 违规（leaf/oidc.rs/id_token.rs/client.rs/lib.rs 命中全在 allowlist）；flag 镜像 0 违规。`bash scripts/test-claim-contract-guard.sh` = **20/20 绿**。

## 2. Compatibility constraints

| 面 | 约束 | 证明方式 |
|---|---|---|
| aero-auth 公共 API | `aero_auth::ClientCredentialsClaims` / `ClientCredentialsTokenConfig` / `validate_client_credentials_token` 名字与签名**不变**（lib.rs:28-29 → oidc.rs re-export 链）；`integrations.rs` 编译零改动（全仓 `rg validate_client_credentials_token` 仅 oidc.rs / oidc/tests.rs / lib.rs / integrations.rs 4 文件） | `cargo check -p aero-server` + `cargo test -p aero-auth --lib`（85/85 不降） |
| aero-auth 语义 | typ/alg/required_spec_claims/iat/jti/sub==client_id 策略零变化；检查顺序逐字保留；`iat`/`jti` Option 化不削弱严格度——**注意**：jsonwebtoken 的 `required_spec_claims` 只覆盖 exp/sub/iss/aud/nbf（硬编码 match），iat/jti 存在性由 §1.2 future-iat 行的**显式合并检查**重新断言（`is_some_and` 合并缺省+future 双拒） | 85/85 + §1.2 逐符号替换表 |
| connector 配置面 | `RelayConfig` 四个 `expected_*` 字段与 `AERO_AUDIT_*` env 零变化；`expected_sub` 保留（**不**预支并行方向的删除） | config.rs 零 diff |
| connector 行为 | relay 状态机三臂分类零变化；claims 拒绝仍先于任何 POST（:166-176 臂结构不变）；exp/nbf when-present 面保留 | `state_machine.rs` 8 用例逐字绿 + 46→49 pin（3 个新 delta pin） |
| 夹具兼容 | `oidc/tests.rs` 零**构造点**改动 + **1 处断言站点适配**（:351 `jti` 比较，F3 勘误）；connector 夹具随迁是**必需前置**（F2） | 随迁后 24+8 用例断言零改动 |
| 依赖面 | 零新依赖边（两消费方均已依赖 aero-common）；aero-common 不新增 serde/jsonwebtoken 依赖（serde 已在；jsonwebtoken 只在 aero-auth） | Cargo.toml 零 diff |
| 工程规则 | 无 DB 迁移（无 SQL 变更）；`unsafe_code=forbid` / clippy pedantic 无新增警告；`model/mod.rs`+`lib.rs` 显式清单接线（漏接 = 编译错误，良性失败） | `cargo clippy --workspace --all-targets` / `cargo check --workspace` |
| MSRV 1.80 | `is_some_and` / `BTreeSet<&str>` / `serde(default)` 均可用 | rust-toolchain.toml 已钉 1.80 |

## 3. Failure modes

| # | 模式 | 触发 | 行为 | 缓解 |
|---|---|---|---|---|
| FM1 | **交付停滞（fail-closed）** | IdP 铸币缺 `client_id` / `iat` 非整数 → typed gate 拒 → Transient → requeue（backoff 上限 300s，**永不 dead**） | 行在轮换中不投递；与今日 claim-drift 同一臂（relay 日志 `"audit token claim validation failed"`）；属既有语义，非新状态 | B5-4 配给门修 IdP 漂移；监控既有 stale-claimed 行告警；A1 的 `posts==0` pin 证明无 POST 泄漏 |
| FM1b | **交付停滞（自伤新形态，轮换不恢复）** | IdP 铸币 `iat` 带小数（RFC 7519 NumericDate 允许小数秒）→ 门结构体 `Option<u64>` serde 拒 → Transient → requeue | 与 FM1 同臂同分类，**但 token 轮换不恢复**——同一 IdP 每次铸币同形状，行无限轮换直至 IdP 侧修复（stale-claimed 告警可观测） | aero-auth 既有 `iat: u64` 同样拒小数（serde decode），门行为与既有契约一致而非新分歧；IdP 侧修复经 B5-4 配给门（停铸小数 iat）；`fractional_iat_is_rejected_before_any_post` pin。**数组形 scope 不是停滞**（delta ④ 已解析，正例 pin） |
| FM2 | **guard 误报（构建阻断）** | 未来其他契约（如 SCIM/OIDC 新面）新增 `"scope"` 等字面量 | `make check-truth` 非零 | allowlist 是**file:line:literal 三要素粒度 + 逐条真契约理由**（§1.5(b)），merge 时评审；误报代价 = 加一行带理由的 allowlist；**STALE 警告**确保漂移的条目先自曝再遮蔽 |
| FM3 | **guard 漏报** | 字面量落在 allowlist 文件内（如 sso.rs 新增 claim 校验）；或复制校验逻辑但引用常量（零字面量命中） | 前者不被字面量扫描捕获；后者连 struct 定义都不产生 | (a) 类型单源 §1.5(a) 仍全局捕获重定义；(b) allowlist 条目带契约理由、评审可见；(c) 本契约消费面（oidc.rs/client.rs）**不在** allowlist——裸字面量回归必被捕获；(d) **usage 侧扫描 §1.5(c) 新增**：leaf 语义函数调用点限 leaf+两消费方——常量复制的逻辑重复必然暴露调用点（FM3 最大洞已封） |
| FM4 | **aero-auth 回归** | 替换表误序/漏改（如 jti 检查前移） | 行为漂移但测试可能不炸 | 85/85 pin + §1.2 替换表逐项 + 检查顺序逐字保留红线 |
| FM5 | **夹具漏迁** | stub 默认缺 `client_id` 而 typed gate 上线 | 46 用例全转 Transient（success-path/reason-fragment 用例 `valid_token_is_accepted_and_delivery_proceeds`/`exp_nbf_missing_claims_still_pass` 等炸；纯 `is_err` 负例静默降级 reason-fidelity——relay.rs 夹具漏迁完全静默，见 §1.4 注记） | §1.4 是**实施顺序前置**（先夹具后 gate，或同一提交内）；46→49 pin 是最终守卫 |
| FM6 | **guard 退出码回归** | §3 未并入退出码 → 字面量违规只打印不阻断 | 静默漂移 | **已接线**：`exit $((orphan_violations + CLAIM_GUARD_VIOLATIONS))`（F5）+ 自测 ⑫ 断言接线副本退出码（干净 0 / 注入 1 违规 → 1） |
| FM7 | **误伤 audit.rs 镜像** | `"admin.content.flag"` 扫描误伤非 audit.rs 命中 | 构建阻断 | 实测全仓唯一（2 命中均 audit.rs）；如未来迁移使用，经 allowlist 评审（flag 扫描无豁免，是显式决策面） |
| FM8 | **并行方向冲突** | unification 方向先落地（删 connector 验证器） | 本设计 leaf 是其共享底层，不冲突（§6） | 状态 Proposed，未实施；本设计不预支其行为 |

## 4. Migration steps（有序，每步可验证）

1. **基线（本批次现场）**：工作树已含 step 2-4 的全部落地（leaf + `ClientCredentialsGateClaims` + oidc.rs/client.rs 常量替换 + stub/relay/claim_validation 夹具随迁，全部在途未提交）——`cargo test -p aero-auth --lib` / `cargo test -p aero-audit-connector --all-targets` 重跑确认后进入 step 5。
2. **leaf 落地**：✅ 已完成（`crates/aero-common/src/model/client_credentials.rs` 含值 pin + `model/mod.rs`/`lib.rs` 接线；`cargo check -p aero-common` + `cargo test -p aero-common` 验证）。
3. **aero-auth 替换**：✅ 已完成（§1.2 表逐项落地；`cargo test -p aero-auth --lib` 重跑验证）。
4. **connector 替换 + 夹具随迁**：✅ 已完成（§1.3 typed gate + check_* + §1.4 夹具；`cargo test -p aero-audit-connector --all-targets` 重跑验证）。
5. **guard（本批次交付）**：`scripts/truth-check-lib.sh`（§1.5 a-d 四段扫描 + STALE 检测）+ `scripts/truth-check.sh` 接线（source + `exit $((orphan + CLAIM_GUARD_VIOLATIONS))`）+ `scripts/test-claim-contract-guard.sh`（20 用例）→ `bash scripts/truth-check.sh`（**实扫 0 违规、exit 0 已验证**）→ `bash scripts/test-claim-contract-guard.sh`（**20/20 绿已验证**）。
6. **全量门禁**：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（无新增警告）· `scripts/{truth-check,file-size-check,web-check}.sh`。
7. **提交**：显式路径（leaf、oidc.rs、id_token.rs、client.rs、stub.rs、claim_validation.rs、relay.rs、truth-check.sh、truth-check-lib.sh、test-claim-contract-guard.sh、本 design/req docs）；`git status --short` 无本批次残留。

## 5. Testable acceptance mapping（req §6 三条 acceptance 原样保留，逐条可执行）

| # | Acceptance（req 原文） | 可执行断言 / 命令 |
|---|---|---|
| A1 | "Both validators consume the shared leaf types: … a required-scope or audience semantic change in one is a compile error in the other, not a silent drift." | ① `rg -n -e '^\s*(pub(\([^)]*\))?\s+)?struct\s+(ClientCredentialsClaims|ClientCredentialsTokenConfig|ClientCredentialsGateClaims)\b' crates --glob '*.rs'` 各恰 1 命中且位于 leaf（§1.5(a) 类型扫描已内置）；`rg -n 'pub use aero_common::model::client_credentials' crates/aero-auth/src/oidc.rs` ≥1；`rg -n 'use aero_common::model::client_credentials' crates/aero-audit-connector/src/client.rs` ≥1。② **负例 drill**：临时改 leaf（如 `required_scopes: BTreeSet<String>`）→ `cargo check --workspace --all-targets`（两侧 + integrations.rs:220 + oidc/tests.rs:170 全部编译失败；单侧 `cargo check -p aero-auth` 不碰 `#[cfg(test)]` 且 `iter()`/`contains(&str)` 在 BTreeSet 下仍编译——**必须全 workspace 全 targets**）→ 还原。③ `cargo test -p aero-auth --lib` = 85/85；`cargo test -p aero-audit-connector --all-targets` = **49 passed/3 ignored**（含 3 个新 pin）。 |
| A2 | "truth-check-style hard-fail (mirroring AC4 for admin.content.flag): duplicate iss/aud/scope validation logic outside the leaf fails CI." | ① `bash scripts/truth-check.sh`：**干净树零退出（实扫 0 违规，exit 0 已验证）**；负例由 `bash scripts/test-claim-contract-guard.sh` 即种即测（**20/20 绿已验证**）——在真实树拷贝的既有非豁免文件追加字面量/重复 struct/越界 usage 调用/大小写变体 → 断言违规消息模式 + 非零退出（无孤儿混淆）。② 接线：`make check-truth` 门禁已含 guard（truth-check.sh source lib + 退出码折入，F5 已落地）；truth-check 不在 CI jobs（现场核实），"fails CI" 按仓库既有门禁语义执行。③ audit.rs 镜像：guard §1.5(d) 扫描 `"admin.content.flag"` 只允许在 `crates/aero-common/src/model/audit.rs`（实扫 2 命中均 audit.rs——:150 定义 + :269 值 pin），audit.rs doc 注释由声明变事实。④ **usage 侧扫描（新增）**：`check_issuer|granted_scopes|subject_is_client_id` 等 8 个 leaf 语义函数调用点仅限 leaf+两消费方——常量复制型逻辑重复（零字面量命中）也被捕获。 |
| A3 | "No behavior change to relay state machine: state_machine.rs … still passes with the shared types; fail-closed claim rejection still precedes any delivery POST." | ① `cargo test -p aero-audit-connector --all-targets` = **49 passed / 3 ignored 不降**（lib 14 / cv 27 / sm 8——cv 24+3 新 delta pin）；`state_machine.rs` lease/backoff/`permanent_error_dead_after_exactly_two_attempts`/403-dead 用例逐字绿。② 全部 `assert_eq!(posts, 0)` 保持绿（wrong_issuer/missing_audience/missing_audit_scope/wrong_subject/opaque/expired/future_nbf/non_numeric + `token_without_client_id_is_rejected_before_any_post` + `fractional_iat_is_rejected_before_any_post`）；`array_form_scope_token_is_accepted_and_delivery_proceeds` 反向 pin（posts>=1，delta ④ 非回归）。③ 拒绝分类不变：新 pin 断言 `Err(DeliveryError::Transient(_))`（requeue 非 dead）；`deliver` :166-176 臂结构不变。④ `cargo clippy --workspace --all-targets` 无新增警告；`cargo check --workspace` 干净。 |

## 6. Out of scope / parallel direction（兼容但独立，不预支）

`docs/design/2026-08-08-aero-auth-b5-2-relay-claims-unification.design.md`（Proposed）是**另一种架构**：删除 connector 手写验证器、claims 面整体委托 `aero_auth::validate_client_credentials_token`、JWKS-off 面关闭、`expected_sub` 删除、exp/nbf/iat/jti/typ 必填化。本设计与其兼容：leaf 是两者共同的底层契约——本设计落地后，unification 实施时其 claims 面替换自动获得共享类型基础（`ClientCredentialsTokenConfig` 已在 leaf）。本设计明确不预支其任何行为变更：`expected_sub` 保留、JWKS-off 跳过臂保留、exp/nbf when-present 保留、connector 验证器保留。

**本设计不含**：DB 迁移（零 SQL）、配置/env 变更（`AERO_AUDIT_*` 零变化）、web 变更、CI 接线策略变更（guard 走既有 `make check-truth` 门禁）。

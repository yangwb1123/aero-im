# Requirements Spec — relay connector claim/scope 契约统一到 aero-auth `validate_client_credentials_token`（B5-2：删手写 claims 平面 + `audit:event:write` 单字面量源）

- **Module (analysis root)**: `crates/aero-auth` — 验证器 seam 所在；**零 aero-auth 生产代码改动**（本 direction 的 aero-auth 基线 = 当前工作树，含前一批次未提交的 additive 改动，见 §2 勘误 ⑦）。落地动作在消费侧 `crates/aero-audit-connector`（untracked）+ 单字面量 pin 在 `crates/aero-common` + snaplink 字面量替换在 `crates/aero-server`
- **Direction**: "Unify the relay connector's claim/scope validation onto aero-auth's canonical `validate_client_credentials_token` (B5-2 claim contract, `audit:event:write`)"（value 9 / risk_reduction 8 / effort 5 / confidence 9）
- **Source analysis**: `docs/auto/analyses/crates-aero-auth-0b9b4b9f.json`（direction #0）
- **Prior art**: `docs/requirements/2026-08-08-aero-auth-b5-2-machine-token-verification-seam.req.md`（昨日版同方向）、`docs/requirements/2026-08-08-aero-audit-connector-b5-2-claim-contract-exp-nbf-jwks.req.md` + `docs/design/2026-08-08-aero-audit-connector-b5-2-claim-contract-exp-nbf-jwks.design.md`（前一批次：签名面 + exp/nbf + T-11 dead 已落地，claims 面保留手写——本 spec 是其后继）
- **Status**: Requirements（全部引用经源码 grep + 实跑测试复核）
- **Verification date**: 2026-08-08。行号是复核时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点（AGENTS.md §0）

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `oidc.rs:364-478` — `ClientCredentialsTokenConfig` / `ClientCredentialsClaims::granted_scopes` / `validate_client_credentials_token` / `KeyProvider`+`JwksKeyProvider` | ✅ 精确命中 `crates/aero-auth/src/oidc.rs`：`KeyProvider` trait :243（含 fallible 面 `decoding_key_fallible` :257，D10 判别器）；`StaticKeyProvider` :266；`ClientCredentialsTokenConfig {issuer, audience, required_scopes}` :378-383；`ClientCredentialsClaims` :386-399；`granted_scopes` :401-410（`scopes` 数组 ∪ 空格分隔 `scope` 的 BTreeSet 归一化）；`validate_client_credentials_token` :420-487：at+jwt typ 白名单 :424-433 → alg 白名单 RS256/EdDSA :434-441 → `KeyProvider::decoding_key(kid)` :442-446 → `required_spec_claims` 含 `iss,aud,exp,nbf,iat,jti,sub,client_id` :453-456 → `sub == client_id` + `valid_identity_component` :461-469 → future-iat 拒绝 :471-475 → jti 非空/≤1024/无控制字符 :477-481 → `required_scopes ⊆ granted_scopes` :483-486；`JwksKeyProvider` :543（TTL 缓存 + D10 未知-kid 单槽绕过 :571-613）；`LEEWAY_SECS = 60` :307 |
| E2 | `oidc/tests.rs:392`（缺 required scope）、`:332/:350`（scopes-array vs 空格 scope）、`:378`（sub≠client_id）、`:406`（iss/aud）、`:496/:515`（alg allowlist） | ✅ 符号全部命中 `crates/aero-auth/src/oidc/tests.rs`（行号 +6~+8 漂移）：`accepts_client_credentials_at_jwt_with_scopes_array` :338、`accepts_client_credentials_with_space_delimited_scope` :356、`rejects_client_credentials_token_when_sub_differs_from_client_id` :384、`rejects_client_credentials_token_without_required_scope` :398、`rejects_client_credentials_token_with_wrong_issuer_or_audience` :412、`rejects_expired_client_credentials_token_and_token_without_nbf` :435、`rejects_missing_or_invalid_client_credentials_iat_and_jti` :458、`rejects_algorithm_outside_explicit_allowlist` :502、`rejects_algorithm_and_key_family_mismatch` :521、`rejects_bad_signature_from_different_key` :642。夹具：`good_client_access_claims` :214-228（sub==client_id=="erp-production"、scopes 数组、exp/nbf/iat/jti 齐全）、`sign_client_access_token` :236-241（显式 typ 参数）。**实跑确认**：`cargo test -p aero-auth --lib` = **85 passed / 0 failed**（基线绿，含全部上述用例） |
| E3 | `client.rs:33-34`（`SCOPE_AUDIT`）、`:199-260`（`validate_token_claims` 无验签）、`:128-138`（POST 前 fail-closed） | ✅ 符号命中（行号漂移）：`SCOPE_AUDIT = "audit:event:write"` 实为 :43；`validate_token_claims` :238 / `validate_token_claims_at` :252-336（base64url 解 payload、iss/aud/scope/sub + exp/nbf validated-when-present、leeway 60s）；POST 前 claims 拒绝臂实为 :166-176（`invalidate_token` + `DeliveryError::Transient` + "no delivery attempted"）；`decode_jwt_claims` :570-611（注释已无 "[PROPOSED]"——该注释被前一批次删除，见 §2 ①）；`ClaimRejection` :79-82 |
| E4 | `config.rs:87-90`（`AERO_AUDIT_EXPECTED_SCOPE` 默认） | ✅ 符号命中（行号漂移）：`expected_scope` 字段 :35；`from_env` 默认 `"audit:event:write"` 实为 :119-120；`AERO_AUDIT_JWKS_URL` 解析 :92-113（AM-3 "URL wins"，`…_URI` 同设 bail）；`JWKS_ENV_KEYS` env 清单 :302-317（含 `AERO_AUDIT_EXPECTED_SUB`，随 R1 同步删） |
| E5 | `integrations.rs:51,223,231-246`（既有 consumer 模式） | ✅ 精确命中 `crates/aero-server/src/integrations.rs`：`REQUIRED_PUBLISH_SCOPE = "aero.notify.publish"` :51；`ClientCredentialsTokenConfig { issuer, audience, required_scopes: vec![REQUIRED_PUBLISH_SCOPE] }` :223；`MachinePrincipal` :231；`authenticate_machine` :236-247（:240 `validate_client_credentials_token(token, &config.token, provider)`）+ `INTEGRATION_JWKS` OnceLock :63。**唯一生产消费者注记成立**：`rg "validate_client_credentials_token" crates/` 仅 lib.rs / oidc.rs / oidc/tests.rs / integrations.rs |
| E6 | `aero-audit-t11-drill.rs:121`（pin `expected_scope "audit:event:write"`） | ✅ 精确命中 :121；同 pattern 另两 drill：relay-drill :91、priority-drill :146。drill 均为 `RelayConfig` 直构 + `AuditClient::new`（t11 :154、relay :126、priority :202），`jwks_uri: None` |
| E7 | `aero-common/src/model/audit.rs:150`（`MODERATION_OUTBOUND_ACTION` 单字面量 pin 模式） | ✅ 精确命中 :150（"Action-token vocabulary" 区），值 pin 测试 :269。`aero-common::model::audit` 经 `model/mod.rs:6/:20` `pub mod audit; pub use audit::*;` 公开 |

**补充核对（direction 证据未列、但设计必须处理的现场事实）**：
- 签名面已存在：`verify_token_signature` :346-402（JWKS-on：alg==RS256、`decoding_key_fallible` 判别 mechanism-Err→Transient / unknown-kid→Permanent、Validation 显式清空时间/aud 检查——F1 钉）；签名拒绝 → `Permanent(SignatureRejected)`，dead 面 `signature_rejected_dead_after_exactly_two_attempts`（state_machine.rs:470，foreign key → unknown-kid → attempt-1 requeue → attempt-2 dead，posts==0）**已落地且绿**。
- connector **已依赖 aero-auth**：`crates/aero-audit-connector/Cargo.toml` `aero-auth.workspace = true`，`client.rs` 已 `use aero_auth::{JwksKeyProvider, KeyProvider}`——direction 所述"依赖方向缺失"已不成立（§2 ③）。
- 生产 boot 链：`aero-server/src/bin/main.rs:251-259` `RelayConfig::from_env()` → `AuditClient::new(relay_cfg)` → `AuditRelay::new`。
- stub 已有真实签名能力：`trusted_key()`（stub.rs:31-45，确定性种子，kid `test-audit-1`）、`make_rs256_jwt` :349-367（**未设 typ——jsonwebtoken `Header::default()` 的 typ="JWT" 会被验证器 typ gate 拒**）、`/jwks` 端点 :269 + `jwks_keys` 可换、`TamperMode::{CorruptSignature, MutateClaims}` + `corrupt_signature_segment` :369。

## 2. 勘误（2026-08-08 现场 vs direction 引用）

| # | 引用声称 | 实际（已复核） | 影响 |
|---|---|---|---|
| ① | "no signature verification — 模块注释标记 'JWKS verification is [PROPOSED]'" | **注释已不存在**；签名面（`verify_token_signature` :346，JWKS-on 强制、D10 限速、Permanent/Transient 分类）由前一批次落地 | direction 问题陈述的该半句过期；**剩余洞 = JWKS-off 面（`jwks_uri: None` 时 `Ok(())` 跳过验签）+ claims 平面手写**——本 spec 据此收敛 |
| ② | "dependency direction aero-auth → aero-audit-connector is absent" | connector 已依赖 aero-auth（Cargo.toml + `use`）——正确方向的依赖已存在 | 验证器**无需迁移**到 aero-common；aero-auth 保持只读消费 |
| ③ | `SCOPE_AUDIT` client.rs:34 | **:43** | 行号漂移，符号稳定 |
| ④ | `validate_token_claims` client.rs:199-260、POST 前拒绝 :128-138 | `validate_token_claims` :238；claims 拒绝臂 :166-176；`decode_jwt_claims` :570 | 漂移 ~40 行；删除面以符号为准 |
| ⑤ | `config.rs:90` 默认字面量 | **:119-120** | 漂移；单字面量源改造点 |
| ⑥ | "`claim_validation.rs` 37-test surface (scope-array acceptance at :149)" | :149 位于 `missing_audit_scope_is_rejected_before_any_post`（:143）函数体内，**非 scope-array 用例**；scope-array/空格 scope 双形状 acceptance 目前在 connector 测试面不存在（只在 oidc/tests.rs:338/:356）；"37" = `scripts/b5-pin.sh` 的 **37/37 契约槽 pin**（15 executed + 22 [PROPOSED]），非 claim_validation.rs 用例数 | 双形状用例须**新增**（G2）；37/37 guard 保持绿 |
| ⑦ | （前一批次 spec）"aero-auth 零改动" | 当前工作树 `git status`：`M crates/aero-auth/{Cargo.toml, src/oidc.rs, src/oidc/tests.rs}`——前一批次 AM-1 的 additive `decoding_key_fallible` + D10 throttle 未提交 | 本 direction 的 aero-auth 基线 = **当前工作树**；门禁 = 本 direction 不得再增 aero-auth diff（§6 G6） |

**核心结论不变且更强**：claims 契约的两套实现并存仍然属实——connector `validate_token_claims_at`（iss/aud/scope/sub + exp/nbf，**无 typ/iat/jti/sub==client_id**）vs aero-auth `validate_client_credentials_token`（8 项全契约）。且 JWKS-off 面（`jwks_uri: None`，config 默认即此）下伪造/错发 token 仅凭"形状正确的 claims"即可注入 audit sink——这正是 direction 预警的洞，AC3 的无条件化要求关闭它（R2）。

## 3. Verified current state

```
a) aero-auth 已测验证器（SEAM，零改动消费）            crates/aero-auth/src/oidc.rs
   validate_client_credentials_token :420 —— at+jwt typ gate → alg 白名单(RS256/EdDSA)
     → KeyProvider(kid) → iss/aud/exp/nbf/iat/jti/sub/client_id 全 required → sub==client_id
     → future-iat 拒 → jti 约束 → required_scopes ⊆ granted_scopes（数组 ∪ 空格分隔）
   LEEWAY_SECS=60；JwksKeyProvider TTL 缓存 + D10 未知-kid 绕过；re-export lib.rs:27-31
   实跑：cargo test -p aero-auth --lib = 85 passed / 0 failed

b) connector 现状（claims 平面仍手写，双实现并存）     crates/aero-audit-connector/
   deliver :155-235：validate_token_shape(:553, TokenResponse 形状) → access_token
     → verify_token_signature :346（签名面，JWKS-on；None 键源 = Ok(()) 跳过）
     → validate_token_claims_at :252（claims 面：iss/aud/scope/sub + exp/nbf validated-when-present）
     → POST；claims 失败 → invalidate_token + Transient（无 POST）；签名失败 → Permanent(SignatureRejected)
   config.rs：expected_iss/aud/scope/sub :31-37；jwks_uri: Option<Url> :51（AERO_AUDIT_JWKS_URL，缺失=None）
   relay.rs deliver_claim :182-260：Forbidden→dead / Permanent→requeue×1→dead / Transient→requeue
   实跑：cargo test -p aero-audit-connector --all-targets = 46 passed / 3 ignored（lib 14 / claim_validation 24 / state_machine 8）

c) scope 字面量多源（AC5 目标清单，现场 5 处）
   crates/aero-audit-connector/src/client.rs:43  SCOPE_AUDIT
   crates/aero-audit-connector/src/config.rs:119  AERO_AUDIT_EXPECTED_SCOPE 默认字面量
   crates/aero-server/src/snaplink_commercial/http.rs:23  const SCOPE_AUDIT（:146 用于 v1 token 请求 scope 参数）
   三 drill：t11 :121 / relay :91 / priority :146
   测试夹具字面量（claim_validation.rs config :28、state_machine.rs :46、stub 默认 token_claims）——契约被测值，豁免

d) stub（stub.rs）：trusted_key() :31（确定性）；make_rs256_jwt :349（无 typ）；make_jwt :341（alg:none 遗留夹具）；
   /jwks :269；SinkBehavior{token_claims, signing_key, jwks_keys, tamper, …}；默认 signing_key=None → alg:none
   token 默认 sub="aero-im.source" ≠ client_id="drill-client"（R2 强制 sub==client_id 后必挂——须随迁）

e) 生产 boot：aero-server/src/bin/main.rs:251-259 from_env → AuditClient::new → AuditRelay（唯一入口）
   drill 门禁：scripts/test-integration.sh（A3 :335-341、T-11 :361-438、priority :456-484；0239 已落库门已开）
   b5-pin.sh：37/37 契约槽 guard（15 executed + 22 [PROPOSED]）
```

## 4. Scope

**In scope（本 direction）**：
- `crates/aero-audit-connector/src/client.rs`：删 `validate_token_claims` / `validate_token_claims_at` / `ClaimRejection` / `decode_jwt_claims` / `TIME_CLAIM_LEEWAY_SECS`；`deliver` claims 面替换为 `aero_auth::validate_client_credentials_token`（每 POST 前 + 401-刷新重试 POST 前）；`keys` 由 `Option<Arc<dyn KeyProvider>>` 改非 Option；`AuditClient::new` 在 `jwks_uri: None` 时 `bail!`；`verify_token_signature` 的 JWKS-off 跳过臂删除（签名面其余不动）。
- `crates/aero-audit-connector/src/config.rs`：删 `expected_sub` 字段 + `AERO_AUDIT_EXPECTED_SUB` env（验证器 sub==client_id 取代）；`from_env` 在 connector 启用而 `AERO_AUDIT_JWKS_URL` 缺失时 `bail!`（fail-loud，AC3 无条件化）；`expected_scope` 默认值改引单字面量常量（R4）。
- `crates/aero-common/src/model/audit.rs`：新增 `SCOPE_AUDIT_EVENT_WRITE` 常量（`audit:event:write` 唯一字面量源，镜像 `MODERATION_OUTBOUND_ACTION` :150）。
- `crates/aero-server/src/snaplink_commercial/http.rs:23`：`SCOPE_AUDIT` 字面量改引常量（机械替换，行为零变化——AC5 明示"no second scope-literal site remains"）。
- `crates/aero-audit-connector/src/stub.rs`：默认自签 RS256 at+jwt（typ `at+jwt`、kid `test-audit-1`、sub==client_id=="drill-client"、exp/nbf/iat/jti 齐全）；`/jwks` 默认服务 trusted key；legacy `make_jwt`（alg:none）保留为拒绝夹具。
- 测试随迁：`tests/claim_validation.rs`（24 用例迁移到签名 token + keys 面，**新增** scopes-array / 空格 scope 双 acceptance 用例）、`tests/state_machine.rs`（8 用例保持，含 priority / signature-rejected 面）、`src/relay.rs` 内部单测、drill ×3（t11 占位 https jwks_uri；relay/priority 走 stub `/jwks` 全链路）。
- 门禁：`scripts/b5-pin.sh` 37/37 guard、`scripts/test-integration.sh` drill 段保持绿。

**Out of scope（并行方向/既有面——本 direction 不建、不改）**：
- **aero-auth 生产代码：零新增改动**（基线 = 当前工作树，前一批次 additive 改动为既有状态；验证器只读消费，不迁移、不改签名、不加 typed error）。
- `aero-server/src/integrations.rs` 与 snaplink v1 投递语义（除 :23 字面量替换外不碰；v1 退役属 campaign 决策）。
- outbox / lease / 退避 / 投递 / 回执 / payload guard / relay 状态机分类臂（`Forbidden`/`Permanent`/`Transient` 语义逐字保持）——**relay 零行为变更**。
- 0239/0240/0241 与 `audit_governance.rs`（B5-1 已落库）；priority（B5-3）；配给门 `aero-eng/src/audit_provision.rs`（B5-4）。
- per-client allowlist + scope registry、freshness floor（max token age）——direction acceptance 未含，不做。
- `base64` 依赖移除——legacy `make_jwt`（拒绝夹具）仍用 base64，不在本 direction 删除面。

## 5. Requirements

### R1 — claims 平面委托 aero-auth 验证器，删手写重复实现
`AuditClient` 构造期构建一次 `ClientCredentialsTokenConfig { issuer: config.expected_iss, audience: config.expected_aud, required_scopes: vec![config.expected_scope] }`（`expected_scope` 默认 = R4 单字面量常量）。`deliver`（client.rs:155）循环内：签名面（`verify_token_signature`，不变）→ **claims 面 = `aero_auth::validate_client_credentials_token(token, &claims_cfg, keys.as_ref())`**；任意 `OidcError` → `invalidate_token` + `DeliveryError::Transient`（与今日 ClaimRejection 臂逐字同语义：fail-closed 无 POST、行 requeue、B5-4 修 IdP 漂移）+ warn 日志；401-刷新后重试 POST 前同一 token 重新过验证器（`refreshed_token_must_repass_claim_validation_before_retry_post` 语义保持）。**删除** `validate_token_claims`、`validate_token_claims_at`、`ClaimRejection`、`decode_jwt_claims`、`TIME_CLAIM_LEEWAY_SECS`（leeway 由验证器 `LEEWAY_SECS=60` 继承，数值相同）；`validate_token_shape`（:553，TokenResponse 形状护栏）保留。`expected_sub` 字段 + `AERO_AUDIT_EXPECTED_SUB` env + `JWKS_ENV_KEYS` 清单项删除——**sub==client_id 成为唯一 sub 契约**（验证器强制），connector 不再有第二个 sub 比较点。

### R2 — KeyProvider 强制化（AC3 无条件：验签先于一切投递）
- `AuditClient.keys`：`Option<Arc<dyn KeyProvider>>` → `Arc<dyn KeyProvider>`；`with_key_provider(config, keys: Arc<dyn KeyProvider>)` 为测试/drill 注入 seam。
- `AuditClient::new(config)`：`jwks_uri: None` → `bail!`（**relay 不启动、不 claim、不投递**——生产入口 `aero-server/src/bin/main.rs:257` 直接命中）；`Some` → `JwksKeyProvider::new`。
- `RelayConfig::from_env`（config.rs:60）：connector 启用（`AERO_AUDIT_TOKEN_ENDPOINT` 存在）而 `AERO_AUDIT_JWKS_URL` 缺失 → `bail!`（与 AM-3 同姿态 fail-loud；`…_URL`/`…_URI` 同设 bail 保持）。
- `verify_token_signature`（:346）**保留**：删除 `let Some(keys) = … else { return Ok(()) }` 跳过臂；其余逐字不动——分类语义（mechanism-Err→Transient / unknown-kid、非 RS256、坏签名→`Permanent(SignatureRejected)`）与既有测试面（`signature_rejected_dead_after_exactly_two_attempts` 等）零漂移。验证器内部 decode 再验一次签（同 key source、TTL 缓存共享，无新增出网）——双验签为 defense-in-depth，非重复出网。

### R3 — claim 契约全矩阵（由 E1 验证器唯一保证，connector 侧零 claim 判断代码）
JOSE typ ∈ {`at+jwt`, `application/at+jwt`}；alg ∈ {RS256, EdDSA}（有效 gate 仍是签名面 RS256，验证器 allowlist 为纵深）；`iss`/`aud` 精确匹配；`exp`/`nbf`/`iat`/`jti` **必填**（nbf 由 validated-when-present 变必填——行为变更，见 §8）；future-iat 拒；jti 非空/≤1024/无控制字符；`sub == client_id`（`valid_identity_component`）；`granted_scopes`（scopes 数组 ∪ 空格分隔 scope）⊇ `required_scopes`。opaque/畸形 token → `OidcError::MalformedToken` → 签名面先行命中（`decode_header` 失败 → Permanent）或验证器拒绝 → 同样 fail-closed 无 POST。

### R4 — `audit:event:write` 单字面量源（AC5）
`crates/aero-common/src/model/audit.rs`「Action-token vocabulary」区（`MODERATION_OUTBOUND_ACTION` :150 旁）新增：
```rust
/// The single legal literal site for the audit sink's client-credentials
/// scope (v1 `SCOPE_AUDIT` / connector config default / drill pins all
/// reference this name). A flip is a one-line edit; the value pin test
/// below follows the `MODERATION_OUTBOUND_ACTION` pattern.
pub const SCOPE_AUDIT_EVENT_WRITE: &str = "audit:event:write";
```
消费方替换：connector `client.rs:43`（`SCOPE_AUDIT` 删除，改用常量）、`config.rs:119` 默认值、`snaplink_commercial/http.rs:23`（`SCOPE_AUDIT` 删除，改用常量）、drill ×3 字面量。值 pin 测试镜像 audit.rs:269（`assert_eq!(SCOPE_AUDIT_EVENT_WRITE, "audit:event:write")`）。

### R5 — stub 默认自签 at+jwt（夹具随迁，全部测试/drill 的驱动源）
- `SinkBehavior::default()`：`signing_key: Some(trusted_key())`（kid `test-audit-1`）、`jwks_keys: vec![trusted_key()]`（`/jwks` :269 服务既有）。
- `make_rs256_jwt`（stub.rs:349）header 补 `typ: Some("at+jwt")`——jsonwebtoken `Header::default()` 的 typ="JWT" 会被验证器 typ gate 拒。
- 默认 `token_claims` → 完整 RFC 9068 形状（镜像 oidc/tests.rs `good_client_access_claims` :214-228）：`iss`/`aud`/`scope: "audit:event:write"`/`sub: "drill-client"`/`client_id: "drill-client"`/`exp: now+3600`/`nbf: now-1`/`iat: now`/`jti: 非空`。
- legacy `make_jwt`（alg:none, typ JWT）保留为拒绝夹具（opaque/alg:none 用例）。

### R6 — drills 随迁（test-integration.sh 门禁保持绿）
- t11 drill（closed token endpoint，token 永不可得）：`jwks_uri` → `Some(Url::parse("https://idp.example.test/jwks"))` 占位（JWKS 永不取，语义不变：pending 行 + transport-error 不变量）；`expected_sub` 字段删除。
- relay / priority drill：`jwks_uri: Some(stub.jwks_url())` + R5 默认自签 → 全链路走真实 `JwksKeyProvider`（fetch stub `/jwks`、kid 匹配、验签、claims 契约）——drill 从 JWKS-off 面升级为生产同构面。
- 三 drill 的 `client_id`/stub `sub` 对齐为 `"drill-client"`（R2 强制 sub==client_id）。

## 6. 验收门禁（direction acceptance 原样保留，逐条 testable）

### G1（AC1）— 缺 `audit:event:write` → 无 POST；T-11 drill 绿
- `tests/claim_validation.rs` `missing_audit_scope_is_rejected_before_any_post`（现 :143）迁移到签名 token + keys 面：scope 为 `"billing:entitlement:read"` 的 RS256 at+jwt（其余 claims 全合法）→ `outcome.is_err()` + `assert_eq!(stub.posts(), 0)`——镜像 oidc `rejects_client_credentials_token_without_required_scope` :398 的契约面。
- T-11 drill：`cargo run -p aero-audit-connector --bin aero-audit-t11-drill`（throwaway DB，0239 已落库门已开）退出 0；`scripts/test-integration.sh` 产出 `B5-CHECK t11-fail-closed: PASS`。

### G2（AC2）— 双 scope 形状经 `granted_scopes()` 接受；claim_validation 面 + 37/37 guard 绿
- **新增** `accepts_scopes_array_claim`：`scopes: ["audit:event:write"]`（镜像 oidc :338）→ `outcome.is_ok()` + `posts() >= 1`。
- **新增** `accepts_space_delimited_scope_claim`：`scope: "audit:event:write metering:read"`（镜像 oidc :356）→ 同上。
- `tests/claim_validation.rs` 既有 24 用例全量随迁绿（断言不变或更强）；`tests/state_machine.rs` 8 用例绿。
- `scripts/b5-pin.sh` 37/37 契约槽 guard 绿（15 executed + 22 [PROPOSED] 计数/格式/去重/非空 + verdict 行）。

### G3（AC3）— 验签先于投递：异 key / 篡改 token 拒投
- 既有面保持绿：`tampered_signature_is_rejected_before_any_post`（claim_validation.rs:492，`TamperMode::CorruptSignature`，posts==0）——即"connector tests/claim_validation.rs 的 bad-signature case"；`signature_rejected_dead_after_exactly_two_attempts`（state_machine.rs:470，foreign key → unknown-kid → attempt-1 requeue、attempt-2 `FakeStatus::Dead`、`posts() == 0`、`last_error` 含 `SignatureRejected` 且不含 `transient`）。
- 验证器双验签面：同一篡改 token 在签名面放行场景不存在（签名面先命中）；claims 面（如 token 由 trusted key 签但 claims 违规）→ Transient + posts==0（`refreshed_token_must_repass_claim_validation_before_retry_post` 保持）。
- **无 JWKS-off 跳过臂**：`AuditClient` 构造无 key 源 → 构造期 `Err`（R2），不存在"跳过验签的投递路径"。

### G4（AC4）— moderation priority 不变
- `pg.rs::mixed_priority_claim_orders_moderation_first_then_fifo`（:536，PG-gated `--ignored`，throwaway `DATABASE_URL`）绿：priority DESC 抢占 FIFO 的 claim 集断言不变。
- `tests/state_machine.rs` `priority_first_claim_preempts_fifo_and_limit1_keeps_top_lane`（:360，无 PG）绿。
- priority drill：`cargo run -p aero-audit-connector --bin aero-audit-priority-drill` 退出 0；`B5-CHECK moderation-priority-drill: PASS`。

### G5（AC5）— 无第二个 scope 字面量站点
- `rg -n '"audit:event:write"' crates/*/src` 仅命中 `crates/aero-common/src/model/audit.rs`（测试夹具字面量为契约被测值，豁免；drill/测试经常量或夹具值引用）。
- 值 pin：`assert_eq!(SCOPE_AUDIT_EVENT_WRITE, "audit:event:write")`（audit.rs 测试，镜像 :269 模式）绿。
- `cargo check --workspace` 干净（snaplink http.rs 与 connector 引用常量编译通过）。

### G6 — 回归守卫（基线门禁）
- `cargo test -p aero-auth --lib`：**85/85 零改动绿**（验证器本体与 8 项 client_credentials 契约测试不动）。
- `cargo test -p aero-audit-connector --all-targets`：全量绿（现 46 passed / 3 ignored；迁移后 ≥ 现数，含新增双形状用例与随迁面）。
- `cargo clippy --workspace --all-targets` 无新增警告；`scripts/{truth-check,file-size-check}.sh` 0 违规。
- **aero-auth diff 守卫**：本 direction 交付后 `git diff crates/aero-auth` 相对**当前工作树**无新增改动（前一批次未提交的 additive 改动为基线，不算本 direction）。
- `scripts/test-integration.sh`：A3 / T-11 / moderation-priority drill 段 + b5-pin 37/37 全绿。

## 7. Test placement

| Test | Location | Harness |
|---|---|---|
| G1 缺 scope 无 POST（签名 token + keys 面）；G2 双形状新增用例；G3 bad-signature（篡改） | `crates/aero-audit-connector/tests/claim_validation.rs`（迁移 + 新增） | `cargo test -p aero-audit-connector`，无 DB（stub sink + StaticKeyProvider / stub `/jwks`） |
| G3 异 key → dead-after-2（Permanent 面）；G4 priority 无 PG 面 | `crates/aero-audit-connector/tests/state_machine.rs`（既有 8 用例保持绿） | 同上（fake outbox） |
| G4 priority PG 面 | `crates/aero-audit-connector/src/pg.rs::mixed_priority_claim_orders_moderation_first_then_fifo`（`#[ignore]`） | `cargo test -p aero-audit-connector -- --ignored`，需 `DATABASE_URL`（throwaway 库） |
| G5 单字面量 pin + 值 pin | `crates/aero-common/src/model/audit.rs` 测试（镜像 :269 模式）；`rg` grep 守卫 | `cargo test -p aero-common`；`rg -n '"audit:event:write"' crates/*/src` |
| 验证器本体回归（零改动） | 既有 `crates/aero-auth/src/oidc/tests.rs` 8+ 项 | `cargo test -p aero-auth --lib`（85/85） |
| G1/G4 drill 门禁 | `src/bin/aero-audit-{t11,relay,priority}-drill.rs`（config 随迁） | `scripts/test-integration.sh`（0239 已落库门已开）；37/37 由 `scripts/b5-pin.sh` 收口 |
| 验证器接线（构造期 fail-loud） | `config.rs` env 单测（缺 `AERO_AUDIT_JWKS_URL` → bail；`JWKS_ENV_KEYS` 清单更新） | `cargo test -p aero-audit-connector --lib` |

## 8. Risks / 决策点

- **nbf 必填化是行为变更**：手写平面 exp/nbf 为 validated-when-present（缺失放行，D1 既决）；验证器 `required_spec_claims` 含 nbf → 缺 nbf 的 token 全拒（Transient requeue，不丢不死）。RFC 9068 要求 exp，真实 Snaplink token 应有 nbf——**staging 以真实 token 复核（AGENTS §4.5，写「待联调」不写「完成」）**。
- **`expected_sub` 移除 = 部署 fail-closed 面**：旧 env `AERO_AUDIT_EXPECTED_SUB="aero-im.source"` 的部署（含现 drill/测试 config）必须对齐 `sub == client_id`（"drill-client"），否则全部 fail-closed。`AERO_AUDIT_EXPECTED_SUB` 从 env 清单删除（遗留 env 由 presence-gating 的 stray 检测兜底报错）。
- **JWKS 必填 = 部署行为变更**：`AERO_AUDIT_JWKS_URL` 从 opt-in 变必填（connector 启用时缺省 → boot bail）。这是前一批次 D2 自己注记的"合并终态（fail-loud 必填）"建议的忠实落地，且是 AC3 无条件化的唯一路径；drill 走 stub `/jwks` 全链路验证，staging 须验真实 JWKS 可达。
- **双验签冗余**：签名面（`verify_token_signature`）与验证器 decode 各验一次——同 key source（JwksKeyProvider TTL 缓存），零新增出网；保留签名面是为了**分类语义零漂移**（mechanism-Err→Transient / unknown-kid→Permanent 的 `decoding_key_fallible` 判别是验证器 infallible 面给不出的，前一批次 F2/F7/D10 的产物，relay 零变更）。
- **OidcError → 分类单一映射**：全部 `OidcError` 变体 → `DeliveryError::Transient`（claims 面语义，与今日 ClaimRejection 臂逐字一致）；签名/结构面由签名面先行分类。不按错误文案串匹配（前一批次 §8 已警示脆弱性，且 aero-auth 零改动红线禁止加 typed error）。
- **aero-auth 零改动红线**：本 direction 不迁移验证器、不改签名、不加变体；`git diff crates/aero-auth` 相对当前工作树必须为空（G6）。
- **snaplink 字面量替换的最小面**：AC5 要求无第二字面量站点 → snaplink_commercial/http.rs:23 必须引用常量；这是机械替换（v1 投递语义零变化），不构成 v1 退役。

## 9. Sequencing

1. `aero-common`：新增 `SCOPE_AUDIT_EVENT_WRITE` + 值 pin（G5 先落，后续所有消费点引用）。
2. `config.rs`：删 `expected_sub`；`AERO_AUDIT_JWKS_URL` 必填 bail；`expected_scope` 默认引常量；`JWKS_ENV_KEYS` 更新 + env 单测。
3. `client.rs`：`keys` 非 Option + `AuditClient::new` bail + 删签名面跳过臂；`ClientCredentialsTokenConfig` 接线；删 `validate_token_claims*`/`ClaimRejection`/`decode_jwt_claims`/`TIME_CLAIM_LEEWAY_SECS`。
4. `stub.rs`：默认自签 at+jwt + `jwks_keys` 默认 + `make_rs256_jwt` typ 修正 + 默认 claims 完整化（R5）。
5. 测试迁移：`claim_validation.rs`（24 用例 → 签名面 + 新增双形状/坏签名用例）、`state_machine.rs`（保持）、`relay.rs` 内部单测、`config.rs` 单测。
6. drill 随迁：t11 占位 https jwks_uri；relay/priority 走 stub `/jwks`；`expected_sub` 删除；sub==client_id 对齐。
7. 全量门禁：G1–G6（`cargo check --workspace`、`cargo test -p aero-auth --lib`、`cargo test -p aero-audit-connector --all-targets`、`-- --ignored` PG 面、`cargo clippy --workspace --all-targets`、`scripts/{truth-check,file-size-check}.sh`、`scripts/test-integration.sh` drill + b5-pin 37/37、`rg` 字面量守卫、aero-auth diff 守卫）。
8. **staging（AGENTS §4.5）**：真实 token endpoint + 真实 JWKS + 真实签发 token 的 sub==client_id/nbf 核对——写「待联调」，不写「完成」。

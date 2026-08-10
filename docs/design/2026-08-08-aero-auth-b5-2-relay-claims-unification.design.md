# Design — relay connector claim/scope 契约统一到 aero-auth `validate_client_credentials_token`（B5-2）

- **Requirements**: `docs/requirements/2026-08-08-aero-auth-b5-2-relay-claims-unification.req.md`
- **Module (analysis root)**: `crates/aero-auth`（只读 seam）· `crates/aero-audit-connector`（落地主体）· `crates/aero-common`（单字面量）· `crates/aero-server`（snaplink :23 机械替换）
- **Verification date**: 2026-08-08（全部证据已对 live tree 复核，见 §0；同日 acceptance-oracle 复核修正见 §5 标记——行号漂移与 guard 可执行性以 `docs/auto/runs/…/adversarial_review-9c87f3a7/task-2-acceptance-oracle-verification.md` 为准；**security-review F1 后检查 + migration-reviewer 部署/回滚 runbook 并入本版（§1.3 / §3 F2·F11 / §4 step 9-10），acceptance 行第三次再钉见 §5**）
- **Design status**: Proposed（待 design-gate 评审）

## 0. Evidence verification result（untrusted claims → live-tree 复核）

全部 7 项证据 + spec 勘误 3 条**复核成立**；符号级命中，行号漂移均在 spec 记录的 ±40 内：

| Evidence | Live-tree 复核 |
|---|---|
| E1 oidc.rs 验证器符号 | ✅ 全命中（KeyProvider:243 / decoding_key_fallible:252 / ClientCredentialsTokenConfig:378 / granted_scopes:401 / validate_client_credentials_token:420 / JwksKeyProvider:543 / LEEWAY_SECS:307）；`cargo test -p aero-auth --lib` 实跑 **85/85** |
| E2 oidc/tests.rs 契约用例 | ✅ 符号命中（漂移 +6~+8）；85/85 含全部 |
| E3 client.rs 手写 claims 平面 | ✅ `SCOPE_AUDIT`:43、`validate_token_claims`:238、`validate_token_claims_at`:248、`ClaimRejection`:81、`decode_jwt_claims`:570；**JWKS-off 跳过臂 live 确认**（verify_token_signature:347-349 `let Some(keys) = … else { return Ok(()) }`）；POST 前拒绝臂 :166-176 |
| E4 config.rs | ✅ expected_scope:35 / expected_sub:36 / jwks_uri:50 / JWKS_ENV_KEYS:302（含 EXPECTED_SUB:313，design 记 :312 漂移 1）/ 默认字面量:119-120 / stray-env bail:59-64 / env 单测 `jwks_url_absent_parses_none`:342 |
| E5 integrations.rs 既有 consumer 模式 | ✅ REQUIRED_PUBLISH_SCOPE:51 / INTEGRATION_JWKS:63 / token config:220-223 / authenticate_machine:236；`rg validate_client_credentials_token` 仅 4 文件（oidc.rs/tests.rs/lib.rs/integrations.rs） |
| E6 drills 字面量 | ✅ t11:121 / relay:91 / priority:146 精确命中；三 drill 均 `jwks_uri: None` + `expected_sub: "aero-im.source"` + `client_id: "drill-client"` |
| E7 audit.rs 单字面量模式 | ✅ MODERATION_OUTBOUND_ACTION:150 + 值 pin:269；`SCOPE_AUDIT_EVENT_WRITE` 尚不存在（R4 为真新增） |

spec §2 三条勘误的现场确认：
1. ✅ "[PROPOSED] JWKS verification" 注释不存在；签名面（verify_token_signature:346 + D10 throttle + Permanent(SignatureRejected)）已落地；**剩余洞 = JWKS-off 跳过臂 + 手写 claims 平面**（缺 typ/iat/jti/sub==client_id）。
2. ✅ connector 已 `aero-auth.workspace = true` + `use aero_auth::{JwksKeyProvider, KeyProvider}`——依赖方向正确，验证器无需迁移。
3. ✅ `missing_audit_scope_is_rejected_before_any_post`（claim_validation.rs:143）的 `assert_eq!(posts, 0)` 断言行在 **:152**（spec 记 :149，实为 json! 收尾 `}))`——行号漂移 3 行，符号级结论不变）：**非** scope-array 用例；connector 层 `rg '"scopes"'` 全 0 命中，正向用例全为标量 `scope`（:121/:134/:160/:275/:369/:394/:419/:434/:472/:671）——双形状 acceptance 确需新增（G2）；b5-pin.sh 37/37（15 executed + 22 [PROPOSED]）确认。

漂移记录：stub `trusted_key`:30（spec :31）、`make_rs256_jwt`:352（spec :349）——符号稳定，行号漂移不构成证据问题。

**基线**：`cargo test -p aero-auth --lib` 85/85 绿；`cargo test -p aero-audit-connector --all-targets` 46 passed / 3 ignored（lib 14 / claim_validation 24 / state_machine 8）。工作树 `git status` 含前一批次未提交的 aero-auth additive 改动（`M crates/aero-auth/{Cargo.toml,src/oidc.rs,src/oidc/tests.rs}`）——本 design 的 aero-auth 基线 = 当前工作树，**本 direction 不得再增 aero-auth diff**（G6）。

## 1. API changes（before → after，逐符号）

### 1.1 `crates/aero-common`（additive，仅 1 公共常量）

`src/model/audit.rs`「Action-token vocabulary」区（MODERATION_OUTBOUND_ACTION:150 旁）：

```rust
/// The single legal literal site for the audit sink's client-credentials
/// scope (v1 `SCOPE_AUDIT` / connector config default / drill pins all
/// reference this name). A flip is a one-line edit; the value pin test
/// below follows the `MODERATION_OUTBOUND_ACTION` pattern.
pub const SCOPE_AUDIT_EVENT_WRITE: &str = "audit:event:write";
```

导出：经既有 `model/mod.rs` `pub use audit::*`（已公开，零改动）。消费方路径 `aero_common::model::audit::SCOPE_AUDIT_EVENT_WRITE`（crate 依赖已存在：connector 依赖 common；server 依赖 common）。

### 1.2 `crates/aero-audit-connector/src/config.rs`

| Symbol | Before | After |
|---|---|---|
| `RelayConfig.expected_sub: String` (:36) | 存在 | **删除**（sub==client_id 成为唯一 sub 契约） |
| `RelayConfig.expected_scope: String` (:35) | 默认字面量 `"audit:event:write"` (:119-120) | 默认 `aero_common::model::audit::SCOPE_AUDIT_EVENT_WRITE.to_owned()`（可被 `AERO_AUDIT_EXPECTED_SCOPE` 覆盖的语义不变） |
| `RelayConfig.jwks_uri: Option<Url>` (:50) | Option（语义：None ⇒ 验签关） | **字段类型不变**（disabled 态可表示）；语义改为：`from_env` 在 connector 启用时保证 `Some` |
| `RelayConfig::from_env` (:60) | token_endpoint 存在而 JWKS 缺失 → `jwks_uri: None`（验签关） | token_endpoint 存在而 `AERO_AUDIT_JWKS_URL`/`_URI` 均缺失 → **`bail!`**（fail-loud）；URL/URI 同设 bail 保持；disabled 态（无 token endpoint + 无 stray）→ `Ok(None)` 不变 |
| env 解析 `AERO_AUDIT_EXPECTED_SUB` (:121) | 存在 | **删除** |
| `JWKS_ENV_KEYS` (:302-314) | 13 项含 `AERO_AUDIT_EXPECTED_SUB` (:313) | **删除该项**（12 项） |
| env 单测（:342-351） | `jwks_url_absent_parses_none`——`unset AERO_AUDIT_JWKS_URL must leave signature verification off`（断言 `jwks_uri.is_none()` :348） | **翻转**：unset + 启用 → bail 断言；新增「URL 缺失 bail / URL 存在 Some」用例 |

### 1.3 `crates/aero-audit-connector/src/client.rs`

| Symbol | Before | After |
|---|---|---|
| `AuditClient.keys: Option<Arc<dyn KeyProvider>>` (:108) | Option | **`Arc<dyn KeyProvider>`**（非 Option） |
| 新增字段 `claims_config: ClientCredentialsTokenConfig` | — | 构造期 `ClientCredentialsTokenConfig { issuer: config.expected_iss.clone(), audience: config.expected_aud.clone(), required_scopes: vec![config.expected_scope.clone()] }`（一次构建，复用；`ClientCredentialsTokenConfig` 来自 `aero_auth::oidc`，经 `aero_auth` re-export） |
| `AuditClient::new(config)` (:126) | `jwks_uri: None` → keys=None（JWKS-off） | `jwks_uri: None` → **`bail!("audit connector requires AERO_AUDIT_JWKS_URL …")`**；`Some(uri)` → `Arc::new(JwksKeyProvider::new(uri.as_str()))`；随后构建 claims_config |
| `AuditClient::with_key_provider(config, keys: Option<…>)` (:135-137) | Option 参数 | **`keys: Arc<dyn KeyProvider>`**（测试/drill 注入 seam，签名随 keys 非 Option 化） |
| `verify_token_signature` (:346) | 首行跳过臂 `let Some(keys) = … else { return Ok(()) }` (:347-349) | **删除跳过臂**；其余逐字不动（alg==RS256 gate → `decoding_key_fallible` 判别 mechanism-Err→Transient / unknown-kid→Permanent(SignatureRejected)） |
| `deliver` (:155-235) claims 面 | `self.validate_token_claims(&token)`（:166） | `aero_auth::validate_client_credentials_token(&token, &self.claims_config, self.keys.as_ref()).await`；**任意 `Err(OidcError)` → `invalidate_token` + `DeliveryError::Transient` + warn**（与今日 ClaimRejection 臂逐字同语义：fail-closed 无 POST、行 requeue）。**新增 F11 后检查（security-review F1，零 aero-auth diff）**：验证器返回 `ClientCredentialsClaims`（aero-auth `lib.rs:29` 已导出，`oidc.rs:388` `pub client_id`）后，`claims.client_id != config.client_id` → 同一 `invalidate_token` + Transient + warn（同臂逐字复用）。`RelayConfig.client_id` **已必填**（config.rs:31 `String` 非 Option + :115 `identity_env` 必填）——零 config 面变化，**无「client_id 变必填」的 config-bail 面** |
| `TIME_CLAIM_LEEWAY_SECS` (:40) | 存在 | **删除**（leeway 由验证器 `LEEWAY_SECS=60` 继承，数值相同） |
| `SCOPE_AUDIT` (:43) | 存在 | **删除**（改用 common 常量） |
| `ClaimRejection` (:81) | 存在 | **删除** |
| `validate_token_claims` (:238) / `validate_token_claims_at` (:248-336) | 存在 | **删除** |
| `decode_jwt_claims` (:570) | 存在 | **删除**（验证器内部 decode 取代） |
| `validate_token_shape` (:553) | 保留 | **保留**（TokenResponse 形状护栏，非 claims 判断） |

401-刷新重试语义（:198-205）**结构性保持**：`deliver` 的 `loop` 每次迭代先过签名面再过 claims 面——刷新后的新 token 在重试 POST 前重新过验证器，`refreshed_token_must_repass_claim_validation_before_retry_post`（claim_validation.rs:268）不变量零漂移。

### 1.4 `crates/aero-audit-connector/src/stub.rs`（夹具随迁）

| Symbol | Before | After |
|---|---|---|
| `SinkBehavior::default().signing_key` (:103) | `None`（→ alg:none） | **`Some(trusted_key())`**（kid `test-audit-1`） |
| `SinkBehavior::default().jwks_keys` (:105) | `Vec::new()`（→ /jwks 404） | **`vec![trusted_key()]`**（/jwks:269 服务） |
| `SinkBehavior::default().token_claims` (:91) | 4 claim（iss/aud/scope/sub，sub=`aero-im.source`） | **完整 RFC 9068 形状**：iss/aud/`scope: "audit:event:write"`/`sub: "drill-client"`/`client_id: "drill-client"`/`exp: now+3600`/`nbf: now-1`/`iat: now`/`jti: 非空`（镜像 oidc/tests.rs `good_client_access_claims`:214-228） |
| `make_rs256_jwt` (:352) | `Header { alg, kid, ..Header::default() }`（typ="JWT" → 被验证器 typ gate 拒） | header 补 **`typ: Some("at+jwt")`** |
| `make_jwt` (:341, alg:none) | 存在 | **保留**为拒绝夹具（opaque/alg:none 用例） |

### 1.5 `crates/aero-server`

- `src/snaplink_commercial/http.rs:23`：`const SCOPE_AUDIT: &str = "audit:event:write";` → **删除本地常量**，:146 使用点改引 `aero_common::model::audit::SCOPE_AUDIT_EVENT_WRITE`（机械替换，v1 投递语义零变化）。
- `src/bin/main.rs:251-259`：**零改动**——`RelayConfig::from_env()` 的 `bail!` 与 `AuditClient::new` 的 `bail!` 自动生效（connector 启用而 JWKS 缺失 → boot 失败）。
- `src/integrations.rs`：**零改动**（`aero.notify.publish` 是另一 scope 域，不在本 direction 字面量清单）。

### 1.6 drills（`crates/aero-audit-connector/src/bin/*.rs`）

| Drill | Before | After |
|---|---|---|
| t11（:113-130, :154） | `jwks_uri: None`、`expected_sub: "aero-im.source"` | `jwks_uri: Some(Url::parse("https://idp.example.test/jwks")?)` 占位（token endpoint 关闭 → JWKS 永不取，pending 行 + transport-error 不变量不变）；删 `expected_sub` |
| relay（:83-100, :126） | `jwks_uri: None` | `jwks_uri: Some(stub.jwks_url())`（:167）→ 全链路真实 `JwksKeyProvider`（fetch stub /jwks、kid 匹配、验签、claims 契约）；删 `expected_sub` |
| priority（:138-155, :202） | 同上 | 同上 |
| 三 drill 共同 | `client_id: "drill-client"` 已有 | stub 默认 sub 对齐 `"drill-client"`（R2 强制 sub==client_id 后必须一致） |

## 2. Compatibility constraints

1. **aero-auth 零生产 diff（硬红线）**：验证器只读消费。基线 = 当前工作树（含前一批次未提交 additive 改动）。`git diff crates/aero-auth` 相对当前工作树必须为空（G6）。由此派生：**不得**给 `OidcError` 加 typed error、**不得**迁移验证器到 common、**不得**改验证器签名。
2. **错误分类单一映射**：全部 `OidcError` 变体 → `DeliveryError::Transient`（与今日 ClaimRejection 臂逐字一致）；签名/结构面仍由 `verify_token_signature` 先行分类（mechanism-Err→Transient / unknown-kid、非 RS256、坏签名→Permanent(SignatureRejected)）。relay 状态机（Forbidden/Permanent/Transient 三臂）**零行为变更**。
3. **`RelayConfig.jwks_uri` 保持 `Option<Url>`**：struct-literal 直构面（**6 处 `RelayConfig {` 字面量**——3 drills + claim_validation.rs:22 + state_machine.rs:44 + relay.rs:303；migration-reviewer F6 勘误，非 12+；config.rs:27/:53 是定义/impl 非字面量）不破坏编译；强制化在 `AuditClient::new`（`None → bail!`）与 `from_env`（启用态缺 JWKS → bail）两层实施。disabled 态（`Ok(None)`）仍然可表示。
4. **部署 env 语义变化（fail-closed 面，staging 待联调）**：
   - `AERO_AUDIT_JWKS_URL`：opt-in → **必填**（connector 启用时）。旧部署缺此 env → boot bail，relay 不启动、不 claim、不投递。
   - `AERO_AUDIT_EXPECTED_SUB`：**删除**。遗留 env 触发 stray 检测（config.rs:59-64：connector 启用路径外的 `AERO_AUDIT_*` 残留 → bail）或直接失效（启用路径内不再读取）。设置过 `AERO_AUDIT_EXPECTED_SUB="aero-im.source"` 而 token sub≠client_id 的部署 → 全部 fail-closed（Transient requeue）直到 sub 对齐。
5. **token 契约收紧（行为变更 ×3，均 fail-closed 不丢不死）**：
   - `nbf` 由 validated-when-present → **必填**（验证器 `required_spec_claims` 含 nbf）；
   - `typ ∈ {at+jwt, application/at+jwt}`（手写平面不查 typ；jsonwebtoken 默认 typ="JWT" 会被拒——R5 stub 修正即此）；
   - `iat` 必填 + future-iat 拒、`jti` 非空/≤1024/无控制字符、`sub == client_id`（替代可配置 expected_sub）。
   缺项 token → Transient requeue（不 POST、不进 dead、不丢行）；修复路径 = IdP 侧（B5-4 配给门修漂移）。
6. **双验签为纵深非重复出网**：`verify_token_signature`（分类面）+ 验证器内部 decode 验签（同 key source `JwksKeyProvider`，TTL 缓存共享）——零新增网络往返。保留签名面是因为 `decoding_key_fallible` 的 mechanism-Err / unknown-kid 判别是验证器 infallible 面给不出的（前一批次 F2/F7/D10 产物）。
7. **v1 snaplink 投递语义零变化**：仅 :23 字面量替换；v1 退役属 campaign 决策，不在此 direction。
8. **测试夹具字面量豁免**（G5）：claim_validation.rs/state_machine.rs/stub 默认 claims 中的 `"audit:event:write"` 是契约被测值，可保留字面量；生产代码（client.rs:43 / config.rs:119 / snaplink http.rs:23 / drill ×3）必须引用常量。
9. **`validate_token_shape` 保留**：TokenResponse 形状护栏（access_token 非空等）与 claims 契约正交，删除面不含它。

## 3. Failure modes（含处理路径与不变量）

| # | 故障 | 触发 | 处理 | 不变量 |
|---|---|---|---|---|
| F1 | JWKS 缺失部署 | connector 启用 + 无 `AERO_AUDIT_JWKS_URL` | `from_env` bail → **整个 server boot bail**（main.rs:251-265 传播；`AuditClient::new` 二道 bail 兜底直构面） | 无「跳过验签的投递路径」（G3 核心） |
| F2 | 遗留 `AERO_AUDIT_EXPECTED_SUB` | 部署残留 env | disabled 态 stray bail / 启用态**忽略——load-bearing**（runbook「env 先行、后删」依赖 B_new 容忍遗留 env；安全论证：替代检查 `sub==client_id` 严格更强，无任何弱化面；security-review F4 的「启用路径 stray-bail」**明确不接受**——会破坏 §4 step 10 回滚顺序）。切换后以 `last_error`/delivery stats 监控 sub 漂移（静默 requeue，仅 warn 日志） | fail-loud 或 fail-closed，非静默。**测试**：新增 config 单测——`AERO_AUDIT_TOKEN_ENDPOINT` unset 且仅留 `AERO_AUDIT_EXPECTED_SUB` → `from_env` bail（`unset while … present`，config.rs:59-64；已现场确认今日 6 个 config 单测无一覆盖此路径） |
| F3 | IdP token 缺 nbf/iat/jti/typ 不合/sub≠client_id | 真实 token 不合新契约 | 验证器拒绝 → invalidate_token + Transient + warn → 行 requeue（永不 dead） | fail-closed 无 POST；B5-4 修漂移 |
| F4 | JWKS endpoint 不可达/刷新失败 | 出网故障 | `decoding_key_fallible` Err → Transient requeue（语义与今日一致） | 永不 dead |
| F5 | unknown-kid（JWKS 滞后 IdP） | 新 key 未入 JWKS | Permanent(SignatureRejected) → attempt-1 requeue → attempt-2 dead（D10 10s throttle 保持） | `signature_rejected_dead_after_exactly_two_attempts` 绿 |
| F6 | 非 RS256 alg / 坏签名 / opaque token | 篡改或错配 | 签名面先行命中 → Permanent(SignatureRejected)；**opaque 分类从今日 Transient（JWKS-off 面）改为 Permanent（签名面 `decode_header` 失败）** | posts==0；`tampered_signature_is_rejected_before_any_post`（:492）绿；`jwks_off_keeps_opaque_token_transient`（:447）前提被删——删除或翻转（见 §5 R1+G2 用例命运） |
| F7 | 401 → 刷新 token 仍不合 claims | IdP 侧持续漂移 | 刷新后重新过验证器 → Transient（无第二次 POST） | `refreshed_token_must_repass_claim_validation_before_retry_post` 绿 |
| F8 | claims 面 OidcError（其余变体） | 任意 claims 违规 | 全部 → Transient（单一映射，不按文案串匹配） | 与今日 ClaimRejection 臂逐字一致 |
| F9 | stub/drill 夹具未随迁 | 迁移中途 | 测试全量红（构造期/契约期 fail-loud） | 迁移顺序 §5 消除 |
| F10 | 双验签 key 源分歧 | key rotation 竞态 | 同 TTL 缓存共享；签名面先行，验证器 decode 二道 | 零新增出网。**测试**：counting `KeyProvider` wrapper（包 `JwksKeyProvider`）断言一次 deliver 内 fetch 数 == 1；或显式标注「cache-shared 设计，不测试钉」（acceptance-oracle 钉于本行——二选一皆可满足，非空缺） |
| F11 | 同 IdP 兄弟 client 的合法 token（security-review F1 残留洞） | sibling client 被授 audit scope + audience，`sub==client_id` 仅 token 内一致 | 验证器通过（oidc.rs:461 只查 token 内一致）→ **connector 后检查 `claims.client_id != config.client_id`**（§1.3）→ invalidate + Transient + warn → 行 requeue（永不 dead） | fail-closed 无 POST；负例 `sibling_client_token_is_rejected_before_any_post`（§5 R2+G3，G3 断言「另一 client 的合法 token 不投递」） |

## 4. Migration steps（sequenced，每步可独立验证）

1. **aero-common 常量**（G5 先落）：加 `SCOPE_AUDIT_EVENT_WRITE` + 值 pin（镜像 audit.rs:269 模式）。验证：`cargo test -p aero-common`；`rg -n '"audit:event:write"' crates/aero-common/src` 仅 1 命中。
2. **config.rs**：删 `expected_sub` 字段 + env 解析 + `JWKS_ENV_KEYS` 项；`expected_scope` 默认引常量；`from_env` 启用态 JWKS 缺失 bail；翻转/新增 env 单测（`jwks_url_absent_parses_none`:342 语义反转，见 §1.2）。
3. **client.rs**：`keys` 非 Option（:108/:137）→ `new` bail（:126）→ `claims_config` 字段 + `deliver` claims 面替换（:166）→ **F11 后检查（`claims.client_id != config.client_id` → invalidate + Transient + warn，零 aero-auth diff）** → 删跳过臂（:347-349）→ 删 `validate_token_claims*`/`ClaimRejection`/`decode_jwt_claims`/`TIME_CLAIM_LEEWAY_SECS`/`SCOPE_AUDIT`。
4. **stub.rs**（R5）：默认自签 + jwks_keys 默认 + typ 修正 + 默认 claims 完整化（sub==client_id=="drill-client"）。
5. **snaplink http.rs:23**：字面量 → 常量引用（机械替换）。
6. **测试迁移**：claim_validation.rs 24 用例 → 签名 token + keys 面（`deliver_with_claims` 夹具升级为 `make_rs256_jwt` + `with_key_provider`/stub /jwks——`AuditClient::new` 对 jwks_uri None bail 后该 helper 构造即断，12 用例随 helper 一次迁移），**新增** `accepts_scopes_array_claim` / `accepts_space_delimited_scope_claim`（G2，枚举见 §5 R1+G2）+ **`sibling_client_token_is_rejected_before_any_post`（F11，枚举见 §5 R2+G3）**；**2 用例命运决策**（§5 R1+G2）：`exp_nbf_missing_claims_still_pass`（:415，与 required-nbf 矛盾）与 `jwks_off_keeps_opaque_token_transient`（:447，JWKS-off 面被删）删除或翻转；state_machine.rs 8 用例（config 直构面删 expected_sub、jwks_uri 指向 stub）；relay.rs 内部单测随迁（:311/:466 夹具保留字面量，rg guard 豁免）。
7. **drill 随迁**（R6）：t11 占位 https jwks_uri；relay/priority 走 stub `/jwks`；删 expected_sub。
8. **全量门禁**：§6 G1–G6 全部命令。
9. **staging（AGENTS §4.5，写「待联调」不写「完成」）**：真实 token endpoint + 真实 JWKS URL + 真实签发 token 的 `sub==client_id`、nbf 存在、typ=at+jwt 核对（migration-reviewer F4 增补：**iat 必填 + future-iat 拒**、**JWKS 必须服务当前 kid**——否则 F5 dead-after-2）；**生产切换 blocked on 此 gate**（显式声明，非隐含）。
10. **部署/回滚 runbook（migration-reviewer F1/F2，design-gate 必含）**：上线 = **env 先行**——先加 `AERO_AUDIT_JWKS_URL`（保留 `EXPECTED_SUB`：旧二进制立即 JWKS-on，洞提前闭合）→ 再换二进制（B_new 启用路径容忍遗留 `EXPECTED_SUB`，§3 F2 论证）→ step-9 gate 通过后删 `EXPECTED_SUB`。**二进制先行 = 全服 boot bail 直至 env 修好，禁止**。**回滚 = 先恢复 `AERO_AUDIT_EXPECTED_SUB`（设为 IdP 当前 sub；IdP 若已翻转则 = client_id）再回退二进制**；可选 unset `AERO_AUDIT_JWKS_URL` 完全回 C_old——回滚时 URL 仍设比迁移前更严（JWKS-on），双向安全。禁用态 + 遗留 `AERO_AUDIT_*` → stray bail 保持（config.rs:59-64）。**回滚永远不可能静默重开 JWKS-off 洞**（跳过臂要求 `keys: None` ← 旧二进制要求 `jwks_uri: None` ← 旧 config）。

## 5. Testable acceptance mapping（requirement → 断言 → 命令）

| Req | Acceptance | 断言 / 命令 |
|---|---|---|
| R1+G1 | 缺 `audit:event:write` scope → 无 POST | `missing_audit_scope_is_rejected_before_any_post`（claim_validation.rs:143，**posts==0 断言在 :152**，迁移到签名 token + keys 面）：scope=`"billing:entitlement:read"` 的 RS256 at+jwt（余 claims 全合法，sub==client_id=="drill-client"）→ `outcome.is_err()` + `assert_eq!(stub.posts(), 0)`；T-11 drill `cargo run -p aero-audit-connector --bin aero-audit-t11-drill` 退出 0 + `scripts/test-integration.sh` 产 `B5-CHECK t11-fail-closed: PASS` |
| R1+G2 | 双 scope 形状经 `granted_scopes()` 接受 | **新增** `accepts_scopes_array_claim`（RFC 9068 数组形状 `scopes: ["audit:event:write"]`，`granted_scopes()` 第一来源 oidc.rs:401-408；connector 层现零数组用法——`rg '"scopes"' crates/aero-audit-connector` 全 0）→ `outcome.is_ok()` + `posts() >= 1`，夹具 = `make_rs256_jwt` + `with_key_provider`/stub `/jwks`；**新增** `accepts_space_delimited_scope_claim`（OAuth 空格分隔 `scope: "audit:event:write metering:read"` → 同上）；（可选）数组负向镜像 `scopes: ["billing:entitlement:read"]` → Transient + posts==0。验证器层等价物已有（oidc/tests.rs:338/:356）——缺的是 deliver 路径集成面 |
| R1+G2（用例命运） | 24 用例「全量随迁绿」修正 | **22 随迁 + 2 命运决策**：`exp_nbf_missing_claims_still_pass`（:415，断言无 exp/nbf 也 ok）与验证器 `required_spec_claims` 含 exp/nbf/iat/jti（oidc.rs:452-454）**直接矛盾**——删除或翻转为负向（Transient + posts==0）；`jwks_off_keeps_opaque_token_transient`（:447）前提 = 被删的 JWKS-off 面，迁移后 opaque 走签名面 `decode_header` 失败 → Permanent(SignatureRejected)（F6）——删除或翻转。`opaque_non_jwt_token_is_rejected_before_any_post`（:169）只断言 is_err + posts==0，分类无关，存活。**计数再钉（F11 后）**：22 随迁 + 2 命运 + 2 双形状 + 1 config bail（lib 面）+ **1 sibling-client 负例（F11）** = claim_validation **25–27 用例**（两命运删除=25、一翻一删=26、全翻转=27）；全 crate **47–49 passed / 3 ignored**（lib 14 + state_machine 8 不变）——G6「≥47」成立（旧钉「≈25-26 / ≥46」按 +1 位移） |
| R2+G3 | 验签先于投递、无 JWKS-off 路径 | 既有 `tampered_signature_is_rejected_before_any_post`（:492，posts==0 在 :514）+ `signature_rejected_dead_after_exactly_two_attempts`（state_machine.rs:470，dead + last_error 含 `SignatureRejected` 不含 `transient`，:512-517）+ F4 pin `jwks_fetch_failure_requeues_without_any_post`（:525，`audit jwks unavailable` + posts==0）保持绿；**新增** config 单测：启用态缺 `AERO_AUDIT_JWKS_URL` → bail（翻转 `jwks_url_absent_parses_none` :342）；`AuditClient::new` 缺 key 源 → `Err`；**新增（F11）`sibling_client_token_is_rejected_before_any_post`**（claim_validation.rs，锚 :156-167 区 `wrong_subject…` 用例之后；fixture 同 `tampered_signature…` :492 模式：`SinkBehavior { signing_key: Some(trusted_key()), token_claims: {iss/aud/scope 合法 + sub: "sibling-client" + client_id: "sibling-client" + 完整 RFC 9068 形状（exp/nbf/iat/jti/typ）} }` + `StaticKeyProvider::single(stub.decoding_key())`，config client_id 保持 `"drill-client"`（claim_validation.rs:26）→ 验证器通过（sub==client_id 仅 token 内一致）→ **后检查拒绝** → `Err(Transient)` + `assert_eq!(stub.posts(), 0)`；与 :156（sub≠client_id → 验证器 oidc.rs:461 拒）构成两层 sub 契约互补负例） |
| R2+G6 | 构造期 fail-loud | `cargo test -p aero-audit-connector --lib`（config env 单测段） |
| R3+G3 | 篡改 token 双验签面 | 签名面先命中 → Permanent 面（上）；claims 面违规（trusted key 签但 claims 不合）→ Transient + posts==0（`refreshed_token_must_repass_claim_validation_before_retry_post`:268 保持） |
| R4+G5 | 无第二 scope 字面量站点 | **可执行 guard**（落 b5-pin.sh，迁移完成前必须红）：`hits=$(rg -l '"audit:event:write"' crates/*/src --glob '*.rs' | grep -vE 'stub\.rs|relay\.rs'); [ "$hits" = "crates/aero-common/src/model/audit.rs" ]`——豁免 **stub.rs:94**（默认夹具，constraint 8）与 **relay.rs:311/:466**（`#[cfg(test)]` 夹具，:245-246，豁免清单补名；不豁免则迁移后命中 3 文件，guard 不可满足）；今日（迁移前）同命令命中 8 文件（client.rs:43 / config.rs:120 / snaplink http.rs:23 / drills ×3 / stub.rs / relay.rs ×2）；**F11 修订再钉（豁免清单零变化）**：后检查比较 `config.client_id`，不新增任何 src 字面量站点；新负例夹具字面量在 `tests/claim_validation.rs`——不在 `crates/*/src` glob 内，豁免清单**维持 stub.rs + relay.rs 两项不变**；允许唯一命中 = `crates/aero-common/src/model/audit.rs`；值 pin `assert_eq!(SCOPE_AUDIT_EVENT_WRITE, "audit:event:write")`（镜像 audit.rs:269 模式）绿；`cargo check --workspace` 干净 |
| R5+R6 | drill 全链路自签 | relay/priority drill 走 stub `/jwks`：`cargo run -p aero-audit-connector --bin aero-audit-relay-drill` / `--bin aero-audit-priority-drill` 退出 0；**verdict 槽名 = `a3-relay-drill`**（b5-pin.sh 槽名与 `assert_b5_contract_pin` grep 一致；test-integration.sh 产 `B5-CHECK a3-relay-drill: PASS`）+ `B5-CHECK moderation-priority-drill: PASS` |
| G4 | moderation priority 不变 | `pg.rs::mixed_priority_claim_orders_moderation_first_then_fifo`（:536，`#[ignore]`，throwaway `DATABASE_URL`）绿；`priority_first_claim_preempts_fifo_and_limit1_keeps_top_lane`（state_machine.rs:360）绿；priority drill 绿 |
| G6 | 回归守卫 | `cargo test -p aero-auth --lib` **85/85 零改动**（本次实跑确认）；`cargo test -p aero-audit-connector --all-targets` ≥ **47** passed / 3 ignored（本次实跑 46/3；迁移后 47–49/3，见 R1+G2 计数再钉）；`cargo clippy --workspace --all-targets` 无新增警告；`scripts/{truth-check,file-size-check}.sh` 0 违规；`scripts/b5-pin.sh` **37/37**（本次实跑 15+22 计数 + guard 回归用例全过）；**aero-auth diff guard（可执行形态）**：先 commit 当前工作树为基线（其已含前批次未提交 aero-auth additive 改动——`git diff crates/aero-auth` vs HEAD 今日非空），此后 `git diff --exit-code crates/aero-auth` 必须空；**新增 `assert_deleted_symbols_absent`（b5-pin.sh）**：`rg -n 'validate_token_claims|ClaimRejection|decode_jwt_claims|TIME_CLAIM_LEEWAY_SECS|expected_sub|AERO_AUDIT_EXPECTED_SUB' crates/aero-audit-connector/src crates/aero-server/src` 必须 0 命中（6 符号全 pub/独立项，重加编译干净——**编译成功检测不了重加**，diff guard 也看不见 connector 符号；**F11 后检查用 `claims.client_id` 字段访问，不在 6 符号删除清单内，guard 不变量不变**；**今日 b5-pin.sh 尚无此 guard 与 R4+G5 guard——实施期落脚本（同 acceptance-oracle 钉）**）；`scripts/test-integration.sh` A3/T-11/priority drill 段全绿（a3-relay-drill 槽名已实跑确认：b5-pin.sh:39 + test-integration.sh:338/:341 + :582 relay-legs grep） |

## 6. Out of scope（与 spec §4 一致，relay 零行为变更）

- aero-auth 生产代码（零 diff 红线）；`integrations.rs`（aero.notify.publish 域）；outbox/lease/退避/回执/payload guard/relay 三臂分类（逐字保持）；0239-0241 + `audit_governance.rs`（B5-1）；priority（B5-3）；`aero-eng/src/audit_provision.rs`（B5-4）；per-client allowlist/scope registry/freshness floor；`base64` 依赖移除（legacy `make_jwt` 仍用）。

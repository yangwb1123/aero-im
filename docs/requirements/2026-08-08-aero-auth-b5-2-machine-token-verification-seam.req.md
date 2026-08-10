# Requirements Spec — aero-auth 机器令牌（client_credentials）验证 seam 作为 relay connector 唯一验证源（B5-2：杀重复 4/8 契约验证器 + alg:none stub）

- **Module (analysis root)**: `crates/aero-auth` — seam 所在；**零 aero-auth 生产代码改动**（seam 已完整公开并 re-export，见 E1/E3）。落地动作全部在消费侧 `crates/aero-audit-connector`（untracked，前一批次产物）
- **Direction**: "Make aero-auth's tested client_credentials seam the single verification source for the relay connector (kill the duplicate 4/8-contract verifier + alg:none stub)"（value 9 / risk_reduction 9 / effort 4 / confidence 9，`proposed: false`）
- **Source analysis**: `docs/auto/analyses/crates-aero-auth-0b9b4b9f.json`（direction #0）
- **Prior art**: `docs/requirements/2026-08-07-aero-auth-b5-2-machine-token-verification-seam.req.md`（昨日版）与 `docs/design/2026-08-07-aero-auth-b5-2-machine-token-verification-seam.design.md`。本文档以 2026-08-08 现场复核为准，**取代**昨日版的证据行号与状态事实（见 §2 勘误）
- **Status**: Requirements（全部引用经源码 grep + 实跑测试复核）
- **Verification date**: 2026-08-08。行号是复核时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点（AGENTS.md §0）

## 1. Evidence verification（direction 引用逐条核对，全部 ✅）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `oidc.rs` — `validate_client_credentials_token` :406、`ClientCredentialsTokenConfig` :364、`granted_scopes` :387、sub==client_id :445-450、future-iat 拒绝 :453、RS256/EdDSA allowlist :423-428、`LEEWAY_SECS` :293 | ✅ 全部精确命中 `crates/aero-auth/src/oidc.rs`。`LEEWAY_SECS: u64 = 60` :293；`ClientCredentialsTokenConfig {issuer, audience, required_scopes}` :364-368；`ClientCredentialsClaims` :372-380；`granted_scopes` :387-394（`scopes` 数组 ∪ 空格分隔 `scope` 的 BTreeSet 归一化）；`validate_client_credentials_token` :406-471：at+jwt typ 白名单 :414-417 → alg 白名单 RS256/EdDSA :423-428 → `KeyProvider::decoding_key(kid)` :431-434 → `required_spec_claims` 含 `iss,aud,exp,nbf,iat,jti,sub,client_id` :440 → sub==client_id + `valid_identity_component`（非空/无空白/无控制字符）:445-450 → future-iat 拒绝 :453 → jti 非空/≤1024/无控制字符 :455-459 → required_scopes ⊆ granted :461-466 |
| E2 | `oidc/tests.rs` — 8 个 client_credentials 测试 :332/:350/:366/:378/:392/:406/:429/:452 + 夹具 :163/:214/:230/:64 | ✅ 全部精确命中 `crates/aero-auth/src/oidc/tests.rs`：`keypair` :64（RS256）；`client_credentials_cfg` :163-168；`good_client_access_claims` :214-228（sub==client_id=="erp-production"、scopes 数组、jti、nbf/iat/exp）；`sign_client_access_token` :230-238（RS256 + kid + 显式 typ）；8 测试函数 = `accepts_client_credentials_at_jwt_with_scopes_array` :332、`accepts_client_credentials_with_space_delimited_scope` :350、`rejects_client_credentials_token_with_wrong_typ` :366、`rejects_client_credentials_token_when_sub_differs_from_client_id` :378、`rejects_client_credentials_token_without_required_scope` :392、`rejects_client_credentials_token_with_wrong_issuer_or_audience` :406、`rejects_expired_client_credentials_token_and_token_without_nbf` :429、`rejects_missing_or_invalid_client_credentials_iat_and_jti` :452（含 5 子例：missing/future iat、missing/empty/control jti）。**实跑确认**：`cargo test -p aero-auth --lib` = **81 passed / 0 failed**（基线绿，含此 8 项） |
| E3 | `lib.rs:28-29` — seam re-export | ✅ 命中（块边界微移）：`crates/aero-auth/src/lib.rs:27-31` `pub use oidc::{validate_client_credentials_token, validate_id_token, validate_jwks_uri, ClientCredentialsClaims, ClientCredentialsTokenConfig, JwksKeyProvider, JwksUriError, KeyProvider, OidcClaims, OidcConfig, OidcError, StaticKeyProvider}`——验证器 + 配置 + KeyProvider trait + 静态/JWKS 实现已全公开，**零改动消费** |
| E4 | 唯一生产消费方 | ✅ `crates/aero-server/src/integrations.rs` `authenticate_machine` :233-246（`:240` = `validate_client_credentials_token(token, &config.token, provider)`）；`INTEGRATION_JWKS` OnceLock :63 → `JwksKeyProvider::new(config.jwks_uri.clone())`；全仓 grep：`validate_client_credentials_token` 仅出现于 lib.rs/oidc.rs/oidc/tests.rs/integrations.rs——**唯一消费者注记成立** |
| E5 | connector 手写重复验证器：`client.rs::validate_token_claims` :197-263（4/8 契约）、`decode_jwt_claims` :415（无验签） | ✅ 全部命中 `crates/aero-audit-connector/src/client.rs`（crate 全仓 untracked，`git status` `?? crates/aero-audit-connector/`）：`deliver` :120-190 每次 POST 前置调 `validate_token_claims` :126（fail-closed 文案 "no delivery attempted"）；`validate_token_claims` :197-263 = 形状（非空/≤16KiB/无控制字符）+ **仅 4 查**：iss :212-221、aud :222-235、scope :236-246、sub 比对**配置的 `expected_sub`** :247-257（非 client_id）——**无 typ/exp/nbf/iat/jti/sub==client_id/签名**；`decode_jwt_claims` :415-435（base64url 解 payload，注释明言 "without signature verification"）；`validate_token_shape` :398-413 保留作前置护栏（不在删除面） |
| E6 | `stub.rs::make_jwt` :233-245 — alg:none 假签名 | ✅ 命中 `crates/aero-audit-connector/src/stub.rs`：`make_jwt` :233-245 发 `{"alg":"none","typ":"JWT"}` 头 + 假签名段 `ZmFrZS1zaWduYXR1cmU`；doc 注释 :229-231 明言 "The connector does not verify signatures … fake signature is enough"。**全部既有测试/ drill 均由此驱动**：`StubSink::default().token_claims` :45-52（sub=`"aero-im.source"`，非 client_id=`"drill-client"`）；token 端点 :179 `make_jwt(claims)`；`posts()` :116-118 计数 |
| E7 | connector `Cargo.toml` 无 aero-auth（仅 base64 "0.22"） | ✅ 命中：`crates/aero-audit-connector/Cargo.toml` 依赖面 = workspace-pinned（tokio/tokio-util/async-trait/futures/reqwest/sqlx/serde/serde_json/thiserror/anyhow/tracing/time/uuid）+ `base64 = "0.22"` :30-32（注释 "Unverified JWT claim payload decode"）——**无 aero-auth** |
| E8 | 测试面：claim_validation.rs + state_machine.rs（"All 17 tests"）+ `forbidden_dead_on_first_attempt` :216 | ⚠️ **行号/计数漂移（符号全部命中）**：`tests/claim_validation.rs` 实为 **11** 个测试函数（wrong_issuer :76 / missing_audience :89 / missing_audit_scope :102 / wrong_subject :115 / opaque_non_jwt :128 / valid_token :137 / unauthorized_refreshes_once :150 / refreshed_token_must_repass :174 / client_classifies_statuses :203 / payload_guard :234 / jwt_claims_decode_roundtrip :254）+ `tests/state_machine.rs` **7** 个（stale_token :64 / backoff :112 / permanent_dead :145 / **forbidden_dead_on_first_attempt :231**（direction 引 :216，漂移）/ happy_path :262 / skew :297 / priority_first :334）= **18**（非 17）。`relay.rs` 内部另有 8 个单测（含 `transient_claim_drift_requeues_without_any_post` :398——wrong-iss → Transient requeue 无 POST 的对照语义）+ 3 个 PG 门控 `#[ignore]`。**实跑确认**：`cargo test -p aero-audit-connector --all-targets` = **26 passed / 0 failed / 3 ignored** |
| E9 | 状态机：403→dead、claim 校验失败→Transient | ✅ `relay.rs` `deliver_claim` :182-260：`Err(DeliveryError::Forbidden)` → `mark_dead` :202-214（T-11 已实现）；`Permanent` → 第 1 次 requeue 第 2 次 dead :216-230；`Transient` → requeue :232-244。**scope-deficient/未配给目前走 Transient 无限重试**（acception 要补 dead ≤1 次的面）；`fake.rs` `FakeStatus::{Ready,Claimed,Delivered,Dead}` :28-32（Dead=3），`is_dead_at` :48；`stub.rs` `SinkBehavior {token_claims, token_claims_after_first, events_status, receipt_valid, unauthorized_once, delay_ms}` :24-40——**无 `token_style`/`token_endpoint_status` 字段（AC2(b) 需新增）** |
| E10 | 设计文档 `docs/design/2026-08-07-aero-auth-b5-2-machine-token-verification-seam.design.md`（"fully evidence-adjudicated"；§2.3-§2.6、§6 AC1-AC3） | ✅ 存在且内容自洽（232 行）：§2.3 client.rs 替换面、§2.5 stub 签名化、§2.6 测试随迁、§6 AC1（8 项矩阵 + posts()==0 断言）/AC2（T-11 dead 三面）/AC3（allowlist [PROPOSED]）。**但其中「0239 未落库 → drill SKIP」状态已过期**（见 §2 勘误 ④） |

## 2. 勘误（2026-08-08 现场 vs 引用/昨日文档）

| # | 引用声称 | 实际（已复核） | 影响 |
|---|---|---|---|
| ① | "All 17 tests (claim_validation.rs + state_machine.rs)" | **18**（11 + 7） | 接受面口径按 18 计；不改变验收（全量迁移绿） |
| ② | `forbidden_dead_on_first_attempt` :216 | **:231**（昨日 design/req 文档同引 :216） | 行号漂移，符号稳定；验收引用改 :231 |
| ③ | `bin/aero-audit-relay-drill.rs` | **`src/bin/aero-audit-relay-drill.rs`**（同目录还有 `aero-audit-t11-drill.rs`、`aero-audit-priority-drill.rs`） | 路径补齐；drill 用 `expected_sub: "aero-im.source"`（:92）+ `client_id: "drill-client"`（:87）——stub 换签名后 `expected_sub` 须对齐 `client_id` |
| ④ | "0239 未落库 → drill 显式 SKIP"（昨日文档 §0/§5/§6） | **0239/0240/0241 已落库**（`migrations/` 尾号 0241；`scripts/test-integration.sh` :306-341 A3 relay drill、:344+ T-11 drill 的门 `[ -f migrations/0239_audit_governance_outbox.sql ]` 已开，drill 现实际运行） | drill 已是活门禁：stub 签名化必须连带 drill config 随迁（`expected_sub` → `"drill-client"`），否则 T-11/A3 drill 在集成脚本里红 |
| ⑤ | lib.rs :28-29 | re-export 块 :27-31（`validate_client_credentials_token` 在 :28） | 无实质影响 |
| ⑥ | connector 测试面数（昨日文档 §6 "24 项"） | 26 passed（lib 8 + claim_validation 11 + state_machine 7）+ 3 PG-ignored | 随迁面按实跑数计 |

**核心结论不变且更强**：direction 预警的重复实现已实际存在于工作树（E5/E6），且 18 项测试 + drill 全部由 alg:none 假签名 token 驱动——T-11 fail-closed 门（403/scope-deficient → dead）当前闸在**任何攻击者本地可铸**的断言上；复用 E1 验证器后签名验证由 "[PROPOSED]/不验签" 变为**强制**。

## 3. Verified current state

```
a) aero-auth 已测验证器（SEAM = 唯一验证源，零改动）   crates/aero-auth/src/oidc.rs
   validate_client_credentials_token :406 —— RFC 9068 at+jwt：typ 白名单 → alg 白名单(RS256/EdDSA)
     → KeyProvider(kid) → iss/aud/exp/nbf/iat/jti/sub/client_id 全 required → sub==client_id
     → future-iat 拒绝 → jti 约束 → required_scopes ⊆ granted_scopes（数组 ∪ 空格分隔）
   LEEWAY_SECS=60 :293；re-export lib.rs:27-31；测试 oidc/tests.rs 8 项（:332-478）实跑绿

b) 唯一生产消费者                                     crates/aero-server/src/integrations.rs
   authenticate_machine :233-246（:240 调用）—— AERO__INTEGRATIONS__*/AERO__OIDC__* + INTEGRATION_JWKS

c) B5-2 connector（untracked，重复实现已发生）        crates/aero-audit-connector/
   client.rs: validate_token_claims :197-263（iss/aud/scope/sub 四查，sub 比对 expected_sub）
               decode_jwt_claims :415-435（base64url 无验签）；deliver :120 每次 POST 前置
   stub.rs:    make_jwt :233-245 发 alg:none 假签名；token_claims 默认 sub="aero-im.source"
   relay.rs:   403→dead :202；Permanent→requeue×1→dead :216；Transient→requeue :232；
               transient_claim_drift_requeues_without_any_post :398（wrong-iss 仍 Transient）
   Cargo.toml: 无 aero-auth；base64 "0.22" 直接依赖
   config.rs:  expected_iss/aud/scope/sub :31-37；from_env :52 presence-gated（AERO_AUDIT_TOKEN_ENDPOINT）
               expected_scope 默认 "audit:event:write" :89-90；SCOPE_AUDIT 常量 client.rs:32

d) 环境（2026-08-08）：0239/0240/0241 已落库（B5-1 完成）；drill 门已开（test-integration.sh :320/:344）
   workspace：aero-audit-connector member :29；jsonwebtoken = "9" :133（aero-auth 同款，零新增供应链项）
```

**实跑基线**：`cargo test -p aero-auth --lib` = 81 passed / 0 failed；`cargo test -p aero-audit-connector --all-targets` = 26 passed / 0 failed / 3 ignored（PG 门控）。

## 4. Scope

**In scope（本 direction，B5-2 机器令牌验证 seam 消费侧替换）**：
- `crates/aero-audit-connector` 增 `aero-auth.workspace = true`（workspace crate，零新增第三方供应链项）；`base64 = "0.22"` 随 `decode_jwt_claims` 删除而移除。
- `AuditClient::deliver` 的 token 校验替换为 `aero_auth::validate_client_credentials_token`（`ClientCredentialsTokenConfig` + `KeyProvider`），**首 POST 前与 401-刷新重试 POST 前每次校验**；删除手写 `decode_jwt_claims` + `validate_token_claims` 的 claim 检查部分（`validate_token_shape` 形状快速失败保留为前置护栏，非契约校验）。
- 生产 KeyProvider = `JwksKeyProvider`（`AERO_AUDIT_JWKS_URI`，fail-loud）；测试 KeyProvider = `StaticKeyProvider`。
- T-11 dead 语义补全：scope-deficient token 与 token-endpoint 403（未配给）→ dead ≤1 次尝试（sink 403 → dead 已实现，保持）。
- `stub.rs` 换发 RS256 真签 at+jwt（`jsonwebtoken::encode`，workspace 已钉 :133）；opaque/alg:none 保留为拒绝路径用例。
- claim 契约测试矩阵镜像 `oidc/tests.rs` 8 项（测试内 RS256 keypair，真签）。
- drill 随迁（`src/bin/aero-audit-relay-drill.rs` config：`expected_sub` → `"drill-client"`，与 `client_id` 对齐）。

**Out of scope（并行方向/既有面——本 direction 不建、不改）**：
- **`crates/aero-auth` 生产代码改动：零**（红线，验收 git-diff 守卫；测试夹具镜像而非导出 test-support）。
- `crates/aero-server/src/integrations.rs` 与 `snaplink_commercial/`（唯一消费者 + v1 入站零校验面——v1 退役属 B5-1/campaign 决策，本 direction 不碰）。
- outbox/lease/退避/投递/回执/payload guard/config 其余字段 → 既有 B5-2 connector spec（`docs/requirements/2026-08-07-aero-ai-b5-2-audit-connector.req.md`）已钉；本 spec 只改 claim 校验相关分类与配置面。
- 0239/0240/0241 与 `audit_governance.rs` → **B5-1（已落库，不动）**；priority → **B5-3**；配给门（`aero-cli audit-provision-check`）→ **B5-4**（本 direction 交付 403/scope-deficient → dead 的 enforcement point，配给门消费此语义）。
- **per-client allowlist + scope registry（昨日文档 AC3/[PROPOSED]）→ 明确不做**（direction acceptance 未含；registry 本体在 IdP 仓、仓外）。
- **F10 可观测/ops-requeue（设计文档 §2.7，delivery-semantics review 追加）→ 明确不做**（标注 "非 requirements AC 增量"；本 spec 不扩面）。
- freshness floor（max token age，问题陈述提及、acceptance 未含）→ 不做。

## 5. Requirements

### R1 — connector 消费 aero-auth 验证器，替换手写重复实现
`crates/aero-audit-connector/Cargo.toml` 增 `aero-auth.workspace = true`；`AuditClient` 构造时从 `RelayConfig` 建一次 `ClientCredentialsTokenConfig { issuer: expected_iss, audience: expected_aud, required_scopes: vec![expected_scope] }`（`expected_scope` 默认 `SCOPE_AUDIT = "audit:event:write"`，client.rs:32）；`deliver`（client.rs:120）内对每个待发 token 调 `aero_auth::validate_client_credentials_token`（含 401-刷新后重试的 token——`refreshed_token_must_repass_claim_validation_before_retry_post` 语义保持）；失败 → 不出网（无 POST）、`invalidate_token` 失效缓存、进 R4 分类。**删除** `decode_jwt_claims` 与 `validate_token_claims`（`validate_token_shape` :398 保留）；`base64` 直接依赖移除。

### R2 — claim 契约全矩阵（POST 前置，fail-closed；全部由 E1 验证器保证）
JOSE typ ∈ {`at+jwt`, `application/at+jwt`}；alg ∈ {RS256, EdDSA} + **签名经 KeyProvider 验证**（kid 匹配，unknown key 拒绝）；`iss`/`aud` 精确匹配；`exp`/`nbf` 生效（LEEWAY 60s）；`iat` 必在且非未来；`jti` 非空/≤1024/无控制字符；`sub == client_id`（`valid_identity_component`）；`granted_scopes` ⊇ `required_scopes`。opaque/畸形 token → `OidcError::MalformedToken` → 同样 fail-closed 无 POST。

### R3 — KeyProvider 接线
生产：`JwksKeyProvider::new(config.jwks_uri)`（`AERO_AUDIT_JWKS_URI` 进 presence-gated `AERO_AUDIT_*` 家族；URI 过 `validate_jwks_uri` 策略——HTTPS 或 loopback-HTTP、无 userinfo/query/fragment；缺失/非法 → `bail!` fail-loud，relay 不启动不 claim）。测试/ drill：`StaticKeyProvider::single(dec)` 注入（`AuditClient::with_key_provider` 或等价构造），不触碰 jwks_uri。**签名验证由 connector 现行 "[PROPOSED]" 姿态变为强制**——方向明示，T-11 方向一致。

### R4 — T-11 fail-closed：unprovisioned / scope-deficient → dead ≤1 次尝试
| 面 | 检测点 | 分类（钉死） |
|---|---|---|
| scope-deficient token（granted 缺 `audit:event:write`） | 验证器 `Invalid("token lacks a required application scope")` | **dead attempt 1**（无 requeue、无 POST）——闭既有 Transient 无限重试环 |
| 未配给 client（token endpoint 403） | `request_token` 状态码 | **dead attempt 1**——403 分类进 `DeliveryError::Forbidden`（既有变体，relay :202 臂直接覆盖；或独立 `Unprovisioned` 变体保 last_error 保真，二选一，relay dead 臂同判） |
| sink 拒绝身份（投递 403） | `deliver` HTTP 403 | dead attempt 1（**已实现** `forbidden_dead_on_first_attempt` :231，保持绿） |
| 其余 claim 违规（wrong iss/aud、sub!=client_id、wrong typ、expired、missing/future iat/jti、unknown key、opaque、坏签名） | 验证器各 `OidcError` 变体 | **Transient requeue + 下次 claim 轮换新 token**（`transient_claim_drift_requeues_without_any_post` :398 语义保持——IdP 配置漂移由 B5-4 修复，本 direction 不擅自扩大 dead 面） |

### R5 — stub 签名化（全部既有测试/ drill 的驱动源随迁）
`stub.rs`：`make_jwt`（:233）改用 `jsonwebtoken::encode` 产 **RS256 真签 at+jwt**（`Header { alg: RS256, kid: Some("test-key"), typ: Some("at+jwt") }`，claims 含 `sub == client_id == "drill-client"`）；暴露 `decoding_key()` 访问器（测试/ drill 建 `StaticKeyProvider`）；`SinkBehavior` 增 `token_style ∈ {Signed, Opaque, AlgNone}`（默认 Signed）与 `token_endpoint_status`（默认 200）——Opaque/AlgNone 保留为**拒绝路径**用例（fail-closed 面）。默认 `token_claims` 的 `sub` 由 `"aero-im.source"` 改 `"drill-client"`。测试与 drill config 的 `expected_sub` 同步收敛为 `"drill-client"`（R2 强制 sub==client_id 后 `expected_sub` 不再参与验证器输入）。

### R6 — claim 契约测试镜像 oidc/tests.rs（37/37 的 in-repo 子集）
`tests/claim_validation.rs` 重写为 8 项契约矩阵（§7 AC1），夹具镜像 E2（测试内 RS256 keypair 真签 at+jwt、kid、显式 typ、sub==client_id）；stub 换发签名 token；opaque/alg:none 保留为拒绝路径测试。"37/37 claim-contract 清单" 全量在仓外（`docs/proposals/audit-contract-batch-aero-im.md` :13 [PROPOSED]）；**in-repo 钉死读法 = `oidc/tests.rs` 的 8 个 client_credentials 测试**（E2），AC1 矩阵与之一一对应。

## 6. 验收门禁（direction acceptance 原样保留，逐条 testable）

### G1（T-11）— `tests/state_machine.rs` 三面，fake outbox + stub sink
- **AC2(a)** scope-deficient **签名** token（stub 发缺 `audit:event:write` 的 RS256 at+jwt）→ `FakeStatus::Dead`、`attempts == 1`、`stub.posts() == 0`、后续 `claim_due` 为空（dead 行永不被选中，无 retry loop）。可测断言：`assert_eq!(row.status, FakeStatus::Dead)`；`assert_eq!(row.attempts, 1)`；`assert_eq!(stub.posts(), 0)`；再次 `claim_due` 迭代为空。
- **AC2(b)** `token_endpoint_status = 403`（未配给）→ `FakeStatus::Dead`、`attempts == 1`、`posts() == 0`。
- **AC2(c)** sink 403 → 既有 `forbidden_dead_on_first_attempt`（**state_machine.rs:231**）保持绿（不改）。

### G2（37/37 in-repo 子集）— `tests/claim_validation.rs` 重写为 8 项契约矩阵（镜像 oidc/tests.rs）
| # | 用例（镜像 `oidc/tests.rs`） | 断言 |
|---|---|---|
| 1 | accepts scopes 数组（`scopes: ["audit:event:write"]`，镜像 :332） | Ok + `posts() >= 1` |
| 2 | accepts 空格分隔 scope（`scope: "audit:event:write metering:read"`，镜像 :350） | Ok + `posts() >= 1` |
| 3 | 拒 wrong typ（头 `typ: "JWT"`，镜像 :366） | `Err(DeliveryError::…)` + `posts() == 0` |
| 4 | 拒 sub != client_id（镜像 :378） | 同上 |
| 5 | 拒缺 `audit:event:write`（scope 为 `billing:entitlement:read`，镜像 :392） | 同上（R4 scope-deficient 面） |
| 6 | 拒 wrong iss / wrong aud（两子例，镜像 :406） | 同上 |
| 7 | 拒 expired / 缺 nbf（镜像 :429） | 同上 |
| 8 | 拒 missing iat / future iat / missing jti / empty jti / control jti（5 子例参数化，镜像 :452） | 同上 |
- **保留路径**：opaque token / alg:none token（`token_style`）→ `posts() == 0`（fail-closed 面）；401-刷新两例随迁保持（`unauthorized_refreshes_once_and_retries_within_the_attempt` posts==2、`refreshed_token_must_repass_claim_validation_before_retry_post` posts==1——重试 POST 前重新过验证器）。
- stub 换签：`make_jwt` = `jsonwebtoken::encode` RS256 at+jwt（kid、sub==client_id）；`expected_sub` 对齐 `"drill-client"`。

### G3 — 回归守卫
- `cargo test -p aero-auth`：8 项 seam 测试**零改动**仍绿（实跑 81/81，含 :332/:350/:366/:378/:392/:406/:429/:452）。
- `cargo test -p aero-audit-connector --all-targets`：全量迁移绿（现 26 passed / 3 ignored；重写后 ≥ 现数，含既有 18 项随迁 + 新增矩阵/T-11 面；`-- --ignored` PG 门控另跑需 `DATABASE_URL`）。
- **git-diff 守卫**：`crates/aero-auth/` 零改动（`git diff --stat crates/aero-auth` 为空）。
- drill 随迁编译 + 语义保持：`cargo test -p aero-audit-connector --all-targets` 覆盖 bin 编译；`scripts/test-integration.sh` 的 A3/T-11 drill（0239 已落库，门已开）在 stub 换签后仍绿。

## 7. Test placement

| Test | Location | Harness |
|---|---|---|
| G2 契约矩阵 8 项 + 子例（真签 RS256，`posts()==0` 断言）+ opaque/alg:none 拒绝 + 401 刷新两例 | `crates/aero-audit-connector/tests/claim_validation.rs`（重写） | `cargo test -p aero-audit-connector`，无 DB（stub sink） |
| G1 T-11 dead 三面（scope-deficient 签名 token / token-endpoint 403 / sink 403 保持） | `crates/aero-audit-connector/tests/state_machine.rs`（扩展） | 同上（fake outbox） |
| 验证器本体回归（零改动） | 既有 `crates/aero-auth/src/oidc/tests.rs` 8 项 | `cargo test -p aero-auth` |
| relay 内部单测随迁（`transient_claim_drift_requeues_without_any_post` 语义不变） | `crates/aero-audit-connector/src/relay.rs` tests | `cargo test -p aero-audit-connector` |
| drill 随迁（config `expected_sub` → `drill-client`、`with_key_provider`） | `crates/aero-audit-connector/src/bin/aero-audit-relay-drill.rs`（+ t11/priority drill） | `scripts/test-integration.sh`（0239 已落库，门已开） |
| 依赖面 | `cargo tree -p aero-audit-connector` | aero-auth workspace crate + jsonwebtoken（已钉 :133）；base64 直接依赖消失；零新增第三方供应链项 |

## 8. Risks / 决策点

- **签名验证强制化是行为变更**：真实 IdP 若发不透明/无 JWKS 的 token → connector fail-closed 不投递（T-11 方向一致）；staging 须验真实 JWKS 可达（AGENTS.md §4.5）。
- **token-endpoint 403 → dead 的误杀风险**：若真实 IdP 以 403 表临时限流而非未配给会误杀——按 acceptance 钉死 dead，401 保持 Transient（secret 轮换可修复），staging 复核。
- **串匹配分类脆弱性**（scope-deficient 与其余 `Invalid` 同变体）：若实现按 seam 固定文案区分，收敛为单一分类点 + 单测钉死；seam 文案变更 → 测试响亮失败。改 aero-auth 加 typed error 违反零改动红线，不取。
- **夹具共享决策**：connector 镜像（复制）E2 夹具模式而非 aero-auth 导出 test-support——守零改动红线；第三消费者出现再议。
- **`expected_sub` 语义收敛**：R2 强制 sub==client_id 后旧部署 env（`AERO_AUDIT_EXPECTED_SUB="aero-im.source"`）须对齐 client_id 值，否则 fail-closed——部署告警面。
- **freshness floor（max token age）不在验收内**：验证器只拒 future-iat（E1 :453），无最大年龄下限；若 v2 契约要求属另一 direction。
- **AC3（allowlist/scope registry）与 F10（可观测/ops-requeue）明确不扩入本 spec**：direction acceptance 未含；设计文档各自标注 [PROPOSED]/review-追加，留待后续 direction。

## 9. Sequencing

1. **本 direction（无未落库依赖）**：`Cargo.toml` 加 aero-auth（+ jsonwebtoken 常规依赖用于 stub，− base64）→ client.rs 换验证器 + 删 `decode_jwt_claims`/`validate_token_claims` + KeyProvider 接线 + R4 分类 → config.rs `AERO_AUDIT_JWKS_URI`（fail-loud）→ stub 签名化 + `token_style`/`token_endpoint_status` → claim_validation.rs 重写（G2）+ state_machine.rs 三面（G1）→ drill config 随迁 → 全量门禁（G3：`cargo check --workspace`、`cargo test -p aero-auth`、`cargo test -p aero-audit-connector --all-targets`、`cargo clippy --workspace --all-targets` 无新警告、`scripts/{truth-check,file-size-check}.sh`、`cargo tree` 零新增供应链项、git-diff 守卫 aero-auth 零改动）。
2. **B5-4 并行**：配给门消费本 direction 的 403/scope-deficient → dead（T-11 enforcement point 已就位）。
3. **staging（AGENTS.md §4.5）**：真实 token endpoint + JWKS 可达 + 真实签发 token 的 sub==client_id 核对——写「待联调」，不写「完成」。

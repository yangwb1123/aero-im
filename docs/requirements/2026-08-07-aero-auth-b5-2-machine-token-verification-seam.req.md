# Requirements Spec — aero-auth 机器令牌（client_credentials）验证 seam 复用：B5-2 connector 的 cc + claim 契约校验收口到已测验证器

- **Module (analysis root)**: `crates/aero-auth/src` — seam 所在；**零 aero-auth 生产代码改动**（seam 已公开，见 E3；本 direction 的落地动作在消费侧 `crates/aero-audit-connector`）
- **Direction**: "Extract a reusable machine-token (client_credentials) verification seam from aero-auth for the B5-2 relay connector crate"（value 9 / risk_reduction 9 / effort 4 / confidence 9）
- **Source analysis**: `docs/auto/analyses/crates-aero-auth-src-650f2e56.json`（direction #1）
- **Campaign**: `aero-im-b5-outbox-relay`（`docs/campaigns/campaign-aero-im-b5.yaml:35-37`："Rust relay connector crate (aero-id pattern: lease/backoff/422→dead-terminal, client_credentials + claim contract, scope audit:event:write)"）；in-repo contract anchor `docs/proposals/audit-contract-batch-aero-im.md`（B5-2 行 :9、T-11 门 :15；v2 契约正文与 "37/37" 清单在仓外，[PROPOSED] :13）
- **Status**: Requirements（下述证据全部经源码 grep 核对）
- **Verification date**: 2026-08-07。行号是核对时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点（AGENTS.md §0）

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-auth/src/oidc.rs` — `validate_client_credentials_token`、`ClientCredentialsTokenConfig{issuer,audience,required_scopes}`、`ClientCredentialsClaims::granted_scopes`、sub==client_id、at+jwt typ、RS256/EdDSA allowlist | ✅ **Verified**。`ClientCredentialsTokenConfig` :364（`issuer`/`audience`/`required_scopes` 三字段）；`ClientCredentialsClaims` :372（`sub`/`client_id`/`iat`/`jti`/`scopes`/`scope`）；`granted_scopes` :387 = `scopes` 数组 ∪ 空格分隔 `scope` 兼容 claim 的 BTreeSet 归一化；`validate_client_credentials_token` :406-473：JOSE typ 必须 `at+jwt`/`application/at+jwt`（:414-417，id-token 混淆即 `Invalid`）→ alg 白名单仅 `RS256`/`EdDSA`（:423-428）→ `KeyProvider` 按 kid 取 key → `jsonwebtoken::decode` 带 `iss`/`aud`/`exp`/`nbf` 校验 + **required_spec_claims 显式要求 `iss,aud,exp,nbf,iat,jti,sub,client_id`**（:440）→ **sub==client_id 且均过 `valid_identity_component`**（:445-450，空/含空白/含控制字符拒绝）→ **future-iat 拒绝**（:453，`iat > now + LEEWAY_SECS`）→ **jti 非空/≤1024/无控制字符**（:455-459）→ **required_scopes 全含于 granted_scopes**（:461-466）。`LEEWAY_SECS=60`（:293）。 |
| E2 | `crates/aero-auth/src/oidc/tests.rs` — client_credentials 测试块（accepts scopes array / space-delimited scope；rejects wrong typ、sub!=client_id、missing required scope、wrong iss/aud、expired、missing iat/jti） | ✅ **Verified**。夹具：`client_credentials_cfg` :163-168（issuer `https://sso.example.com`、audience `aero-im-integrations`、required_scopes `["aero.notify.publish"]`）、`good_client_access_claims` :214-228（`sub==client_id=="erp-production"`、scopes 数组、jti、nbf/iat/exp）、`sign_client_access_token` :230-238（RS256 + `kid` + 显式 typ）、`keypair` :64 / `ed25519_keypair` :83。**8 个测试函数**：`:332` accepts scopes 数组、`:350` 空格分隔 scope（`application/at+jwt`）、`:366` 拒 wrong typ（`JWT`）、`:378` 拒 sub!=client_id、`:392` 拒缺 required scope、`:406` 拒 wrong iss / wrong aud、`:429` 拒 expired + 拒缺 nbf、`:452` 拒 missing iat / future iat / missing jti / empty jti / control jti（5 子例）。全部 `Err(OidcError::Invalid(_))` 断言。 |
| E3 | `crates/aero-auth/src/lib.rs` — re-exports | ✅ **Verified**。:28-29：`pub use oidc::{validate_client_credentials_token, validate_id_token, validate_jwks_uri, ClientCredentialsClaims, ClientCredentialsTokenConfig, JwksKeyProvider, JwksUriError, KeyProvider, OidcClaims, OidcConfig, OidcError, StaticKeyProvider}`——**seam 已完整公开**（验证器 + 配置 + KeyProvider trait + 静态/JWKS 实现），无需移动代码。 |
| E4 | `crates/aero-server/src/integrations.rs:200-240` — 唯一生产消费方 | ✅ **Verified**（路径全仓 grep：`validate_client_credentials_token` 仅出现在 `lib.rs` / `oidc.rs` / `oidc/tests.rs` / `integrations.rs`）。`IntegrationAuthConfig::from_env` :197-231（issuer 回落 `AERO__OIDC__ISSUER`、jwks 回落 `AERO__OIDC__JWKS_URI`、`validate_jwks_uri` 门、`required_scopes: vec![REQUIRED_PUBLISH_SCOPE]` :223 = `"aero.notify.publish"` :51）；`authenticate_machine` :233-246 —— `INTEGRATION_JWKS` OnceLock :63 → `validate_client_credentials_token(token, &config.token, provider)` **:240** → 失败统一 `Unauthorized`。这是「唯一已测验证器 + 唯一消费者」的准确注记。 |
| E5 | `crates/aero-server/src/snaplink_commercial/http.rs:23,230` — v1 relay 只取 cc token、不校验返回 claim 契约 | ✅ **Verified**。`const SCOPE_AUDIT: &str = "audit:event:write";` :23；`request_token` :220-250（`basic_auth` + form `grant_type=client_credentials` :233 + `scope` + `resource`；非 200 即 bail）；token 缓存 :182-232（ttl 折算 `refresh_at`，401 失效 :250-260）。**入站 claim 校验确实缺席**：`validate_token` 只做形状校验（非空 / ≤16KiB / 无控制字符 / `token_type==bearer`）——无 iss/aud/scope/sub/typ/exp 任何检查。`deliver` 的 Audit 臂 :133-180（`SCOPE_AUDIT` :146、期望 `ACCEPTED`）。 |
| E6 | `docs/proposals/audit-contract-batch-aero-im.md` B5-2（§1.2 lease/backoff/422→dead 语义） | ✅ **Verified（as proposed）**。:9 = "B5-2：新 crate（候选 `crates/aero-audit-connector`，无新第三方依赖）—— §1.2 语义（lease > 2×timeout、退避 cap 300s、**422/409/回执错 → dead ≤1 次**、403 → dead = T-11 fail-closed）、cc + claim 校验（iss/aud/scope/sub）、usage relay 原地不动"；:15 门禁 "T-11 与 moderation drill 本仓库可全绿；37/37 需先把契约测试清单钉入 `test-integration.sh`"；:13 明示 §1.2 原文与 "37/37" 清单在仓外。**"37/37 的 claim-contract 子集" 的 in-repo 规范化读法 = E2 的 8 个 client_credentials 测试**（本 spec §5 AC1 钉死）。 |
| E7 | （补充）B5-2 connector 工作树现状——**已存在手写 claim 校验，正是 direction 预警的重复实现** | ✅ **Verified**。`crates/aero-audit-connector/` 在工作树（untracked，前一批次产物，`src/{client,config,fake,outbox,pg,relay,stub}.rs` + `bin/aero-audit-relay-drill.rs` + `tests/{claim_validation,state_machine}.rs`）。`client.rs::validate_token_claims` :197-263 = **base64url 手解 payload**（`decode_jwt_claims` :415，无签名校验、无 typ 检查、无 exp/nbf/iat/jti、无 sub==client_id——sub 只比对配置 `expected_sub` :251）；`stub.rs::make_jwt` :233-245 发 `{"alg":"none","typ":"JWT"}` 假签名 token（模块注释明言 "The connector does not verify signatures"）。**重复面**：E2 的 8 项契约检查中该实现只覆盖 4 项（iss/aud/scope/sub），且无签名验证——direction 的「重复 security-critical 逻辑」在仓库中已实际发生。 |
| E8 | （补充）connector 依赖面与映射现状 | ✅ **Verified**。`aero-audit-connector/Cargo.toml` **无 aero-auth 依赖**（仅 workspace-pinned + 直接声明 `base64="0.22"`）；`config.rs` 有 `expected_iss/expected_aud/expected_scope/expected_sub` :31-37（env `AERO_AUDIT_*`，presence-gated :47-68）；`relay.rs` 状态机：403→`mark_dead` 立即（:177-190，T-11 已实现）、permanent（422/409/回执/payload-guard）→第 1 次 requeue 第 2 次 dead（:191-202）、**claim 校验失败 → Transient requeue 永不 dead**（:204-213，`:383` 测试名 `claim_drift` 即此路径）。`aero-auth` 依赖面（`Cargo.toml`：jsonwebtoken/reqwest/axum 均 workspace）——connector 引用 aero-auth **零新增第三方供应链项**。 |
| E9 | （补充）in-repo 门禁/工具链 | ✅ **Verified**。`scripts/test-integration.sh` 存在（connector drill 已接线 :236-270，0239 缺失时显式 SKIP）；migrations 尾号 0238（B5-1 的 0239 未落库）；root `Cargo.toml` 已有 `aero-audit-connector` workspace member；`jsonwebtoken = "9"` 已在 workspace（:133，被 aero-auth/aero-server 钉住）。 |

### 1.1 对 direction 问题陈述的勘误（evidence-backed）

- **「A new crate re-implementing token validation would duplicate」——已在工作树发生**：B5-2 connector（前一批次产物，E7）不是「将要重复」，而是**已经用 `decode_jwt_claims` 手写实现了一份更弱的 claim 校验**（无 typ/exp/iat/jti/sub==client_id/签名）。本 direction 的落地动作 = 把这份重复实现替换为 aero-auth 已测验证器（R1），而非「防止未来重复」。
- **seam「extract」实为消费侧动作**：E3 证明 aero-auth 已完整公开 seam（`validate_client_credentials_token` + `ClientCredentialsTokenConfig` + `KeyProvider`/`StaticKeyProvider`/`JwksKeyProvider`）。不需要把代码移到更低层 crate——aero-auth 是基础层、connector 是组合层，依赖方向合法且无环；移动反而增加 churn。
- **「无 freshness floor」属实但不在本 direction 验收内**：验证器只拒 future-iat（E1 :453），无最大 token 年龄下限——问题陈述提及，但 acceptance 未要求；本 spec 显式留出范围（§3 / §7），不做。
- **签名验证由 [PROPOSED] 变为强制**：connector 现行姿态「signature is not verified (trusted IdP via client_credentials)」（E7）与复用 E1 验证器互斥——验证器必然验签（RS256/EdDSA + KeyProvider）。这是方向明示的强化（evidence 列出 "RS256/EdDSA allowlist"），JWKS 接线 = `JwksKeyProvider`（E3 已公开），无新依赖。
- **sub 语义变更**：connector 现行 `expected_sub` 是自由配置（stub 里 = `"aero-im.source"`，与 `client_id="drill-client"` 不同，E8）；复用验证器后 **sub==client_id 强制**（E1 :445），`expected_sub` 语义收敛为「client 身份 allowlist 条目」（R5），`source_system` 绑定仍在 payload guard（`client.rs::validate_delivery_payload`）——两层身份解耦，出站 payload 语义不变。

## 2. Verified current state

```
a) aero-auth 已测机器令牌验证器（SEAM，唯一已测实现）   crates/aero-auth/src/oidc.rs
   validate_client_credentials_token :406   RFC 9068 at+jwt：typ 白名单 → alg 白名单(RS256/EdDSA)
     → KeyProvider(kid) → iss/aud/exp/nbf/iat/jti/sub/client_id 全 required → sub==client_id
     → future-iat 拒绝 → jti 约束 → required_scopes ⊆ granted_scopes（数组 ∪ 空格分隔）
   ClientCredentialsTokenConfig :364 / ClientCredentialsClaims :372（granted_scopes :387）
   测试：oidc/tests.rs 8 函数（:332-482），夹具 :163/:214/:230（RS256 真签 + kid + typ）
   re-export：lib.rs :28-29 —— seam 公开完备，消费者零成本接入

b) 唯一生产消费者                                        crates/aero-server/src/integrations.rs
   authenticate_machine :233-246（:240 = validate 调用）：AERO__INTEGRATIONS__* 或回落
   AERO__OIDC__* 配置 + INTEGRATION_JWKS OnceLock(:63) + required_scopes=[aero.notify.publish]

c) v1 内嵌 audit relay（入站零 claim 校验）               crates/aero-server/src/snaplink_commercial/http.rs
   SCOPE_AUDIT="audit:event:write" :23；cc 取 token :220-250（grant_type :233）；
   validate_token 仅形状校验（非空/≤16KiB/无控制字符/bearer）——iss/aud/scope/sub/typ 全缺

d) B5-2 connector（工作树已存在，重复实现已发生）        crates/aero-audit-connector/
   validate_token_claims :197-263 = base64url 手解 + iss/aud/scope/sub 四查（无 typ/exp/
   iat/jti/sub==client_id/签名）；relay.rs：403→dead 立即(T-11)、permanent→requeue×1→dead、
   claim 校验失败→Transient 永不 dead；config expected_iss/aud/scope/sub（AERO_AUDIT_*）；
   stub 发 alg:none 假签名 JWT；无 aero-auth 依赖（E8）
```

**Gaps this direction closes**（all verified）：① connector 的手写 claim 校验（E7）是 E2 已测契约的弱化重复——替换为 `aero-auth` 验证器后补齐 typ/exp/nbf/iat/jti/sub==client_id/签名全矩阵；② v1 relay 对取回的 cc token 零入站校验（E5）——本方向不碰 v1（usage/audit 原地不动，B5-2 范围外），但 connector 是 v2 承载者；③ 无 client allowlist / per-client scope registry（E7/E8）——[PROPOSED] 补；④ 403/scope-deficient 的 dead 语义已部分存在（403→dead），scope-deficient token 当前却走 Transient 无限重试（E8）——按 acceptance 改为 dead ≤1 次（R4）。

## 3. Scope

**In scope（本 direction，B5-2 的机器令牌验证 seam）**：
- `crates/aero-audit-connector` 增加 `aero-auth.workspace = true` 依赖（workspace crate，零新增第三方供应链项；`base64` 直接依赖可随 `decode_jwt_claims` 删除而移除）。
- `AuditClient::deliver` 的 token 校验替换为 `aero_auth::validate_client_credentials_token`（`ClientCredentialsTokenConfig` + `KeyProvider`），首次 POST 前与 401-刷新重试前**每次**校验；删除手写 `decode_jwt_claims`/`validate_token_claims` 的 claim 检查部分。
- 生产 KeyProvider = `JwksKeyProvider`（`AERO_AUDIT_JWKS_URI` 配置，fail-loud）；测试 KeyProvider = `StaticKeyProvider`。
- T-11 dead 语义补全：scope-deficient token / 未配给（token endpoint 403）→ dead ≤1 次尝试（sink 403 → dead 已实现，保持）。
- [PROPOSED] per-client allowlist + scope registry 检查 + connector crate 内单测。
- claim-contract 测试矩阵镜像 `oidc/tests.rs` 8 项（E2 夹具模式：真 RS256 签名 at+jwt，测试内 keypair）。

**Out of scope（并行方向/其他模块——本 direction 不建）**：
- 状态机 / outbox / lease / 退避 / 投递 / 回执 / payload guard / config 其余字段 → 既有 B5-2 spec（`docs/requirements/2026-08-07-aero-ai-b5-2-audit-connector.req.md` R1-R9、A1-A6）已钉，本 spec 只改 claim 校验相关的分类与配置面（R4/R5 与 A5 读法的 delta 显式列出）。
- 0239 DDL + `audit_governance.rs` → **B5-1**；priority → **B5-3**；`aero-cli audit-provision-check` 配给门 → **B5-4**（403→dead 的 fail-closed 行为是本 direction 的；配给门本身不是）。
- **`crates/aero-auth` 生产代码改动：零**（seam 已公开，E3；如需夹具共享见 §7 决策点）。
- v1 内嵌 relay 退役/0236 重定向 → B5-1/campaign 决策；usage 目的地原地不动。
- 仓外 v2 契约三文档与 IdP scope registry 本体、"37/37" 全清单 → [PROPOSED] seam；max-token-age freshness floor（问题陈述提及、acceptance 未含）→ 不做。

## 4. Requirements

### R1 — connector 消费 aero-auth 已测验证器（替换手写重复实现）
`crates/aero-audit-connector` 声明 `aero-auth.workspace = true`；`AuditClient::deliver`（`client.rs:120`）对**每个待发 token**（含 401-刷新后重试的 token）调用 `aero_auth::validate_client_credentials_token`（`KeyProvider` 由配置构造）；校验失败 → 该 token 不出网（无 POST）、失效缓存并进入 R4 分类。**删除** `client.rs::decode_jwt_claims` 与 `validate_token_claims` 中重复的 claim 检查（token 形状检查可保留为前置快速失败，但契约校验一律走验证器）。`base64` 直接依赖随删除移除（`Cargo.toml`）。

### R2 — claim 契约全矩阵（POST 前置，fail-closed）
每次投递前对 token 强制（全部由 E1 验证器保证，逐条与 `oidc/tests.rs` 对齐）：
- JOSE typ ∈ {`at+jwt`, `application/at+jwt`}（拒 id-token/`JWT` 混淆）；
- alg ∈ {RS256, EdDSA} + **签名经 KeyProvider 验证**（kid 匹配，unknown key 拒绝）；
- `iss` == 配置 issuer、`aud` == 配置 audience（精确）；
- `exp`/`nbf` 生效（LEEWAY 60s）；`iat` 必在且非未来（future-iat 拒绝）；
- `jti` 非空 / ≤1024 / 无控制字符；
- `sub` == `client_id`（机器身份，`valid_identity_component`）；
- 授予 scope（`granted_scopes` = `scopes` 数组 ∪ 空格分隔 `scope` 的归一化并集）⊇ `required_scopes`（含 `audit:event:write`）。
- opaque 非 JWT / 畸形 token → `OidcError::MalformedToken` → 同样 fail-closed（无 POST）。
配置映射：`RelayConfig.expected_iss/expected_aud` → `ClientCredentialsTokenConfig.issuer/audience`；`expected_scope`（默认 `audit:event:write`，`client.rs:32 SCOPE_AUDIT`）→ `required_scopes`。

### R3 — KeyProvider 接线
生产：`JwksKeyProvider::new(AERO_AUDIT_JWKS_URI)`（镜像 `integrations.rs:63,239` 的 OnceLock 模式），URI 过 `validate_jwks_uri` 策略（HTTPS 或 loopback-HTTP，无 userinfo/query/fragment——E1 同款策略），配置缺失/非法 → boot fail-loud（进 `config.rs` presence-gated `AERO_AUDIT_*` 家族）。测试：`StaticKeyProvider::single(dec)`（`oidc/tests.rs` 同款）。**签名验证由 connector 现行 "[PROPOSED]"（E7 注释）变为强制**——这是复用 E1 的必然结果，也是 direction 明示（RS256/EdDSA allowlist 属 seam 一部分）。

### R4 — T-11 fail-closed：unprovisioned / scope-deficient → dead ≤1 次尝试（delta vs 既有 A5 读法）
| 面 | 检测点 | 分类（本 direction 钉死） |
|---|---|---|
| scope-deficient token（granted 缺 `audit:event:write`） | 验证器 `Invalid("token lacks a required application scope")` | **`mark_dead` attempt 1**（无 requeue、无 POST）——acception "scope-deficient … dead-terminal (≤1 attempt)" |
| 未配给 client（token endpoint 返回 403） | `request_token` 非 200 | **`mark_dead` attempt 1**（现为 Transient 无限重试，E8——「unprovisioned … retry loop」缺口） |
| sink 拒绝身份（投递 403） | `deliver` HTTP 403 | `mark_dead` attempt 1（**已实现** `forbidden_dead_on_first_attempt`，保持绿） |
| 其余 claim 违规（wrong iss/aud、sub!=client_id、wrong typ、expired、future/missing iat/jti、unknown key、opaque） | 验证器各 `OidcError` 变体 | **Transient requeue + 下次 claim 轮换新 token**（既有 A5 in-repo 读法保持——IdP 配置漂移由 B5-4 配给修复；若 v2 契约要求全类 dead，只改 `client.rs` 分类 + A2 参数化测试为 enforcement point，本 direction 不擅自扩大） |
区分依据 = acceptance 原文 "unprovisioned or scope-deficient credential"；dead 行的落库/排除语义沿用既有 `mark_dead`（fence 栅栏、claim/lag 查询排除）。

### R5 — [PROPOSED] per-client allowlist + scope registry 检查
- `RelayConfig` 增 `AERO_AUDIT_CLIENT_ALLOWLIST`（逗号分隔 client_id；`expected_sub` 语义收敛为单客户端 allowlist 条目，兼容旧配置）；配置时启用。
- 检查（在验证器之后叠加）：`claims.sub`（== `client_id`，R2 已保证）必须 ∈ allowlist；per-client scope registry：该 client 注册的允许 scope 集（最小形式 = 注册条目必须含 `audit:event:write` 且 granted 不得含注册集之外的 scope）——registry 本体在 IdP 仓（仓外 [PROPOSED]），本仓交付检查逻辑 + 配置注入。
- 空 allowlist / 未配置 = 不额外限制（`ip_allowlist` 先例：空名单放行；AGENTS.md §3 企业接入）。配置冲突（allowlist 存在但不含本 client_id）→ fail-loud boot 错误，绝不静默放行。

### R6 — claim-contract 测试镜像 oidc/tests.rs 夹具（37/37 子集）
connector 的 claim 校验测试以 E2 夹具模式驱动：测试内 `keypair()`（RS256）签真 JWT、`ClientAccessTokenClaims` 同构 claims 构造、显式 `typ` 头部——覆盖 E2 全部 8 项契约（含 5 个 iat/jti 子例）。`stub.rs::make_jwt`（alg:none 假签名）**必须改**：stub token 端点改发 RS256 真签 at+jwt（测试 keypair），并暴露匹配的 `StaticKeyProvider` 供 `AuditClient` 配置；opaque/alg:none token 保留为**拒绝路径**测试用例（R2 的 fail-closed 面）。测试所需签名用 `jsonwebtoken`（workspace :133 已钉，dev-dependency 零新增供应链项）。

## 5. Acceptance checks（direction 原样保留，逐条 testable）

### AC1 — Connector rejects a fetched token lacking audit:event:write (or with wrong iss/aud/sub!=client_id/typ) before any outbound send — reuses oidc/tests.rs fixtures as the 37/37 claim-contract subset
- **测试**（`crates/aero-audit-connector/tests/claim_validation.rs` 扩展，夹具镜像 E2）：8 项契约矩阵——① accepts scopes 数组；② accepts 空格分隔 scope；③ 拒 wrong typ（`JWT` 头）；④ 拒 sub!=client_id；⑤ 拒缺 `audit:event:write`；⑥ 拒 wrong iss；⑦ 拒 wrong aud；⑧ 拒 expired / 缺 nbf / 缺 iat / future iat / 缺 jti / 空 jti（参数化子例）。**拒绝场景全部断言 `stub.posts() == 0`**（字面测量：任何 POST 之前被拒）+ `Err(DeliveryError::…)`；接受场景 `posts() >= 1`。签名用测试内 RS256 keypair 真签（`oidc/tests.rs` 同款），stub 换发签名 token。
- **"37/37 claim-contract subset" 的 in-repo 钉死读法**：全量 37/37 清单在仓外（E6 :13 [PROPOSED]）；本仓可验证的 claim-contract 子集 = E2 的 8 个 client_credentials 测试函数（`oidc/tests.rs:332-482`）——AC1 矩阵与之一一对应，作为 connector 侧同矩阵测试的镜像基线。

### AC2（T-11）— an unprovisioned or scope-deficient credential drives the connector to dead-terminal (≤1 attempt) rather than retry loop
- **测试**（`tests/state_machine.rs` 扩展，fake outbox + stub sink）：三个驱动面参数化——(a) stub token 端点发缺 `audit:event:write` 的签名 token → `deliver_claim` → `FakeStatus::Dead`、`attempts == 1`、`stub.posts() == 0`、后续 `claim_due` 为空（dead 行永不被选中）；(b) token 端点返回 403（未配给）→ `Dead`、`attempts == 1`、`posts() == 0`；(c) sink 返回 403 → `Dead`、`attempts == 1`（既有 `forbidden_dead_on_first_attempt` 保持绿）。三面均断言**无 retry loop**（无 requeue 后的二次 claim）。
- **配给缺席面**：`config.rs::from_env` presence-gated（无 `AERO_AUDIT_TOKEN_ENDPOINT` → `Ok(None)`，relay 不启动 = fail-closed 不投递，既有 A1 面保持）。

### AC3（[PROPOSED]）— per-client allowlist + scope registry check unit tests in the connector crate
- **测试**（connector crate 内单测）：① allowlist 未含 token 的 client_id → 拒绝、`posts() == 0`；② granted scope 含注册集之外的 scope（如 `admin:*` 未注册）→ 拒绝、`posts() == 0`；③ allowlist 未配置 → 不额外限制（放行有效 token）；④ allowlist 配置但不含本 client_id → config fail-loud（boot Err）；⑤ 注册 client + `audit:event:write` → 通过并投递（202 回执路径）。registry/allowlist 输入经 `RelayConfig` 注入（仓外 registry 本体不依赖）。

## 6. Test placement

| Test | Location | Harness |
|---|---|---|
| AC1 契约矩阵（8 项 + iat/jti 子例，真签名 JWT，`posts()==0` 断言） | `crates/aero-audit-connector/tests/claim_validation.rs`（改造/扩展） | `cargo test -p aero-audit-connector`，无 DB（stub sink） |
| AC2 T-11 dead 三面（scope-deficient token / token-endpoint 403 / sink 403 → `attempts==1` 终态） | `crates/aero-audit-connector/tests/state_machine.rs`（扩展） | 同上（fake outbox） |
| AC3 allowlist + scope registry（5 用例） | `crates/aero-audit-connector/tests/claim_validation.rs` 或 `src/config.rs` tests | 同上 |
| 验证器本体回归（seam 不动） | 既有 `crates/aero-auth/src/oidc/tests.rs` 8 项 | `cargo test -p aero-auth`（已全绿，零改动） |
| 既有 B5-2 全套回归（A1-A6 不改面） | `crates/aero-audit-connector` 既有 24 项 + `--ignored` PG 并发测试 + drill | `cargo test -p aero-audit-connector` + `scripts/test-integration.sh` |
| 依赖面 | `cargo tree -p aero-audit-connector` | aero-auth workspace crate + `jsonwebtoken`（dev，已钉 :133）——零新增第三方供应链项 |

## 7. Risks / [PROPOSED] / 决策点

- **"37/37" 与 §1.2 契约原文在仓外**（E6 :13）：AC1 钉 in-repo 读法 = E2 的 8 项子集；契约若给出更大矩阵，AC1 是扩展点（同夹具追加用例），不是重写。
- **签名验证强化是行为变更**：connector 现行「不验签」姿态（E7）被替换；真实 IdP 若签发不透明/无 JWKS 的 token，连接器将 fail-closed（不投递）。这与 T-11 fail-closed 方向一致，但需在 staging 用真实 token endpoint 验证 JWKS 可达（AGENTS.md §4.5 真实凭据面）。
- **AC2(b) token-endpoint 403 → dead 是新增分类**（现 Transient 无限重试，E8）：若真实 IdP 以 403 表示「临时限流」而非「未配给」，此分类会误杀——按 acceptance 钉死为 dead，staging 复核；401（secret 轮换可修复）保持 Transient。
- **夹具共享决策点**：connector 测试镜像（复制）E2 夹具模式 vs aero-auth 暴露 `#[doc(hidden)]` test-support 模块——前者零 aero-auth 公共面变更（本 spec 采用，§3「aero-auth 零改动」红线）；若后续出现第三个消费者再议共享。
- **freshness floor（max token age）不在验收内**：问题陈述提及但 acceptance 未含，本 direction 不做；若 v2 契约要求，`ClientCredentialsTokenConfig` 增字段 + E1 验证器一处检查即可，属另一 direction。
- **配置面 delta**：`AERO_AUDIT_JWKS_URI`（新，必填当 connector 启用）与 `AERO_AUDIT_CLIENT_ALLOWLIST`（新，[PROPOSED]）进 `config.rs` presence-gated 家族；`expected_sub` 语义收敛（§1.1）——既有部署 env 需对齐，spec 变更即告警面。

## 8. Sequencing

1. **本 direction（无 B5-1 依赖）**：connector 加 aero-auth 依赖 → `deliver` 换验证器 + 删 `decode_jwt_claims` → `AERO_AUDIT_JWKS_URI` 配置 → stub 改发签名 token + `StaticKeyProvider` → AC1/AC2/AC3 测试落地 → 全量门禁（`cargo check --workspace`、`cargo test -p aero-audit-connector`（既有 24 项 + 新增）、`cargo clippy --workspace --all-targets` 无新警告、`scripts/{truth-check,file-size-check}.sh`、`cargo tree` 零新增供应链项）。
2. **B5-1 落库后**：`scripts/test-integration.sh` drill 门解开（0239 + enqueue 路径），A4 端到端照既有 spec 执行。
3. **B5-3 / B5-4 并行**：不改 connector 状态机；B5-4 配给门消费本 direction 的 403/scope-deficient → dead（T-11）。
4. **A6 standing gate**：usage relay 与 v1 audit 路径零改动（`git diff` 守卫：`crates/aero-server/src/snaplink_commercial/` 不在本 direction 变更集）。

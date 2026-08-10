# Requirements Spec — 补全 claim 契约：exp/nbf 时间 claims 校验 + [PROPOSED] JWKS 签名验证（fail-closed 语义）

- **Module (analysis root)**: `crates/aero-audit-connector`（`src/{client,config,relay,stub}.rs` + `tests/claim_validation.rs` + `src/bin/*-drill.rs`；`scripts/test-integration.sh` / `scripts/b5-pin.sh` 仅回归消费，不改契约）
- **Direction**: "Complete the claim contract: validate exp/nbf time claims now, and resolve the [PROPOSED] JWKS signature verification with fail-closed semantics"（value 8 / risk_reduction 9 / effort 6 / confidence 9；**proposed**）
- **Source analysis**: `docs/auto/analyses/crates-aero-audit-connector-src-7edb5949.json`（direction #1，`proposed: true`；同文件 direction #0 = L1 聚合、#2 = relay liveness，均不在本 spec 范围）
- **Prior art（同 seam 的兄弟方向）**: `docs/requirements/2026-08-08-aero-audit-connector-b5-2-jwks-signature-verification.req.md` + `docs/design/2026-08-08-aero-audit-connector-b5-2-jwks-signature-verification.design.md`（`crates-aero-ai-src-1bbf99ce.json` #1，纯 JWKS 方向，签名拒绝 = permanent ≤1-retry→dead、`AERO_AUDIT_JWKS_URI` 必填 fail-loud）；`docs/requirements/2026-08-08-aero-auth-b5-2-machine-token-verification-seam.req.md`（aero-auth 侧 8/8 契约：typ/exp/iat/jti/sub==client_id —— **不同 seam，范围外**）。本 direction = JWKS 兄弟方向的**超集**（+exp/nbf 半场），acceptance 以本 direction 为准
- **Status**: Requirements（下述证据全部经源码 grep 核对）
- **Verification date**: 2026-08-08。行号是核对时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点（AGENTS.md §0）

## 0. 修订记录（reconciliation，2026-08-08）

> 合并实施评审（`docs/design/2026-08-08-aero-audit-connector-b5-2-claim-contract-exp-nbf-jwks.design.md` §3/§4）发现本 spec 与实现裁决之间的冲突/缺口，逐条裁决如下并回写正文（下文 § 引用即修订后状态）。

- **AM-1 — amend-spec 裁决（aero-auth 红线放宽，唯一一处）**：D5 双平面（fetch 失败 Transient / unknown-kid Permanent）经现 `KeyProvider` trait（`aero-auth/src/oidc.rs:243`，`#[axum::async_trait] async fn decoding_key(&self, kid: Option<&str>, algorithm) -> Option<DecodingKey>`）的 `Option` 塌缩**不可表达**——`JwksKeyProvider::decoding_key` 对 `cached_set` fetch/refresh 失败 `warn!` 后同样返回 `None`，与「key 真不在集合」在 trait 层不可区分；而本 spec 自身（AC3(a)+AC3(b) 双钉 + D4 继承兄弟 permanent 分类）同时要求两个失败面可表达。**裁决：amend spec**——§7 红线放宽为**唯一 additive 项**：`decoding_key_fallible`（默认实现委托既有 `decoding_key` = 零行为变化，`StaticKeyProvider` 及外部 impl 均不动；`JwksKeyProvider` 覆写：fetch/刷新失败 → `Err`（复用 `safe_jwks_request_error` 文案）、key 不在集 → `Ok(None)`；约 15 行 + 3 单测）。amend-design 备选全部否决：① connector 自持 JWKS 客户端（复制 ~100 行 + TTL/限速策略再推导，双份维护）；② 双平面塌缩（违反 AC3(a) 或 AC3(b)——JWKS 网络故障会把全表行 ≤1-retry 后 dead = 数据丢失面，即设计 F9）；③ connector 预检 fetch（与 provider 缓存竞态，且仍无法区分 refresh 失败 vs unknown-kid）。兄弟 spec（`…jwks-signature-verification.req.md` §1/§7）同款红线已同步放宽并交叉引用本项。
- **AM-2 — D6 顺序裁决采纳**：校验顺序 = **签名先、claims 后**（同一循环迭代内，D9 单调用点）。JWKS-on 面结构性失败（opaque/畸形/alg:none/非 RS256）→ `Permanent(SignatureRejected)`——`opaque_non_jwt_token_is_rejected_before_any_post` 夹具在 JWKS-on 面从今日 Transient **有意改判**（结构性坏 token 重试不自愈，attempt-2 轮换新 token 是唯一解药）；JWKS-off 面分类逐字不变。§6 全部 AC 复核仍字面可满足（§6 末复核表）。
- **AM-3 — env 统一规则落文**：规范名 = `AERO_AUDIT_JWKS_URL`（**"URL wins"**，本 spec D2 既决）；合并实施期 `…_URL` 与兄弟 `…_URI` **同设 → boot `bail!`**（歧义即错误，与 R2 畸形 fail-loud 同姿态）——回写 §3 D2。
- **AM-4 — 部署约束确认**：exp/nbf 半场**无条件生效**（D1 既决「无配置门」）⇒ 无 env 回滚开关，**回滚 = 回退 binary（binary-rollback-only）**。spec 从未承诺该半场的开关（全篇无「回滚/开关」措辞）；失败姿态安全（行 pend + Transient requeue，不丢不死，与 T-11 同族；leeway 60s 吸收时钟抖动；validated-when-present 使无时间 claims 的 token 与既有 13 用例不受影响）——回写 §5 R1。
- **AM-5 — F2 缓解的限速边界裁决（D10，受控放宽）**：签名面 unknown-kid 重试在 10s 限速窗内的**同 kid 单次绕过刷新**（设计 §3 D10）构成 `JwksKeyProvider` **限速行为的受控变更**——有效 unknown-kid 刷新上界从 1 次/10s 放宽至 **2 次/10s**（per provider，风暴属性保持：绕过照常占用刷新窗、新 kid 无绕过、last-missed 单槽）。AM-1 边界原文「不改 `JwksKeyProvider` TTL/限速行为」按字面不含此变更，故**显式 amend 本项**：放宽范围严格限定——① 仅 additive `decoding_key_fallible` 覆写路径（既有 `decoding_key`/`cached_set`/`reserve_unknown_kid_refresh`/`UNKNOWN_KID_REFRESH_MIN_INTERVAL`/TTL 300s 逐字节不动，aero-auth 既有调用方零观察差异）；② 每 (kid, 窗) 至多一次绕过；③ 绕过照常占用刷新窗（总量 ≤2 次/10s）；④ 新 kid 扫描仍被限速。fail-closed 语义、relay 零改动、T-11 姿态均不变。实现/测试计数随迁（aero-auth 约 25–30 行 + 4–5 单测，原 AM-1 的 ~15 行 + 3 单测估计被 D10 超越——回写设计 §3 D10 / §4 API-4 / §5 F7 / §7 钉测试）。

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `client.rs:150-200` `validate_token_claims`（iss/aud/scope/sub 四查） | ✅ **Verified**。`client.rs:199-263`：形状检查 :200-206 + iss :209-217 + aud :218-231 + scope :232-242 + sub :243-253——**无 exp/nbf、无签名验证**；模块注释 :196-198 明言 "signature is *not* verified … JWKS verification is **[PROPOSED]**"（:198 精确命中）。`ClaimRejection{reason}` 纯函数 |
| E2 | `client.rs:437-451` `decode_jwt_claims`（无签名验证） | ✅ **Verified**（实际 :416-435）。base64url 手解 payload 段，仅检查 3 段结构（:424-431），**签名段内容从不校验**；注释明言 "without signature verification" |
| E3 | `tests/claim_validation.rs`（wrong_issuer/missing_audience/missing_audit_scope/wrong_subject，无 exp/nbf/alg 用例） | ✅ **Verified**。实跑 = **12 async + 1 sync**（`jwt_claims_decode_roundtrip`）。拒绝例全部经 `deliver_with_claims` → `stub.posts() == 0` 字面断言（A2 计量方式）；无任何 exp/nbf/alg 时间或签名用例 |
| E4 | `stub.rs::make_jwt`（`alg:none`） | ✅ **Verified**（:242-248）。header = `{"alg":"none","typ":"JWT"}` + 假签名段 `ZmFrZS1zaWduYXR1cmU`（= "fake-signature" 的 base64）。**当前 claim 校验接受它**——direction 主威胁的 fixture 本体 |
| E5 | `config.rs::RelayConfig`（无 JWKS 字段） | ✅ **Verified**。结构体 :37-55 无 jwks 字段；`from_env` :57-119 解析 `AERO_AUDIT_*` 家族（presence-gated：token endpoint 缺而其他 `AERO_AUDIT_*` 在 → bail；`identity_env`/`service_url` 已实现 HTTPS-or-loopback、无 userinfo/query/fragment、长度有界策略——JWKS URL 复用同款） |
| E6 | `transient_claim_drift_requeues_without_any_post`（接受面引用的「same shape」） | ✅ **Verified**（`relay.rs` 模块内测试 :403-424）。claim 漂移 → `DeliveryError::Transient` → `assert_transient_requeue`：`posts()==0`、`FakeStatus::Ready`（requeue **永不 dead**）、attempts==1、`available_at == t0 + backoff(1)`（fake 时钟钉死）、`claim_token`/lease 清空、`last_error` 含原因片段、重 claim 后**轮换新 token**（:443-449） |
| E7 | 37/37 test-integration.sh pins | ✅ **Verified**。`scripts/b5-pin.sh` `B5_CONTRACT_TEST_LIST` = **15 个仓内可执行槽 + 22 个 `[PROPOSED]` 槽 = 37**；仓内槽含 `audit_governance::`、`moderation_finalize_outbox_parity`、`a3-relay-drill`、`t11-fail-closed`、`moderation-priority-drill`、`notification-fanout`、`relay-mock-probe`、`audit-provision-check`。`assert_b5_contract_pin` guard = 恰 37 槽/无重复/格式/≥1 实执行/verdict 行（`test-b5-pin-guard.sh` 自测） |
| E8 | T-11 drill（token endpoint 关闭 ⇒ rows pend、attempts grow、never falsely dead） | ✅ **Verified**。`src/bin/aero-audit-t11-drill.rs`：token endpoint = 绑定后即丢的 loopback 端口（连接拒绝，无 wall-clock 窗口）→ token 永不可得、events 永不达；断言 `COUNT(status=0)==N`、`COUNT(status IN (1,2,3))==0`、`SUM(attempts)` 第 1 轮 N → 第 2 轮 2N（1.2s sleep > backoff(1)，证明 retry-forever）、`last_error LIKE '%audit connector HTTP transport failed%'`。`test-integration.sh` :343+ 以 0239 文件存在为门跑之 |
| E9 | 安全级 payload（0239/0241） | ✅ **Verified**。`migrations/0239_audit_governance_outbox.sql:126-127` 与 `migrations/0241_governance_reconcile.sql:113-114` 均含 `'data_classification','confidential'` + `'retention_class','security'`——direction "security-class feed" 属实 |
| E10 | 支撑面（本 spec 补充复核） | ✅ **Verified**。workspace root `Cargo.toml:133` 已钉 `jsonwebtoken = "9"`、`:137` `rsa 0.9 (features pem)`（aero-auth 在用；aero-server 已用 rsa 做 SAML 测试 keygen）——**零新增供应链条目**可达成。aero-auth seam 公开：`KeyProvider` trait（`oidc.rs:243`）、`StaticKeyProvider::{single,with_keyed}`（:252/:267）、`JwksKeyProvider::new`（:529，TTL 缓存 300s + unknown-kid 限速刷新 10s）、`OidcError`（:220）、`validate_jwks_uri`（:54）。**connector 现无 aero-auth 依赖**（加入无环：aero-auth 是基础层） |

### 1.1 行号漂移勘误（direction 原始引用 vs 实际）

`validate_token_claims` :150-200 → **:199-263**；`decode_jwt_claims` :437-451 → **:416-435**；`[PROPOSED]` :198 → **:198 精确命中**（模块注释）。全部符号命中，无事实性出入。

## 2. Verified current state

```
claim 契约现状（三条独立事实）：
a) 时间 claims（E1）   exp/nbf 完全缺席：iss/aud/scope/sub 四查通过即放行——过期/未生效
                      token 直接进 POST；403→dead 是唯一 backstop（T-11）
b) 签名（E2/E4）       decode_jwt_claims 只解 payload；stub 明造 alg:none 假签名 token 且被接受；
                      client.rs:198 自标注 "[PROPOSED]"；RelayConfig 无任何 JWKS 配置面（E5）
c) 已钉死的既有面（E6/E7/E8）  claim 漂移 → Transient requeue 永不 dead（posts()==0 形状）；
                      37/37 pin guard；T-11 drill（closed token endpoint ⇒ pend + attempts grow）

可复用车辆（本 direction 只接线）：
  deliver_with_claims（claim_validation.rs）  token_claims 任意 JSON → deliver 结果 + posts() 计数
  assert_transient_requeue（relay.rs 模块内）  fake 时钟钉死的 requeue 全量断言（含 token 轮换）
  SinkBehavior.token_claims（stub.rs）         任意 claims 直灌，exp/nbf 只需加字段
  JwksKeyProvider / StaticKeyProvider（E10）  已测密钥源 seam（TTL + unknown-kid 刷新）
```

**Gaps this direction closes**（all verified）：① exp/nbf 缺席——四查外补时间窗校验（纯函数，时钟注入）；② 签名验证 [PROPOSED]——JWKS 启用时强制（alg:none/伪造/篡改在 POST 前失败）；③ 无配置面——`AERO_AUDIT_JWKS_URL`；④ 获取失败 fail-closed——JWKS 不可达 = 不 POST、transient requeue（E6 同形状）；⑤ kid 轮换——轮换后新 key 可用、旧 key 拒绝。

## 3. Decision points

| # | 决策 | 依据 / 证据 |
|---|---|---|
| D1 | **exp/nbf 校验无条件启用**（无配置门），但**只在 claim 存在时校验**（缺失 = 放行） | direction "The exp/nbf half is cheap and pure"——无需密钥材料，不与配置面绑定。存在性不强制的理由：direction acceptance 只要求「past exp 拒 / future nbf 拒」，且既有 13 个 claim 测试的 token 全无 exp/nbf——强制存在会打爆整个既有面（属兄弟 aero-auth 8/8 契约的 typ/exp/iat 存在性强制，范围外）。**时钟 seam**：`validate_token_claims_at(token, now: OffsetDateTime)` 纯函数 + 既有 `validate_token_claims` 委托（`now_utc()`），测试钉 now（aero-auth `totp::verify(secret,code,unix_secs)` 同款）——**不违反 relay "repo 唯一时钟"纪律**（那是 repo 侧；client 校验是 app 侧纯函数，钉 now 反而消除 wall-clock 竞态）。leeway = 命名常量（建议 `TIME_CLAIM_LEEWAY_SECS: i64 = 60`，防边界抖动；测试用 ≥2×leeway 的明确余量） |
| D2 | **JWKS 配置面 = opt-in**：`AERO_AUDIT_JWKS_URL` 设置 ⇒ 签名验证强制；未设置 ⇒ 现状保留（exp/nbf 半场仍无条件） | direction acceptance 原文 "once JWKS verification is enabled (new AERO_AUDIT_JWKS_URL config, fetch failure = fail-closed, no POST…)"——契约最小面 = 启用即强制。**命名以本 direction 为准**（`…_URL`）；兄弟 spec 用 `AERO_AUDIT_JWKS_URI`——两 spec 合并实施时须统一为一个 env 名（本 spec 契约名 = `AERO_AUDIT_JWKS_URL`）。兄弟 spec R2 主张 boot fail-loud 必填；opt-in 使既有 28 个无 PG 测试与 3 个 drill 无需全量随迁即可保持绿（D3 的 fetch-failure 形状依赖 opt-in 测试面），且满足本 direction 全部 acceptance——**推荐合并实施时取 fail-loud 终态**（T-11 姿态一致），但不构成本 direction 的硬要求。**合并实施规则（AM-3）**：规范名 = `AERO_AUDIT_JWKS_URL`（"URL wins"）；`…_URL` 与兄弟 `…_URI` **同设 → boot `bail!`**（歧义即错误，fail-loud，与畸形 URL 同姿态）——两 spec 只认一个 env 名，**无运行时优先级语义** |
| D3 | **JWKS 获取/刷新失败 = Transient**（requeue 永不 dead、`posts()==0`），**与签名拒绝分属两个失败面** | direction acceptance 明示 "fetch failure = fail-closed, no POST — **same shape as transient_claim_drift_requeues_without_any_post**"（E6 全套断言：Ready/attempts 1/backoff(1)/last_error 片段/重 claim 轮换 token）。机制暂不可用 ≠ token 无效——刷新后重试可成功，与 claim 漂移（IdP 配置漂移，B5-4 修复）同类 |
| D4 | **签名拒绝（alg:none / 非白名单 alg / 坏签名 / unknown kid 刷新后仍无）分类 = 采纳兄弟 spec 决定：permanent（≤1 次重试 → dead）** | 本 direction acceptance 只钉「rejected + 0 POST」，未钉终态分类；兄弟 spec R3/D1 已裁定 permanent（身份面故障，重试不自愈，与 403→dead 同族但保留 1 次重试容忍 JWKS 轮换传播抖动）。**两 spec 不得矛盾**——本 spec 继承该分类，relay 既有 permanent 臂（`deliver_claim` :222，attempt 1 requeue / attempt ≥ 2 `mark_dead`）泛化处理，relay 零改动 |
| D5 | **kid 轮换 = served-JWKS 语义**（非静态 key 列表）：unknown kid 触发 provider 刷新（`JwksKeyProvider` 10s 限速 + 300s TTL），轮换后新 key 签发的 token 被接受、退役 key 被拒 | direction acceptance "kid-rotation test (new key served after rotation)"——"served" = 从端点服务。stub 增 `/jwks` 端点（`SinkBehavior` 可换 key 集）。实现注意：10s 限速窗内同 provider 不重刷——测试分阶段构造新 provider（每阶段 fresh），或在 provider 暴露测试 seam；acceptance 只钉属性。**双平面可表达性（AM-1）**：现 `KeyProvider` 的 `Option` 塌缩使 fetch 失败与 unknown-kid 不可区分——§7 红线放宽为唯一 additive 项 `decoding_key_fallible`（`Err` → Transient / `Ok(None)` → Permanent）；备选（connector 复制 JWKS 客户端 / 双平面塌缩）被拒 |
| D6 | 验证时机 = **每个将用于 POST 的 token**（缓存命中 / 新获取 / 401-refresh 后重验） | `deliver()` :120-190 现于每次 POST 前置 `validate_token_claims`（:128），401-refresh 后循环重验（:131-137）——exp/nbf 与签名检查接入**同一调用点**即天然覆盖三路径，零新调用面 |

## 4. Threat-model boundary（诚实标注能力边界，非 scope 缺口）

关闭：伪造/形状匹配 token（无私钥的攻击者——`alg:none` 是现成入口）；被篡改的合法 token（改 claims 即坏签名）；过期/未生效 token 的重放窗口（exp/nbf）；被攻破/误配 token endpoint 用非信任密钥签发的 token；JWKS 不可达时的静默放行（fail-closed 不 POST）。

**不覆盖**：真实 IdP 私钥泄露（可签真 token）；合法未过期 token 的泄露重放（无 jti 状态检查）；`typ/iat/jti` 存在性强制与 `sub==client_id` 收敛（兄弟 aero-auth 8/8 契约，范围外）；token endpoint 403→dead、scope-deficient→dead（兄弟方向分类面）。

## 5. Requirements

### R1 — exp/nbf 时间 claims 校验（无条件、纯函数、时钟注入）

`validate_token_claims`（`client.rs:199-263`）在四查之外补时间窗校验：
- 新纯函数 `validate_token_claims_at(&self, token, now: time::OffsetDateTime) -> Result<(), ClaimRejection>`；既有 `validate_token_claims` 委托之（`now_utc()`）——**deliver() 调用点不变**（D6）。
- 规则（RFC 7519 NumericDate = JSON number，秒）：`exp` 存在且 `exp <= now_secs + TIME_CLAIM_LEEWAY_SECS` → 拒（reason 含稳定片段 `"token has expired"`）；`nbf` 存在且 `nbf > now_secs + TIME_CLAIM_LEEWAY_SECS` → 拒（`"token is not yet valid"`）；exp/nbf 存在但非 number → 拒（malformed，fail-closed）；**缺失 → 放行**（D1）。
- `TIME_CLAIM_LEEWAY_SECS: i64 = 60` 命名常量（config 不暴露；测试用 ±120s 余量）。
- 失败面与既有 claims 漂移一致：`warn!(reason=…)` + `DeliveryError::Transient`（requeue 永不 dead）——**不改分类面**（`transient_claim_drift_requeues_without_any_post` 形状直接复用）。

- **Rationale**: direction "cheap and pure" 半场；过期/未生效 token 现可直通 POST（E1/E9），403 是唯一 backstop。
- **部署（AM-4）**：无条件 ⇒ **无 env 开关**，回滚 = 回退 binary；binary 布上即行为变更（过期/未生效 token 不再可投递），最坏形态 = 行 pend + Transient requeue（不丢不死，T-11 同族）；leeway 60s 吸收时钟抖动，validated-when-present 保既有面不动。
- **验证**: AC1、AC2。

### R2 — 配置面 `AERO_AUDIT_JWKS_URL`（启用 ⇒ 强制；畸形 ⇒ boot fail-loud）

`RelayConfig` 增 `pub jwks_uri: Option<Url>`（tests/drill 直接构造仍兼容）；`from_env`（`config.rs:57-119`）解析 `AERO_AUDIT_JWKS_URL`：设置时过 `service_url` 同款策略（HTTPS 或 loopback-HTTP、无 userinfo/query/fragment、≤2048 字节；直接复用 `aero_auth::validate_jwks_uri` 或镜像策略于 config.rs），畸形 → `bail!`（**relay 不启动、不 claim、不投递**——presence-gated 家族既有行为）。未设置 → `None`（签名校验关闭，D2）。设置 ⇒ 生产路径 `AuditClient` 由 URI 构造密钥源（`JwksKeyProvider`），测试/drill 经注入 seam（R6）。

- **Rationale**: 签名验证必须有密钥来源；畸形 URL 在 boot 拒绝（fail-loud），与 `AERO_AUDIT_*` 家族及 T-11 姿态一致。
- **验证**: AC3、AC4；config 单测三例（缺失/畸形/合法）。

### R3 — 签名验证强制化（JWKS 启用时；关闭 client.rs [PROPOSED] seam）

`AuditClient` 对**每一个将用于 delivery POST 的 bearer token**（缓存命中、新获取、401-refresh 后重验——D6 单调用点）执行签名验证：签名必须以配置的 trusted-JWKS 密钥集中某密钥验证通过，alg 白名单 = RS256（`alg:none`/HS*/其他 → 拒），否则该 token 一律不得用于 POST（fail-closed，0 POST）。`client.rs:198` 的 "[PROPOSED]" 注释随实现删除。

- **Rationale**: 现信任姿态 = "cc grant 换来的所以可信"（E1），伪造/泄漏 shape-match token 仅靠结构校验不可防（E4 是现成 exploit fixture）。
- **验证**: AC3(a)、AC4。

### R4 — JWKS 获取失败 fail-closed：Transient requeue，永不 dead，0 POST

JWKS 端点获取/刷新失败（网络/HTTP 错/文档畸形）→ 不 POST，`DeliveryError::Transient`（reason 含稳定片段，如 `"JWKS"`），relay 既有 Transient 臂 requeue（backoff、永不 dead）——**与 `transient_claim_drift_requeues_without_any_post` 同形状**（D3）。

- **Rationale**: direction acceptance 明示；机制暂不可用 ≠ token 无效，刷新后重试可成功。
- **验证**: AC3(b)。

### R5 — kid 轮换（served-JWKS 语义）

轮换后（端点服务新 key 集、新 kid）：新 key 签发的 token 被接受并投递；退役 key 签发的 token 被拒（unknown kid，刷新后仍无 → 签名拒绝面，D4 分类）。未知 kid 触发密钥源刷新（E10 `JwksKeyProvider` 行为），不要求静态 key 列表。

- **Rationale**: direction acceptance "new key served after rotation"；JWKS 轮换是 IdP 常态运维。
- **验证**: AC4。

### R6 — 测试基建：stub 真签 RS256 + 可换 key + 服务 `/jwks`

`stub.rs` 扩展（**既有 `make_jwt` alg:none 形式保留为拒绝路径 fixture**，不破坏 opt-in 面既有测试）：
- `make_jwt` 增 RS256 签名能力（`jsonwebtoken::encode`，workspace 已钉 "9"；rsa 0.9 生成测试 keypair；kid 可指定）；
- `SinkBehavior` 增：签名密钥选择（trusted / foreign）、可选篡改模式（签后改 claims 或签名段）、alg:none 保留；`/jwks` 端点服务 key 集（`SinkBehavior` 可换 = 轮换驱动）；
- 测试注入 seam：`AuditClient` 增 key-provider 注入构造（如 `with_key_provider(config, Arc<dyn KeyProvider>)`），生产 `new` 由 `jwks_uri` 构造（D2）。既有 28 个无 PG 测试 + 3 个 drill 在 JWKS 未配置时**零随迁保持绿**（opt-in，D2）——与兄弟 spec R5 的全量随迁路线二选一，本 direction acceptance 在 opt-in 下即可满足。

- **Rationale**: AC3(a)（alg:none 拒）与 AC4（轮换）需要真签 + 可换 key 集 + 服务端点；stub 是唯一测试车辆（E3/E4）。
- **验证**: AC1-AC4 全绿 + `cargo test -p aero-audit-connector --all-targets` 全绿。

### R7 — 回归钉：37/37 pins + T-11 drill 保持绿

`scripts/test-integration.sh` 全 harness 保持绿：37/37 契约钉（E7：`a3-relay-drill`、`t11-fail-closed`、`moderation-priority-drill`、`audit-provision-check` 等仓内槽 + 22 个 [PROPOSED] 槽计数不变）；T-11 drill（E8）断言逐字不变——**closed token endpoint ⇒ token 永不可得 ⇒ 签名路径永不参与**（T-11 语义与 exp/nbf/JWKS 正交）。

- **Rationale**: direction acceptance 明示；T-11 是 fail-closed 总纲（token endpoint 关闭 ⇒ rows pend、attempts grow、never falsely dead）。
- **验证**: AC5。

## 6. Acceptance criteria（direction 提供的 5 项，逐条可测化）

### AC1 — past `exp` ⇒ 拒绝，**零 POST**（stub minting expired claims）
新增 `tests/claim_validation.rs` 用例（沿用 `deliver_with_claims` 模式）：`SinkBehavior.token_claims` = 默认四查全匹配 + `"exp": <now_pinned - 120s>`；`client.deliver(&claim())` → `Err(_)`，且 **`stub.posts() == 0`**。确定性：经 `validate_token_claims_at` 钉 now（D1 时钟 seam），测试体内无 wall-clock 竞态（余量 120s = 2×leeway，无边界抖动）。

### AC2 — future `nbf` ⇒ 拒绝，**零 POST**
同模式：默认四查全匹配 + `"nbf": <now_pinned + 3600s>` → `Err(_)` + **`posts() == 0`**。

### AC3 — `alg:none` 拒绝（JWKS 启用时）+ 获取失败 fail-closed（transient 形状）
- (a) 配置 `AERO_AUDIT_JWKS_URL`（stub 服务 trusted key 集）时，stub 用现 `make_jwt`（`alg:none`）签发、四查全匹配 → `Err(_)` + **`posts() == 0`**；
- (b) JWKS 端点不可达/500（stub `/jwks` 关闭或错误状态）→ `Err(DeliveryError::Transient(_))` + **`posts() == 0`**，且经 `FakeOutbox` 驱动 relay 时**与 `transient_claim_drift_requeues_without_any_post` 同形状**（E6 全套断言）：`FakeStatus::Ready`（requeue 永不 dead）、`attempts == 1`、`available_at == t0 + backoff(1)`（fake 时钟钉死）、`claim_token`/lease 清空、`last_error` 含 JWKS 片段、重 claim 后**轮换新 token**。

### AC4 — kid 轮换（new key served after rotation）
stub 服务 key 集 {A}：A 签发的 token → `Ok(())`、`posts() == 1`；`set_behavior` 换服务 key 集为 {B}（新 kid）：B 签发的 token → `Ok(())`（刷新后接受）；A 签发的 token → `Err(_)`（unknown kid，签名拒绝面）且该次尝试 **`posts()` 不增长**。（实现可每阶段 fresh provider 绕过 10s 限速窗，D5；acceptance 只钉属性。）

### AC5 — 既有 pin 全绿
`scripts/test-integration.sh` 通过：37/37 契约钉 guard（E7）不回归；**T-11 fail-closed drill 绿且断言不改**（relay 缺席、token endpoint 关闭 ⇒ `COUNT(status=0)==N`、`COUNT(status IN (1,2,3))==0`、`SUM(attempts)` N→2N、`last_error` transport 片段——rows pend、attempts grow、**never falsely dead**）；`a3-relay-drill`、`moderation-priority-drill` 槽不变；`cargo test -p aero-audit-connector --all-targets` 全绿（既有 31 测试 = lib 11 + claim_validation 13 + state_machine 7，其中 3 PG 门控，+ 本 direction 新增用例）。

### AC 字面可满足性复核（reconciliation AM-2，2026-08-08）

> 改判（JWKS-on 面 opaque/畸形 → Permanent）只发生在签名面；下列每条 AC 的字面断言逐条复核，全部满足。

| AC | spec 字面断言 | 最终语义下的满足路径 | 判定 |
|---|---|---|---|
| AC1 | `Err(_)` + `posts()==0`；`exp = pinned −120s` | JWKS-off 面（`keys: None`，签名步 no-op）→ claims 面 `exp ≤ now+60` 拒 → `Err(Transient)`，0 POST；120s = 2×leeway 余量吞掉 wall-clock 微竞态 | ✅ 字面满足（Transient ⊂ `Err(_)`；改判仅发生在 JWKS-on 面，本夹具不在该面） |
| AC2 | `Err(_)` + `posts()==0`；`nbf = pinned +3600s` | 同上，`nbf > now+60` 拒 | ✅ 字面满足 |
| AC3(a) | 配置 `AERO_AUDIT_JWKS_URL` + 现 `make_jwt`（alg:none）→ `Err(_)` + `posts()==0` | JWKS-on 面签名先验：`decode_header` 后 alg ∉ {RS256} → `Err(Permanent(SignatureRejected))`，0 POST | ✅ 字面满足（spec 未钉 variant，`Err(Permanent)` ⊂ `Err(_)`） |
| AC3(b) | `Err(DeliveryError::Transient(_))` + `posts()==0` + E6 全套 requeue 形状 | `decoding_key_fallible` `Err` → Transient；`assert_transient_requeue` 全套（Ready/attempts==1/`t0+backoff(1)`/claim_token·lease 清空/last_error 含 JWKS 片段/重 claim 轮换 token） | ✅ 字面满足（R4 的 `"JWKS"` 片段为「如」示例，设计片段 `"audit jwks unavailable"` 含之） |
| AC4 | {A}: A 签 → Ok、`posts()==1`；换 {B}: B 签 → Ok、`posts()==2`；A 签 → Err、posts 不增 | `Ok(None)` → Permanent(SignatureRejected)；每阶段 fresh provider 或测试 seam 绕过 10s 限速（acceptance 只钉属性） | ✅ 字面满足 |
| AC5 | 37/37 guard + T-11 断言逐字不变 + `--all-targets` 31+ 全绿 | JWKS-off 默认构造（`new` 不变，12 构造点零随迁）使既有 28 run + 3 PG 门控语义不变；T-11 token endpoint 关闭 → 签名路径永不参与；6 处 `RelayConfig` 字面量补 `jwks_uri: None` | ✅ 字面满足（既有 `opaque_non_jwt_token…` 夹具在 JWKS-off 面分类不变；新增 `jwks_off_keeps_opaque_token_transient` 双钉） |

## 7. Out of scope（红线）

- **typ/iat/jti 存在性强制、sub==client_id 收敛、scope registry、token-endpoint 403→dead / scope-deficient→dead 分类**——兄弟 aero-auth B5-2 8/8 契约与兄弟 connector B5-2 分类面（E11）；本 direction 只补 exp/nbf + 签名验证，**不改四查行为与分类面**（R1/R3 的失败分类沿用既有 Transient / 兄弟永久面）。
- **L1 聚合（分析文件 direction #0）与 relay liveness（#2）**——同分析文件其他方向，不建。
- **aero-auth 生产代码改动**——只读 seam 消费（E10 类型复用）；**唯一例外 = AM-1 的 additive `decoding_key_fallible`**（默认实现委托既有 `decoding_key`，零行为变化，约 15 行 + 3 单测；不改既有方法语义、不改 `JwksKeyProvider` TTL/限速行为、不改 aero-auth 配置/boot）。`AERO_AUDIT_JWKS_URL` 与兄弟 `AERO_AUDIT_JWKS_URI` 统一为 `…_URL`（"URL wins"），同设 → `bail!`（AM-3 / D2）。
- **JWKS 可达性 boot 探测 / metrics / gauges**——staging 项（AGENTS.md §4.5「待联调」口径）与兄弟 F10 面，不在本 direction。
- **真实 IdP/sink 联调**——本地 stub 先行（E4/E6 车辆）。

## 8. Implementation notes（锚定已验证 seam）

**exp/nbf 半场**：在 `decode_jwt_claims` 已解出的 `Value` 上直接读 `exp`/`nbf`（NumericDate = JSON number；字符串数字形态按 malformed 拒，fail-closed）——**零新依赖**；`validate_token_claims_at` 纯函数 + `TIME_CLAIM_LEEWAY_SECS` 常量，`deliver()` 调用点不变（D6）。

**签名半场**：`jsonwebtoken::decode::<Value>`（`Validation::new(Algorithm::RS256)`；issuer/audience/required_spec_claims 关闭以免与既有四查重复）+ 密钥源 = `aero_auth::{KeyProvider, StaticKeyProvider, JwksKeyProvider}`（trait `oidc.rs:243` 公开，**实签 = `#[axum::async_trait]` + `async fn decoding_key(&self, kid: Option<&str>, algorithm)`——实现勿照抄 sync 草图**；`JwksKeyProvider::new` :571 带 TTL 300s + unknown-kid 限速刷新——轮换语义免费获得）+ **AM-1 additive `decoding_key_fallible`**（`Err` → Transient / `Ok(None)` → Permanent 双平面）。connector `Cargo.toml` 增 `aero-auth.workspace = true` + `jsonwebtoken.workspace = true` + `rsa`（dev，测试 keygen）——全部 workspace 已钉条目（E10），零新增供应链。签名拒绝映射 `PermanentKind` 新 variant（继承兄弟 R3/D1 分类，D4）；JWKS 获取失败 → `Transient`（R4）。

**构造点清单**（opt-in 路线下**零随迁**，仅新增注入 seam）：`AuditClient::new`（`relay.rs:316`、`state_machine.rs:63`、`claim_validation.rs` 全部 7 点、drill ×3）在 `jwks_uri: None` 时行为不变；新 JWKS 用例走注入构造。T-11 drill（E8）不受影响——token endpoint 关闭，签名路径永不参与。

**文件级锚点（grep 用）**：`client.rs::{validate_token_claims, decode_jwt_claims, deliver}` · `config.rs::{RelayConfig, from_env, service_url}` · `relay.rs::{deliver_claim, transient_claim_drift_requeues_without_any_post}` · `stub.rs::{make_jwt, SinkBehavior, StubSink::posts}` · `tests/claim_validation.rs::{deliver_with_claims, config}` · `scripts/b5-pin.sh::B5_CONTRACT_TEST_LIST` · `src/bin/aero-audit-t11-drill.rs` · `aero-auth/src/oidc.rs::{KeyProvider, StaticKeyProvider, JwksKeyProvider, validate_jwks_uri}`。

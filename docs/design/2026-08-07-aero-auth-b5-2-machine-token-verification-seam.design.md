# Design — B5-2 机器令牌验证 seam 复用：connector 消费 aero-auth 已测验证器（消费侧替换，零 aero-auth 改动）

- **Direction**: "Extract a reusable machine-token (client_credentials) verification seam from aero-auth for the B5-2 relay connector crate"（value 9 / risk_reduction 9 / effort 4 / confidence 9）
- **Requirements**: `docs/requirements/2026-08-07-aero-auth-b5-2-machine-token-verification-seam.req.md`（R1–R6, AC1–AC3）
- **Status**: Design（证据全部逐条复核；工作树已有 B5-2 connector 实现，本文档为替换/验收设计）
- **Verification date**: 2026-08-07

## 0. Evidence adjudication（untrusted claims → verified anchors）

| Evidence claim | Verdict | Verified anchor（实际） |
|---|---|---|
| `oidc.rs` — `validate_client_credentials_token` :406、`ClientCredentialsTokenConfig` :364、`ClientCredentialsClaims::granted_scopes` :387、sub==client_id :445-450、at+jwt typ :414-417、RS256/EdDSA allowlist :423-428、future-iat 拒绝 :453 | ✅ 全部精确命中 | `crates/aero-auth/src/oidc.rs`：`ClientCredentialsTokenConfig{issuer,audience,required_scopes}` :364；`granted_scopes` = `scopes` 数组 ∪ 空格分隔 `scope` 的 BTreeSet 归一化 :387；验证器 :406-471：typ ∈ {`at+jwt`,`application/at+jwt`} :414-417 → alg 白名单 RS256/EdDSA :423-428 → KeyProvider 按 kid :431-434 → `required_spec_claims` 含 iss/aud/exp/nbf/iat/jti/sub/client_id :440 → `sub == client_id` + `valid_identity_component` :445-450 → future-iat 拒绝 :453 → jti 非空/≤1024/无控制字符 :455-459 → required_scopes ⊆ granted :461-466。`LEEWAY_SECS=60` :293 |
| `oidc/tests.rs` — 8 个 client_credentials 测试函数（:332/:350/:366/:378/:392/:406/:429/:452）+ 夹具（`client_credentials_cfg` :163、`good_client_access_claims` :214、`sign_client_access_token` :230、`keypair` :64） | ✅ 全部精确命中 | `crates/aero-auth/src/oidc/tests.rs`：8 函数逐行对上（accepts scopes 数组 / 空格分隔 scope；rejects wrong typ、sub!=client_id、缺 required scope、wrong iss/aud、expired、缺 nbf、missing/future iat、missing/empty/control jti——:452 含 5 子例）；夹具 RS256 真签 + kid + 显式 typ |
| `lib.rs` :28-29 — seam 完整 re-export，无需移动代码 | ✅ 精确命中 | `crates/aero-auth/src/lib.rs:28-29`：`pub use oidc::{validate_client_credentials_token, …, ClientCredentialsTokenConfig, JwksKeyProvider, JwksUriError, KeyProvider, OidcError, StaticKeyProvider, …}` |
| `integrations.rs` :240 — 唯一生产消费方 | ✅ 成立（实为 `crates/aero-server/src/integrations.rs`） | `authenticate_machine` :233-246，`:240` = `validate_client_credentials_token(token, &config.token, provider)`；`INTEGRATION_JWKS` OnceLock :63 + `JwksKeyProvider::new(config.jwks_uri)`；全仓 grep 确认无第三处调用 |
| `snaplink_commercial/http.rs` — `SCOPE_AUDIT` :23、cc grant :233、`validate_token` 仅形状校验 | ✅ 成立（路径实为 `crates/aero-server/src/snaplink_commercial/http.rs`） | `SCOPE_AUDIT = "audit:event:write"` :23；cc grant form `grant_type=client_credentials` :233；`validate_token` 实为 :430-441（行号漂移）——非空/≤16KiB/无控制字符/bearer 类型，无任何 claim 检查 |
| Proposal doc — B5-2 :9、T-11 gate :15、37/37 仓外 :13 | ✅ 精确命中 | `docs/proposals/audit-contract-batch-aero-im.md`：:9 B5-2 语义（lease>2×timeout、退避 cap 300s、422/409/回执→dead ≤1 次、403→dead = T-11 fail-closed、cc + claim 校验）；:15 T-11 门禁；:13 "37/37" 清单在仓外 [PROPOSED] |
| **关键发现**：connector 工作树已存在手写重复实现——`client.rs::validate_token_claims` :197-263 = base64url 手解（`decode_jwt_claims` :415），8 项契约只覆盖 4 项（iss/aud/scope/sub），无 typ/exp/iat/jti/sub==client_id/签名；stub 发 `alg:none` 假签名 token | ✅ 全部成立 | `crates/aero-audit-connector/` 全仓 untracked（前一批次产物）。`validate_token_claims` :197-263（shape + iss/aud/scope/sub 四查，sub 比对**配置的** `expected_sub` 而非 client_id）；`decode_jwt_claims` :415-435（base64url，无验签）；`stub.rs::make_jwt` :233-245 发 `{"alg":"none","typ":"JWT"}` + 假签名段，模块注释明言 "signature is not verified" |
| （补充）connector 依赖面：无 aero-auth 依赖；`expected_iss/aud/scope/sub` 配置 :31-37；relay 状态机 403→dead 立即 / permanent→requeue→dead / claim 校验失败→Transient；`base64="0.22"` 直接依赖 | ✅ 全部成立 | `Cargo.toml` 无 aero-auth（仅 workspace-pinned + base64）；`config.rs` :31-37 + from_env :52-110（`AERO_AUDIT_*` presence-gated，`AERO_AUDIT_EXPECTED_SUB` 必填）；`relay.rs::deliver_claim` :168-228（Forbidden→`mark_dead` :176-190、Permanent→requeue×1→dead :191-202、Transient→requeue :204-213）；`transient_claim_drift_requeues_without_any_post` 实为 :370（requirements 文档记 :383，行号漂移，符号确认） |
| （补充）in-repo 门禁：`scripts/test-integration.sh` drill 接线 :244-269；workspace `jsonwebtoken="9"` :133；`aero-audit-connector` 为 workspace member :29 | ✅ 全部成立 | drill 用 `StubSink` + 手建 `RelayConfig`（client_id="drill-client"、expected_sub="aero-im.source"）；0239 未落库（B5-1 门控），drill 显式 SKIP |
| （补充）既有测试面 | ✅ 实跑口径 | `tests/claim_validation.rs` 现有 10 个 A2 异步用例（wrong iss/aud/scope/sub、opaque、valid、401-refresh、refresh-revalidate、status 分类、payload guard）+ 1 个同步 `jwt_claims_decode_roundtrip`；`tests/state_machine.rs` 6 个 A1 用例（含 `forbidden_dead_on_first_attempt` :216）；`relay.rs` 内部 5 个测试（含 3 个 transient 中继）。**全部经由 stub 的 alg:none token 驱动——stub 改造后全部需随迁到签名 token** |

**勘误/细化汇总**：① `integrations.rs` 与 `snaplink_commercial/http.rs` 均在 `crates/aero-server/src/` 下（路径补齐）；② `validate_token` 实际 :430（非 :353-364）；③ `transient_claim_drift_requeues_without_any_post` 实际 :370；④ 既有测试面比 requirements 文档列的多 2 个（`unauthorized_refreshes_once_and_retries_within_the_attempt`、`refreshed_token_must_repass_claim_validation_before_retry_post`、`client_classifies_statuses`、`payload_guard_rejects_tenant_selection_as_permanent`——后两者在 claim_validation.rs）。**核心结论不变且更强**：direction 预警的重复实现已实际存在于工作树，且 22 项既有测试（claim_validation 10 + state_machine 6 + relay 内部 5 + drill 1）都依赖 stub 的 alg:none token（config 2 项不涉）——签名验证强制化（复用 E1 的必然结果）是**测试面全面随迁**的触发点，本文档显式覆盖。⑤ F10 硬化（delivery-semantics review 追加）：新增 §2.7（status=3 可观测信号 + ops requeue runbook + blast-radius 对账）、§5 step 9（时钟偏移带 staging 检查 + 告警规则 + runbook 发布）、§6 可观测/ops 验收、§7（静默停投告警 + dead 出口 + 对账）——见下。

## 1. Design overview

**消费侧替换**：`crates/aero-audit-connector` 增加 `aero-auth`（workspace crate）依赖，把 `AuditClient::deliver` 里每次 POST 前置的手写 `validate_token_claims`（4/8 契约 + 无验签）替换为已测 seam `aero_auth::validate_client_credentials_token`（8/8 契约 + RS256/EdDSA 强制验签），生产 KeyProvider = `JwksKeyProvider`（`AERO_AUDIT_JWKS_URI`，boot fail-loud），删除 `decode_jwt_claims` 与 `validate_token_claims` 的 claim 检查部分（形状快速失败可留）。**零 aero-auth 生产代码改动**（seam 已公开，lib.rs:28-29；移动反而增加 churn）。

```
  crates/aero-audit-connector                    crates/aero-auth（零改动）
 ┌──────────────────────────────────┐            ┌──────────────────────────┐
 │ RelayConfig::from_env (AERO_AUDIT_*)│           │ oidc.rs                  │
 │  + jwks_uri / client_allowlist    │            │  validate_client_        │
 │        │                          │            │   credentials_token :406 │
 │        ▼                          │            │  ClientCredentialsToken  │
 │ AuditClient.deliver(claim)        │            │   Config :364            │
 │  access_token (缓存+401失效)       │ ──────►   │  KeyProvider trait :243  │
 │  └─ validate_client_credentials_  │  依赖       │  StaticKeyProvider :252  │
 │     token(token, cc_cfg, keys)    │ (workspace) │  JwksKeyProvider :529    │
 │     ├─ Ok → POST events_url       │            │  validate_jwks_uri :54   │
 │     ├─ scope-deficient → Unprovisioned → dead 1│  oidc/tests.rs 8 夹具    │
 │     └─ 其他 OidcError → Transient requeue      └──────────────────────────┘
 │  request_token: 403 → Forbidden → dead 1（新增分类，闭 unprovisioned 重试环）
 │  stub.rs: alg:none 假签名 → RS256 真签 at+jwt（jsonwebtoken::encode）
 └──────────────────────────────────┘
```

分类收敛（T-11 fail-closed）：**dead 于 attempt 1** 的面 = sink 403（既有）+ token-endpoint 403（新增）+ scope-deficient token（新增，由 Transient 无限重试改判）；**Transient requeue** 的面 = 其余全部 claim 违规（wrong iss/aud、sub!=client_id、wrong typ、expired、future/missing iat/jti、unknown key、opaque、坏签名——IdP 配置漂移，由 B5-4 配给修复）+ 传输类错误（不变）。

## 2. API changes

### 2.1 `crates/aero-audit-connector/Cargo.toml`
| 变更 | 内容 | 理由 |
|---|---|---|
| + `aero-auth.workspace = true` | 常规依赖 | seam 消费；workspace crate，零新增第三方供应链项 |
| + `jsonwebtoken.workspace = true` | 常规依赖 | `stub.rs` 在 lib 内（`pub mod stub`），改用 `jsonwebtoken::encode` 产真签 at+jwt；workspace 已钉 `jsonwebtoken = "9"`（Cargo.toml :133，aero-auth 同款），零新增供应链项 |
| − `base64 = "0.22"` | 移除 | 唯一两处使用（`client.rs` decode_jwt_claims、`stub.rs` make_jwt 手编 header/payload）随改造消失；jsonwebtoken 内化 base64url |

### 2.2 `src/config.rs` — `RelayConfig`
| 字段 | 类型 | 语义 |
|---|---|---|
| + `jwks_uri` | `Option<String>` | 生产必填：`AERO_AUDIT_JWKS_URI`（presence-gated 家族），from_env 时过 `validate_jwks_uri`（HTTPS 或 loopback-HTTP，无 userinfo/query/fragment，镜像 `integrations.rs:197-231`），缺失/非法 → `bail!` fail-loud。`Option` 仅为测试/ drill 手建 config 提供 `None`（配合 `with_key_provider`），生产路径 `None` 即 boot Err |
| + `client_allowlist` | `Vec<String>` | `[PROPOSED]`：`AERO_AUDIT_CLIENT_ALLOWLIST` 逗号分隔；与既有 `expected_sub`（保持必填，收敛为单条目 allowlist，兼容旧部署）取并集；空 = 不额外限制（`ip_allowlist` 先例）；配置存在但不含本 client_id → boot fail-loud |

其余字段不动。`expected_iss`/`expected_aud`/`expected_scope`（默认 `audit:event:write` = `SCOPE_AUDIT`）→ 映射 `ClientCredentialsTokenConfig`；`expected_sub` 不再参与 validator 输入（sub==client_id 由 validator 强制），仅作 allowlist 条目。

### 2.3 `src/client.rs` — `AuditClient`
| 变更 | 签名/行为 |
|---|---|
| + `AuditClient::with_key_provider(config, Arc<dyn KeyProvider>)` | 测试/ drill 注入 `StaticKeyProvider::single(dec)`；不触碰 jwks_uri |
| ~ `AuditClient::new(config)` | 生产构造：`config.jwks_uri` 为 `Some` 且过 `validate_jwks_uri` → `JwksKeyProvider::new(uri)`（OnceLock 或实例字段均可，镜像 `integrations.rs:63` 模式）；`None`/非法 → `anyhow::Err` fail-loud |
| ~ `deliver` 循环内校验 | `self.validate_token_claims(&token)` → `aero_auth::validate_client_credentials_token(&token, &self.cc_config, self.keys.as_ref())`。`cc_config` 在构造时从 config 建一次（`ClientCredentialsTokenConfig{issuer: expected_iss, audience: expected_aud, required_scopes: vec![expected_scope]}`）。**每次 POST 前置**（首 POST + 401-刷新重试 POST 都过校验，`refreshed_token_must_repass_claim_validation_before_retry_post` 语义保持） |
| + `classify_validation_error(OidcError) -> ValidationClass` | 单一分类点：`Invalid(msg)` 且 msg == seam 的固定串 `"token lacks a required application scope"` → `Unprovisioned`；其余 `MalformedToken`/`UnknownKey`/`UnsupportedAlgorithm`/`Invalid(_)` → `Other`。串匹配集中在**一个函数** + 单测钉死（seam 文案若变，测试响亮失败而非静默） |
| ~ `request_token` | 403 → `Err(DeliveryError::Forbidden)`（新增分类，闭 unprovisioned 无限重试环）；其余非 200 → 既有 `Transient`（401 属 `access_token` 刷新路径，不动） |
| − `validate_token_claims` / `ClaimRejection` / `decode_jwt_claims` | 删除（token 形状快速失败 `validate_token_shape` 保留为前置护栏，非契约校验） |
| + `DeliveryError::Unprovisioned` | `#[error("audit credential is not provisioned for the audit scope (T-11)")]`——与 `Forbidden` 同 terminal（dead attempt 1），独立变体保 `last_error` 保真（relay 注释明言 distinction kept for last_error fidelity） |

### 2.4 `src/relay.rs`
`deliver_claim` 的 dead 臂扩为 `Err(DeliveryError::Forbidden | DeliveryError::Unprovisioned)` → `mark_dead(event_id, token, attempts, …)`（attempt 1 即 dead，无 requeue）。既有 Forbidden 臂 :176-190 原样保留。

### 2.5 `src/stub.rs`（测试桩——全部既有测试的驱动源）
| 变更 | 内容 |
|---|---|
| + `test_keypair() -> (EncodingKey, DecodingKey)` | RS256 测试密钥对（镜像 `oidc/tests.rs:64` 夹具模式；connector 测试镜像而非共享，守「aero-auth 零改动」红线）；`StubSink` 暴露 `decoding_key()` 访问器供测试/ drill 建 `StaticKeyProvider` |
| ~ `make_jwt(claims)` | 改发 **RS256 真签 at+jwt**：`Header{alg: RS256, kid: Some("test-key"), typ: Some("at+jwt")}` + `jsonwebtoken::encode`（claims 含 `sub == client_id`） |
| + `SinkBehavior.token_style` | `TokenStyle::{Signed, Opaque, AlgNone}`，默认 `Signed`——Opaque/AlgNone 保留为**拒绝路径**用例（fail-closed 面） |
| + `SinkBehavior.token_endpoint_status` | 默认 200；AC2(b) 未配给面置 403 |

### 2.6 `tests/` 与 `bin/aero-audit-relay-drill.rs`
- 两测试文件 + relay.rs 内部测试的 config 构造加 `jwks_uri: None`、`client_allowlist: vec![]`；`expected_sub` 由 `"aero-im.source"` 改 `"drill-client"`（validator 强制 sub==client_id，stub 发 sub==client_id 的 token）。
- 测试构造 `AuditClient::with_key_provider(config, StaticKeyProvider::single(stub.decoding_key()))`。
- drill（真实 PG + StubSink）：同上随迁；0239 未落库 → SKIP 路径保持。

### 2.7 可观测性与 ops 通道（F10 硬化——delivery-semantics review 追加）

**status=3 可观测信号**（种子 = drill 的 stuck 计数——`bin/aero-audit-relay-drill.rs` 现统计 `WHERE status IN (0,1,3)`，按口径拆分为两个 gauge）：

| 信号 | 名称常量（`aero_common::metrics::names`，对齐 `AI_DEAD_LETTER_QUEUE_SIZE` 先例） | 采样 SQL（源自 drill stuck 口径拆分） |
|---|---|---|
| dead 计数 gauge | `AUDIT_OUTBOX_DEAD_ROWS` = `aero_audit_outbox_dead_rows` | `SELECT COUNT(*)::bigint FROM audit_governance_outbox WHERE status = 3` |
| 最老卡行年龄 gauge | `AUDIT_OUTBOX_STUCK_OLDEST_SECONDS` = `aero_audit_outbox_stuck_oldest_seconds` | `SELECT MAX(EXTRACT(EPOCH FROM (clock_timestamp() - available_at)))::bigint FROM audit_governance_outbox WHERE status IN (0,1,3)` |

- **采样位置**：`aero-server/src/bin/boot/metrics_tasks.rs`（connector relay 由 aero-server boot 装配，`bin/main.rs` 的 `RelayConfig::from_env` 分支；镜像 AI DLQ 采样块：30s tick、共享 cancel token、query Err 留旧值 warn——observability_gauge_samplers 模式：**纯读设 gauge，永不改状态**）。0239 未落库时查询 Err → warn + 旧值保留（与 boot 降级一致，不 panic）。
- **告警规则**（随部署 yaml 下发，非本仓代码）：`aero_audit_outbox_dead_rows > 0` → warning（对齐 `AI_DEAD_LETTER_QUEUE_SIZE` 的 "alert at threshold > 0"；dead 行需人工 requeue 或配给修复）；`aero_audit_outbox_stuck_oldest_seconds > 600` → critical（600 = 2×退避 cap 300s：健康队列 re-park ≤300s，>600 即至少一轮投递停滞——覆盖 F17 时钟偏移 Transient 环、0239 缺失、relay 未启等 **dead=0 但投递静默停止**的面）。

**ops requeue 路径（status 3→0）**——无 sweeper、无复活。dead 是状态机终态（§3.3），唯一出口 = **显式人工操作**，per-row fenced：

```sql
-- runbook：操作员显式 requeue 单行（status 3→0）
UPDATE audit_governance_outbox
   SET status = 0, attempts = 0,
       available_at = clock_timestamp(),
       claim_token = NULL, lease_expires_at = NULL,
       last_error = 'ops requeue by <operator> at <timestamp>'
 WHERE event_id = $1 AND status = 3 AND delivered_at IS NULL;
-- 批量形式：WHERE event_id = ANY($1::uuid[]) AND status = 3 AND delivered_at IS NULL
```

四守卫：① `status = 3`——dead 行 `claim_token`/`lease_expires_at` 均为 NULL（mark_dead 清空两者），claim 期栅栏（token 匹配 + 未过期 lease）天然不适用，**status=3 即本路径的栅栏**：claimed(1)/delivered(2) 行不可达；② `delivered_at IS NULL`——防复活双保险（dead 行现恒为 NULL；DDL 演化时仍显式成立）；③ `attempts = 0` 重置——permanent 类行在 attempts≥2 时 dead（claim 时 attempts+1），保留 attempts 会在下次 claim 立即再 dead，requeue = 新预算；④ `rows_affected == 0` 幂等 no-op（并发双操作员 / 行已不在 dead）→ 操作员复查。**无 timer/spawn 调用它**（无 sweeper）；批量形式只接受操作员圈定的显式 event_id 清单，非按时间/状态批量复活。

代码形态：`OutboxRepo::ops_requeue(event_id, operator) -> Result<bool, Error>`（`outbox.rs` trait + `pg.rs`/`fake.rs` 双实现，SQL 即上文）；**relay 永不调用**（结构性：仅 ops 工具/控制台可达，无自动路径）。可选 admin HTTP 面属另一 direction 的范围决策，本设计不引入。

**与 per-row blast-radius 对账**：delivery-semantics review 结论（"dead rows don't block the queue; blast radius is per-row-at-claim-time"）**不变且被加强**——requeue 行重新进入 claim 池后仍受完整栅栏（token 轮换、lease、attempts、SKIP LOCKED），单行失败仍只影响自身；gauge 是队列级**只读**信号，不触碰状态；requeue 是行级显式写。**全局可见性 + 逐行爆炸半径**：可观测在队列级，状态转换永远在行级；告警不自动 requeue（自动 requeue = sweeper，禁止）。

## 3. Compatibility constraints

1. **aero-auth 零生产改动**（红线）：seam 已公开且签名稳定（`validate_client_credentials_token` 不变），connector 只消费。测试夹具镜像（复制 `oidc/tests.rs` 模式）而非导出 test-support——不扩大 aero-auth 公共面。
2. **依赖方向合法无环**：aero-auth 是基础层、connector 是组合层；aero-auth 不依赖 connector。零新增第三方供应链项：`aero-auth` + `jsonwebtoken` 均 workspace 已钉；`base64` 净移除（Cargo.lock 仍含 base64——aero-server/aero-live-srt 在用，无 lockfile 震荡）。
3. **不动面**：`crates/aero-server/src/integrations.rs`（唯一既有消费者，原样）；v1 `snaplink_commercial/`（仍零入站 claim 校验，B5-2 范围外，git diff 守卫）；B5-1 0239 DDL/outbox（未落库，drill SKIP 门保持）；relay 状态机既有分类（403→dead、permanent→requeue×1→dead、transients 永不 dead、lease>2×timeout 不变量）；**dead 行永不被自动复活**（无 sweeper——reviewer 确认既有不变量）：唯一 3→0 写路径是 §2.7 显式 ops requeue（relay 四语句永不写 status=3，grep 守卫）。
4. **行为变更（direction 明示，接受）**：① 签名验证由 "[PROPOSED]/不验签" 变为**强制**——真实 IdP 必须提供可达 JWKS，opaque 无 JWKS token fail-closed（不投递）；② `sub==client_id` 强制——`expected_sub` 收敛为 allowlist 条目，旧部署若 token sub ≠ client_id 将 fail-closed 直到对齐（B5-4 配给门）；③ token-endpoint 403 → dead attempt 1（原 Transient 无限重试）——401 保持 Transient（secret 轮换可修复）；④ scope-deficient token → dead attempt 1（原 Transient）。
5. **配置面 delta（部署告警）**：`AERO_AUDIT_JWKS_URI` 新必填（connector 启用时缺失 → boot Err）；`AERO_AUDIT_CLIENT_ALLOWLIST` 新可选；`AERO_AUDIT_EXPECTED_SUB` 语义收敛（值应等于 client_id）。
6. **既有测试面全量随迁**：24 项既有测试全部经 stub alg:none token 驱动——stub 换签名 token 后逐项随迁（机械替换，语义断言不变，新增/改判面见 §6）。

## 4. Failure modes（每行 = 故障 → 检测点 → 分类 → relay 转换）

| # | 故障 | 检测点 | 分类 | 转换 |
|---|---|---|---|---|
| F1 | opaque / 畸形 token | `OidcError::MalformedToken` | Transient | requeue，下次 claim 轮换新 token，无 POST |
| F2 | wrong typ（id-token/JWT 混淆） | `OidcError::Invalid("token is not an RFC 9068 access token")` | Transient | 同上 |
| F3 | 白名单外算法（含 stub 旧 alg:none） | `OidcError::UnsupportedAlgorithm` | Transient | 同上 |
| F4 | unknown kid / JWKS 不可达 / 轮换未生效 | `OidcError::UnknownKey`（JwksKeyProvider 内部限速刷新，防请求风暴） | Transient | 同上 |
| F5 | 签名不匹配（伪造/错 key） | `OidcError::Invalid`（jsonwebtoken decode 失败） | Transient | 同上 |
| F6 | iss/aud 不匹配 | `OidcError::Invalid` | Transient | 同上 |
| F7 | expired / 缺 nbf / missing-future iat / 坏 jti | `OidcError::Invalid` | Transient | 同上 |
| F8 | sub != client_id（机器身份混淆） | `OidcError::Invalid("token is not a client_credentials machine identity")` | Transient | 同上（IdP 配置漂移，B5-4 修复） |
| F9 | **scope-deficient（granted 缺 `audit:event:write`）** | `OidcError::Invalid("token lacks a required application scope")` | **Unprovisioned** | **dead attempt 1，无 POST**（T-11；闭既有 Transient 无限重试环） |
| F10 | **token-endpoint 403（未配给）** | `request_token` 状态码 | **Forbidden** | **dead attempt 1**（闭既有 Transient 无限重试环） |
| F11 | sink 403（拒绝服务身份） | POST 状态码 | Forbidden | dead attempt 1（既有，`forbidden_dead_on_first_attempt` 保持绿） |
| F12 | JWKS URI 缺失/非法（boot） | `config.rs::from_env` | — | **fail-loud**：relay 不启动、不 claim（fail-closed 不投递） |
| F13 | allowlist 不含本 client_id / granted 含注册集外 scope（[PROPOSED]） | 验证器之后叠加检查 | Unprovisioned | dead attempt 1 |
| F14 | token-endpoint 5xx/超时/400 | `request_token` | Transient | requeue（不变） |
| F15 | 401-after-POST | `access_token` 失效 + 刷新 | 同一 attempt 内重试一次 | **重试 POST 前重新过验证器**（`refreshed_token_must_repass…` 保持）；刷新后仍 401 → Transient |
| F16 | claim 校验失败但 `last_error` 需要区分（串匹配脆弱性） | `classify_validation_error` 单点 | — | 单测钉死 seam 固定文案；seam 文案变更 → 测试响亮失败而非静默误分类 |
| F17 | **时钟偏移越界**（\|app↔IdP\| > LEEWAY 带 / audit 服务时钟滞后 IdP） | `aero_audit_outbox_stuck_oldest_seconds` gauge | Transient（既有 F7/F15 语义不变，无新分类） | requeue 环 0 POST 0 dead——**检测**由 stuck-oldest 告警覆盖（静默停投，§2.7/§5 step 9）；修复 = NTP/配给对齐 |
| F18 | dead 行累积（403/Unprovisioned/permanent 终态） | `aero_audit_outbox_dead_rows` gauge | — | 告警 > 0 → 操作员按 §2.7 runbook 显式 requeue（fenced 3→0）或修复配给（B5-4） |

## 5. Migration steps（每步带门禁；worktree 全 untracked，先 `git reset --hard master` 校准基线）

1. **依赖**：`Cargo.toml` + `aero-auth.workspace = true`、+ `jsonwebtoken.workspace = true`、− `base64`。门禁：`cargo check -p aero-audit-connector` 预期失败（代码未迁），`cargo tree -p aero-audit-connector` 无新第三方包。
2. **config.rs**：+ `jwks_uri: Option<String>`、+ `client_allowlist: Vec<String>`；from_env 解析 `AERO_AUDIT_JWKS_URI`（`validate_jwks_uri` 门 + 缺失 bail）+ `AERO_AUDIT_CLIENT_ALLOWLIST`（逗号分隔 ∪ expected_sub）；config 单测（fail-loud 三例：缺失/非法/allowlist 不含本 client）。
3. **client.rs**：构造 `cc_config` 与 key provider（`new` + `with_key_provider`）；`deliver` 换验证器；+ `classify_validation_error`；+ `DeliveryError::Unprovisioned`；`request_token` 403 → Forbidden；删除 `validate_token_claims`/`ClaimRejection`/`decode_jwt_claims`（保留 `validate_token_shape` 前置护栏）。门禁：`cargo check -p aero-audit-connector` 绿。
4. **relay.rs + ops 通道（F10 硬化）**：dead 臂扩为 `Forbidden | Unprovisioned`；`outbox.rs` trait + `pg.rs`/`fake.rs` 加 `ops_requeue(event_id, operator)`（§2.7 fenced 3→0 SQL；relay 不调用——无 sweeper）。
5. **stub.rs**：+ `test_keypair()`、`make_jwt` 改 `jsonwebtoken::encode` 真签 at+jwt（kid="test-key"、typ="at+jwt"、sub==client_id）、+ `token_style`（Signed/Opaque/AlgNone）、+ `token_endpoint_status`、+ `decoding_key()` 访问器、+ `token_skew_secs`（mint 偏移：iat/nbf/exp 整体平移，§2.7 偏移带模式）、+ `sink_skew_secs`（sink 校验时钟偏移：按自身时钟验 exp/nbf，过期/未生效即 401）。
6. **测试随迁**：`claim_validation.rs` 重写为 8 项契约矩阵（§6 AC1）+ 保留 opaque/alg:none 拒绝路径 + 401 刷新两例 + 状态分类 + payload guard；`state_machine.rs` 加 AC2 三面 + ops_requeue 四用例（§6）；relay.rs 内部测试 config 随迁（`transient_claim_drift_requeues_without_any_post` 语义不变——evil-iss 仍 Transient）。门禁：`cargo test -p aero-audit-connector --all-targets` 全绿（含既有 24 项随迁后 + 新增）。
7. **drill bin**：config 加 `jwks_uri: None`、`expected_sub: "drill-client"`、`with_key_provider(stub.decoding_key())`；+ 三偏移带模式 flag：`--token-skew <s>`、`--sink-skew <s>`、`--check-expires-in-exp`（断言见 §5 step 9）；PASS 行打印 stuck 明细（dead/stuck/oldest——gauge 种子值）。门禁：`scripts/test-integration.sh` drill 在 0239 未落库时保持显式 SKIP；B5-1 落库后解门（三模式随之可跑）。
8. **全量门禁**：`cargo check --workspace` 干净 · `cargo test --workspace --lib`（`-- --ignored` PG 门控另跑）· `cargo clippy --workspace --all-targets` 无新警告 · `scripts/{truth-check,file-size-check,web-check}.sh` 0 违规 · `cargo test -p aero-auth`（seam 8 项零改动仍绿）· `git diff` 守卫：`crates/aero-auth/`、`crates/aero-server/src/integrations.rs`、`crates/aero-server/src/snaplink_commercial/` 零改动。
9. **部署 env 清单 + staging 验证清单（含时钟偏移带与 ops 通道——F10 硬化）**：
   - **env 清单**：补 `AERO_AUDIT_JWKS_URI`（必填）；`AERO_AUDIT_EXPECTED_SUB` 对齐 client_id；可选 `AERO_AUDIT_CLIENT_ALLOWLIST`。gauge 采样 30s 固定（对齐 AI DLQ 先例，不加 env）。
   - **staging 真实对端**（AGENTS.md §4.5）：真实 token endpoint + JWKS 可达 + 真实签发 token 的 sub==client_id 核对；**NTP 漂移检查**——connector 主机与 IdP 时钟差 ≤30s（HTTPS `Date` 头或 chrony 对账；LEEWAY=60 的透明带中央留半带余量），audit 服务时钟不得滞后 IdP（sink 滞后 → token 未生效 401 环，§2.7）；**真实 token 的 `--check-expires-in-exp`**——`|(iat + expires_in) − exp| ≤ 60s` 且 `exp − iat ≥ 120s`（refresh 窗 ttl−30 + LEEWAY 余量）。
   - **drill 时钟偏移带模式**（0239 解门后随 `scripts/test-integration.sh` 跑；带边界由 `LEEWAY_SECS=60` / 缓存 ttl−30 / ttl+LEEWAY=360 派生，drill 断言行为口径）：
     - `--token-skew <s>`（s = IdP 时钟 − app 时钟）：−360 ≤ s ≤ 60 → 全投递（双向 LEEWAY 透明带）；s > 60 → future-iat 拒绝 → 0 POST / 0 delivered / 0 dead、行 requeue；s < −360 → exp+LEEWAY 拒绝 → 同上（0 POST fail-closed 环，stuck 增长）；
     - `--sink-skew <s>`（s = audit 服务时钟 − IdP 时钟）：0 ≤ s ≤ 30 → 全投递（缓存 ttl−30 窗内 sink 仍有效）；30 < s ≤ 300 → 首 POST 401 → 恰一次刷新 → 重验证 → 重 POST → 全投递、`posts() == 2·N`（**401-window refresh-once** 的 staging 形态）；s > 300 → 401-after-refresh → Transient 环（0 delivered / 0 dead）；s < 0 → sink 视角 token 未生效 → 401 环（不对称带）；
     - `--check-expires-in-exp`：expires_in↔exp 漂移守卫（stub 真实签发 token 同检）。
   - **告警规则**（随部署 yaml 下发，非本仓代码）：`aero_audit_outbox_dead_rows > 0` → warning（dead 行需人工 requeue / 配给修复）；`aero_audit_outbox_stuck_oldest_seconds > 600` → critical（静默停投）。
   - **ops requeue runbook**（§2.7 SQL + 四守卫 + 事后复查 `SELECT status, attempts, last_error FROM audit_governance_outbox WHERE event_id = …`）随部署文档发布；grep 守卫确认 relay 无 3→0 自动写路径。

## 6. Testable acceptance mapping（AC1–AC3 → 精确测试符号）

### AC1 — 8 项契约矩阵（37/37 的 in-repo 子集 = `oidc/tests.rs` 8 个 client_credentials 测试）
`crates/aero-audit-connector/tests/claim_validation.rs` 重写，夹具镜像 E2（测试内 RS256 keypair 真签 at+jwt、`sub==client_id=="drill-client"`、kid="test-key"），每项拒绝断言 `stub.posts() == 0`（字面测量，无 POST 前置拒绝）：

| # | 用例（镜像 `oidc/tests.rs`） | 断言 |
|---|---|---|
| 1 | accepts scopes 数组（`scopes: ["audit:event:write"]`） | Ok + `posts() >= 1` |
| 2 | accepts 空格分隔 scope（`scope: "audit:event:write metering:read"`） | Ok + `posts() >= 1` |
| 3 | 拒 wrong typ（`typ: "JWT"` 头） | Err + `posts() == 0` |
| 4 | 拒 sub != client_id | Err + `posts() == 0` |
| 5 | 拒缺 `audit:event:write`（scope 为 `billing:entitlement:read`） | Err(Unprovisioned) + `posts() == 0` |
| 6 | 拒 wrong iss / wrong aud（两子例） | Err + `posts() == 0` |
| 7 | 拒 expired / 缺 nbf | Err + `posts() == 0` |
| 8 | 拒 missing iat / future iat / missing jti / empty jti / control jti（5 子例，参数化） | Err + `posts() == 0` |

+ 保留路径：opaque token / alg:none token（`token_style`）→ `posts() == 0`；`unauthorized_refreshes_once_and_retries_within_the_attempt`（posts==2）与 `refreshed_token_must_repass_claim_validation_before_retry_post`（posts==1）随迁保持。

### AC2（T-11）— unprovisioned / scope-deficient → dead-terminal ≤1 attempt，无 retry loop
`tests/state_machine.rs` 三面参数化（fake outbox + stub sink，每面断言 `FakeStatus::Dead`、`attempts == 1`、后续 `claim_due` 为空即 dead 行永不被选中）：
- (a) stub 发缺 `audit:event:write` 的**签名** token → dead attempt 1、`posts() == 0`；
- (b) `token_endpoint_status = 403`（未配给）→ dead attempt 1、`posts() == 0`；
- (c) sink 403 → 既有 `forbidden_dead_on_first_attempt`（tests/state_machine.rs :216）保持绿（不改）。
- 对照（不回归）：`transient_claim_drift_requeues_without_any_post`（relay.rs :370，evil-iss → Transient requeue 无 POST）语义保持——wrong-iss 仍 Transient，只有 scope-deficient/unprovisioned 改判 dead。
- 配给缺席面：`config.rs::from_env` 无 `AERO_AUDIT_TOKEN_ENDPOINT` → `Ok(None)`（relay 不启动 = fail-closed），既有 config 单测保持。

### AC3（[PROPOSED]）— per-client allowlist + scope registry 单测（5 用例，connector crate 内）
① allowlist 不含 token 的 client_id → 拒、`posts() == 0`；② granted 含注册集外 scope（如 `admin:*`）→ 拒、`posts() == 0`；③ allowlist 未配置 → 不额外限制（有效 token 放行投递）；④ allowlist 配置但不含本 client_id → config fail-loud（boot Err）；⑤ 注册 client + `audit:event:write` → 通过并投递（202 回执路径）。registry 输入经 `RelayConfig` 注入（仓外 registry 本体不依赖）。

### 可观测性与 ops 通道验收（F10 硬化——review 追加，非 requirements AC 增量）
- `state_machine.rs`（fake）：`ops_requeue_returns_a_dead_row_to_the_claimable_pool`（Dead → ops_requeue → 后续 `claim_due` 重新选中、attempts==1、status Ready、`last_error` 含 operator）；`ops_requeue_refuses_delivered_rows` / `ops_requeue_refuses_claimed_rows`（status 2/1 → false）；`ops_requeue_is_idempotent`（二次调用 false）；`ops_requeue_records_operator_in_last_error`。
- `pg.rs` db_tests（`#[ignore]`，`DATABASE_URL` 门控）：`ops_requeue_is_fenced_on_status_three_only`（真实 PG：dead 行 requeue 后可 claim 投递；delivered 行返回 false）。
- drill 三模式（§5 step 9）：`--token-skew`（s>60 future-iat / s<−360 exp+leeway → 0 POST、0 delivered、0 dead、行 requeue；−360≤s≤60 → 全投递）、`--sink-skew`（0≤s≤30 全投递；30<s≤300 自愈 `posts()==2·N`；s>300 与 s<0 → 401-after-refresh Transient 环）、`--check-expires-in-exp`（\|(iat+expires_in)−exp\| ≤60s 且 exp−iat ≥120s）。
- gauge：`metrics_tasks.rs` 30s 采样 `aero_audit_outbox_dead_rows` / `aero_audit_outbox_stuck_oldest_seconds`（SQL 形状 = drill stuck 计数拆分）；query Err 留旧值。
- 守卫：`git grep` 断言 relay/pg 无 `status = 3` → `status = 0` 自动写路径（status=3 仅出现在 mark_dead、ops_requeue、drill/gauge 只读查询）。

### 回归与守卫
- 既有 24 项（A1 6 + A2 7 + relay 内部 5 + PG ignored 1 + config 若干）随迁后全绿：`cargo test -p aero-audit-connector --all-targets`（+ `-- --ignored` 需 `DATABASE_URL`）。
- seam 本体：`cargo test -p aero-auth` 8 项零改动绿。
- 零 aero-auth/既有消费者改动：`git diff --stat crates/aero-auth crates/aero-server/src/integrations.rs crates/aero-server/src/snaplink_commercial/` 为空。
- 依赖面：`cargo tree -p aero-audit-connector` 仅 workspace crate + jsonwebtoken（已钉）；base64 直接依赖消失。

## 7. Risks / decisions

- **串匹配分类的脆弱性**：scope-deficient 与其余 `OidcError::Invalid` 同变体，靠 seam 固定文案区分。收敛为 `classify_validation_error` 单点 + 单测钉死；seam 文案变更时测试响亮失败。备选（改 aero-auth 加 typed error）违反零改动红线，不取。
- **签名验证强制化是行为变更**（原 "[PROPOSED]"/不验签）：真实 IdP 无 JWKS/发 opaque token → connector fail-closed 不投递——与 T-11 方向一致；staging 须验真实 JWKS 可达（AGENTS.md §4.5）。
- **token-endpoint 403 → dead 的误杀风险**：若真实 IdP 以 403 表临时限流而非未配给会误杀——按 acceptance 钉死 dead，staging 复核；401 保持 Transient。
- **expected_sub 语义收敛**：validator 强制 sub==client_id 后 `expected_sub` 只作 allowlist 条目；旧部署 env 需对齐（值 = client_id），staging 核对。
- **夹具共享决策**：connector 镜像（复制）`oidc/tests.rs` 夹具模式而非 aero-auth 导出 test-support——守零改动红线；第三消费者出现再议共享。
- **freshness floor（max token age）**：问题陈述提及、acceptance 未含 → 明确不做；若 v2 契约要求，`ClientCredentialsTokenConfig` 增字段属另一 direction。
- **37/37 仓外**：AC1 钉 in-repo 读法 = 8 个 oidc 测试；契约若给出更大矩阵，AC1 是同夹具追加用例的扩展点。
- **stub 的 `jsonwebtoken` 是常规依赖**（`pub mod stub` 在 lib 内）而非 dev——但 workspace 已钉、零新增供应链项，不影响门禁。
- **B5-1/B5-3/B5-4 并行**：0239 DDL 与配给门是并行 direction；本方向只交付分类与 dead 语义（403/scope-deficient → dead 的 enforcement point），配给门消费此语义。
- **静默停投（fail-closed 的盲区）**：时钟偏移越界（s>+60 future-iat / s<−360 exp+leeway / audit 服务时钟滞后）或 0239 缺失 → Transient 环 0 POST、0 dead——fail-closed 但不响亮。**无 dead 行 ≠ 无故障**。缓解：NTP（|app−IdP| ≤30s）+ expires_in↔exp staging 检查（§5 step 9）+ `aero_audit_outbox_stuck_oldest_seconds > 600` critical 告警（§2.7）。
- **dead 行的运维出口（F10 兜底）**：无 sweeper 是硬约束（reviewer 确认 no-resurrection 为既有不变量）；唯一出口 = §2.7 显式 ops requeue（fenced 3→0、attempts 归零、operator 审计痕迹、幂等）。403 误杀面（真实 IdP 以 403 表临时限流）由该路径恢复——requeue 既是出口也是误杀兜底。可选 body 检查 `unauthorized_client`（RFC 6749 §5.2）为未来低风险强化，本设计不做。
- **blast-radius 对账**：reviewer 的 per-row-at-claim-time 结论不变——requeue 行重回完整栅栏（token/lease/attempts/SKIP LOCKED），失败仍只影响自身；队列级 gauge/告警是只读观测，**永不自动 requeue**（自动 requeue = sweeper，禁止）；「全局可见性 + 逐行爆炸半径」成立。

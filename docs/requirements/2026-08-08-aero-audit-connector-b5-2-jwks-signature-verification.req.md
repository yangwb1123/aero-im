# Requirements Spec — JWKS signature verification for relay bearer tokens (close the `client.rs` [PROPOSED] seam)

- **Module (analysis root)**: `crates/aero-audit-connector` — 全部落地动作在此 crate（本方向的实际证据面；见 §1 模块标注勘误）
- **Direction**: "JWKS signature verification for relay bearer tokens (close the client.rs [PROPOSED] seam)"（value 7 / risk_reduction 9 / effort 6 / confidence 8）
- **Source analysis**: `docs/auto/analyses/crates-aero-ai-src-1bbf99ce.json`（direction #1）
- **Prior art（同 seam 的兄弟方向，范围不同）**: `docs/requirements/2026-08-08-aero-auth-b5-2-machine-token-verification-seam.req.md`（复用 `aero_auth::validate_client_credentials_token` 全量 8/8 契约）与 `docs/design/2026-08-07|08-08-aero-auth-b5-2-machine-token-verification-seam.design.md`。兄弟方向换**验证器整体**（含 typ/exp/iat/jti/sub==client_id 契约与 scope-deficient→dead 分类）；本方向只关**签名验证缺口**，不改 claims 契约面（§3 决策 D2 明示分界）
- **Status**: Requirements（全部引用经源码逐条复核）
- **Verification date**: 2026-08-08。行号是复核时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点（AGENTS.md §0）

## 1. Scope & module-label correction

| 项 | 结论 |
|---|---|
| 分析文件命名 | `crates-aero-ai-src-1bbf99ce.json` 含 3 个 direction；**选中者（#1 JWKS）的证据面全部在 `crates/aero-audit-connector`**。文件内 direction #0（L1 聚合，`crates/aero-ai/src/governance.rs`）与 #2（boot/readiness 门）**不在本 spec 范围** |
| 落地面 | `crates/aero-audit-connector/src/{client,config,relay,stub,fake}.rs` + `tests/{claim_validation,state_machine}.rs` + `src/bin/*-drill.rs`；`crates/aero-auth` 仅作**只读 seam 消费**（零生产改动，其公开类型可直接复用，见 §8；**勘误：D5 双平面需一处 additive 修正 `decoding_key_fallible`——见 claim-contract spec §0 AM-1，本 spec 相应放宽**） |
| 行为契约面 | **只加**：签名验证（trusted-JWKS 强制）+ 失败分类。**不动**：现有 iss/aud/scope/sub 四查（`validate_token_claims`）、transient/403/permanent 既有分类、状态机/租约/幂等语义 |

## 2. Evidence verification（direction 引用逐条核对，全部 ✅）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `client.rs:195-245` `AuditClient::validate_claims`，`:198` 标注 "JWKS verification is [PROPOSED]" | ⚠️ **符号名漂移（命中）**：实际符号是 **`validate_token_claims`**（`crates/aero-audit-connector/src/client.rs:199-263`；模块注释 :196-198 含 "JWKS verification is [PROPOSED]"）。函数 = 形状检查 :200-206 + iss :209-217 + aud :218-231 + scope :232-242 + sub :243-253，**纯函数、无签名验证**；`decode_jwt_claims` :416-435（base64url 手解，注释明言 "without signature verification"） |
| E2 | `client.rs:295-296` `client_credentials` grant | ✅ `request_token` :287-310，`:295` = `("grant_type", "client_credentials")`（+ scope + resource；non-200 → `bail!` → 经 `#[from]` 变 `DeliveryError::Transient`） |
| E3 | `relay.rs` — 403 → 立即 `mark_dead`（T-11 fail-closed） | ✅ `deliver_claim` Forbidden 臂 :202-214（注释明言 "Deliberately NOT `is_dead_at` … T-11"）；`PERMANENT_DEAD_AT = 2` :42 + `is_dead_at` :49；permanent 臂 :222（attempt 1 → `requeue`，attempt ≥ 2 → `mark_dead`）；Transient 臂 :233（`requeue`，永不 dead） |
| E4 | `fake.rs` — "fake sink records zero requests" | ⚠️ **措辞漂移（意图成立）**：`fake.rs` 是内存版 **`OutboxRepo`** 测试双（`FakeOutbox`/`FakeStatus`/`row()`），**不数 POST**；POST 计数在 `stub.rs::StubSink::posts()`（:116-118，`AtomicUsize`）。既有验收断言即 `posts() == 0`（如 `wrong_issuer_is_rejected_before_any_post`）——AC1 沿用此计量 |
| E5 | `tests/claim_validation.rs` claim-validation 套件 | ✅ 存在（实跑 `--list` 确认 **13**）：**12 async + 1 sync** = `wrong_issuer_is_rejected_before_any_post` / `missing_audience…` / `missing_audit_scope…` / `wrong_subject…` / `opaque_non_jwt_token_is_rejected_before_any_post` / `valid_token_is_accepted_and_delivery_proceeds` / `receipt_echoing_payload_uuid_event_id_is_accepted` / `receipt_echoing_base32_header_event_id_is_accepted` / `unauthorized_refreshes_once_and_retries_within_the_attempt` / `refreshed_token_must_repass_claim_validation_before_retry_post` / `client_classifies_statuses` / `payload_guard_rejects_tenant_selection_as_permanent` / `jwt_claims_decode_roundtrip`（sync）。**全部经 `stub.rs::make_jwt`（:242，`alg:none` + 假签名 `ZmFrZS1zaWduYXR1cmU`）驱动** |
| E6 | `tests/state_machine.rs` T-11 403→dead 状态机套件 | ✅ 存在：**7 tests** = `stale_token_cannot_ack_after_reclaim` / `backoff_is_bounded_and_exponential` / `permanent_error_dead_after_exactly_two_attempts`（422/409/receipt-mismatch → attempt1 requeue、attempt2 dead）/ `forbidden_dead_on_first_attempt`（T-11）/ `happy_path_settles_and_removes_from_claimable` / `skew_gt_lease_cannot_livelock_claim_fence_settle` / `priority_first_claim_preempts_fifo_and_limit1_keeps_top_lane`。同经 alg:none stub token |
| E7 | `docs/design/2026-08-07-aero-auth-b5-2-machine-token-verification-seam.design.md` | ✅ 存在（232 行；另有 08-08 更新版 design + req）。兄弟方向设计，含 JWKS 接线（`AERO_AUDIT_JWKS_URI`、`JwksKeyProvider`、stub 签名化、测试随迁清单）——本 spec 的 §8 实现路径直接锚定其已验证事实 |
| E8 | 支撑面（本 spec 补充复核） | `aero_auth` seam 公开可用：`KeyProvider` trait `oidc.rs:243`、`StaticKeyProvider::{single,with_keyed}` :252/:267、`JwksKeyProvider::new` :529（TTL 缓存 300s + unknown-kid 限速刷新 10s）、`OidcError` :220（`MalformedToken`/`UnknownKey`/`UnsupportedAlgorithm`/`Invalid`）、`validate_jwks_uri` :54；`aero-auth/src/lib.rs:27-31` 全部 re-export。root `Cargo.toml:133` 已钉 `jsonwebtoken = "9"`。`AuditClient::new` 构造点共 **12 处**：drill ×3（`src/bin/aero-audit-relay-drill.rs:125`、`aero-audit-t11-drill.rs:153`、`aero-audit-priority-drill.rs:201`）+ `relay.rs:316`（模块内测试）+ `state_machine.rs:63` + `claim_validation.rs` ×7（:76/:168/:200/:219/:249/:279/:298）。测试面实跑（`cargo test -p aero-audit-connector --all-targets -- --list`）：**lib 11**（config 2 + pg 3 门控 + relay 6）+ **claim_validation 13** + **state_machine 7** = 31（3 个 drill bin 0），其中 3 个 PG 门控 → 无 PG 可跑 28 |

**方向原始引用行号 vs 实际**：`validate_claims` :195-245 → `validate_token_claims` :199-263（名+行漂移）；`:198` [PROPOSED] 标注 → :196-198 模块注释；`:295-296` → :295。全部符号命中，无事实性出入。

## 3. Decision points（与兄弟方向的分界，本 spec 立场）

| # | 决策 | 依据 / 证据 |
|---|---|---|
| D1 | **签名失败 = permanent**（≤1 次重试 → dead），**永不 transient requeue** | direction acceptance 明示。**与兄弟方向 08-08 req/design 的 F-table 冲突**（其 "坏签名/unknown key → Transient requeue"）——本 direction 为后续/独立方向，**在 connector 内对本分类面取 supersede**；兄弟方向文档该行需相应勘误（housekeeping，非本 spec 要求） |
| D2 | **claims 契约面不动**（仍为 iss/aud/scope/sub 四查，无 typ/exp/nbf/iat/jti/sub==client_id 强制） | 本 direction 只关签名缺口；8/8 契约整体替换属兄弟方向（其 AC 明示 sub==client_id 收敛等行为变更）。两者可独立落地，但**同一文件区域冲突**（都改 `deliver` 前置校验）——若并行实施须合并为一个变更 |
| D3 | 签名面定义：**trusted-JWKS 内无匹配 key（unknown kid / key 不在集合）与签名不验（bad signature）同属签名失败类** | direction negative test 双形式："signed by a non-trusted JWKS key (or with valid claims but bad signature)"——两者都必须在 POST 前失败并走同一 permanent 分类。JWKS **下载/刷新不可用**（网络/HTTP 错/文档畸形）是**独立的 transient 平面**（机制暂不可用，非 token 无效；缓存刷新后重试可成功） |
| D4 | 验证时机：**每个将用于 POST 的 token**（含 401 刷新后的重验 token） | `deliver()` :120-190 现于每次 POST 前置 `validate_token_claims`（:128），401-refresh 后循环重验（:131-137）——签名检查接入同一位置即天然覆盖两路径 |

## 4. Threat-model boundary（诚实标注能力边界，非 scope 缺口）

签名验证证明的是 **token 由 trusted-JWKS 私钥持有者签发**，由此关闭：
- 伪造/形状匹配 token（无签名密钥的攻击者）——direction 主威胁；
- 被篡改的合法 token（改 claims 即坏签名）；
- 被攻破/误配 token endpoint 发出的、**用非信任密钥签名**的 token。

**不覆盖**（本 direction 不要求，兄弟方向契约面部分覆盖）：真实 IdP 私钥泄露（攻击者可签真 token）；合法 token 的泄露重放（无 exp/jti 状态检查）；`sub` 语义（保持 `expected_sub` 配置比对，非 client_id 自指）。

## 5. Requirements

### R1 — 签名验证强制化（关闭 [PROPOSED] seam）
`AuditClient` 对**每一个将用于 delivery POST 的 bearer token**（缓存命中、新获取、401-refresh 后重验三种来源）执行签名验证：token 的签名必须以**配置的 trusted-JWKS 密钥集**中某密钥验证通过，否则该 token 一律不得用于 POST（fail-closed，0 POST）。`client.rs:196-198` 的 "[PROPOSED]" 注释随之删除。

- **Rationale**: 现信任姿态 = "token 是 cc grant 换来的所以可信"（`client.rs:295`），方向问题陈述的主威胁（伪造/泄漏 shape-match token）仅靠结构校验不可防。
- **验证**: AC1（负例 0 POST）、AC2（正例投递）。

### R2 — 配置面：`AERO_AUDIT_JWKS_URI`（connector 启用时必填，boot fail-loud）
`RelayConfig` 增加 trusted-JWKS 来源；`from_env`（`config.rs:52-107`）解析 `AERO_AUDIT_JWKS_URI`，过 `aero_auth::validate_jwks_uri` 同款策略（HTTPS 或 loopback-HTTP、无 userinfo/query/fragment、长度有界），缺失/非法 → `bail!`（**relay 不启动、不 claim、不投递**）。测试/drill 用 `AuditClient::with_key_provider(config, Arc<dyn KeyProvider>)` 注入 `StaticKeyProvider::single(dec)`（不经网络）；`AuditClient::new` 生产路径由 URI 构造 `JwksKeyProvider`。

- **Rationale**: 签名验证必须有密钥来源；URI 非法在 boot 拒绝（fail-loud），与 `AERO_AUDIT_*` presence-gated 家族及 T-11 fail-closed 方向一致（兄弟设计 F12 同构）。
- **验证**: `config.rs` 单测三例（缺失/非法 URI/合法 URI）+ `from_env` 缺 URI 且其他 `AERO_AUDIT_*` 存在 → Err（现有 stray 检查不变）。

### R3 — 签名失败分类 = permanent（≤1 次重试 → dead），永不 transient
`DeliveryError` 新增 permanent 类（`PermanentKind` 新 variant，如 `SignatureRejected`）：签名失败（D3 定义的 unknown-kid/key 不匹配/坏签名/非白名单 alg）→ 走 relay 既有 permanent 臂（`relay.rs:222`）——attempt 1 `requeue`（backoff(1)=1s）、attempt ≥ 2 `mark_dead`（`is_dead_at`/`PERMANENT_DEAD_AT=2`），**绝不进入 Transient 无限 requeue**。JWKS 下载/刷新不可用保持 Transient（不投递、有界 backoff、下次 tick 重试）。

- **Rationale**: direction acceptance 明示；签名失败 = 身份面故障（伪造/密钥失配），重试不会自愈，与 403→dead 同为身份类 fail-closed，但保留 1 次重试预算（422 类路径）以容忍 JWKS 轮换传播的单窗抖动。
- **验证**: AC4（状态机双 attempt 断言）。

### R4 — claims 四查行为不变
iss/aud/scope/sub 校验逻辑（`validate_token_claims` :199-263）与失败分类不变：**claims 内容漂移仍为 Transient**（现 `client.rs:128-142` 行为；`relay.rs` 模块内测试 `transient_claim_drift_requeues_without_any_post` 回归钉死）。签名检查与之并存（先签名后 claims 或反之皆可，两者独立失败面）。

- **Rationale**: D2 分界；防止把 claims 漂移（IdP 配置问题，B5-4 修复）误判为永久。
- **验证**: AC3 回归 + `transient_claim_drift_requeues_without_any_post` 保持绿。

### R5 — 测试基建：stub 真签 RS256，负/正例夹具齐备
`stub.rs::make_jwt`（:242）从 alg:none 假签名改为 **RS256 真签**（`jsonwebtoken::encode`，workspace 已钉 "9"；测试 keypair + kid）；`SinkBehavior` 增加签名密钥选择（trusted key / 非 trusted key / 可选 tamper 模式；opaque 与 alg:none 保留为拒绝路径 fixture）。既有测试面（E5/E6 全部 + `relay.rs` 模块内 6 测试 + 3 个 drill bin）随迁到签名 token + `with_key_provider` 注入（构造点清单见 E8），**同一变更落地**，否则接受面不可测。

- **Rationale**: 现存 28 个无 PG 测试全部经 alg:none 假签名驱动（E5/E6/E8），无签名验证时它们不触及新代码路径；签名化是 AC1/AC2 可测的前提。
- **验证**: `cargo test -p aero-audit-connector --all-targets` 全绿（现有 **31 个测试**（28 无 PG + 3 PG 门控）基线 + 新增用例，`--list` 实跑确认）。

### R6 — 可观测信号保留
签名拒绝路径保留现有 `warn!(reason=…)` "no delivery attempted"（`client.rs:132-135`）并写入 `last_error`（`truncate_error` 有界）；`last_error` 含稳定哨兵子串（如 `"audit token signature rejected"`），使 dead 行可按哨兵 grep/复苏（`mark_dead` 行 `last_error` 现为 `"audit delivery classified permanent: {kind:?}"`，新 variant 的 Debug 输出需含可识别词）。

- **Rationale**: connector 零 metrics（兄弟方向 F10 范围外）；`last_error` + warn 是仅有两个可观测信号（兄弟 design 同结论）。
- **验证**: AC4 断言 `row.last_error` 含哨兵；`warn!(reason=…)` 保留（代码审查 + `transient_claim_drift…` 现有断言不回归）。

## 6. Acceptance criteria（direction 提供的 4 项，逐条可测化）

### AC1 — 负例：非信任密钥签名 / 坏签名 → POST 前失败，sink 零请求
新增 `tests/claim_validation.rs` 用例（沿用 `deliver_with_claims` 模式）：
- (a) stub 用**非 trusted keypair** 签发、claims 与 `SinkBehavior::default().token_claims` 相同（iss/aud/scope/sub 全匹配）→ `client.deliver(&claim())` 返回 `Err(DeliveryError::Permanent(PermanentKind::SignatureRejected))`（variant 名以 R3 实现为准，断言按实际命名），且 **`stub.posts() == 0`**；
- (b) trusted keypair 签发后**篡改签名段/claims**（坏签名）→ 同上，`posts() == 0`。

与既有 `wrong_issuer_is_rejected_before_any_post` 同一计量方式（`posts()==0` 字面断言），区别仅在失败原因从 claims 漂移变为签名失败。

### AC2 — 正例：真签 + 匹配 claims → 投递并 settle
新增用例：stub 用 trusted keypair 签发、claims = iss/aud/scope/sub 全匹配（D1 的 UUID 格式 payload + 合法 receipt）→ `deliver` 返回 `Ok(())`、`stub.posts() == 1`；经 relay（`FakeOutbox`）驱动时行状态 `FakeStatus::Delivered`（镜像 `happy_path_settles_and_removes_from_claimable` 断言集：claim_token/lease 清空、不再可 claim）。

### AC3 — 回归：既有套件全绿
- `tests/claim_validation.rs`（12 async + 1 sync，含 5 个 `posts()==0` 拒绝例、`valid_token_is_accepted_and_delivery_proceeds`、401-refresh 两例、D1 双格式 receipt 两例、`client_classifies_statuses`、`payload_guard…`）全部保持绿（随迁签名 token 后语义不变）；
- `tests/state_machine.rs` 7 例全绿，**T-11 `forbidden_dead_on_first_attempt`（403→attempt 1 dead）与 `permanent_error_dead_after_exactly_two_attempts`（422/409/receipt → ≤1 retry → dead）断言不改**；
- `relay.rs` 模块内 6 测试（含 `transient_claim_drift_requeues_without_any_post`）与 3 个 drill bin（`aero-audit-{relay,t11,priority}-drill.rs`）全绿。

### AC4 — 分类：签名失败走 422 类 ≤1-retry→dead，永不 transient
新增状态机用例（镜像 `permanent_error_dead_after_exactly_two_attempts` 结构，stub 用非 trusted key）：
- attempt 1：`dispatch_batch` → `FakeStatus::Ready`、`attempts == 1`、`available_at == t0 + backoff(1)`（fake 时钟钉死）、`last_error` 含签名哨兵；
- `make_due_now` 后 attempt 2：`FakeStatus::Dead`、`attempts == PERMANENT_DEAD_AT`、`is_dead_at` 为真、**不再可 claim**；
- 显式断言**非 Transient 语义**：`last_error` 不含 transient 文案，且无 `claim_token` 轮换重试循环（attempt 2 即终态）。

## 7. Out of scope（红线）

- claims 契约扩展（typ/exp/nbf/iat/jti/sub==client_id 强制、scope registry）——兄弟方向；
- `client_allowlist`（兄弟设计 [PROPOSED]）；token-endpoint 403 → dead、scope-deficient → dead 等**兄弟方向的分类变更**；
- metrics/gauges/告警（兄弟设计 F10 面）；`crates/aero-ai/src/*`（同分析文件 direction #0/#2）；aero-auth 生产代码任何改动（只读消费；**唯一例外 = additive `decoding_key_fallible`，见 claim-contract spec §0 AM-1**）；
- JWKS URI 可达性 boot 探测（staging 项，AGENTS.md §4.5「待联调」口径）。

## 8. Implementation notes（锚定已验证 seam）

**推荐路径（最小面、复用已测 seam 类型）**：connector 保留手写 4 查 `validate_token_claims`（R4），在其前置/并存处加入签名验证——`jsonwebtoken::decode`（alg 白名单 RS256，`Validation::new(RS256)` + issuer/audience 关闭以免与四查重复）+ `aero_auth::{KeyProvider, StaticKeyProvider, JwksKeyProvider}` 作密钥源（trait :243 公开、`JwksKeyProvider::new` :529 带 TTL 缓存与 unknown-kid 限速刷新，`StaticKeyProvider::single` :252 供测试）。`AuditClient` 增 `with_key_provider(config, Arc<dyn KeyProvider>)`；`new` 生产路径由 `config.jwks_uri` 构造 provider（fail-loud 已在 R2）。签名失败按 D3 分类映射到新 `PermanentKind` variant；relay 无需改动（permanent 臂 :222 泛化处理）。备选路径 = 兄弟方向整体换 `aero_auth::validate_client_credentials_token`（8/8 契约）——分类冲突见 D1/D3，二选一，不可并存。

**随迁清单（构造点）**：drill ×3（relay-drill.rs:125 / t11-drill.rs:153 / priority-drill.rs:201，各自注入 stub 的 trusted `StaticKeyProvider`——`stub.rs` 需随 R5 增加签名密钥暴露）、`relay.rs:316` 模块内 3 个 transient 用例（否则 JWKS 不可达 → Transient → `posts()==0` 撞现有 `posts()==1` 断言）、`state_machine.rs:63`、`claim_validation.rs` 全部 7 个 `AuditClient::new` 点（:76/:168/:200/:219/:249/:279/:298）。`SinkBehavior` 默认夹具（`stub.rs:45-52`，sub=`"aero-im.source"`）在 R5 签名化时**保持 claims 不变**（本方向不动 `expected_sub` 语义）；新增密钥选择字段与 tamper 模式。

**文件级锚点（grep 用）**：`client.rs::{validate_token_claims, decode_jwt_claims, request_token, deliver}` · `config.rs::{RelayConfig, from_env}` · `relay.rs::{deliver_claim, is_dead_at, PERMANENT_DEAD_AT}` · `stub.rs::{make_jwt, SinkBehavior, StubSink::posts}` · `fake.rs::{FakeOutbox, FakeStatus}` · `oidc.rs::{KeyProvider, StaticKeyProvider, JwksKeyProvider, OidcError, validate_jwks_uri}`。

# Design — JWKS signature verification for relay bearer tokens (close the `client.rs` [PROPOSED] seam)

- **Module**: `crates/aero-audit-connector`（落地主体）+ `crates/aero-auth`（只读 seam 消费 + **一处受控的 additive 修正**，见 D5）
- **Requirements**: `docs/requirements/2026-08-08-aero-audit-connector-b5-2-jwks-signature-verification.req.md`（D1–D4/R1–R6/AC1–AC4 全部采纳）
- **Status**: Design（证据逐条复核完成；含 2 项 spec 未覆盖的新发现，见 §0.2）

## 0. Evidence verification ledger（untrusted claims → repo reality）

> 全部引用以源码/`cargo test --list` 现场复核（2026-08-08）。行号为复核时锚点，可能漂移；符号是稳定锚。

### 0.1 逐条核对（10/10 命中，措辞/行号漂移与 spec 自述一致）

| # | Claim | Verdict |
|---|---|---|
| E1 | `client.rs:195-245` `validate_claims`，`:198` [PROPOSED] | ✅ 符号漂移属实：`validate_token_claims` `client.rs:199-263`（形状 :200-206 + iss/aud/scope/sub 四查，纯函数无验签）；`:196-198` 模块注释含 "JWKS verification is [PROPOSED]"；`decode_jwt_claims` 实际在 **:440**（spec 引 :416-435，行漂移） |
| E2 | `client.rs:295-296` cc grant | ✅ `request_token` :287-310，`:295` = `("grant_type", "client_credentials")` |
| E3 | relay 403→`mark_dead` T-11 | ✅ `relay.rs` Forbidden 臂（:207-215，"Deliberately NOT is_dead_at…T-11" 注释）；`PERMANENT_DEAD_AT=2` :42、`is_dead_at` :48-49；Permanent 臂 :216-230（spec 引 :222，漂移）、Transient 臂 :232-244（:233，漂移） |
| E4 | `fake.rs` = OutboxRepo double | ✅ `FakeOutbox` :89 / `FakeStatus` :30（Ready=0/Claimed=1/Delivered=2/Dead=3）/ `row()` :175 / `set_now` :111 / `make_due_now` :164；POST 计数在 `stub.rs::StubSink::posts()` **:122**（spec 引 :116-118，漂移） |
| E5 | claim_validation 套件 13 | ✅ `cargo test --list` 实跑 13（12 async + 1 sync），名单与 spec 逐字一致 |
| E6 | state_machine 套件 7 | ✅ 实跑 7，名单与 spec 一致（含 `forbidden_dead_on_first_attempt` T-11） |
| E7 | 兄弟 design 232 行 | ✅ `docs/design/2026-08-07-aero-auth-b5-2-machine-token-verification-seam.design.md` = 232 行（另有 08-08 版 364 行） |
| E8 | aero-auth seam | ✅ `KeyProvider` trait :243（**async** `decoding_key(kid, alg) -> Option<DecodingKey>`）；`StaticKeyProvider` :252、`single` :262、`with_keyed` :272；`JwksKeyProvider` :529、`new` :571（spec 引 :529，漂移）、TTL 300s + unknown-kid 限速 10s 属实（`DEFAULT_JWKS_CACHE_TTL`/`UNKNOWN_KID_REFRESH_MIN_INTERVAL`）；`OidcError` :220（MalformedToken/UnknownKey/UnsupportedAlgorithm/Invalid）；`validate_jwks_uri` :54；`aero-auth/src/lib.rs:27-31` 全量 re-export；root `Cargo.toml:133` `jsonwebtoken = "9"`（**在 `[workspace.dependencies]`**，`rsa = "0.9"` 亦 workspace 钉） |
| E9 | `AuditClient::new` 12 处 | ✅ grep 逐点确认：drill ×3（relay-drill:125 / t11-drill:153 / priority-drill:201）+ relay.rs:316 + state_machine.rs:63 + claim_validation ×7（:76/:168/:200/:219/:249/:279/:298） |
| E10 | 测试总量 31 = lib 11 + 13 + 7，bins 0 | ✅ 实跑：lib 11（config 2 + pg 3 门控 + relay 6）+ 13 + 7 = 31，3 PG 门控 → 无 PG 可跑 28。与 spec E8 完全一致 |

### 0.2 新发现（spec 未覆盖，本设计必须处理）

- **N1 — `KeyProvider` trait 的 Option 塌缩使 D3 双平面不可实现（最重要）**：`JwksKeyProvider::decoding_key`（oidc.rs:709-727）在 JWKS **fetch/refresh 失败**时 `warn!` 后返回 `None`，与「key 真不在集合」（unknown kid）在 trait 层**不可区分**。spec D3 要求 unknown-kid → permanent、fetch 不可用 → transient——经现 seam 无法表达。若直接映射 `None → Permanent`，JWKS 网络故障会把全表行 ≤1 次重试后 dead（数据丢失面）；若 `None → Transient` 则 AC1（非信任密钥必须 Permanent）不可满足。**解决：D5 新增 fallible 解析面**（aero-auth 一处 additive trait 方法），否则只能复制 JWKS 客户端（约 100 行 + 策略再推导）。
- **N2 — token 缓存投毒使 ≤1-retry 预算虚设**：`access_token()` 缓存 token 至 TTL（300s）。若缓存的 token 签名失败（如 IdP 轮换后旧 key 签的、或端点被攻破后发的坏 token），attempt 1 失败 → requeue（1s）→ attempt 2 `access_token()` 返回**同一个坏 token** → 再败 → dead。**重试从未拿到新 token**。现状 claims 失败路径已 `invalidate_token`（relay 测试断言 `claim_token != token_a` 轮换）；签名拒绝路径必须同样失效缓存（D7），否则「容忍轮换传播单窗抖动」的预算不成立。
- **N3 — 兄弟方向构造点清单已过期**：08-08 兄弟 design 引 10 处（claim_validation 5、state_machine:57、priority-drill:198、relay.rs:311）；实仓 12 处（7/63/201/316）。两方向若并行，**必须用本设计的 12 处现网清单**合并，否则漏迁。
- **N4 — opaque/畸形 token 的分类取决于校验顺序**：`opaque_non_jwt_token_is_rejected_before_any_post` 夹具实际是「payload 为 JSON 字符串的合法形状 JWT」（`json!("opaque-bearer-token")`）。签名先验 → jsonwebtoken 解码失败 → Permanent；claims 先验 → 无 iss → Transient（今日行为）。AC3 只钉 `Err + posts()==0`，两种顺序都绿；但 relay 级语义不同（D6 裁决）。
- **N5 — `RelayConfig` 新字段破坏 6 处字面量构造**：drill ×3（relay-drill:83 / t11-drill:113 / priority-drill:138）+ relay.rs test_config :265 + state_machine.rs:40 + claim_validation.rs:17。spec §8 只列了 `AuditClient::new` 12 处；`RelayConfig` 字面量 6 处同样编译驱动。
- **N6 — 依赖边**：connector 现**不依赖** aero-auth（仅 aero-common）。需新增 `aero-auth.workspace = true` + `jsonwebtoken.workspace = true`（dev 加 `rsa`）。aero-auth 为基础层、不依赖 connector，无环；均为 workspace 已钉版本，零新供应链条目。
- **N7 — 生产 boot 确认 fail-loud 路径**：`main.rs:251-265`：`from_env` Err → 整个 server boot 失败。`AERO_AUDIT_JWKS_URI` 必填后，**connector 启用但缺该 env → 服务器拒启**（与既有 `AERO_AUDIT_*` 族一致）；运维侧 rollout 顺序见 §5。

## 1. 设计决策（含 spec D1–D4 采纳 + 新 D5–D8）

| # | 决策 | 依据 |
|---|---|---|
| D1 | 签名失败 = permanent（attempt 1 requeue → attempt ≥2 dead），**永不 transient**；supersede 兄弟方向「坏签名/unknown key → Transient」行（其文档需勘误，housekeeping） | spec 采纳；兄弟 req :103 冲突已核实 |
| D2 | claims 契约面不动（iss/aud/scope/sub 四查，无 typ/exp/nbf/iat/jti 强制） | spec 采纳；8/8 换验证器属兄弟方向，两者同区冲突须合并（N3 清单） |
| D3 | 签名面 = unknown-kid / key 不在 trusted 集 / 坏签名 / 非白名单 alg / 结构性非法 JWT；JWKS **fetch/刷新不可用** = 独立 transient 平面 | spec 采纳，**但经现 seam 不可表达 → D5** |
| D4 | 验证时机：每个将用于 POST 的 token（缓存命中 / 新获取 / 401-refresh 重验） | spec 采纳；挂在 `deliver()` 循环内即天然覆盖 |
| **D5** | **aero-auth `KeyProvider` 增 additive fallible 方法** `decoding_key_fallible(&self, kid, alg) -> Result<Option<DecodingKey>, String>`，默认实现委托 `decoding_key`（`Ok(...)`）；`JwksKeyProvider` 覆写：fetch/refresh 失败 → `Err(msg)`（复用 `safe_jwks_request_error` 文案），key 不在集 → `Ok(None)`。connector 映射 `Err → Transient`、`Ok(None)/验签失败 → Permanent(SignatureRejected)` | N1。这是 D3 双平面唯一不复制 JWKS 客户端的路径；additive 默认实现 = 零行为变化（StaticKeyProvider 及任何外部 impl 不用改）。**spec §7「aero-auth 零生产改动」需相应放宽为本项**（约 15 行 + 3 单测）；备选 B：connector 自持 fetch（复制 ~100 行 + URI/size/限速策略），备选 C（塌缩成单类）被拒——违反 AC1 或 R3 |
| **D6** | 校验顺序 = **签名先、claims 后**（同一循环迭代内）。结构性失败（opaque/畸形/alg:none/非 RS256）→ `Permanent(SignatureRejected)`——opaque 夹具从今日 Transient **有意改判** Permanent | 方向主威胁是 shape-match 伪造；结构性坏 token 重试不会自愈，且 attempt-2 轮换新 token 是唯一解药。AC3 各例只钉 `Err + posts()==0`（opaque 例）或已签好 token 的 claims 漂移（仍 Transient），全绿不受影响。保守替代（claims 先验保 opaque→Transient）在 §7 注明，二选一不可混 |
| **D7** | 签名拒绝必须 `invalidate_token`（与 claims 路径同构） | N2。否则 attempt 2 复用投毒缓存 token，≤1-retry 预算虚设、轮换窗口不闭合 |
| **D8** | jsonwebtoken `Validation`：`algorithms=[RS256]`、`validate_exp=false`、`validate_nbf=false`、`required_spec_claims=[]`、`validate_aud=false`（默认）——签名只管签名 | R4/D2：exp/nbf/aud/iss 校验属于 4 查契约面（现 4 查不查 exp），jsonwebtoken 只做密码学验证，避免契约面被悄悄扩大 |

## 2. API changes（精确签名）

### 2.1 aero-auth（D5，唯一跨 crate 改动，additive）

```rust
// crates/aero-auth/src/oidc.rs — KeyProvider trait（:243 原位扩）
#[axum::async_trait]
pub trait KeyProvider: Send + Sync {
    async fn decoding_key(&self, kid: Option<&str>, algorithm: Algorithm) -> Option<DecodingKey>;

    /// Fallible resolution: `Err` = mechanism unavailable (JWKS fetch/refresh
    /// failure — transient plane); `Ok(None)` = no compatible key (genuine
    /// unknown-kid — permanent plane). Default impl keeps every existing
    /// provider behavior-identical.
    async fn decoding_key_fallible(
        &self,
        kid: Option<&str>,
        algorithm: Algorithm,
    ) -> Result<Option<DecodingKey>, String> {
        Ok(self.decoding_key(kid, algorithm).await)
    }
}
```
`JwksKeyProvider` 覆写（唯一行为变化点）：`cached_set`/refresh 的 `Err(e)` 分支返回 `Err(e)`；`key_from_set` miss 返回 `Ok(None)`；**D10 绕过臂（合并 AM-5 受控放宽）**：同 kid 窗内重试至多一次绕过刷新（last-missed 单槽 + 强制占用刷新窗）——见合并 design §4 API-4 与合并 req §0 AM-5。`StaticKeyProvider` 走默认实现，零改动。

### 2.2 aero-audit-connector

```rust
// client.rs
pub enum PermanentKind {
    Unprocessable, Conflict, ReceiptMismatch, PayloadGuard,
    /// Signature-plane rejection: unknown kid / key not in the trusted set /
    /// bad signature / disallowed alg / structurally-invalid JWT.
    SignatureRejected,          // ← 新 variant（Debug 即 R6 哨兵）
}

pub struct AuditClient {
    config: RelayConfig,
    http: reqwest::Client,
    token_cache: Mutex<Option<CachedToken>>,
    keys: Arc<dyn aero_auth::KeyProvider>,   // ← 新字段
}

impl AuditClient {
    /// 生产：`with_key_provider(config, Arc::new(JwksKeyProvider::new(config.jwks_uri.clone())))`
    pub fn new(config: RelayConfig) -> anyhow::Result<Self>;   // 签名不变 → main.rs:257 零改动
    /// 测试/drill 注入面（12 处构造点全迁）
    pub fn with_key_provider(config: RelayConfig, keys: Arc<dyn aero_auth::KeyProvider>)
        -> anyhow::Result<Self>;

    /// 签名平面（D6：先于 validate_token_claims 执行）：
    /// 1. `jsonwebtoken::decode_header` 失败 / alg ∉ {RS256} → Permanent(SignatureRejected)
    /// 2. `keys.decoding_key_fallible(kid, RS256)`：
    ///    Err(e) → Transient(anyhow!("audit jwks unavailable: {e}"))   // D3 transient 平面
    ///    Ok(None) → Permanent(SignatureRejected)
    /// 3. `jsonwebtoken::decode::<Value>(token, &key, &validation)`（D8 Validation）：
    ///    失败 → Permanent(SignatureRejected)
    /// 4. 通过 → Ok(())
    async fn verify_token_signature(&self, token: &str) -> Result<(), DeliveryError>;
}

// config.rs — RelayConfig 增字段
pub struct RelayConfig {
    // …既有字段不变…
    /// Trusted-JWKS source (R2): required when the connector is enabled.
    pub jwks_uri: String,
}
// from_env：`AERO_AUDIT_JWKS_URI` required + `aero_auth::validate_jwks_uri(&raw)`
// + trim/长度 ≤ MAX_ENDPOINT_BYTES(2048)；缺失/非法 → bail!（fail-loud，main.rs 拒启）
// stray 检查天然覆盖：TOKEN_ENDPOINT 缺失而 JWKS_URI 存在 → 既有 AERO_AUDIT_* 计数 Err。

// deliver() 循环（:122-190）改造 —— 唯一行为面：
loop {
    self.verify_token_signature(&token).await?;        // 新，D6 先
    self.validate_token_claims(&token).map_err(|rejection| {  // 既有，原样
        self.invalidate_token(&token).await;
        warn!(reason = %rejection.reason, "audit token failed claim validation; no delivery attempted");
        DeliveryError::Transient(anyhow!("audit token claim validation failed: {}", rejection.reason))
    })?;
    …POST/分类/401-refresh 逻辑零改动…
}
// verify_token_signature 内部：签名拒绝（Permanent 分支）返回前同样
// `self.invalidate_token(&token).await`（D7）——放 `deliver` 的 map_err 处或
// verify 内部均可，单点实现 + 单测钉死。
```

`relay.rs` **零改动**（Permanent 臂 :216 对 `PermanentKind` 泛化，`format!("audit delivery classified permanent: {kind:?}")` → 哨兵 `SignatureRejected` 自动入 `last_error`，R6 满足）。

### 2.3 stub.rs（R5 测试基建）

```rust
/// 签名密钥选择（R5）：trusted = 内嵌 keypair 真签（RS256, kid "test-audit-1"）；
/// untrusted = 另一 keypair 签（claims 全匹配）；tamper = trusted 签后篡改签名段；
/// alg_none = 保留 alg:none 拒绝路径夹具。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigningMode { Trusted, Untrusted, Tamper, AlgNone }

pub struct SinkBehavior {
    // …既有字段不变（token_claims/token_claims_after_first/events_status/…）…
    pub signing: SigningMode,        // ← 新，默认 Trusted
}

// make_jwt(claims) 语义变为「trusted RS256 真签」（1 处调用方 jwt_claims_decode_roundtrip
// 只数 segment==3，天然兼容）；新增 make_jwt_mode(claims, mode) 供负例夹具。
// 新增公开访问器（drill + StaticKeyProvider 注入用）：
pub fn decoding_key(&self) -> jsonwebtoken::DecodingKey;   // 内嵌 keypair 公钥侧
```
密钥来源：`rsa`(workspace "0.9", pem) + `EncodingKey::from_rsa_pem` 内嵌 PEM 常量（或 `include_bytes!`），header 带 `kid: "test-audit-1"`、`alg: RS256`。

### 2.4 依赖（Cargo.toml）

```toml
# aero-audit-connector/Cargo.toml
[dependencies]
aero-common.workspace = true
aero-auth.workspace = true          # ← 新（N6；基础层，无环）
jsonwebtoken.workspace = true       # ← 新（root :133 已钉 "9"）
[dev-dependencies]
rsa.workspace = true                # ← 新（stub keypair，root 已钉 "0.9" pem）
```

## 3. Compatibility constraints

1. **部署序**：`AERO_AUDIT_JWKS_URI` 在 connector 启用时变必填。先布 env 再布 binary——否则 server boot 拒启（N7 已验证路径），fail-loud 而非静默降级。无 DB 迁移、无 web/NATS/总线改动。
2. **`RelayConfig` 字段增** → 6 处字面量（N5）+ `from_env` 编译驱动更新；全在 crate 内/测试内，无外部消费方（grep 确认 `RelayConfig` 仅 connector crate 与 main.rs 使用）。
3. **`AuditClient::new` 签名不变**（委托 with_key_provider）→ `main.rs:257` 零改动；12 处测试/drill 构造点全迁 `with_key_provider(StaticKeyProvider::single(stub.decoding_key()))`（N3 现网清单）。
4. **`KeyProvider` additive 方法**：既有 impl 全零改动；`JwksKeyProvider` 行为变化仅限新方法（旧 `decoding_key` 语义不变，aero-auth 既有调用方 `validate_id_token` 不受影响）。
5. **claims 四查 + 分类 + 状态机/租约/幂等**：逐字节不变（R4）；relay Permanent 臂已泛化，无需动。
6. **既有 28 个无 PG 测试**：全部随 stub 签名化在同一变更迁入（R5）——alg:none → RS256 后，凡走 `deliver` 的测试自动过新签名路径；`jwt_claims_decode_roundtrip`（3 segment 断言）不受影响；`transient_claim_drift_requeues_without_any_post`（签好但 wrong-iss）语义不变。
7. **兄弟方向互斥**：分类冲突（D1 supersede）+ 同文件区（`deliver` 前置校验、`config.rs::from_env`、`stub.rs::make_jwt`、12+6 构造点）+ 过期清单（N3）——并行实施必须合并为一个变更；合并基线用本设计的现网清单。

## 4. Failure modes（新增面，含缓解）

| # | 失败 | 分类 | 行为 | 缓解 |
|---|---|---|---|---|
| F1 | `AERO_AUDIT_JWKS_URI` 缺失/非法 | boot | server 拒启（main.rs:262-265） | R2 校验 + 部署序（§3.1）；`validate_jwks_uri` 同款策略（HTTPS/loopback-HTTP、无 userinfo/query/fragment、≤2048） |
| F2 | JWKS 网络/HTTP/畸形文档（fetch 或 unknown-kid 强制刷新） | **Transient**（D3/D5） | 不投递、`requeue` 有界 backoff、永不 dead；provider 自带 warn；下次 tick 自动恢复 | `decoding_key_fallible` 的 `Err` 面；测试用 `FailingKeyProvider` 双钉 |
| F3 | unknown kid / key 不在集 / 坏签名 / alg ∉ RS256 / 结构性非法 | **Permanent**（D1/D3/D6） | attempt 1 requeue(1s) → attempt 2 dead；`last_error` 含哨兵 `SignatureRejected` | D7 缓存失效 → attempt 2 拿新 token；provider unknown-kid 限速刷新（10s）在 attempt 1 已触发；≤1-retry 预算 = 轮换传播单窗 |
| F4 | IdP 轮换：旧 key 签的缓存 token | Permanent（F3 路径） | 同上 | D7 + F3 缓解；JWKS 新集在 attempt 1 已刷入，attempt 2 新 token 即过 |
| F5 | `JwksKeyProvider::new` 策略失败 URI（infallible 构造，保留无目标 provider） | Transient 循环 | 每次查找 `Err("jwks URI rejected by policy")` → 无限 requeue | R2 已在 boot 拒掉，生产不可达；测试/`with_key_provider` 永不触 |
| F6 | 真实 IdP 私钥泄露 / 合法 token 重放 | 不可检测（§4 threat-model 边界） | — | 明确出范围：JWKS 验证证明「trusted 私钥持有者签发」，不覆盖私钥泄露与无状态重放（兄弟方向契约面部分覆盖 exp/jti） |
| F7 | 签名风暴 dead 行（F3 高频） | 可观测 | 行 dead + warn | R6：`last_error` 哨兵 grep/复苏；`warn!(reason=…)` 保留 |

## 5. Migration steps（每步带可执行门禁；steps 2–8 一个原子变更）

0. **基线**：`cargo test -p aero-audit-connector --all-targets` → 28 passed / 3 ignored（31 清单核对）；`git status` 干净。
1. **aero-auth（D5）**：trait + 默认方法 + `JwksKeyProvider` 覆写 + 单测（fetch 失败→`Err`、kid 不在集→`Ok(None)`、kid 命中→`Ok(Some)`）。门禁：`cargo test -p aero-auth`。
2. **connector 依赖**：Cargo.toml 增 aero-auth/jsonwebtoken（dev: rsa）。门禁：`cargo check -p aero-audit-connector`（暂未用，绿）。
3. **config.rs**：+`jwks_uri` 字段、from_env 解析（必填 + `validate_jwks_uri` + ≤2048）、单测 3 例（缺 URI 且其他 `AERO_AUDIT_*` 在 → Err；非法 URI → Err；合法 → Ok）。门禁：`cargo test -p aero-audit-connector --lib config::`。
4. **client.rs**：`PermanentKind::SignatureRejected`、`keys` 字段、`with_key_provider`、`new` 委托、`verify_token_signature`、`deliver` 循环接入（签名先 claims 后 + D7 失效）、删 `:196-198` [PROPOSED] 注释。门禁：编译过（既有测试**暂红**——stub 仍 alg:none，预期中间态，勿在此停）。
5. **stub.rs（R5）**：内嵌 keypair、`make_jwt` 真签（kid `test-audit-1`）、`SigningMode`/`make_jwt_mode`、`decoding_key()` 访问器。门禁：编译过。
6. **随迁**：12 处 `AuditClient::new` → `with_key_provider(..., Arc::new(StaticKeyProvider::single(stub.decoding_key())))`（relay.rs:316、state_machine.rs:63、claim_validation ×7、drill ×3——用 N3 现网清单，**勿信兄弟文档的 10 处**）；6 处 `RelayConfig` 字面量 + `jwks_uri: "https://idp.example.test/jwks".into()` 占位（with_key_provider 下永不 fetch）。门禁：**既有 31 全绿恢复**（28 run + 3 ignored）——回归基线。
7. **新增用例**（§6 全表）。门禁：新增全绿。
8. **drill 验收**：`aero-audit-{relay,t11,priority}-drill` 三 bin 跑绿（A3 门）。
9. **全量门禁**：`cargo check --workspace` · `cargo test --workspace --lib`（PG 门控 + `-- --ignored` 需 `DATABASE_URL`+已迁移）· `cargo clippy --workspace --all-targets`（零新警告）· `scripts/{truth-check,file-size-check}.sh`（web 无改动，web-check 可略或照跑）。
10. **housekeeping**：兄弟方向 req/design（08-08 aero-auth-b5-2）签名分类行（:103/:207）按 D1 勘误；本 design 归档。

## 6. Testable acceptance mapping（AC1–AC4 → 具体用例）

> 断言四件套：`stub.posts()` / `FakeStatus` / `attempts` / `claim_due` 迭代（与既有 `assert_transient_requeue`/`permanent_error_dead_after_exactly_two_attempts` 同构；fake 时钟钉死，`make_due_now` 推进）。

| 验收 | 用例（tests/…） | 断言要点 |
|---|---|---|
| **AC1(a)** 非信任密钥签、claims 全匹配 | claim_validation: `untrusted_key_signature_is_rejected_before_any_post`（`SinkBehavior { signing: Untrusted, ..default }`） | `matches!(outcome, Err(Permanent(SignatureRejected)))` + `posts() == 0` |
| **AC1(b)** trusted 签后篡改签名段 | claim_validation: `tampered_signature_is_rejected_before_any_post`（`signing: Tamper`） | 同上 |
| AC1 补（D3 alg 面） | claim_validation: `alg_none_token_is_rejected_before_any_post`（`signing: AlgNone`） | 同上（非白名单 alg 归签名面） |
| **AC2** 真签 + claims 匹配 | claim_validation: `trusted_signature_delivers_and_settles`（client 层：`Ok(())` + `posts() == 1`）；state_machine 或 claim_validation relay 层（`FakeOutbox`）：行 `FakeStatus::Delivered`、`claim_token == None`、`lease_expires_at == None`、`claim_due` 不再返回（镜像 `happy_path_settles_and_removes_from_claimable`） | 双层次 |
| **AC3** 回归 | 既有 13 + 7 + relay 6 + config 2 + drill 3 全绿（steps 6 门禁）；**T-11 `forbidden_dead_on_first_attempt` 与 `permanent_error_dead_after_exactly_two_attempts` 断言不改**；`transient_claim_drift_requeues_without_any_post` 语义保持（签好但 wrong-iss → Transient、`posts()==0`、fresh-token 轮换） | 基线恢复即通过 |
| **AC4** 签名失败 ≤1-retry → dead、永不 transient | state_machine: `signature_rejected_dead_after_exactly_two_attempts`（`signing: Untrusted`，fake 时钟 t0）：attempt 1 `dispatch_batch` → `Ready`、`attempts == 1`、`available_at == t0 + backoff(1) = t0 + 1s`、`last_error` 含 **`SignatureRejected`** 哨兵；`make_due_now` 后 attempt 2 → `Dead`、`attempts == PERMANENT_DEAD_AT(2)`、`is_dead_at` 真、不再可 claim；显式非 transient 断言：`last_error` 不含 transient 文案、attempt 2 即终态（无 claim_token 轮换重试循环） | 镜像 `permanent_error_dead_after_exactly_two_attempts` 结构 |
| **R2** | config: `jwks_uri_missing_fails_loud`（其他 `AERO_AUDIT_*` 在而 URI 缺 → Err）/ `jwks_uri_illegal_fails_loud` / `jwks_uri_legal_parses` | from_env 三例 |
| **R3/D5** transient 平面 | claim_validation 或 state_machine: `jwks_fetch_failure_requeues_transient`（测试双 `FailingKeyProvider` 恒 `Err` → deliver `Err(Transient)`、`posts()==0`；relay 层 requeue、`attempts==1`、**非 dead**） | D5 seam 的 payoff |
| **R4** | `transient_claim_drift…` 保持绿（既有）+ `refreshed_token_must_repass_claim_validation_before_retry_post` 保持绿（401 刷新 token 先过签名再过 claims，posts==1 断言不变） | 四查面零行为变化 |
| **R6** | AC4 断言 `last_error` 含哨兵；`warn!(reason=…)` 保留（代码审查 + 既有 transient 断言不回归） | 可观测 |

## 7. Out of scope（红线重申 + 一处注明）

- claims 契约扩展（typ/exp/nbf/iat/jti/sub==client_id、scope registry）→ 兄弟方向；`client_allowlist`、token-endpoint 403→dead、scope-deficient→dead → 兄弟方向分类面。
- metrics/gauges/告警（兄弟设计 F10）；`crates/aero-ai/src/*`；JWKS URI 可达性 boot 探测（staging「待联调」）。
- 真实 IdP 私钥泄露 / 合法 token 无状态重放（threat-model §4，诚实边界）。
- **D6 保守替代**（若评审否决 opaque 改判 Permanent）：claims 先验、签名后验——opaque/畸形保持今日 Transient（R4 字面最保守），其余分类不变；两序在全部既有夹具 + AC1/AC2/AC4 上结果一致，仅 opaque 例分类不同。**二选一，不可混**；本设计主推签名先验（方向主威胁是 shape-match 伪造，结构性坏 token 应快速 dead 而非无限 requeue）。
- aero-auth 改动面：**仅 D5 的 additive trait 方法 + JwksKeyProvider 覆写 + 单测**；其余 aero-auth 生产代码不动。

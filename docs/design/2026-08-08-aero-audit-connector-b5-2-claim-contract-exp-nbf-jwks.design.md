# Design — 补全 claim 契约：exp/nbf 时间 claims 校验 + [PROPOSED] JWKS 签名验证（fail-closed 语义）

- **Module (analysis root)**: `crates/aero-audit-connector`（`src/{client,config,relay,stub}.rs` + `tests/claim_validation.rs` + `src/bin/*-drill.rs`）
- **Spec**: `docs/requirements/2026-08-08-aero-audit-connector-b5-2-claim-contract-exp-nbf-jwks.req.md`（R1–R7 / AC1–AC5）
- **Sibling（同 seam，合并实施）**: `docs/design/2026-08-08-aero-audit-connector-b5-2-jwks-signature-verification.design.md`（纯 JWKS 方向：必填 fail-loud + 签名拒绝 permanent；本设计在其 D1/D3/D5/D6 裁决之上叠加 exp/nbf 半场，两方向统一为一个实现）
- **Status**: Design（证据全部复核，见 §1）

## 1. Evidence verification（untrusted claims → 源码复核判决）

| # | Claim | Verdict |
|---|---|---|
| V1 | `client.rs::validate_token_claims` 只做 iss/aud/scope/sub 四查，无 exp/nbf、无签名验证；`[PROPOSED]` 注释在 :198 | ✅ 成立。fn 在 :199；形状 :201-207 → iss :209-217 → aud :218-231 → scope :232-242 → sub :243-253；doc comment :196-198 明言 "signature is *not* verified … JWKS verification is **[PROPOSED]**" |
| V2 | `decode_jwt_claims` 只解 payload、从不校验签名段 | ✅ 成立（fn 实际在 **:440**，证据引 :416-435 有漂移——行为无出入：3 段结构检查 :442-450 + base64url 解 payload + JSON parse，签名段内容从不读取校验） |
| V3 | `tests/claim_validation.rs` = 12 async + 1 sync，无 exp/nbf/alg 用例 | ✅ 成立。`cargo test --list` 实跑 = 13（12 `#[tokio::test]` + `jwt_claims_decode_roundtrip` sync）；`wrong_issuer`/`missing_audience`/`missing_audit_scope`/`wrong_subject` 等全部经 `deliver_with_claims` → `posts()==0` 字面断言 |
| V4 | `stub.rs::make_jwt` = `{"alg":"none","typ":"JWT"}` + 假签名 `ZmFrZS1zaWduYXR1cmU`，当前被接受 | ✅ 成立（:242-247）。现成 exploit fixture |
| V5 | `RelayConfig` 无 JWKS 字段；`from_env` 有 HTTPS/loopback 的 `service_url` 策略可复用 | ✅ 成立（struct :35-55 无 jwks 字段；`service_url` :121-136：HTTPS 或 insecure-loopback-HTTP、无 userinfo/query/fragment、≤2048；`AERO_AUDIT_*` presence-gating :60-66） |
| V6 | `relay.rs:403` `transient_claim_drift_requeues_without_any_post` = 「同形状」参照 | ✅ 成立（:403 精确命中）。`assert_transient_requeue` :300-350 全套断言：`FakeStatus::Ready`（永不 dead）、`attempts==1`、`available_at==t0+1s`（fake 时钟钉死）、claim_token/lease 清空、`last_error` 片段、`posts()==0`、重 claim 轮换新 token |
| V7 | 37/37 pins = 15 executed + 22 [PROPOSED] | ✅ 成立（`scripts/b5-pin.sh:29-68`：15 仓内槽含 `audit_governance::`/`a3-relay-drill`/`t11-fail-closed`/`moderation-priority-drill`/`relay-mock-probe`/`audit-provision-check`；22 `[PROPOSED]` 计数槽；guard 校验 :86-96） |
| V8 | T-11 drill 断言（token endpoint 关闭 ⇒ pend、attempts N→2N、never dead） | ✅ 成立（`src/bin/aero-audit-t11-drill.rs`：loopback 绑定即丢 → 连接拒绝；`COUNT(status=0)==N`、`COUNT(status IN (1,2,3))==0`、attempts 1.2s sleep 后 N→2N、`last_error LIKE '%audit connector HTTP transport failed%'`） |
| V9 | 0239:126-127 / 0241:113-114 `data_classification 'confidential'` + `retention_class 'security'` | ✅ 成立（逐字命中） |
| V10 | workspace 已钉 `jsonwebtoken "9"`（root :133）+ `rsa 0.9`（:137）；aero-auth 暴露 `KeyProvider`(:243)/`StaticKeyProvider`(:252)/`JwksKeyProvider`(:529, `new` :571, `DEFAULT_JWKS_CACHE_TTL` 300s :566, `UNKNOWN_KID_REFRESH_MIN_INTERVAL` 10s :567)/`validate_jwks_uri`(:54)/`OidcError`(:220)；connector 现无 aero-auth 依赖（加入无环） | ✅ 全部成立。aero-auth 仅依赖 aero-common 等基础 crate，无环 |
| V11 | 既有 31 测试 = lib 11 + claim_validation 13 + state_machine 7（3 PG 门控） | ✅ 成立（`cargo test --list` 实跑 11/13/7 = 31；state_machine 的 PG 用例 `#[ignore]`） |
| V12 | 兄弟 spec 已裁定签名拒绝 = permanent（≤1 次重试 → dead），本 spec D4 继承之 | ✅ 成立（兄弟 req D1 :37；兄弟 design D1 :40、D3/F2/F3、`PermanentKind::SignatureRejected` :82）——**但合并实施有两条兄弟裁决必须纳入**：N1/D5（双平面经现 `KeyProvider` trait 不可表达，需 aero-auth 增 additive fallible 方法）与 D6（校验顺序 = 签名先 claims 后，JWKS 启用时 opaque/畸形 token 从今日 Transient **有意改判** Permanent） |

**勘误汇总**：`decode_jwt_claims` 实际 :440（证据引 :416-435）；`JwksKeyProvider::new` 实际 :571（证据引 :529 为 struct 定义行）——均符号命中、无事实性出入。**额外发现（证据未覆盖，设计必须处理，均已裁决）**：① 兄弟 D5 使「aero-auth 零生产改动」不成立（约 15 行 additive 方法 + 3 单测，见 §4 API-4）——**已裁决 amend-spec（spec §0 AM-1 放宽 §7 红线为本项），矛盾消除**；② exp/nbf 无条件上线意味着**仅靠 env 开关无法回滚该半场**（见 §6 部署序）——spec D1 既决无条件，binary-rollback-only 即其忠实后果（AM-4 已落文）；③ 签名先验顺序下 AC1/AC2 夹具若在 JWKS 启用面跑会先撞签名层（§3 顺序矩阵裁决——AC1/AC2 在 JWKS-off 面跑，分类保持 Transient，AM-2 已落文）；④ 安全评审 F1/F2/F4（9.3.1 `Validation` 默认值泄漏进签名面、unknown-kid 限速 vs 1s backoff 的轮换滞后窗、`pub mod stub` 生产可达）已全部落文：API-2 字面清单 + §7 F1 钉测试 + D10 + b5-pin guard（§4/§5/§6/§7）。

## 2. Design overview

```
deliver() 每轮（初始 / 缓存命中 / 401-refresh 后重验 — 单调用点循环）:
  ┌─ keys 配置? ──否──→ 仅 claims 校验（今日行为 + exp/nbf）
  │                  （opaque/畸形 → Transient，不变）
  └─ 是 → ① verify_token_signature      ← 签名面
              │  fetch/刷新失败 → Transient（0 POST，requeue 永不 dead）
              │  alg∉RS256 / 坏签名 / unknown-kid → Permanent(SignatureRejected)
              │  （opaque/畸形 → Permanent — 兄弟 D6 改判，仅 JWKS-on 面）
           ② validate_token_claims_at(token, now)  ← claims 面（含 exp/nbf 时间窗）
              四查失败 / exp 过期 / nbf 未生效 / exp·nbf 非 number → Transient（0 POST）
           ③ POST（带 Idempotency-Key）→ 既有状态分类不变
```

- **半场一（exp/nbf，无条件）**：纯函数 `validate_token_claims_at(token, now)` 时钟注入；`exp <= now+60s` 拒 / `nbf > now+60s` 拒 / 存在但非 number 拒 / **缺失放行**（否则既有 13 个 claim 测试全爆）。leeway = `TIME_CLAIM_LEEWAY_SECS: i64 = 60` 命名常量，config 不暴露。
- **半场二（签名，opt-in）**：`AERO_AUDIT_JWKS_URL` 设置 ⇒ 强制；未设置 ⇒ 现状（exp/nbf 仍无条件）。密钥源复用 `aero_auth::{KeyProvider, StaticKeyProvider, JwksKeyProvider}`（TTL 300s + unknown-kid 10s 限速刷新 = 轮换语义免费）。alg 白名单 RS256。
- **两失败面严格分离**（兄弟 D3/F2/F3）：JWKS 机制不可用 = Transient（与 `transient_claim_drift_requeues_without_any_post` 同形状）；签名拒绝 = Permanent ≤1-retry → dead（继承兄弟 D1）。relay **零改动**（permanent 臂对 `PermanentKind` 泛化）。

## 3. Decision record（含与兄弟方向的合并裁决）

| # | 决策 | 依据 |
|---|---|---|
| D1 | exp/nbf **无条件**、**validated-when-present**（缺失放行）；`validate_token_claims_at(&self, token, now: OffsetDateTime)` 纯函数 + `validate_token_claims` 委托（`now_utc()`） | spec D1；leeway 60s 防边界抖动，测试余量 ≥2×leeway（±120s） |
| D2 | JWKS = opt-in `AERO_AUDIT_JWKS_URL`（本 direction 契约名）；畸形 → boot fail-loud（`bail!`，与 `AERO_AUDIT_*` presence-gating 一致） | spec D2。**合并终态**：与兄弟 `AERO_AUDIT_JWKS_URI` 统一为 `AERO_AUDIT_JWKS_URL`，终态取兄弟 fail-loud 必填（connector 启用而缺 → 拒启）；两变量同时设置 → `bail!`（歧义即错误）。本 direction acceptance 只要求 opt-in 面，fail-loud 属合并 housekeeping |
| D3 | JWKS fetch/刷新失败（网络/HTTP/畸形文档）= **Transient**，0 POST，requeue 永不 dead；**与签名拒绝分属两个失败面** | spec D3；`transient_claim_drift_requeues_without_any_post` 同形状 |
| D4 | 签名拒绝（alg:none / 非 RS256 / 坏签名 / unknown-kid 刷新后仍无）= **Permanent(SignatureRejected)**，attempt 1 requeue → attempt ≥2 `mark_dead`（`is_dead_at`/`PERMANENT_DEAD_AT=2` 既有泛化臂，relay 零改动） | spec D4 + 兄弟 D1；`last_error` 经 `PermanentKind` Debug 自动含 `SignatureRejected` 哨兵（兄弟 R6） |
| D5 | **aero-auth `KeyProvider` 增 additive fallible 方法** `decoding_key_fallible(&self, kid, alg) -> Result<Option<DecodingKey>, String>`：默认实现委托 `decoding_key`（`Ok(...)`，零行为变化）；`JwksKeyProvider` 覆写：fetch/刷新失败 → `Err(msg)`，key 不在集 → `Ok(None)`。映射 `Err → Transient`、`Ok(None)/验签失败 → Permanent` | 兄弟 N1/D5。现 trait 的 `Option` 塌缩使双平面不可表达——不采纳则只能复制 ~100 行 JWKS 客户端（备选 B，拒）或塌缩单类（违反 AC3(a)/R4，拒）。spec §0 AM-1 已裁决 amend-spec 并放宽 §7 红线为本项（约 15 行 + 3 单测）；**方法实签 = async + `kid: Option<&str>`，见 API-4 勘误** |
| D6 | **校验顺序 = 签名先、claims 后**（同一循环迭代内）。JWKS-on 面：结构性失败（opaque/畸形/alg:none/非 RS256）→ `Permanent(SignatureRejected)`；JWKS-off 面：顺序无关，opaque/畸形保持今日 Transient | 兄弟 D6。**顺序矩阵**（合并实现必须显式测试）：JWKS-off + exp 过期 → Transient（claims 面）；JWKS-on + trusted 签名 + exp 过期 → Transient（claims 面，**依赖 API-2 的 `validate_exp=false` 字面清单——9.3.1 默认 `true` 会把该格打回 Permanent，F1 钉测试锁定**）；JWKS-on + alg:none + exp 过期 → Permanent（签名面先命中）。AC1/AC2 夹具在 JWKS-off 面跑，分类不漂移 |
| D7 | 签名拒绝时失效 token 缓存（`invalidate_token`）→ attempt 2 轮换新 token（≤1-retry 预算 = 轮换传播单窗解药） | 兄弟 D7；`assert_transient_requeue` 的轮换断言镜像到 permanent 面 |
| D8 | exp/nbf 非 number = claims 面 Transient（fail-closed 拒，不投递）；**不**按结构性畸形归 Permanent | 非 number 是 IdP 配置/版本漂移，token 刷新后自愈，与 wrong-iss 同族；结构性 JWT 非法（3 段/JSON/base64）才归签名面（D6） |
| D9 | 验证时机 = 每个将用于 POST 的 token（缓存命中/新获取/401-refresh 后重验）——`deliver()` 现有循环内**同一调用点**接入 | spec D6；零新调用面 |
| D10 | **unknown-kid 重试刷新绕过（F2 已决缓解）**：`decoding_key_fallible` 覆写内记录 last-missed kid；**同 kid** 在 10s 限速窗内重试（requeue backoff 1s < 10s）时允许**一次**绕过刷新（`reserve_unknown_kid_refresh` 照常占用 → 风暴上界仅 1→2 次/10s，且只对真实 requeue 过的行生效；新 kid 扫描仍被限速） | 安全评审 F2：原 F7「attempt 2 新 token → 恢复」在端点滞后时因限速失败（attempt 1 已消耗 10s 窗；attempt 2 于 t0+1s 被 throttle → 陈旧集 → `Ok(None)` → dead）。备选 (a) 签名面 backoff ≥10s 触碰 relay 泛化臂（违反 relay 零改动，拒）；(c) 死行复活路径无现成 seam（OutboxRepo 无 revival，范围外，拒）——选 (b)。边界诚实化：端点须在 attempt-2 刷新窗内追上，否则 dead + 哨兵（§5 F7 改写） |

## 4. API changes（逐文件、逐符号）

### API-1 `crates/aero-audit-connector/src/config.rs`

```rust
pub struct RelayConfig {
    // …既有 16 字段不动…
    /// JWKS 端点（`AERO_AUDIT_JWKS_URL`）。`Some` ⇒ 签名验证强制；`None` ⇒ 关闭。
    pub jwks_uri: Option<Url>,
}
```

- `from_env()`：解析 `AERO_AUDIT_JWKS_URL`（optional）→ `Some(raw)` 时过 `service_url(name, raw, allow_insecure)` 同款策略（HTTPS 或 insecure-loopback-HTTP、无 userinfo/query/fragment、≤2048 字节）；畸形 → `bail!`（boot fail-loud，relay 不启动、不 claim、不投递）。未设置 → `None`。
- 合并终态（兄弟 fail-loud）：connector 启用（`AERO_AUDIT_TOKEN_ENDPOINT` 存在）而 URL 缺失 → `bail!`。**本 direction 单飞时保持 opt-in**——终态切换是 env 语义变更，需与部署序（§6）一起做。
- 兼容：struct 增字段是 `#[derive(Clone, Debug)]` 纯数据变更；tests/drill 直接构造点（`RelayConfig { .. }` 字面量）需补 `jwks_uri: None`（`..Default` 不存在——字面量 6 处，见 §6 step 5）。

### API-2 `crates/aero-audit-connector/src/client.rs`

```rust
/// 时间 claims 校验 leeway（防边界抖动；config 不暴露）。
pub const TIME_CLAIM_LEEWAY_SECS: i64 = 60;

pub enum PermanentKind {
    // …既有 4 variant 不动…
    /// 签名验证失败（alg 非白名单 / 坏签名 / unknown-kid）——≤1 次重试 → dead。
    SignatureRejected,
}

pub struct AuditClient {
    config: RelayConfig,
    http: reqwest::Client,
    token_cache: Mutex<Option<CachedToken>>,
    keys: Option<Arc<dyn KeyProvider>>,   // None ⇒ 签名验证关闭（JWKS-off）
}

impl AuditClient {
    /// 既有签名不变；keys: None（零随迁）。
    pub fn new(config: RelayConfig) -> anyhow::Result<Self>;
    /// 新增注入 seam（测试/drill/生产 JWKS 路径）。
    pub fn with_key_provider(config: RelayConfig, keys: Arc<dyn KeyProvider>) -> anyhow::Result<Self>;

    /// 纯函数：四查 + exp/nbf 时间窗。now 注入，测试钉 now（无 wall-clock 竞态）。
    pub fn validate_token_claims_at(&self, token: &str, now: OffsetDateTime) -> Result<(), ClaimRejection>;
    /// 委托 validate_token_claims_at(token, OffsetDateTime::now_utc())——deliver() 调用点不变。
    pub fn validate_token_claims(&self, token: &str) -> Result<(), ClaimRejection>;

    fn verify_token_signature(&self, token: &str) -> Result<(), DeliveryError>;  // 见下
}
```

- `verify_token_signature` 内部顺序（D5/D6）：`jsonwebtoken::decode_header` 失败或 `alg` ∉ {RS256} → `Permanent(SignatureRejected)`；`keys.decoding_key_fallible(kid, alg)`：`Err(msg)` → `Transient(anyhow!("audit jwks unavailable: {msg}"))`（D3），`Ok(None)` → `Permanent(SignatureRejected)`；`jsonwebtoken::decode::<Value>`（**字面 Validation 字段清单，勿依赖 9.3.1 默认值**——默认 `validate_aud: true`/`validate_exp: true`/`required_spec_claims={"exp"}` 会把带 aud 的真 token、过期 token、缺 exp token 在签名面误判 Permanent，破坏 D6 矩阵 Transient 格与 D1 缺失放行；9.3.1 **无 `validate_iss` 字段**，iss 关闭 = 保持 `iss: None` 默认）：

  ```rust
  let mut validation = Validation::new(Algorithm::RS256); // algorithms=[RS256] 闭集
  validation.validate_aud = false;          // aud 属四查契约面，签名面不判
  validation.validate_exp = false;          // exp 属 claims 面（validate_token_claims_at）
  validation.validate_nbf = false;          // nbf 同
  validation.required_spec_claims.clear();  // 缺 exp 放行（D1）；validate_signature 保持默认 true
  ```

  失败 → `Permanent(SignatureRejected)`。JWKS-off（`keys.is_none()`）→ `Ok(())`。
- `deliver()` 循环接入（D9）：每轮先 `verify_token_signature`（仅 keys 配置时）再 `validate_token_claims_at`；签名拒绝 → `invalidate_token` + 返回 `Permanent(SignatureRejected)`（D7）；claims 拒绝 → 既有 Transient 路径逐字保留。删除 :196-198 `[PROPOSED]` 注释。
- exp/nbf 读取在 `decode_jwt_claims` 已解出的 `Value` 上直接取（NumericDate = JSON number；字符串数字形态按 malformed 拒）——**零新依赖**。

### API-3 `crates/aero-audit-connector/src/stub.rs`

```rust
pub struct SinkBehavior {
    // …既有字段不动（token_claims / token_claims_after_first / events_status /
    //   delay_ms / unauthorized_once / receipt_valid / receipt_event_id_override）…
    /// None ⇒ alg:none make_jwt（既有夹具）；Some ⇒ RS256 签名（kid 指定）。
    pub signing_key: Option<(RsaPrivateKey, String)>,   // (key, kid)
    /// 篡改模式（签后改 claims / 改签名段）——负路径夹具。
    pub tamper: Option<TamperMode>,
    /// /jwks 端点服务的 key 集（可换 = 轮换驱动）。
    pub jwks_keys: Vec<(RsaPrivateKey, String)>,        // 默认空 ⇒ /jwks 404
}

pub fn make_jwt(claims: &Value) -> String;            // 既有 alg:none 形式保留（拒绝路径夹具）
pub fn make_rs256_jwt(claims: &Value, key: &RsaPrivateKey, kid: &str) -> String;  // jsonwebtoken::encode
```

- stub server 增 `/jwks` 路径：按 `jwks_keys` 生成 `{"keys":[{kty/kid/n/e}]}`；`set_behavior` 换 key 集 = 「served after rotation」语义（AC4）。
- `/token` 按 `signing_key` 选 alg:none 或 RS256 签发；`tamper` 在签后改 payload/签名段。
- `StubSink` 增 `decoding_key()`（当前 key 集导出 `DecodingKey`，供 `StaticKeyProvider::single` 注入）与 `jwks_url()`（loopback 地址，供 `JwksKeyProvider::new`）。
- **stub 可达性硬化（F4 已决）**：`pub mod stub` 保持编译进 lib（测试 + 三 drill 依赖；feature-gate 会迫使 harness 全部 `cargo run --bin`/`cargo test` 调用加 `--features stub`，churn 且易漏），改以 **CI grep pin** 锁死生产路径（见 §6 step 7）——`make_jwt` 的 alg:none 接受面由合并终态 fail-loud（§6 step 8）从配置层消灭。

### API-4 `crates/aero-auth/src/oidc.rs`（兄弟 D5 的唯一跨界改动）

```rust
#[axum::async_trait]
pub trait KeyProvider: Send + Sync {
    /// 既有实签（oidc.rs:244）：async + `kid: Option<&str>`——勘误：本设计初稿的 sync
    /// 草图不符实签，实现以本块为准（与兄弟 design §2.1 一致）。
    async fn decoding_key(&self, kid: Option<&str>, algorithm: Algorithm) -> Option<DecodingKey>;
    /// additive fallible 面：Err = 密钥源机制不可用（fetch/刷新失败 → Transient）；
    /// Ok(None) = 无匹配 key（unknown-kid → Permanent）。默认实现委托既有方法（零行为变化）。
    async fn decoding_key_fallible(
        &self,
        kid: Option<&str>,
        algorithm: Algorithm,
    ) -> Result<Option<DecodingKey>, String> {
        Ok(self.decoding_key(kid, algorithm).await)
    }
}
```

- `JwksKeyProvider` 覆写：fetch/刷新失败 → `Err`（复用 `safe_jwks_request_error` 文案）；key 不在集 → `Ok(None)`；**D10 绕过臂（AM-5 受控放宽）**——覆写内维护 last-missed kid 单槽 `Mutex<Option<(String, Instant)>>`（additive 字段，`new` 不变）：未限速 miss 时记录 (kid, now)；同 kid 重试且窗内（与 `reserve_unknown_kid_refresh` 共享同一窗口谓词 = 单源 `UNKNOWN_KID_REFRESH_MIN_INTERVAL` 常量）→ 走私有 `bypass_refresh()`：**强制占用**刷新窗（写 `last_unknown_kid_refresh = now`，下一次普通刷新被推后）→ `fetch()` → 存 entry → 重新 `key_from_set`（命中即恢复，F7）。语义钉：**每 (kid, 窗) 至多一次绕过**、新 kid 无绕过（仍限速）、last-missed 单槽被新 kid 覆盖（交错 K2→K3→K2 会拒 K2 绕过——fail-closed 边注一行）；竞态（并发同 kid 重试）最坏多一次 fetch，仍 ≤2 次/10s 界内。`cached_set`/`decoding_key`/限速常量**逐字节不动**（fetch+store 提为私有 `fetch_and_store` 供两处共用 = 行为保持重构）。`StaticKeyProvider` 不覆写（默认实现 = 零行为变化）。
- 约 25–30 行 + 4–5 单测（`StaticKeyProvider` 默认委托、`JwksKeyProvider` Err 面、`JwksKeyProvider` Ok(None) 面、**D10 同 kid 单次绕过面**、可选交错覆盖边）。红线状态：spec §0 AM-1 + **AM-5** 已裁决 amend-spec 并放宽 §7 为本项——本设计 D5/D10 与 spec 不再矛盾。

### API-5 `crates/aero-audit-connector/Cargo.toml`

```toml
[dependencies]
aero-auth.workspace = true      # 新增；基础层，无环
jsonwebtoken.workspace = true   # 新增；root 已钉 "9"
[dev-dependencies]
rsa = { workspace = true, features = ["pem"] }   # 测试 keygen；root 已钉 0.9
```

**零新增供应链条目**（全部 workspace 已钉）。root `Cargo.toml` 不动；无 str0m；无 DB 迁移；无 web/NATS/总线改动。

## 5. Failure modes（含分类、可观测、恢复）

| # | 故障 | 分类 | 行为 | 恢复 / 可观测 |
|---|---|---|---|---|
| F1 | `AERO_AUDIT_JWKS_URL` 畸形（opt-in 面）/ 合并终态缺失（connector 启用而 URL 缺） | boot | `from_env` `bail!`——relay 不启动、不 claim、不投递（fail-loud，与 `AERO_AUDIT_*` presence-gating 一致）；终态拒启路径兄弟 N7 已验 `main.rs` boot | config 单测三例 + 部署序先 env 后 binary（§6 step 8） |
| F2 | JWKS fetch/刷新网络/HTTP 失败 | **Transient** | 0 POST；`requeue` backoff、永不 dead；`last_error` 含 `"audit jwks unavailable"` 片段 | 下次 tick 自动恢复；`transient_claim_drift…` 同形状断言 |
| F3 | JWKS 文档畸形（坏 JSON / 无 RSA key / 重复 kid） | **Transient** | 同上（fetch 平面，非 token 无效） | provider 自带 warn + TTL 300s 后重试 |
| F4 | unknown kid（刷新后仍无） | **Permanent** | 0 POST；attempt 1 requeue(1s) → attempt ≥2 `mark_dead`；`last_error` 含 `SignatureRejected` 哨兵 | D7 缓存失效 → attempt 2 拿新 token；D10 同 kid 绕过限速再刷新（端点已追上即恢复）；10s 限速 + 300s TTL 防风暴 |
| F5 | alg:none / HS* / 非白名单 alg / 坏签名 | **Permanent** | 0 POST；同上 | 哨兵可 grep；**无自动复活路径**（OutboxRepo 无 revival seam，grep 实证），运维人工处置属范围外 |
| F6 | exp 过期 / nbf 未生效 / exp·nbf 非 number | **Transient**（claims 面） | 0 POST；requeue 轮换新 token；`last_error` 含 `"token has expired"` / `"token is not yet valid"` | IdP 配置漂移，刷新自愈 |
| F7 | 轮换传播单窗（IdP 已换 key，端点暂未服务） | **Permanent** ≤1-retry | attempt 1 unknown-kid 同步刷新（限速起点 t0）；attempt 2 新 token + D10 同 kid 单次绕过（每 (kid, 窗) 一次；绕过占用刷新窗）→ 端点已追上即恢复；端点滞后超窗 → dead（哨兵可观测） | D10 已决缓解（AM-5）；**不承诺**端点无限期滞后下的恢复（诚实边界）；交错新 kid 覆盖 last-missed 单槽 → 该 kid 绕过被拒（fail-closed，边注） |
| F8 | token endpoint 关闭（T-11） | n/a | token 永不可得，签名路径永不参与；rows pend、attempts N→2N、never dead | T-11 drill 断言逐字不变 |
| F9 | 未采纳 D5 的双平面塌缩 | — | JWKS 网络故障会把全表行 ≤1-retry 后 dead（数据丢失面）或违反 AC3(a) | **被 D5 否决**，测试 `FailingKeyProvider` 双钉防回归 |
| F10 | 401-refresh 路径叠加签名面 fetch 超出 30s 租约（verify ≤8s + POST 10s + token refresh 10s + 重验 ≤8s + 重 POST 10s ≈ 36–46s） | n/a（可用性） | 租约到期 → `settle`/`requeue`/`mark_dead` fence 失败 → warn + 租约到期重取（Idempotency-Key 幂等，at-least-once） | 显式声明：`check_lease_invariant`（2×timeout+2s = 22s ≤ 30s）**不含**签名面 fetch 预算；D10 绕过只出现在 attempt-2 未 POST 路径（≤8s fetch + 10s POST = 18s < 30s，不超租约）——F2 缓解不改此面 |

**不新增**：metrics/gauges/JWKS 可达性 boot 探测（spec §7 红线，staging 项）；不改变 403→dead、scope-deficient→dead 等既有分类。

## 6. Migration & deployment steps（零 DB 迁移）

1. **合并 housekeeping**：统一 env 名为 `AERO_AUDIT_JWKS_URL`；兄弟 req/design 的 `AERO_AUDIT_JWKS_URI` 行勘误为引用本名；两变量同设 → `bail!`。
2. **aero-auth D5/D10 先落**（additive，独立可合）：`decoding_key_fallible`（含 D10 绕过臂 + last-missed 单槽 + `fetch_and_store` 共用）+ 4–5 单测（默认委托 / Err 面 / Ok(None) 面 / D10 绕过面）。门禁：`cargo test -p aero-auth --lib` 全绿、无新 clippy 警告。
3. **connector 依赖**：`Cargo.toml` +`aero-auth`/`jsonwebtoken`（deps）、+`rsa`（dev）。门禁：`cargo check -p aero-audit-connector`。
4. **config.rs**：`jwks_uri` 字段 + `from_env` 解析 + 单测 3 例（`jwks_url_absent_parses_none` / `jwks_url_illegal_bails` / `jwks_url_legal_parses`）。门禁：`cargo test -p aero-audit-connector --lib config::`。
5. **client.rs + stub.rs**：`TIME_CLAIM_LEEWAY_SECS`、`validate_token_claims_at`、`SignatureRejected`、`with_key_provider`、`verify_token_signature`、`deliver` 接入（D6 顺序 + D7 失效）、删 `[PROPOSED]` 注释；stub RS256 minting + `SinkBehavior` 扩展 + `/jwks` 端点。**6 处 `RelayConfig` 字面量补 `jwks_uri: None`**（relay.rs、state_machine.rs、claim_validation.rs、drill ×3——以 `git grep "RelayConfig {"` 现网清单为准，勿信文档计数）。
6. **新增用例**（§7 AC 映射全表）。门禁：`cargo test -p aero-audit-connector --all-targets` = 既有 31（28 run + 3 PG `#[ignore]`）+ 新增全绿。
7. **回归钉**：`scripts/test-integration.sh` 全 harness——37/37 pin guard、T-11 drill 断言逐字不变、`a3-relay-drill`/`moderation-priority-drill` 槽不变；**`scripts/b5-pin.sh` 新增 F4 stub 可达性 guard**（① `crates/aero-server/src/` 零 `stub` 引用；② connector `src/` 除 `stub.rs`/`src/bin/`/`relay.rs`（其引用在 `#[cfg(test)] mod tests` 内）外零 `stub` 引用——生产构造面 `RelayConfig::from_env → AuditClient::new` 永不经 stub）。
8. **部署序（opt-in 面）**：① 布 binary（exp/nbf 半场**无条件生效**——行为变更：过期/未生效 token 不再可投递；leeway 60s 吸收时钟抖动；如 IdP token 生命周期异常 → 行 pend + Transient requeue，不丢不死，回滚 = 回退 binary，**无 env 开关**）；② 验证 IdP `/jwks` 可达且含当前签名 key（`curl` + kid 比对）；③ 设置 `AERO_AUDIT_JWKS_URL` 并滚动重启（签名面激活，fail-closed）；④ 轮换演练：换 IdP 签名 key 并**先更新端点 JWKS**（传播 ≤ attempt-2 刷新窗）→ 观察 ≤1-retry 后恢复（F7/D10）；端点滞后超窗 → 行 dead + 哨兵可观测（诚实边界，勿当故障）。回滚 JWKS 面 = unset env + 重启（exp/nbf 仍生效）。**合并终态（fail-loud 必填）**：先布 env 再布 binary，否则拒启（F1）。

## 7. Testable acceptance mapping（AC → 测试 → 断言 → 门禁）

| AC | 新增测试（`tests/claim_validation.rs` 与 `src/relay.rs` 模块内、`tests/state_machine.rs`） | 断言（全部确定性、时钟钉死） | 门禁 |
|---|---|---|---|
| AC1 | `expired_token_is_rejected_before_any_post`（`deliver_with_claims` 模式，JWKS-off 面，`SinkBehavior.token_claims` 四查全匹配 + `"exp": pinned_now - 120s`） | `Err(_)`；`stub.posts() == 0`；`last_error` 含 `"token has expired"`；分类 = Transient（claims 面，D6 矩阵） | `cargo test -p aero-audit-connector --test claim_validation` |
| AC2 | `future_nbf_token_is_rejected_before_any_post`（`"nbf": pinned_now + 3600s`） | `Err(_)`；`posts() == 0`；`"token is not yet valid"`；Transient | 同上 |
| AC3(a) | `alg_none_token_rejected_when_jwks_enabled`（`with_key_provider(StaticKeyProvider::single(stub.decoding_key()))` + 既有 alg:none `make_jwt`） | `Err(Permanent(SignatureRejected))`；`posts() == 0`；签名拒绝 → `invalidate_token`（D7） | 同上 |
| AC3(b) | `jwks_fetch_failure_requeues_without_any_post`（stub `/jwks` 关闭/500；`FailingKeyProvider` 双钉）+ relay 面走 `assert_transient_requeue` 形状 | `FakeStatus::Ready`（永不 dead）；`attempts == 1`；`available_at == t0 + backoff(1)`（fake 时钟）；`claim_token`/`lease_expires_at` 均 `None`；`last_error` 含 `"audit jwks unavailable"`；`posts() == 0`；重 claim 轮换新 token | `src/relay.rs` 模块内 + `--test claim_validation` |
| AC4 | `kid_rotation_new_key_served_is_accepted`（stub 服务 {A} → `Ok(())`、`posts()==1`；`set_behavior` 换 {B} → B 签 token `Ok(())`、`posts()==2`；A 签 token `Err(SignatureRejected)`、posts 不增长）。实现：每阶段 fresh provider 或 provider 测试 seam 绕过 10s 限速（acceptance 只钉属性） | 三段断言如上 | 同上 |
| AC5 | 无新测试——回归钉：`scripts/test-integration.sh` 37/37 guard、T-11 drill 断言逐字不变（`COUNT(status=0)==N`、`COUNT(status IN (1,2,3))==0`、attempts N→2N、transport 片段）、`a3-relay-drill`/`moderation-priority-drill` 槽不变；`cargo test -p aero-audit-connector --all-targets` 31+ 全绿 | 见 spec AC5 | `scripts/test-integration.sh` + `scripts/b5-pin.sh` |
| 补 | `signature_plane_ignores_aud_and_time_claims`（**F1 钉**：RS256 真签 token 带 `aud` + 无 `exp` → 签名面 `Ok`；带过期 `exp` → 签名面 `Ok`（claims 面单独判 Transient）——钉死 API-2 字面 Validation 清单，防 9.3.1 默认值回归）· `exp_nbf_missing_claims_still_pass`（无 exp/nbf token → `Ok`，防 D1 回归）· `exp_nbf_non_numeric_rejected`（string 数字 → 拒，fail-closed）· `signature_rejected_dead_after_exactly_two_attempts`（state_machine 镜像 `permanent_error_dead_after_exactly_two_attempts`：attempt 2 → `FakeStatus::Dead`、`is_dead_at`、不再可 claim）· `unknown_kid_is_permanent_never_transient`（`Ok(None)` 面——D10 绕过面除外：同 kid 窗内重试先走绕过刷新，非直接 `Ok(None)`）· `same_kid_retry_bypasses_unknown_kid_throttle_once`（aero-auth 单测，D10 判别器：同 kid 窗内重试 → **恰好一次**绕过 fetch（fetch 计数 == 2：首次普通 + 一次绕过）；窗内第二次同 kid 重试不再绕过；新 kid 重试无绕过）· `rotation_lag_recovers_via_bypass_before_dead`（connector relay 级 F2/F7 判别器：stub 端点滞后——claim 1 unknown-kid miss → Permanent requeue(1s)；端点换 {K1,K2} → claim 2 新 token + 同 kid 绕过 → `Delivered`、posts==1；**无 D10 则 claim 2 限速 stale → `Ok(None)` → dead**）· `jwks_off_keeps_opaque_token_transient`（D6 矩阵：JWKS-off 面 opaque 仍 Transient）· `both_jwks_env_vars_bail`（合并终态） | 各测试内字面断言 | 全量门禁 |

**提交前必过**（AGENTS.md §4.3）：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（不新增警告）· `scripts/{truth-check,file-size-check,web-check}.sh` · `scripts/test-integration.sh`（37/37 + T-11）。

## 8. Out of scope（红线，与 spec §7 一致）

- typ/iat/jti 存在性强制、`sub==client_id` 收敛、scope registry、403→dead / scope-deficient→dead 分类（兄弟 aero-auth 8/8 契约面）。
- L1 聚合（分析文件 direction #0）与 relay liveness（#2）。
- JWKS 可达性 boot 探测 / metrics / gauges（staging 项）。
- 真实 IdP/sink 联调（stub 先行）。
- 本设计不触碰：`relay.rs` 状态机/租约/幂等、`pg.rs`、`outbox.rs`、`fake.rs` 语义、web/NATS/总线。

## 9. 文件级锚点（grep 用，勿引行号）

`client.rs::{validate_token_claims, validate_token_claims_at, decode_jwt_claims, verify_token_signature, deliver, PermanentKind, TIME_CLAIM_LEEWAY_SECS}` · `config.rs::{RelayConfig, from_env, service_url}` · `relay.rs::{deliver_claim, assert_transient_requeue, is_dead_at, PERMANENT_DEAD_AT}` · `stub.rs::{make_jwt, make_rs256_jwt, SinkBehavior, StubSink}` · `tests/claim_validation.rs::{deliver_with_claims, config}` · `tests/state_machine.rs` · `src/bin/aero-audit-t11-drill.rs` · `scripts/b5-pin.sh::B5_CONTRACT_TEST_LIST` · `aero-auth/src/oidc.rs::{KeyProvider, decoding_key_fallible, StaticKeyProvider, JwksKeyProvider, validate_jwks_uri}`。
